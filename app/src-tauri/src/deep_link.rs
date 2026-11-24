//! `backer://activate?key=...` links from the purchase email. The operating system starts Backer (or hands the link to
//! the running copy) and the license is activated with no copy and paste. Parsing lives in `backer_license::link`.
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_deep_link::DeepLinkExt;

/// Sent to the UI after a link was handled, so it can refresh and say what happened.
pub const EVENT: &str = "license-link";

#[derive(Debug, Clone, Serialize)]
pub struct LinkResult {
    pub ok: bool,
    pub message: String,
}

fn dispatch(app: &AppHandle, urls: Vec<String>) {
    crate::show_main(app);
    // A link that is not an activation link only brings the window forward.
    let Some(key) = backer_license::link::key_from_links(&urls) else { return };
    let app = app.clone();
    // The agent call blocks (and may talk to the license server), so keep it off the event thread.
    std::thread::spawn(move || {
        let result = match crate::call("set_license_key", serde_json::json!({ "key": key })) {
            Ok(l) if l["state"] == "licensed" => LinkResult { ok: true, message: "Your license is active. Thank you!".into() },
            Ok(_) => LinkResult { ok: true, message: "Key saved. Backer will finish activating as soon as it can reach the license server.".into() },
            Err(message) => LinkResult { ok: false, message },
        };
        let _ = app.emit(EVENT, result);
    });
}

pub fn setup(app: &tauri::App) {
    let handle = app.handle().clone();
    app.deep_link().on_open_url({
        let handle = handle.clone();
        move |event| dispatch(&handle, event.urls().iter().map(|u| u.to_string()).collect())
    });
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        dispatch(&handle, urls.iter().map(|u| u.to_string()).collect());
    }
    // A development run has no installer to register the scheme.
    #[cfg(windows)]
    if cfg!(debug_assertions) {
        let _ = app.deep_link().register_all();
    }
}
