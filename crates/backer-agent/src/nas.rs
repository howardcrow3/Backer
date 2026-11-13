//! App-only calls (ui.sock) for setting up a NAS connection in the job wizard: confirm the SFTP host
//! key, save the password to the OS credential store, test the connection, list shares or exports,
//! browse folders and create one. None of these is offered to assistants (MCP), and no call ever
//! returns a stored password.
use crate::Agent;
use backer_connector::{dest::Protocol, nfs, sftp, smb, Destination, Secret};
use backer_core::Error;
use serde_json::{json, Value};
use std::sync::Arc;

fn s(p: &Value, k: &str) -> Result<String, String> {
    p.get(k).and_then(Value::as_str).map(String::from).ok_or_else(|| format!("{k} required"))
}

fn dest(p: &Value) -> Result<Destination, String> {
    let d = Destination::parse(&s(p, "url")?).map_err(|e| e.to_string())?;
    if d.protocol() == Protocol::Mounted {
        return Err("use the folder picker for a mounted folder".into());
    }
    Ok(d)
}

/// What kind of failure, so the wizard can point at the right field (spec 5: unreachable, bad
/// credentials, share not found, no write permission; plus an unconfirmed or changed host key).
fn kind(e: &Error) -> &'static str {
    let m = e.to_string();
    match e {
        Error::Credentials(_) => "credentials",
        Error::HostKey(_) => "host_key",
        _ if m.contains("not found") || m.contains("missing") || m.contains("does not export") => "not_found",
        _ if m.contains("permission") => "permission",
        Error::DestinationNotReady(_) => "unreachable",
        _ => "other",
    }
}

fn failure(e: Error) -> Value {
    json!({"ok": false, "kind": kind(&e), "error": e.to_string()})
}

pub fn handle(agent: &Arc<Agent>, method: &str, p: &Value) -> Result<Value, String> {
    match method {
        // SFTP: the key the server presents now, and whether it matches the pinned one.
        "nas_probe" => {
            let d = dest(p)?;
            let (Destination::Sftp { host, port, .. }, Some(id)) = (&d, d.host_key_id()) else {
                return Ok(json!({"host_key": null}));
            };
            let pinned = agent.host_keys.get(&id);
            match sftp::probe_host_key(host, *port) {
                Ok(fp) => Ok(json!({"host_key": fp, "pinned": pinned, "trusted": pinned.as_deref() == Some(fp.as_str())})),
                Err(e) => Ok(failure(e)),
            }
        }
        // The user confirmed the fingerprint the app showed. Pin it only if the server still presents it.
        "nas_trust_host_key" => {
            let d = dest(p)?;
            let want = s(p, "fingerprint")?;
            let (Destination::Sftp { host, port, .. }, Some(id)) = (&d, d.host_key_id()) else {
                return Err("only SFTP connections have host keys".into());
            };
            let now = sftp::probe_host_key(host, *port).map_err(|e| e.to_string())?;
            if now != want {
                return Err(format!("{host} now presents {now}, not the key you confirmed. Nothing was trusted."));
            }
            agent.host_keys.pin(&id, &now).map_err(|e| e.to_string())?;
            Ok(json!({"ok": true}))
        }
        "nas_save_password" => {
            let d = dest(p)?;
            let key = d.credential_key().ok_or("this connection does not use a password")?;
            let secret = match p.get("private_key").and_then(Value::as_str) {
                Some(pem) if !pem.trim().is_empty() => {
                    Secret::PrivateKey { pem: pem.to_string(), passphrase: p.get("passphrase").and_then(Value::as_str).filter(|x| !x.is_empty()).map(String::from) }
                }
                _ => Secret::Password(s(p, "password")?),
            };
            agent.secrets.set(&key, &secret)?;
            Ok(json!({"ok": true}))
        }
        "nas_forget_password" => {
            let d = dest(p)?;
            if let Some(key) = d.credential_key() {
                agent.secrets.delete(&key)?;
            }
            Ok(json!({"ok": true}))
        }
        // Test connection: log in, find the folder, write and remove a small test file.
        "nas_test" => {
            let c = agent.open_dest(&s(p, "url")?);
            Ok(match c.check_writable() {
                Ok(()) => json!({"ok": true, "has_marker": c.check_ready().is_ok()}),
                Err(e) => failure(e),
            })
        }
        // Top level of a server: SMB shares or NFS exports. SFTP starts at "/".
        "nas_list_shares" => {
            let d = dest(p)?;
            let r = match &d {
                Destination::Smb { host, port, user, .. } => smb::list_shares(host, *port, user, &agent.access_for(&d)),
                Destination::Nfs { host, port, .. } => nfs::list_exports(host, *port),
                _ => return Ok(json!({"ok": true, "shares": []})),
            };
            Ok(match r {
                Ok(v) => json!({"ok": true, "shares": v}),
                Err(e) => failure(e),
            })
        }
        // Folders inside `path`: absolute within the share, export or SFTP server; defaults to the URL's folder.
        "nas_browse" => {
            let d = dest(p)?;
            let at = d.with_path(p.get("path").and_then(Value::as_str).or(d.base_path()).unwrap_or("/")).map_err(|e| e.to_string())?;
            let c = agent.open_dest(&at.to_url());
            Ok(match c.reachable().and_then(|_| c.list_dir("")) {
                Ok(v) => json!({"ok": true, "path": at.base_path(), "url": at.to_url(), "folders": v.into_iter().filter(|e| e.is_dir).map(|e| e.name).collect::<Vec<_>>()}),
                Err(e) => failure(e),
            })
        }
        "nas_create_folder" => {
            let d = dest(p)?;
            let parent = d.with_path(p.get("path").and_then(Value::as_str).or(d.base_path()).unwrap_or("/")).map_err(|e| e.to_string())?;
            let name = s(p, "name")?;
            if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
                return Err("folder names cannot contain / or \\".into());
            }
            agent.open_dest(&parent.to_url()).create_dir(&name).map_err(|e| e.to_string())?;
            let child = parent.with_path(&format!("{}/{name}", parent.base_path().unwrap_or("/"))).map_err(|e| e.to_string())?;
            Ok(json!({"ok": true, "url": child.to_url()}))
        }
        _ => Err(format!("unknown method {method}")),
    }
}

