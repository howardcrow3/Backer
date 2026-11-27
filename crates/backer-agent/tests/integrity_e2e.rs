//! Integration: checking the NAS copies against the saved checksums, through the real agent, store, engine and app API.
//! The check must report damage and never change anything on the NAS.
use backer_agent::{handle_ui, Agent};
use jiff::{tz::TimeZone, Span, Timestamp};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct T {
    _t: tempfile::TempDir,
    src: PathBuf,
    dst: PathBuf,
    agent: Arc<Agent>,
    id: i64,
}

fn ui(agent: &Arc<Agent>, m: &str, p: Value) -> Value {
    handle_ui(agent, &json!({"method": m, "params": p}))
}

fn setup() -> T {
    let t = tempfile::tempdir().unwrap();
    let (src, dst) = (t.path().join("src"), t.path().join("nas"));
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    fs::write(src.join("a.txt"), b"alpha").unwrap();
    fs::write(src.join("b.txt"), b"bravo").unwrap();
    fs::write(src.join("c.txt"), b"charlie").unwrap();
    let agent = Agent::new_sync(t.path().join("db.sqlite"), TimeZone::get("America/Los_Angeles").unwrap()).unwrap();
    assert_eq!(ui(&agent, "init_destination", json!({"path": dst}))["ok"], true);
    let job = json!({"id": 0, "name": "docs", "dest_root": dst, "schedule": {"kind": "manual"}, "paused": false, "catch_up": true,
        "exclude_names": [], "max_file_bytes": null, "folders": [{"source": src, "dest": "mac/src"}]});
    let id = ui(&agent, "save_job", json!({"job": job}))["result"]["job_id"].as_i64().unwrap();
    assert_eq!(ui(&agent, "run_job", json!({"job_id": id}))["ok"], true);
    T { _t: t, src, dst, agent, id }
}

fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v = vec![];
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            v.extend(tree(&p));
        } else {
            v.push((p.display().to_string(), fs::read(&p).unwrap()));
        }
    }
    v.sort();
    v
}

fn last(t: &T) -> Value {
    ui(&t.agent, "list_verifications", json!({"job_id": t.id}))["result"][0].clone()
}

#[test]
fn manual_check_of_a_healthy_backup_is_recorded_and_shown_on_the_job() {
    let t = setup();
    assert_eq!(ui(&t.agent, "start_verification", json!({"job_id": t.id}))["result"]["started"], true);
    let v = last(&t);
    assert_eq!((v["status"].as_str(), v["trigger"].as_str()), (Some("ok"), Some("manual")));
    assert_eq!((v["report"]["total"].as_u64(), v["report"]["ok"].as_u64(), v["report"]["bytes"].as_u64()), (Some(3), Some(3), Some(17)));
    assert!(v["finished"].as_i64().is_some());
    // The job screen gets the last result and the schedule in the same call it already makes.
    let j = ui(&t.agent, "get_job", json!({"job_id": t.id}))["result"].clone();
    assert_eq!(j["integrity"]["last"]["status"], "ok");
    assert_eq!(j["integrity"]["schedule"]["kind"], "manual");
    assert_eq!(ui(&t.agent, "verification_status", json!({}))["result"]["running"], false);
}

#[test]
fn damage_is_reported_and_the_nas_is_left_exactly_as_found() {
    let t = setup();
    let nas = t.dst.join("mac/src");
    fs::write(nas.join("a.txt"), b"alphX").unwrap(); // bit rot: same size, other contents
    fs::write(nas.join("b.txt"), b"br").unwrap(); // cut short
    fs::remove_file(nas.join("c.txt")).unwrap();
    let before = tree(&t.dst);
    ui(&t.agent, "start_verification", json!({"job_id": t.id}));
    let v = last(&t);
    assert_eq!(v["status"], "problems");
    let r = &v["report"];
    assert_eq!((r["ok"].as_u64(), r["damaged_total"].as_u64(), r["missing_total"].as_u64()), (Some(0), Some(2), Some(1)));
    assert_eq!(r["missing"][0], "mac/src/c.txt");
    // Nothing was repaired, replaced, renamed or deleted by the check.
    assert_eq!(tree(&t.dst), before);
    assert!(!nas.join("c.txt").exists());
    // The next backup is what replaces a damaged copy of a changed file; the check itself never does.
    assert_eq!(ui(&t.agent, "get_job", json!({"job_id": t.id}))["result"]["integrity"]["last"]["status"], "problems");
}

#[test]
fn a_nas_that_is_not_there_makes_the_check_fail_not_pass() {
    let t = setup();
    fs::remove_file(t.dst.join(".backer-destination")).unwrap();
    ui(&t.agent, "start_verification", json!({"job_id": t.id}));
    let v = last(&t);
    assert_eq!(v["status"], "failed");
    assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()));
}

#[test]
fn scheduled_check_runs_in_its_slot_once_and_only_when_it_is_on() {
    let t = setup();
    let bad = ui(&t.agent, "set_verify_schedule", json!({"job_id": t.id, "schedule": {"kind": "every_minutes", "minutes": 10}}));
    assert!(bad["error"].as_str().unwrap().contains("hours, days"), "{bad}");
    // Off by default: ticking does nothing.
    t.agent.tick(Timestamp::now()).unwrap();
    assert!(ui(&t.agent, "list_verifications", json!({"job_id": t.id}))["result"].as_array().unwrap().is_empty());

    let on = ui(&t.agent, "set_verify_schedule", json!({"job_id": t.id, "schedule": {"kind": "weekly", "days": [1,2,3,4,5,6,7], "hour": 3, "minute": 30}}));
    let slot = Timestamp::from_second(on["result"]["next"].as_i64().unwrap()).unwrap();
    // Before the slot nothing is due.
    t.agent.tick(slot.checked_sub(Span::new().hours(2)).unwrap()).unwrap();
    assert!(ui(&t.agent, "list_verifications", json!({"job_id": t.id}))["result"].as_array().unwrap().is_empty());
    // In the slot the check starts by itself and is marked as scheduled.
    t.agent.tick(slot.checked_add(Span::new().seconds(10)).unwrap()).unwrap();
    let v = last(&t);
    assert_eq!((v["status"].as_str(), v["trigger"].as_str()), (Some("ok"), Some("schedule")));
    // Turning it off stops further checks.
    ui(&t.agent, "set_verify_schedule", json!({"job_id": t.id, "schedule": {"kind": "manual"}}));
    let n = ui(&t.agent, "list_verifications", json!({"job_id": t.id}))["result"].as_array().unwrap().len();
    t.agent.tick(slot.checked_add(Span::new().hours(24 * 8)).unwrap()).unwrap();
    assert_eq!(ui(&t.agent, "list_verifications", json!({"job_id": t.id}))["result"].as_array().unwrap().len(), n);
}

#[test]
fn deleting_a_job_forgets_its_checks_but_not_the_nas() {
    let t = setup();
    ui(&t.agent, "start_verification", json!({"job_id": t.id}));
    ui(&t.agent, "delete_job", json!({"job_id": t.id}));
    assert!(t.dst.join("mac/src/a.txt").exists());
    assert!(t.src.join("a.txt").exists());
    assert!(t.agent.store().unwrap().list_verifications(t.id, 5).unwrap().is_empty());
}
