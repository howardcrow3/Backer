//! Trial and license state. Stored next to the database in `license.json`.
//!
//! Rules: 14 days of full use from the first launch. After that, starting backups (by hand, on a schedule or
//! from an assistant) is refused until a valid key is entered. Everything that only reads, such as the run
//! history, Verify and the deleted-files list, keeps working, so nobody is locked out of their own data.

use backer_license::{verify, License, LicenseError};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

pub const TRIAL_DAYS: i64 = 14;
const DAY: i64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Licensed { email: String },
    Trial { days_left: i64 },
    Expired,
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    first_run: i64,
    /// Latest time seen, so setting the clock back does not extend the trial.
    last_seen: i64,
    key: Option<String>,
}

pub struct Licensing {
    path: PathBuf,
    saved: Mutex<Saved>,
}

/// The app's major version, never below 1 (0.x builds count as version 1).
fn app_major() -> u8 {
    env!("CARGO_PKG_VERSION").split('.').next().and_then(|m| m.parse().ok()).unwrap_or(1).max(1)
}

impl Licensing {
    pub fn load(path: PathBuf, now: i64) -> Self {
        let mut saved: Saved = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        if saved.first_run == 0 {
            saved.first_run = now;
            saved.last_seen = now;
            let _ = std::fs::write(&path, serde_json::to_string(&saved).unwrap_or_default());
        }
        Self { path, saved: Mutex::new(saved) }
    }

    fn save(&self, s: &Saved) {
        let _ = std::fs::write(&self.path, serde_json::to_string(s).unwrap_or_default());
    }

    fn valid_license(key: &Option<String>) -> Option<License> {
        verify(key.as_deref()?).ok().filter(|l| l.major >= app_major())
    }

    pub fn status(&self, now: i64) -> Status {
        let mut s = self.saved.lock().unwrap();
        if let Some(l) = Self::valid_license(&s.key) {
            return Status::Licensed { email: l.email };
        }
        let effective = now.max(s.last_seen);
        if effective - s.last_seen >= 3600 {
            s.last_seen = effective;
            self.save(&s);
        }
        let left = (s.first_run + TRIAL_DAYS * DAY - effective + DAY - 1).div_euclid(DAY);
        if left > 0 { Status::Trial { days_left: left.min(TRIAL_DAYS) } } else { Status::Expired }
    }

    /// True when starting a backup is allowed.
    pub fn may_back_up(&self, now: i64) -> bool {
        !matches!(self.status(now), Status::Expired)
    }

    pub fn set_key(&self, key: &str, now: i64) -> Result<Status, String> {
        let lic = verify(key).map_err(|e: LicenseError| e.to_string())?;
        if lic.major < app_major() {
            return Err(format!("That key covers Backer {}.x. This is version {}.x, which needs an upgrade.", lic.major, app_major()));
        }
        {
            let mut s = self.saved.lock().unwrap();
            s.key = Some(key.trim().to_string());
            self.save(&s);
        }
        Ok(self.status(now))
    }

    pub fn json(&self, now: i64) -> serde_json::Value {
        use serde_json::json;
        let purchase_url = backer_license::PURCHASE_URL.trim();
        match self.status(now) {
            Status::Licensed { email } => json!({"state": "licensed", "email": email, "purchase_url": purchase_url}),
            Status::Trial { days_left } => json!({"state": "trial", "days_left": days_left, "purchase_url": purchase_url}),
            Status::Expired => json!({"state": "expired", "purchase_url": purchase_url}),
        }
    }
}

pub const TRIAL_ENDED: &str = "Your 14-day trial has ended. Enter a license key in Settings to keep backing up. Your backups, history, Verify and restore still work.";

#[cfg(test)]
mod tests {
    use super::*;

    fn at(dir: &std::path::Path, now: i64) -> Licensing {
        Licensing::load(dir.join("license.json"), now)
    }

    #[test]
    fn trial_counts_down_then_expires_and_survives_restarts() {
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_800_000_000;
        assert_eq!(at(d.path(), t0).status(t0), Status::Trial { days_left: 14 });
        let l = at(d.path(), t0 + 10 * DAY);
        assert_eq!(l.status(t0 + 10 * DAY), Status::Trial { days_left: 4 });
        assert_eq!(l.status(t0 + 13 * DAY + 3600), Status::Trial { days_left: 1 });
        assert!(l.may_back_up(t0 + 13 * DAY + 3600));
        assert_eq!(l.status(t0 + 14 * DAY), Status::Expired);
        assert!(!l.may_back_up(t0 + 20 * DAY));
        // A restart does not restart the trial.
        assert_eq!(at(d.path(), t0 + 30 * DAY).status(t0 + 30 * DAY), Status::Expired);
    }

    #[test]
    fn setting_the_clock_back_does_not_extend_the_trial() {
        let d = tempfile::tempdir().unwrap();
        let t0 = 1_800_000_000;
        let l = at(d.path(), t0);
        assert_eq!(l.status(t0 + 20 * DAY), Status::Expired);
        assert_eq!(l.status(t0 + 2 * DAY), Status::Expired, "going back in time must not revive the trial");
        assert_eq!(at(d.path(), t0 + DAY).status(t0 + DAY), Status::Expired);
    }

    #[test]
    fn a_junk_key_is_rejected_with_a_clear_message_and_not_saved() {
        let d = tempfile::tempdir().unwrap();
        let l = at(d.path(), 1_800_000_000);
        let e = l.set_key("BACKER-AAAAA", 1_800_000_000).unwrap_err();
        assert!(e.contains("Backer license key"), "{e}");
        assert!(matches!(l.status(1_800_000_000), Status::Trial { .. }));
    }

    #[test]
    fn a_damaged_license_file_starts_a_fresh_trial_rather_than_crashing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("license.json"), "{not json").unwrap();
        assert!(matches!(at(d.path(), 1_800_000_000).status(1_800_000_000), Status::Trial { .. }));
    }
}
