//! Run pipeline: scan each source folder, copy what changed, flag what vanished.
//! The engine never deletes anything on the NAS.
use backer_connector::TempCleaner;
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

fn excluded(job: &Job, name: &str) -> bool {
    DEFAULT_EXCLUDES.contains(&name) || job.exclude_names.iter().any(|e| e == name)
}

/// Short, path-free reason for a file-system error, so identical problems group together in the run report.
fn io_cause(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "Permission denied".into(),
        std::io::ErrorKind::NotFound => "File vanished during backup".into(),
        _ => strip_os_code(&e.to_string()),
    }
}

fn walk_cause(e: &walkdir::Error) -> String {
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
        let excluded_here = std::cell::Cell::new(0u64);
        let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
            let skip = e.depth() > 0 && excluded(job, &e.file_name().to_string_lossy());
            if skip {
                excluded_here.set(excluded_here.get() + 1);
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
                    lg.ev("warning", &rel, Some(&walk_cause(&e)), None)?;
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
            if stats.copied % 50 == 0 {
                progress.update(&stats);
            }
        }
        stats.excluded += excluded_here.get();
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
    } else if stats.warnings > 0 {
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
            assert_eq!((rep.issues[0].cause.as_str(), rep.issues[0].count), ("Permission denied", 2));
            assert_eq!(r.stats.warnings, 2);
        }
    }
}
