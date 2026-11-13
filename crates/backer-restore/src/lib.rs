//! Restore: browse a job's backup on the NAS and copy files or folders back to this computer (spec FR-31).
//!
//! Safety rules:
//! - The NAS is only read. Nothing here can write to or delete from it.
//! - A file that already exists on this computer is replaced only if the user confirmed that exact file
//!   (it was listed as a conflict in the plan they approved). Anything that appears after they confirmed
//!   is left alone and reported as skipped.
//! - Each file is written to a temp file next to its destination, checked with BLAKE3, then moved into place.
use backer_connector::{Connector, Reader};
use backer_core::{Error, FolderPair, Job, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

/// Failures kept by name in a report; the rest are only counted.
pub const MAX_LISTED: usize = 200;

/// One row of the backup browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    /// Path on the NAS relative to the destination root, with forward slashes.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    /// For the top level only: the folder on this computer that this backup folder came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Where restored files go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Back where they were backed up from.
    Original,
    /// Into this folder; each selected file or folder lands inside it by name.
    Folder(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFile {
    pub nas: String,
    pub local: PathBuf,
    pub size: u64,
    pub mtime: i64,
}

/// A planned file that would replace something already on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub nas: String,
    pub local: PathBuf,
    pub nas_size: u64,
    pub nas_mtime: i64,
    pub local_size: u64,
    pub local_mtime: i64,
    /// A folder (not a file) has this name, so the file cannot be restored there at all.
    pub blocked: bool,
}

/// What a restore would do. Built before anything is written, so the user can confirm replacements.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub files: Vec<PlanFile>,
    pub bytes: u64,
    pub conflicts: Vec<Conflict>,
}

impl Plan {
    /// Local files the user would be agreeing to replace.
    pub fn replaceable(&self) -> HashSet<PathBuf> {
        self.conflicts.iter().filter(|c| !c.blocked).map(|c| c.local.clone()).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreStatus {
    Running,
    Done,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub status: RestoreStatus,
    pub files_total: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub bytes_done: u64,
    /// Restored where nothing was before.
    pub restored: u64,
    /// Restored over a file the user agreed to replace.
    pub replaced: u64,
    /// Left alone because a file was already there and the user did not agree to replace it.
    pub skipped_existing: u64,
    pub failed_total: u64,
    pub failed: Vec<Failure>,
    pub message: Option<String>,
}

impl Report {
    /// A report for a restore of `plan` that has not copied anything yet.
    pub fn new(plan: &Plan) -> Self {
        Self {
            status: RestoreStatus::Running,
            files_total: plan.files.len() as u64,
            bytes_total: plan.bytes,
            files_done: 0,
            bytes_done: 0,
            restored: 0,
            replaced: 0,
            skipped_existing: 0,
            failed_total: 0,
            failed: vec![],
            message: None,
        }
    }
    fn fail(&mut self, path: &str, e: impl std::fmt::Display) {
        self.failed_total += 1;
        if self.failed.len() < MAX_LISTED {
            self.failed.push(Failure { path: path.into(), error: e.to_string() });
        }
    }
}

/// Reject anything that is not a plain relative path with forward slashes.
fn clean_rel(rel: &str) -> Result<String> {
    let t = rel.trim_matches('/');
    if t.contains('\\') || t.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return Err(Error::Invalid(format!("unsafe path: {rel}")));
    }
    Ok(t.to_string())
}

/// The job folder a NAS path belongs to, and the rest of the path inside it ("" for the folder itself).
fn folder_of<'a>(job: &'a Job, rel: &'a str) -> Option<(&'a FolderPair, &'a str)> {
    job.folders.iter().find_map(|f| {
        if rel == f.dest {
            Some((f, ""))
        } else {
            rel.strip_prefix(&f.dest).and_then(|r| r.strip_prefix('/')).map(|r| (f, r))
        }
    })
}

fn push_rel(mut base: PathBuf, rel: &str) -> PathBuf {
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        base.push(part);
    }
    base
}

