// On Windows this is a background program: no console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{Context, Result};
use jiff::tz::TimeZone;
use backer_agent::{ipc::Listener, platform, server, svclog::SvcLog, Agent};
use std::path::PathBuf;
use std::time::Duration;

fn data_dir() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        return PathBuf::from(base).join("Backer");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    if cfg!(target_os = "macos") {
        PathBuf::from(format!("{home}/Library/Application Support/Backer"))
    } else {
        PathBuf::from(format!("{home}/.local/share/backer"))
    }
}

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let dir = std::env::var_os("BACKER_DATA").map(PathBuf::from).unwrap_or_else(data_dir);
    std::fs::create_dir_all(&dir)?;
    match cmd.as_str() {
        // Used by the app's installer step on macOS; users never type these.
        "print-launch-agent" => {
            let exe = std::env::current_exe()?;
            print!("{}", platform::launch_agent_plist("com.backer.agent", &exe, &dir));
            Ok(())
        }
        // Launched by an AI assistant as its MCP server: copies JSON-RPC lines between stdio and the agent.
        "mcp-bridge" => {
            let out: Box<dyn std::io::Write + Send> = Box::new(std::io::stdout());
            backer_agent::bridge::run(&dir, std::io::stdin().lock(), out)?;
            Ok(())
        }
        "run" => {
            let log = std::sync::Arc::new(SvcLog::new(&dir));
            log.line(&format!("service starting, version {}, data folder {}", env!("CARGO_PKG_VERSION"), dir.display()));
            let agent = Agent::new(dir.join("state.sqlite"), TimeZone::system()).map_err(|e| {
                log.line(&format!("could not open state: {e:#}"));
                e
            }).context("opening state")?;
            let ui = Listener::bind(&dir, "ui").map_err(|e| {
                log.line(&format!("could not start the app listener: {e:#}"));
                e
            }).context("ui listener")?;
            let mcp = Listener::bind(&dir, "mcp").map_err(|e| {
                log.line(&format!("could not start the assistant listener: {e:#}"));
                e
            }).context("mcp listener")?;
            log.line("listeners ready");
            let a1 = agent.clone();
            let l1 = log.clone();
            std::thread::spawn(move || { if let Err(e) = server::serve_ui(a1, ui) { l1.line(&format!("app listener stopped: {e}")); } });
            let a2 = agent.clone();
            let l2 = log.clone();
            std::thread::spawn(move || { if let Err(e) = server::serve_mcp(a2, mcp) { l2.line(&format!("assistant listener stopped: {e}")); } });
            backer_agent::service::spawn_license_refresh(&agent);
            backer_agent::service::run_scheduler(&agent, &log);
            // Give cancelled runs a moment to finish their current file, then leave. The next start rebinds the sockets.
            std::thread::sleep(Duration::from_millis(800));
            log.line("service stopped");
            Ok(())
        }
        other => anyhow::bail!("unknown command {other}"),
    }
}
