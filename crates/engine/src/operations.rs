//! Session-scoped operation receipts. Never evict receipts and accidentally execute a retry twice.
//! A new app process has a different session identity; uncertain writes must be inspected there.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};

const MAX_OPERATIONS: usize = 4096;
const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_RECEIPT_BYTES: usize = 1_048_576;
const MAX_LEDGER_BYTES: usize = 64 * 1_048_576;
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Inspect,
    Propose,
    #[default]
    Edit,
}

pub struct OperationStore {
    pub session_id: String,
    pub permission: Permission,
    records: BTreeMap<String, Record>,
    keys: BTreeMap<String, String>,
    receipt_bytes: usize,
    order: Vec<String>,
}

impl OperationStore {
    pub fn activity(&self) -> Value {
        Value::Array(self.order.iter().rev().take(100).filter_map(|id| self.records.get(id)).map(|r| r.receipt.clone()).collect())
    }
}

impl Default for OperationStore {
    fn default() -> Self {
        // Opaque identity, not a calendar date. web_time also supports the browser build.
        let stamp = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        Self {
            session_id: format!("{stamp}-{}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)),
            permission: Permission::Edit,
            records: BTreeMap::new(),
            keys: BTreeMap::new(),
            receipt_bytes: 0,
            order: Vec::new(),
        }
    }
}

struct Record {
    key: String,
    command: String,
    params: Value,
    base_revision: Option<u64>,
    receipt: Value,
}

/// Whether a command needs a receipt when sent over an unreliable transport.
pub fn is_write(id: &str, params: &Value) -> bool {
    !crate::command_access::is_read_only(id, params)
}

/// Gate *all* remote command paths, including the generic engine and UI routes.
pub fn authorize(s: &Session, id: &str, params: &Value) -> Result<()> {
    if id == "agent.execute" {
        let inner = params.get("command").and_then(Value::as_str).ok_or_else(|| bad(id, "missing command"))?;
        if inner == "agent.execute" {
            return Err(bad(id, "nested operations are not supported"));
        }
        return authorize(s, inner, params.get("params").unwrap_or(&Value::Null));
    }
    let allowed = !is_write(id, params)
        || s.operations.permission == Permission::Edit
        || (s.operations.permission == Permission::Propose && matches!(id, "agent.plan" | "agent.discard"));
    if allowed { Ok(()) } else { Err(crate::EngineError::Other(format!("permission_denied: {id} requires Edit permission in the Agent Review window"))) }
}

fn required_string<'a>(p: &'a Value, key: &str) -> Result<&'a str> {
    p.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .ok_or_else(|| bad("agent.execute", format!("{key} must be a nonempty string of at most 256 bytes without control characters")))
}