fn last(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

fn mtime_of(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// List one folder of the job's backup. `rel` "" lists the job's backed-up folders.
/// Only paths inside the job's own folders can be browsed.
pub fn browse<C: Connector + Reader + ?Sized>(conn: &C, job: &Job, rel: &str) -> Result<Vec<Entry>> {
    conn.check_ready()?;
    if rel.trim_matches('/').is_empty() {
        return Ok(job
            .folders
            .iter()
            .map(|f| Entry { name: last(&f.dest).to_string(), path: f.dest.clone(), is_dir: true, size: 0, mtime: 0, source: Some(f.source.clone()) })
            .collect());
    }
    let rel = clean_rel(rel)?;
    if folder_of(job, &rel).is_none() {
        return Err(Error::Invalid(format!("{rel} is not part of this job's backup")));
    }
    Ok(conn
        .list_dir(&rel)?
        .into_iter()
        .map(|e| Entry { path: format!("{rel}/{}", e.name), name: e.name, is_dir: e.is_dir, size: e.size, mtime: e.mtime, source: None })
        .collect())
}

/// Work out every file a restore would write, and which ones would replace something already here.
/// Reads the NAS and this computer; writes nothing.
pub fn plan<C: Connector + Reader + ?Sized>(conn: &C, job: &Job, items: &[String], target: &Target, cancel: &AtomicBool) -> Result<Plan> {
    conn.check_ready()?;
    if let Target::Folder(d) = target {
        if !d.is_absolute() {
            return Err(Error::Invalid("choose a full folder path to restore into".into()));
        }
    }
    let mut picked: Vec<String> = vec![];
    for i in items {
        let r = clean_rel(i)?;
        if folder_of(job, &r).is_none() {
            return Err(Error::Invalid(format!("{r} is not part of this job's backup")));
        }
        picked.push(r);
    }
    picked.sort();
    picked.dedup();
    // A file or folder inside another selected folder is already covered by it.
    let inside = |a: &str, b: &str| a.starts_with(&format!("{b}/"));
    let picked: Vec<String> = picked.iter().filter(|a| !picked.iter().any(|b| inside(a, b))).cloned().collect();
    if picked.is_empty() {
        return Err(Error::Invalid("nothing selected".into()));
    }

    let mut plan = Plan::default();
    for item in &picked {
        // Where the item's own name starts, for restoring into another folder.
        let parent_len = item.rfind('/').map(|i| i + 1).unwrap_or(0);
        let local_for = |nas: &str| -> PathBuf {
            match target {
                Target::Original => {
                    let (f, rest) = folder_of(job, nas).expect("checked above");
                    push_rel(PathBuf::from(&f.source), rest)
                }
                Target::Folder(d) => push_rel(d.clone(), &nas[parent_len..]),
            }
        };
        let add = |plan: &mut Plan, nas: String, size: u64, mtime: i64| {
            let local = local_for(&nas);
            if let Ok(m) = fs::symlink_metadata(&local) {
                plan.conflicts.push(Conflict {
                    nas: nas.clone(),
                    local: local.clone(),
                    nas_size: size,
                    nas_mtime: mtime,
                    local_size: if m.is_dir() { 0 } else { m.len() },
                    local_mtime: mtime_of(&m),
                    blocked: m.is_dir(),
                });
            }
            plan.bytes += size;
            plan.files.push(PlanFile { nas, local, size, mtime });
        };
        if let Some(st) = conn.stat(item)? {
            add(&mut plan, item.clone(), st.size, st.mtime);
            continue;
        }
        // A folder: walk it. Depth-first with an explicit stack so very deep trees are fine.
        let mut stack = vec![item.clone()];
        while let Some(dir) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            for e in conn.list_dir(&dir)? {
                let p = format!("{dir}/{}", e.name);
                if e.is_dir {
                    stack.push(p);
                } else {
                    add(&mut plan, p, e.size, e.mtime);
                }
            }
        }
    }
    Ok(plan)
}

/// Copy the planned files back. `replace` must contain exactly the local files the user agreed to replace
/// (normally `plan.replaceable()` after they confirmed, or empty to keep everything already here).
/// `on_progress` is called after each file with the report so far.
pub fn execute<C: Connector + Reader + ?Sized>(
    conn: &C,
    plan: &Plan,
    replace: &HashSet<PathBuf>,
    cancel: &AtomicBool,
    on_progress: &dyn Fn(&Report),
) -> Report {
    let mut rep = Report::new(plan);
    if let Err(e) = conn.check_ready() {
        rep.status = RestoreStatus::Failed;
        rep.message = Some(e.to_string());
        return rep;
    }
    for (seq, f) in plan.files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            rep.status = RestoreStatus::Cancelled;
            on_progress(&rep);
            return rep;
        }
        let shown = f.local.display().to_string();
        let existing = fs::symlink_metadata(&f.local).ok();
        match existing {
            Some(m) if m.is_dir() => rep.fail(&shown, "A folder with this name is in the way"),
            Some(_) if !replace.contains(&f.local) => rep.skipped_existing += 1,
            _ => {
                let may_replace = existing.is_some();
                match copy_back(conn, f, may_replace, seq as u64) {
                    Ok(Placed::New) => rep.restored += 1,
                    Ok(Placed::Replaced) => rep.replaced += 1,
                    Ok(Placed::AlreadyThere) => rep.skipped_existing += 1,
                    Err(e) => rep.fail(&shown, e),
                }
            }
        }
        rep.files_done += 1;
        rep.bytes_done += f.size;
        on_progress(&rep);
    }
    rep.status = RestoreStatus::Done;
    rep
}

