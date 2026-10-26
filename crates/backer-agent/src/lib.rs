//! The background agent: owns schedules, runs jobs, serves the app (ui.sock) and assistants (mcp.sock).
pub mod approvals;
pub mod bridge;
pub mod ipc;
pub mod server;
pub mod platform;

use jiff::{tz::TimeZone, Timestamp};
use backer_connector::{Connector, MountedConnector};
use backer_core::*;
use backer_engine::{run_job, NoProgress};
use backer_mcp::Control;
use backer_store::Store;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const MAX_CONCURRENT: usize = 2;

#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    Started,
    Queued,
    AlreadyRunning,
}

#[derive(Default)]
struct Running {
    active: HashMap<JobId, Arc<AtomicBool>>,
    queue: VecDeque<JobId>,
}

pub struct Agent {
    db_path: PathBuf,
    tz: TimeZone,
    state: Mutex<Running>,
    pub approvals: approvals::Approvals,
    /// Test hook: run synchronously instead of on a thread.
    sync_runs: bool,
    /// Set when the tray asks the agent to stop; the main loop then exits.
    quit: AtomicBool,
}

pub fn now_ts() -> Timestamp {
    Timestamp::now()
}

impl Agent {
    pub fn new(db_path: PathBuf, tz: TimeZone) -> Result<Arc<Self>> {
        let approvals = approvals::Approvals::load(db_path.with_file_name("mcp-clients.json"));
        let a = Arc::new(Self { db_path, tz, state: Mutex::new(Running::default()), approvals, sync_runs: false, quit: AtomicBool::new(false) });
        a.store()?.recover_interrupted(now_ts().as_second())?;
        Ok(a)
    }
    #[doc(hidden)]
    pub fn new_sync(db_path: PathBuf, tz: TimeZone) -> Result<Arc<Self>> {
        let approvals = approvals::Approvals::load(db_path.with_file_name("mcp-clients.json"));
        let a = Arc::new(Self { db_path, tz, state: Mutex::new(Running::default()), approvals, sync_runs: true, quit: AtomicBool::new(false) });
        a.store()?;
        Ok(a)
    }

    /// Each caller gets its own connection (WAL mode); SQLite serialises writers.
    pub fn store(&self) -> Result<Store> {
        Store::open(&self.db_path)
    }

    pub fn connector_for(job: &Job) -> MountedConnector {
        MountedConnector::new(&job.dest_root)
    }

    pub fn start_job(self: &Arc<Self>, id: JobId) -> Result<Start> {
        let job = self.store()?.get_job(id)?.ok_or_else(|| Error::Invalid(format!("no job {id}")))?;
        let cancel = {
            let mut st = self.state.lock().unwrap();
            if st.active.contains_key(&id) || st.queue.contains(&id) {
                return Ok(Start::AlreadyRunning);
            }
            if st.active.len() >= MAX_CONCURRENT {
                st.queue.push_back(id);
                return Ok(Start::Queued);
            }
            let c = Arc::new(AtomicBool::new(false));
            st.active.insert(id, c.clone());
            c
        };
        self.spawn_run(job, cancel);
        Ok(Start::Started)
    }

    fn spawn_run(self: &Arc<Self>, job: Job, cancel: Arc<AtomicBool>) {
        let me = self.clone();
        let work = move || {
            let _awake = platform::AwakeGuard::new();
            if let Ok(store) = me.store() {
                let conn = Agent::connector_for(&job);
                let _ = run_job(&store, &conn, &job, now_ts().as_second(), &cancel, &NoProgress);
            }
            let next = {
                let mut st = me.state.lock().unwrap();
                st.active.remove(&job.id);
                st.queue.pop_front()
            };
            if let Some(n) = next {
                let _ = me.start_job(n);
            }
        };
        if self.sync_runs {
            work();
        } else {
            std::thread::spawn(work);
        }
    }

