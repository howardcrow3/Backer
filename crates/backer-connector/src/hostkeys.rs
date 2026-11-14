//! SFTP host keys pinned on first connect (spec section 5). Stored as a small JSON file next to the
//! job database: `{"nas.local:22": "SHA256:..."}`. A changed key blocks runs until the user approves it.
use backer_core::{Error, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct HostKeys {
    path: PathBuf,
    lock: Mutex<()>,
}

impl HostKeys {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), lock: Mutex::new(()) }
    }
    fn load(&self) -> BTreeMap<String, String> {
        std::fs::read(&self.path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn get(&self, id: &str) -> Option<String> {
        let _g = self.lock.lock().unwrap();
        self.load().get(id).cloned()
    }
    /// Pin (or, after the user approved a change, replace) the key for `id`.
    pub fn pin(&self, id: &str, fingerprint: &str) -> Result<()> {
        if !fingerprint.starts_with("SHA256:") {
            return Err(Error::Invalid("not a SHA256 host key fingerprint".into()));
        }
        let _g = self.lock.lock().unwrap();
        let mut m = self.load();
        m.insert(id.to_string(), fingerprint.to_string());
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&m).map_err(|e| Error::Invalid(e.to_string()))?;
        std::fs::write(&tmp, body).map_err(|e| Error::Store(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| Error::Store(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pin_and_replace() {
        let d = tempfile::tempdir().unwrap();
        let k = HostKeys::new(d.path().join("host-keys.json"));
        assert_eq!(k.get("nas:22"), None);
        k.pin("nas:22", "SHA256:abc").unwrap();
        k.pin("other:22", "SHA256:zzz").unwrap();
        assert_eq!(k.get("nas:22").as_deref(), Some("SHA256:abc"));
        k.pin("nas:22", "SHA256:def").unwrap();
        let again = HostKeys::new(d.path().join("host-keys.json"));
        assert_eq!(again.get("nas:22").as_deref(), Some("SHA256:def"));
        assert_eq!(again.get("other:22").as_deref(), Some("SHA256:zzz"));
        assert!(k.pin("nas:22", "md5:xx").is_err());
    }
}
