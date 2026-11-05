//! Durable job identities and restart-visible receipts. Jobs never resume implicitly after restart.

use crate::{EngineError, Job, Result, Session};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_RECORDS: usize = 4096;
const MAX_FILE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: u64,
    pub label: String,
    pub input_revision: u64,
    pub session_id: String,
    pub project_path: Option<String>,
    pub project_name: String,
    pub command: Option<String>,
    pub created_at: String,
    pub state: String,
    pub snapshot: Value,
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    next_id: u64,
    records: BTreeMap<u64, JobRecord>,
}
#[derive(Default)]
struct Store {
    ledger: Ledger,
    path: Option<PathBuf>,
    lock: Option<std::fs::File>,
    blocked: Option<String>,
    dirty: bool,
    last_flush: Option<web_time::Instant>,
}
#[derive(Clone, Default)]
pub struct JobRegistry(Arc<Mutex<Store>>);

fn err(message: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("job registry: {message}"))
}
fn timestamp() -> String {
    let secs = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap_or_default().as_secs();
    i64::try_from(secs).ok().and_then(|s| chrono::DateTime::from_timestamp(s, 0)).map(|t| t.to_rfc3339()).unwrap_or_else(|| "unavailable".into())
}
fn write(store: &Store) -> Result<()> {
    let Some(path) = &store.path else { return Ok(()) };
    let bytes = serde_json::to_vec(&store.ledger).map_err(err)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(err("history exceeds 64 MiB; forget finished jobs"));
    }
    filmcraft_format::atomic_write(path, &bytes).map_err(err)
}

