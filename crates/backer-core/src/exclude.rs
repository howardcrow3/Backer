//! Exclude patterns: which files and folders a job leaves out (spec FR-5).
//!
//! Each pattern is a glob, matched case-insensitively against the source folder's contents:
//! - No `/` in it (`*.tmp`, `Thumbs.db`, `~$*`): matches a file or folder name at any depth.
//! - Ends in `/` (`node_modules/`, `*.cache/`): matches folders only. Everything inside is skipped.
//! - Any other `/` (`build/out`, `/Cache`, `**/logs/*.log`): matched against the path from the top of
//!   the source folder, written with `/`. A leading `/` only anchors it there.
//!
//! `*` and `?` never cross a `/`, `**` does, and `[abc]` / `[!abc]` / `{a,b}` work as usual.
//! A plain name with no wildcards is still an exact name match, so jobs saved before patterns existed keep working.
use crate::{Error, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use unicode_normalization::UnicodeNormalization;

/// Compiled exclude patterns for one job. Build it once per walk; matching is cheap.
#[derive(Debug, Clone)]
pub struct ExcludeSet {
    name_any: GlobSet,
    name_dir: GlobSet,
    path_any: GlobSet,
    path_dir: GlobSet,
    empty: bool,
}

impl Default for ExcludeSet {
    fn default() -> Self {
        Self::new::<&str>(&[]).expect("empty set always builds")
    }
}

fn nfc(s: &str) -> String {
    if s.is_ascii() {
        s.to_string()
    } else {
        s.nfc().collect()
    }
}

impl ExcludeSet {
    /// Compiles the patterns. Blank entries are ignored. Errors name the pattern that is wrong.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<Self> {
        let mut b = [GlobSetBuilder::new(), GlobSetBuilder::new(), GlobSetBuilder::new(), GlobSetBuilder::new()];
        let mut empty = true;
        for raw in patterns {
            let raw = raw.as_ref();
            let p = raw.trim();
            if p.is_empty() {
                continue;
            }
            if p.starts_with('!') {
                return Err(Error::Invalid(format!("exclude pattern “{raw}”: patterns starting with ! are not supported")));
            }
            if p.contains('\\') {
                return Err(Error::Invalid(format!("exclude pattern “{raw}”: use / to separate folders, not \\")));
            }
            let dir_only = p.ends_with('/');
            let body = p.trim_end_matches('/');
            let anchored = body.starts_with('/') || body.contains('/');
            let body = body.trim_start_matches('/');
            if body.is_empty() {
                return Err(Error::Invalid(format!("exclude pattern “{raw}” would leave out everything")));
            }
            let glob = GlobBuilder::new(&nfc(body))
                .case_insensitive(true)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map_err(|e| Error::Invalid(format!("exclude pattern “{raw}”: {}", e.kind())))?;
            let i = match (anchored, dir_only) {
                (false, false) => 0,
                (false, true) => 1,
                (true, false) => 2,
                (true, true) => 3,
            };
            b[i].add(glob);
            empty = false;
        }
        let [a, d, pa, pd] = b.map(|x| x.build().map_err(|e| Error::Invalid(format!("exclude patterns: {e}"))));
        Ok(Self { name_any: a?, name_dir: d?, path_any: pa?, path_dir: pd?, empty })
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// `rel` is the path from the top of the source folder with `/` separators, e.g. "src/app/x.tmp".
    pub fn is_excluded(&self, rel: &str, is_dir: bool) -> bool {
        if self.empty {
            return false;
        }
        let rel = nfc(rel.trim_matches('/'));
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        self.name_any.is_match(name)
            || self.path_any.is_match(&rel)
            || (is_dir && (self.name_dir.is_match(name) || self.path_dir.is_match(&rel)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(p: &[&str]) -> ExcludeSet {
        ExcludeSet::new(p).unwrap()
    }

    #[test]
    fn extension_matches_at_any_depth() {
        let s = set(&["*.tmp"]);
        assert!(s.is_excluded("a.tmp", false));
        assert!(s.is_excluded("deep/down/b.tmp", false));
        assert!(!s.is_excluded("a.tmp.txt", false));
        assert!(!s.is_excluded("tmp", false));
    }

    #[test]
    fn trailing_slash_means_folders_only() {
        let s = set(&["node_modules/"]);
        assert!(s.is_excluded("node_modules", true));
        assert!(s.is_excluded("web/app/node_modules", true));
        assert!(!s.is_excluded("notes/node_modules", false), "a file with that name is kept");
    }

    #[test]
    fn name_without_slash_matches_files_and_folders() {
        let s = set(&["cache"]);
        assert!(s.is_excluded("cache", true));
        assert!(s.is_excluded("x/cache", false));
        assert!(!s.is_excluded("x/cache2", false));
    }

    #[test]
    fn slash_anchors_to_the_source_folder() {
        let s = set(&["build/out", "/Cache"]);
        assert!(s.is_excluded("build/out", true));
        assert!(!s.is_excluded("app/build/out", true));
        assert!(s.is_excluded("Cache", true));
        assert!(!s.is_excluded("sub/Cache", true));
    }

    #[test]
    fn star_stays_in_one_folder_and_double_star_crosses() {
        let s = set(&["logs/*.log", "**/tmp/**"]);
        assert!(s.is_excluded("logs/a.log", false));
        assert!(!s.is_excluded("logs/old/a.log", false));
        assert!(s.is_excluded("a/b/tmp/c/d.txt", false));
        assert!(s.is_excluded("tmp/x", false));
    }

    #[test]
    fn matching_ignores_case() {
        let s = set(&["*.TMP", "Thumbs.db"]);
        assert!(s.is_excluded("x.tmp", false));
        assert!(s.is_excluded("THUMBS.DB", false));
    }

    #[test]
    fn character_classes_and_alternatives() {
        let s = set(&["~$*", "*.{bak,swp}", "file[0-9].txt"]);
        assert!(s.is_excluded("~$Report.docx", false));
        assert!(s.is_excluded("a.swp", false));
        assert!(s.is_excluded("a.bak", false));
        assert!(s.is_excluded("file7.txt", false));
        assert!(!s.is_excluded("fileX.txt", false));
    }

    #[test]
    fn decomposed_unicode_names_match_composed_patterns() {
        let s = set(&["café*"]); // typed as composed é
        assert!(s.is_excluded("cafe\u{301} menu.txt", false)); // macOS often stores e + combining accent
    }

    #[test]
    fn exact_names_from_older_jobs_still_work() {
        let s = set(&["secret.txt"]);
        assert!(s.is_excluded("a/secret.txt", false));
        assert!(!s.is_excluded("a/secret.txt.bak", false));
    }

    #[test]
    fn blanks_are_ignored_and_empty_set_matches_nothing() {
        let s = set(&["", "   "]);
        assert!(s.is_empty());
        assert!(!s.is_excluded("anything", false));
    }

    #[test]
    fn bad_patterns_are_rejected_with_the_pattern_named() {
        for bad in ["[abc", "!keep.txt", "/", "a\\b", "{a,b"] {
            let e = ExcludeSet::new(&[bad]).unwrap_err().to_string();
            assert!(e.contains(bad.trim_end_matches('/')) || e.contains("everything"), "{bad}: {e}");
        }
    }
}
