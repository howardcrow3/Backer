// The tray app. All real work happens in the background agent; this talks to it on a private local connection.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(feature = "direct")]
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
#[cfg(feature = "direct")]
use std::process::Command;
#[cfg(feature = "direct")]
mod deep_link;
mod feedback;
mod tray_dot;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
#[cfg(feature = "direct")]
use tauri_plugin_updater::UpdaterExt;

#[cfg(feature = "direct")]
trait Rw: Read + Write {}
#[cfg(feature = "direct")]
impl<T: Read + Write> Rw for T {}

#[cfg(feature = "direct")]
fn data_dir() -> PathBuf {
    if cfg!(windows) {
        return PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("Backer");
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/Backer")
}

/// macOS: Unix socket. Windows: 127.0.0.1 port and token read from the endpoint file in the user's profile.
#[cfg(feature = "direct")]
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

/// Turns the agent's `{"ok":..,"result":..}` answer into a Result.
fn unwrap_reply(v: serde_json::Value) -> Result<serde_json::Value, String> {
    if v["ok"] == true { Ok(v["result"].clone()) } else { Err(v["error"].as_str().unwrap_or("failed").to_string()) }
}

#[cfg(feature = "direct")]
fn call(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut s = connect().map_err(|e| format!("The background agent is not running ({e})"))?;
    let req = serde_json::json!({ "method": method, "params": params });
    writeln!(s, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    unwrap_reply(v)
}

/// Mac App Store edition: the agent lives inside this process, so there is no socket, helper program or LaunchAgent.
#[cfg(feature = "mas")]
static AGENT: std::sync::OnceLock<std::sync::Arc<backer_agent::Agent>> = std::sync::OnceLock::new();

/// Folders the user chose, reopened at every start (the sandbox forgets them otherwise).
#[cfg(all(feature = "mas", target_os = "macos"))]
static BOOKMARKS: std::sync::OnceLock<backer_agent::bookmarks::Bookmarks> = std::sync::OnceLock::new();

#[cfg(feature = "mas")]
fn call(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let agent = AGENT.get().ok_or("Backer is still starting")?;
    let req = serde_json::json!({ "method": method, "params": params });
    unwrap_reply(backer_agent::server::handle_ui_ext(agent, &req))
}

#[tauri::command]
fn agent_call(method: String, params: serde_json::Value) -> Result<serde_json::Value, String> {
    call(&method, params)
}

/// The JSON an AI assistant needs in its MCP settings to launch the bridge.
/// Asks where to save a text file (the run log as CSV) and writes it. Async so the blocking dialog never runs on the main thread.
/// Returns false when the user cancels.
#[tauri::command]
async fn save_text_file(app: tauri::AppHandle, name: String, text: String) -> Result<bool, String> {
    let Some(picked) = app.dialog().file().set_file_name(name).add_filter("CSV", &["csv"]).blocking_save_file() else {
        return Ok(false);
    };
    let path = picked.into_path().map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Opens the store's checkout page in the default browser. The address is built into the app, never taken from the page.
#[tauri::command]
fn open_purchase_page() -> Result<(), String> {
    if cfg!(feature = "mas") {
        return Err("Backer was bought in the Mac App Store.".into());
    }
    let url = backer_license::PURCHASE_URL.trim();
    if !url.starts_with("https://") {
        return Err("The store address is not set up in this build.".into());
    }
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    r.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(feature = "mas")]
#[tauri::command]
fn mcp_config() -> Result<String, String> {
    Err("The MCP server is not part of the Mac App Store edition.".into())
}

#[cfg(feature = "direct")]
#[tauri::command]
fn mcp_config() -> Result<String, String> {
    let exe = agent_exe().ok_or("cannot locate the agent")?;
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": { "backer": { "command": exe.display().to_string(), "args": ["mcp-bridge"] } }
    })).map_err(|e| e.to_string())?)
}

#[cfg(feature = "direct")]
fn agent_exe() -> Option<PathBuf> {
    let name = if cfg!(windows) { "backer-agent.exe" } else { "backer-agent" };
    std::env::current_exe().ok()?.parent().map(|d| d.join(name))
}

