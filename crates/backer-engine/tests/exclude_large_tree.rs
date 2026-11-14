//! Integration test: exclude patterns on a tree of well over 100,000 files, through the public engine API.
use backer_connector::MountedConnector;
use backer_core::*;
use backer_engine::{run_job, verify_job, NoProgress};
use backer_store::Store;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

const PROJECTS: usize = 100;
const MODULES_PER_PROJECT: usize = 1_000; // inside node_modules/: pruned without being read
const TMP_PER_PROJECT: usize = 50; // *.tmp files next to real ones
const KEEP_PER_PROJECT: usize = 20; // the files that are actually backed up

fn build_tree(src: &Path) -> usize {
    let mut total = 0;
    for p in 0..PROJECTS {
        let proj = src.join(format!("project{p:03}"));
        let nm = proj.join("node_modules/pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::create_dir_all(proj.join("src")).unwrap();
        for i in 0..MODULES_PER_PROJECT {
            fs::write(nm.join(format!("m{i}.js")), b"x").unwrap();
        }
        for i in 0..TMP_PER_PROJECT {
            fs::write(proj.join(format!("src/scratch{i}.tmp")), b"t").unwrap();
        }
        for i in 0..KEEP_PER_PROJECT {
            fs::write(proj.join(format!("src/file{i}.txt")), format!("{p}-{i}")).unwrap();
        }
        total += MODULES_PER_PROJECT + TMP_PER_PROJECT + KEEP_PER_PROJECT;
    }
    total
}

#[test]
fn excludes_on_a_tree_of_over_100k_files() {
    let s = tempfile::tempdir().unwrap();
    let d = tempfile::tempdir().unwrap();
    let src = s.path().join("Code");
    let files = build_tree(&src);
    assert!(files > 100_000, "tree has {files} files");

    let conn = MountedConnector::new(d.path());
    conn.init_marker().unwrap();
    let store = Store::open_memory().unwrap();
    let mut job = Job { id: 0, name: "big".into(), folders: vec![FolderPair { source: src.to_string_lossy().into(), dest: "mac/Code".into() }],
        dest_root: d.path().to_string_lossy().into(), schedule: Schedule::Manual, paused: false, catch_up: true,
        exclude_names: vec!["*.tmp".into(), "node_modules/".into()], max_file_bytes: None };
    store.save_job(&mut job).unwrap();

    let t = Instant::now();
    let r = run_job(&store, &conn, &job, 1, &AtomicBool::new(false), &NoProgress).unwrap();
    eprintln!("run over {files} files took {:?}", t.elapsed());
    let kept = PROJECTS * KEEP_PER_PROJECT;
    assert_eq!(r.status, RunStatus::Ok);
    assert_eq!(r.stats.copied as usize, kept);
    assert_eq!(r.stats.scanned as usize, kept, "excluded files are never looked at");
    // One node_modules folder per project, plus every .tmp file.
    assert_eq!(r.stats.excluded as usize, PROJECTS * (1 + TMP_PER_PROJECT));
    assert!(!d.path().join("mac/Code/project000/node_modules").exists());
    assert!(!d.path().join("mac/Code/project000/src/scratch0.tmp").exists());
    assert_eq!(fs::read_to_string(d.path().join("mac/Code/project099/src/file19.txt")).unwrap(), "99-19");

    // A second run finds nothing to do and flags nothing as deleted.
    let r = run_job(&store, &conn, &job, 2, &AtomicBool::new(false), &NoProgress).unwrap();
    assert_eq!((r.stats.copied, r.stats.unchanged as usize, r.stats.newly_deleted), (0, kept, 0));

    let v = verify_job(&store, &conn, &job, &AtomicBool::new(false)).unwrap();
    assert_eq!((v.checked as usize, v.ok as usize, v.missing_total), (kept, kept, 0));
}

#[test]
fn matching_200k_paths_is_fast() {
    let set = ExcludeSet::new(&["*.tmp", "node_modules/", "**/cache/**", "/build", "~$*", "*.{bak,swp}", "Thumbs.db"]).unwrap();
    let paths: Vec<String> = (0..200_000).map(|i| format!("dir{}/sub{}/file{i}.{}", i % 97, i % 13, ["txt", "tmp", "rs", "bak"][i % 4])).collect();
    let t = Instant::now();
    let hits = paths.iter().filter(|p| set.is_excluded(p, false)).count();
    let took = t.elapsed();
    eprintln!("matched 200k paths in {took:?}");
    assert_eq!(hits, 100_000);
    // Generous bound so slow CI machines pass; the point is that matching does not dominate a run.
    assert!(took.as_secs() < 20, "took {took:?}");
}
