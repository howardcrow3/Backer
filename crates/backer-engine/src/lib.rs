//! Run pipeline: scan each source folder, copy what changed, flag what vanished.
//! The engine never deletes anything on the NAS.
use backer_connector::{Connector, TempCleaner};
use backer_core::*;
use backer_store::{FileRec, Store};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use unicode_normalization::UnicodeNormalization;
use walkdir::WalkDir;

pub const DEFAULT_EXCLUDES: &[&str] = &[".DS_Store", ".localized", ".Trashes", ".fseventsd", ".Spotlight-V100", ".TemporaryItems",
    "Thumbs.db", "desktop.ini", "$RECYCLE.BIN", "System Volume Information"];

pub trait Progress {
    fn update(&self, _stats: &RunStats) {}
}
pub struct NoProgress;
impl Progress for NoProgress {}

fn mtime_of(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

/// What a walk leaves out: the built-in junk names plus the job's own patterns, compiled once per run.
struct Excludes<'a> {
    root: &'a Path,
    set: ExcludeSet,
}

impl<'a> Excludes<'a> {
    fn new(job: &Job, root: &'a Path) -> Result<Self> {
        Ok(Self { root, set: ExcludeSet::new(&job.exclude_names)? })
    }
    /// Path from the top of the source folder with `/` separators, for an entry below it.
    fn rel(&self, e: &walkdir::DirEntry) -> String {
        e.path().strip_prefix(self.root).map(|r| r.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")).unwrap_or_default()
    }
    fn skips(&self, e: &walkdir::DirEntry) -> bool {
        if e.depth() == 0 {
            return false;
        }
        if DEFAULT_EXCLUDES.contains(&&*e.file_name().to_string_lossy()) {
            return true;
        }
        !self.set.is_empty() && self.set.is_excluded(&self.rel(e), e.file_type().is_dir())
    }
}

/// Short, path-free reason for a file-system error, so identical problems group together in the run report.
fn io_cause(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "Permission denied".into(),
        std::io::ErrorKind::NotFound => "File vanished during backup".into(),
        _ => strip_os_code(&e.to_string()),
    }
}

/// Cause shown for links and shortcuts, which Backer does not follow.
pub const LINK_SKIPPED: &str = "Shortcut or link, not copied";

/// Cause shown when a whole folder could not be opened (so everything inside it was skipped).
pub const FOLDER_LOCKED: &str = "Folder can’t be opened";

fn walk_cause(e: &walkdir::Error) -> String {
    let denied = e.io_error().is_some_and(|i| i.kind() == std::io::ErrorKind::PermissionDenied);
    let is_dir = e.path().is_some_and(|p| std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(true));
    if denied && is_dir {
        return FOLDER_LOCKED.into();
    }
    e.io_error().map(io_cause).unwrap_or_else(|| strip_os_code(&e.to_string()))
}

fn strip_os_code(s: &str) -> String {
    match s.rfind(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Writes a run's file log. Sequence numbers only ever go up, and each row is stamped with the time it happened.
struct RunLog<'a> {
    store: &'a Store,
    run: i64,
    seq: u64,
    base: i64,
    started: Instant,
}

impl RunLog<'_> {
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
    fn ev(&mut self, kind: &str, path: &str, detail: Option<&str>, size: Option<u64>) -> Result<()> {
        let row = LogRow { seq: self.next_seq(), ts: self.base + self.started.elapsed().as_secs() as i64, kind: kind.into(), path: path.into(), detail: detail.map(Into::into), size };
        self.store.log(self.run, &row)
    }
}

pub fn run_job(
    store: &Store,
    conn: &dyn TempCleaner,
    job: &Job,
    now: i64,
    cancel: &AtomicBool,
    progress: &dyn Progress,
) -> Result<RunSummary> {
    run_job_as(store, conn, job, now, cancel, progress, Trigger::Manual)
}

