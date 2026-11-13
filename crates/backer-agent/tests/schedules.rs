//! Integration: every-N-minutes and monthly schedules through the real agent, store, engine and MCP layer.
use backer_agent::{handle_ui, Agent, AgentControl};
use backer_core::Schedule;
use jiff::{tz::TimeZone, Span};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

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
    let agent = Agent::new_sync(t.path().join("db.sqlite"), TimeZone::get("America/Los_Angeles").unwrap()).unwrap();
    assert_eq!(ui(&agent, "init_destination", json!({"path": dst}))["ok"], true);
    T { _t: t, src, dst, agent }
}

fn ui(agent: &Arc<Agent>, m: &str, p: Value) -> Value {
    handle_ui(agent, &json!({"method": m, "params": p}))
}

/// Saves a job the way the app's wizard does, with the schedule as JSON.
fn save(t: &T, schedule: Value) -> Value {
    let job = json!({"id": 0, "name": "j", "dest_root": t.dst, "schedule": schedule, "paused": false, "catch_up": true,
        "exclude_names": [], "max_file_bytes": null, "folders": [{"source": t.src, "dest": "mac/src"}]});
    ui(&t.agent, "save_job", json!({"job": job}))
}

fn mins(n: i64) -> Span {
    Span::new().minutes(n)
}

#[test]
fn every_15_minutes_runs_on_the_interval_and_copies_files() {
    // AT-5: a job set to every 15 minutes.
    let t = setup();
    let r = save(&t, json!({"kind": "every_minutes", "minutes": 15}));
    let id = r["result"]["job_id"].as_i64().unwrap_or_else(|| panic!("{r}"));
    let now = backer_agent::now_ts();

    // First tick: never ran, so it runs at once and the file reaches the NAS.
    assert_eq!(t.agent.tick(now).unwrap(), vec![id]);
    assert!(t.dst.join("mac/src/a.txt").exists());

    // Not again before 15 minutes have passed.
    assert!(t.agent.tick(now.checked_add(mins(5)).unwrap()).unwrap().is_empty());
    assert!(t.agent.tick(now.checked_add(mins(14)).unwrap()).unwrap().is_empty());

    // The app shows the next run about 15 minutes after the last start.
    let job = ui(&t.agent, "get_job", json!({"job_id": id}));
    let next = job["result"]["next_run"].as_i64().unwrap_or_else(|| panic!("{job}"));
    assert!((next - now.as_second() - 15 * 60).abs() <= 5, "next_run {next} vs now {}", now.as_second());
    assert_eq!(job["result"]["job"]["schedule"], json!({"kind": "every_minutes", "minutes": 15}));

    // After 15 minutes it runs again and picks up a new file.
    fs::write(t.src.join("b.txt"), b"2").unwrap();
    assert_eq!(t.agent.tick(now.checked_add(mins(16)).unwrap()).unwrap(), vec![id]);
    assert!(t.dst.join("mac/src/b.txt").exists());
    let runs = t.agent.store().unwrap().list_runs(id, 5).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.stats.trigger == "schedule"));
}

#[test]
fn monthly_runs_on_its_day_and_shows_the_next_run() {
    let t = setup();
    // A day two days from now, so the slot falls inside the free trial whatever day the test runs.
    let tz = TimeZone::get("America/Los_Angeles").unwrap();
    let day = backer_agent::now_ts().checked_add(Span::new().hours(48)).unwrap().to_zoned(tz.clone()).day() as u8;
    let sched = Schedule::Monthly { day, hour: 2, minute: 0 };
    let r = save(&t, serde_json::to_value(&sched).unwrap());
    let id = r["result"]["job_id"].as_i64().unwrap_or_else(|| panic!("{r}"));
    let slot = backer_sched::next_run(&sched, None, backer_agent::now_ts(), &tz).unwrap();
    assert!(slot.duration_since(backer_agent::now_ts()).as_secs() < 4 * 24 * 3600, "slot {slot} should be days away, not a month");

    assert!(t.agent.tick(slot.checked_sub(mins(1)).unwrap()).unwrap().is_empty());
    assert_eq!(t.agent.tick(slot.checked_add(Span::new().seconds(10)).unwrap()).unwrap(), vec![id]);
    assert!(t.dst.join("mac/src/a.txt").exists());

    // The app always gets a next run for a monthly job.
    let job = ui(&t.agent, "get_job", json!({"job_id": id}));
    assert!(job["result"]["next_run"].as_i64().is_some(), "{job}");
}

#[test]
fn out_of_range_schedules_are_rejected_by_the_app_and_assistants() {
    let t = setup();
    assert_eq!(save(&t, json!({"kind": "every_minutes", "minutes": 2}))["ok"], false);
    assert_eq!(save(&t, json!({"kind": "monthly", "day": 32, "hour": 2, "minute": 0}))["ok"], false);
    let id = save(&t, json!({"kind": "manual"}))["result"]["job_id"].as_i64().unwrap();

    // An assistant changes the schedule through the MCP tool.
    let ctl = AgentControl(t.agent.clone());
    let call = |args: Value| backer_mcp::handle(&ctl, &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"set_schedule","arguments":args}})).unwrap();
    assert!(call(json!({"job_id": id, "kind": "every_minutes", "minutes": 15}))["result"].get("isError").is_none());
    assert_eq!(t.agent.store().unwrap().get_job(id).unwrap().unwrap().schedule, Schedule::EveryMinutes { minutes: 15 });
    assert!(call(json!({"job_id": id, "kind": "monthly", "day": 1, "hour": 3}))["result"].get("isError").is_none());
    assert_eq!(t.agent.store().unwrap().get_job(id).unwrap().unwrap().schedule, Schedule::Monthly { day: 1, hour: 3, minute: 0 });
    assert_eq!(call(json!({"job_id": id, "kind": "every_minutes", "minutes": 1}))["result"]["isError"], true);
    // A rejected change leaves the last good schedule in place.
    assert_eq!(t.agent.store().unwrap().get_job(id).unwrap().unwrap().schedule, Schedule::Monthly { day: 1, hour: 3, minute: 0 });
}
