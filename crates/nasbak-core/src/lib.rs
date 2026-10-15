//! Shared types. No I/O, no dependencies on other nasbak crates.
use serde::{Deserialize, Serialize};

pub type JobId = i64;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("store: {0}")]
    Store(String),
    #[error("connector: {0}")]
    Connector(String),
    #[error("destination not ready: {0}")]
    DestinationNotReady(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("cancelled")]
    Cancelled,
}
pub type Result<T> = std::result::Result<T, Error>;

/// One source folder paired with its folder on the NAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderPair {
    /// Absolute path on this machine.
    pub source: String,
    /// Path relative to the destination root, e.g. "mac/Documents".
    pub dest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Only when the user clicks Run now.
    Manual,
    EveryHours { hours: u32 },
    /// Local wall-clock time, every day.
    Daily { hour: u8, minute: u8 },
    /// Local wall-clock time on the given weekdays (1 = Monday .. 7 = Sunday).
    Weekly { days: Vec<u8>, hour: u8, minute: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub name: String,
    pub folders: Vec<FolderPair>,
    /// Root of the NAS destination as a mounted path (e.g. /Volumes/backups).
    pub dest_root: String,
    pub schedule: Schedule,
    pub paused: bool,
    pub catch_up: bool,
    pub exclude_names: Vec<String>,
    /// Files larger than this are skipped with a warning. None = no limit.
    pub max_file_bytes: Option<u64>,
}

impl Job {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(Error::Invalid("name is empty".into()));
        }
        if self.folders.is_empty() {
            return Err(Error::Invalid("no folders".into()));
        }
        for f in &self.folders {
            if !std::path::Path::new(&f.source).is_absolute() {
                return Err(Error::Invalid(format!("source must be absolute: {}", f.source)));
            }
            if f.dest.starts_with('/') || f.dest.contains('\\') || f.dest.split('/').any(|p| p == "..") || f.dest.is_empty() {
                return Err(Error::Invalid(format!("bad destination: {}", f.dest)));
            }
        }
        // Two sources must not share a destination.
        for (i, a) in self.folders.iter().enumerate() {
            for b in &self.folders[i + 1..] {
                if a.dest == b.dest || a.dest.starts_with(&format!("{}/", b.dest)) || b.dest.starts_with(&format!("{}/", a.dest)) {
                    return Err(Error::Invalid(format!("destinations overlap: {} and {}", a.dest, b.dest)));
                }
            }
        }
        if let Schedule::Daily { hour, minute } | Schedule::Weekly { hour, minute, .. } = &self.schedule {
            if *hour > 23 || *minute > 59 {
                return Err(Error::Invalid("bad time".into()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Ok,
    Warnings,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Ok => "ok",
            RunStatus::Warnings => "warnings",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => RunStatus::Running,
            "ok" => RunStatus::Ok,
            "warnings" => RunStatus::Warnings,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStats {
    pub scanned: u64,
    pub copied: u64,
    pub bytes: u64,
    pub unchanged: u64,
    pub skipped: u64,
    pub warnings: u64,
    pub newly_deleted: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub id: i64,
    pub job_id: JobId,
    pub started: i64,
    pub finished: Option<i64>,
    pub status: RunStatus,
    pub stats: RunStats,
    pub message: Option<String>,
}

/// A file that is on the NAS but no longer on the source. Awaiting the user's decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeletedFile {
    pub job_id: JobId,
    pub dest_path: String,
    pub size: u64,
    pub detected: i64,
}

/// Name of the marker file that proves the destination is the real NAS and not an empty mount point.
pub const DEST_MARKER: &str = ".nasbak-destination";

#[cfg(test)]
mod tests {
    use super::*;
    fn job(source: &str, dest: &str) -> Job {
        Job { id: 1, name: "j".into(), folders: vec![FolderPair { source: source.into(), dest: dest.into() }], dest_root: "x".into(),
            schedule: Schedule::Manual, paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None }
    }
    #[test]
    fn dest_must_be_forward_slash_relative() {
        let abs = if cfg!(windows) { r"C:\Users\h\Docs" } else { "/Users/h/Docs" };
        assert!(job(abs, "mac/Docs").validate().is_ok());
        assert!(job(abs, r"mac\Docs").validate().is_err());
        assert!(job(abs, "../Docs").validate().is_err());
        assert!(job("relative/path", "mac/Docs").validate().is_err());
    }
}