/// Same as [`run_job`], recording what started the run.
pub fn run_job_as(
    store: &Store,
    conn: &dyn TempCleaner,
    job: &Job,
    now: i64,
    cancel: &AtomicBool,
    progress: &dyn Progress,
    trigger: Trigger,
) -> Result<RunSummary> {
    job.validate()?;
    let run = store.begin_run(job.id, now)?;
    let started = Instant::now();
    let mut lg = RunLog { store, run, seq: 0, base: now, started };
    let mut stats = RunStats { trigger: trigger.as_str().into(), ..Default::default() };
    let mut failure: Option<String> = None;
    let mut cancelled = false;
    lg.ev("run", "", Some(match trigger { Trigger::Manual => "Backup started by you", Trigger::Schedule => "Backup started by schedule" }), None)?;

    // Refuse to run against an unmounted or wrong destination.
    if let Err(e) = conn.check_ready() {
        let msg = e.to_string();
        stats.errors += 1;
        lg.ev("error", "", Some(&msg), None)?;
        lg.ev("run", "", Some("Run ended: nothing was copied"), None)?;
        stats.duration_ms = started.elapsed().as_millis() as u64;
        store.finish_run(run, now + started.elapsed().as_secs() as i64, RunStatus::Failed, &stats, Some(&msg))?;
        return summary(store, job.id, run);
    }

    let mut cleaned: HashSet<String> = HashSet::new();

    'folders: for pair in &job.folders {
        let root = Path::new(&pair.source);
        let folder_started = Instant::now();
        let mut fs = FolderStat { source: pair.source.clone(), ..Default::default() };
        if !root.is_dir() {
            stats.errors += 1;
            fs.issues += 1;
            lg.ev("error", &pair.source, Some("Source folder is not available"), None)?;
            failure.get_or_insert_with(|| format!("source not available: {}", pair.source));
            store.save_folder_stat(run, &fs)?;
            continue; // never flag deletions for a folder we could not scan
        }
        let mut scan_complete = true;
        let mut fatal = false;
        let ex = Excludes::new(job, root)?;
        let excluded_here = std::cell::Cell::new(0u64);
        // Excluded entries already backed up stay on the NAS; remember them so they are not flagged as deleted.
        let excluded_paths = std::cell::RefCell::new(Vec::<(String, bool)>::new());
        let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
            let skip = ex.skips(e);
            if skip {
                excluded_here.set(excluded_here.get() + 1);
                excluded_paths.borrow_mut().push((format!("{}/{}", pair.dest, nfc(&ex.rel(e))), e.file_type().is_dir()));
            }
            !skip
        });
        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    scan_complete = false;
                    stats.warnings += 1;
                    fs.issues += 1;
                    let p = e.path().map(|p| p.display().to_string()).unwrap_or_default();
                    lg.ev("warning", &p, Some(&walk_cause(&e)), None)?;
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                if !entry.file_type().is_dir() && entry.depth() > 0 {
                    // Shortcuts, links and device files are not followed or copied. Say so instead of dropping them silently.
                    stats.skipped += 1;
                    fs.issues += 1;
                    lg.ev("skipped", &entry.path().display().to_string(), Some(LINK_SKIPPED), None)?;
                }
                continue;
            }
            stats.scanned += 1;
            fs.scanned += 1;
            let rel = match entry.path().strip_prefix(root) {
                Ok(r) => r.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"),
                Err(_) => continue,
            };
            let dest_path = format!("{}/{}", pair.dest, nfc(&rel));
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    scan_complete = false;
                    stats.warnings += 1;
                    fs.issues += 1;
                    lg.ev("warning", &entry.path().display().to_string(), Some(&walk_cause(&e)), None)?;
                    continue;
                }
            };
            let (size, mtime) = (meta.len(), mtime_of(&meta));
            if let Some(max) = job.max_file_bytes {
                if size > max {
                    // Still on the NAS if previously copied; just don't update it.
                    store.mark_seen(job.id, &dest_path, run)?;
                    stats.skipped += 1;
                    fs.issues += 1;
                    lg.ev("skipped", &rel, Some("Larger than the size limit"), Some(size))?;
                    continue;
                }
            }
            // Unchanged means: same size and mtime as last time AND the NAS still has a file of that size.
            let known = store.get_file(job.id, &dest_path)?;
            if let Some(rec) = &known {
                if rec.size == size && rec.mtime == mtime {
                    if let Ok(Some(st)) = conn.stat(&dest_path) {
                        if st.size == size {
                            store.mark_seen(job.id, &dest_path, run)?;
                            stats.unchanged += 1;
                            continue;
                        }
                    }
                }
            }
            // First time in this NAS folder: remove stale temp files from an interrupted run.
            let dir = dest_path.rsplit_once('/').map(|x| x.0).unwrap_or("").to_string();
            if cleaned.insert(dir.clone()) {
                for t in conn.list_temp_files(&dir).unwrap_or_default() {
                    let _ = conn.remove_temp(&t);
                }
            }
            let temp_seq = lg.next_seq();
            match conn.put_file(entry.path(), &dest_path, run, temp_seq) {
                Ok(put) => {
                    // Re-stat the source: if it changed while copying, record the old mtime so the next run copies again.
                    let after = std::fs::metadata(entry.path()).ok();
                    let stable = after.as_ref().map(|m| m.len() == size && mtime_of(m) == mtime).unwrap_or(false);
                    let rec_mtime = if stable { mtime } else { 0 };
                    store.upsert_file(job.id, &dest_path, &FileRec { size: put.size, mtime: rec_mtime, hash: put.hash }, run)?;
                    stats.copied += 1;
                    stats.bytes += put.size;
                    fs.copied += 1;
                    fs.bytes += put.size;
                    if known.is_some() {
                        stats.updated += 1;
                        lg.ev("updated", &rel, None, Some(put.size))?;
                    } else {
                        stats.new_files += 1;
                        lg.ev("new", &rel, None, Some(put.size))?;
                    }
                }
                Err(Error::DestinationNotReady(m)) => {
                    stats.errors += 1;
                    fs.issues += 1;
                    lg.ev("error", "", Some(&format!("Destination not ready: {m}")), None)?;
                    failure = Some(m);
                    fatal = true;
                    break;
                }
                Err(e) => {
                    // Keep the previous NAS copy valid; just report.
                    store.mark_seen(job.id, &dest_path, run)?;
                    stats.warnings += 1;
                    fs.issues += 1;
                    lg.ev("warning", &rel, Some(&e.to_string().replace(&*entry.path().to_string_lossy(), "…")), Some(size))?;
                }
            }
            if stats.copied.is_multiple_of(50) {
                progress.update(&stats);
            }
        }
        stats.excluded += excluded_here.get();
        for (p, is_dir) in excluded_paths.take() {
            if is_dir {
                store.mark_seen_under(job.id, &p, run)?;
            } else {
                store.mark_seen(job.id, &p, run)?;
            }
        }
        if cancelled || fatal {
            fs.duration_ms = folder_started.elapsed().as_millis() as u64;
            store.save_folder_stat(run, &fs)?;
            break 'folders;
        }
        if scan_complete && failure.is_none() {
            let flagged = store.flag_missing(job.id, &pair.dest, run, now)?;
            stats.newly_deleted += flagged.len() as u64;
            let prefix = format!("{}/", pair.dest);
            for p in flagged.iter().take(MAX_FLAG_ROWS) {
                lg.ev("deleted_flagged", p.strip_prefix(&prefix).unwrap_or(p), Some("No longer on this computer. Still on your NAS."), None)?;
            }
            if flagged.len() > MAX_FLAG_ROWS {
                lg.ev("run", "", Some(&format!("{} more files flagged as deleted in {}", flagged.len() - MAX_FLAG_ROWS, pair.source)), None)?;
            }
        }
        fs.duration_ms = folder_started.elapsed().as_millis() as u64;
        store.save_folder_stat(run, &fs)?;
    }

    let (status, msg) = if cancelled {
        (RunStatus::Cancelled, None)
    } else if let Some(f) = &failure {
        (RunStatus::Failed, Some(f.as_str()))
    } else if stats.warnings > 0 || stats.skipped > 0 {
        // Anything that was not copied means the backup is not complete, so it is never reported as plain success.
        (RunStatus::Warnings, None)
    } else {
        (RunStatus::Ok, None)
    };
    stats.duration_ms = started.elapsed().as_millis() as u64;
    lg.ev("run", "", Some(&match status {
        RunStatus::Cancelled => "Run cancelled. Files already copied are safe on your NAS".to_string(),
        _ => format!("Run ended: {} copied, {} unchanged, {} skipped, {} warnings", stats.copied, stats.unchanged, stats.skipped, stats.warnings),
    }), None)?;
    store.finish_run(run, now + started.elapsed().as_secs() as i64, status, &stats, msg)?;
    progress.update(&stats);
    summary(store, job.id, run)
}