impl JobRegistry {
    pub fn configure(&self, dir: &Path) -> Result<()> {
        let result = self.configure_inner(dir);
        if let Err(e) = &result {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).blocked = Some(e.to_string());
        }
        result
    }
    fn configure_inner(&self, dir: &Path) -> Result<()> {
        let mut store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if store.path.is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(dir).map_err(err)?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("jobs.lock")).map_err(err)?;
        #[cfg(not(target_arch = "wasm32"))]
        lock.try_lock().map_err(|e| err(format!("another app owns the job history, or it cannot be locked: {e}")))?;
        let path = dir.join("jobs.json");
        let mut ledger: Ledger = match std::fs::metadata(&path) {
            Ok(meta) if meta.len() <= MAX_FILE => serde_json::from_slice(&std::fs::read(&path).map_err(err)?).map_err(err)?,
            Ok(_) => return Err(err("history exceeds 64 MiB; file preserved")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(e) => return Err(err(e)),
        };
        if ledger.records.len() > MAX_RECORDS || ledger.records.iter().any(|(id, r)| !valid_record(*id, r)) {
            return Err(err("invalid job history; file preserved"));
        }
        ledger.next_id = ledger.next_id.max(ledger.records.keys().next_back().copied().unwrap_or(0));
        for record in ledger.records.values_mut() {
            if matches!(record.state.as_str(), "running" | "cancelling") {
                record.state = "interrupted".into();
                record.snapshot["finished"] = json!(true);
                record.snapshot["status"] = json!("Interrupted by application shutdown; inspect output before starting a new job");
                record.snapshot["result"] = json!({"error":"interrupted"});
            }
        }
        store.ledger = ledger;
        store.path = Some(path);
        store.lock = Some(lock);
        write(&store)
    }
    pub fn new_job(&self, s: &Session, label: String) -> Result<Job> {
        if label.len() > 4096 || serde_json::to_vec(&label).map_err(err)?.len() > 4096 {
            return Err(err("label exceeds 4096 bytes"));
        }
        let mut store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = &store.blocked {
            return Err(err(e));
        }
        if store.ledger.records.len() >= MAX_RECORDS {
            return Err(err("history capacity reached; forget finished jobs"));
        }
        let id = store.ledger.next_id.max(s.jobs.iter().map(|j| j.id).max().unwrap_or(0)).checked_add(1).ok_or_else(|| err("identity space exhausted"))?;
        let job = Job { id, label: label.clone(), progress: Default::default(), result: Default::default() };
        let record = JobRecord {
            id,
            label,
            input_revision: s.revision,
            session_id: s.operations.session_id.clone(),
            project_path: s.path.clone(),
            project_name: s.project.name.chars().take(1024).collect(),
            command: s.command_origin.clone(),
            created_at: timestamp(),
            state: "running".into(),
            snapshot: job.to_json(),
        };
        let previous_id = store.ledger.next_id;
        store.ledger.next_id = id;
        store.ledger.records.insert(id, record);
        if let Err(e) = write(&store) {
            store.ledger.records.remove(&id);
            store.ledger.next_id = previous_id;
            return Err(e);
        }
        Ok(job)
    }
    pub fn sync(&self, jobs: &[Job]) -> Result<()> {
        let mut store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        let mut terminal = false;
        for job in jobs {
            let Some(record) = store.ledger.records.get_mut(&job.id) else { continue };
            let mut snapshot = job.to_json();
            // to_json observes the result once under its mutex. Derive terminal state from
            // that same observation; a worker may finish immediately after this snapshot.
            let state = if snapshot["finished"] == true {
                if snapshot["result"].get("error").is_some() {
                    if job.progress.cancel.load(std::sync::atomic::Ordering::Relaxed) { "cancelled" } else { "failed" }
                } else {
                    "completed"
                }
            } else if job.progress.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                "cancelling"
            } else {
                "running"
            };
            if serde_json::to_vec(&snapshot).map_err(err)?.len() > 16 * 1024 {
                let result = if snapshot["finished"] != true {
                    Value::Null
                } else if let Some(error) = snapshot["result"]["error"].as_str() {
                    json!({"error":error.chars().take(1024).collect::<String>(),"errorTruncated":true})
                } else {
                    json!({"reportOmitted":true,"reason":"durable receipt exceeds 16 KiB; inspect output"})
                };
                snapshot["result"] = result;
                if let Some(text) = snapshot["status"].as_str() {
                    snapshot["status"] = json!(text.chars().take(512).collect::<String>());
                }
            }
            if record.snapshot != snapshot || record.state != state {
                record.snapshot = snapshot;
                record.state = state.into();
                terminal |= !matches!(state, "running" | "cancelling");
                changed = true;
            }
        }
        store.dirty |= changed;
        if store.dirty && (terminal || store.last_flush.is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1))) {
            write(&store)?;
            store.last_flush = Some(web_time::Instant::now());
            store.dirty = false;
        }
        Ok(())
    }
    pub fn check(&self) -> Result<()> {
        let store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(message) = &store.blocked {
            return Err(err(message));
        }
        Ok(())
    }
    pub fn get(&self, id: u64) -> Option<Value> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).ledger.records.get(&id).map(record_json)
    }
    pub fn list(&self) -> Vec<Value> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).ledger.records.values().map(record_json).collect()
    }
    pub fn forget(&self, id: u64) -> Result<()> {
        let mut store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let record = store.ledger.records.get(&id).ok_or_else(|| err("no such job"))?;
        if matches!(record.state.as_str(), "running" | "cancelling") {
            return Err(err("cannot forget a running job"));
        }
        store.ledger.records.remove(&id);
        write(&store)
    }
}
fn valid_record(id: u64, r: &JobRecord) -> bool {
    id != 0
        && id == r.id
        && r.label.len() <= 4096
        && r.session_id.len() <= 256
        && r.project_path.as_ref().is_none_or(|p| p.len() <= 8192)
        && r.project_name.len() <= 4096
        && r.command.as_ref().is_none_or(|c| c.len() <= 256)
        && r.created_at.len() <= 128
        && matches!(r.state.as_str(), "running" | "cancelling" | "completed" | "interrupted" | "cancelled" | "failed")
        && r.snapshot.is_object()
        && r.snapshot.get("id").and_then(Value::as_u64) == Some(id)
        && r.snapshot.get("label").and_then(Value::as_str) == Some(r.label.as_str())
        && r.snapshot.get("finished").is_some_and(Value::is_boolean)
}

