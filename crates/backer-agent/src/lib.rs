//! The background agent: owns schedules, runs jobs, serves the app (ui.sock) and assistants (mcp.sock).
pub mod approvals;
pub mod bridge;
pub mod ipc;
pub mod licensing;
pub mod notify;
pub mod retry;
pub mod server;
pub mod platform;

use jiff::{tz::TimeZone, Timestamp};
use backer_connector::{Connector, MountedConnector};
use backer_core::*;
use backer_engine::{run_job_as, verify_job, NoProgress};
use backer_mcp::Control;
use backer_store::Store;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub const MAX_CONCURRENT: usize = 2;
/// Runs whose per-file log is kept for each job. Older runs keep their summary but lose the file rows.
pub const KEEP_LOG_RUNS: u32 = 30;

#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    Started,
    Queued,
    AlreadyRunning,
}

#[derive(Default)]
struct Running {
    active: HashMap<JobId, Arc<AtomicBool>>,
    queue: VecDeque<(JobId, Trigger)>,
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
    /// A job set-up an assistant asked for (suggested name). The user completes or dismisses it in the app.
    new_job_request: Mutex<Option<String>>,
    pub licensing: licensing::Licensing,
    /// FR-18: how long to wait for an unreachable NAS before a run is marked failed.
    retry: retry::RetryPolicy,
    notifier: Arc<dyn notify::Notifier>,
}

pub fn now_ts() -> Timestamp {
    Timestamp::now()
}

impl Agent {
    pub fn new(db_path: PathBuf, tz: TimeZone) -> Result<Arc<Self>> {
        let license_path = db_path.with_file_name("license.json");
        let approvals = approvals::Approvals::load(db_path.with_file_name("mcp-clients.json"));
        let a = Arc::new(Self { db_path, tz, state: Mutex::new(Running::default()), approvals, sync_runs: false, quit: AtomicBool::new(false), new_job_request: Mutex::new(None), licensing: licensing::Licensing::load(license_path, now_ts().as_second()), retry: retry::RetryPolicy::default(), notifier: Arc::new(notify::SystemNotifier) });
        a.store()?.recover_interrupted(now_ts().as_second())?;
        Ok(a)
    }
    /// Test agent: runs synchronously, retries without waiting, shows no notifications.
    #[doc(hidden)]
    pub fn new_sync(db_path: PathBuf, tz: TimeZone) -> Result<Arc<Self>> {
        Self::new_sync_with(db_path, tz, retry::RetryPolicy { retries: 3, delay: std::time::Duration::ZERO }, Arc::new(notify::NullNotifier))
    }
    #[doc(hidden)]
    pub fn new_sync_with(db_path: PathBuf, tz: TimeZone, retry: retry::RetryPolicy, notifier: Arc<dyn notify::Notifier>) -> Result<Arc<Self>> {
        let license_path = db_path.with_file_name("license.json");
        let approvals = approvals::Approvals::load(db_path.with_file_name("mcp-clients.json"));
        let a = Arc::new(Self { db_path, tz, state: Mutex::new(Running::default()), approvals, sync_runs: true, quit: AtomicBool::new(false), new_job_request: Mutex::new(None), licensing: licensing::Licensing::load(license_path, now_ts().as_second()), retry, notifier });
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
        self.start_job_as(id, Trigger::Manual)
    }

    pub fn start_job_as(self: &Arc<Self>, id: JobId, trigger: Trigger) -> Result<Start> {
        let job = self.store()?.get_job(id)?.ok_or_else(|| Error::Invalid(format!("no job {id}")))?;
        if !self.licensing.may_back_up(now_ts().as_second()) {
            return Err(Error::Invalid(licensing::TRIAL_ENDED.into()));
        }
        let cancel = {
            let mut st = self.state.lock().unwrap();
            if st.active.contains_key(&id) || st.queue.iter().any(|(j, _)| *j == id) {
                return Ok(Start::AlreadyRunning);
            }
            if st.active.len() >= MAX_CONCURRENT {
                st.queue.push_back((id, trigger));
                return Ok(Start::Queued);
            }
            let c = Arc::new(AtomicBool::new(false));
            st.active.insert(id, c.clone());
            c
        };
        self.spawn_run(job, cancel, trigger);
        Ok(Start::Started)
    }

