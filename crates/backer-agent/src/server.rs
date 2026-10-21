//! Servers. ui: full API, same user only. mcp: restricted, per-client approval.
use crate::ipc::{Conn, Listener};
use crate::{handle_ui, platform, Agent};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

fn serve(agent: Arc<Agent>, l: Listener, handler: fn(Arc<Agent>, Conn)) -> std::io::Result<()> {
    loop {
        match l.accept()? {
            Some(c) => {
                let a = agent.clone();
                std::thread::spawn(move || handler(a, c));
            }
            None => continue,
        }
    }
}

pub fn serve_ui(agent: Arc<Agent>, l: Listener) -> std::io::Result<()> {
    serve(agent, l, ui_conn)
}

fn ui_conn(a: Arc<Agent>, mut c: Conn) {
    if !c.authenticate() {
        return;
    }
    let mut w = match c.stream.dup() { Ok(w) => w, Err(_) => return };
    for line in BufReader::new(c.stream).lines().map_while(Result::ok) {
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle_ui_ext(&a, &req),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
        if writeln!(w, "{resp}").is_err() { break; }
    }
}

/// UI-only calls for approving assistants; everything else goes to handle_ui.
pub fn handle_ui_ext(a: &Arc<Agent>, req: &Value) -> Value {
    let m = req.get("method").and_then(Value::as_str).unwrap_or("");
    let ident = req.pointer("/params/identity").and_then(Value::as_str).unwrap_or("");
    match m {
        "list_mcp_clients" => json!({"ok": true, "result": a.approvals.list().into_iter().map(|(id, c)| json!({"identity": id, "client": c})).collect::<Vec<_>>()}),
        "approve_mcp_client" => json!({"ok": a.approvals.approve(ident)}),
        "revoke_mcp_client" => json!({"ok": a.approvals.revoke(ident)}),
        // App-only (never offered to assistants): the tray's "Quit Backer and stop backups".
        "quit_agent" => {
            a.request_quit();
            json!({"ok": true})
        }
        _ => handle_ui(a, req),
    }
}

pub fn serve_mcp(agent: Arc<Agent>, l: Listener) -> std::io::Result<()> {
    serve(agent, l, mcp_conn)
}

fn mcp_conn(a: Arc<Agent>, mut c: Conn) {
    if !c.authenticate() {
        return;
    }
    let (ident, name) = platform::client_identity(c.peer_pid);
    let mut w = match c.stream.dup() { Ok(w) => w, Err(_) => return };
    for line in BufReader::new(c.stream).lines().map_while(Result::ok) {
        let now = crate::now_ts().as_second();
        // Checked on every message so a revoke takes effect immediately.
        let resp = if !a.approvals.check(&ident, &name, now) {
            let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned());
            Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"This program has not been approved. Open Backer and allow it under Settings, MCP server."}}))
        } else {
            match serde_json::from_str::<Value>(&line) {
                Ok(req) => backer_mcp::handle(&crate::AgentControl(a.clone()), &req),
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
    use crate::ipc;
    use jiff::tz::TimeZone;

    fn rpc(s: &mut Box<dyn ipc::Duplex>, v: Value) -> Value {
        writeln!(s, "{v}").unwrap();
        let mut line = String::new();
        BufReader::new(s.dup().unwrap()).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn mcp_flow(make: impl Fn(&std::path::Path) -> Listener, connect: impl Fn(&std::path::Path) -> Box<dyn ipc::Duplex>) {
        let d = tempfile::tempdir().unwrap();
        let agent = Agent::new_sync(d.path().join("db"), TimeZone::UTC).unwrap();
        let l = make(d.path());
        { let a = agent.clone(); std::thread::spawn(move || { let _ = serve_mcp(a, l); }); }
        let mut c = connect(d.path());
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

    #[cfg(unix)]
    #[test]
    fn mcp_unix_socket_requires_approval_then_works_then_revoke_blocks() {
        mcp_flow(|d| Listener::bind_unix(d, "mcp").unwrap(), |d| ipc::connect(d, "mcp").unwrap());
    }

    #[test]
    fn mcp_tcp_requires_approval_then_works_then_revoke_blocks() {
        mcp_flow(|d| Listener::bind_tcp(d, "mcp").unwrap(), |d| ipc::connect_tcp(d, "mcp").unwrap());
    }

    #[test]
    fn quit_agent_sets_the_flag_and_cancels_running_work() {
        let d = tempfile::tempdir().unwrap();
        let agent = Agent::new_sync(d.path().join("db"), TimeZone::UTC).unwrap();
        assert!(!agent.quit_requested());
        let r = handle_ui_ext(&agent, &json!({"method":"quit_agent"}));
        assert_eq!(r["ok"], true);
        assert!(agent.quit_requested());
    }

    #[test]
    fn ui_tcp_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let agent = Agent::new_sync(d.path().join("db"), TimeZone::UTC).unwrap();
        let l = Listener::bind_tcp(d.path(), "ui").unwrap();
        std::thread::spawn(move || { let _ = serve_ui(agent, l); });
        let mut c = ipc::connect_tcp(d.path(), "ui").unwrap();
        assert_eq!(rpc(&mut c, json!({"method":"list_jobs"}))["ok"], true);
    }
}