/// What a verification found. Lists are capped; the `*_total` fields are exact.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct VerifyReport {
    /// Files on this computer that should be on the NAS.
    pub checked: u64,
    /// Present on the NAS with the same size.
    pub ok: u64,
    /// Not on the NAS at all.
    pub missing: Vec<String>,
    pub missing_total: u64,
    /// Changed on this computer since they were backed up. The next backup will update them.
    pub changed: Vec<String>,
    pub changed_total: u64,
    /// On the NAS, but with a size that matches neither the file here nor what Backer recorded.
    pub wrong_size: Vec<String>,
    pub wrong_size_total: u64,
    /// Files or folders that could not be read here, so they could not be checked.
    pub unreadable: Vec<String>,
    pub unreadable_total: u64,
    /// Not backed up by design.
    pub links: u64,
    pub too_large: u64,
    pub excluded: u64,
    /// Source folders that were not available.
    pub folders_unavailable: Vec<String>,
    pub cancelled: bool,
    pub duration_ms: u64,
}

const VERIFY_LIST_CAP: usize = 200;

fn push_capped(list: &mut Vec<String>, total: &mut u64, item: String) {
    *total += 1;
    if list.len() < VERIFY_LIST_CAP {
        list.push(item);
    }
}

/// Compares what is on this computer with what is on the NAS, without copying or changing anything.
/// Every file the job would back up must exist on the NAS with the same size.
pub fn verify_job(store: &Store, conn: &dyn Connector, job: &Job, cancel: &AtomicBool) -> Result<VerifyReport> {
    job.validate()?;
    conn.check_ready()?;
    let started = Instant::now();
    let mut rep = VerifyReport::default();
    'folders: for pair in &job.folders {
        let root = std::path::Path::new(&pair.source);
        if !root.is_dir() {
            rep.folders_unavailable.push(pair.source.clone());
            continue;
        }
        let ex = Excludes::new(job, root)?;
        let excluded_here = std::cell::Cell::new(0u64);
        let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
            let skip = ex.skips(e);
            if skip {
                excluded_here.set(excluded_here.get() + 1);
            }
            !skip
        });
        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                rep.cancelled = true;
                break 'folders;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    let p = e.path().map(|p| p.display().to_string()).unwrap_or_default();
                    push_capped(&mut rep.unreadable, &mut rep.unreadable_total, format!("{p} ({})", walk_cause(&e)));
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                if !entry.file_type().is_dir() && entry.depth() > 0 {
                    rep.links += 1;
                }
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                push_capped(&mut rep.unreadable, &mut rep.unreadable_total, entry.path().display().to_string());
                continue;
            };
            if job.max_file_bytes.is_some_and(|max| meta.len() > max) {
                rep.too_large += 1;
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(root) else { continue };
            let rel = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
            let dest_path = format!("{}/{}", pair.dest, nfc(&rel));
            rep.checked += 1;
            let shown = entry.path().display().to_string();
            match conn.stat(&dest_path)? {
                None => push_capped(&mut rep.missing, &mut rep.missing_total, shown),
                Some(st) if st.size == meta.len() => rep.ok += 1,
                Some(st) => {
                    let backed_up_size = store.get_file(job.id, &dest_path)?.map(|r| r.size);
                    if backed_up_size == Some(st.size) {
                        push_capped(&mut rep.changed, &mut rep.changed_total, shown);
                    } else {
                        push_capped(&mut rep.wrong_size, &mut rep.wrong_size_total, shown);
                    }
                }
            }
        }
        rep.excluded += excluded_here.get();
    }
    rep.duration_ms = started.elapsed().as_millis() as u64;
    Ok(rep)
}

