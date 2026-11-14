//! End to end through the agent, the way the app drives it: set up a NAS connection in the wizard
//! calls, save the job, back up, change and delete files, back up again, review and remove.
//!
//! NFS always runs against an in-process NFSv3 server. SFTP and SMB run when CI (or you) start the
//! containers in scripts/nas-test-servers.sh; see crates/backer-connector/tests/protocols.rs.
use backer_agent::{handle_ui, Agent};
use backer_core::*;
use jiff::tz::TimeZone;
use serde_json::{json, Value};
use std::fs;
use std::sync::Arc;

fn ui(a: &Arc<Agent>, m: &str, p: Value) -> Value {
    let r = handle_ui(a, &json!({"method": m, "params": p}));
    assert_eq!(r["ok"], true, "{m} failed: {r}");
    r["result"].clone()
}

fn backup_flow(url: &str, password: Option<&str>) {
    let t = tempfile::tempdir().unwrap();
    let a = Agent::new_sync(t.path().join("db.sqlite"), TimeZone::UTC).unwrap();
    let src = t.path().join("Documents");
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("a.txt"), b"alpha").unwrap();
    fs::write(src.join("sub/b.txt"), b"bravo").unwrap();

    // Wizard: confirm the host key (SFTP), save the password, test, create the destination folder.
    let probe = ui(&a, "nas_probe", json!({"url": url}));
    if let Some(fp) = probe["host_key"].as_str() {
        assert_eq!(probe["trusted"], false);
        ui(&a, "nas_trust_host_key", json!({"url": url, "fingerprint": fp}));
        assert_eq!(ui(&a, "nas_probe", json!({"url": url}))["trusted"], true);
    }
    if let Some(pw) = password {
        ui(&a, "nas_save_password", json!({"url": url, "password": pw}));
    }
    assert_eq!(ui(&a, "nas_test", json!({"url": url}))["ok"], true);
    let folder = format!("backer-e2e-{}-{}", std::process::id(), t.path().file_name().unwrap().to_string_lossy());
    let created = ui(&a, "nas_create_folder", json!({"url": url, "name": folder}));
    let dest = created["url"].as_str().unwrap().to_string();
    let base = backer_connector::Destination::parse(url).unwrap();
    let listed = ui(&a, "nas_browse", json!({"url": url, "path": base.base_path().unwrap()}));
    assert!(listed["folders"].as_array().unwrap().iter().any(|f| f == &json!(folder)), "{listed}");

    assert_eq!(ui(&a, "check_destination", json!({"path": dest}))["ready"], false);
    ui(&a, "init_destination", json!({"path": dest}));
    assert_eq!(ui(&a, "check_destination", json!({"path": dest}))["ready"], true);

    let job = Job { id: 0, name: "NAS job".into(), folders: vec![FolderPair { source: src.to_string_lossy().into(), dest: "mac/Documents".into() }],
        dest_root: dest.clone(), schedule: Schedule::Manual, paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None };
    let id = ui(&a, "save_job", json!({"job": job}))["job_id"].as_i64().unwrap();
    assert!(!ui(&a, "get_job", json!({"job_id": id})).to_string().contains(password.unwrap_or("\u{0}")), "no secrets in job JSON");
    assert_eq!(ui(&a, "test_connection", json!({"job_id": id}))["ok"], true);

    // First run copies everything.
    ui(&a, "run_job", json!({"job_id": id}));
    let run = ui(&a, "list_runs", json!({"job_id": id}))[0].clone();
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["stats"]["new_files"], 2);

    // Second run: one change, one deletion.
    fs::write(src.join("a.txt"), b"alpha, edited").unwrap();
    fs::remove_file(src.join("sub/b.txt")).unwrap();
    ui(&a, "run_job", json!({"job_id": id}));
    let run = ui(&a, "list_runs", json!({"job_id": id}))[0].clone();
    assert_eq!(run["stats"]["updated"], 1, "{run}");
    assert_eq!(run["stats"]["newly_deleted"], 1, "{run}");

    // Verify reads every file back from the NAS.
    let v = ui(&a, "verify_job", json!({"job_id": id}));
    assert_eq!(v["missing"].as_array().map(|x| x.len()).unwrap_or(0), 0, "{v}");

    // The deleted file stays on the NAS until the user removes it.
    let del = ui(&a, "list_deleted", json!({"job_id": id}));
    assert_eq!(del.as_array().unwrap().len(), 1);
    let r = ui(&a, "remove_deleted", json!({"job_id": id, "paths": ["mac/Documents/sub/b.txt"]}));
    assert_eq!(r[0]["outcome"], "Removed", "{r}");
    let c = a.open_dest(&dest);
    assert!(c.stat("mac/Documents/sub/b.txt").unwrap().is_none());
    let mut back = vec![];
    c.read_file("mac/Documents/a.txt", &mut back).unwrap();
    assert_eq!(back, b"alpha, edited");

    // Restore from the NAS into another folder.
    let top = ui(&a, "browse_backup", json!({"job_id": id, "path": "mac/Documents"}));
    let files: Vec<_> = top["entries"].as_array().unwrap().iter().filter(|e| e["is_dir"] == false).map(|e| e["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(files, vec!["a.txt"], "{top}");
    let into = t.path().join("Restored");
    let plan = ui(&a, "plan_restore", json!({"job_id": id, "paths": ["mac/Documents"], "target": into.to_string_lossy()}));
    ui(&a, "start_restore", json!({"plan_id": plan["plan_id"], "replace": false}));
    let rep = ui(&a, "restore_status", json!({}))["report"].clone();
    assert_eq!(rep["status"], "done", "{rep}");
    assert_eq!(rep["restored"], 1, "{rep}");
    let restored: Vec<_> = walk(&into).into_iter().filter(|(_, b)| b == b"alpha, edited").collect();
    assert_eq!(restored.len(), 1, "restored file in {}", into.display());
}

fn walk(d: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = vec![];
    for e in fs::read_dir(d).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push((p.clone(), fs::read(&p).unwrap()));
        }
    }
    out
}