struct HashingWriter<W: Write> {
    inner: W,
    h: blake3::Hasher,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.h.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

enum Placed {
    New,
    Replaced,
    /// Something appeared at the destination while copying and we were not allowed to replace it.
    AlreadyThere,
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn ioe(e: impl std::fmt::Display) -> Error {
    Error::Connector(e.to_string())
}

fn copy_back<C: Reader + ?Sized>(conn: &C, f: &PlanFile, may_replace: bool, seq: u64) -> Result<Placed> {
    let dir = f.local.parent().ok_or_else(|| Error::Invalid("no parent folder".into()))?;
    fs::create_dir_all(dir).map_err(ioe)?;
    let tmp = dir.join(format!(".backer-restore-{}-{}-{seq}.part", std::process::id(), TEMP_SEQ.fetch_add(1, Ordering::Relaxed)));
    let r = (|| -> Result<Placed> {
        // Hash the bytes as they arrive, then check the file on disk matches them.
        let mut w = HashingWriter { inner: std::io::BufWriter::with_capacity(1 << 20, File::create(&tmp).map_err(ioe)?), h: blake3::Hasher::new() };
        conn.read_file(&f.nas, &mut w)?;
        let want = w.h.finalize();
        let out = w.inner.into_inner().map_err(ioe)?;
        out.sync_all().map_err(ioe)?;
        // Keep the backup's modification time so the next backup sees the file as unchanged.
        if f.mtime > 0 {
            let _ = out.set_modified(UNIX_EPOCH + Duration::from_secs(f.mtime as u64));
        }
        drop(out);
        if hash_of(&tmp)? != want {
            return Err(Error::Connector("the restored copy did not match what was read from the NAS".into()));
        }
        if may_replace {
            fs::rename(&tmp, &f.local).map_err(ioe)?;
            return Ok(Placed::Replaced);
        }
        place_new(&tmp, &f.local)
    })();
    let _ = fs::remove_file(&tmp);
    r
}

/// Move `tmp` to `dest` only if nothing is there. A hard link fails atomically when the name is taken;
/// where links are not supported (some FAT or network drives) fall back to check-then-rename.
fn place_new(tmp: &Path, dest: &Path) -> Result<Placed> {
    match fs::hard_link(tmp, dest) {
        Ok(()) => Ok(Placed::New),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Placed::AlreadyThere),
        Err(_) => {
            if fs::symlink_metadata(dest).is_ok() {
                return Ok(Placed::AlreadyThere);
            }
            fs::rename(tmp, dest).map_err(ioe)?;
            Ok(Placed::New)
        }
    }
}

fn hash_of(p: &Path) -> Result<blake3::Hash> {
    let mut f = File::open(p).map_err(ioe)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(ioe)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use backer_connector::MountedConnector;
    use backer_core::Schedule;

    struct Env {
        _t: tempfile::TempDir,
        src: PathBuf,
        nas: PathBuf,
        conn: MountedConnector,
        job: Job,
    }

    fn env() -> Env {
        let t = tempfile::tempdir().unwrap();
        let (src, nas) = (t.path().join("Documents"), t.path().join("nas"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(nas.join("mac/Documents/sub")).unwrap();
        fs::write(nas.join("mac/Documents/a.txt"), b"aaa").unwrap();
        fs::write(nas.join("mac/Documents/sub/b.txt"), b"bbbb").unwrap();
        fs::write(nas.join("mac/Documents/sub/.backer-9-9.part"), b"half").unwrap();
        fs::write(nas.join("elsewhere.txt"), b"not ours").unwrap();
        let conn = MountedConnector::new(&nas);
        conn.init_marker().unwrap();
        let job = Job {
            id: 1,
            name: "j".into(),
            folders: vec![FolderPair { source: src.to_string_lossy().into(), dest: "mac/Documents".into() }],
            dest_root: nas.to_string_lossy().into(),
            schedule: Schedule::Manual,
            paused: false,
            catch_up: true,
            exclude_names: vec![],
            max_file_bytes: None,
        };
        Env { _t: t, src, nas, conn, job }
    }

    fn no() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn browse_starts_at_the_jobs_folders_and_stays_inside_them() {
        let e = env();
        let top = browse(&e.conn, &e.job, "").unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!((top[0].name.as_str(), top[0].path.as_str(), top[0].source.as_deref()), ("Documents", "mac/Documents", Some(e.src.to_str().unwrap())));
        let inner = browse(&e.conn, &e.job, "mac/Documents").unwrap();
        let names: Vec<_> = inner.iter().map(|x| (x.name.as_str(), x.is_dir)).collect();
        assert_eq!(names, vec![("sub", true), ("a.txt", false)]);
        assert_eq!(inner[1].path, "mac/Documents/a.txt");
        // Temp files are hidden.
        assert!(browse(&e.conn, &e.job, "mac/Documents/sub").unwrap().iter().all(|x| x.name == "b.txt"));
        // Outside the job, or escaping, is refused.
        assert!(browse(&e.conn, &e.job, "mac").is_err());
        assert!(browse(&e.conn, &e.job, "mac/Documents/../..").is_err());
        assert!(browse(&e.conn, &e.job, "mac/DocumentsX").is_err());
    }

    #[test]
    fn browse_refuses_an_unmounted_nas() {
        let e = env();
        fs::remove_file(e.nas.join(backer_core::DEST_MARKER)).unwrap();
        assert!(matches!(browse(&e.conn, &e.job, ""), Err(Error::DestinationNotReady(_))));
    }

    #[test]
    fn restores_a_folder_to_its_original_place() {
        let e = env();
        let p = plan(&e.conn, &e.job, &["mac/Documents".into()], &Target::Original, &no()).unwrap();
        assert_eq!((p.files.len(), p.bytes, p.conflicts.len()), (2, 7, 0));
        let r = execute(&e.conn, &p, &HashSet::new(), &no(), &|_| {});
        assert_eq!((r.status, r.restored, r.failed_total), (RestoreStatus::Done, 2, 0));
        assert_eq!(fs::read(e.src.join("a.txt")).unwrap(), b"aaa");
        assert_eq!(fs::read(e.src.join("sub/b.txt")).unwrap(), b"bbbb");
        // No temp files left behind, and the NAS is untouched.
        assert!(fs::read_dir(e.src.join("sub")).unwrap().all(|x| !x.unwrap().file_name().to_string_lossy().ends_with(".part")));
        assert_eq!(fs::read(e.nas.join("mac/Documents/a.txt")).unwrap(), b"aaa");
    }

    #[test]
    fn keeps_the_backups_modification_time() {
        let e = env();
        let nas_mtime = mtime_of(&fs::metadata(e.nas.join("mac/Documents/a.txt")).unwrap());
        let p = plan(&e.conn, &e.job, &["mac/Documents/a.txt".into()], &Target::Original, &no()).unwrap();
        execute(&e.conn, &p, &HashSet::new(), &no(), &|_| {});
        assert_eq!(mtime_of(&fs::metadata(e.src.join("a.txt")).unwrap()), nas_mtime);
    }

    #[test]
    fn restores_into_another_folder_by_name() {
        let e = env();
        let out = e._t.path().join("restored");
        let p = plan(&e.conn, &e.job, &["mac/Documents/sub".into(), "mac/Documents/a.txt".into()], &Target::Folder(out.clone()), &no()).unwrap();
        execute(&e.conn, &p, &HashSet::new(), &no(), &|_| {});
        assert_eq!(fs::read(out.join("sub/b.txt")).unwrap(), b"bbbb");
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"aaa");
        assert!(!e.src.join("a.txt").exists(), "the original location is untouched");
        // The top-level folder lands under its own name.
        let out2 = e._t.path().join("r2");
        let p = plan(&e.conn, &e.job, &["mac/Documents".into()], &Target::Folder(out2.clone()), &no()).unwrap();
        execute(&e.conn, &p, &HashSet::new(), &no(), &|_| {});
        assert!(out2.join("Documents/sub/b.txt").exists());
        assert!(plan(&e.conn, &e.job, &["mac/Documents".into()], &Target::Folder("relative".into()), &no()).is_err());
    }

    #[test]
    fn existing_files_are_listed_and_kept_unless_confirmed() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"mine, newer").unwrap();
        let p = plan(&e.conn, &e.job, &["mac/Documents".into()], &Target::Original, &no()).unwrap();
        assert_eq!(p.conflicts.len(), 1);
        assert_eq!((p.conflicts[0].local_size, p.conflicts[0].nas_size, p.conflicts[0].blocked), (11, 3, false));
        // Not confirmed: the existing file is kept, the rest restored.
        let r = execute(&e.conn, &p, &HashSet::new(), &no(), &|_| {});
        assert_eq!((r.restored, r.skipped_existing, r.replaced), (1, 1, 0));
        assert_eq!(fs::read(e.src.join("a.txt")).unwrap(), b"mine, newer");
        // Confirmed: replaced. (sub/b.txt is here now too, from the run above, so both are listed.)
        let p = plan(&e.conn, &e.job, &["mac/Documents".into()], &Target::Original, &no()).unwrap();
        assert_eq!(p.conflicts.len(), 2);
        let r = execute(&e.conn, &p, &p.replaceable(), &no(), &|_| {});
        assert_eq!((r.replaced, r.skipped_existing), (2, 0));
        assert_eq!(fs::read(e.src.join("a.txt")).unwrap(), b"aaa");
    }

