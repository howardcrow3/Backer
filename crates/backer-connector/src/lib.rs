//! Destination access. The `Connector` trait has no delete method on purpose:
//! only `Eraser` (used by backer-removal) can remove NAS files.
use backer_core::{Error, Result, DEST_MARKER};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteStat {
    pub size: u64,
    pub mtime: i64,
}

pub trait Connector: Send + Sync {
    /// Verify the destination is reachable and is the expected one (marker present).
    fn check_ready(&self) -> Result<()>;
    fn stat(&self, rel: &str) -> Result<Option<RemoteStat>>;
    /// Copy `src` to a temp file next to `rel`, verify, then atomically rename. Returns the BLAKE3 hash.
    fn put_file(&self, src: &Path, rel: &str, run: i64, seq: u64) -> Result<PutResult>;
    /// Read back the BLAKE3 hash of a file on the NAS.
    fn hash(&self, rel: &str) -> Result<String>;
    fn list_temp_files(&self, rel_dir: &str) -> Result<Vec<String>>;
}

#[derive(Debug, Clone)]
pub struct PutResult {
    pub hash: String,
    pub size: u64,
}

/// One entry in a folder on the NAS, for browsing a backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

/// Read-only access used by restore: list a folder and read a file back. Separate from `Connector`
/// so each destination type opts in, and nothing here can change the NAS.
pub trait Reader: Send + Sync {
    /// Entries in `rel_dir` ("" is the destination root). Backer's own marker and temp files are left out.
    /// A folder that does not exist is an empty list.
    fn list_dir(&self, rel_dir: &str) -> Result<Vec<DirEntry>>;
    /// Stream a file on the NAS into `out`. Returns the number of bytes written.
    fn read_file(&self, rel: &str, out: &mut dyn Write) -> Result<u64>;
}

/// True for files Backer itself keeps on the NAS (the marker and leftover temp files), never shown to people.
pub fn is_backer_file(name: &str) -> bool {
    name == DEST_MARKER || (name.starts_with(".backer-") && name.ends_with(".part"))
}

/// Can remove leftover temp files created by this app (never user files).
pub trait TempCleaner: Connector {
    fn remove_temp(&self, rel: &str) -> Result<()>;
}

/// The only way to delete a backed-up file. Only backer-removal may use this.
pub trait Eraser {
    fn erase(&self, rel: &str) -> Result<()>;
}

/// Connector for any mounted path (SMB/NFS mounted by the OS, or a local disk for tests).
pub struct MountedConnector {
    root: PathBuf,
}

fn cerr(e: impl std::fmt::Display) -> Error {
    Error::Connector(e.to_string())
}

/// Reject absolute paths and `..` so a bad relative path cannot escape the root.
fn safe_rel(rel: &str) -> Result<PathBuf> {
    // Rebuilt component by component so separators are native (backslashes on Windows).
    let mut out = PathBuf::new();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(n) => out.push(n),
            _ => return Err(Error::Invalid(format!("unsafe path: {rel}"))),
        }
    }
    Ok(out)
}

impl MountedConnector {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Create the marker file. Called once by the setup wizard after the user confirms the destination.
    pub fn init_marker(&self) -> Result<()> {
        fs::write(self.root.join(DEST_MARKER), b"backer destination\n").map_err(cerr)
    }
    fn full(&self, rel: &str) -> Result<PathBuf> {
        Ok(long_path(self.root.join(safe_rel(rel)?)))
    }
}

/// Windows: use the `\\?\` form so paths over 260 characters work. `\\server\share` becomes `\\?\UNC\server\share`.
#[cfg(windows)]
fn long_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().to_string();
    if s.starts_with(r"\\?\") || !p.is_absolute() {
        return p;
    }
    match s.strip_prefix(r"\\") {
        Some(unc) => PathBuf::from(format!(r"\\?\UNC\{unc}")),
        None => PathBuf::from(format!(r"\\?\{s}")),
    }
}
#[cfg(not(windows))]
fn long_path(p: PathBuf) -> PathBuf {
    p
}

