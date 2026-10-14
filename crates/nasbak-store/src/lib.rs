//! SQLite state: jobs, runs, per-file records, and the deleted-files review list.
//! One `Store` owns the connection; the agent is the single writer.
use nasbak_core::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Store {
    conn: Connection,
}

fn se(e: rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRec {
    pub size: u64,
    pub mtime: i64,
    pub hash: String,
}

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY, job_id INTEGER NOT NULL, started INTEGER NOT NULL, finished INTEGER,
  status TEXT NOT NULL, stats TEXT NOT NULL, message TEXT);
CREATE TABLE IF NOT EXISTS run_log (run_id INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, path TEXT NOT NULL, detail TEXT);
CREATE INDEX IF NOT EXISTS run_log_run ON run_log(run_id);
CREATE TABLE IF NOT EXISTS files (
  job_id INTEGER NOT NULL, dest_path TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
  hash TEXT NOT NULL, seen_run INTEGER NOT NULL, PRIMARY KEY (job_id, dest_path)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deleted (
  job_id INTEGER NOT NULL, dest_path TEXT NOT NULL, size INTEGER NOT NULL, detected INTEGER NOT NULL,
  PRIMARY KEY (job_id, dest_path)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS kept (job_id INTEGER NOT NULL, dest_path TEXT NOT NULL, PRIMARY KEY (job_id, dest_path)) WITHOUT ROWID;
";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(se)?;
        conn.execute_batch(SCHEMA).map_err(se)?;
        Ok(Self { conn })
    }
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(se)?;
        conn.execute_batch(SCHEMA).map_err(se)?;
        Ok(Self { conn })
    }

    // ---- jobs
    pub fn save_job(&self, job: &mut Job) -> Result<JobId> {
        job.validate()?;
        if job.id == 0 {
            self.conn.execute("INSERT INTO jobs(json) VALUES ('')", []).map_err(se)?;
            job.id = self.conn.last_insert_rowid();
        }
        let json = serde_json::to_string(job).map_err(|e| Error::Store(e.to_string()))?;
        self.conn.execute("INSERT INTO jobs(id,json) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET json=?2", params![job.id, json]).map_err(se)?;
        Ok(job.id)
    }
    pub fn get_job(&self, id: JobId) -> Result<Option<Job>> {
        let j: Option<String> = self.conn.query_row("SELECT json FROM jobs WHERE id=?1", [id], |r| r.get(0)).optional().map_err(se)?;
        j.map(|s| serde_json::from_str(&s).map_err(|e| Error::Store(e.to_string()))).transpose()
    }
    pub fn list_jobs(&self) -> Result<Vec<Job>> {
        let mut st = self.conn.prepare("SELECT json FROM jobs ORDER BY id").map_err(se)?;
        let rows = st.query_map([], |r| r.get::<_, String>(0)).map_err(se)?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r.map_err(se)?).map_err(|e| Error::Store(e.to_string()))?);
        }
        Ok(out)
    }

    // ---- runs
    pub fn begin_run(&self, job_id: JobId, now: i64) -> Result<i64> {
        self.conn.execute("INSERT INTO runs(job_id,started,status,stats) VALUES(?1,?2,'running','{}')", params![job_id, now]).map_err(se)?;
        Ok(self.conn.last_insert_rowid())
    }
    pub fn finish_run(&self, run: i64, now: i64, status: RunStatus, stats: &RunStats, message: Option<&str>) -> Result<()> {
        let s = serde_json::to_string(stats).map_err(|e| Error::Store(e.to_string()))?;
        self.conn.execute("UPDATE runs SET finished=?2,status=?3,stats=?4,message=?5 WHERE id=?1", params![run, now, status.as_str(), s, message]).map_err(se)?;
        Ok(())
    }
    /// Runs left "running" by a crash are marked failed at startup.
    pub fn recover_interrupted(&self, now: i64) -> Result<usize> {
        self.conn.execute("UPDATE runs SET status='failed',finished=?1,message='interrupted' WHERE status='running'", [now]).map_err(se)
    }
    pub fn list_runs(&self, job_id: JobId, limit: u32) -> Result<Vec<RunSummary>> {
        let mut st = self.conn.prepare("SELECT id,job_id,started,finished,status,stats,message FROM runs WHERE job_id=?1 ORDER BY id DESC LIMIT ?2").map_err(se)?;
        let rows = st.query_map(params![job_id, limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<String>>(6)?))
        }).map_err(se)?;
        let mut out = vec![];
        for r in rows {
            let (id, job_id, started, finished, status, stats, message) = r.map_err(se)?;
            out.push(RunSummary {
                id, job_id, started, finished,
                status: RunStatus::parse(&status).unwrap_or(RunStatus::Failed),
                stats: serde_json::from_str(&stats).unwrap_or_default(),
                message,
            });
        }
        Ok(out)
    }
    pub fn last_run_started(&self, job_id: JobId) -> Result<Option<i64>> {
        self.conn.query_row("SELECT MAX(started) FROM runs WHERE job_id=?1", [job_id], |r| r.get(0)).map_err(se)
    }
    pub fn log(&self, run: i64, seq: u64, kind: &str, path: &str, detail: Option<&str>) -> Result<()> {
        self.conn.execute("INSERT INTO run_log VALUES(?1,?2,?3,?4,?5)", params![run, seq as i64, kind, path, detail]).map_err(se)?;
        Ok(())
    }
    pub fn run_log(&self, run: i64, limit: u32) -> Result<Vec<(String, String, Option<String>)>> {
        let mut st = self.conn.prepare("SELECT kind,path,detail FROM run_log WHERE run_id=?1 ORDER BY seq LIMIT ?2").map_err(se)?;
        let rows = st.query_map(params![run, limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(se)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(se)
    }

    // ---- files
    pub fn get_file(&self, job: JobId, dest_path: &str) -> Result<Option<FileRec>> {
        self.conn.query_row("SELECT size,mtime,hash FROM files WHERE job_id=?1 AND dest_path=?2", params![job, dest_path], |r| {
            Ok(FileRec { size: r.get::<_, i64>(0)? as u64, mtime: r.get(1)?, hash: r.get(2)? })
        }).optional().map_err(se)
    }
    pub fn upsert_file(&self, job: JobId, dest_path: &str, rec: &FileRec, run: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO files VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(job_id,dest_path) DO UPDATE SET size=?3,mtime=?4,hash=?5,seen_run=?6",
            params![job, dest_path, rec.size as i64, rec.mtime, rec.hash, run]).map_err(se)?;
        self.conn.execute("DELETE FROM deleted WHERE job_id=?1 AND dest_path=?2", params![job, dest_path]).map_err(se)?;
        Ok(())
    }
    pub fn mark_seen(&self, job: JobId, dest_path: &str, run: i64) -> Result<()> {
        self.conn.execute("UPDATE files SET seen_run=?3 WHERE job_id=?1 AND dest_path=?2", params![job, dest_path, run]).map_err(se)?;
        self.conn.execute("DELETE FROM deleted WHERE job_id=?1 AND dest_path=?2", params![job, dest_path]).map_err(se)?;
        Ok(())
    }
    /// After a complete scan of `dest_prefix`, files not seen this run move to the review list.
    /// Never touches the NAS. Files the user chose to keep are not listed again.
    pub fn flag_missing(&self, job: JobId, dest_prefix: &str, run: i64, now: i64) -> Result<u64> {
        let like = format!("{}/%", escape_like(dest_prefix));
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO deleted(job_id,dest_path,size,detected)
             SELECT job_id,dest_path,size,?4 FROM files
             WHERE job_id=?1 AND dest_path LIKE ?2 ESCAPE '\\' AND seen_run < ?3
               AND dest_path NOT IN (SELECT dest_path FROM kept WHERE job_id=?1)",
            params![job, like, run, now]).map_err(se)?;
        Ok(n as u64)
    }

    // ---- deleted-file review
    pub fn list_deleted(&self, job: Option<JobId>) -> Result<Vec<DeletedFile>> {
        let mut st = self.conn.prepare("SELECT job_id,dest_path,size,detected FROM deleted WHERE (?1 IS NULL OR job_id=?1) ORDER BY detected,dest_path").map_err(se)?;
        let rows = st.query_map([job], |r| Ok(DeletedFile { job_id: r.get(0)?, dest_path: r.get(1)?, size: r.get::<_, i64>(2)? as u64, detected: r.get(3)? })).map_err(se)?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(se)
    }
    pub fn keep_deleted(&self, job: JobId, dest_path: &str) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO kept VALUES(?1,?2)", params![job, dest_path]).map_err(se)?;
        self.conn.execute("DELETE FROM deleted WHERE job_id=?1 AND dest_path=?2", params![job, dest_path]).map_err(se)?;
        Ok(())
    }
    /// Called by nasbak-removal after the file is gone from the NAS.
    pub fn forget_file(&self, job: JobId, dest_path: &str) -> Result<()> {
        self.conn.execute("DELETE FROM deleted WHERE job_id=?1 AND dest_path=?2", params![job, dest_path]).map_err(se)?;
        self.conn.execute("DELETE FROM files WHERE job_id=?1 AND dest_path=?2", params![job, dest_path]).map_err(se)?;
        Ok(())
    }
    pub fn is_listed_deleted(&self, job: JobId, dest_path: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM deleted WHERE job_id=?1 AND dest_path=?2", params![job, dest_path], |r| r.get(0)).map_err(se)?;
        Ok(n > 0)
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        Job { id: 0, name: "t".into(), folders: vec![FolderPair { source: "/a".into(), dest: "mac/a".into() }],
            dest_root: "/Volumes/x".into(), schedule: Schedule::Manual, paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None }
    }

    #[test]
    fn job_roundtrip_and_validation() {
        let s = Store::open_memory().unwrap();
        let mut j = job();
        let id = s.save_job(&mut j).unwrap();
        assert_eq!(s.get_job(id).unwrap().unwrap(), j);
        let mut bad = job();
        bad.folders.push(FolderPair { source: "/b".into(), dest: "mac/a/sub".into() });
        assert!(s.save_job(&mut bad).is_err());
    }

    #[test]
    fn missing_files_are_flagged_not_deleted_and_keep_sticks() {
        let s = Store::open_memory().unwrap();
        let rec = FileRec { size: 3, mtime: 1, hash: "h".into() };
        s.upsert_file(1, "mac/a/x", &rec, 1).unwrap();
        s.upsert_file(1, "mac/a/y", &rec, 1).unwrap();
        s.upsert_file(1, "mac/ab/z", &rec, 1).unwrap(); // different folder with similar prefix
        s.mark_seen(1, "mac/a/x", 2).unwrap();
        assert_eq!(s.flag_missing(1, "mac/a", 2, 100).unwrap(), 1);
        assert_eq!(s.list_deleted(None).unwrap()[0].dest_path, "mac/a/y");
        s.keep_deleted(1, "mac/a/y").unwrap();
        s.mark_seen(1, "mac/a/x", 3).unwrap();
        assert_eq!(s.flag_missing(1, "mac/a", 3, 101).unwrap(), 0);
        assert!(s.list_deleted(None).unwrap().iter().all(|d| d.dest_path != "mac/a/y"));
    }

    #[test]
    fn reappearing_file_leaves_review_list() {
        let s = Store::open_memory().unwrap();
        let rec = FileRec { size: 3, mtime: 1, hash: "h".into() };
        s.upsert_file(1, "m/a/x", &rec, 1).unwrap();
        s.flag_missing(1, "m/a", 2, 5).unwrap();
        assert!(s.is_listed_deleted(1, "m/a/x").unwrap());
        s.mark_seen(1, "m/a/x", 3).unwrap();
        assert!(!s.is_listed_deleted(1, "m/a/x").unwrap());
    }

    #[test]
    fn interrupted_runs_recovered() {
        let s = Store::open_memory().unwrap();
        let r = s.begin_run(1, 10).unwrap();
        assert_eq!(s.recover_interrupted(20).unwrap(), 1);
        assert_eq!(s.list_runs(1, 5).unwrap()[0].id, r);
        assert_eq!(s.list_runs(1, 5).unwrap()[0].status, RunStatus::Failed);
    }
}
