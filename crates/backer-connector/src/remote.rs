//! Backup rules shared by every network protocol, written once over a small file-system trait.
//!
//! Each protocol (SFTP, SMB, NFS) only implements [`RemoteFs`]: stat, mkdir, write, read, rename,
//! remove and list. [`RemoteConnector`] adds everything Backer promises on top: paths that cannot
//! escape the destination, the marker check, copy to a temporary name, read-back verification and
//! the final rename. So a new protocol cannot get those rules wrong.
use crate::{is_backer_file, temp_name, Connector, DirEntry, Eraser, PutResult, Reader, RemoteStat, Setup, TempCleaner};
use backer_core::{Error, Result, DEST_MARKER};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

/// Primitive operations on a NAS. Paths are absolute within the protocol's root ("/a/b"):
/// the SFTP server's root, the SMB share or the NFS export.
pub trait RemoteFs: Send + Sync {
    /// `None` when nothing is there.
    fn stat(&self, path: &str) -> Result<Option<Meta>>;
    /// Create one folder. Succeeds when it already exists.
    fn mkdir(&self, path: &str) -> Result<()>;
    /// Create or truncate `path` and write all of `src` to it, durably.
    fn write(&self, path: &str, src: &mut dyn Read) -> Result<u64>;
    fn read(&self, path: &str, out: &mut dyn Write) -> Result<u64>;
    /// Rename, replacing `to` if it exists.
    fn rename(&self, from: &str, to: &str) -> Result<()>;
    /// Remove one file.
    fn remove(&self, path: &str) -> Result<()>;
    /// Entries of a folder, without "." and "..". A missing folder is an error.
    fn list(&self, path: &str) -> Result<Vec<DirEntry>>;
    /// Short description for messages, e.g. "sftp://nas:22".
    fn describe(&self) -> String;
}

pub struct RemoteConnector<F: RemoteFs> {
    fs: F,
    /// The destination folder inside the protocol root, e.g. "/volume1/backups" or "/" for a whole share.
    base: String,
    /// Folders known to exist, so 100,000 files do not cost 100,000 mkdir round trips.
    made: Mutex<HashSet<String>>,
}

/// Join `base` and a relative path, rejecting anything that could escape `base`.
pub(crate) fn join_rel(base: &str, rel: &str) -> Result<String> {
    let mut out = base.trim_end_matches('/').to_string();
    for part in rel.split('/') {
        if part.is_empty() {
            continue;
        }
        if part == "." || part == ".." || part.contains('\\') || part.contains('\0') {
            return Err(Error::Invalid(format!("unsafe path: {rel}")));
        }
        out.push('/');
        out.push_str(part);
    }
    if rel.starts_with('/') {
        return Err(Error::Invalid(format!("unsafe path: {rel}")));
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

struct HashRead<'a, R: Read> {
    inner: R,
    h: &'a mut blake3::Hasher,
    n: &'a mut u64,
}
impl<R: Read> Read for HashRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let r = self.inner.read(buf)?;
        self.h.update(&buf[..r]);
        *self.n += r as u64;
        Ok(r)
    }
}

struct HashWrite {
    h: blake3::Hasher,
    n: u64,
}
impl Write for HashWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.h.update(buf);
        self.n += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<F: RemoteFs> RemoteConnector<F> {
    pub fn new(fs: F, base: &str) -> Result<Self> {
        let base = crate::dest::clean_path(base)?;
        Ok(Self { fs, base, made: Mutex::new(HashSet::new()) })
    }
    pub fn fs(&self) -> &F {
        &self.fs
    }
    pub fn base(&self) -> &str {
        &self.base
    }
    fn full(&self, rel: &str) -> Result<String> {
        join_rel(&self.base, rel)
    }
    /// mkdir -p for a folder under `base` (the base itself must already exist).
    fn make_dirs(&self, rel_dir: &str) -> Result<()> {
        let mut cur = String::new();
        for part in rel_dir.split('/').filter(|p| !p.is_empty()) {
            cur = if cur.is_empty() { part.to_string() } else { format!("{cur}/{part}") };
            if self.made.lock().unwrap().contains(&cur) {
                continue;
            }
            let full = self.full(&cur)?;
            match self.fs.stat(&full)? {
                Some(m) if m.is_dir => {}
                Some(_) => return Err(Error::Connector(format!("{full} exists on the NAS and is not a folder"))),
                None => self.fs.mkdir(&full)?,
            }
            self.made.lock().unwrap().insert(cur.clone());
        }
        Ok(())
    }
    fn hash_remote(&self, full: &str) -> Result<(String, u64)> {
        let mut w = HashWrite { h: blake3::Hasher::new(), n: 0 };
        self.fs.read(full, &mut w)?;
        Ok((w.h.finalize().to_hex().to_string(), w.n))
    }
}