/// Install and start the per-user background agent if it is not already answering. An agent that is answering but
/// runs a different program file (left over from an older install, or replaced on disk since it started) is stopped
/// and started again from this app's copy, so the app never talks to an agent that lacks the calls it needs.
#[cfg(feature = "direct")]
fn ensure_agent() {
    let Some(agent) = agent_exe() else { return };
    if connect().is_ok() {
        match call("agent_matches", serde_json::json!({ "exe": agent.display().to_string() })) {
            Ok(v) if v["current"] == true => return,
            // Agents from before this check do not know the call: they are the stale ones.
            Err(e) if !e.starts_with("unknown method") => return,
            _ => stop_service(),
        }
    }
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

/// Mac App Store edition: open the state, then run the scheduler on a thread of this process.
#[cfg(feature = "mas")]
fn ensure_agent() {
    if AGENT.get().is_some() {
        return;
    }
    // The sandbox makes $HOME this app's own container, so this lands inside it.
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/Backer");
    let _ = std::fs::create_dir_all(&dir);
    let log = std::sync::Arc::new(backer_agent::svclog::SvcLog::new(&dir));
    log.line(&format!("starting, version {}, data folder {}", env!("CARGO_PKG_VERSION"), dir.display()));
    // Regain access to the folders chosen in earlier sessions before any backup can start.
    #[cfg(target_os = "macos")]
    {
        let b = BOOKMARKS.get_or_init(|| backer_agent::bookmarks::Bookmarks::load(dir.join("bookmarks.json"), Box::new(backer_agent::native::SystemBookmarks)));
        for p in b.restore_all() {
            log.line(&format!("could not reopen the folder {p}; it needs to be chosen again"));
        }
    }
    match backer_agent::Agent::new(dir.join("state.sqlite"), jiff::tz::TimeZone::system()) {
        Ok(agent) => {
            let _ = AGENT.set(agent.clone());
            std::thread::spawn(move || backer_agent::service::run_scheduler(&agent, &log));
        }
        Err(e) => log.line(&format!("could not open state: {e:#}")),
    }
}

/// Stop scheduled backups and let running ones finish their current file.
#[cfg(feature = "mas")]
fn stop_service() {
    let _ = call("quit_agent", serde_json::json!({}));
    std::thread::sleep(std::time::Duration::from_millis(800));
}

fn show_main(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_dock_visibility(true);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Hide the window but keep the tray (and the agent) running. On a Mac the Dock icon goes too.
fn hide_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_dock_visibility(false);
}

/// Ask the agent to stop, wait for it to go, and make sure the OS does not restart it.
#[cfg(feature = "direct")]
fn stop_service() {
    let _ = call("quit_agent", serde_json::json!({}));
    for _ in 0..20 {
        if connect().is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    #[cfg(target_os = "macos")]
    {
        // Older installs had KeepAlive=true; unloading the job makes sure launchd never restarts it this session.
        let uid = Command::new("/usr/bin/id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|_| "501".into());
        let _ = Command::new("/bin/launchctl").args(["bootout", &format!("gui/{uid}/com.backer.agent")]).status();
    }
}

/// The tray's quit: stops backups, so ask first.
fn confirm_quit(app: &AppHandle) {
    let done = app.clone();
    app.dialog()
        .message("Backups will stop until you open Backer again. A backup in progress is cancelled; files already copied stay on your NAS.")
        .title("Quit Backer and stop backups?")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom("Quit and stop backups".into(), "Keep running".into()))
        .show(move |yes| {
            if yes {
                stop_service();
                done.exit(0);
            }
        });
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

#[derive(serde::Serialize)]
#[cfg_attr(feature = "mas", allow(dead_code))]
struct UpdateInfo {
    version: String,
    current: String,
    notes: Option<String>,
}

#[cfg(feature = "direct")]
/// Updates are switched on once tauri.conf.json carries the real updater public key and address.
fn updater_ready(app: &AppHandle) -> bool {
    app.config().plugins.0.get("updater").and_then(|c| c["pubkey"].as_str()).map(|k| !k.is_empty() && !k.contains("REPLACE")).unwrap_or(false)
}

/// Ask the update server whether a newer version exists. `None` means up to date (or updates are not set up yet).
#[cfg(feature = "direct")]
#[tauri::command]
async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    if !updater_ready(&app) {
        return Ok(None);
    }
    let up = app.updater().map_err(|e| e.to_string())?;
    let found = up.check().await.map_err(|e| format!("Could not check for updates: {e}"))?;
    Ok(found.map(|u| UpdateInfo { version: u.version.clone(), current: u.current_version.clone(), notes: u.body.clone() }))
}

/// Download the update, stop the background agent (so its file can be replaced), install, and restart.
#[cfg(feature = "direct")]
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let up = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = up.check().await.map_err(|e| format!("Could not check for updates: {e}"))? else {
        return Ok(());
    };
    // The download is checked against the signature in the update manifest before anything is replaced.
    let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| format!("The update could not be downloaded: {e}"))?;
    stop_service();
    if let Err(e) = update.install(bytes) {
        ensure_agent(); // keep backups running if the install failed
        return Err(format!("The update could not be installed: {e}"));
    }
    app.restart();
}

/// Mac App Store edition: the App Store delivers updates, so the app never checks for or installs any itself.
#[cfg(feature = "mas")]
#[tauri::command]
async fn check_update() -> Result<Option<UpdateInfo>, String> {
    Ok(None)
}

#[cfg(feature = "mas")]
#[tauri::command]
async fn install_update() -> Result<(), String> {
    Err("Updates come from the Mac App Store.".into())
}

