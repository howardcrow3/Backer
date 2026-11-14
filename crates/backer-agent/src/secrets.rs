//! NAS passwords and SFTP keys in the OS credential store (the macOS Keychain, the Windows Credential
//! Manager). Never in the job database, the config, the manifest or logs (spec section 5).
use backer_connector::Secret;
use std::collections::HashMap;
use std::sync::Mutex;

/// Keychain service name; one item per NAS account (see `Destination::credential_key`).
pub const SERVICE: &str = "Backer NAS";

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Option<Secret>;
    fn set(&self, key: &str, secret: &Secret) -> Result<(), String>;
    fn delete(&self, key: &str) -> Result<(), String>;
}

fn encode(s: &Secret) -> String {
    match s {
        Secret::Password(p) => serde_json::json!({ "password": p }),
        Secret::PrivateKey { pem, passphrase } => serde_json::json!({ "private_key": pem, "passphrase": passphrase }),
    }
    .to_string()
}

fn decode(s: &str) -> Option<Secret> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    if let Some(p) = v.get("password").and_then(|x| x.as_str()) {
        return Some(Secret::Password(p.to_string()));
    }
    let pem = v.get("private_key")?.as_str()?.to_string();
    let passphrase = v.get("passphrase").and_then(|x| x.as_str()).map(String::from);
    Some(Secret::PrivateKey { pem, passphrase })
}

/// The OS credential store.
pub struct Keychain;

impl SecretStore for Keychain {
    fn get(&self, key: &str) -> Option<Secret> {
        keyring::Entry::new(SERVICE, key).ok()?.get_password().ok().and_then(|s| decode(&s))
    }
    fn set(&self, key: &str, secret: &Secret) -> Result<(), String> {
        keyring::Entry::new(SERVICE, key).and_then(|e| e.set_password(&encode(secret))).map_err(|e| format!("could not save to the credential store: {e}"))
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        match keyring::Entry::new(SERVICE, key).and_then(|e| e.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// The store the agent uses: the OS credential store on macOS and Windows. Linux is only a development
/// platform for Backer and has no dependable store here, so secrets live in memory for that session.
pub fn os_store() -> Box<dyn SecretStore> {
    if cfg!(any(target_os = "macos", windows)) {
        Box::new(Keychain)
    } else {
        Box::new(MemorySecrets::default())
    }
}

/// For tests (and Linux development builds).
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, key: &str) -> Option<Secret> {
        self.0.lock().unwrap().get(key).and_then(|s| decode(s))
    }
    fn set(&self, key: &str, secret: &Secret) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.into(), encode(secret));
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip() {
        let m = MemorySecrets::default();
        m.set("sftp://u@nas:22", &Secret::Password("p w\"x".into())).unwrap();
        assert!(matches!(m.get("sftp://u@nas:22"), Some(Secret::Password(p)) if p == "p w\"x"));
        m.set("k", &Secret::PrivateKey { pem: "-----BEGIN".into(), passphrase: None }).unwrap();
        assert!(matches!(m.get("k"), Some(Secret::PrivateKey { passphrase: None, .. })));
        m.delete("k").unwrap();
        assert!(m.get("k").is_none());
    }
}
