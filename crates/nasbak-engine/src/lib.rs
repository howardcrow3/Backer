//! Run pipeline: scan each source folder, copy what changed, flag what vanished.
//! The engine never deletes anything on the NAS.
use nasbak_connector::TempCleaner;
use nasbak_core::*;
use nasbak_store::{FileRec, Store};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
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

pub fn run_job(
    store: &Store,
    conn: &dyn TempCleaner,
    job: &Job,
    now: i64,
    cancel: &AtomicBool,
    progress: &dyn Progress,
) -> Result<RunSummary> {
    job.validate()?;
    let run = store.begin_run(job.id, now)?;
    let mut stats = RunStats::default();
    let mut seq = 0u64;
    let mut failure: Option<String> = None;
    let mut cancelled = false;

    // Refuse to run against an unmounted or wrong destination.
    if let Err(e) = conn.check_ready() {
        let msg = e.to_string();
        store.finish_run(run, now, RunStatus::Failed, &stats, Some(&msg))?;
        return summary(store, job.id, run);
    }

    let mut cleaned: HashSet<String> = HashSet::new();

    'folders: for pair in &job.folders {
        let root = Path::new(&pair.source);
        if !root.is_dir() {
            stats.warnings += 1;
            store.log(run, seq, "warning", &pair.source, Some("source folder is not available"))?;
            seq += 1;
            failure.get_or_insert_with(|| format!("source not available: {}", pair.source));
            continue; // never flag deletions for a folder we could not scan
        }
        let mut scan_complete = true;
        let walker = WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
            e.depth() == 0 || !excluded(job, &e.file_name().to_string_lossy())
        });
        for entry in walker {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break 'folders;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    scan_complete = false;
                    stats.warnings += 1;
                    let p = e.path().map(|p| p.display().to_string()).unwrap_or_default();
                    store.log(run, seq, "warning", &p, Some(&e.to_string()))?;
                    seq += 1;
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            stats.scanned += 1;
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
                    store.log(run, seq, "warning", &rel, Some(&e.to_string()))?;
                    seq += 1;
                    continue;
                }
            };
            let (size, mtime) = (meta.len(), mtime_of(&meta));
            if let Some(max) = job.max_file_bytes {
                if size > max {
                    // Still on the NAS if previously copied; just don't update it.
                    store.mark_seen(job.id, &dest_path, run)?;
                    stats.skipped += 1;
                    store.log(run, seq, "skipped", &rel, Some("larger than the size limit"))?;
                    seq += 1;
                    continue;
                }
            }
            // Unchanged means: same size and mtime as last time AND the NAS still has a file of that size.
            if let Some(rec) = store.get_file(job.id, &dest_path)? {
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
            seq += 1;
            match conn.put_file(entry.path(), &dest_path, run, seq) {
                Ok(put) => {
                    // Re-stat the source: if it changed while copying, record the old mtime so the next run copies again.
                    let after = std::fs::metadata(entry.path()).ok();
                    let stable = after.as_ref().map(|m| m.len() == size && mtime_of(m) == mtime).unwrap_or(false);
                    let rec_mtime = if stable { mtime } else { 0 };
                    store.upsert_file(job.id, &dest_path, &FileRec { size: put.size, mtime: rec_mtime, hash: put.hash }, run)?;
                    stats.copied += 1;
                    stats.bytes += put.size;
                    store.log(run, seq, "copied", &rel, None)?;
                }
                Err(Error::DestinationNotReady(m)) => {
                    failure = Some(m);
                    break 'folders;
                }
                Err(e) => {
                    // Keep the previous NAS copy valid; just report.
                    store.mark_seen(job.id, &dest_path, run)?;
                    stats.warnings += 1;
                    store.log(run, seq, "warning", &rel, Some(&e.to_string()))?;
                }
            }
            if stats.copied % 50 == 0 {
                progress.update(&stats);
            }
        }
        if scan_complete && failure.is_none() {
            stats.newly_deleted += store.flag_missing(job.id, &pair.dest, run, now)?;
        }
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
    store.finish_run(run, now, status, &stats, msg)?;
    progress.update(&stats);
    summary(store, job.id, run)
}

fn summary(store: &Store, job: JobId, run: i64) -> Result<RunSummary> {
    store.list_runs(job, 1)?.into_iter().find(|r| r.id == run).ok_or_else(|| Error::Store("run missing".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nasbak_connector::MountedConnector;
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
        fs::write(e.dst.join("mac/Documents/.nasbak-9-9.part"), b"junk").unwrap();
        fs::write(e.src.join("a.txt"), b"aaa").unwrap();
        go(&e, 1);
        assert!(!e.dst.join("mac/Documents/.nasbak-9-9.part").exists());
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
}
