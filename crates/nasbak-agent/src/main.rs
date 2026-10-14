use anyhow::{Context, Result};
use jiff::tz::TimeZone;
use nasbak_agent::{platform, server, Agent};
use std::path::PathBuf;
use std::time::Duration;

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let base = if cfg!(target_os = "macos") { format!("{home}/Library/Application Support/FileBackup") } else { format!("{home}/.local/share/filebackup") };
    PathBuf::from(base)
}

fn main() -> Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    let dir = std::env::var_os("NASBAK_DATA").map(PathBuf::from).unwrap_or_else(data_dir);
    std::fs::create_dir_all(&dir)?;
    match cmd.as_str() {
        // Used by the app's installer step; users never type these.
        "print-launch-agent" => {
            let exe = std::env::current_exe()?;
            print!("{}", platform::launch_agent_plist("com.filebackup.agent", &exe, &dir));
            Ok(())
        }
        "run" => {
            let tz = TimeZone::system();
            let agent = Agent::new(dir.join("state.sqlite"), tz).context("opening state")?;
            let (a1, p1) = (agent.clone(), dir.join("ui.sock"));
            std::thread::spawn(move || { if let Err(e) = server::serve_ui(a1, &p1) { eprintln!("ui socket: {e}"); } });
            let (a2, p2) = (agent.clone(), dir.join("mcp.sock"));
            std::thread::spawn(move || { if let Err(e) = server::serve_mcp(a2, &p2) { eprintln!("mcp socket: {e}"); } });
            // Wall-clock based, so a sleep/wake gap simply makes the next tick catch up.
            loop {
                if let Err(e) = agent.tick(nasbak_agent::now_ts()) { eprintln!("tick: {e}"); }
                std::thread::sleep(Duration::from_secs(30));
            }
        }
        other => anyhow::bail!("unknown command {other}"),
    }
}
