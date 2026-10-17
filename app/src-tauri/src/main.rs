// The tray app. All real work happens in the background agent; this talks to it on a private local connection.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::Command;
mod tray_dot;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WindowEvent};

trait Rw: Read + Write {}
impl<T: Read + Write> Rw for T {}

fn data_dir() -> PathBuf {
    if cfg!(windows) {
        return PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("Backer");
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/Backer")
}

/// macOS: Unix socket. Windows: 127.0.0.1 port and token read from the endpoint file in the user's profile.
fn connect() -> std::io::Result<Box<dyn Rw>> {
    #[cfg(unix)]
    {
        Ok(Box::new(std::os::unix::net::UnixStream::connect(data_dir().join("ui.sock"))?))
    }
    #[cfg(not(unix))]
    {
        let ep: serde_json::Value = serde_json::from_slice(&std::fs::read(data_dir().join("ui.endpoint"))?)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let port = ep["port"].as_u64().unwrap_or(0) as u16;
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
        writeln!(s, "{}", ep["token"].as_str().unwrap_or(""))?;
        Ok(Box::new(s))
    }
}

fn call(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut s = connect().map_err(|e| format!("The background agent is not running ({e})"))?;
    let req = serde_json::json!({ "method": method, "params": params });
    writeln!(s, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if v["ok"] == true { Ok(v["result"].clone()) } else { Err(v["error"].as_str().unwrap_or("failed").to_string()) }
}

#[tauri::command]
fn agent_call(method: String, params: serde_json::Value) -> Result<serde_json::Value, String> {
    call(&method, params)
}

/// The JSON an AI assistant needs in its MCP settings to launch the bridge.
#[tauri::command]
fn mcp_config() -> Result<String, String> {
    let exe = agent_exe().ok_or("cannot locate the agent")?;
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": { "backer": { "command": exe.display().to_string(), "args": ["mcp-bridge"] } }
    })).map_err(|e| e.to_string())?)
}

fn agent_exe() -> Option<PathBuf> {
    let name = if cfg!(windows) { "backer-agent.exe" } else { "backer-agent" };
    std::env::current_exe().ok()?.parent().map(|d| d.join(name))
}

/// Install and start the per-user background agent if it is not already answering.
fn ensure_agent() {
    if connect().is_ok() {
        return;
    }
    let Some(agent) = agent_exe() else { return };
    let _ = std::fs::create_dir_all(data_dir());
    #[cfg(target_os = "macos")]
    {
        let Ok(plist) = Command::new(&agent).arg("print-launch-agent").output() else { return };
        let home = std::env::var("HOME").unwrap_or_default();
        let path = PathBuf::from(&home).join("Library/LaunchAgents/com.backer.agent.plist");
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if std::fs::write(&path, plist.stdout).is_ok() {
            let uid = Command::new("/usr/bin/id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|_| "501".into());
            let _ = Command::new("/bin/launchctl").args(["bootout", &format!("gui/{uid}/com.backer.agent")]).status();
            let _ = Command::new("/bin/launchctl").args(["bootstrap", &format!("gui/{uid}"), path.to_str().unwrap_or("")]).status();
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        // Start at every login (per user, no admin rights needed), and start it now.
        let _ = Command::new("schtasks")
            .args(["/Create", "/TN", "BackerAgent", "/SC", "ONLOGON", "/RL", "LIMITED", "/F", "/TR", &format!("\"{}\" run", agent.display())])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        let _ = Command::new(&agent).arg("run").creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS).spawn();
        // Give it a moment to publish its connection file.
        for _ in 0..20 {
            if connect().is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = agent;
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn run_all() {
    if let Ok(jobs) = call("list_jobs", serde_json::json!({})) {
        for j in jobs.as_array().cloned().unwrap_or_default() {
            if let Some(id) = j["job"]["id"].as_i64() {
                let _ = call("run_job", serde_json::json!({ "job_id": id }));
            }
        }
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![agent_call, mcp_config])
        .setup(|app| {
            ensure_agent();
            let open = MenuItem::with_id(app, "open", "Open Backer", true, None::<&str>)?;
            let run = MenuItem::with_id(app, "run_all", "Run all now", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Backer", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open, &run, &sep, &quit])?;
            let base = app.default_window_icon().unwrap().clone();
            let (rgba, iw, ih) = (base.rgba().to_vec(), base.width(), base.height());
            TrayIconBuilder::with_id("main")
                .icon(base.clone())
                .icon_as_template(false) // a template icon would be drawn in one colour and hide the dot
                .menu(&menu)
                .on_menu_event(|app, ev| match ev.id.as_ref() {
                    "open" => show_main(app),
                    "run_all" => run_all(),
                    "quit" => app.exit(0), // the background agent keeps running
                    _ => {}
                })
                .build(app)?;
            // Refresh the dot from the agent's job list; the agent stays the single source of truth.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                let mut shown = None;
                loop {
                    let h = call("list_jobs", serde_json::json!({})).map(|j| tray_dot::health_of(&j)).unwrap_or(tray_dot::Health::None);
                    if shown != Some(h) {
                        if let Some(tray) = handle.tray_by_id("main") {
                            let img = tauri::image::Image::new_owned(tray_dot::with_dot(&rgba, iw, ih, h), iw, ih);
                            let _ = tray.set_icon(Some(img));
                            let _ = tray.set_tooltip(Some(h.tooltip()));
                            shown = Some(h);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_secs(15));
                }
            });
            Ok(())
        })
        // Closing the window hides it; backups continue.
        .on_window_event(|w, ev| {
            if let WindowEvent::CloseRequested { api, .. } = ev {
                api.prevent_close();
                let _ = w.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Backer");
}
