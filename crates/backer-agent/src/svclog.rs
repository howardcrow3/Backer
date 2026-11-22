//! A small log file for the background service (`agent.log` in the data folder).
//! The service has no console on Windows and runs unattended on a Mac, so this is the only way to see why it
//! failed to start or why a tick failed. It keeps at most about 2 MB: when the file passes 1 MB it is moved to
//! `agent.log.1`, replacing the older one. It never holds file names from the user's backups, only service events.
use std::io::Write;
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 1_000_000;

pub struct SvcLog {
    path: PathBuf,
}

impl SvcLog {
    pub fn new(dir: &Path) -> Self {
        SvcLog { path: dir.join("agent.log") }
    }

    /// Appends one line, `<unix seconds> <message>`. Logging must never stop the service, so errors are ignored.
    pub fn line(&self, msg: &str) {
        self.rotate_if_big();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(f, "{now} {}", msg.replace('\n', " "));
        }
    }

    fn rotate_if_big(&self) {
        if std::fs::metadata(&self.path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
            let old = self.path.with_extension("log.1");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&self.path, old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_lines_and_flattens_newlines() {
        let d = tempfile::tempdir().unwrap();
        let l = SvcLog::new(d.path());
        l.line("started");
        l.line("two\nlines");
        let t = std::fs::read_to_string(d.path().join("agent.log")).unwrap();
        assert_eq!(t.lines().count(), 2);
        assert!(t.lines().next().unwrap().ends_with(" started"));
        assert!(t.contains("two lines"));
    }

    #[test]
    fn rotates_when_big() {
        let d = tempfile::tempdir().unwrap();
        let l = SvcLog::new(d.path());
        std::fs::write(d.path().join("agent.log"), vec![b'x'; 1_100_000]).unwrap();
        l.line("fresh");
        assert!(d.path().join("agent.log.1").exists());
        let t = std::fs::read_to_string(d.path().join("agent.log")).unwrap();
        assert_eq!(t.lines().count(), 1);
    }
}