fn record_json(record: &JobRecord) -> Value {
    let mut value = if record.snapshot.is_object() {
        record.snapshot.clone()
    } else {
        json!({"id":record.id,"label":record.label,"finished":true,"result":{"error":"invalid retained job snapshot"}})
    };
    value["inputRevision"] = json!(record.input_revision);
    value["inputSession"] = json!(record.session_id);
    value["projectPath"] = json!(record.project_path);
    value["projectName"] = json!(record.project_name);
    value["command"] = json!(record.command);
    value["createdAt"] = json!(record.created_at);
    value["state"] = json!(record.state);
    value
}

pub(crate) fn guard(s: &Session, job: &Job, run: impl FnOnce() + Send + 'static) -> impl FnOnce() + Send + 'static {
    let registry = s.job_registry.clone();
    let job = job.clone();
    let guarded = crate::export_tools::guard_job(job.progress.clone(), job.result.clone(), run);
    move || {
        guarded();
        if let Err(e) = registry.sync(&[job]) {
            log::error!("{e}");
        }
    }
}
pub fn list(s: &mut Session, _: &Value) -> Result<Value> {
    s.job_registry.check()?;
    s.job_registry.sync(&s.jobs)?;
    let mut records = s.job_registry.list();
    // Hosts/tests that injected jobs before the registry was configured retain their existing view.
    for job in &s.jobs {
        if let Some(existing) = records.iter_mut().find(|v| v["id"].as_u64() == Some(job.id)) {
            if let Some(offline) = crate::export_tools::job_json(s, job).get("offlineMedia") {
                existing["offlineMedia"] = offline.clone();
            }
        } else {
            records.push(crate::export_tools::job_json(s, job));
        }
    }
    Ok(Value::Array(records))
}
pub(crate) fn commands() -> Vec<crate::CommandSpec> {
    vec![
        crate::CommandSpec {
            id: "jobs.get",
            label: "Get Durable Job",
            menu: &[],
            shortcut: None,
            params: r#"{"job":id}"#,
            enabled: crate::commands::always,
            journal: false,
            run: |s, p| {
                s.job_registry.check()?;
                s.job_registry.sync(&s.jobs)?;
                let id = p.get("job").and_then(Value::as_u64).ok_or_else(|| err("need job id"))?;
                s.job_registry.get(id).ok_or_else(|| err("no such job"))
            },
        },
        crate::CommandSpec {
            id: "jobs.forget",
            label: "Forget Finished Job",
            menu: &[],
            shortcut: None,
            params: r#"{"job":id}"#,
            enabled: crate::commands::always,
            journal: true,
            run: |s, p| {
                s.job_registry.check()?;
                s.job_registry.sync(&s.jobs)?;
                let id = p.get("job").and_then(Value::as_u64).ok_or_else(|| err("need job id"))?;
                s.job_registry.forget(id)?;
                s.jobs.retain(|j| j.id != id);
                Ok(json!({"forgotten":id}))
            },
        },
    ]
}

/// Spawn failure is a finished job error, never a receipt that remains running forever.
pub(crate) fn spawn(s: &Session, id: u64, name: &str, run: impl FnOnce() + Send + 'static) -> Result<()> {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(run) {
        let message = format!("could not start job worker: {e}");
        if let Some(job) = s.jobs.iter().find(|j| j.id == id) {
            *job.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(message.clone()));
            *job.progress.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message.clone());
            job.progress.finished.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        s.job_registry.sync(&s.jobs)?;
        return Err(err(message));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_retains_identity_revision_and_interrupts_unfinished_jobs() {
        let dir = crate::temp_dir().join(format!("filmcraft-jobs-{}", web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap().as_nanos()));
        let mut s = Session::default();
        s.job_registry.configure(&dir).unwrap();
        let job = s.job_registry.new_job(&s, "sample".into()).unwrap();
        let id = job.id;
        assert_eq!(s.job_registry.get(id).unwrap()["inputRevision"], s.revision);
        s.jobs.push(job);
        drop(s);
        let s = Session::default();
        s.job_registry.configure(&dir).unwrap();
        assert_eq!(s.job_registry.get(id).unwrap()["state"], "interrupted");
        assert!(s.job_registry.new_job(&s, "next".into()).unwrap().id > id);
        s.job_registry.forget(id).unwrap();
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn oversized_failure_receipts_keep_a_failure_marker_and_bounded_error() {
        let directory = crate::temp_dir().join(format!(
            "filmcraft-large-job-{}-{}",
            std::process::id(),
            web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let s = Session::default();
        s.job_registry.configure(&directory).unwrap();
        let job = s.job_registry.new_job(&s, "L".repeat(600)).unwrap();
        *job.progress.status.lock().unwrap() = "🟩".repeat(17000);
        s.job_registry.sync(std::slice::from_ref(&job)).unwrap();
        let running = s.job_registry.get(job.id).unwrap();
        assert_eq!(running["state"], "running");
        assert_eq!(running["finished"], false);
        assert!(running["result"].is_null());
        *job.result.lock().unwrap() = Some(Err("failed ".to_owned() + &"🟩".repeat(17000)));
        *job.progress.status.lock().unwrap() = "status ".to_owned() + &"🟩".repeat(17000);
        s.job_registry.sync(std::slice::from_ref(&job)).unwrap();
        let record = s.job_registry.get(job.id).unwrap();
        assert_eq!(record["state"], "failed");
        assert_eq!(record["finished"], true);
        assert!(record["result"]["error"].as_str().unwrap().starts_with("failed"));
        assert!(serde_json::to_vec(&record).unwrap().len() < 16 * 1024);
        drop(s);
        let reopened = Session::default();
        reopened.job_registry.configure(&directory).unwrap();
        let restored = reopened.job_registry.get(job.id).unwrap();
        assert_eq!(restored["label"], "L".repeat(600));
        assert_eq!(restored["state"], "failed");
        assert!(restored["result"]["error"].is_string());
        drop(reopened);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_completion_never_persists_an_unfinished_terminal_receipt() {
        let s = Session::default();
        for _ in 0..64 {
            let job = s.job_registry.new_job(&s, "racing completion".into()).unwrap();
            let result = job.result.clone();
            let worker = std::thread::spawn(move || {
                *result.lock().unwrap() = Some(Err("synthetic failure".into()));
            });
            for _ in 0..8 {
                s.job_registry.sync(std::slice::from_ref(&job)).unwrap();
                let record = s.job_registry.get(job.id).unwrap();
                if matches!(record["state"].as_str(), Some("completed" | "failed" | "cancelled")) {
                    assert_eq!(record["finished"], true);
                    assert!(!record["result"].is_null());
                }
            }
            worker.join().unwrap();
            s.job_registry.sync(std::slice::from_ref(&job)).unwrap();
            assert_eq!(s.job_registry.get(job.id).unwrap()["state"], "failed");
        }
    }

    #[test]
    fn hostile_saved_snapshot_shapes_return_errors_without_panicking_or_overwriting() {
        for snapshot in [json!([]), json!("invalid")] {
            let dir = crate::temp_dir()
                .join(format!("filmcraft-hostile-jobs-{}", web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap().as_nanos()));
            std::fs::create_dir_all(&dir).unwrap();
            let s = Session::default();
            let job = s.job_registry.new_job(&s, "hostile".into()).unwrap();
            let mut record = s.job_registry.0.lock().unwrap().ledger.records.get(&job.id).unwrap().clone();
            record.snapshot = snapshot;
            let bytes = serde_json::to_vec(&Ledger { next_id: job.id, records: BTreeMap::from([(job.id, record)]) }).unwrap();
            std::fs::write(dir.join("jobs.json"), &bytes).unwrap();
            let mut loaded = Session::default();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loaded.job_registry.configure(&dir)));
            assert!(outcome.unwrap().is_err());
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| loaded.execute("jobs.list", json!({})))).unwrap().is_err());
            assert_eq!(std::fs::read(dir.join("jobs.json")).unwrap(), bytes);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn job_guard_persists_failure_and_running_jobs_cannot_be_forgotten() {
        let s = Session::default();
        let job = s.job_registry.new_job(&s, "guard".into()).unwrap();
        assert!(s.job_registry.forget(job.id).is_err());
        guard(&s, &job, || panic!("synthetic worker failure"))();
        assert_eq!(s.job_registry.get(job.id).unwrap()["state"], "failed");
        s.job_registry.forget(job.id).unwrap();
    }
}
