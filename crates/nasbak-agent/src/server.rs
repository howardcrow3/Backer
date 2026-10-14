//! Unix-socket servers. ui.sock: full API, same user only. mcp.sock: restricted, per-client approval.
use crate::{handle_ui, platform, Agent};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

fn same_user(s: &UnixStream) -> bool {
    platform::peer_uid(s) == Some(platform::my_uid())
}

pub fn serve_ui(agent: Arc<Agent>, path: &Path) -> std::io::Result<()> {
    let l = bind(path)?;
    for s in l.incoming().flatten() {
        if !same_user(&s) {
            continue;
        }
        let a = agent.clone();
        std::thread::spawn(move || {
            let mut w = match s.try_clone() { Ok(w) => w, Err(_) => return };
            for line in BufReader::new(s).lines().map_while(Result::ok) {
                let resp = match serde_json::from_str::<Value>(&line) {
                    Ok(req) => handle_ui_ext(&a, &req),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                };
                if writeln!(w, "{resp}").is_err() { break; }
            }
        });
    }
    Ok(())
}

/// UI-only calls for approving assistants; everything else goes to handle_ui.
pub fn handle_ui_ext(a: &Arc<Agent>, req: &Value) -> Value {
    let m = req.get("method").and_then(Value::as_str).unwrap_or("");
    let ident = req.pointer("/params/identity").and_then(Value::as_str).unwrap_or("");
    match m {
        "list_mcp_clients" => json!({"ok": true, "result": a.approvals.list().into_iter().map(|(id, c)| json!({"identity": id, "client": c})).collect::<Vec<_>>()}),
        "approve_mcp_client" => json!({"ok": a.approvals.approve(ident)}),
        "revoke_mcp_client" => json!({"ok": a.approvals.revoke(ident)}),
        _ => handle_ui(a, req),
    }
}

pub fn serve_mcp(agent: Arc<Agent>, path: &Path) -> std::io::Result<()> {
    let l = bind(path)?;
    for s in l.incoming().flatten() {
        if !same_user(&s) {
            continue;
        }
        let a = agent.clone();
        std::thread::spawn(move || handle_mcp_conn(a, s));
    }
    Ok(())
}

fn handle_mcp_conn(a: Arc<Agent>, s: UnixStream) {
    let (ident, name) = platform::peer_pid(&s).map(platform::client_identity).unwrap_or(("unknown".into(), "unknown".into()));
    let mut w = match s.try_clone() { Ok(w) => w, Err(_) => return };
    for line in BufReader::new(s).lines().map_while(Result::ok) {
        let now = crate::now_ts().as_second();
        // Checked on every message so a revoke takes effect immediately.
        let resp = if !a.approvals.check(&ident, &name, now) {
            let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned());
            Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"This program has not been approved. Open File Backup and allow it under Settings, MCP server."}}))
        } else {
            match serde_json::from_str::<Value>(&line) {
                Ok(req) => nasbak_mcp::handle(&crate::AgentControl(a.clone()), &req),
                Err(_) => Some(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}})),
            }
        };
        if let Some(r) = resp {
            if writeln!(w, "{r}").is_err() { break; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::tz::TimeZone;

    fn rpc(s: &mut UnixStream, v: Value) -> Value {
        writeln!(s, "{v}").unwrap();
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap()).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn mcp_socket_requires_approval_then_works_then_revoke_blocks() {
        let d = tempfile::tempdir().unwrap();
        let agent = Agent::new_sync(d.path().join("db"), TimeZone::UTC).unwrap();
        let sock = d.path().join("mcp.sock");
        { let (a, p) = (agent.clone(), sock.clone()); std::thread::spawn(move || { let _ = serve_mcp(a, &p); }); }
        for _ in 0..50 { if sock.exists() { break; } std::thread::sleep(std::time::Duration::from_millis(20)); }
        let mut c = UnixStream::connect(&sock).unwrap();
        let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        let r = rpc(&mut c, list.clone());
        assert_eq!(r["error"]["code"], -32001);
        let clients = agent.approvals.list();
        assert_eq!(clients.len(), 1);
        let ident = clients[0].0.clone();
        handle_ui_ext(&agent, &json!({"method":"approve_mcp_client","params":{"identity": ident}}));
        let r = rpc(&mut c, list.clone());
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 9);
        handle_ui_ext(&agent, &json!({"method":"revoke_mcp_client","params":{"identity": ident}}));
        assert_eq!(rpc(&mut c, list)["error"]["code"], -32001);
    }

    #[test]
    fn ui_socket_is_0600() {
        let d = tempfile::tempdir().unwrap();
        let agent = Agent::new_sync(d.path().join("db"), TimeZone::UTC).unwrap();
        let sock = d.path().join("ui.sock");
        { let (a, p) = (agent, sock.clone()); std::thread::spawn(move || { let _ = serve_ui(a, &p); }); }
        for _ in 0..50 { if sock.exists() { break; } std::thread::sleep(std::time::Duration::from_millis(20)); }
        assert_eq!(std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777, 0o600);
        let mut c = UnixStream::connect(&sock).unwrap();
        assert_eq!(rpc(&mut c, json!({"method":"list_jobs"}))["ok"], true);
    }
}