fn execute(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, MAX_REQUEST_BYTES).map_err(|e| bad("agent.execute", e.to_string()))?;
    if !p.is_object() {
        return Err(bad("agent.execute", "arguments must be an object"));
    }
    let operation = required_string(p, "operationId")?;
    let key = required_string(p, "idempotencyKey")?;
    let session = required_string(p, "sessionId")?;
    let command = required_string(p, "command")?;
    let params = p.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return Err(bad("agent.execute", "params must be an object"));
    }
    if command == "agent.execute" {
        return Err(bad("agent.execute", "nested operations are not supported"));
    }
    if command == "file.exportMedia" && params.get("wait").and_then(Value::as_bool) == Some(true) {
        return Err(bad(
            "agent.execute",
            "blocking exports are not supported inside operation envelopes; use wait:false and poll the returned job with jobs.list",
        ));
    }
    let base = match p.get("baseRevision") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| bad("agent.execute", "baseRevision must be an unsigned integer"))?),
    };
    if session != s.operations.session_id {
        return Err(bad(
            "agent.execute",
            "unknown_commit: app session changed; inspect the document and get_operation before deciding whether to create a new operation",
        ));
    }
    if let Some(original) = s.operations.keys.get(key)
        && original != operation
    {
        return Err(bad("agent.execute", format!("idempotency key belongs to operation {original}; use get_operation")));
    }
    if let Some(record) = s.operations.records.get(operation) {
        if record.key != key || record.command != command || record.params != params || record.base_revision != base {
            return Err(bad("agent.execute", "operationId was already used with different arguments"));
        }
        let mut receipt = record.receipt.clone();
        receipt["replayed"] = json!(true);
        return Ok(receipt);
    }
    if !is_write(command, &params) {
        return Err(bad("agent.execute", "read-only commands do not need operation receipts; run the query directly"));
    }
    if s.operations.records.len() >= MAX_OPERATIONS || s.operations.receipt_bytes >= MAX_LEDGER_BYTES {
        return Err(bad("agent.execute", "operation receipt capacity reached; save your project and start a new app session before submitting more writes"));
    }
    let bytes = serde_json::to_vec(p).map_err(|e| bad("agent.execute", e.to_string()))?;
    if s.operations.receipt_bytes.saturating_add(bytes.len()).saturating_add(MAX_RECEIPT_BYTES) > MAX_LEDGER_BYTES {
        return Err(bad("agent.execute", "operation receipt memory budget reached; save and start a new app session"));
    }
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(bad("agent.execute", "operation arguments exceed 1 MiB"));
    }
    authorize(s, command, &params)?;
    if base.is_some_and(|revision| revision != s.revision) {
        return Err(bad("agent.execute", format!("stale_revision: expected {:?}, current {}", base, s.revision)));
    }
    let before_revision = s.revision;
    let before = s.project.clone();
    // Reserve the receipt before running. An unexpected failure cannot make the key reusable.
    let initial = json!({"operationId": operation, "idempotencyKey": key, "sessionId": session,
        "status": "unknown_commit", "beforeRevision": before_revision, "afterRevision": null,
        "command": command, "replayed": false});
    s.operations.receipt_bytes = s.operations.receipt_bytes.saturating_add(bytes.len());
    s.operations.order.push(operation.to_string());
    s.operations.keys.insert(key.to_string(), operation.to_string());
    s.operations.records.insert(
        operation.to_string(),
        Record { key: key.to_string(), command: command.to_string(), params: params.clone(), base_revision: base, receipt: initial },
    );
    let result = s.execute(command, params);
    // An unchanged revision proves nothing about external files, jobs, or partial session changes.
    let unchanged_atomic = (crate::agent_plans::eligible_command(command) || matches!(command, "agent.plan" | "agent.apply" | "agent.discard"))
        && before_revision == s.revision
        && *before == *s.project;
    let mut receipt = json!({"operationId": operation, "idempotencyKey": key, "sessionId": session,
        "status": if result.is_ok() { "committed" } else if unchanged_atomic { "failed" } else { "unknown_commit" },
        "beforeRevision": before_revision, "afterRevision": s.revision, "command": command, "replayed": false});
    match crate::agent_plans::project_diff(&before, &s.project) {
        Ok(diff) => receipt["affected"] = diff.get("affected").cloned().unwrap_or(Value::Null),
        Err(e) => receipt["inspectionWarning"] = json!(e.to_string()),
    }
    match result {
        Ok(value) => receipt["result"] = value,
        Err(error) => receipt["error"] = json!(error.to_string()),
    }
    // Keep receipts bounded even when a write returns a large proposal/report. Retain stable
    // created identities so the caller can fetch the full report with the corresponding query.
    let size = serde_json::to_vec(&receipt).map(|v| v.len()).unwrap_or(MAX_RECEIPT_BYTES.saturating_add(1));
    if (size > MAX_RECEIPT_BYTES || s.operations.receipt_bytes.saturating_add(size) > MAX_LEDGER_BYTES)
        && let Some(value) = receipt.get_mut("result")
    {
        let mut summary = serde_json::Map::new();
        for key in ["id", "item", "sequence", "clip", "plan", "job", "path", "status", "beforeRevision", "afterRevision", "affected"] {
            if let Some(v) = value.get(key) {
                summary.insert(key.to_string(), v.clone());
            }
        }
        summary.insert("reportOmitted".into(), json!(true));
        summary.insert("reason".into(), json!("receipt report budget; inspect the affected object or preview_plan for details"));
        *value = Value::Object(summary);
    }
    let stored_size = serde_json::to_vec(&receipt).map(|v| v.len()).unwrap_or(MAX_RECEIPT_BYTES);
    s.operations.receipt_bytes = s.operations.receipt_bytes.saturating_add(stored_size);
    if let Some(record) = s.operations.records.get_mut(operation) {
        record.receipt = receipt.clone();
    }
    Ok(receipt)
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    let operation = required_string(p, "operationId")?;
    if let Some(session) = p.get("sessionId").and_then(Value::as_str)
        && session != s.operations.session_id
    {
        return Ok(
            json!({"operationId": operation, "status": "unknown_commit", "sessionId": s.operations.session_id, "reason": "app session changed; receipts are session-scoped"}),
        );
    }
    Ok(s.operations.records.get(operation).map(|r| r.receipt.clone()).unwrap_or_else(|| {
        json!({"operationId": operation, "status": "unknown_commit", "sessionId": s.operations.session_id, "reason": "no receipt in this app session; inspect before submitting a new operation"})
    }))
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "agent.session",
            label: "Inspect Agent Session",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: always,
            journal: false,
            run: |s, _| {
                Ok(json!({"sessionId": s.operations.session_id, "revision": s.revision, "documentEpoch": s.document_epoch,
                "permission": s.operations.permission, "operations": s.operations.records.len(), "capacity": MAX_OPERATIONS}))
            },
        },
        CommandSpec {
            id: "agent.execute",
            label: "Execute Retry-safe Operation",
            menu: &[],
            shortcut: None,
            params: r#"{"operationId":string,"idempotencyKey":string,"sessionId":string,"command":string,"params":object?,"baseRevision":u64?}"#,
            enabled: always,
            journal: true,
            run: execute,
        },
        CommandSpec {
            id: "agent.operation",
            label: "Get Operation Receipt",
            menu: &[],
            shortcut: None,
            params: r#"{"operationId":string,"sessionId":string?}"#,
            enabled: always,
            journal: false,
            run: get,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_cannot_create_another_item_or_replace_arguments() {
        let mut s = Session::default();
        let p = json!({"operationId":"one", "idempotencyKey":"one", "sessionId":s.operations.session_id,
            "command":"file.newSequence", "params":{"name":"One"}});
        let first = execute(&mut s, &p).unwrap();
        assert_eq!(first["status"], "committed");
        let revision = s.revision;
        let count = s.project.items.len();
        let replay = execute(&mut s, &p).unwrap();
        assert_eq!(replay["result"], first["result"]);
        assert_eq!(replay["replayed"], true);
        assert_eq!(s.revision, revision);
        assert_eq!(s.project.items.len(), count);
        let mut changed = p;
        changed["params"]["name"] = json!("Other");
        assert!(execute(&mut s, &changed).is_err());
    }

    #[test]
    fn changed_session_and_stale_revision_fail_closed() {
        let mut s = Session::default();
        let mut p = json!({"operationId":"one", "idempotencyKey":"one", "sessionId":"old", "command":"file.newSequence"});
        assert!(execute(&mut s, &p).unwrap_err().to_string().contains("unknown_commit"));
        p["sessionId"] = json!(s.operations.session_id);
        p["baseRevision"] = json!(0);
        assert!(execute(&mut s, &p).unwrap_err().to_string().contains("stale_revision"));
        assert!(s.operations.records.is_empty());
    }

    #[test]
    fn permission_applies_inside_the_generic_operation() {
        let mut s = Session::default();
        s.operations.permission = Permission::Inspect;
        assert!(authorize(&s, "sequence.inspect", &json!({})).is_ok());
        assert!(authorize(&s, "agent.execute", &json!({"command":"file.newSequence"})).is_err());
        s.operations.permission = Permission::Propose;
        assert!(authorize(&s, "agent.plan", &json!({})).is_ok());
        assert!(authorize(&s, "agent.apply", &json!({})).is_err());
    }

    #[test]
    fn hostile_operations_and_queries_do_not_fill_the_ledger() {
        let mut s = Session::default();
        let mut p = json!({"operationId":"one","idempotencyKey":"one","sessionId":s.operations.session_id,"command":"project.inspect"});
        assert!(execute(&mut s, &p).unwrap_err().to_string().contains("read-only"));
        p["command"] = json!("file.newSequence");
        let mut nested = json!(null);
        for _ in 0..64 {
            nested = json!({"nested":nested});
        }
        p["params"] = nested;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(&mut s, &p)));
        assert!(result.unwrap().is_err());
        p["params"] = json!({"name":"a".repeat(MAX_REQUEST_BYTES)});
        assert!(execute(&mut s, &p).is_err());
        assert!(s.operations.records.is_empty());
        assert!(s.history.undo.is_empty());
    }

    #[test]
    fn exhausted_receipt_budget_still_allows_lookup_and_replay() {
        let mut s = Session::default();
        let mut p = json!({"operationId":"one","idempotencyKey":"one","sessionId":s.operations.session_id,"command":"file.newSequence","params":{}});
        let first = execute(&mut s, &p).unwrap();
        s.operations.receipt_bytes = MAX_LEDGER_BYTES;
        assert_eq!(execute(&mut s, &p).unwrap()["result"], first["result"]);
        assert_eq!(get(&mut s, &json!({"operationId":"one"})).unwrap()["status"], "committed");
        p["operationId"] = json!("two");
        p["idempotencyKey"] = json!("two");
        assert!(execute(&mut s, &p).is_err());
        assert_eq!(s.project.sequences().count(), 1);
    }

    #[test]
    fn external_write_error_is_ambiguous_even_without_revision_change() {
        struct NoWrites;
        impl crate::Services for NoWrites {
            fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
                Err(std::io::ErrorKind::NotFound.into())
            }
            fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
                Err(std::io::Error::other("synthetic disk failure"))
            }
        }
        let mut s = Session::new(std::sync::Arc::new(NoWrites));
        let p = json!({"operationId":"save","idempotencyKey":"save","sessionId":s.operations.session_id,"command":"file.saveAs","params":{"path":"/unused/Example.fcproj"}});
        let receipt = execute(&mut s, &p).unwrap();
        assert_eq!(receipt["status"], "unknown_commit");
        assert_eq!(execute(&mut s, &p).unwrap()["error"], receipt["error"]);
    }

    #[test]
    fn blocking_export_envelopes_are_rejected_before_starting_a_job() {
        let mut s = Session::default();
        let p = json!({"operationId":"export","idempotencyKey":"export","sessionId":s.operations.session_id,"command":"file.exportMedia","params":{"wait":true,"path":"/unused/export.mp4"}});
        assert!(execute(&mut s, &p).unwrap_err().to_string().contains("wait:false"));
        assert!(s.jobs.is_empty());
        assert!(s.operations.records.is_empty());
        assert!(s.history.undo.is_empty());
    }
}
