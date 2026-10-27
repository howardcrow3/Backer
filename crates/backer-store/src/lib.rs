//! SQLite state: jobs, runs, per-file records, and the deleted-files review list.
//! One `Store` owns the connection; the agent is the single writer.
use backer_core::*;
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
CREATE TABLE IF NOT EXISTS run_log (run_id INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, path TEXT NOT NULL, detail TEXT,
  ts INTEGER NOT NULL DEFAULT 0, size INTEGER);
CREATE INDEX IF NOT EXISTS run_log_run ON run_log(run_id);
CREATE TABLE IF NOT EXISTS run_folders (
  run_id INTEGER NOT NULL, source TEXT NOT NULL, scanned INTEGER NOT NULL, copied INTEGER NOT NULL, bytes INTEGER NOT NULL,
  issues INTEGER NOT NULL, duration_ms INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS run_folders_run ON run_folders(run_id);
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
        Self::init(Connection::open(path).map_err(se)?)
    }
    pub fn open_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory().map_err(se)?)
    }
    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA).map_err(se)?;
        // Databases from before v0.4 have a run_log without timestamps and sizes.
        for (col, ddl) in [("ts", "INTEGER NOT NULL DEFAULT 0"), ("size", "INTEGER")] {
            let has: bool = conn.query_row("SELECT COUNT(*) FROM pragma_table_info('run_log') WHERE name=?1", [col], |r| r.get::<_, i64>(0)).map_err(se)? > 0;
            if !has {
                conn.execute(&format!("ALTER TABLE run_log ADD COLUMN {col} {ddl}"), []).map_err(se)?;
            }
        }
        conn.execute_batch("CREATE INDEX IF NOT EXISTS run_log_kind ON run_log(run_id, kind);").map_err(se)?;
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
    /// Forgets a job and everything recorded about it (runs, logs, file records, review lists).
    /// Only the local database changes; files already on the NAS are not touched.
    pub fn delete_job(&self, id: JobId) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(se)?;
        tx.execute("DELETE FROM run_log WHERE run_id IN (SELECT id FROM runs WHERE job_id=?1)", [id]).map_err(se)?;
        tx.execute("DELETE FROM run_folders WHERE run_id IN (SELECT id FROM runs WHERE job_id=?1)", [id]).map_err(se)?;
        for t in ["runs", "files", "deleted", "kept"] {
            tx.execute(&format!("DELETE FROM {t} WHERE job_id=?1"), [id]).map_err(se)?;
        }
        tx.execute("DELETE FROM jobs WHERE id=?1", [id]).map_err(se)?;
        tx.commit().map_err(se)
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
    fn runs_where(&self, clause: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<RunSummary>> {
        let mut st = self.conn.prepare(&format!("SELECT id,job_id,started,finished,status,stats,message FROM runs WHERE {clause}")).map_err(se)?;
        let rows = st.query_map(args, |r| {
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
    pub fn list_runs(&self, job_id: JobId, limit: u32) -> Result<Vec<RunSummary>> {
        self.runs_where("job_id=?1 ORDER BY id DESC LIMIT ?2", &[&job_id, &limit])
    }
    pub fn get_run(&self, run: i64) -> Result<Option<RunSummary>> {
        Ok(self.runs_where("id=?1", &[&run])?.into_iter().next())
    }
    pub fn last_run_started(&self, job_id: JobId) -> Result<Option<i64>> {
        self.conn.query_row("SELECT MAX(started) FROM runs WHERE job_id=?1", [job_id], |r| r.get(0)).map_err(se)
    }
    /// Appends one row to a run's log. `seq` must increase within a run so rows read back in the order they happened.
    pub fn log(&self, run: i64, row: &LogRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO run_log(run_id,seq,kind,path,detail,ts,size) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![run, row.seq as i64, row.kind, row.path, row.detail, row.ts, row.size.map(|s| s as i64)]).map_err(se)?;
        Ok(())
    }
    fn log_filter(q: &LogQuery) -> (String, Vec<String>) {
        let mut sql = String::from("run_id=?1");
        let mut args = vec![];
        match q.kind.as_deref() {
            None | Some("") | Some("all") => {}
            Some("attention") => sql.push_str(" AND kind IN ('warning','error','skipped')"),
            Some("copied") => sql.push_str(" AND kind IN ('new','updated','copied')"),
            Some(k) => {
                args.push(k.to_string());
                sql.push_str(&format!(" AND kind=?{}", args.len() + 1));
            }
        }
        if let Some(t) = q.search.as_deref().filter(|t| !t.is_empty()) {
            args.push(format!("%{}%", escape_like(t)));
            sql.push_str(&format!(" AND path LIKE ?{} ESCAPE '\\'", args.len() + 1));
        }
        (sql, args)
    }
    /// One page of a run's log in the order things happened, plus how many rows match in all.
    pub fn run_log(&self, run: i64, q: &LogQuery) -> Result<LogPage> {
        let (filter, args) = Self::log_filter(q);
        let mut all: Vec<rusqlite::types::Value> = vec![run.into()];
        all.extend(args.iter().cloned().map(Into::into));
        let total: i64 = self.conn.query_row(&format!("SELECT COUNT(*) FROM run_log WHERE {filter}"), rusqlite::params_from_iter(all.iter()), |r| r.get(0)).map_err(se)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT seq,ts,kind,path,detail,size FROM run_log WHERE {filter} ORDER BY seq LIMIT {} OFFSET {}", q.limit.clamp(1, 1000), q.offset)).map_err(se)?;
        let rows = st.query_map(rusqlite::params_from_iter(all.iter()), |r| Ok(LogRow {
            seq: r.get::<_, i64>(0)? as u64, ts: r.get(1)?, kind: r.get(2)?, path: r.get(3)?, detail: r.get(4)?, size: r.get::<_, Option<i64>>(5)?.map(|s| s as u64),
        })).map_err(se)?;
        Ok(LogPage { total: total as u64, rows: rows.collect::<std::result::Result<Vec<_>, _>>().map_err(se)? })
    }
    pub fn save_folder_stat(&self, run: i64, f: &FolderStat) -> Result<()> {
        self.conn.execute("INSERT INTO run_folders VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![run, f.source, f.scanned as i64, f.copied as i64, f.bytes as i64, f.issues as i64, f.duration_ms as i64]).map_err(se)?;
        Ok(())
    }
    /// Everything the run screen needs in one go: totals, per-folder numbers, log counts and the problems grouped by cause.
    pub fn run_report(&self, run: i64) -> Result<RunReport> {
        let summary = self.get_run(run)?.ok_or_else(|| Error::Store("run not found".into()))?;
        let mut st = self.conn.prepare("SELECT source,scanned,copied,bytes,issues,duration_ms FROM run_folders WHERE run_id=?1 ORDER BY rowid").map_err(se)?;
        let folders = st.query_map([run], |r| Ok(FolderStat {
            source: r.get(0)?, scanned: r.get::<_, i64>(1)? as u64, copied: r.get::<_, i64>(2)? as u64, bytes: r.get::<_, i64>(3)? as u64,
            issues: r.get::<_, i64>(4)? as u64, duration_ms: r.get::<_, i64>(5)? as u64 })).map_err(se)?.collect::<std::result::Result<Vec<_>, _>>().map_err(se)?;
        let mut st = self.conn.prepare("SELECT kind,COUNT(*) FROM run_log WHERE run_id=?1 GROUP BY kind").map_err(se)?;
        let counts: std::collections::BTreeMap<String, u64> = st.query_map([run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))
            .map_err(se)?.collect::<std::result::Result<_, _>>().map_err(se)?;
        let mut st = self.conn.prepare(
            "SELECT kind,COALESCE(detail,''),COUNT(*) FROM run_log WHERE run_id=?1 AND kind IN ('error','warning','skipped') GROUP BY kind,COALESCE(detail,'')
             ORDER BY CASE kind WHEN 'error' THEN 0 WHEN 'warning' THEN 1 ELSE 2 END, COUNT(*) DESC").map_err(se)?;
        let groups = st.query_map([run], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as u64))).map_err(se)?
            .collect::<std::result::Result<Vec<_>, _>>().map_err(se)?;
        let mut issues = vec![];
        for (kind, cause, count) in groups {
            let mut st = self.conn.prepare("SELECT path FROM run_log WHERE run_id=?1 AND kind=?2 AND COALESCE(detail,'')=?3 ORDER BY seq LIMIT 5").map_err(se)?;
            let sample = st.query_map(params![run, kind, cause], |r| r.get::<_, String>(0)).map_err(se)?.collect::<std::result::Result<Vec<_>, _>>().map_err(se)?;
            issues.push(IssueGroup { kind, cause, count, sample });
        }
        let log_rows = counts.values().sum();
        Ok(RunReport { run: summary, folders, counts, issues, log_rows })
    }
    /// Keeps the file-level log of each job's newest `keep` runs and drops older ones. Run summaries and per-folder
    /// numbers stay, so history survives; only the long per-file rows go. Returns the rows removed.
    pub fn prune_logs(&self, keep: u32) -> Result<usize> {
        let old = "SELECT id FROM runs r WHERE (SELECT COUNT(*) FROM runs r2 WHERE r2.job_id=r.job_id AND r2.id>r.id) >= ?1 AND status<>'running'";
        self.conn.execute(&format!("DELETE FROM run_log WHERE run_id IN ({old})"), [keep]).map_err(se)
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
    /// Flags files that were on the source last time but not this run. Returns the NAS paths newly flagged.
    pub fn flag_missing(&self, job: JobId, dest_prefix: &str, run: i64, now: i64) -> Result<Vec<String>> {
        let like = format!("{}/%", escape_like(dest_prefix));
        let cond = "job_id=?1 AND dest_path LIKE ?2 ESCAPE '\\' AND seen_run < ?3
               AND dest_path NOT IN (SELECT dest_path FROM kept WHERE job_id=?1)
               AND dest_path NOT IN (SELECT dest_path FROM deleted WHERE job_id=?1)";
        let mut st = self.conn.prepare(&format!("SELECT dest_path FROM files WHERE {cond} ORDER BY dest_path")).map_err(se)?;
        let paths = st.query_map(params![job, like, run], |r| r.get::<_, String>(0)).map_err(se)?.collect::<std::result::Result<Vec<_>, _>>().map_err(se)?;
        self.conn.execute(
            &format!("INSERT OR IGNORE INTO deleted(job_id,dest_path,size,detected) SELECT job_id,dest_path,size,?4 FROM files WHERE {cond}"),
            params![job, like, run, now]).map_err(se)?;
        Ok(paths)
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
    /// Called by backer-removal after the file is gone from the NAS.
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

    fn abs(p: &str) -> String {
        if cfg!(windows) { format!("C:{}", p.replace('/', "\\")) } else { p.to_string() }
    }

    fn job() -> Job {
        Job { id: 0, name: "t".into(), folders: vec![FolderPair { source: abs("/a"), dest: "mac/a".into() }],
            dest_root: "/Volumes/x".into(), schedule: Schedule::Manual, paused: false, catch_up: true, exclude_names: vec![], max_file_bytes: None }
    }

    #[test]
    fn job_roundtrip_and_validation() {
        let s = Store::open_memory().unwrap();
        let mut j = job();
        let id = s.save_job(&mut j).unwrap();
        assert_eq!(s.get_job(id).unwrap().unwrap(), j);
        let mut bad = job();
        bad.folders.push(FolderPair { source: abs("/b"), dest: "mac/a/sub".into() });
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
        assert_eq!(s.flag_missing(1, "mac/a", 2, 100).unwrap(), vec!["mac/a/y".to_string()]);
        assert_eq!(s.list_deleted(None).unwrap()[0].dest_path, "mac/a/y");
        s.keep_deleted(1, "mac/a/y").unwrap();
        s.mark_seen(1, "mac/a/x", 3).unwrap();
        assert!(s.flag_missing(1, "mac/a", 3, 101).unwrap().is_empty());
        assert!(s.list_deleted(None).unwrap().iter().all(|d| d.dest_path != "mac/a/y"));
    }

    #[test]
    fn deleting_a_job_removes_only_that_jobs_records() {
        let s = Store::open_memory().unwrap();
        let (mut a, mut b) = (job(), job());
        let (ia, ib) = (s.save_job(&mut a).unwrap(), s.save_job(&mut b).unwrap());
        let rec = FileRec { size: 3, mtime: 1, hash: "h".into() };
        for id in [ia, ib] {
            let run = s.begin_run(id, 10).unwrap();
            s.log(run, &row(1, "new", "x", None)).unwrap();
            s.upsert_file(id, "mac/a/x", &rec, run).unwrap();
            s.flag_missing(id, "mac/a", run + 1, 100).unwrap();
            s.keep_deleted(id, "mac/a/x").unwrap();
        }
        s.delete_job(ia).unwrap();
        assert!(s.get_job(ia).unwrap().is_none());
        assert!(s.list_runs(ia, 10).unwrap().is_empty());
        assert!(s.get_file(ia, "mac/a/x").unwrap().is_none());
        assert!(s.list_deleted(Some(ia)).unwrap().is_empty());
        // the other job is untouched
        assert!(s.get_job(ib).unwrap().is_some());
        assert_eq!(s.list_runs(ib, 10).unwrap().len(), 1);
        assert!(s.get_file(ib, "mac/a/x").unwrap().is_some());
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

    fn row(seq: u64, kind: &str, path: &str, detail: Option<&str>) -> LogRow {
        LogRow { seq, ts: 100 + seq as i64, kind: kind.into(), path: path.into(), detail: detail.map(Into::into), size: Some(seq * 10) }
    }

    #[test]
    fn log_pages_filters_and_searches() {
        let s = Store::open_memory().unwrap();
        let run = s.begin_run(1, 10).unwrap();
        for i in 0..30u64 {
            s.log(run, &row(i, if i % 10 == 0 { "warning" } else { "new" }, &format!("dir/f{i}_100%.txt"), Some("Permission denied"))).unwrap();
        }
        let all = s.run_log(run, &LogQuery { limit: 10, ..Default::default() }).unwrap();
        assert_eq!((all.total, all.rows.len(), all.rows[0].seq), (30, 10, 0));
        assert_eq!(all.rows[0].ts, 100);
        assert_eq!(all.rows[3].size, Some(30));
        let page3 = s.run_log(run, &LogQuery { limit: 10, offset: 20, ..Default::default() }).unwrap();
        assert_eq!(page3.rows[0].seq, 20);
        let warn = s.run_log(run, &LogQuery { kind: Some("warning".into()), limit: 10, ..Default::default() }).unwrap();
        assert_eq!(warn.total, 3);
        let att = s.run_log(run, &LogQuery { kind: Some("attention".into()), limit: 10, ..Default::default() }).unwrap();
        assert_eq!(att.total, 3);
        let found = s.run_log(run, &LogQuery { search: Some("F7_100%".into()), limit: 10, ..Default::default() }).unwrap();
        assert_eq!(found.rows.len(), 1, "search is case-insensitive and treats % literally");
        let both = s.run_log(run, &LogQuery { kind: Some("warning".into()), search: Some("f20".into()), limit: 10, ..Default::default() }).unwrap();
        assert_eq!(both.total, 1);
    }

    #[test]
    fn report_groups_problems_by_cause() {
        let s = Store::open_memory().unwrap();
        let run = s.begin_run(1, 10).unwrap();
        for (i, (k, p, d)) in [("warning", "a", "Permission denied"), ("warning", "b", "Permission denied"), ("skipped", "c", "larger than the size limit"),
            ("new", "d", ""), ("error", "", "Destination not ready")].into_iter().enumerate() {
            s.log(run, &row(i as u64, k, p, Some(d))).unwrap();
        }
        s.save_folder_stat(run, &FolderStat { source: "/x".into(), scanned: 5, copied: 1, bytes: 9, issues: 3, duration_ms: 1500 }).unwrap();
        s.finish_run(run, 20, RunStatus::Warnings, &RunStats { copied: 1, ..Default::default() }, None).unwrap();
        let r = s.run_report(run).unwrap();
        assert_eq!(r.run.stats.copied, 1);
        assert_eq!(r.folders, vec![FolderStat { source: "/x".into(), scanned: 5, copied: 1, bytes: 9, issues: 3, duration_ms: 1500 }]);
        assert_eq!(r.counts["warning"], 2);
        assert_eq!(r.log_rows, 5);
        let kinds: Vec<_> = r.issues.iter().map(|g| (g.kind.as_str(), g.count)).collect();
        assert_eq!(kinds, vec![("error", 1), ("warning", 2), ("skipped", 1)], "errors first, then warnings, then skips");
        assert_eq!(r.issues[1].sample, vec!["a", "b"]);
        assert!(s.run_report(999).is_err());
    }

    #[test]
    fn pruning_keeps_summaries_and_newest_logs() {
        let s = Store::open_memory().unwrap();
        let mut runs = vec![];
        for i in 0..5 {
            let r = s.begin_run(1, i).unwrap();
            s.log(r, &row(0, "new", "x", None)).unwrap();
            s.finish_run(r, i + 1, RunStatus::Ok, &RunStats::default(), None).unwrap();
            runs.push(r);
        }
        let other = s.begin_run(2, 9).unwrap();
        s.log(other, &row(0, "new", "x", None)).unwrap();
        s.finish_run(other, 10, RunStatus::Ok, &RunStats::default(), None).unwrap();
        assert_eq!(s.prune_logs(2).unwrap(), 3);
        let q = LogQuery { limit: 10, ..Default::default() };
        assert_eq!(s.run_log(runs[0], &q).unwrap().total, 0);
        assert_eq!(s.run_log(runs[3], &q).unwrap().total, 1);
        assert_eq!(s.run_log(other, &q).unwrap().total, 1, "each job keeps its own newest runs");
        assert_eq!(s.list_runs(1, 10).unwrap().len(), 5, "run summaries are never pruned");
    }

    #[test]
    fn old_database_gets_new_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch("CREATE TABLE run_log (run_id INTEGER NOT NULL, seq INTEGER NOT NULL, kind TEXT NOT NULL, path TEXT NOT NULL, detail TEXT);
                INSERT INTO run_log VALUES(1,0,'copied','old.txt',NULL);").unwrap();
        }
        let s = Store::open(&path).unwrap();
        let p = s.run_log(1, &LogQuery { limit: 10, ..Default::default() }).unwrap();
        assert_eq!((p.rows[0].ts, p.rows[0].size), (0, None));
        s.log(1, &row(1, "new", "n.txt", None)).unwrap();
        assert_eq!(s.run_log(1, &LogQuery { kind: Some("copied".into()), limit: 10, ..Default::default() }).unwrap().total, 2, "Copied covers old and new rows");
        drop(s);
        Store::open(&path).unwrap(); // reopening must not try to add the columns again
    }
}