    #[test]
    fn a_file_that_appears_after_confirming_is_never_replaced() {
        let e = env();
        let p = plan(&e.conn, &e.job, &["mac/Documents".into()], &Target::Original, &no()).unwrap();
        assert!(p.conflicts.is_empty());
        // The user confirmed a plan with no conflicts; then a file shows up.
        fs::create_dir_all(e.src.join("sub")).unwrap();
        fs::write(e.src.join("sub/b.txt"), b"arrived later").unwrap();
        let r = execute(&e.conn, &p, &p.replaceable(), &no(), &|_| {});
        assert_eq!((r.restored, r.skipped_existing), (1, 1));
        assert_eq!(fs::read(e.src.join("sub/b.txt")).unwrap(), b"arrived later");
    }

    #[test]
    fn a_folder_in_the_way_is_reported_not_replaced() {
        let e = env();
        fs::create_dir_all(e.src.join("a.txt")).unwrap();
        let p = plan(&e.conn, &e.job, &["mac/Documents/a.txt".into()], &Target::Original, &no()).unwrap();
        assert!(p.conflicts[0].blocked);
        assert!(p.replaceable().is_empty());
        let r = execute(&e.conn, &p, &p.replaceable(), &no(), &|_| {});
        assert_eq!((r.failed_total, r.restored), (1, 0));
        assert!(e.src.join("a.txt").is_dir());
    }

