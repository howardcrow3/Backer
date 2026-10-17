//! The only crate allowed to remove files from the NAS, and only files the user has reviewed.
//! Nothing here is reachable from the MCP crates (checked by scripts/check-crate-graph.sh).
use backer_connector::{Connector, Eraser};
use backer_core::*;
use backer_store::Store;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Removed,
    /// Not on the review list, so refused.
    NotListed,
    Failed(String),
}

/// Remove the given files from the NAS. Each path must currently be on the deleted-files review list.
/// The user's confirmation happens in the app before this is called.
pub fn remove_reviewed<C: Connector + Eraser>(store: &Store, conn: &C, job: JobId, dest_paths: &[String]) -> Result<Vec<(String, Outcome)>> {
    conn.check_ready()?;
    let mut out = vec![];
    for p in dest_paths {
        if !store.is_listed_deleted(job, p)? {
            out.push((p.clone(), Outcome::NotListed));
            continue;
        }
        match conn.erase(p) {
            Ok(()) => {
                store.forget_file(job, p)?;
                out.push((p.clone(), Outcome::Removed));
            }
            Err(e) => out.push((p.clone(), Outcome::Failed(e.to_string()))),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use backer_connector::MountedConnector;
    use backer_store::FileRec;

    #[test]
    fn removes_only_listed_files() {
        let d = tempfile::tempdir().unwrap();
        let c = MountedConnector::new(d.path());
        c.init_marker().unwrap();
        std::fs::create_dir_all(d.path().join("m")).unwrap();
        std::fs::write(d.path().join("m/gone"), b"1").unwrap();
        std::fs::write(d.path().join("m/live"), b"2").unwrap();
        let s = Store::open_memory().unwrap();
        let rec = FileRec { size: 1, mtime: 1, hash: "h".into() };
        s.upsert_file(1, "m/gone", &rec, 1).unwrap();
        s.upsert_file(1, "m/live", &rec, 2).unwrap();
        s.flag_missing(1, "m", 2, 5).unwrap();
        let r = remove_reviewed(&s, &c, 1, &["m/gone".into(), "m/live".into()]).unwrap();
        assert_eq!(r[0].1, Outcome::Removed);
        assert_eq!(r[1].1, Outcome::NotListed);
        assert!(!d.path().join("m/gone").exists());
        assert!(d.path().join("m/live").exists());
    }

    #[test]
    fn refuses_when_destination_not_mounted() {
        let d = tempfile::tempdir().unwrap();
        let c = MountedConnector::new(d.path()); // no marker
        let s = Store::open_memory().unwrap();
        assert!(remove_reviewed(&s, &c, 1, &["x".into()]).is_err());
    }
}