#[cfg(test)]
mod tests {
    use crate::{handle_ui, Agent};
    use jiff::tz::TimeZone;
    use serde_json::{json, Value};

    fn agent() -> (tempfile::TempDir, std::sync::Arc<Agent>) {
        let t = tempfile::tempdir().unwrap();
        let a = Agent::new_sync(t.path().join("db.sqlite"), TimeZone::UTC).unwrap();
        (t, a)
    }
    fn ui(a: &std::sync::Arc<Agent>, m: &str, p: Value) -> Value {
        handle_ui(a, &json!({"method": m, "params": p}))
    }

    #[test]
    fn passwords_go_to_the_secret_store_and_never_come_back() {
        let (_t, a) = agent();
        let url = "smb://howard@nas.local/Backups";
        assert_eq!(ui(&a, "nas_save_password", json!({"url": url, "password": "hunter2"}))["ok"], true);
        assert!(a.secrets.get("smb://howard@nas.local:445").is_some());
        // No call returns it, and the job JSON never holds it.
        for m in ["nas_probe", "nas_test", "nas_list_shares"] {
            let out = ui(&a, m, json!({"url": "smb://howard@127.0.0.1:1/Backups"})).to_string();
            assert!(!out.contains("hunter2"), "{m}: {out}");
        }
        ui(&a, "nas_forget_password", json!({"url": url}));
        assert!(a.secrets.get("smb://howard@nas.local:445").is_none());
    }

    #[test]
    fn rejects_mounted_paths_and_bad_urls() {
        let (_t, a) = agent();
        assert_eq!(ui(&a, "nas_test", json!({"url": "/Volumes/x"}))["ok"], true, "the call itself works");
        assert_eq!(ui(&a, "nas_test", json!({"url": "/Volumes/x"}))["result"]["ok"], false);
        assert_eq!(ui(&a, "nas_browse", json!({"url": "/Volumes/x"}))["ok"], false);
        assert_eq!(ui(&a, "nas_save_password", json!({"url": "sftp://u:p@nas/x", "password": "x"}))["ok"], false);
        assert_eq!(ui(&a, "nas_create_folder", json!({"url": "sftp://u@nas/x", "name": "../up"}))["ok"], false);
    }

    #[test]
    fn unreachable_server_is_reported_by_kind() {
        let (_t, a) = agent();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let url = format!("sftp://u@127.0.0.1:{port}/x");
        let r = ui(&a, "nas_probe", json!({"url": url}));
        assert_eq!(r["result"]["kind"], "unreachable", "{r}");
        let r = ui(&a, "nas_test", json!({"url": format!("smb://u@127.0.0.1:{port}/s")}));
        assert_eq!(r["result"]["kind"], "credentials", "no password saved yet: {r}");
        ui(&a, "nas_save_password", json!({"url": format!("smb://u@127.0.0.1:{port}/s"), "password": "p"}));
        let r = ui(&a, "nas_test", json!({"url": format!("smb://u@127.0.0.1:{port}/s")}));
        assert_eq!(r["result"]["kind"], "unreachable", "{r}");
    }

    #[test]
    fn save_job_rejects_malformed_nas_urls() {
        let (t, a) = agent();
        let src = t.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let job = json!({"id": 0, "name": "j", "folders": [{"source": src, "dest": "x"}], "dest_root": "sftp://nobody/x",
            "schedule": {"kind": "manual"}, "paused": false, "catch_up": true, "exclude_names": [], "max_file_bytes": null});
        let r = ui(&a, "save_job", json!({"job": job}));
        assert_eq!(r["ok"], false, "{r}");
    }
}