    #[test]
    fn selections_outside_the_job_or_unsafe_are_refused() {
        let e = env();
        for bad in ["elsewhere.txt", "../x", "mac/Documents/../../elsewhere.txt", "mac\\Documents", ""] {
            assert!(plan(&e.conn, &e.job, &[bad.into()], &Target::Original, &no()).is_err(), "{bad}");
        }
        assert!(plan(&e.conn, &e.job, &[], &Target::Original, &no()).is_err());
    }

    #[test]
    fn overlapping_selections_are_restored_once() {
        let e = env();
        let p = plan(&e.conn, &e.job, &["mac/Documents/sub/b.txt".into(), "mac/Documents".into(), "mac/Documents/sub".into()], &Target::Original, &no()).unwrap();
        assert_eq!(p.files.len(), 2);
    }

    #[test]
    fn cancel_stops_between_files_and_reports_progress() {
        let e = env();
        let p = plan(&e.conn, &e.job, &["mac/Documents".into()], &Target::Original, &no()).unwrap();
        let cancel = AtomicBool::new(false);
        let seen = std::cell::Cell::new(0);
        let r = execute(&e.conn, &p, &HashSet::new(), &cancel, &|r| {
            seen.set(r.files_done);
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!((r.status, r.files_done, seen.get()), (RestoreStatus::Cancelled, 1, 1));
    }

    #[test]
    fn unmounted_nas_fails_without_writing() {
        let e = env();
        let p = plan(&e.conn, &e.job, &["mac/Documents".into()], &Target::Original, &no()).unwrap();
        fs::remove_file(e.nas.join(backer_core::DEST_MARKER)).unwrap();
        let r = execute(&e.conn, &p, &HashSet::new(), &no(), &|_| {});
        assert_eq!(r.status, RestoreStatus::Failed);
        assert!(!e.src.join("a.txt").exists());
    }
}
