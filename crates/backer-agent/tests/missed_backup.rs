//! Integration test: a job with no successful backup for N days raises one alert a day until it succeeds.
//! Uses the real agent and store; run history is written directly so no real waiting is needed.
use backer_agent::notify::RecordingNotifier;
use backer_agent::retry::RetryPolicy;
use backer_agent::Agent;
use backer_core::{FolderPair, Job, RunStatus, RunStats, Schedule};
use jiff::tz::TimeZone;
use std::sync::Arc;

const DAY: i64 = 86_400;
const T0: i64 = 1_800_000_000;

fn fixture(alert: Option<u32>) -> (tempfile::TempDir, Arc<Agent>, Arc<RecordingNotifier>, i64) {
    let t = tempfile::tempdir().unwrap();
    let notes = Arc::new(RecordingNotifier::default());
    let agent = Agent::new_sync_with(t.path().join("db.sqlite"), TimeZone::UTC, RetryPolicy { retries: 0, delay: std::time::Duration::ZERO }, notes.clone()).unwrap();
    let mut job = Job {
        id: 0, name: "Documents".into(),
        folders: vec![FolderPair { source: t.path().to_string_lossy().into(), dest: "mac/src".into() }],
        dest_root: t.path().join("nas").to_string_lossy().into(), schedule: Schedule::Daily { hour: 2, minute: 0 },
        paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None, alert_after_days: alert,
    };
    let id = agent.store().unwrap().save_job(&mut job).unwrap();
    (t, agent, notes, id)
}

fn run(agent: &Agent, id: i64, at: i64, status: RunStatus) {
    let s = agent.store().unwrap();
    let r = s.begin_run(id, at).unwrap();
    s.finish_run(r, at + 60, status, &RunStats::default(), None).unwrap();
}

#[test]
fn alerts_after_n_days_without_success_then_once_a_day_and_clears_on_success() {
    let (_t, agent, notes, id) = fixture(Some(3));
    run(&agent, id, T0, RunStatus::Ok);
    run(&agent, id, T0 + DAY, RunStatus::Failed);
    agent.check_missed_backups(T0 + 2 * DAY).unwrap();
    assert!(notes.0.lock().unwrap().is_empty(), "under the threshold");

    agent.check_missed_backups(T0 + 4 * DAY).unwrap();
    {
        let n = notes.0.lock().unwrap();
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(n[0].0.contains("Documents") && n[0].1.contains("No successful backup"), "{n:?}");
    }
    agent.check_missed_backups(T0 + 4 * DAY + 30).unwrap();
    assert_eq!(notes.0.lock().unwrap().len(), 1, "not repeated on every tick");
    agent.check_missed_backups(T0 + 5 * DAY + 100).unwrap();
    assert_eq!(notes.0.lock().unwrap().len(), 2, "repeats a day later");

    run(&agent, id, T0 + 5 * DAY + 200, RunStatus::Warnings);
    agent.check_missed_backups(T0 + 6 * DAY).unwrap();
    assert_eq!(notes.0.lock().unwrap().len(), 2, "a run with warnings counts as success");
    assert!(agent.store().unwrap().missed_alert_at(id).unwrap().is_none());
    agent.check_missed_backups(T0 + 9 * DAY).unwrap();
    assert_eq!(notes.0.lock().unwrap().len(), 3, "alerts again after a new lapse");
}

#[test]
fn never_succeeded_counts_from_the_first_attempt() {
    let (_t, agent, notes, id) = fixture(Some(2));
    run(&agent, id, T0, RunStatus::Failed);
    agent.check_missed_backups(T0 + 3 * DAY).unwrap();
    let n = notes.0.lock().unwrap();
    assert_eq!(n.len(), 1);
    assert!(n[0].1.contains("never completed"), "{n:?}");
}

#[test]
fn no_alert_when_unconfigured_paused_or_never_run() {
    let (_t, agent, notes, id) = fixture(None);
    run(&agent, id, T0, RunStatus::Failed);
    agent.check_missed_backups(T0 + 30 * DAY).unwrap();
    let (_t2, agent2, notes2, id2) = fixture(Some(1));
    agent2.check_missed_backups(T0 + 30 * DAY).unwrap(); // no history at all
    let s = agent2.store().unwrap();
    let mut j = s.get_job(id2).unwrap().unwrap();
    j.paused = true;
    s.save_job(&mut j).unwrap();
    run(&agent2, id2, T0, RunStatus::Failed);
    agent2.check_missed_backups(T0 + 30 * DAY).unwrap();
    assert!(notes.0.lock().unwrap().is_empty() && notes2.0.lock().unwrap().is_empty());
}

#[test]
fn resuming_a_long_paused_job_restarts_the_clock() {
    let (_t, agent, notes, id) = fixture(Some(3));
    run(&agent, id, T0, RunStatus::Ok);
    let s = agent.store().unwrap();
    let mut j = s.get_job(id).unwrap().unwrap();
    j.paused = true;
    s.save_job(&mut j).unwrap();
    agent.check_missed_backups(T0 + DAY).unwrap();
    agent.check_missed_backups(T0 + 20 * DAY).unwrap(); // still paused: no alert
    j.paused = false;
    s.save_job(&mut j).unwrap();
    agent.check_missed_backups(T0 + 20 * DAY + 60).unwrap();
    assert!(notes.0.lock().unwrap().is_empty(), "no alert right after resuming");
    agent.check_missed_backups(T0 + 22 * DAY).unwrap();
    assert!(notes.0.lock().unwrap().is_empty(), "still within 3 days of the resume");
    agent.check_missed_backups(T0 + 23 * DAY + 120).unwrap();
    assert_eq!(notes.0.lock().unwrap().len(), 1, "alerts once 3 days pass after the resume");
}

#[test]
fn old_saved_jobs_without_the_field_still_load() {
    let j: Job = serde_json::from_str(r#"{"id":1,"name":"j","folders":[],"dest_root":"x","schedule":{"kind":"manual"},"paused":false,"catch_up":true,"max_file_bytes":null}"#).unwrap();
    assert_eq!(j.alert_after_days, None);
}