    fn spawn_run(self: &Arc<Self>, job: Job, cancel: Arc<AtomicBool>, trigger: Trigger) {
        let me = self.clone();
        let work = move || {
            let _awake = platform::AwakeGuard::new();
            if let Ok(store) = me.store() {
                let conn = Agent::connector_for(&job);
                // FR-18: wait out a NAS that is briefly away. The engine then records the run as usual,
                // including a failed run if the NAS never came back.
                let reach = retry::wait_until_reachable(&conn, &me.retry, &cancel);
                let run = run_job_as(&store, &conn, &job, now_ts().as_second(), &cancel, &NoProgress, trigger);
                let _ = store.prune_logs(KEEP_LOG_RUNS);
                if let Some((title, body)) = failure_notice(&job, &reach, run.as_ref().ok(), &me.retry) {
                    me.notifier.notify(&title, &body);
                }
            }
            let next = {
                let mut st = me.state.lock().unwrap();
                st.active.remove(&job.id);
                st.queue.pop_front()
            };
            if let Some((n, t)) = next {
                let _ = me.start_job_as(n, t);
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
        st.queue.retain(|(j, _)| *j != id);
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
        if !self.licensing.may_back_up(now.as_second()) {
            return Ok(vec![]);
        }
        let store = self.store()?;
        let mut started = vec![];
        for job in store.list_jobs()? {
            let last = store.last_run_started(job.id)?.and_then(|s| Timestamp::from_second(s).ok());
            if backer_sched::is_due(&job, last, now, &self.tz) && self.start_job_as(job.id, Trigger::Schedule)? != Start::AlreadyRunning {
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

    /// Checks that every file the job backs up is on the NAS with the right size. Reads only; changes nothing.
    pub fn verify_job(&self, id: JobId) -> std::result::Result<Value, String> {
        let store = self.store().map_err(es)?;
        let job = self.require_job(&store, id)?;
        if self.is_running(id) {
            return Err("This job is backing up right now. Check it again when the backup has finished.".into());
        }
        let rep = verify_job(&store, &Agent::connector_for(&job), &job, &AtomicBool::new(false)).map_err(es)?;
        serde_json::to_value(rep).map_err(|e| e.to_string())
    }

    /// Forgets a job and its history. Never touches the NAS or the source folders.
    /// `expected_name` is required from assistants so a wrong id cannot delete the wrong job.
    pub fn delete_job(&self, id: JobId, expected_name: Option<&str>) -> std::result::Result<Value, String> {
        let store = self.store().map_err(es)?;
        if let Some(job) = store.get_job(id).map_err(es)? {
            if let Some(n) = expected_name {
                if n != job.name {
                    return Err(format!("job {id} is named \"{}\", not \"{n}\". Nothing was deleted.", job.name));
                }
            }
            if self.is_running(id) {
                return Err("This job is running. Stop it first, then delete it.".into());
            }
            self.cancel_job(id); // drops it from the queue if it was waiting
            store.delete_job(id).map_err(es)?;
        } else if expected_name.is_some() {
            return Err(format!("no job {id}"));
        }
        Ok(json!({"deleted": true}))
    }

    fn require_job(&self, store: &Store, id: JobId) -> std::result::Result<Job, String> {
        store.get_job(id).map_err(|e| e.to_string())?.ok_or_else(|| format!("no job {id}"))
    }
}

/// Spec 8.3: a failed run raises a notification by default. A run the user stopped does not.
fn failure_notice(job: &Job, reach: &retry::Reach, run: Option<&RunSummary>, policy: &retry::RetryPolicy) -> Option<(String, String)> {
    if matches!(reach, retry::Reach::Cancelled) {
        return None;
    }
    let run = run?;
    if run.status != RunStatus::Failed {
        return None;
    }
    let title = format!("Backup of \"{}\" failed", job.name);
    let body = match reach {
        retry::Reach::Unreachable { .. } if policy.retries > 0 => format!(
            "Backer could not reach the NAS, even after trying {} more times over {}. It will try again at the next scheduled backup.",
            policy.retries,
            retry::describe(policy.total_wait())
        ),
        retry::Reach::Unreachable { .. } => "Backer could not reach the NAS. It will try again at the next scheduled backup.".to_string(),
        _ => run.message.clone().unwrap_or_else(|| "Open Backer to see what went wrong.".into()),
    };
    Some((title, body))
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
    fn get_run_log(&self, run_id: i64, q: &LogQuery) -> std::result::Result<Value, String> {
        let page = self.0.store().map_err(es)?.run_log(run_id, q).map_err(es)?;
        serde_json::to_value(page).map_err(|e| e.to_string())
    }
    fn get_run_summary(&self, run_id: i64) -> std::result::Result<Value, String> {
        serde_json::to_value(self.0.store().map_err(es)?.run_report(run_id).map_err(es)?).map_err(|e| e.to_string())
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
    fn delete_job(&self, id: JobId, job_name: &str) -> std::result::Result<Value, String> {
        self.0.delete_job(id, Some(job_name))
    }
    fn create_job(&self, suggested_name: Option<&str>) -> std::result::Result<Value, String> {
        let mut slot = self.0.new_job_request.lock().unwrap();
        if slot.is_some() {
            return Err("A set-up request is already waiting for the user in Backer. Ask them to finish or dismiss it.".into());
        }
        *slot = Some(suggested_name.unwrap_or("").trim().chars().take(80).collect());
        Ok(json!({"status": "waiting_for_user", "message": "Backer is asking the user to choose the folders, NAS destination and schedule. Nothing has been created yet; check list_jobs later."}))
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
            "get_run_log" => ctl.get_run_log(p.get("run_id").and_then(Value::as_i64).ok_or("run_id required")?, &backer_mcp::log_query(&p, 200)),
            "get_run_summary" => ctl.get_run_summary(p.get("run_id").and_then(Value::as_i64).ok_or("run_id required")?),
            "test_connection" => ctl.test_connection(need()?),
            "run_job" => ctl.run_job(need()?),
            "pause_job" => ctl.pause_job(need()?),
            "resume_job" => ctl.resume_job(need()?),
            "delete_job" => agent.delete_job(need()?, None),
            "license_status" => Ok(agent.licensing.json(now_ts().as_second())),
            "set_license_key" => agent.licensing.set_key(p.get("key").and_then(Value::as_str).unwrap_or(""), now_ts().as_second()).map(|_| agent.licensing.json(now_ts().as_second())),
            "verify_job" => agent.verify_job(need()?),
            "new_job_request" => Ok(json!({"request": agent.new_job_request.lock().unwrap().clone()})),
            "dismiss_new_job_request" => { *agent.new_job_request.lock().unwrap() = None; Ok(json!({"ok": true})) }
            "cancel_job" => Ok(json!({"cancelled": agent.cancel_job(need()?)})),
            "save_job" => {
                let mut job: Job = serde_json::from_value(p.get("job").cloned().unwrap_or(Value::Null)).map_err(|e| e.to_string())?;
                let id = agent.store().map_err(es)?.save_job(&mut job).map_err(es)?;
                *agent.new_job_request.lock().unwrap() = None; // the user has set up a job, so any waiting request is done
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
    fn run_summary_and_paged_log_are_served_to_the_app_and_assistants() {
        let t = setup();
        for i in 0..5 {
            fs::write(t.src.join(format!("f{i}.txt")), b"x").unwrap();
        }
        t.agent.start_job(t.id).unwrap();
        let ui = |m: &str, p: Value| handle_ui(&t.agent, &json!({"method": m, "params": p}));
        let run = ui("list_runs", json!({"job_id": t.id}))["result"][0].clone();
        assert_eq!((run["stats"]["trigger"].as_str(), run["stats"]["new_files"].as_u64()), (Some("manual"), Some(5)));
        let rid = run["id"].as_i64().unwrap();
        let sum = ui("get_run_summary", json!({"run_id": rid}));
        assert_eq!(sum["result"]["run"]["stats"]["copied"], 5);
        assert_eq!(sum["result"]["folders"][0]["copied"], 5);
        assert_eq!(sum["result"]["counts"]["new"], 5);
        let page = ui("get_run_log", json!({"run_id": rid, "kind": "new", "limit": 2, "offset": 1}));
        assert_eq!((page["result"]["total"].as_u64(), page["result"]["rows"].as_array().unwrap().len()), (Some(5), 2));
        let found = ui("get_run_log", json!({"run_id": rid, "search": "f3"}));
        assert_eq!(found["result"]["total"], 1);
        assert_eq!(ui("get_run_summary", json!({"run_id": 9999}))["ok"], false);
    }

    #[test]
    fn scheduled_runs_are_recorded_as_scheduled() {
        let t = setup();
        fs::write(t.src.join("a"), b"1").unwrap();
        AgentControl(t.agent.clone()).set_schedule(t.id, Schedule::EveryHours { hours: 1 }).unwrap();
        assert_eq!(t.agent.tick(now_ts()).unwrap(), vec![t.id]);
        let runs = t.agent.store().unwrap().list_runs(t.id, 5).unwrap();
        assert_eq!(runs[0].stats.trigger, "schedule");
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
    fn assistant_delete_needs_the_exact_name_and_never_touches_the_nas() {
        let t = setup();
        fs::write(t.src.join("a"), b"1").unwrap();
        t.agent.start_job(t.id).unwrap();
        let ctl = AgentControl(t.agent.clone());
        assert!(ctl.delete_job(t.id, "wrong name").unwrap_err().contains("Nothing was deleted"));
        assert!(t.agent.store().unwrap().get_job(t.id).unwrap().is_some());
        assert!(ctl.delete_job(9999, "j").is_err());
        ctl.delete_job(t.id, "j").unwrap();
        assert!(t.agent.store().unwrap().get_job(t.id).unwrap().is_none());
        assert!(t.dst.join("mac/src/a").exists(), "NAS file must remain");
    }

    #[test]
    fn assistant_create_job_only_leaves_a_request_for_the_user() {
        let t = setup();
        let ctl = AgentControl(t.agent.clone());
        let before = t.agent.store().unwrap().list_jobs().unwrap().len();
        assert_eq!(ctl.create_job(Some("Photos")).unwrap()["status"], "waiting_for_user");
        assert_eq!(t.agent.store().unwrap().list_jobs().unwrap().len(), before, "no job is created by the assistant");
        assert!(ctl.create_job(None).unwrap_err().contains("already waiting"));
        let r = handle_ui(&t.agent, &json!({"method":"new_job_request"}));
        assert_eq!(r["result"]["request"], "Photos");
        handle_ui(&t.agent, &json!({"method":"dismiss_new_job_request"}));
        assert!(handle_ui(&t.agent, &json!({"method":"new_job_request"}))["result"]["request"].is_null());
        // Saving a job in the app also clears a waiting request.
        ctl.create_job(None).unwrap();
        let job = t.agent.store().unwrap().get_job(t.id).unwrap().unwrap();
        handle_ui(&t.agent, &json!({"method":"save_job","params":{"job": job}}));
        assert!(handle_ui(&t.agent, &json!({"method":"new_job_request"}))["result"]["request"].is_null());
    }

    #[test]
    fn verify_finds_missing_files_and_run_with_skips_is_not_success() {
        let t = setup();
        fs::write(t.src.join("a"), b"1").unwrap();
        fs::write(t.src.join("b"), b"22").unwrap();
        t.agent.start_job(t.id).unwrap();
        let v = |id| handle_ui(&t.agent, &json!({"method":"verify_job","params":{"job_id": id}}))["result"].clone();
        let r = v(t.id);
        assert_eq!((r["checked"].as_u64(), r["ok"].as_u64(), r["missing_total"].as_u64()), (Some(2), Some(2), Some(0)));
        // A file that never reached the NAS is reported by name.
        fs::write(t.src.join("c"), b"333").unwrap();
        fs::remove_file(t.dst.join("mac/src/b")).unwrap();
        let r = v(t.id);
        assert_eq!((r["checked"].as_u64(), r["ok"].as_u64(), r["missing_total"].as_u64()), (Some(3), Some(1), Some(2)));
        assert!(r["missing"].as_array().unwrap().iter().any(|p| p.as_str().unwrap().ends_with('b')));
        // Verify never copies anything.
        assert!(!t.dst.join("mac/src/c").exists());
        // Changed since the backup is called that, not damage.
        fs::write(t.src.join("a"), b"1111").unwrap();
        let r = v(t.id);
        assert_eq!(r["changed_total"], 1);
        assert_eq!(r["wrong_size_total"], 0);
    }

    #[test]
    fn after_the_trial_backups_stop_but_reading_and_verify_keep_working() {
        let t = tempfile::tempdir().unwrap();
        let (src, dst) = (t.path().join("src"), t.path().join("nas"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a"), b"1").unwrap();
        // A trial that started 30 days ago.
        let started = now_ts().as_second() - 30 * 86_400;
        fs::write(t.path().join("license.json"), json!({"first_run": started, "last_seen": started, "key": null}).to_string()).unwrap();
        let agent = Agent::new_sync(t.path().join("db.sqlite"), TimeZone::get("America/Los_Angeles").unwrap()).unwrap();
        let ui = |m: &str, p: Value| handle_ui(&agent, &json!({"method": m, "params": p}));
        assert_eq!(ui("license_status", json!({}))["result"]["state"], "expired");
        ui("init_destination", json!({"path": dst}));
        let job = Job { id: 0, name: "j".into(), folders: vec![FolderPair { source: src.to_string_lossy().into(), dest: "mac/src".into() }],
            dest_root: dst.to_string_lossy().into(), schedule: Schedule::Daily { hour: 2, minute: 0 }, paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None };
        let id = ui("save_job", json!({"job": job}))["result"]["job_id"].as_i64().unwrap();
        // Starting is refused with a message people can act on, from the app and from an assistant alike.
        let msg = ui("run_job", json!({"job_id": id}))["error"].as_str().unwrap().to_string();
        assert!(msg.contains("trial has ended") && msg.contains("license key"), "{msg}");
        assert!(AgentControl(agent.clone()).run_job(id).unwrap_err().contains("trial has ended"));
        assert!(!dst.join("mac/src/a").exists());
        // The schedule starts nothing.
        assert!(agent.tick(now_ts() + jiff::SignedDuration::from_hours(48)).unwrap().is_empty());
        // Reading and checking still work.
        assert!(ui("list_jobs", json!({}))["result"].is_array());
        assert!(ui("verify_job", json!({"job_id": id}))["result"]["missing_total"].as_u64() == Some(1));
        // An invalid key is refused and the trial state is unchanged.
        assert!(ui("set_license_key", json!({"key": "BACKER-AAAAA-BBBBB"}))["error"].is_string());
        assert_eq!(ui("license_status", json!({}))["result"]["state"], "expired");
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

    fn summary(status: RunStatus, message: Option<&str>) -> RunSummary {
        RunSummary { id: 1, job_id: 1, started: 0, finished: Some(1), status, stats: RunStats::default(), message: message.map(String::from) }
    }

    #[test]
    fn failure_notice_explains_an_unreachable_nas_and_skips_other_outcomes() {
        let t = setup();
        let job = t.agent.store().unwrap().get_job(t.id).unwrap().unwrap();
        let p = retry::RetryPolicy::default();
        let away = retry::Reach::Unreachable { error: "gone".into() };
        let (title, body) = failure_notice(&job, &away, Some(&summary(RunStatus::Failed, Some("gone"))), &p).unwrap();
        assert_eq!(title, "Backup of \"j\" failed");
        assert!(body.contains("3 more times over 15 minutes") && body.contains("next scheduled backup"), "{body}");
        // Other failures pass on the engine's message.
        let ready = retry::Reach::Ready { retries_used: 0 };
        let (_, body) = failure_notice(&job, &ready, Some(&summary(RunStatus::Failed, Some("source not available: /x"))), &p).unwrap();
        assert_eq!(body, "source not available: /x");
        // Success, warnings and a stopped job raise nothing.
        assert!(failure_notice(&job, &ready, Some(&summary(RunStatus::Ok, None)), &p).is_none());
        assert!(failure_notice(&job, &ready, Some(&summary(RunStatus::Warnings, None)), &p).is_none());
        assert!(failure_notice(&job, &retry::Reach::Cancelled, Some(&summary(RunStatus::Failed, None)), &p).is_none());
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
