//! End to end: a real agent process, driven over the app's local connection like the app does.
//! Back up, lose files on this computer, browse the backup, then restore with and without replacing.
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_backer-agent");

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ui(dir: &std::path::Path, method: &str, params: Value) -> Value {
    let mut c = backer_agent::ipc::connect(dir, "ui").unwrap();
    writeln!(c, "{}", json!({"method": method, "params": params})).unwrap();
    let mut l = String::new();
    BufReader::new(c).read_line(&mut l).unwrap();
    serde_json::from_str(&l).unwrap()
}

fn ok(v: Value) -> Value {
    assert_eq!(v["ok"], true, "{v}");
    v["result"].clone()
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(30), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn back_up_lose_files_browse_and_restore() {
    let d = tempfile::tempdir().unwrap();
    let data = d.path().join("data");
    let (src, nas) = (d.path().join("Documents"), d.path().join("nas"));
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(src.join("photos/2026")).unwrap();
    fs::create_dir_all(&nas).unwrap();
    fs::write(src.join("notes.txt"), b"first draft").unwrap();
    fs::write(src.join("photos/2026/a.jpg"), vec![7u8; 300_000]).unwrap();
    fs::write(src.join("photos/2026/b.jpg"), vec![9u8; 5]).unwrap();

    let _agent = Kill(Command::new(EXE).arg("run").env("BACKER_DATA", &data).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    wait_until("the agent", || backer_agent::ipc::connect(&data, "ui").is_ok());
    let dir = data.as_path();

    ok(ui(dir, "init_destination", json!({"path": nas})));
    let job = json!({"id": 0, "name": "Docs", "folders": [{"source": src, "dest": "mac/Documents"}], "dest_root": nas,
        "schedule": {"kind": "manual"}, "paused": false, "catch_up": true, "exclude_names": [], "max_file_bytes": null});
    let id = ok(ui(dir, "save_job", json!({"job": job})))["job_id"].as_i64().unwrap();
    ok(ui(dir, "run_job", json!({"job_id": id})));
    wait_until("the backup", || ok(ui(dir, "list_runs", json!({"job_id": id})))[0]["status"] == "ok");

    // Lose a folder, and change a file after the backup.
    fs::remove_dir_all(src.join("photos")).unwrap();
    fs::write(src.join("notes.txt"), b"second draft, newer").unwrap();

    // Browse: the job's folders, then inside.
    let top = ok(ui(dir, "browse_backup", json!({"job_id": id, "path": ""})));
    assert_eq!(top["entries"][0]["path"], "mac/Documents");
    let inner = ok(ui(dir, "browse_backup", json!({"job_id": id, "path": "mac/Documents"})));
    let names: Vec<_> = inner["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec!["photos", "notes.txt"]);
    assert_eq!(ui(dir, "browse_backup", json!({"job_id": id, "path": "mac"}))["ok"], false, "outside the job");

    // Plan restoring everything to the original place: one existing file is listed for the user to decide.
    let plan = ok(ui(dir, "plan_restore", json!({"job_id": id, "paths": ["mac/Documents"]})));
    assert_eq!((plan["files"].as_u64(), plan["conflicts_total"].as_u64()), (Some(3), Some(1)));
    assert!(plan["conflicts"][0]["local"].as_str().unwrap().ends_with("notes.txt"));

    // Keep existing files: the lost folder comes back, the newer notes stay.
    ok(ui(dir, "start_restore", json!({"plan_id": plan["plan_id"], "replace": false})));
    wait_until("the restore", || ok(ui(dir, "restore_status", json!({})))["report"]["status"] == "done");
    let rep = ok(ui(dir, "restore_status", json!({})))["report"].clone();
    assert_eq!((rep["restored"].as_u64(), rep["skipped_existing"].as_u64(), rep["failed_total"].as_u64()), (Some(2), Some(1), Some(0)));
    assert_eq!(fs::read(src.join("photos/2026/a.jpg")).unwrap(), vec![7u8; 300_000]);
    assert_eq!(fs::read(src.join("notes.txt")).unwrap(), b"second draft, newer");
    // A used plan can't be started twice.
    assert_eq!(ui(dir, "start_restore", json!({"plan_id": plan["plan_id"], "replace": true}))["ok"], false);

    // Now the user confirms replacing the one file.
    let plan = ok(ui(dir, "plan_restore", json!({"job_id": id, "paths": ["mac/Documents/notes.txt"]})));
    assert_eq!(plan["conflicts_total"], 1);
    ok(ui(dir, "start_restore", json!({"plan_id": plan["plan_id"], "replace": true})));
    wait_until("the restore", || ok(ui(dir, "restore_status", json!({})))["report"]["status"] == "done");
    assert_eq!(ok(ui(dir, "restore_status", json!({})))["report"]["replaced"], 1);
    assert_eq!(fs::read(src.join("notes.txt")).unwrap(), b"first draft");

    // Restore into another folder.
    let elsewhere = d.path().join("Restored");
    let plan = ok(ui(dir, "plan_restore", json!({"job_id": id, "paths": ["mac/Documents/photos"], "target": elsewhere})));
    assert_eq!(plan["conflicts_total"], 0);
    ok(ui(dir, "start_restore", json!({"plan_id": plan["plan_id"], "replace": false})));
    wait_until("the restore", || ok(ui(dir, "restore_status", json!({})))["report"]["status"] == "done");
    assert!(elsewhere.join("photos/2026/b.jpg").exists());
    ok(ui(dir, "dismiss_restore", json!({})));
    assert!(ok(ui(dir, "restore_status", json!({})))["report"].is_null());

    // The NAS still has everything; restore never changes it.
    assert!(nas.join("mac/Documents/photos/2026/a.jpg").exists() && nas.join("mac/Documents/notes.txt").exists());
}
