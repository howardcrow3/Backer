//! MCP server logic (JSON-RPC 2.0, protocol 2024-11-05). Transport and client approval live in the agent.
//! This crate depends only on backer-core: it cannot reach the engine, the store's writers or the removal code.
use backer_core::{JobId, LogQuery, Schedule};
use serde_json::{json, Value};

/// What a connected assistant is allowed to do. It can forget a whole job (`delete_job`, which never touches NAS files)
/// and ask the user to set up a new one (`create_job`: the user picks the folders, NAS and times in the app).
/// It has no way to remove NAS files or to choose or edit folders and destinations.
pub trait Control: Send + Sync {
    fn list_jobs(&self) -> Result<Value, String>;
    fn get_job(&self, id: JobId) -> Result<Value, String>;
    fn list_runs(&self, id: JobId, limit: u32) -> Result<Value, String>;
    /// One page of a run's file log: `{total, rows}`.
    fn get_run_log(&self, run_id: i64, q: &LogQuery) -> Result<Value, String>;
    /// Totals, per-folder numbers, log counts and problems grouped by cause for one run.
    fn get_run_summary(&self, run_id: i64) -> Result<Value, String>;
    fn test_connection(&self, id: JobId) -> Result<Value, String>;
    fn run_job(&self, id: JobId) -> Result<Value, String>;
    fn set_schedule(&self, id: JobId, schedule: Schedule) -> Result<Value, String>;
    fn pause_job(&self, id: JobId) -> Result<Value, String>;
    fn resume_job(&self, id: JobId) -> Result<Value, String>;
    /// Removes the job and its history. `job_name` must match the job's exact name. NAS files are never touched.
    fn delete_job(&self, id: JobId, job_name: &str) -> Result<Value, String>;
    /// Asks the user, in the app, to set up a new job. The assistant supplies at most a suggested name;
    /// the user chooses the source folders, the NAS destination and the schedule. Nothing is created until the user confirms.
    fn create_job(&self, suggested_name: Option<&str>) -> Result<Value, String>;
}

pub const TOOL_NAMES: [&str; 12] = [
    "list_jobs", "get_job", "list_runs", "get_run_summary", "get_run_log", "test_connection", "run_job", "set_schedule", "pause_job", "resume_job", "delete_job", "create_job",
];

/// Reads kind, search, offset and limit from tool or UI arguments. The limit is capped at 1000 rows.
pub fn log_query(a: &Value, default_limit: u32) -> LogQuery {
    LogQuery {
        kind: a.get("kind").and_then(Value::as_str).map(str::to_string),
        search: a.get("search").and_then(Value::as_str).map(str::to_string),
        offset: a.get("offset").and_then(Value::as_u64).map(|o| o.min(u32::MAX as u64) as u32).unwrap_or(0),
        limit: a.get("limit").and_then(Value::as_u64).map(|l| l.min(1000) as u32).unwrap_or(default_limit),
    }
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({"name": name, "description": desc, "inputSchema": {"type": "object", "properties": props, "required": required}})
}

pub fn tool_list() -> Value {
    let id = json!({"job_id": {"type": "integer"}});
    json!([
        tool("list_jobs", "List backup jobs with status and next run.", json!({}), &[]),
        tool("get_job", "Show one job's folders, destination and schedule.", id.clone(), &["job_id"]),
        tool("list_runs", "Recent runs of a job.", json!({"job_id": {"type":"integer"}, "limit": {"type":"integer"}}), &["job_id"]),
        tool("get_run_summary", "How one run went: totals, per-folder numbers, and problems grouped by cause. Start here before reading the file log.", json!({"run_id": {"type":"integer"}}), &["run_id"]),
        tool("get_run_log", "File-level log of one run, a page at a time. Returns {total, rows}. Filter with kind (new, updated, skipped, warning, error, deleted_flagged, run, or attention for warnings, errors and skips) and search (text in the path).",
            json!({"run_id": {"type":"integer"}, "kind": {"type":"string"}, "search": {"type":"string"}, "offset": {"type":"integer"}, "limit": {"type":"integer"}}), &["run_id"]),
        tool("test_connection", "Check the NAS destination of a job is reachable.", id.clone(), &["job_id"]),
        tool("run_job", "Start a job now.", id.clone(), &["job_id"]),
        tool("set_schedule", "Change a job's schedule. kind: manual | every_hours | daily | weekly.", json!({
            "job_id": {"type":"integer"}, "kind": {"type":"string", "enum": ["manual","every_hours","daily","weekly"]},
            "hours": {"type":"integer"}, "hour": {"type":"integer"}, "minute": {"type":"integer"},
            "days": {"type":"array", "items": {"type":"integer", "minimum":1, "maximum":7}}}), &["job_id","kind"]),
        tool("pause_job", "Pause scheduled runs of a job.", id.clone(), &["job_id"]),
        tool("resume_job", "Resume scheduled runs of a job.", id, &["job_id"]),
        tool("delete_job", "Delete a backup job and its history so it never runs again. Files already backed up on the NAS are NOT deleted. Cannot be undone. job_name must be the job's exact name (see get_job). Fails while the job is running.",
            json!({"job_id": {"type":"integer"}, "job_name": {"type":"string"}}), &["job_id","job_name"]),
        tool("create_job", "Ask the user to set up a new backup job. This does not create anything by itself: Backer shows the user a prompt and the USER chooses which folders to back up, which NAS folder to use and the schedule times. Do not ask for or pass folders, paths or times; you can only suggest a name. Returns immediately with status waiting_for_user; use list_jobs later to see whether the job was created.",
            json!({"suggested_name": {"type":"string"}}), &[]),
    ])
}