impl<F: RemoteFs> Connector for RemoteConnector<F> {
    fn check_ready(&self) -> Result<()> {
        match self.fs.stat(&self.base)? {
            Some(m) if m.is_dir => {}
            _ => return Err(Error::DestinationNotReady(format!("the folder {} is missing on {}", self.base, self.fs.describe()))),
        }
        match self.fs.stat(&self.full(DEST_MARKER)?)? {
            Some(m) if !m.is_dir => Ok(()),
            _ => Err(Error::DestinationNotReady(format!(
                "{} on {} has no marker file; it may not be the folder this job was set up with",
                self.base,
                self.fs.describe()
            ))),
        }
    }

    fn stat(&self, rel: &str) -> Result<Option<RemoteStat>> {
        Ok(match self.fs.stat(&self.full(rel)?)? {
            Some(m) if !m.is_dir => Some(RemoteStat { size: m.size, mtime: m.mtime }),
            _ => None,
        })
    }

    fn put_file(&self, src: &Path, rel: &str, run: i64, seq: u64) -> Result<PutResult> {
        let dest = self.full(rel)?;
        let (rel_dir, _) = rel.rsplit_once('/').unwrap_or(("", rel));
        self.make_dirs(rel_dir)?;
        let tmp = join_rel(&self.full(rel_dir)?, &temp_name(run, seq))?;
        let input = File::open(src).map_err(|e| Error::Connector(e.to_string()))?;
        let mut h = blake3::Hasher::new();
        let mut total = 0u64;
        let wrote = self.fs.write(&tmp, &mut HashRead { inner: input, h: &mut h, n: &mut total });
        let cleanup = |e: Error| {
            let _ = self.fs.remove(&tmp);
            e
        };
        wrote.map_err(cleanup)?;
        let want = h.finalize().to_hex().to_string();
        // Verify what landed on the NAS before it replaces anything.
        let (got, size) = self.hash_remote(&tmp).map_err(cleanup)?;
        if got != want || size != total {
            return Err(cleanup(Error::Connector(format!("verification failed for {rel}"))));
        }
        self.fs.rename(&tmp, &dest).map_err(cleanup)?;
        Ok(PutResult { hash: want, size: total })
    }

    fn hash(&self, rel: &str) -> Result<String> {
        Ok(self.hash_remote(&self.full(rel)?)?.0)
    }

    fn list_temp_files(&self, rel_dir: &str) -> Result<Vec<String>> {
        let full = self.full(rel_dir)?;
        let entries = match self.fs.stat(&full)? {
            Some(m) if m.is_dir => self.fs.list(&full)?,
            _ => return Ok(vec![]),
        };
        Ok(entries
            .into_iter()
            .filter(|e| !e.is_dir && e.name.starts_with(".backer-") && e.name.ends_with(".part"))
            .map(|e| if rel_dir.is_empty() { e.name } else { format!("{rel_dir}/{}", e.name) })
            .collect())
    }
}

impl<F: RemoteFs> TempCleaner for RemoteConnector<F> {
    fn remove_temp(&self, rel: &str) -> Result<()> {
        let name = rel.rsplit('/').next().unwrap_or("");
        if !(name.starts_with(".backer-") && name.ends_with(".part")) {
            return Err(Error::Invalid("not a backer temp file".into()));
        }
        self.fs.remove(&self.full(rel)?)
    }
}

