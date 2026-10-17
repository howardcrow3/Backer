//! Stdio bridge for MCP clients. AI assistants launch MCP servers as programs that speak JSON-RPC
//! lines over stdin/stdout; the agent speaks the same lines over its private local connection.
//! This copies lines between the two. It adds no tools and holds no authority of its own: the agent
//! decides, per message, whether the program that launched the bridge has been approved.
use crate::ipc::{self, Duplex};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type Out = Arc<Mutex<Box<dyn Write + Send>>>;

fn say(out: &Out, v: &Value) {
    if let Ok(mut o) = out.lock() {
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
}

fn request_id(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line).ok().and_then(|v| v.get("id").cloned())
}

struct Link {
    w: Box<dyn Duplex>,
    alive: Arc<AtomicBool>,
}

fn open(dir: &Path, out: &Out) -> Option<Link> {
    let s = ipc::connect(dir, "mcp").ok()?;
    let r = s.dup().ok()?;
    let alive = Arc::new(AtomicBool::new(true));
    let flag = alive.clone();
    let out = out.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines().map_while(Result::ok) {
            if let Ok(mut o) = out.lock() {
                let _ = writeln!(o, "{line}");
                let _ = o.flush();
            }
        }
        flag.store(false, Ordering::SeqCst);
    });
    Some(Link { w: s, alive })
}

/// Runs until `input` ends. The agent is reconnected on demand, so restarting it does not require
/// restarting the assistant.
pub fn run(dir: &Path, input: impl BufRead, output: Box<dyn Write + Send>) -> std::io::Result<()> {
    let out: Out = Arc::new(Mutex::new(output));
    let mut link: Option<Link> = None;
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let mut sent = false;
        for _ in 0..2 {
            if link.as_ref().is_some_and(|l| !l.alive.load(Ordering::SeqCst)) {
                link = None;
            }
            if link.is_none() {
                link = open(dir, &out);
            }
            let ok = link.as_mut().is_some_and(|l| writeln!(l.w, "{line}").is_ok());
            if ok {
                sent = true;
                break;
            }
            link = None;
        }
        if !sent {
            if let Some(id) = request_id(&line) {
                say(&out, &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32002,"message":"Backer is not running. Open the Backer app and try again."}}));
            }
        }
    }
    Ok(())
}
