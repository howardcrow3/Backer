//! Which assistants may use the MCP socket. Approval is tied to the program's code-signing identity,
//! so a different program (or a copy) cannot reuse an approval.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Client {
    pub name: String,
    pub approved: bool,
    pub first_seen: i64,
    pub last_used: Option<i64>,
}

pub struct Approvals {
    path: PathBuf,
    inner: Mutex<BTreeMap<String, Client>>,
}

impl Approvals {
    pub fn load(path: PathBuf) -> Self {
        let m = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, inner: Mutex::new(m) }
    }
    fn save(&self, m: &BTreeMap<String, Client>) {
        if let Ok(b) = serde_json::to_vec_pretty(m) {
            let tmp = self.path.with_extension("tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
    /// True if approved. Unknown clients are recorded as pending so the app can ask the user.
    pub fn check(&self, identity: &str, name: &str, now: i64) -> bool {
        let mut m = self.inner.lock().unwrap();
        let ok = match m.get_mut(identity) {
            Some(c) => {
                if c.approved {
                    c.last_used = Some(now);
                }
                c.approved
            }
            None => {
                m.insert(identity.to_string(), Client { name: name.to_string(), approved: false, first_seen: now, last_used: None });
                false
            }
        };
        self.save(&m);
        ok
    }
    pub fn approve(&self, identity: &str) -> bool {
        let mut m = self.inner.lock().unwrap();
        let found = m.get_mut(identity).map(|c| c.approved = true).is_some();
        self.save(&m);
        found
    }
    pub fn revoke(&self, identity: &str) -> bool {
        let mut m = self.inner.lock().unwrap();
        let found = m.remove(identity).is_some();
        self.save(&m);
        found
    }
    pub fn list(&self) -> Vec<(String, Client)> {
        self.inner.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_then_approved_then_revoked_and_persisted() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.json");
        let a = Approvals::load(p.clone());
        assert!(!a.check("id1", "Claude Desktop", 1));
        assert!(!a.check("id1", "Claude Desktop", 2));
        assert!(a.approve("id1"));
        assert!(Approvals::load(p.clone()).check("id1", "x", 3));
        assert!(!a.check("id2", "other", 4)); // different identity never inherits approval
        assert!(a.revoke("id1"));
        assert!(!Approvals::load(p).check("id1", "x", 5));
    }
}
