//! Integration: "back up when files change" with a real file watcher, the real agent, store and engine.
use backer_agent::{handle_ui, Agent};
use jiff::{tz::TimeZone, Span, Timestamp};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct T {
    _t: tempfile::TempDir,
    src: PathBuf,
    dst: PathBuf,
    agent: Arc<Agent>,
}

fn setup() -> T {
    let t = tempfile::tempdir().unwrap();
    let (src, dst) = (t.path().join("src"), t.path().join("nas"));
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    fs::write(src.join("a.txt"), b"1").unwrap();
    let agent = Agent::new_sync(t.path().join("db.sqlite"), TimeZone::UTC).unwrap();
    assert_eq!(ui(&agent, "init_destination", json!({"path": dst}))["ok"], true);
    T { _t: t, src, dst, agent }
}

fn ui(agent: &Arc<Agent>, m: &str, p: Value) -> Value {
    handle_ui(agent, &json!({"method": m, "params": p}))
}

fn save(t: &T, schedule: Value, exclude: Value) -> i64 {
    let job = json!({"id": 0, "name": "j", "dest_root": t.dst, "schedule": schedule, "paused": false, "catch_up": true,
        "exclude_names": exclude, "max_file_bytes": null, "folders": [{"source": t.src, "dest": "mac/src"}]});
    let r = ui(&t.agent, "save_job", json!({"job": job}));
    r["result"]["job_id"].as_i64().unwrap_or_else(|| panic!("{r}"))
}

fn secs(n: i64) -> Span {
    Span::new().seconds(n)
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

fn at(offset: i64) -> Timestamp {
    backer_agent::now_ts().checked_add(secs(offset)).unwrap()
}

#[test]
fn a_changed_file_is_backed_up_once_the_folder_has_been_quiet() {
    let t = setup();
    let id = save(&t, json!({"kind": "on_change", "quiet_secs": 30}), json!([]));

    // First tick: the job has never run, so it backs up at once and starts watching.
    assert_eq!(t.agent.tick(backer_agent::now_ts()).unwrap(), vec![id]);
    assert!(t.dst.join("mac/src/a.txt").exists());
    assert!(!t.agent.change_pending(id), "the first run covers what was reported so far");
    assert!(t.agent.tick(at(60)).unwrap().is_empty(), "nothing changed, so nothing runs");

    // A new file is reported by the OS...
    fs::write(t.src.join("b.txt"), b"2").unwrap();
    wait_for("the file event", || t.agent.change_pending(id));
    // ...but the backup waits for 30 quiet seconds.
    assert!(t.agent.tick(at(10)).unwrap().is_empty());
    assert!(!t.dst.join("mac/src/b.txt").exists());
    assert_eq!(t.agent.tick(at(31)).unwrap(), vec![id]);
    assert!(t.dst.join("mac/src/b.txt").exists());
    let runs = t.agent.store().unwrap().list_runs(id, 5).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.stats.trigger == "schedule"));
    // The change is now backed up: no further run.
    assert!(!t.agent.change_pending(id));
    assert!(t.agent.tick(at(120)).unwrap().is_empty());
}

#[test]
fn excluded_files_and_paused_jobs_do_not_start_backups() {
    let t = setup();
    let id = save(&t, json!({"kind": "on_change", "quiet_secs": 30}), json!(["*.tmp", "cache/"]));
    assert_eq!(t.agent.tick(backer_agent::now_ts()).unwrap(), vec![id]);

    fs::create_dir_all(t.src.join("cache")).unwrap();
    fs::write(t.src.join("cache/x.bin"), b"x").unwrap();
    fs::write(t.src.join("scratch.tmp"), b"x").unwrap();
    // A real change right after, so we know the watcher has caught up with the excluded ones.
    fs::write(t.src.join("real.txt"), b"y").unwrap();
    wait_for("the real change", || t.agent.change_pending(id));
    assert_eq!(t.agent.tick(at(31)).unwrap(), vec![id]);
    assert!(t.dst.join("mac/src/real.txt").exists());
    assert!(!t.dst.join("mac/src/scratch.tmp").exists());

    // Only excluded files changed: stays quiet.
    fs::write(t.src.join("more.tmp"), b"z").unwrap();
    fs::write(t.src.join("cache/y.bin"), b"z").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!t.agent.change_pending(id));
    assert!(t.agent.tick(at(90)).unwrap().is_empty());

    // Pausing drops the watcher; changes are not tracked until resumed.
    ui(&t.agent, "pause_job", json!({"job_id": id}));
    fs::write(t.src.join("while-paused.txt"), b"p").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(t.agent.tick(at(200)).unwrap().is_empty());
    assert!(!t.agent.change_pending(id));
}
