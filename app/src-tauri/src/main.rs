// The menu-bar app. All real work happens in the background agent; this talks to it over ui.sock.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WindowEvent};

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FileBackup")
}

fn call(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut s = UnixStream::connect(data_dir().join("ui.sock")).map_err(|e| format!("The background agent is not running ({e})"))?;
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

/// Install and start the per-user background agent (LaunchAgent) if it is not already answering.
fn ensure_agent() {
    if UnixStream::connect(data_dir().join("ui.sock")).is_ok() {
        return;
    }
    let Ok(me) = std::env::current_exe() else { return };
    let Some(agent) = me.parent().map(|d| d.join("nasbak-agent")) else { return };
    let Ok(plist) = Command::new(&agent).arg("print-launch-agent").output() else { return };
    let home = std::env::var("HOME").unwrap_or_default();
    let path = PathBuf::from(&home).join("Library/LaunchAgents/com.filebackup.agent.plist");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::create_dir_all(data_dir());
    if std::fs::write(&path, plist.stdout).is_ok() {
        let uid = unsafe_uid();
        let _ = Command::new("/bin/launchctl").args(["bootout", &format!("gui/{uid}/com.filebackup.agent")]).status();
        let _ = Command::new("/bin/launchctl").args(["bootstrap", &format!("gui/{uid}"), path.to_str().unwrap_or("")]).status();
    }
}

fn unsafe_uid() -> String {
    Command::new("/usr/bin/id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|_| "501".into())
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
        .invoke_handler(tauri::generate_handler![agent_call])
        .setup(|app| {
            ensure_agent();
            let open = MenuItem::with_id(app, "open", "Open File Backup", true, None::<&str>)?;
            let run = MenuItem::with_id(app, "run_all", "Run all now", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit File Backup", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open, &run, &sep, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .on_menu_event(|app, ev| match ev.id.as_ref() {
                    "open" => show_main(app),
                    "run_all" => run_all(),
                    "quit" => app.exit(0), // the background agent keeps running
                    _ => {}
                })
                .build(app)?;
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
        .expect("error while running File Backup");
}