impl<F: RemoteFs> Eraser for RemoteConnector<F> {
    fn erase(&self, rel: &str) -> Result<()> {
        self.check_ready()?;
        self.fs.remove(&self.full(rel)?)
    }
}

impl<F: RemoteFs> Reader for RemoteConnector<F> {
    fn list_dir(&self, rel_dir: &str) -> Result<Vec<DirEntry>> {
        let full = self.full(rel_dir)?;
        match self.fs.stat(&full)? {
            Some(m) if m.is_dir => {}
            _ => return Ok(vec![]),
        }
        let mut v: Vec<DirEntry> = self.fs.list(&full)?.into_iter().filter(|e| !is_backer_file(&e.name)).collect();
        v.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(v)
    }
    fn read_file(&self, rel: &str, out: &mut dyn Write) -> Result<u64> {
        self.fs.read(&self.full(rel)?, out)
    }
}

impl<F: RemoteFs> Setup for RemoteConnector<F> {
    fn reachable(&self) -> Result<()> {
        match self.fs.stat(&self.base)? {
            Some(m) if m.is_dir => Ok(()),
            Some(_) => Err(Error::Invalid(format!("{} is a file, not a folder", self.base))),
            None => Err(Error::DestinationNotReady(format!("the folder {} was not found on {}", self.base, self.fs.describe()))),
        }
    }
    fn check_writable(&self) -> Result<()> {
        self.reachable()?;
        let probe = self.full(".backer-probe.part")?;
        self.fs
            .write(&probe, &mut &b"backer write test\n"[..])
            .map_err(|e| match e {
                Error::Connector(m) => Error::Connector(format!("no write permission in {}: {m}", self.base)),
                e => e,
            })?;
        self.fs.remove(&probe)
    }
    fn init_marker(&self) -> Result<()> {
        self.reachable()?;
        self.fs.write(&self.full(DEST_MARKER)?, &mut &b"backer destination\n"[..]).map(|_| ())
    }
    fn create_dir(&self, rel: &str) -> Result<()> {
        self.reachable()?;
        self.make_dirs(rel)
    }
}

