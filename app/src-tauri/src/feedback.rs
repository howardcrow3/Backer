//! "Send feedback": signs the user's rating and text with this build's key and posts them to the High
//! Turtle feedback service. Runs in the app (not the agent), so it works even when backups are stopped.

use backer_license::feedback::{body, new_key, sign, FEEDBACK_URL};

/// This install's signing key: read from the OS keychain, or made and saved on first use. If the
/// keychain refuses, the key lasts only until the app closes, which still works but means the
/// service sees a new install next time.
fn install_key() -> ed25519_dalek::SigningKey {
    #[cfg(any(windows, target_os = "macos"))]
    if let Ok(entry) = keyring::Entry::new("studio.turtlebox.backer", "feedback-install-key") {
        use backer_license::feedback::{key_from_hex, key_to_hex};
        if let Some(k) = entry.get_password().ok().and_then(|h| key_from_hex(&h)) {
            return k;
        }
        let k = new_key();
        let _ = entry.set_password(&key_to_hex(&k));
        return k;
    }
    new_key()
}

#[derive(serde::Deserialize)]
pub struct Feedback {
    rating: Option<u8>,
    #[serde(default)]
    message: String,
    category: Option<String>,
    email: Option<String>,
    #[serde(default)]
    anonymous: bool,
}

#[tauri::command]
pub fn feedback_available() -> bool {
    // No build-time secret is needed: each install signs with its own key.
    true
}

#[tauri::command]
pub async fn send_feedback(feedback: Feedback) -> Result<serde_json::Value, String> {
    let platform = if cfg!(target_os = "windows") { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" };
    let payload = body(feedback.rating, &feedback.message, feedback.category.as_deref(), feedback.email.as_deref(), feedback.anonymous, env!("CARGO_PKG_VERSION"), platform)?;
    tauri::async_runtime::spawn_blocking(move || {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs() as i64;
        let (install, sig) = sign(&install_key(), ts, &payload);
        let agent = ureq::AgentBuilder::new().timeout_connect(std::time::Duration::from_secs(8)).timeout(std::time::Duration::from_secs(20)).build();
        let res = agent
            .post(FEEDBACK_URL)
            .set("content-type", "application/json")
            .set("x-ht-app", backer_license::feedback::APP_ID)
            .set("x-ht-install", &install)
            .set("x-ht-timestamp", &ts.to_string())
            .set("x-ht-signature", &sig)
            .send_string(&payload);
        match res {
            Ok(r) => r.into_string().ok().and_then(|t| serde_json::from_str(&t).ok()).ok_or_else(|| "The feedback service sent an odd answer.".to_string()),
            Err(ureq::Error::Status(429, _)) => Err("You've sent a lot of feedback recently. Please try again later.".into()),
            Err(ureq::Error::Status(400, r)) => Err(r.into_string().ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v["error"].as_str().map(str::to_string)).unwrap_or_else(|| "The feedback couldn't be sent.".into())),
            Err(ureq::Error::Status(code, _)) => Err(format!("The feedback service answered {code}. Please try again later.")),
            Err(_) => Err("Couldn't reach the feedback service. Check your internet connection and try again.".into()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}