    /// Stop for good: cancel every running backup (files already copied stay valid) and tell the main loop to exit.
    pub fn request_quit(&self) {
        self.quit.store(true, Ordering::SeqCst);
        for c in self.state.lock().unwrap().active.values() {
            c.store(true, Ordering::SeqCst);
        }
    }

    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }

    pub fn cancel_job(&self, id: JobId) -> bool {
        let mut st = self.state.lock().unwrap();
        st.queue.retain(|j| *j != id);
        match st.active.get(&id) {
            Some(c) => {
                c.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn is_running(&self, id: JobId) -> bool {
        self.state.lock().unwrap().active.contains_key(&id)
    }

    /// Called every 30 s and after wake. Starts every due job.
    pub fn tick(self: &Arc<Self>, now: Timestamp) -> Result<Vec<JobId>> {
        let store = self.store()?;
        let mut started = vec![];
        for job in store.list_jobs()? {
            let last = store.last_run_started(job.id)?.and_then(|s| Timestamp::from_second(s).ok());
            if backer_sched::is_due(&job, last, now, &self.tz) && self.start_job(job.id)? != Start::AlreadyRunning {
                started.push(job.id);
            }
        }
        Ok(started)
    }

    fn job_json(&self, store: &Store, job: &Job) -> Result<Value> {
        let last = store.last_run_started(job.id)?;
        let last_ts = last.and_then(|s| Timestamp::from_second(s).ok());
        let next = backer_sched::next_run(&job.schedule, last_ts, now_ts(), &self.tz).map(|t| t.as_second());
        let last_run = store.list_runs(job.id, 1)?.into_iter().next();
        Ok(json!({
            "job": job, "running": self.is_running(job.id), "next_run": next, "last_run": last_run,
            "pending_deleted": store.list_deleted(Some(job.id))?.len(),
        }))
    }

    fn require_job(&self, store: &Store, id: JobId) -> std::result::Result<Job, String> {
        store.get_job(id).map_err(|e| e.to_string())?.ok_or_else(|| format!("no job {id}"))
    }
}

fn es(e: Error) -> String {
    e.to_string()
}

/// What assistants may do. Wired to the same agent as the app but through the narrow trait.
/// Newtype so the orphan rule allows implementing `Control`.
pub struct AgentControl(pub Arc<Agent>);

impl Control for AgentControl {
    fn list_jobs(&self) -> std::result::Result<Value, String> {
        let s = self.0.store().map_err(es)?;
        let v: std::result::Result<Vec<_>, _> = s.list_jobs().map_err(es)?.iter().map(|j| self.0.job_json(&s, j)).collect();
        Ok(Value::Array(v.map_err(es)?))
    }
    fn get_job(&self, id: JobId) -> std::result::Result<Value, String> {
        let s = self.0.store().map_err(es)?;
        let j = self.0.require_job(&s, id)?;
        self.0.job_json(&s, &j).map_err(es)
    }
    fn list_runs(&self, id: JobId, limit: u32) -> std::result::Result<Value, String> {
        serde_json::to_value(self.0.store().map_err(es)?.list_runs(id, limit).map_err(es)?).map_err(|e| e.to_string())
    }
    fn get_run_log(&self, run_id: i64, limit: u32) -> std::result::Result<Value, String> {
        let rows = self.0.store().map_err(es)?.run_log(run_id, limit).map_err(es)?;
        Ok(Value::Array(rows.into_iter().map(|(k, p, d)| json!({"kind": k, "path": p, "detail": d})).collect()))
    }
    fn test_connection(&self, id: JobId) -> std::result::Result<Value, String> {
        let s = self.0.store().map_err(es)?;
        let j = self.0.require_job(&s, id)?;
        Ok(match Agent::connector_for(&j).check_ready() {
            Ok(()) => json!({"ok": true}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        })
    }
    fn run_job(&self, id: JobId) -> std::result::Result<Value, String> {
        let r = self.0.start_job(id).map_err(es)?;
        Ok(json!({"result": format!("{r:?}").to_lowercase()}))
    }
    fn set_schedule(&self, id: JobId, schedule: Schedule) -> std::result::Result<Value, String> {
        let s = self.0.store().map_err(es)?;
        let mut j = self.0.require_job(&s, id)?;
        j.schedule = schedule;
        s.save_job(&mut j).map_err(es)?;
        Ok(json!({"ok": true}))
    }
    fn pause_job(&self, id: JobId) -> std::result::Result<Value, String> {
        set_paused(&self.0, id, true)
    }
    fn resume_job(&self, id: JobId) -> std::result::Result<Value, String> {
        set_paused(&self.0, id, false)
    }
}

fn set_paused(a: &Arc<Agent>, id: JobId, p: bool) -> std::result::Result<Value, String> {
    let s = a.store().map_err(es)?;
    let mut j = a.require_job(&s, id)?;
    j.paused = p;
    s.save_job(&mut j).map_err(es)?;
    Ok(json!({"ok": true}))
}

/// Full API for the signed app (ui.sock). One JSON object in, one out.
pub fn handle_ui(agent: &Arc<Agent>, req: &Value) -> Value {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let p = req.get("params").cloned().unwrap_or(Value::Null);
    let id = p.get("job_id").and_then(Value::as_i64);
    let ctl = AgentControl(agent.clone());
    let res: std::result::Result<Value, String> = (|| {
        let need = || id.ok_or_else(|| "job_id required".to_string());
        match method {
            // Everything assistants can do, plus app-only calls.
            "list_jobs" => ctl.list_jobs(),
            "get_job" => ctl.get_job(need()?),
            "list_runs" => ctl.list_runs(need()?, p.get("limit").and_then(Value::as_u64).unwrap_or(20) as u32),
            "get_run_log" => ctl.get_run_log(p.get("run_id").and_then(Value::as_i64).ok_or("run_id required")?, 500),
            "test_connection" => ctl.test_connection(need()?),
            "run_job" => ctl.run_job(need()?),
            "pause_job" => ctl.pause_job(need()?),
            "resume_job" => ctl.resume_job(need()?),
            // App only (assistants cannot do this). Forgets the job; nothing is removed from the NAS or the Mac.
            "delete_job" => {
                let id = need()?;
                if agent.is_running(id) {
                    return Err("This job is running. Stop it first, then delete it.".into());
                }
                agent.cancel_job(id); // drops it from the queue if it was waiting
                agent.store().map_err(es)?.delete_job(id).map_err(es)?;
                Ok(json!({"ok": true}))
            }
            "cancel_job" => Ok(json!({"cancelled": agent.cancel_job(need()?)})),
            "save_job" => {
                let mut job: Job = serde_json::from_value(p.get("job").cloned().unwrap_or(Value::Null)).map_err(|e| e.to_string())?;
                let id = agent.store().map_err(es)?.save_job(&mut job).map_err(es)?;
                Ok(json!({"job_id": id}))
            }
            "check_destination" => {
                let root = p.get("path").and_then(Value::as_str).ok_or("path required")?;
                Ok(json!({"ready": MountedConnector::new(root).check_ready().is_ok()}))
            }
            "init_destination" => {
                let root = p.get("path").and_then(Value::as_str).ok_or("path required")?;
                let c = MountedConnector::new(root);
                if !std::path::Path::new(root).is_dir() { return Err("not a folder".into()); }
                c.init_marker().map_err(es)?;
                Ok(json!({"ok": true}))
            }
            "list_deleted" => serde_json::to_value(agent.store().map_err(es)?.list_deleted(id).map_err(es)?).map_err(|e| e.to_string()),
            "keep_deleted" => {
                let s = agent.store().map_err(es)?;
                for path in paths(&p)? { s.keep_deleted(need()?, &path).map_err(es)?; }
                Ok(json!({"ok": true}))
            }
            "remove_deleted" => {
                let s = agent.store().map_err(es)?;
                let j = agent.require_job(&s, need()?)?;
                let r = backer_removal::remove_reviewed(&s, &Agent::connector_for(&j), j.id, &paths(&p)?).map_err(es)?;
                Ok(Value::Array(r.into_iter().map(|(path, o)| json!({"path": path, "outcome": format!("{o:?}")})).collect()))
            }
            _ => Err(format!("unknown method {method}")),
        }
    })();
    match res {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

fn paths(p: &Value) -> std::result::Result<Vec<String>, String> {
    Ok(p.get("paths").and_then(Value::as_array).ok_or("paths required")?.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct T {
        _t: tempfile::TempDir,
        src: PathBuf,
        dst: PathBuf,
        agent: Arc<Agent>,
        id: JobId,
    }
    fn setup() -> T {
        let t = tempfile::tempdir().unwrap();
        let (src, dst) = (t.path().join("src"), t.path().join("nas"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let agent = Agent::new_sync(t.path().join("db.sqlite"), TimeZone::get("America/Los_Angeles").unwrap()).unwrap();
        let ui = |m: &str, p: Value| handle_ui(&agent, &json!({"method": m, "params": p}));
        assert_eq!(ui("check_destination", json!({"path": dst}))["result"]["ready"], false);
        assert_eq!(ui("init_destination", json!({"path": dst})) ["ok"], true);
        assert_eq!(ui("check_destination", json!({"path": dst}))["result"]["ready"], true);
        let job = Job { id: 0, name: "j".into(), folders: vec![FolderPair { source: src.to_string_lossy().into(), dest: "mac/src".into() }],
            dest_root: dst.to_string_lossy().into(), schedule: Schedule::Daily { hour: 2, minute: 0 }, paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None };
        let id = ui("save_job", json!({"job": job}))["result"]["job_id"].as_i64().unwrap();
        T { _t: t, src, dst, agent, id }
    }

    #[test]
    fn run_then_delete_review_flow() {
        let t = setup();
        fs::write(t.src.join("a"), b"1").unwrap();
        fs::write(t.src.join("b"), b"2").unwrap();
        AgentControl(t.agent.clone()).run_job(t.id).unwrap();
        assert!(t.dst.join("mac/src/b").exists());
        fs::remove_file(t.src.join("b")).unwrap();
        AgentControl(t.agent.clone()).run_job(t.id).unwrap();
        let ui = |m: &str, p: Value| handle_ui(&t.agent, &json!({"method": m, "params": p}));
        let del = ui("list_deleted", json!({"job_id": t.id}));
        assert_eq!(del["result"].as_array().unwrap().len(), 1);
        assert!(t.dst.join("mac/src/b").exists());
        let r = ui("remove_deleted", json!({"job_id": t.id, "paths": ["mac/src/b", "mac/src/a"]}));
        assert_eq!(r["result"][0]["outcome"], "Removed");
        assert_eq!(r["result"][1]["outcome"], "NotListed");
        assert!(!t.dst.join("mac/src/b").exists() && t.dst.join("mac/src/a").exists());
    }

    #[test]
    fn deleting_a_job_forgets_it_but_never_touches_the_nas() {
        let t = setup();
        fs::write(t.src.join("a"), b"1").unwrap();
        t.agent.start_job(t.id).unwrap();
        let on_nas = t.dst.join("mac/src/a");
        assert!(on_nas.exists());
        let r = handle_ui(&t.agent, &json!({"method":"delete_job","params":{"job_id": t.id}}));
        assert_eq!(r["ok"], true, "{r}");
        assert!(t.agent.store().unwrap().get_job(t.id).unwrap().is_none());
        assert_eq!(handle_ui(&t.agent, &json!({"method":"list_jobs"}))["result"].as_array().unwrap().len(), 0);
        assert!(on_nas.exists(), "the backed-up file must still be on the NAS");
        assert!(t.src.join("a").exists());
        // deleting again is a clear error, not a crash
        let again = handle_ui(&t.agent, &json!({"method":"delete_job","params":{"job_id": t.id}}));
        assert_eq!(again["ok"], true);
    }

    #[test]
    fn tick_runs_due_job_once_and_respects_pause() {
        let t = setup();
        let s = t.agent.store().unwrap();
        // Never ran; slot is 2:00 AM LA. 09:00:10Z is inside the first minute of the slot.
        let at: Timestamp = "2026-10-14T09:00:10Z".parse().unwrap();
        AgentControl(t.agent.clone()).pause_job(t.id).unwrap();
        assert!(t.agent.tick(at).unwrap().is_empty());
        AgentControl(t.agent.clone()).resume_job(t.id).unwrap();
        assert_eq!(t.agent.tick(at).unwrap(), vec![t.id]);
        assert_eq!(s.list_runs(t.id, 5).unwrap().len(), 1);
    }

    #[test]
    fn assistant_can_change_schedule_but_not_destination() {
        let t = setup();
        AgentControl(t.agent.clone()).set_schedule(t.id, Schedule::Manual).unwrap();
        let j = t.agent.store().unwrap().get_job(t.id).unwrap().unwrap();
        assert_eq!(j.schedule, Schedule::Manual);
        assert_eq!(j.dest_root, t.dst.to_string_lossy());
    }
}
