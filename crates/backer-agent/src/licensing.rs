//! Trial and license state. Stored next to the database in `license.json`.
//!
//! Rules: 14 days of full use from the first launch. After that, starting backups (by hand, on a schedule or
//! from an assistant) is refused until a valid key is entered. Everything that only reads, such as the run
//! history, Verify and the deleted-files list, keeps working, so nobody is locked out of their own data.
//!
//! A key works on 2 computers. Entering a key activates it online: the license service registers this computer and
//! hands back a signed token, which the app checks offline from then on. Because this is a backup app, the service
//! being down must never stop a backup:
//!   - a token is good for 90 days and is refreshed in the background once it is 7 days old;
//!   - a key that could not be confirmed (offline when entered, or after an upgrade from a version without
//!     activation) keeps working for a 14-day grace period while the app keeps trying;
//!   - only a definite answer from the service (the key is on 2 other computers, or was deactivated) stops it early.

use backer_license::activation::{machine_fingerprint, verify_activation, Activation, SERVER_URL};
use backer_license::{embedded_public_key, key_id, verify_with, License, LicenseError};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

pub const TRIAL_DAYS: i64 = 14;
/// How long a key that has not been confirmed online keeps working.
pub const GRACE_DAYS: i64 = 14;
const DAY: i64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Key checked and this computer activated.
    Licensed { email: String },
    /// Key checked, activation not confirmed yet (offline, or just upgraded). Works until the grace period ends.
    Grace { email: String, days_left: i64 },
    Trial { days_left: i64 },
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    /// The service answered no (key not valid, deactivated, or already on 2 computers). The text is for the user.
    Refused(String),
    /// No answer: offline, a timeout, or the service having a bad day. Never stops a backup.
    Unreachable(String),
}

pub trait LicenseServer: Send + Sync {
    /// Registers this computer for the key and returns the signed activation token.
    fn activate(&self, key: &str, machine: &str) -> Result<String, ServerError>;
    /// Frees this computer's place on the key.
    fn deactivate(&self, key: &str, machine: &str) -> Result<(), ServerError>;
}

/// The real service at turtlebox.studio.
pub struct HttpServer;

impl HttpServer {
    fn post(&self, route: &str, body: serde_json::Value) -> Result<serde_json::Value, ServerError> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(8))
            .timeout(std::time::Duration::from_secs(20))
            .build();
        let res = agent.post(&format!("{SERVER_URL}/{route}")).set("content-type", "application/json").send_string(&body.to_string());
        match res {
            Ok(r) => Ok(r.into_string().ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(serde_json::Value::Null)),
            Err(ureq::Error::Status(code, r)) => {
                let v: serde_json::Value = r.into_string().ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(serde_json::Value::Null);
                let msg = v.get("message").and_then(|m| m.as_str()).map(str::to_string);
                match code {
                    404 => Err(ServerError::Refused(msg.unwrap_or_else(|| LicenseError::BadSignature.to_string()))),
                    403 | 409 => Err(ServerError::Refused(msg.unwrap_or_else(|| "This key can't be activated on this computer.".into()))),
                    _ => Err(ServerError::Unreachable(format!("license server answered {code}"))),
                }
            }
            Err(e) => Err(ServerError::Unreachable(e.to_string())),
        }
    }
}

