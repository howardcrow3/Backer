//! Checks that the copies on the NAS are still intact: re-reads them and compares each one with the BLAKE3
//! checksum saved when it was backed up. Runs on a schedule or when the user asks, in the background with progress.
//! Read-only on the NAS: it never copies, repairs, renames or deletes anything there. Problems are reported,
//! and the user decides what to do (a normal backup replaces a changed file; a damaged copy of an unchanged
//! file is reported but not touched).
use crate::{es, now_ts, Agent};
use backer_core::*;
use jiff::Timestamp;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Saved checks kept per job.
const KEEP_CHECKS: u32 = 30;

struct Active {
    job_id: JobId,
    id: i64,
    cancel: Arc<AtomicBool>,
    checked: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
}

#[derive(Default)]
pub struct Integrity {
    /// One check at a time: reading a whole NAS is heavy, so they queue up behind the schedule instead of piling on.
    active: Arc<Mutex<Option<Active>>>,
    /// Scheduled checks that were due but found the agent busy; they start as soon as it is free.
    pending: Mutex<std::collections::HashSet<JobId>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum VerifyStart {
    Started,
    /// A check is already running (for this or another job).
    Busy,
}

fn trigger_str(t: Trigger) -> &'static str {
    t.as_str()
}

/// Schedules that make sense for reading a whole backup: not every few minutes.
fn check_schedule(job: &Job, s: &Schedule) -> std::result::Result<(), String> {
    if matches!(s, Schedule::EveryMinutes { .. }) {
        return Err("Checking file contents reads every file on the NAS, so choose hours, days, weeks or months.".into());
    }
    let mut j = job.clone();
    j.schedule = s.clone();
    j.validate().map_err(es)
}

impl Agent {
    /// Starts a check of the job's NAS copies in the background. A manual start that cannot go ahead says why;
    /// a scheduled one that cannot just waits for the next tick.
    pub fn start_verification(self: &Arc<Self>, id: JobId, trigger: Trigger) -> std::result::Result<VerifyStart, String> {
        let store = self.store().map_err(es)?;
        let job = self.require_job(&store, id)?;
        if self.is_running(id) {
            return if trigger == Trigger::Schedule { Ok(VerifyStart::Busy) } else { Err("This job is backing up right now. Check the copies when the backup has finished.".into()) };
        }
        if self.restoring(id) {
            return if trigger == Trigger::Schedule { Ok(VerifyStart::Busy) } else { Err("Backer is restoring files for this job. Check the copies when the restore has finished.".into()) };
        }
        let now = now_ts().as_second();
        let (cancel, checked, total, run) = {
            let mut a = self.integrity.active.lock().unwrap();
            if a.is_some() {
                return Ok(VerifyStart::Busy);
            }
            let run = store.begin_verification(id, now, trigger_str(trigger)).map_err(es)?;
            let (cancel, checked, total) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
            *a = Some(Active { job_id: id, id: run, cancel: cancel.clone(), checked: checked.clone(), total: total.clone() });
            (cancel, checked, total, run)
        };
        let me = self.clone();
        let work = move || {
            let _awake = crate::platform::AwakeGuard::new();
            let conn = me.connector_for(&job);
            let res = store_run(&me, &*conn, &job, &cancel, &checked, &total);
            let (status, report, message) = match res {
                Ok(r) if r.cancelled => (VerifyStatus::Cancelled, r, None),
                Ok(r) if r.problems() > 0 => (VerifyStatus::Problems, r, None),
                Ok(r) => (VerifyStatus::Ok, r, None),
                Err(e) => (VerifyStatus::Failed, ChecksumReport::default(), Some(e.to_string())),
            };
            if let Ok(store) = me.store() {
                let _ = store.finish_verification(run, now_ts().as_second(), status, &report, message.as_deref());
                let _ = store.prune_verifications(KEEP_CHECKS);
            }
            if let Some((title, body)) = notice(&job, status, &report, message.as_deref()) {
                me.notifier.notify(&title, &body);
            }
            *me.integrity.active.lock().unwrap() = None;
        };
        if self.sync_runs {
            work();
        } else {
            std::thread::spawn(work);
        }
        Ok(VerifyStart::Started)
    }

    /// Progress of the check in progress, if any.
    pub fn verification_status(&self) -> Value {
        match &*self.integrity.active.lock().unwrap() {
            Some(a) => json!({"running": true, "job_id": a.job_id, "id": a.id, "checked": a.checked.load(Ordering::Relaxed), "total": a.total.load(Ordering::Relaxed)}),
            None => json!({"running": false}),
        }
    }

