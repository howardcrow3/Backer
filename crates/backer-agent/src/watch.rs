//! "Back up when files change": watches a job's source folders with the OS file-event API (FSEvents on macOS,
//! ReadDirectoryChangesW on Windows) and tells the scheduler's `ChangeTracker` when something changed.
//!
//! Events are only a hint. A run always walks the whole folder, so a lost, merged or overflowed event can never
//! cause a missed file: anything we cannot trust (event overflow, a watcher error, a folder that cannot be watched)
//! simply marks the job dirty, which leads to a full scan. Excluded paths do not count as changes, so churn in
//! `node_modules/` does not start backups.
use backer_core::{ExcludeSet, Job};
use backer_engine::DEFAULT_EXCLUDES;
use backer_sched::ChangeTracker;
use jiff::Timestamp;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) type Tracker = Arc<Mutex<ChangeTracker>>;

/// Live watchers for one job, plus what they were built from so a changed job gets new ones.
pub(crate) struct Watch {
    sources: Vec<String>,
    exclude: Vec<String>,
    /// True when some folder could not be watched (gone, unmounted, system limit): the agent then polls with a full scan.
    pub degraded: bool,
    _watchers: Vec<RecommendedWatcher>,
}

impl Watch {
    pub fn matches(&self, job: &Job) -> bool {
        self.sources == sources_of(job) && self.exclude == job.exclude_names
    }

    pub fn start(job: &Job, tracker: &Tracker, now: Timestamp) -> Self {
        let roots: Vec<PathBuf> = sources_of(job).iter().map(PathBuf::from).collect();
        // Validated when the job is saved; an invalid set just means nothing is excluded.
        let set = Arc::new(ExcludeSet::new(&job.exclude_names).unwrap_or_default());
        let roots_arc = Arc::new(roots.clone());
        let mut watchers = Vec::new();
        let mut degraded = false;
        for root in &roots {
            let (t, set, roots, id) = (tracker.clone(), set.clone(), roots_arc.clone(), job.id);
            let handler = move |res: notify::Result<Event>| {
                if event_matters(&res, &roots, &set) {
                    t.lock().unwrap().mark(id, crate::now_ts().as_second());
                }
            };
            match RecommendedWatcher::new(handler, Config::default()).and_then(|mut w| w.watch(root, RecursiveMode::Recursive).map(|_| w)) {
                Ok(w) => watchers.push(w),
                Err(_) => degraded = true,
            }
        }
        // Whatever happened while the agent was not watching is found by a first full scan.
        tracker.lock().unwrap().mark(job.id, now.as_second());
        Self { sources: sources_of(job), exclude: job.exclude_names.clone(), degraded, _watchers: watchers }
    }
}

fn sources_of(job: &Job) -> Vec<String> {
    job.folders.iter().map(|f| f.source.clone()).collect()
}

/// Should this file-system report start (or extend) a backup? Errors and rescan requests always do.
pub(crate) fn event_matters(res: &notify::Result<Event>, roots: &[PathBuf], set: &ExcludeSet) -> bool {
    let ev = match res {
        Ok(ev) => ev,
        Err(_) => return true,
    };
    if ev.need_rescan() {
        return true;
    }
    match ev.kind {
        // Opening or reading a file changes nothing.
        EventKind::Access(_) => false,
        _ => ev.paths.is_empty() || ev.paths.iter().any(|p| !is_excluded(p, roots, set)),
    }
}

/// Mirrors the engine's walk: a path is left out when it, or any folder above it, is excluded.
pub(crate) fn is_excluded(path: &Path, roots: &[PathBuf], set: &ExcludeSet) -> bool {
    let Some(rel) = roots.iter().find_map(|r| path.strip_prefix(r).ok()) else { return false };
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let mut prefix = String::new();
    for (i, part) in parts.iter().enumerate() {
        if DEFAULT_EXCLUDES.contains(&part.as_str()) {
            return true;
        }
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        let is_dir = i + 1 < parts.len() || path.is_dir();
        if set.is_excluded(&prefix, is_dir) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, CreateKind, Flag, ModifyKind};

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/data/docs")]
    }
    fn set() -> ExcludeSet {
        ExcludeSet::new(&["*.tmp", "node_modules/"]).unwrap()
    }
    fn ev(kind: EventKind, path: &str) -> notify::Result<Event> {
        Ok(Event::new(kind).add_path(PathBuf::from(path)))
    }

    #[test]
    fn real_changes_matter_reads_and_excluded_paths_do_not() {
        let (r, s) = (roots(), set());
        assert!(event_matters(&ev(EventKind::Create(CreateKind::File), "/data/docs/a.txt"), &r, &s));
        assert!(event_matters(&ev(EventKind::Modify(ModifyKind::Any), "/data/docs/sub/b.txt"), &r, &s));
        assert!(!event_matters(&ev(EventKind::Access(AccessKind::Any), "/data/docs/a.txt"), &r, &s));
        assert!(!event_matters(&ev(EventKind::Modify(ModifyKind::Any), "/data/docs/x.tmp"), &r, &s));
        assert!(!event_matters(&ev(EventKind::Modify(ModifyKind::Any), "/data/docs/app/node_modules/pkg/index.js"), &r, &s));
        assert!(!event_matters(&ev(EventKind::Modify(ModifyKind::Any), "/data/docs/.DS_Store"), &r, &s));
        // A rename reports both paths: one that matters is enough.
        let rename = Event::new(EventKind::Modify(ModifyKind::Any)).add_path("/data/docs/a.tmp".into()).add_path("/data/docs/a.txt".into());
        assert!(event_matters(&Ok(rename), &r, &s));
    }

    #[test]
    fn overflow_and_errors_always_mean_rescan() {
        let (r, s) = (roots(), set());
        // Even an excluded path: the report says events were lost, so anything may have changed.
        let overflow = Event::new(EventKind::Other).add_path("/data/docs/x.tmp".into()).set_flag(Flag::Rescan);
        assert!(event_matters(&Ok(overflow), &r, &s));
        assert!(event_matters(&Err(notify::Error::generic("queue overflow")), &r, &s));
    }
}
