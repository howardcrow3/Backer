//! End to end: real agent process, real `mcp-bridge` process speaking stdio, like an AI assistant would.
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_backer-agent");

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ui(dir: &std::path::Path, method: &str, params: Value) -> Value {
    let mut c = backer_agent::ipc::connect(dir, "ui").unwrap();
    writeln!(c, "{}", json!({"method": method, "params": params})).unwrap();
    let mut l = String::new();
    BufReader::new(c).read_line(&mut l).unwrap();
    serde_json::from_str(&l).unwrap()
}

#[test]
fn assistant_through_the_bridge_needs_approval_then_works() {
    let d = tempfile::tempdir().unwrap();
    // Bridge started before the agent: the assistant gets a clear error, not a hang.
    let mut early = Kill(Command::new(EXE).arg("mcp-bridge").env("BACKER_DATA", d.path()).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
    let mut ein = early.0.stdin.take().unwrap();
    writeln!(ein, "{}", json!({"jsonrpc":"2.0","id":7,"method":"tools/list"})).unwrap();
    let mut l = String::new();
    BufReader::new(early.0.stdout.as_mut().unwrap()).read_line(&mut l).unwrap();
    let v: Value = serde_json::from_str(&l).unwrap();
    assert_eq!(v["id"], 7);
    assert_eq!(v["error"]["code"], -32002);
    drop(ein);

    let _agent = Kill(Command::new(EXE).arg("run").env("BACKER_DATA", d.path()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let t = Instant::now();
    while backer_agent::ipc::connect(d.path(), "ui").is_err() || backer_agent::ipc::connect(d.path(), "mcp").is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "agent did not start");
        std::thread::sleep(Duration::from_millis(100));
    }

    let mut b = Kill(Command::new(EXE).arg("mcp-bridge").env("BACKER_DATA", d.path()).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
    let mut input = b.0.stdin.take().unwrap();
    let mut out = BufReader::new(b.0.stdout.take().unwrap());
    let mut rpc = |v: Value| -> Value {
        writeln!(input, "{v}").unwrap();
        let mut l = String::new();
        out.read_line(&mut l).unwrap();
        serde_json::from_str(&l).unwrap()
    };
    let list = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    assert_eq!(rpc(list.clone())["error"]["code"], -32001);

    let clients = ui(d.path(), "list_mcp_clients", json!({}));
    let c = &clients["result"].as_array().unwrap()[0];
    // On Linux the agent can see who launched the bridge: this test process, not the bridge itself.
    #[cfg(target_os = "linux")]
    {
        let me = std::env::current_exe().unwrap().file_stem().unwrap().to_string_lossy().to_string();
        assert_eq!(c["client"]["name"], me);
    }
    let ident = c["identity"].as_str().unwrap().to_string();
    assert_eq!(ui(d.path(), "approve_mcp_client", json!({"identity": ident}))["ok"], true);

    let r = rpc(list.clone());
    assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 10);
    let init = rpc(json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}));
    assert!(init["result"]["serverInfo"].is_object());

    ui(d.path(), "revoke_mcp_client", json!({"identity": ident}));
    assert_eq!(rpc(list)["error"]["code"], -32001);
}
