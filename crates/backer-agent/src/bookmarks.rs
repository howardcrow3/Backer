//! Remembers folders the user chose, so a sandboxed app can open them again after a restart (Mac App Store edition).
//! The system part sits behind `Backend`; the bookkeeping here is plain Rust and tested everywhere.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

pub trait Backend: Send + Sync {
    /// A bookmark for a folder the user has just chosen.
    fn create(&self, path: &str) -> Option<Vec<u8>>;
    /// Gets access to the folder for the rest of the process. `None` when that failed; `Some(true)` when the bookmark is stale.
    fn open(&self, data: &[u8]) -> Option<bool>;
}

pub struct Bookmarks {
    file: PathBuf,
    backend: Box<dyn Backend>,
    /// Folder path to bookmark bytes, as hex.
    saved: Mutex<BTreeMap<String, String>>,
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

impl Bookmarks {
    pub fn load(file: PathBuf, backend: Box<dyn Backend>) -> Self {
        let saved = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Self { file, backend, saved: Mutex::new(saved) }
    }

    fn write(&self, s: &BTreeMap<String, String>) {
        let _ = std::fs::write(&self.file, serde_json::to_string(s).unwrap_or_default());
    }

    /// Call right after the user picks a folder. Returns false when no bookmark could be made.
    pub fn remember(&self, path: &str) -> bool {
        let Some(data) = self.backend.create(path) else { return false };
        let mut s = self.saved.lock().unwrap();
        s.insert(path.to_string(), to_hex(&data));
        self.write(&s);
        true
    }

    /// Call at start-up: regains access to every remembered folder. Returns the folders that could not be opened,
    /// so the app can ask the user to choose them again.
    pub fn restore_all(&self) -> Vec<String> {
        let mut failed = Vec::new();
        let mut s = self.saved.lock().unwrap();
        let mut changed = false;
        for (path, hex) in s.clone() {
            match from_hex(&hex).and_then(|d| self.backend.open(&d)) {
                Some(false) => {}
                Some(true) => {
                    // Remake a stale bookmark while we have access.
                    if let Some(d) = self.backend.create(&path) {
                        s.insert(path, to_hex(&d));
                        changed = true;
                    }
                }
                None => failed.push(path),
            }
        }
        if changed {
            self.write(&s);
        }
        failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Default)]
    struct Fake {
        opened: Mutex<Vec<Vec<u8>>>,
        stale: bool,
        refuse: bool,
    }
    impl Backend for Arc<Fake> {
        fn create(&self, path: &str) -> Option<Vec<u8>> {
            (!path.is_empty()).then(|| path.as_bytes().to_vec())
        }
        fn open(&self, data: &[u8]) -> Option<bool> {
            if self.refuse {
                return None;
            }
            self.opened.lock().unwrap().push(data.to_vec());
            Some(self.stale)
        }
    }

    #[test]
    fn remembered_folders_are_reopened_after_a_restart() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("bookmarks.json");
        let fake = Arc::new(Fake::default());
        let b = Bookmarks::load(f.clone(), Box::new(fake.clone()));
        assert!(b.remember("/Users/x/Documents"));
        assert!(!b.remember(""));
        let again = Arc::new(Fake::default());
        let b2 = Bookmarks::load(f, Box::new(again.clone()));
        assert!(b2.restore_all().is_empty());
        assert_eq!(again.opened.lock().unwrap().as_slice(), &[b"/Users/x/Documents".to_vec()]);
    }

    #[test]
    fn folders_that_cannot_be_opened_are_reported_and_stale_ones_remade() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("bookmarks.json");
        let b = Bookmarks::load(f.clone(), Box::new(Arc::new(Fake::default())));
        b.remember("/a");
        let refusing = Bookmarks::load(f.clone(), Box::new(Arc::new(Fake { refuse: true, ..Default::default() })));
        assert_eq!(refusing.restore_all(), vec!["/a".to_string()]);
        let stale = Bookmarks::load(f, Box::new(Arc::new(Fake { stale: true, ..Default::default() })));
        assert!(stale.restore_all().is_empty());
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(from_hex(&to_hex(&[0, 1, 254, 255])), Some(vec![0, 1, 254, 255]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
    }
}