/// Called by the page right after the user picks folders, so the sandbox keeps access to them. Does nothing in the direct edition.
#[tauri::command]
fn remember_folders(paths: Vec<String>) -> bool {
    #[cfg(all(feature = "mas", target_os = "macos"))]
    if let Some(b) = BOOKMARKS.get() {
        return paths.iter().fold(true, |ok, p| b.remember(p) && ok);
    }
    let _ = paths;
    true
}

/// "on", "off", "approval" (waiting for the user in System Settings) or "unsupported".
#[tauri::command]
fn login_item() -> &'static str {
    #[cfg(all(feature = "mas", target_os = "macos"))]
    return match backer_agent::native::login_item() {
        backer_agent::native::LoginItem::On => "on",
        backer_agent::native::LoginItem::Off => "off",
        backer_agent::native::LoginItem::NeedsApproval => "approval",
    };
    #[allow(unreachable_code)]
    "unsupported"
}

#[tauri::command]
fn set_login_item(on: bool) -> Result<&'static str, String> {
    #[cfg(all(feature = "mas", target_os = "macos"))]
    {
        if !backer_agent::native::set_login_item(on) {
            return Err("macOS would not change the login item. You can change it in System Settings, General, Login Items.".into());
        }
        return Ok(login_item());
    }
    #[allow(unreachable_code)]
    {
        let _ = on;
        Err("Not available in this edition.".into())
    }
}

/// Tells the page which edition it is, so it can leave out what the Mac App Store edition does not have.
#[tauri::command]
fn edition() -> &'static str {
    if cfg!(feature = "mas") { "store" } else { "direct" }
}

/// This program file and its modification time when the app started.
#[cfg(feature = "direct")]
static STARTED_AS: std::sync::OnceLock<Option<(PathBuf, std::time::SystemTime)>> = std::sync::OnceLock::new();

#[cfg(feature = "direct")]
fn own_identity() -> Option<(PathBuf, std::time::SystemTime)> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let modified = std::fs::metadata(&exe).ok()?.modified().ok()?;
    Some((exe, modified))
}

/// Has a different program file replaced the one this process started from?
#[cfg(feature = "direct")]
fn replaced_on_disk() -> bool {
    match (STARTED_AS.get().cloned().flatten(), own_identity()) {
        (Some(started), Some(now)) => started != now,
        _ => false,
    }
}

fn main() {
    #[cfg(feature = "direct")]
    STARTED_AS.get_or_init(own_identity);
    let builder = tauri::Builder::default();
    #[cfg(feature = "direct")]
    let builder = builder
        // Must be the first plugin. A second launch (double-click, Start menu, Dock, login item) does not start
        // another tray icon or window: it asks the running copy to show its window and then exits.
        // If this copy's program file was replaced since it started (a new version installed over it), the running
        // copy is out of date: restart into the new one, whose start-up also restarts an out-of-date agent.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if replaced_on_disk() {
                app.restart();
            }
            show_main(app)
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_updater::Builder::new().build());
    builder
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![agent_call, mcp_config, save_text_file, open_purchase_page, check_update, install_update, edition, remember_folders, login_item, set_login_item, feedback::feedback_available, feedback::send_feedback])
        .setup(|app| {
            ensure_agent();
            #[cfg(feature = "direct")]
            deep_link::setup(app);
            // Started by macOS at login: stay in the menu bar with the window hidden.
            #[cfg(all(feature = "mas", target_os = "macos"))]
            if backer_agent::native::launched_as_login_item() {
                hide_main(app.handle());
            }
            let open = MenuItem::with_id(app, "open", "Open Backer", true, None::<&str>)?;
            let run = MenuItem::with_id(app, "run_all", "Run all now", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Backer and stop backups…", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open, &run, &sep, &quit])?;
            let base = app.default_window_icon().unwrap().clone();
            let (rgba, iw, ih) = (base.rgba().to_vec(), base.width(), base.height());
            TrayIconBuilder::with_id("main")
                .icon(base.clone())
                .icon_as_template(false) // a template icon would be drawn in one colour and hide the dot
                .menu(&menu)
                // Windows convention: left click opens the app, right click shows the menu.
                .show_menu_on_left_click(!cfg!(windows))
                .on_tray_icon_event(|tray, ev| {
                    if cfg!(windows) {
                        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                            show_main(tray.app_handle());
                        }
                    }
                })
                .on_menu_event(|app, ev| match ev.id.as_ref() {
                    "open" => show_main(app),
                    "run_all" => run_all(),
                    "quit" => confirm_quit(app),
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
        // Closing the window hides it; the tray and the agent keep running.
        .on_window_event(|w, ev| {
            if let WindowEvent::CloseRequested { api, .. } = ev {
                api.prevent_close();
                hide_main(w.app_handle());
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Backer")
        .run(|app, ev| {
            // Quitting the app (Cmd+Q, the app menu) only closes the window. The tray's own quit calls exit(0)
            // with a code, which is let through.
            if let RunEvent::ExitRequested { api, code, .. } = ev {
                if code.is_none() {
                    api.prevent_exit();
                    hide_main(app);
                }
            }
        });
}
