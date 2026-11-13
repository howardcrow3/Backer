//! Integration test for FR-18 / spec 8.2-8.3: an unreachable NAS is retried, then the run is
//! recorded as failed and the user is notified once. Drives the real agent, store, engine and
//! mounted-path connector; only the wait between retries is shortened and notifications are recorded.
use backer_agent::notify::RecordingNotifier;
use backer_agent::retry::RetryPolicy;
use backer_agent::{handle_ui, Agent};
use backer_core::{FolderPair, Job, RunStatus, Schedule};
use jiff::tz::TimeZone;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Fixture {
    _t: tempfile::TempDir,
    src: PathBuf,
    nas: PathBuf,
    agent: Arc<Agent>,
    notes: Arc<RecordingNotifier>,
    id: i64,
}

fn fixture(delay: Duration) -> Fixture {
    let t = tempfile::tempdir().unwrap();
    let (src, nas) = (t.path().join("src"), t.path().join("nas"));
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&nas).unwrap();
    fs::write(src.join("a.txt"), b"hello").unwrap();
    let notes = Arc::new(RecordingNotifier::default());
    let agent = Agent::new_sync_with(
        t.path().join("db.sqlite"),
        TimeZone::get("America/Los_Angeles").unwrap(),
        RetryPolicy { retries: 3, delay },
        notes.clone(),
    )
    .unwrap();
    let ui = |m: &str, p: Value| handle_ui(&agent, &json!({"method": m, "params": p}));
    assert_eq!(ui("init_destination", json!({"path": nas}))["ok"], true);
    let job = Job {
        id: 0,
        name: "Documents".into(),
        folders: vec![FolderPair { source: src.to_string_lossy().into(), dest: "mac/src".into() }],
        dest_root: nas.to_string_lossy().into(),
        schedule: Schedule::Daily { hour: 2, minute: 0 },
        paused: false,
        catch_up: true,
        exclude_names: vec![],
        max_file_bytes: None,
    };
    let id = ui("save_job", json!({"job": job}))["result"]["job_id"].as_i64().unwrap();
    Fixture { _t: t, src, nas, agent, notes, id }
}

/// Simulates the NAS going away: the mount point is gone.
fn unplug(nas: &Path) {
    fs::rename(nas, nas.with_extension("away")).unwrap();
}
fn plug_back(nas: &Path) {
    fs::rename(nas.with_extension("away"), nas).unwrap();
}

#[test]
fn unreachable_nas_is_retried_then_the_run_fails_and_the_user_is_notified_once() {
    let f = fixture(Duration::from_millis(200));
    unplug(&f.nas);
    let t = Instant::now();
    f.agent.start_job(f.id).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(600), "3 retries 200 ms apart, waited only {:?}", t.elapsed());

    let runs = f.agent.store().unwrap().list_runs(f.id, 10).unwrap();
    assert_eq!(runs.len(), 1, "one failed run, not one per attempt");
    assert_eq!(runs[0].status, RunStatus::Failed);

    let notes = f.notes.0.lock().unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    let (title, body) = &notes[0];
    assert!(title.contains("Documents") && title.contains("failed"), "{title}");
    assert!(body.contains("could not reach the NAS") && body.contains("3 more times"), "{body}");
}

#[test]
fn nas_that_comes_back_during_the_retries_gets_a_normal_run_and_no_notification() {
    let f = fixture(Duration::from_millis(300));
    unplug(&f.nas);
    let nas = f.nas.clone();
    let back = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        plug_back(&nas);
    });
    f.agent.start_job(f.id).unwrap();
    back.join().unwrap();

    let runs = f.agent.store().unwrap().list_runs(f.id, 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Ok, "{:?}", runs[0]);
    assert_eq!(fs::read(f.nas.join("mac/src/a.txt")).unwrap(), b"hello");
    assert!(f.src.join("a.txt").exists());
    assert!(f.notes.0.lock().unwrap().is_empty());
}

#[test]
fn stopping_a_job_while_it_waits_for_the_nas_ends_the_wait_without_a_notification() {
    let f = fixture(Duration::from_secs(600));
    unplug(&f.nas);
    let (agent, id) = (f.agent.clone(), f.id);
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        assert!(agent.cancel_job(id), "the job should be waiting, so it counts as running");
    });
    let t = Instant::now();
    f.agent.start_job(f.id).unwrap();
    stopper.join().unwrap();
    assert!(t.elapsed() < Duration::from_secs(10), "cancel must end a 10-minute wait at once");
    assert!(f.notes.0.lock().unwrap().is_empty());
    assert!(!f.agent.is_running(f.id));
}

#[test]
fn a_reachable_nas_is_not_delayed() {
    let f = fixture(Duration::from_secs(600));
    let t = Instant::now();
    f.agent.start_job(f.id).unwrap();
    assert!(t.elapsed() < Duration::from_secs(10));
    let runs = f.agent.store().unwrap().list_runs(f.id, 10).unwrap();
    assert_eq!(runs[0].status, RunStatus::Ok);
    assert!(f.notes.0.lock().unwrap().is_empty());
}