fn parse_schedule(a: &Value) -> Result<Schedule, String> {
    let n = |k: &str| a.get(k).and_then(Value::as_u64);
    match a.get("kind").and_then(Value::as_str) {
        Some("manual") => Ok(Schedule::Manual),
        Some("every_hours") => {
            let h = n("hours").ok_or("hours required")?;
            if !(1..=168).contains(&h) { return Err("hours must be 1-168".into()); }
            Ok(Schedule::EveryHours { hours: h as u32 })
        }
        Some(k @ ("daily" | "weekly")) => {
            let (h, m) = (n("hour").ok_or("hour required")?, n("minute").unwrap_or(0));
            if h > 23 || m > 59 { return Err("bad time".into()); }
            if k == "daily" {
                Ok(Schedule::Daily { hour: h as u8, minute: m as u8 })
            } else {
                let days: Vec<u8> = a.get("days").and_then(Value::as_array).ok_or("days required")?
                    .iter().filter_map(|d| d.as_u64()).map(|d| d as u8).collect();
                if days.is_empty() || days.iter().any(|d| !(1..=7).contains(d)) { return Err("days must be 1-7".into()); }
                Ok(Schedule::Weekly { days, hour: h as u8, minute: m as u8 })
            }
        }
        _ => Err("unknown kind".into()),
    }
}

fn call_tool(c: &dyn Control, name: &str, a: &Value) -> Result<Value, String> {
    let job = || a.get("job_id").and_then(Value::as_i64).ok_or_else(|| "job_id required".to_string());
    let lim = |d: u32| a.get("limit").and_then(Value::as_u64).map(|l| l.min(1000) as u32).unwrap_or(d);
    match name {
        "list_jobs" => c.list_jobs(),
        "get_job" => c.get_job(job()?),
        "list_runs" => c.list_runs(job()?, lim(20)),
        "get_run_summary" => c.get_run_summary(a.get("run_id").and_then(Value::as_i64).ok_or("run_id required")?),
        "get_run_log" => c.get_run_log(a.get("run_id").and_then(Value::as_i64).ok_or("run_id required")?, &log_query(a, 200)),
        "test_connection" => c.test_connection(job()?),
        "run_job" => c.run_job(job()?),
        "set_schedule" => c.set_schedule(job()?, parse_schedule(a)?),
        "pause_job" => c.pause_job(job()?),
        "resume_job" => c.resume_job(job()?),
        "delete_job" => c.delete_job(job()?, a.get("job_name").and_then(Value::as_str).ok_or("job_name required")?),
        "create_job" => c.create_job(a.get("suggested_name").and_then(Value::as_str)),
        _ => Err(format!("unknown tool: {name}")),
    }
}