    pub fn cancel_verification(&self) -> bool {
        match &*self.integrity.active.lock().unwrap() {
            Some(a) => {
                a.cancel.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    pub fn list_verifications(&self, id: JobId, limit: u32) -> std::result::Result<Value, String> {
        let store = self.store().map_err(es)?;
        self.require_job(&store, id)?;
        serde_json::to_value(store.list_verifications(id, limit.clamp(1, KEEP_CHECKS)).map_err(es)?).map_err(|e| e.to_string())
    }

    /// Sets how often the job's NAS copies are checked. `Manual` turns the schedule off.
    pub fn set_verify_schedule(&self, id: JobId, s: Schedule) -> std::result::Result<Value, String> {
        let store = self.store().map_err(es)?;
        let job = self.require_job(&store, id)?;
        check_schedule(&job, &s)?;
        store.set_verify_schedule(id, &s).map_err(es)?;
        self.integrity_json(&store, &job).map_err(es)
    }

    /// What the app shows for a job: how often it is checked, when next, and how the last check went.
    pub(crate) fn integrity_json(&self, store: &backer_store::Store, job: &Job) -> Result<Value> {
        let schedule = store.verify_schedule(job.id)?;
        let last_start = store.last_verification_started(job.id)?.and_then(|s| Timestamp::from_second(s).ok());
        let next = backer_sched::next_run(&schedule, last_start, now_ts(), &self.tz).map(|t| t.as_second());
        let last = store.list_verifications(job.id, 1)?.into_iter().next();
        let running = matches!(&*self.integrity.active.lock().unwrap(), Some(a) if a.job_id == job.id);
        Ok(json!({"schedule": schedule, "next": next, "last": last, "running": running, "files": store.count_files(job.id)?}))
    }

    /// Starts every check that is due. Called from the scheduler tick; read-only, so it also runs after the trial ends.
    pub(crate) fn verify_tick(self: &Arc<Self>, now: Timestamp) -> Result<()> {
        let store = self.store()?;
        for job in store.list_jobs()? {
            let schedule = store.verify_schedule(job.id)?;
            if schedule == Schedule::Manual {
                self.integrity.pending.lock().unwrap().remove(&job.id);
                continue;
            }
            let last = store.last_verification_started(job.id)?.and_then(|s| Timestamp::from_second(s).ok());
            // Same slot rules as backups (paused jobs wait, a missed slot is made up once), with the check's own schedule.
            let mut as_job = job.clone();
            as_job.schedule = schedule;
            let waiting = self.integrity.pending.lock().unwrap().contains(&job.id);
            if waiting || backer_sched::is_due(&as_job, last, now, &self.tz) {
                // Busy now means it is kept waiting and tried again at every tick until it can start.
                let busy = matches!(self.start_verification(job.id, Trigger::Schedule), Ok(VerifyStart::Busy));
                let mut pending = self.integrity.pending.lock().unwrap();
                if busy { pending.insert(job.id); } else { pending.remove(&job.id); }
            }
        }
        Ok(())
    }
}

fn store_run(me: &Arc<Agent>, conn: &dyn backer_connector::Connector, job: &Job, cancel: &AtomicBool, checked: &AtomicU64, total: &AtomicU64) -> Result<ChecksumReport> {
    let store = me.store()?;
    backer_engine::verify_checksums(&store, conn, job, cancel, &|c, t| {
        checked.store(c, Ordering::Relaxed);
        total.store(t, Ordering::Relaxed);
    })
}

/// The notification for a finished check. Good news is not announced; a check the user stopped is not either.
fn notice(job: &Job, status: VerifyStatus, r: &ChecksumReport, message: Option<&str>) -> Option<(String, String)> {
    match status {
        VerifyStatus::Problems => {
            let mut bits = vec![];
            if r.damaged_total > 0 {
                bits.push(format!("{} damaged", r.damaged_total));
            }
            if r.missing_total > 0 {
                bits.push(format!("{} missing", r.missing_total));
            }
            if r.unreadable_total > 0 {
                bits.push(format!("{} could not be read", r.unreadable_total));
            }
            Some((format!("Backer found problems with {}", job.name), format!("{} of the copies on your NAS: {}. Open Backer to see which files.", r.problems(), bits.join(", "))))
        }
        VerifyStatus::Failed => Some((format!("Backer could not check {}", job.name), message.unwrap_or("Open Backer to see what went wrong.").to_string())),
        _ => None,
    }
}
