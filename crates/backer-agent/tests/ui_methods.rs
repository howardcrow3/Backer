//! Every agent call the app's UI makes must be one the agent answers. Catches a renamed or lost method before it
//! ships as "unknown method …" on a page.
use backer_agent::notify::RecordingNotifier;
use backer_agent::retry::RetryPolicy;
use backer_agent::{server::handle_ui_ext, Agent};
use jiff::tz::TimeZone;
use serde_json::json;
use std::sync::Arc;

fn ui_calls() -> Vec<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/ui");
    let mut found = vec![];
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|x| x.to_str()) != Some("js") {
            continue;
        }
        let src = std::fs::read_to_string(&p).unwrap();
        // `call('name', …)`, and `call(cond ? 'a' : 'b', …)` for toggles: every quoted snake_case name before the params.
        for (i, _) in src.match_indices("call(") {
            let head = src[i + 5..].split(['{', ')']).next().unwrap_or("");
            for q in head.split('\'').skip(1).step_by(2) {
                if q.contains('_') && q.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                    found.push(q.to_string());
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[test]
fn every_method_the_ui_calls_exists() {
    let calls = ui_calls();
    assert!(calls.len() > 30, "the scan found too few calls, so it is not reading the UI: {calls:?}");
    for m in ["new_job_request", "list_jobs", "pause_job", "resume_job"] {
        assert!(calls.iter().any(|c| c == m), "scan missed {m}: {calls:?}");
    }
    let t = tempfile::tempdir().unwrap();
    let agent = Agent::new_sync_with(t.path().join("db.sqlite"), TimeZone::UTC, RetryPolicy { retries: 0, delay: std::time::Duration::ZERO }, Arc::new(RecordingNotifier::default())).unwrap();
    for m in calls.iter().filter(|m| *m != "quit_agent") {
        let r = handle_ui_ext(&agent, &json!({"method": m, "params": {}}));
        let err = r["error"].as_str().unwrap_or("");
        assert!(!err.starts_with("unknown method"), "the UI calls {m}, which the agent does not know: {r}");
    }
}

#[test]
fn agent_matches_only_its_own_program_file() {
    let t = tempfile::tempdir().unwrap();
    let agent = Agent::new_sync_with(t.path().join("db.sqlite"), TimeZone::UTC, RetryPolicy { retries: 0, delay: std::time::Duration::ZERO }, Arc::new(RecordingNotifier::default())).unwrap();
    let me = std::env::current_exe().unwrap();
    let r = handle_ui_ext(&agent, &json!({"method": "agent_matches", "params": {"exe": me.display().to_string()}}));
    assert_eq!(r["result"]["current"], true, "{r}");
    let other = t.path().join("backer-agent");
    std::fs::write(&other, b"x").unwrap();
    let r = handle_ui_ext(&agent, &json!({"method": "agent_matches", "params": {"exe": other.display().to_string()}}));
    assert_eq!(r["result"]["current"], false, "{r}");
}