/// Pages of saved checksums read from the store at a time.
const CHECKSUM_PAGE: u32 = 500;
/// A NAS that fails this many files in a row has gone away; the check stops instead of listing every file as unreadable.
const MAX_CONSECUTIVE_READ_ERRORS: u32 = 5;

enum Seen {
    Ok(u64),
    Damaged,
    Missing,
    /// Its record was removed while the check ran.
    Forgotten,
}

/// Re-reads the NAS copy of one file and compares it with the checksum Backer saved when it copied the file.
fn check_copy(conn: &dyn Connector, dest_path: &str, rec: &FileRec) -> Result<Seen> {
    match conn.stat(dest_path)? {
        None => Ok(Seen::Missing),
        Some(st) if st.size != rec.size => Ok(Seen::Damaged),
        Some(_) => Ok(if conn.hash(dest_path)? == rec.hash { Seen::Ok(rec.size) } else { Seen::Damaged }),
    }
}

/// Checks that the copies on the NAS are still intact: reads each one back and compares its BLAKE3 checksum with the
/// one saved when it was backed up. Only reads the NAS (`stat` and `hash`); it never writes, renames or deletes there,
/// and it does not need the source folders. `progress` gets (files checked, files in all).
pub fn verify_checksums(store: &Store, conn: &dyn Connector, job: &Job, cancel: &AtomicBool, progress: &dyn Fn(u64, u64)) -> Result<ChecksumReport> {
    conn.check_ready()?;
    let started = Instant::now();
    let mut rep = ChecksumReport { total: store.count_files(job.id)?, ..Default::default() };
    progress(0, rep.total);
    let mut after = String::new();
    let mut errors_in_a_row = 0u32;
    let mut since_report = 0u32;
    loop {
        let page = store.files_after(job.id, &after, CHECKSUM_PAGE)?;
        let Some((last, _)) = page.last() else { break };
        after = last.clone();
        for (path, rec) in page {
            if cancel.load(Ordering::Relaxed) {
                rep.cancelled = true;
                rep.duration_ms = started.elapsed().as_millis() as u64;
                return Ok(rep);
            }
            let mut seen = check_copy(conn, &path, &rec);
            // A backup that replaced this file while we were reading it is not damage: compare with the newest record
            // instead. If the record is gone (the user removed the file on purpose), there is nothing to report.
            if matches!(seen, Ok(Seen::Damaged) | Ok(Seen::Missing)) {
                match store.get_file(job.id, &path)? {
                    None => seen = Ok(Seen::Forgotten),
                    Some(newer) if newer != rec => seen = check_copy(conn, &path, &newer),
                    Some(_) => {}
                }
            }
            if matches!(seen, Ok(Seen::Forgotten)) {
                rep.total = rep.total.saturating_sub(1);
                continue;
            }
            rep.checked += 1;
            match seen {
                Ok(Seen::Ok(n)) => {
                    errors_in_a_row = 0;
                    rep.ok += 1;
                    rep.bytes += n;
                }
                Ok(Seen::Damaged) => {
                    errors_in_a_row = 0;
                    push_capped(&mut rep.damaged, &mut rep.damaged_total, path);
                }
                Ok(Seen::Forgotten) => {}
                Ok(Seen::Missing) => {
                    errors_in_a_row = 0;
                    push_capped(&mut rep.missing, &mut rep.missing_total, path);
                }
                // Wrong password, changed host key, NAS not the expected one: nothing more can be checked.
                Err(e @ (Error::Credentials(_) | Error::HostKey(_) | Error::DestinationNotReady(_) | Error::Cancelled)) => return Err(e),
                Err(e) => {
                    errors_in_a_row += 1;
                    if errors_in_a_row >= MAX_CONSECUTIVE_READ_ERRORS {
                        return Err(e);
                    }
                    push_capped(&mut rep.unreadable, &mut rep.unreadable_total, format!("{path} ({e})"));
                }
            }
            since_report += 1;
            if since_report == 20 {
                since_report = 0;
                progress(rep.checked, rep.total);
            }
        }
    }
    progress(rep.checked, rep.total);
    rep.duration_ms = started.elapsed().as_millis() as u64;
    Ok(rep)
}

