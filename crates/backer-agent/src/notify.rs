//! Operating-system notifications from the background agent (spec 8.3). The agent runs without the
//! app window, so it posts them itself. Best effort: a notification that cannot be shown never
//! affects a backup.
use std::sync::Mutex;

pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str);
}

/// Posts to Notification Center on macOS, and through notify-send on Linux (development).
/// Windows support arrives with the Windows build.
pub struct SystemNotifier;

impl Notifier for SystemNotifier {
    fn notify(&self, title: &str, body: &str) {
        #[cfg(target_os = "macos")]
        {
            // Text goes in as arguments, never into the script, so quotes in a job name cannot break it.
            let _ = std::process::Command::new("/usr/bin/osascript")
                .args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run", title, body])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(target_os = "linux")]
        {
            let _ = std::process::Command::new("notify-send")
                .args(["--app-name=Backer", "--", title, body])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = (title, body);
    }
}

/// Shows nothing. Used by tests so a developer's machine is not flooded.
pub struct NullNotifier;
impl Notifier for NullNotifier {
    fn notify(&self, _: &str, _: &str) {}
}

/// Remembers what would have been shown, for tests.
#[derive(Default)]
pub struct RecordingNotifier(pub Mutex<Vec<(String, String)>>);
impl Notifier for RecordingNotifier {
    fn notify(&self, title: &str, body: &str) {
        self.0.lock().unwrap().push((title.into(), body.into()));
    }
}