/// Handle one JSON-RPC message. Returns None for notifications.
pub fn handle(c: &dyn Control, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "backer", "version": env!("CARGO_PKG_VERSION")}
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_list()}),
        "tools/call" => {
            let p = req.get("params").cloned().unwrap_or(Value::Null);
            let name = p.get("name").and_then(Value::as_str).unwrap_or("");
            let args = p.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call_tool(c, name, &args) {
                Ok(v) => json!({"content": [{"type": "text", "text": v.to_string()}]}),
                Err(e) => json!({"isError": true, "content": [{"type": "text", "text": e}]}),
            }
        }
        _ if id.is_none() => return None, // notification such as notifications/initialized
        _ => return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})),
    };
    id.map(|id| json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake(Mutex<Vec<String>>);
    impl Control for Fake {
        fn list_jobs(&self) -> Result<Value, String> { Ok(json!([])) }
        fn get_job(&self, id: JobId) -> Result<Value, String> { Ok(json!({"id": id})) }
        fn list_runs(&self, _: JobId, l: u32) -> Result<Value, String> { Ok(json!({"limit": l})) }
        fn get_run_log(&self, _: i64, q: &LogQuery) -> Result<Value, String> { Ok(json!({"total": 0, "rows": [], "kind": q.kind, "limit": q.limit, "offset": q.offset})) }
        fn get_run_summary(&self, id: i64) -> Result<Value, String> { Ok(json!({"run": id})) }
        fn test_connection(&self, _: JobId) -> Result<Value, String> { Ok(json!({"ok": true})) }
        fn run_job(&self, id: JobId) -> Result<Value, String> { self.0.lock().unwrap().push(format!("run {id}")); Ok(json!({"started": true})) }
        fn set_schedule(&self, id: JobId, s: Schedule) -> Result<Value, String> { self.0.lock().unwrap().push(format!("sched {id} {s:?}")); Ok(json!({})) }
        fn pause_job(&self, _: JobId) -> Result<Value, String> { Ok(json!({})) }
        fn resume_job(&self, _: JobId) -> Result<Value, String> { Ok(json!({})) }
        fn delete_job(&self, id: JobId, n: &str) -> Result<Value, String> { self.0.lock().unwrap().push(format!("delete {id} {n}")); Ok(json!({"deleted": true})) }
        fn create_job(&self, n: Option<&str>) -> Result<Value, String> { self.0.lock().unwrap().push(format!("create {n:?}")); Ok(json!({"status": "waiting_for_user"})) }
    }

    #[test]
    fn exactly_the_twelve_tools_create_job_takes_no_paths_or_times() {
        let names: Vec<String> = tool_list().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, TOOL_NAMES);
        // No file removal, no review-list actions, no destination editing. The one delete forgets a job, never NAS files.
        assert!(!names.iter().any(|n| n.contains("remove") || n.contains("keep") || n.contains("destination") || n.contains("update") || n.contains("edit")));
        // create_job can only suggest a name: it has no way to pass folders, paths, destinations or times.
        let cj = tool_list().as_array().unwrap().iter().find(|t| t["name"] == "create_job").unwrap().clone();
        let props: Vec<&String> = cj["inputSchema"]["properties"].as_object().unwrap().keys().collect();
        assert_eq!(props, ["suggested_name"]);
        assert_eq!(names.iter().filter(|n| n.contains("delete")).collect::<Vec<_>>(), ["delete_job"]);
    }

    #[test]
    fn create_job_only_forwards_a_suggested_name() {
        let f = Fake::default();
        let call = |args: Value| handle(&f, &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"create_job","arguments":args}})).unwrap();
        assert!(call(json!({"suggested_name":"Photos","folders":[{"source":"/etc","dest":"x"}],"dest_root":"/Volumes/b","kind":"daily"}))["result"].get("isError").is_none());
        assert!(call(json!({}))["result"].get("isError").is_none());
        assert_eq!(*f.0.lock().unwrap(), ["create Some(\"Photos\")", "create None"]);
    }

    #[test]
    fn call_and_errors() {
        let f = Fake::default();
        let r = handle(&f, &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"run_job","arguments":{"job_id":7}}})).unwrap();
        assert_eq!(r["result"]["content"][0]["text"], "{\"started\":true}");
        assert_eq!(f.0.lock().unwrap()[0], "run 7");
        let r = handle(&f, &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delete_files","arguments":{}}})).unwrap();
        assert_eq!(r["result"]["isError"], true);
        let r = handle(&f, &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_job","arguments":{}}})).unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(handle(&f, &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).is_none());
    }

    #[test]
    fn schedule_validation() {
        let f = Fake::default();
        let bad = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"set_schedule","arguments":{"job_id":1,"kind":"daily","hour":25}}});
        assert_eq!(handle(&f, &bad).unwrap()["result"]["isError"], true);
        let ok = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"set_schedule","arguments":{"job_id":1,"kind":"weekly","hour":3,"days":[1,5]}}});
        assert!(handle(&f, &ok).unwrap()["result"].get("isError").is_none());
    }
}
