//! Restore for the app: browse a job's backup, plan a restore, then run it in the background with progress.
//! App-only. Assistants (MCP) cannot restore, because restoring writes to this computer.
use crate::{es, Agent};
use backer_core::*;
use backer_restore::{Plan, Report, RestoreStatus, Target};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Conflicts sent to the app by name; the rest are counted.
const SHOW_CONFLICTS: usize = 200;
/// Plans kept waiting for the user's answer. Older ones are dropped.
const KEEP_PLANS: usize = 4;

struct Active {
    job_id: JobId,
    cancel: Arc<AtomicBool>,
    report: Report,
}

#[derive(Default)]
pub struct Restores {
    next_id: Mutex<u64>,
    plans: Mutex<VecDeque<(u64, JobId, Plan)>>,
    active: Arc<Mutex<Option<Active>>>,
}

fn target_of(p: &Value) -> Target {
    match p.get("target").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(dir) => Target::Folder(PathBuf::from(dir)),
        None => Target::Original,
    }
}

impl Agent {
    pub fn browse_backup(&self, id: JobId, path: &str) -> std::result::Result<Value, String> {
        let store = self.store().map_err(es)?;
        let job = self.require_job(&store, id)?;
        let entries = backer_restore::browse(&*self.connector_for(&job), &job, path).map_err(es)?;
        Ok(json!({"path": path.trim_matches('/'), "entries": entries}))
    }

    /// Works out what a restore would write. Returns a plan id the app passes back to start it,
    /// and the files that already exist here so the app can ask before replacing them.
    pub fn plan_restore(&self, id: JobId, p: &Value) -> std::result::Result<Value, String> {
        let store = self.store().map_err(es)?;
        let job = self.require_job(&store, id)?;
        let items: Vec<String> = p.get("paths").and_then(Value::as_array).ok_or("paths required")?.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        let target = target_of(p);
        let plan = backer_restore::plan(&*self.connector_for(&job), &job, &items, &target, &AtomicBool::new(false)).map_err(es)?;
        let conflicts: Vec<_> = plan.conflicts.iter().take(SHOW_CONFLICTS).collect();
        let mut out = json!({
            "files": plan.files.len(), "bytes": plan.bytes,
            "conflicts_total": plan.conflicts.len(),
            "blocked_total": plan.conflicts.iter().filter(|c| c.blocked).count(),
            "conflicts": conflicts,
            "target": match &target { Target::Original => None, Target::Folder(d) => Some(d.display().to_string()) },
        });
        let plan_id = {
            let mut n = self.restores.next_id.lock().unwrap();
            *n += 1;
            *n
        };
        let mut plans = self.restores.plans.lock().unwrap();
        plans.push_back((plan_id, id, plan));
        while plans.len() > KEEP_PLANS {
            plans.pop_front();
        }
        out["plan_id"] = json!(plan_id);
        Ok(out)
    }

    /// Starts a planned restore. `replace` true means the user confirmed replacing the files the plan listed;
    /// false keeps every file already here. Files that appear after the plan are never replaced.
    pub fn start_restore(self: &Arc<Self>, plan_id: u64, replace: bool) -> std::result::Result<Value, String> {
        let (job_id, plan) = {
            let mut plans = self.restores.plans.lock().unwrap();
            let i = plans.iter().position(|(p, _, _)| *p == plan_id).ok_or("This restore is out of date. Choose the files again.")?;
            let (_, j, plan) = plans.remove(i).unwrap();
            (j, plan)
        };
        let store = self.store().map_err(es)?;
        let job = self.require_job(&store, job_id)?;
        if self.is_running(job_id) {
            return Err("This job is backing up right now. Restore when the backup has finished.".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut a = self.restores.active.lock().unwrap();
            if matches!(&*a, Some(x) if x.report.status == RestoreStatus::Running) {
                return Err("A restore is already in progress. Wait for it to finish or stop it first.".into());
            }
            *a = Some(Active { job_id, cancel: cancel.clone(), report: Report::new(&plan) });
        }
        let replace: HashSet<PathBuf> = if replace { plan.replaceable() } else { HashSet::new() };
        let active = self.restores.active.clone();
        let conn = self.connector_for(&job);
        let work = move || {
            let set = |r: &Report| {
                if let Some(a) = active.lock().unwrap().as_mut() {
                    a.report = r.clone();
                }
            };
            let fin = backer_restore::execute(&*conn, &plan, &replace, &cancel, &|r| set(r));
            set(&fin);
        };
        if self.sync_runs {
            work();
        } else {
            std::thread::spawn(work);
        }
        Ok(json!({"started": true}))
    }

    /// True while a restore for this job is copying files.
    pub fn restoring(&self, id: JobId) -> bool {
        matches!(&*self.restores.active.lock().unwrap(), Some(a) if a.job_id == id && a.report.status == RestoreStatus::Running)
    }

    pub fn restore_status(&self) -> Value {
        match &*self.restores.active.lock().unwrap() {
            Some(a) => json!({"job_id": a.job_id, "report": a.report}),
            None => json!({"job_id": null, "report": null}),
        }
    }

    pub fn cancel_restore(&self) -> bool {
        match &*self.restores.active.lock().unwrap() {
            Some(a) if a.report.status == RestoreStatus::Running => {
                a.cancel.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    /// Clears a finished restore's result once the app has shown it.
    pub fn dismiss_restore(&self) {
        let mut a = self.restores.active.lock().unwrap();
        if !matches!(&*a, Some(x) if x.report.status == RestoreStatus::Running) {
            *a = None;
        }
    }
}

#[cfg(test)]
impl Agent {
    /// Marks a restore for `job_id` as in progress without copying anything.
    pub(crate) fn fake_running_restore(&self, job_id: JobId) {
        *self.restores.active.lock().unwrap() = Some(Active { job_id, cancel: Arc::new(AtomicBool::new(false)), report: Report::new(&Plan::default()) });
    }
}