pub fn temp_name(run: i64, seq: u64) -> String {
    format!(".backer-{run}-{seq}.part")
}

fn hash_file(p: &Path) -> std::io::Result<(String, u64)> {
    let mut f = File::open(p)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf)?;
        if r == 0 {
            break;
        }
        h.update(&buf[..r]);
        n += r as u64;
    }
    Ok((h.finalize().to_hex().to_string(), n))
}

impl Connector for MountedConnector {
    fn check_ready(&self) -> Result<()> {
        if !self.root.is_dir() {
            return Err(Error::DestinationNotReady(format!("{} is not available", self.root.display())));
        }
        if !self.root.join(DEST_MARKER).is_file() {
            return Err(Error::DestinationNotReady(format!(
                "{} has no marker file; the NAS may not be mounted",
                self.root.display()
            )));
        }
        Ok(())
    }

    fn stat(&self, rel: &str) -> Result<Option<RemoteStat>> {
        let p = self.full(rel)?;
        match fs::metadata(&p) {
            Ok(m) if m.is_file() => {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                Ok(Some(RemoteStat { size: m.len(), mtime }))
            }
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(cerr(e)),
        }
    }

    fn put_file(&self, src: &Path, rel: &str, run: i64, seq: u64) -> Result<PutResult> {
        let dest = self.full(rel)?;
        let dir = dest.parent().ok_or_else(|| Error::Invalid("no parent".into()))?;
        fs::create_dir_all(dir).map_err(cerr)?;
        let tmp = dir.join(temp_name(run, seq));
        // Copy while hashing the bytes we actually read.
        let mut input = File::open(src).map_err(cerr)?;
        let mut out = File::create(&tmp).map_err(cerr)?;
        let mut h = blake3::Hasher::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0u64;
        let copy = (|| -> std::io::Result<()> {
            loop {
                let n = input.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                out.write_all(&buf[..n])?;
                total += n as u64;
            }
            out.sync_all()
        })();
        if let Err(e) = copy {
            let _ = fs::remove_file(&tmp);
            return Err(cerr(e));
        }
        drop(out);
        let want = h.finalize().to_hex().to_string();
        // Verify what landed on the NAS before it replaces anything.
        let (got, size) = hash_file(&tmp).map_err(cerr)?;
        if got != want || size != total {
            let _ = fs::remove_file(&tmp);
            return Err(Error::Connector(format!("verification failed for {rel}")));
        }
        fs::rename(&tmp, &dest).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            cerr(e)
        })?;
        Ok(PutResult { hash: want, size: total })
    }

    fn hash(&self, rel: &str) -> Result<String> {
        hash_file(&self.full(rel)?).map(|x| x.0).map_err(cerr)
    }

    fn list_temp_files(&self, rel_dir: &str) -> Result<Vec<String>> {
        let d = self.full(rel_dir)?;
        let mut out = vec![];
        let rd = match fs::read_dir(&d) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(cerr(e)),
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with(".backer-") && n.ends_with(".part") {
                out.push(format!("{rel_dir}/{n}"));
            }
        }
        Ok(out)
    }
}

fn mtime_secs(m: &fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Reader for MountedConnector {
    fn list_dir(&self, rel_dir: &str) -> Result<Vec<DirEntry>> {
        let d = if rel_dir.is_empty() { long_path(self.root.clone()) } else { self.full(rel_dir)? };
        let rd = match fs::read_dir(&d) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(cerr(e)),
        };
        let mut out = vec![];
        for e in rd {
            let e = e.map_err(cerr)?;
            let name = e.file_name().to_string_lossy().to_string();
            if is_backer_file(&name) {
                continue;
            }
            // Follow symlinks the way the OS shows them; skip entries that vanish or can't be read.
            let Ok(m) = fs::metadata(e.path()) else { continue };
            out.push(DirEntry { name, is_dir: m.is_dir(), size: if m.is_dir() { 0 } else { m.len() }, mtime: mtime_secs(&m) });
        }
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(out)
    }

    fn read_file(&self, rel: &str, out: &mut dyn Write) -> Result<u64> {
        let mut f = File::open(self.full(rel)?).map_err(cerr)?;
        std::io::copy(&mut f, out).map_err(cerr)
    }
}

