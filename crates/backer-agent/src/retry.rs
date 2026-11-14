//! FR-18 / spec 8.2: when the NAS is unreachable, wait and try again before giving up on the run.
//!
//! The check happens before the engine starts a run, so a NAS that comes back during the wait
//! gets a normal run, and one that stays away gets one failed run (recorded by the engine) and
//! one notification, not a run per attempt.
use backer_connector::Connector;
use backer_core::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Further checks after the first one fails.
    pub retries: u32,
    /// Wait before each retry.
    pub delay: Duration,
}

impl Default for RetryPolicy {
    /// Spec default: retry 3 times, 5 minutes apart.
    fn default() -> Self {
        Self { retries: 3, delay: Duration::from_secs(5 * 60) }
    }
}

impl RetryPolicy {
    /// Total time spent waiting when every retry is used.
    pub fn total_wait(&self) -> Duration {
        self.delay * self.retries
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reach {
    /// The NAS answered, possibly after some retries.
    Ready { retries_used: u32 },
    /// Still unreachable after every retry. `error` is the last check's message.
    Unreachable { error: String },
    /// Not worth waiting for (for example a bad path); the engine records it as it is.
    NotRetryable,
    /// The user stopped the job, or the agent is quitting, while it was waiting.
    Cancelled,
}

/// Only "the NAS is not there right now" is worth waiting for. Anything else (an invalid path, and
/// rejected credentials once connectors report them separately) fails at once, so a NAS that blocks
/// an address after repeated bad logins is never triggered.
pub fn is_retryable(e: &Error) -> bool {
    matches!(e, Error::DestinationNotReady(_) | Error::Connector(_))
}

/// Checks the destination, retrying per `policy`. The wait wakes every half second to honour `cancel`.
pub fn wait_until_reachable(conn: &dyn Connector, policy: &RetryPolicy, cancel: &AtomicBool) -> Reach {
    let mut retries_used = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Reach::Cancelled;
        }
        let err = match conn.check_ready() {
            Ok(()) => return Reach::Ready { retries_used },
            Err(e) if !is_retryable(&e) => return Reach::NotRetryable,
            Err(e) => e,
        };
        if retries_used >= policy.retries {
            return Reach::Unreachable { error: err.to_string() };
        }
        if !sleep_unless_cancelled(policy.delay, cancel) {
            return Reach::Cancelled;
        }
        retries_used += 1;
    }
}

/// Returns false if cancelled during the wait.
fn sleep_unless_cancelled(total: Duration, cancel: &AtomicBool) -> bool {
    let step = Duration::from_millis(500);
    let mut left = total;
    while !left.is_zero() {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let d = left.min(step);
        std::thread::sleep(d);
        left -= d;
    }
    !cancel.load(Ordering::Relaxed)
}

/// Plain-language wait, e.g. "15 minutes".
pub fn describe(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s} seconds"),
        60..=119 => "1 minute".into(),
        _ => format!("{} minutes", s / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backer_connector::{PutResult, RemoteStat};
    use backer_core::Result;
    use std::path::Path;
    use std::sync::Mutex;

    /// Fails `check_ready` with the queued errors, then succeeds.
    struct Flaky(Mutex<Vec<Error>>, Mutex<u32>);
    impl Flaky {
        fn new(errs: Vec<Error>) -> Self {
            Self(Mutex::new(errs), Mutex::new(0))
        }
        fn checks(&self) -> u32 {
            *self.1.lock().unwrap()
        }
    }
    impl Connector for Flaky {
        fn check_ready(&self) -> Result<()> {
            *self.1.lock().unwrap() += 1;
            let mut q = self.0.lock().unwrap();
            if q.is_empty() { Ok(()) } else { Err(q.remove(0)) }
        }
        fn stat(&self, _: &str) -> Result<Option<RemoteStat>> { unimplemented!() }
        fn put_file(&self, _: &Path, _: &str, _: i64, _: u64) -> Result<PutResult> { unimplemented!() }
        fn hash(&self, _: &str) -> Result<String> { unimplemented!() }
        fn list_temp_files(&self, _: &str) -> Result<Vec<String>> { unimplemented!() }
    }
    fn down() -> Error {
        Error::DestinationNotReady("/Volumes/nas is not available".into())
    }
    const FAST: RetryPolicy = RetryPolicy { retries: 3, delay: Duration::ZERO };

    #[test]
    fn default_is_three_retries_five_minutes_apart() {
        let p = RetryPolicy::default();
        assert_eq!((p.retries, p.delay), (3, Duration::from_secs(300)));
        assert_eq!(describe(p.total_wait()), "15 minutes");
    }

    #[test]
    fn ready_at_once_needs_no_retry() {
        let c = Flaky::new(vec![]);
        assert_eq!(wait_until_reachable(&c, &FAST, &AtomicBool::new(false)), Reach::Ready { retries_used: 0 });
        assert_eq!(c.checks(), 1);
    }

    #[test]
    fn nas_that_comes_back_is_used() {
        let c = Flaky::new(vec![down(), down()]);
        assert_eq!(wait_until_reachable(&c, &FAST, &AtomicBool::new(false)), Reach::Ready { retries_used: 2 });
        assert_eq!(c.checks(), 3);
    }

    #[test]
    fn gives_up_after_three_retries() {
        let c = Flaky::new(vec![down(), down(), down(), down(), down()]);
        let r = wait_until_reachable(&c, &FAST, &AtomicBool::new(false));
        assert!(matches!(r, Reach::Unreachable { ref error } if error.contains("not available")), "{r:?}");
        assert_eq!(c.checks(), 4, "the first check plus 3 retries");
    }

    #[test]
    fn non_retryable_errors_fail_at_once() {
        let c = Flaky::new(vec![Error::Invalid("unsafe path".into())]);
        assert_eq!(wait_until_reachable(&c, &FAST, &AtomicBool::new(false)), Reach::NotRetryable);
        assert_eq!(c.checks(), 1);
    }

    #[test]
    fn cancel_stops_the_wait() {
        let c = Flaky::new(vec![down(), down(), down(), down()]);
        let cancel = AtomicBool::new(false);
        let slow = RetryPolicy { retries: 3, delay: Duration::from_secs(600) };
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancel.store(true, Ordering::Relaxed);
            });
            let t = std::time::Instant::now();
            assert_eq!(wait_until_reachable(&c, &slow, &cancel), Reach::Cancelled);
            assert!(t.elapsed() < Duration::from_secs(5));
        });
    }
}