/// Per-file rows written for newly flagged deletions in one folder; the rest are only counted.
const MAX_FLAG_ROWS: usize = 1000;

fn summary(store: &Store, _job: JobId, run: i64) -> Result<RunSummary> {
    store.get_run(run)?.ok_or_else(|| Error::Store("run missing".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use backer_connector::MountedConnector;
    use std::fs;

    struct Env {
        _s: tempfile::TempDir,
        _d: tempfile::TempDir,
        src: std::path::PathBuf,
        dst: std::path::PathBuf,
        store: Store,
        conn: MountedConnector,
        job: Job,
    }

    fn env() -> Env {
        let s = tempfile::tempdir().unwrap();
        let d = tempfile::tempdir().unwrap();
        let src = s.path().join("Documents");
        fs::create_dir_all(&src).unwrap();
        let conn = MountedConnector::new(d.path());
        conn.init_marker().unwrap();
        let store = Store::open_memory().unwrap();
        let mut job = Job { id: 0, name: "t".into(), folders: vec![FolderPair { source: src.to_string_lossy().into(), dest: "mac/Documents".into() }],
            dest_root: d.path().to_string_lossy().into(), schedule: Schedule::Manual, paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None };
        store.save_job(&mut job).unwrap();
        Env { dst: d.path().to_path_buf(), _s: s, _d: d, src, store, conn, job }
    }

    fn go(e: &Env, now: i64) -> RunSummary {
        run_job(&e.store, &e.conn, &e.job, now, &AtomicBool::new(false), &NoProgress).unwrap()
    }

    #[test]
    fn first_run_copies_second_run_skips() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        fs::create_dir_all(e.src.join("sub")).unwrap();
        fs::write(e.src.join("sub/b.txt"), b"bbb").unwrap();
        fs::write(e.src.join(".DS_Store"), b"x").unwrap();
        let r = go(&e, 1);
        assert_eq!((r.status, r.stats.copied), (RunStatus::Ok, 2));
        assert_eq!(fs::read(e.dst.join("mac/Documents/sub/b.txt")).unwrap(), b"bbb");
        assert!(!e.dst.join("mac/Documents/.DS_Store").exists());
        let r = go(&e, 2);
        assert_eq!((r.stats.copied, r.stats.unchanged), (0, 2));
    }

    #[test]
    fn changed_file_is_recopied() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        go(&e, 1);
        fs::write(e.src.join("a.txt"), b"aaaa-longer").unwrap();
        let r = go(&e, 2);
        assert_eq!(r.stats.copied, 1);
        assert_eq!(fs::read(e.dst.join("mac/Documents/a.txt")).unwrap(), b"aaaa-longer");
    }

    #[test]
    fn nas_file_removed_behind_our_back_is_restored() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        go(&e, 1);
        fs::remove_file(e.dst.join("mac/Documents/a.txt")).unwrap();
        assert_eq!(go(&e, 2).stats.copied, 1);
        assert!(e.dst.join("mac/Documents/a.txt").exists());
    }

    #[test]
    fn source_deletion_is_flagged_and_nas_copy_kept() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        fs::write(e.src.join("b.txt"), b"bbb").unwrap();
        go(&e, 1);
        fs::remove_file(e.src.join("b.txt")).unwrap();
        let r = go(&e, 2);
        assert_eq!(r.stats.newly_deleted, 1);
        assert!(e.dst.join("mac/Documents/b.txt").exists(), "must never delete on the NAS");
        let del = e.store.list_deleted(None).unwrap();
        assert_eq!(del.len(), 1);
        assert_eq!(del[0].dest_path, "mac/Documents/b.txt");
    }

    #[test]
    fn missing_source_folder_flags_nothing() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        go(&e, 1);
        fs::remove_dir_all(&e.src).unwrap(); // e.g. an unmounted external drive
        let r = go(&e, 2);
        assert_eq!(r.status, RunStatus::Failed);
        assert!(e.store.list_deleted(None).unwrap().is_empty());
    }

    #[test]
    fn unmounted_destination_fails_fast() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        fs::remove_file(e.dst.join(DEST_MARKER)).unwrap();
        let r = go(&e, 1);
        assert_eq!(r.status, RunStatus::Failed);
        assert!(!e.dst.join("mac").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_subfolder_warns_and_flags_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let e = env();
        fs::create_dir_all(e.src.join("locked")).unwrap();
        fs::write(e.src.join("locked/x.txt"), b"x").unwrap();
        go(&e, 1);
        fs::set_permissions(e.src.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        let r = go(&e, 2);
        fs::set_permissions(e.src.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        if unsafe { libc_geteuid() } != 0 {
            assert_eq!(r.status, RunStatus::Warnings);
            assert!(e.store.list_deleted(None).unwrap().is_empty());
        }
    }
    #[cfg(unix)]
    unsafe fn libc_geteuid() -> u32 {
        extern "C" { fn geteuid() -> u32; }
        geteuid()
    }

    #[test]
    fn stale_temp_files_are_cleaned() {
        let e = env();
        fs::create_dir_all(e.dst.join("mac/Documents")).unwrap();
        fs::write(e.dst.join("mac/Documents/.backer-9-9.part"), b"junk").unwrap();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        go(&e, 1);
        assert!(!e.dst.join("mac/Documents/.backer-9-9.part").exists());
    }

    #[test]
    fn cancel_stops_run() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        let r = run_job(&e.store, &e.conn, &e.job, 1, &AtomicBool::new(true), &NoProgress).unwrap();
        assert_eq!(r.status, RunStatus::Cancelled);
    }

    #[test]
    fn size_limit_skips_but_keeps_existing_copy_listed() {
        let mut e = env();
        fs::write(e.src.join("big.bin"), vec![0u8; 100]).unwrap();
        go(&e, 1);
        e.job.max_file_bytes = Some(10);
        let r = go(&e, 2);
        assert_eq!(r.stats.skipped, 1);
        assert!(e.store.list_deleted(None).unwrap().is_empty());
    }

    fn all_rows(e: &Env, run: i64) -> Vec<LogRow> {
        e.store.run_log(run, &LogQuery { limit: 1000, ..Default::default() }).unwrap().rows
    }

    #[test]
    fn log_records_new_then_updated_with_sizes_times_and_ordered_seq() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        let r1 = go(&e, 1000);
        fs::write(e.src.join("a.txt"), b"aaaa-longer").unwrap();
        let r2 = go(&e, 2000);
        let rows1 = all_rows(&e, r1.id);
        let new: Vec<_> = rows1.iter().filter(|r| r.kind == "new").collect();
        assert_eq!((new.len(), new[0].path.as_str(), new[0].size), (1, "a.txt", Some(3)));
        assert_eq!((r1.stats.new_files, r1.stats.updated), (1, 0));
        let rows2 = all_rows(&e, r2.id);
        assert_eq!(rows2.iter().filter(|r| r.kind == "updated").count(), 1);
        assert_eq!((r2.stats.new_files, r2.stats.updated), (0, 1));
        for rows in [&rows1, &rows2] {
            assert!(rows.windows(2).all(|w| w[0].seq < w[1].seq), "seq must strictly increase");
            assert!(rows.iter().all(|r| r.ts >= 1000));
            assert_eq!(rows.first().unwrap().kind, "run");
            assert_eq!(rows.last().unwrap().kind, "run");
        }
    }

    fn with_excludes(mut e: Env, pats: &[&str]) -> Env {
        e.job.exclude_names = pats.iter().map(|p| p.to_string()).collect();
        e.store.save_job(&mut e.job).unwrap();
        e
    }

    #[test]
    fn exclude_patterns_skip_matching_files_and_folders() {
        let e = with_excludes(env(), &["*.tmp", "node_modules/", "/build"]);
        fs::write(e.src.join("keep.txt"), b"k").unwrap();
        fs::write(e.src.join("scratch.TMP"), b"t").unwrap();
        fs::create_dir_all(e.src.join("web/node_modules/pkg")).unwrap();
        fs::write(e.src.join("web/node_modules/pkg/i.js"), b"j").unwrap();
        fs::write(e.src.join("web/node_modules.txt"), b"n").unwrap(); // not a folder, kept
        fs::create_dir_all(e.src.join("build")).unwrap();
        fs::write(e.src.join("build/out.o"), b"o").unwrap();
        fs::create_dir_all(e.src.join("src/build")).unwrap(); // only the top-level build is anchored
        fs::write(e.src.join("src/build/b.rs"), b"b").unwrap();
        let r = go(&e, 1);
        assert_eq!((r.status, r.stats.copied, r.stats.excluded), (RunStatus::Ok, 3, 3));
        let nas = e.dst.join("mac/Documents");
        assert!(nas.join("keep.txt").exists() && nas.join("web/node_modules.txt").exists() && nas.join("src/build/b.rs").exists());
        assert!(!nas.join("scratch.TMP").exists() && !nas.join("web/node_modules").exists() && !nas.join("build").exists());
    }

    #[test]
    fn excluding_something_already_backed_up_leaves_it_on_the_nas_unflagged() {
        let mut e = env();
        fs::write(e.src.join("a.txt"), b"a").unwrap();
        fs::write(e.src.join("old.tmp"), b"t").unwrap();
        fs::create_dir_all(e.src.join("cache/deep")).unwrap();
        fs::write(e.src.join("cache/deep/c.bin"), b"c").unwrap();
        assert_eq!(go(&e, 1).stats.copied, 3);
        e = with_excludes(e, &["*.tmp", "cache/"]);
        let r = go(&e, 2);
        assert_eq!((r.stats.newly_deleted, r.stats.excluded), (0, 2));
        assert!(e.store.list_deleted(Some(e.job.id)).unwrap().is_empty());
        assert!(e.dst.join("mac/Documents/old.tmp").exists(), "the NAS copy is never touched");
        assert!(e.dst.join("mac/Documents/cache/deep/c.bin").exists());
    }

    #[test]
    fn verify_uses_the_same_exclude_patterns() {
        let e = with_excludes(env(), &["*.tmp", "node_modules/"]);
        fs::write(e.src.join("a.txt"), b"a").unwrap();
        fs::write(e.src.join("b.tmp"), b"b").unwrap();
        fs::create_dir_all(e.src.join("node_modules")).unwrap();
        fs::write(e.src.join("node_modules/x.js"), b"x").unwrap();
        go(&e, 1);
        let v = verify_job(&e.store, &e.conn, &e.job, &AtomicBool::new(false)).unwrap();
        assert_eq!((v.checked, v.ok, v.missing_total, v.excluded), (1, 1, 0, 2));
    }

    #[test]
    fn invalid_pattern_stops_the_run_before_anything_is_copied() {
        let mut e = env();
        fs::write(e.src.join("a.txt"), b"a").unwrap();
        e.job.exclude_names = vec!["[bad".into()];
        assert!(run_job(&e.store, &e.conn, &e.job, 1, &AtomicBool::new(false), &NoProgress).is_err());
        assert!(!e.dst.join("mac/Documents/a.txt").exists());
    }

    #[test]
    fn run_stats_record_trigger_excludes_duration_and_folders() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        fs::write(e.src.join(".DS_Store"), b"x").unwrap();
        fs::create_dir_all(e.src.join("Thumbs.db")).unwrap(); // excluded folder counts once
        let r = run_job_as(&e.store, &e.conn, &e.job, 50, &AtomicBool::new(false), &NoProgress, Trigger::Schedule).unwrap();
        assert_eq!((r.stats.trigger.as_str(), r.stats.excluded, r.stats.errors), ("schedule", 2, 0));
        let rep = e.store.run_report(r.id).unwrap();
        assert_eq!(rep.folders.len(), 1);
        assert_eq!((rep.folders[0].scanned, rep.folders[0].copied, rep.folders[0].bytes, rep.folders[0].issues), (1, 1, 3, 0));
        assert!(rep.run.finished.unwrap() >= 50);
        assert_eq!(go(&e, 60).stats.trigger, "manual");
    }

    #[test]
    fn destination_not_ready_is_logged_and_counted() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        fs::remove_file(e.dst.join(DEST_MARKER)).unwrap();
        let r = go(&e, 1);
        assert_eq!((r.status, r.stats.errors), (RunStatus::Failed, 1));
        let rows = all_rows(&e, r.id);
        let kinds: Vec<_> = rows.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(kinds, vec!["run", "error", "run"]);
        assert!(rows[1].detail.as_deref().unwrap().contains("destination not ready"));
        let rep = e.store.run_report(r.id).unwrap();
        assert_eq!(rep.issues.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_reported_as_skipped_and_the_run_is_not_plain_success() {
        let e = env();
        fs::write(e.src.join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(e.src.join("real"), e.src.join("shortcut")).unwrap();
        let r = go(&e, 1);
        assert_eq!((r.stats.skipped, r.status), (1, RunStatus::Warnings));
        let rep = e.store.run_report(r.id).unwrap();
        assert_eq!(rep.issues[0].cause, LINK_SKIPPED);
    }

    #[test]
    fn missing_source_is_an_error_row_not_a_warning() {
        let e = env();
        fs::remove_dir_all(&e.src).unwrap();
        let r = go(&e, 1);
        assert_eq!((r.stats.errors, r.stats.warnings), (1, 0));
        assert!(all_rows(&e, r.id).iter().any(|x| x.kind == "error" && x.detail.as_deref() == Some("Source folder is not available")));
    }

    #[test]
    fn newly_flagged_deletions_are_logged_by_path() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        fs::write(e.src.join("b.txt"), b"bbb").unwrap();
        go(&e, 1);
        fs::remove_file(e.src.join("b.txt")).unwrap();
        let r = go(&e, 2);
        let rows = all_rows(&e, r.id);
        let flagged: Vec<_> = rows.iter().filter(|r| r.kind == "deleted_flagged").map(|r| r.path.as_str()).collect();
        assert_eq!(flagged, vec!["b.txt"]);
        assert_eq!(r.stats.newly_deleted, 1);
    }

    #[test]
    fn size_limit_skip_is_logged_with_size_and_counts_as_a_folder_issue() {
        let mut e = env();
        fs::write(e.src.join("big.bin"), vec![0u8; 100]).unwrap();
        e.job.max_file_bytes = Some(10);
        let r = go(&e, 1);
        let rep = e.store.run_report(r.id).unwrap();
        assert_eq!((rep.issues[0].kind.as_str(), rep.issues[0].cause.as_str(), rep.issues[0].count), ("skipped", "Larger than the size limit", 1));
        assert_eq!(all_rows(&e, r.id).iter().find(|x| x.kind == "skipped").unwrap().size, Some(100));
        assert_eq!(rep.folders[0].issues, 1);
    }

    #[test]
    fn cancelled_run_is_logged_and_keeps_partial_folder_stats() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        let r = run_job(&e.store, &e.conn, &e.job, 1, &AtomicBool::new(true), &NoProgress).unwrap();
        assert_eq!(r.status, RunStatus::Cancelled);
        assert!(all_rows(&e, r.id).last().unwrap().detail.as_deref().unwrap().contains("cancelled"));
        assert_eq!(e.store.run_report(r.id).unwrap().folders.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_folders_group_under_one_cause() {
        use std::os::unix::fs::PermissionsExt;
        let e = env();
        for d in ["l1", "l2"] {
            fs::create_dir_all(e.src.join(d)).unwrap();
            fs::write(e.src.join(d).join("x.txt"), b"x").unwrap();
            fs::set_permissions(e.src.join(d), fs::Permissions::from_mode(0o000)).unwrap();
        }
        let r = go(&e, 1);
        for d in ["l1", "l2"] {
            fs::set_permissions(e.src.join(d), fs::Permissions::from_mode(0o755)).unwrap();
        }
        if unsafe { libc_geteuid() } != 0 {
            let rep = e.store.run_report(r.id).unwrap();
            assert_eq!((rep.issues[0].cause.as_str(), rep.issues[0].count), (FOLDER_LOCKED, 2));
            assert_eq!(r.stats.warnings, 2);
        }
    }

    /// Every file under `dir` with its bytes and modified time, to prove a check changed nothing.
    fn snapshot(dir: &std::path::Path) -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
        let mut v: Vec<_> = WalkDir::new(dir).into_iter().flatten().filter(|e| e.file_type().is_file())
            .map(|e| (e.path().display().to_string(), fs::read(e.path()).unwrap(), e.metadata().unwrap().modified().unwrap())).collect();
        v.sort();
        v
    }

    fn check(e: &Env) -> ChecksumReport {
        verify_checksums(&e.store, &e.conn, &e.job, &AtomicBool::new(false), &|_, _| {}).unwrap()
    }

    #[test]
    fn checksum_check_passes_on_a_clean_backup_and_never_changes_the_nas() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        fs::create_dir_all(e.src.join("sub")).unwrap();
        fs::write(e.src.join("sub/b.txt"), b"bbbb").unwrap();
        go(&e, 1);
        let before = snapshot(&e.dst);
        let r = check(&e);
        assert_eq!((r.total, r.checked, r.ok, r.bytes, r.problems()), (2, 2, 2, 7, 0));
        assert_eq!(snapshot(&e.dst), before, "a check only reads");
    }

    #[test]
    fn checksum_check_finds_flipped_bytes_truncation_and_missing_files_and_leaves_them_alone() {
        let e = env();
        for (n, c) in [("flip", "same-size"), ("short", "will be cut"), ("gone", "bye"), ("fine", "ok")] {
            fs::write(e.src.join(n), c).unwrap();
        }
        go(&e, 1);
        let d = e.dst.join("mac/Documents");
        fs::write(d.join("flip"), b"same-sizX").unwrap(); // same length, different contents
        fs::write(d.join("short"), b"will").unwrap();
        fs::remove_file(d.join("gone")).unwrap();
        let before = snapshot(&e.dst);
        let r = check(&e);
        assert_eq!((r.ok, r.damaged_total, r.missing_total, r.unreadable_total), (1, 2, 1, 0));
        assert!(r.damaged.iter().any(|p| p.ends_with("flip")) && r.damaged.iter().any(|p| p.ends_with("short")));
        assert_eq!(r.missing, vec!["mac/Documents/gone".to_string()]);
        // Nothing was repaired, replaced or deleted by checking.
        assert_eq!(snapshot(&e.dst), before);
        assert!(!d.join("gone").exists());
    }

    #[test]
    fn checksum_check_needs_no_source_folder_and_a_changed_source_is_not_damage() {
        let e = env();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        go(&e, 1);
        fs::write(e.src.join("a.txt"), b"edited since the backup").unwrap();
        assert_eq!(check(&e).problems(), 0);
        fs::remove_dir_all(&e.src).unwrap();
        assert_eq!(check(&e).ok, 1);
    }

    #[test]
    fn checksum_check_can_be_stopped_and_refuses_an_unmounted_nas() {
        let e = env();
        for i in 0..5 {
            fs::write(e.src.join(format!("f{i}")), b"x").unwrap();
        }
        go(&e, 1);
        let stop = AtomicBool::new(true);
        let r = verify_checksums(&e.store, &e.conn, &e.job, &stop, &|_, _| {}).unwrap();
        assert!(r.cancelled);
        assert_eq!(r.checked, 0);
        fs::remove_file(e.dst.join(DEST_MARKER)).unwrap();
        assert!(verify_checksums(&e.store, &e.conn, &e.job, &AtomicBool::new(false), &|_, _| {}).is_err());
    }

    #[test]
    fn checksum_check_pages_through_more_files_than_one_page() {
        let e = env();
        let n = CHECKSUM_PAGE as usize + 25;
        for i in 0..n {
            fs::write(e.src.join(format!("f{i:04}")), b"x").unwrap();
        }
        go(&e, 1);
        let seen = std::cell::Cell::new((0, 0));
        let r = verify_checksums(&e.store, &e.conn, &e.job, &AtomicBool::new(false), &|c, t| seen.set((c, t))).unwrap();
        assert_eq!((r.checked, r.ok), (n as u64, n as u64));
        assert_eq!(seen.get(), (n as u64, n as u64));
    }
}