impl TempCleaner for MountedConnector {
    fn remove_temp(&self, rel: &str) -> Result<()> {
        let name = Path::new(rel).file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !(name.starts_with(".backer-") && name.ends_with(".part")) {
            return Err(Error::Invalid("not a backer temp file".into()));
        }
        fs::remove_file(self.full(rel)?).map_err(cerr)
    }
}

impl Eraser for MountedConnector {
    fn erase(&self, rel: &str) -> Result<()> {
        self.check_ready()?;
        fs::remove_file(self.full(rel)?).map_err(cerr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, MountedConnector) {
        let d = tempfile::tempdir().unwrap();
        let c = MountedConnector::new(d.path());
        c.init_marker().unwrap();
        (d, c)
    }

    #[test]
    fn put_and_stat() {
        let (_d, c) = setup();
        let src = tempfile::NamedTempFile::new().unwrap();
        fs::write(src.path(), b"hello").unwrap();
        let r = c.put_file(src.path(), "a/b/c.txt", 1, 1).unwrap();
        assert_eq!(r.size, 5);
        assert_eq!(c.stat("a/b/c.txt").unwrap().unwrap().size, 5);
        assert_eq!(c.hash("a/b/c.txt").unwrap(), r.hash);
        assert!(c.list_temp_files("a/b").unwrap().is_empty());
    }

    #[test]
    fn missing_marker_blocks() {
        let d = tempfile::tempdir().unwrap();
        let c = MountedConnector::new(d.path());
        assert!(matches!(c.check_ready(), Err(Error::DestinationNotReady(_))));
    }

    #[test]
    fn nested_relative_paths_work_with_forward_slashes() {
        let (_d, c) = setup();
        let src = tempfile::NamedTempFile::new().unwrap();
        fs::write(src.path(), b"x").unwrap();
        c.put_file(src.path(), "a/b/c/d.txt", 1, 1).unwrap();
        assert!(c.stat("a/b/c/d.txt").unwrap().is_some());
    }

    #[test]
    fn rejects_escape() {
        let (_d, c) = setup();
        assert!(c.stat("../x").is_err());
        assert!(c.stat("/etc/passwd").is_err());
    }

    #[test]
    fn reader_lists_folders_first_and_hides_backer_files() {
        let (d, c) = setup();
        fs::create_dir_all(d.path().join("m/Docs")).unwrap();
        fs::write(d.path().join("m/b.txt"), b"bb").unwrap();
        fs::write(d.path().join("m/A.txt"), b"a").unwrap();
        fs::write(d.path().join("m/.backer-3-1.part"), b"x").unwrap();
        let names: Vec<_> = c.list_dir("m").unwrap().into_iter().map(|e| (e.name, e.is_dir, e.size)).collect();
        assert_eq!(names, vec![("Docs".into(), true, 0), ("A.txt".into(), false, 1), ("b.txt".into(), false, 2)]);
        assert!(c.list_dir("").unwrap().iter().all(|e| e.name != DEST_MARKER));
        assert!(c.list_dir("nope").unwrap().is_empty());
        assert!(c.list_dir("../x").is_err());
        let mut s = vec![];
        assert_eq!(c.read_file("m/b.txt", &mut s).unwrap(), 2);
        assert_eq!(s, b"bb");
    }

    #[test]
    fn temp_cleaner_only_removes_temp() {
        let (d, c) = setup();
        fs::write(d.path().join("keep.txt"), b"x").unwrap();
        assert!(c.remove_temp("keep.txt").is_err());
        fs::write(d.path().join(".backer-1-1.part"), b"x").unwrap();
        c.remove_temp(".backer-1-1.part").unwrap();
    }
}