#[cfg(test)]
pub(crate) mod memfs {
    //! An in-memory NAS for testing the shared rules without a server.
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    pub struct MemFs {
        pub files: Mutex<BTreeMap<String, Vec<u8>>>,
        pub dirs: Mutex<HashSet<String>>,
        /// Corrupt the next written file (to test verification).
        pub corrupt_next: Mutex<bool>,
        pub read_only: bool,
        pub mkdirs: Mutex<u32>,
    }
    impl MemFs {
        pub fn with_dirs(d: &[&str]) -> Self {
            let m = MemFs::default();
            m.dirs.lock().unwrap().insert("/".into());
            for x in d {
                m.dirs.lock().unwrap().insert(x.to_string());
            }
            m
        }
        fn parent(p: &str) -> String {
            match p.rsplit_once('/') {
                Some(("", _)) => "/".into(),
                Some((a, _)) => a.into(),
                None => "/".into(),
            }
        }
    }
    impl RemoteFs for MemFs {
        fn stat(&self, p: &str) -> Result<Option<Meta>> {
            if self.dirs.lock().unwrap().contains(p) {
                return Ok(Some(Meta { is_dir: true, size: 0, mtime: 0 }));
            }
            Ok(self.files.lock().unwrap().get(p).map(|f| Meta { is_dir: false, size: f.len() as u64, mtime: 1 }))
        }
        fn mkdir(&self, p: &str) -> Result<()> {
            if !self.dirs.lock().unwrap().contains(&Self::parent(p)) {
                return Err(Error::Connector(format!("no parent for {p}")));
            }
            *self.mkdirs.lock().unwrap() += 1;
            self.dirs.lock().unwrap().insert(p.into());
            Ok(())
        }
        fn write(&self, p: &str, src: &mut dyn Read) -> Result<u64> {
            if self.read_only {
                return Err(Error::Connector("permission denied".into()));
            }
            if !self.dirs.lock().unwrap().contains(&Self::parent(p)) {
                return Err(Error::Connector(format!("no parent for {p}")));
            }
            let mut v = vec![];
            src.read_to_end(&mut v).map_err(|e| Error::Connector(e.to_string()))?;
            if std::mem::take(&mut *self.corrupt_next.lock().unwrap()) {
                v.push(b'!');
            }
            let n = v.len() as u64;
            self.files.lock().unwrap().insert(p.into(), v);
            Ok(n)
        }
        fn read(&self, p: &str, out: &mut dyn Write) -> Result<u64> {
            let f = self.files.lock().unwrap().get(p).cloned().ok_or_else(|| Error::Connector(format!("{p} not found")))?;
            out.write_all(&f).map_err(|e| Error::Connector(e.to_string()))?;
            Ok(f.len() as u64)
        }
        fn rename(&self, from: &str, to: &str) -> Result<()> {
            let mut f = self.files.lock().unwrap();
            let v = f.remove(from).ok_or_else(|| Error::Connector("missing".into()))?;
            f.insert(to.into(), v);
            Ok(())
        }
        fn remove(&self, p: &str) -> Result<()> {
            self.files.lock().unwrap().remove(p).map(|_| ()).ok_or_else(|| Error::Connector("missing".into()))
        }
        fn list(&self, p: &str) -> Result<Vec<DirEntry>> {
            let pre = if p == "/" { "/".to_string() } else { format!("{p}/") };
            let mut out = vec![];
            for (k, v) in self.files.lock().unwrap().iter() {
                if let Some(n) = k.strip_prefix(&pre) {
                    if !n.contains('/') {
                        out.push(DirEntry { name: n.into(), is_dir: false, size: v.len() as u64, mtime: 1 });
                    }
                }
            }
            for d in self.dirs.lock().unwrap().iter() {
                if let Some(n) = d.strip_prefix(&pre) {
                    if !n.is_empty() && !n.contains('/') {
                        out.push(DirEntry { name: n.into(), is_dir: true, size: 0, mtime: 0 });
                    }
                }
            }
            Ok(out)
        }
        fn describe(&self) -> String {
            "mem".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memfs::MemFs;
    use super::*;

    fn conn() -> RemoteConnector<MemFs> {
        let c = RemoteConnector::new(MemFs::with_dirs(&["/vol", "/vol/backups"]), "/vol/backups").unwrap();
        c.init_marker().unwrap();
        c
    }
    fn src(bytes: &[u8]) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), bytes).unwrap();
        f
    }

    #[test]
    fn join_rejects_escapes() {
        assert_eq!(join_rel("/b", "x/y").unwrap(), "/b/x/y");
        assert_eq!(join_rel("/", "x").unwrap(), "/x");
        assert_eq!(join_rel("/b", "").unwrap(), "/b");
        assert_eq!(join_rel("/", "").unwrap(), "/");
        assert!(join_rel("/b", "../x").is_err());
        assert!(join_rel("/b", "a/../../x").is_err());
        assert!(join_rel("/b", "/etc/passwd").is_err());
        assert!(join_rel("/b", r"a\..\x").is_err());
    }

    #[test]
    fn put_creates_folders_verifies_and_renames() {
        let c = conn();
        let s = src(b"hello");
        let r = c.put_file(s.path(), "Docs/a/b.txt", 7, 1).unwrap();
        assert_eq!(r.size, 5);
        assert_eq!(r.hash, blake3::hash(b"hello").to_hex().to_string());
        assert_eq!(c.stat("Docs/a/b.txt").unwrap().unwrap().size, 5);
        assert_eq!(c.hash("Docs/a/b.txt").unwrap(), r.hash);
        assert!(c.list_temp_files("Docs/a").unwrap().is_empty(), "temp file renamed away");
        // A second file in the same folder costs no more mkdirs.
        let before = *c.fs().mkdirs.lock().unwrap();
        c.put_file(s.path(), "Docs/a/c.txt", 7, 2).unwrap();
        assert_eq!(*c.fs().mkdirs.lock().unwrap(), before);
        // Replacing an existing file works.
        let s2 = src(b"hello again");
        assert_eq!(c.put_file(s2.path(), "Docs/a/b.txt", 8, 1).unwrap().size, 11);
        let mut out = vec![];
        c.read_file("Docs/a/b.txt", &mut out).unwrap();
        assert_eq!(out, b"hello again");
    }

    #[test]
    fn failed_verification_leaves_nothing_behind() {
        let c = conn();
        *c.fs().corrupt_next.lock().unwrap() = true;
        let s = src(b"data");
        assert!(c.put_file(s.path(), "x/y.txt", 1, 1).is_err());
        assert!(c.stat("x/y.txt").unwrap().is_none());
        assert!(c.list_temp_files("x").unwrap().is_empty());
    }

    #[test]
    fn marker_and_missing_folder_block_runs() {
        let c = RemoteConnector::new(MemFs::with_dirs(&["/vol", "/vol/backups"]), "/vol/backups").unwrap();
        assert!(matches!(c.check_ready(), Err(Error::DestinationNotReady(_))), "no marker");
        c.init_marker().unwrap();
        c.check_ready().unwrap();
        let gone = RemoteConnector::new(MemFs::with_dirs(&[]), "/vol/backups").unwrap();
        assert!(matches!(gone.check_ready(), Err(Error::DestinationNotReady(_))));
        assert!(gone.init_marker().is_err(), "the marker is never written into a missing folder");
    }

    #[test]
    fn temp_cleaner_only_removes_temp_files() {
        let c = conn();
        let s = src(b"x");
        c.put_file(s.path(), "keep.txt", 1, 1).unwrap();
        assert!(c.remove_temp("keep.txt").is_err());
        c.fs().write("/vol/backups/d/.backer-1-9.part", &mut &b"x"[..]).ok();
        c.create_dir("d").unwrap();
        c.fs().write("/vol/backups/d/.backer-1-9.part", &mut &b"x"[..]).unwrap();
        assert_eq!(c.list_temp_files("d").unwrap(), vec!["d/.backer-1-9.part".to_string()]);
        c.remove_temp("d/.backer-1-9.part").unwrap();
        assert!(c.list_temp_files("d").unwrap().is_empty());
        assert!(c.list_temp_files("nope").unwrap().is_empty());
    }

    #[test]
    fn list_dir_hides_backer_files_and_sorts_folders_first() {
        let c = conn();
        let s = src(b"x");
        c.put_file(s.path(), "b.txt", 1, 1).unwrap();
        c.put_file(s.path(), "Alpha/x.txt", 1, 2).unwrap();
        c.fs().write("/vol/backups/.backer-3-3.part", &mut &b"x"[..]).unwrap();
        let names: Vec<_> = c.list_dir("").unwrap().into_iter().map(|e| (e.name, e.is_dir)).collect();
        assert_eq!(names, vec![("Alpha".into(), true), ("b.txt".into(), false)]);
        assert!(c.list_dir("missing").unwrap().is_empty());
    }

    #[test]
    fn writable_check_reports_permission() {
        let c = conn();
        c.check_writable().unwrap();
        assert!(c.fs().files.lock().unwrap().keys().all(|k| !k.contains("probe")), "probe removed");
        let mut ro = MemFs::with_dirs(&["/s"]);
        ro.read_only = true;
        let r = RemoteConnector::new(ro, "/s").unwrap();
        let e = r.check_writable().unwrap_err().to_string();
        assert!(e.contains("no write permission"), "{e}");
    }

    #[test]
    fn erase_requires_marker() {
        let c = conn();
        let s = src(b"x");
        c.put_file(s.path(), "f.txt", 1, 1).unwrap();
        c.fs().remove("/vol/backups/.backer-destination").unwrap();
        assert!(c.erase("f.txt").is_err());
        assert!(c.stat("f.txt").unwrap().is_some());
    }
}