impl LicenseServer for HttpServer {
    fn activate(&self, key: &str, machine: &str) -> Result<String, ServerError> {
        let platform = if cfg!(target_os = "windows") { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" };
        let v = self.post("activate", serde_json::json!({"key": key, "machine": machine, "app_version": env!("CARGO_PKG_VERSION"), "platform": platform}))?;
        v.get("token").and_then(|t| t.as_str()).map(str::to_string).ok_or_else(|| ServerError::Unreachable("license server sent no token".into()))
    }
    fn deactivate(&self, key: &str, machine: &str) -> Result<(), ServerError> {
        self.post("deactivate", serde_json::json!({"key": key, "machine": machine})).map(|_| ())
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    first_run: i64,
    /// Latest time seen, so setting the clock back does not extend the trial or an activation.
    last_seen: i64,
    key: Option<String>,
    /// The signed activation token for `key` on this computer.
    #[serde(default)]
    token: Option<String>,
    /// When the current key was first entered without being confirmed online; the grace period counts from here.
    #[serde(default)]
    grace_start: i64,
    /// What the service said when it refused the key. While set, the key does not count.
    #[serde(default)]
    refused: Option<String>,
    /// Used as the machine id only when the OS gives none.
    #[serde(default)]
    fallback_machine: Option<String>,
}

pub struct Licensing {
    path: PathBuf,
    saved: Mutex<Saved>,
    server: Box<dyn LicenseServer>,
    /// This computer's fingerprint as sent to the service.
    machine: String,
    public_key: Option<[u8; 32]>,
}

/// The app's major version, never below 1 (0.x builds count as version 1).
fn app_major() -> u8 {
    env!("CARGO_PKG_VERSION").split('.').next().and_then(|m| m.parse().ok()).unwrap_or(1).max(1)
}

impl Licensing {
    pub fn load(path: PathBuf, now: i64) -> Self {
        Self::load_with(path, now, Box::new(HttpServer), crate::machine::os_machine_id(), embedded_public_key())
    }

    pub fn load_with(path: PathBuf, now: i64, server: Box<dyn LicenseServer>, os_machine: Option<String>, public_key: Option<[u8; 32]>) -> Self {
        let mut saved: Saved = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let mut dirty = false;
        if saved.first_run == 0 {
            saved.first_run = now;
            saved.last_seen = now;
            dirty = true;
        }
        // A key saved by a version without activation: start its grace period now.
        if saved.key.is_some() && saved.grace_start == 0 {
            saved.grace_start = now;
            dirty = true;
        }
        let raw = match os_machine {
            Some(id) => id,
            None => {
                if saved.fallback_machine.is_none() {
                    let mut b = [0u8; 16];
                    let _ = getrandom::fill(&mut b);
                    saved.fallback_machine = Some(b.iter().map(|x| format!("{x:02x}")).collect());
                    dirty = true;
                }
                saved.fallback_machine.clone().unwrap_or_default()
            }
        };
        if dirty {
            let _ = std::fs::write(&path, serde_json::to_string(&saved).unwrap_or_default());
        }
        Self { path, saved: Mutex::new(saved), server, machine: machine_fingerprint(&raw), public_key }
    }

    fn save(&self, s: &Saved) {
        let _ = std::fs::write(&self.path, serde_json::to_string(s).unwrap_or_default());
    }

    fn valid_license(&self, key: &Option<String>) -> Option<License> {
        verify_with(key.as_deref()?, &self.public_key?).ok().filter(|l| l.major >= app_major())
    }

    /// The saved token, if it is genuine and made for this key on this computer. Expiry is for the caller to judge.
    fn bound_token(&self, s: &Saved) -> Option<Activation> {
        let a = verify_activation(s.token.as_deref()?, &self.public_key?).ok()?;
        (a.machine == self.machine && Some(a.key_id.as_str()) == key_id(s.key.as_deref()?).ok().as_deref()).then_some(a)
    }

    /// A token the service just sent must be one this computer would accept.
    fn accepts(&self, key: &str, token: &str, now: i64) -> bool {
        let Some(pk) = self.public_key else { return false };
        verify_activation(token, &pk).is_ok_and(|a| a.machine == self.machine && Some(a.key_id) == key_id(key).ok() && now < a.expires_at)
    }

    fn status_locked(&self, s: &mut Saved, now: i64) -> Status {
        let effective = now.max(s.last_seen);
        if effective - s.last_seen >= 3600 {
            s.last_seen = effective;
            self.save(s);
        }
        if s.refused.is_none() {
            if let Some(l) = self.valid_license(&s.key) {
                let token = self.bound_token(s);
                if token.as_ref().is_some_and(|a| effective < a.expires_at) {
                    return Status::Licensed { email: l.email };
                }
                // Not confirmed (yet): the grace period counts from when the key was entered, or from when the token ran out.
                let start = token.map(|a| a.expires_at).unwrap_or(s.grace_start);
                let left = (start + GRACE_DAYS * DAY - effective + DAY - 1).div_euclid(DAY);
                if left > 0 {
                    return Status::Grace { email: l.email, days_left: left.min(GRACE_DAYS) };
                }
            }
        }
        let left = (s.first_run + TRIAL_DAYS * DAY - effective + DAY - 1).div_euclid(DAY);
        if left > 0 { Status::Trial { days_left: left.min(TRIAL_DAYS) } } else { Status::Expired }
    }

    pub fn status(&self, now: i64) -> Status {
        let mut s = self.saved.lock().unwrap();
        self.status_locked(&mut s, now)
    }

    /// True when starting a backup is allowed.
    pub fn may_back_up(&self, now: i64) -> bool {
        !matches!(self.status(now), Status::Expired)
    }

    fn unreachable_text() -> String {
        "Backer couldn't reach the license server. Check your internet connection and try again.".into()
    }

    pub fn set_key(&self, key: &str, now: i64) -> Result<Status, String> {
        let pk = self.public_key.ok_or_else(|| LicenseError::NotConfigured.to_string())?;
        let lic = verify_with(key, &pk).map_err(|e: LicenseError| e.to_string())?;
        if lic.major < app_major() {
            return Err(format!("That key covers Backer {}.x. This is version {}.x, which needs an upgrade.", lic.major, app_major()));
        }
        let key = key.trim().to_string();
        let answer = self.server.activate(&key, &self.machine);
        let token = match answer {
            Ok(t) if self.accepts(&key, &t, now) => Some(t),
            Ok(_) => None, // an answer we can't use is treated like no answer
            Err(ServerError::Refused(m)) => return Err(m),
            Err(ServerError::Unreachable(_)) => None,
        };
        let mut s = self.saved.lock().unwrap();
        let same = s.key.as_deref().and_then(|k| key_id(k).ok()) == key_id(&key).ok();
        if !same || s.grace_start == 0 {
            // Re-entering the same key does not restart its grace period.
            s.grace_start = now;
        }
        if !same {
            s.token = None;
        }
        if token.is_some() {
            s.token = token;
        }
        s.key = Some(key);
        s.refused = None;
        self.save(&s);
        Ok(self.status_locked(&mut s, now))
    }

    /// Asks the service for a fresh activation token (when none is held or the one held is due for renewal).
    /// Quiet about failures: it runs in the background and the next try is soon.
    pub fn refresh(&self, now: i64) {
        let key = {
            let s = self.saved.lock().unwrap();
            let Some(key) = s.key.clone() else { return };
            if self.valid_license(&s.key).is_none() {
                return;
            }
            let due = self.bound_token(&s).is_none_or(|a| now >= a.refresh_after);
            if !due {
                return;
            }
            key
        };
        let _ = self.try_activate(&key, now);
    }

    fn try_activate(&self, key: &str, now: i64) -> Result<(), ServerError> {
        match self.server.activate(key, &self.machine) {
            Ok(t) if self.accepts(key, &t, now) => {
                let mut s = self.saved.lock().unwrap();
                if s.key.as_deref() == Some(key) {
                    s.token = Some(t);
                    s.refused = None;
                    self.save(&s);
                }
                Ok(())
            }
            Ok(_) => Err(ServerError::Unreachable("license server sent a token this computer can't use".into())),
            Err(ServerError::Refused(m)) => {
                let mut s = self.saved.lock().unwrap();
                if s.key.as_deref() == Some(key) {
                    s.token = None;
                    s.refused = Some(m.clone());
                    self.save(&s);
                }
                Err(ServerError::Refused(m))
            }
            Err(e) => Err(e),
        }
    }

    /// The "Try again" button: ask now, and say why if it did not work.
    pub fn retry(&self, now: i64) -> Result<Status, String> {
        let key = self.saved.lock().unwrap().key.clone().ok_or("There's no license key on this computer.")?;
        match self.try_activate(&key, now) {
            Ok(()) => Ok(self.status(now)),
            Err(ServerError::Refused(m)) => Err(m),
            Err(ServerError::Unreachable(_)) => Err(Self::unreachable_text()),
        }
    }

    /// Frees this computer's place on the key so it can be used on another computer, and removes the key from this one.
    /// Needs the service, because the place is counted there.
    pub fn deactivate(&self, now: i64) -> Result<Status, String> {
        let key = self.saved.lock().unwrap().key.clone().ok_or("There's no license key on this computer.")?;
        match self.server.deactivate(&key, &self.machine) {
            Ok(()) | Err(ServerError::Refused(_)) => {}
            Err(ServerError::Unreachable(_)) => return Err(Self::unreachable_text()),
        }
        let mut s = self.saved.lock().unwrap();
        s.key = None;
        s.token = None;
        s.refused = None;
        s.grace_start = 0;
        self.save(&s);
        Ok(self.status_locked(&mut s, now))
    }

    pub fn json(&self, now: i64) -> serde_json::Value {
        use serde_json::json;
        let purchase_url = backer_license::PURCHASE_URL.trim();
        let mut s = self.saved.lock().unwrap();
        let status = self.status_locked(&mut s, now);
        // Why a key that is on this computer is not being honoured, if it isn't.
        let problem = match (&status, self.valid_license(&s.key)) {
            (Status::Trial { .. } | Status::Expired, Some(_)) => Some(s.refused.clone().unwrap_or_else(|| format!("Backer couldn't confirm your key with the license server within {GRACE_DAYS} days. Connect to the internet and try again."))),
            _ => None,
        };
        let mut v = match status {
            Status::Licensed { email } => json!({"state": "licensed", "email": email, "purchase_url": purchase_url}),
            Status::Grace { email, days_left } => json!({"state": "grace", "email": email, "days_left": days_left, "purchase_url": purchase_url}),
            Status::Trial { days_left } => json!({"state": "trial", "days_left": days_left, "purchase_url": purchase_url}),
            Status::Expired => json!({"state": "expired", "purchase_url": purchase_url}),
        };
        if let Some(p) = problem {
            v["problem"] = json!(p);
        }
        v
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

    // ---- Online activation, against a fake license service that counts computers like the real one. ----

    use backer_license::activation::issue::sign_activation;
    use backer_license::issue::{issue_key, new_signing_key};
    use std::collections::HashSet;
    use std::sync::{atomic::{AtomicBool, AtomicI64, Ordering}, Arc};

    struct Fake {
        sk: backer_license::issue::SigningKey,
        now: Arc<AtomicI64>,
        down: Arc<AtomicBool>,
        seats: Mutex<HashSet<String>>, // computers on the key; the fake only knows one key
        revoked: AtomicBool,
        calls: Mutex<Vec<&'static str>>,
    }
    struct Svc(Arc<Fake>);
    impl LicenseServer for Svc {
        fn activate(&self, key: &str, machine: &str) -> Result<String, ServerError> {
            let f = &self.0;
            f.calls.lock().unwrap().push("activate");
            if f.down.load(Ordering::SeqCst) { return Err(ServerError::Unreachable("down".into())); }
            if f.revoked.load(Ordering::SeqCst) { return Err(ServerError::Refused("This license key has been deactivated.".into())); }
            let mut seats = f.seats.lock().unwrap();
            if !seats.contains(machine) && seats.len() >= 2 { return Err(ServerError::Refused("This key is already active on 2 computers.".into())); }
            seats.insert(machine.to_string());
            Ok(sign_activation(&f.sk, &backer_license::key_id(key).unwrap(), machine, f.now.load(Ordering::SeqCst), 7, 90))
        }
        fn deactivate(&self, _key: &str, machine: &str) -> Result<(), ServerError> {
            let f = &self.0;
            f.calls.lock().unwrap().push("deactivate");
            if f.down.load(Ordering::SeqCst) { return Err(ServerError::Unreachable("down".into())); }
            f.seats.lock().unwrap().remove(machine);
            Ok(())
        }
    }

    const T0: i64 = 1_800_000_000;

    struct World { fake: Arc<Fake>, key: String, pk: [u8; 32] }
    fn world() -> World {
        let sk = new_signing_key();
        let pk = sk.verifying_key().to_bytes();
        let key = issue_key(&sk, "buyer@example.com", 1, 1).unwrap();
        let fake = Arc::new(Fake { sk, now: Arc::new(AtomicI64::new(T0)), down: Arc::new(AtomicBool::new(false)), seats: Mutex::new(HashSet::new()), revoked: AtomicBool::new(false), calls: Mutex::new(vec![]) });
        World { fake, key, pk }
    }
    /// A computer: its own data folder and machine id, sharing the world's service.
    fn computer(w: &World, dir: &std::path::Path, id: &str, now: i64) -> Licensing {
        w.fake.now.store(now, Ordering::SeqCst);
        Licensing::load_with(dir.join("license.json"), now, Box::new(Svc(w.fake.clone())), Some(id.to_string()), Some(w.pk))
    }

    #[test]
    fn two_computers_activate_and_a_third_is_refused_until_one_deactivates() {
        let w = world();
        let (d1, d2, d3) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b, c) = (computer(&w, d1.path(), "machine-aaaaaaaa", T0), computer(&w, d2.path(), "machine-bbbbbbbb", T0), computer(&w, d3.path(), "machine-cccccccc", T0));
        assert_eq!(a.set_key(&w.key, T0).unwrap(), Status::Licensed { email: "buyer@example.com".into() });
        assert!(matches!(b.set_key(&w.key, T0), Ok(Status::Licensed { .. })));
        let e = c.set_key(&w.key, T0).unwrap_err();
        assert!(e.contains("2 computers"), "{e}");
        assert!(matches!(c.status(T0), Status::Trial { .. }), "a refused key is not saved");
        // Freeing a place on one computer lets the third one in.
        assert!(matches!(a.deactivate(T0).unwrap(), Status::Trial { .. }));
        assert!(matches!(c.set_key(&w.key, T0), Ok(Status::Licensed { .. })));
    }

    #[test]
    fn a_licensed_computer_keeps_working_offline_and_refreshes_its_token_in_the_background() {
        let w = world();
        let d = tempfile::tempdir().unwrap();
        let l = computer(&w, d.path(), "machine-aaaaaaaa", T0);
        l.set_key(&w.key, T0).unwrap();
        w.fake.down.store(true, Ordering::SeqCst);
        let day = |n: i64| T0 + n * DAY;
        // Service down for 80 days: still licensed, and no pointless calls before the refresh is due.
        assert!(matches!(l.status(day(80)), Status::Licensed { .. }));
        let before = w.fake.calls.lock().unwrap().len();
        l.refresh(day(3));
        assert_eq!(w.fake.calls.lock().unwrap().len(), before, "not due before 7 days");
        // Due, but the service is down: nothing breaks, the old token still works.
        l.refresh(day(8));
        assert!(matches!(l.status(day(8)), Status::Licensed { .. }));
        // Back up: a new token is fetched and the license outlives the old token's 90 days.
        w.fake.down.store(false, Ordering::SeqCst);
        w.fake.now.store(day(85), Ordering::SeqCst);
        l.refresh(day(85));
        assert!(matches!(l.status(day(150)), Status::Licensed { .. }));
        assert!(l.may_back_up(day(150)));
    }

    #[test]
    fn ninety_days_without_the_service_ends_the_token_but_a_grace_period_still_keeps_backups_going() {
        let w = world();
        let d = tempfile::tempdir().unwrap();
        let l = computer(&w, d.path(), "machine-aaaaaaaa", T0);
        l.set_key(&w.key, T0).unwrap();
        w.fake.down.store(true, Ordering::SeqCst);
        let day = |n: i64| T0 + n * DAY;
        assert!(matches!(l.status(day(89)), Status::Licensed { .. }));
        assert!(matches!(l.status(day(91)), Status::Grace { days_left: 13, .. }));
        assert!(l.may_back_up(day(103)));
        assert!(!l.may_back_up(day(105)), "after 90 + 14 days offline backups pause until the key is confirmed");
        let j = l.json(day(105));
        assert_eq!(j["state"], "expired");
        assert!(j["problem"].as_str().unwrap().contains("couldn't confirm"));
        // Once the service is back, one refresh puts it right.
        w.fake.down.store(false, Ordering::SeqCst);
        w.fake.now.store(day(105), Ordering::SeqCst);
        assert!(matches!(l.retry(day(105)), Ok(Status::Licensed { .. })));
    }

    #[test]
    fn a_key_entered_offline_gets_a_14_day_grace_and_activates_when_the_service_returns() {
        let w = world();
        let d = tempfile::tempdir().unwrap();
        let l = computer(&w, d.path(), "machine-aaaaaaaa", T0);
        w.fake.down.store(true, Ordering::SeqCst);
        assert!(matches!(l.set_key(&w.key, T0), Ok(Status::Grace { days_left: 14, .. })));
        assert!(matches!(l.status(T0 + 10 * DAY), Status::Grace { days_left: 4, .. }));
        // Typing the same key again does not restart the grace period.
        l.set_key(&w.key, T0 + 10 * DAY).unwrap();
        assert!(matches!(l.status(T0 + 10 * DAY), Status::Grace { days_left: 4, .. }));
        w.fake.down.store(false, Ordering::SeqCst);
        w.fake.now.store(T0 + 11 * DAY, Ordering::SeqCst);
        l.refresh(T0 + 11 * DAY);
        assert!(matches!(l.status(T0 + 11 * DAY), Status::Licensed { .. }));
    }

    #[test]
    fn a_key_saved_by_an_older_version_without_activation_gets_a_grace_period_then_activates() {
        let w = world();
        let d = tempfile::tempdir().unwrap();
        // What 0.7.0 wrote: first_run, last_seen and the key, nothing else.
        std::fs::write(d.path().join("license.json"), format!(r#"{{"first_run":1,"last_seen":1,"key":"{}"}}"#, w.key)).unwrap();
        let l = computer(&w, d.path(), "machine-aaaaaaaa", T0);
        assert!(matches!(l.status(T0), Status::Grace { days_left: 14, .. }));
        assert!(l.may_back_up(T0), "an existing customer is never cut off by the upgrade");
        l.refresh(T0);
        assert!(matches!(l.status(T0), Status::Licensed { .. }));
    }

    #[test]
    fn a_shared_key_is_refused_by_the_service_and_stops_a_backup_even_if_it_was_first_accepted_offline() {
        let w = world();
        let (d1, d2, d3) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        computer(&w, d1.path(), "machine-aaaaaaaa", T0).set_key(&w.key, T0).unwrap();
        computer(&w, d2.path(), "machine-bbbbbbbb", T0).set_key(&w.key, T0).unwrap();
        // A stranger types the key in while offline, then goes online.
        w.fake.down.store(true, Ordering::SeqCst);
        let c = computer(&w, d3.path(), "machine-cccccccc", T0);
        assert!(matches!(c.set_key(&w.key, T0), Ok(Status::Grace { .. })));
        w.fake.down.store(false, Ordering::SeqCst);
        c.refresh(T0 + DAY);
        let j = c.json(T0 + DAY);
        assert_eq!(j["state"], "trial", "the key no longer counts here");
        assert!(j["problem"].as_str().unwrap().contains("2 computers"));
        assert!(c.retry(T0 + DAY).unwrap_err().contains("2 computers"));
    }

    #[test]
    fn revoking_a_key_takes_effect_at_the_next_refresh() {
        let w = world();
        let d = tempfile::tempdir().unwrap();
        let l = computer(&w, d.path(), "machine-aaaaaaaa", T0);
        l.set_key(&w.key, T0).unwrap();
        w.fake.revoked.store(true, Ordering::SeqCst);
        assert!(matches!(l.status(T0 + 3 * DAY), Status::Licensed { .. }));
        l.refresh(T0 + 8 * DAY);
        assert!(matches!(l.status(T0 + 8 * DAY), Status::Trial { .. } | Status::Expired));
    }

    #[test]
    fn a_token_from_another_computer_or_key_or_signer_is_worthless_and_deactivate_needs_the_service() {
        let w = world();
        let d = tempfile::tempdir().unwrap();
        let l = computer(&w, d.path(), "machine-aaaaaaaa", T0);
        l.set_key(&w.key, T0).unwrap();
        // Copy this computer's license.json to another computer: the token names a different machine.
        let d2 = tempfile::tempdir().unwrap();
        std::fs::copy(d.path().join("license.json"), d2.path().join("license.json")).unwrap();
        w.fake.down.store(true, Ordering::SeqCst);
        let copy = computer(&w, d2.path(), "machine-bbbbbbbb", T0);
        assert!(matches!(copy.status(T0), Status::Grace { .. }), "a copied activation is not an activation");
        // Deactivating with no connection says so and keeps the key.
        assert!(l.deactivate(T0).unwrap_err().contains("couldn't reach"));
        assert!(matches!(l.status(T0), Status::Licensed { .. }));
    }
}