#[test]
fn nfs_backup_end_to_end() {
    use nfs3_server::memfs::{MemFs, MemFsConfig};
    use nfs3_server::tcp::{NFSTcp, NFSTcpListener};
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let mut cfg = MemFsConfig::default();
    cfg.add_dir("/volume1");
    let l = rt.block_on(NFSTcpListener::bind("127.0.0.1:0", MemFs::new(cfg).unwrap())).unwrap();
    let port = l.get_listen_port();
    rt.spawn(async move { l.handle_forever().await });
    backup_flow(&format!("nfs://127.0.0.1:{port}/volume1?export=/&uid=1000&gid=1000"), None);
}

fn env(v: &str) -> Option<String> {
    std::env::var(v).ok().filter(|x| !x.is_empty()).or_else(|| {
        // CI lists the servers it started; a missing one there is a failure, not a skip.
        let proto = v.trim_start_matches("BACKER_TEST_").to_lowercase();
        let required = std::env::var("BACKER_REQUIRE_SERVERS").unwrap_or_default();
        assert!(!required.split(',').any(|r| r.trim() == proto), "{v} is required (BACKER_REQUIRE_SERVERS) but not set");
        eprintln!("skipping: {v} is not set");
        None
    })
}

#[test]
fn sftp_backup_end_to_end() {
    let Some(url) = env("BACKER_TEST_SFTP") else { return };
    backup_flow(&url, Some(&std::env::var("BACKER_TEST_SFTP_PASSWORD").unwrap_or_default()));
}

#[test]
fn smb_backup_end_to_end() {
    let Some(url) = env("BACKER_TEST_SMB") else { return };
    backup_flow(&url, Some(&std::env::var("BACKER_TEST_SMB_PASSWORD").unwrap_or_default()));
}
