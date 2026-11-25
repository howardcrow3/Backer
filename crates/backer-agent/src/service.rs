//! The scheduler loop, shared by the background agent program and the Mac App Store edition (which runs it
//! on a thread inside the app).
use crate::{svclog::SvcLog, Agent};
use std::sync::Arc;
use std::time::Duration;

/// Keeps the license activation fresh in the background. A token is renewed after 7 days; a failed call is retried later.
pub fn spawn_license_refresh(agent: &Arc<Agent>) {
    let a = agent.clone();
    std::thread::spawn(move || loop {
        a.licensing.refresh(crate::now_ts().as_second());
        std::thread::sleep(Duration::from_secs(30 * 60));
    });
}

/// Runs scheduled backups until a quit is requested. Wall-clock based, so a sleep/wake gap simply makes the next tick catch up.
pub fn run_scheduler(agent: &Arc<Agent>, log: &SvcLog) {
    let mut last_err = String::new();
    while !agent.quit_requested() {
        match agent.tick(crate::now_ts()) {
            // Log a repeating error once, not every half minute.
            Err(e) if e.to_string() != last_err => {
                last_err = e.to_string();
                log.line(&format!("scheduler error: {e:#}"));
            }
            Ok(_) => last_err.clear(),
            _ => {}
        }
        // Sleep in short steps so a quit request from the tray is honoured within a second.
        for _ in 0..60 {
            if agent.quit_requested() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
