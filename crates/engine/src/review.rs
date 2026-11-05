//! Persistent checkpoints, timeline annotations and the project activity ledger.
use crate::{
    Result, Session,
    commands::{CommandSpec, always, bad},
};
use filmcraft_project::{
    ItemId, Project,
    review::{Activity, Annotation, Checkpoint, MAX_ACTIVITY, MAX_ANNOTATIONS, MAX_CHECKPOINT_BYTES, MAX_CHECKPOINTS},
};
use filmcraft_time::Tick;
use serde_json::{Value, json};

fn utc_now() -> String {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}
pub fn record(project: &mut Project, command: &str, label: &str, before: u64, after: u64) -> Result<()> {
    let id = project.review.alloc().map_err(|e| bad("review.activity", e))?;
    let clip = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    project.review.activity.push(Activity {
        id,
        command: clip(command, 256),
        label: clip(label, 1024),
        before_revision: before,
        after_revision: after,
        created_utc: utc_now(),
    });
    if project.review.activity.len() > MAX_ACTIVITY {
        project.review.activity.remove(0);
    }
    Ok(())
}
fn checkpoint_id(p: &Value, cmd: &str) -> Result<u64> {
    p["checkpoint"].as_u64().ok_or_else(|| bad(cmd, "checkpoint ID is required"))
}
fn create(s: &mut Session, p: &Value) -> Result<Value> {
    let name = p["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty() && n.len() <= 128)
        .ok_or_else(|| bad("review.checkpoint", "name must contain 1–128 bytes"))?
        .to_string();
    if s.project.review.checkpoints.len() >= MAX_CHECKPOINTS {
        return Err(bad("review.checkpoint", "checkpoint limit reached; explicitly delete an older checkpoint"));
    }
    if s.project.review.checkpoints.iter().any(|c| c.name == name) {
        return Err(bad("review.checkpoint", "checkpoint name already exists"));
    }
    let mut document = (*s.project).clone();
    document.review = Default::default();
    let document = serde_json::to_string(&document).map_err(|e| bad("review.checkpoint", e.to_string()))?;
    if document.len() > MAX_CHECKPOINT_BYTES {
        return Err(bad("review.checkpoint", "project exceeds the 8 MiB checkpoint snapshot limit"));
    }
    let revision = s.revision;
    let id = s.edit("Create Checkpoint", move |pr, _| {
        let id = pr.review.alloc().map_err(|e| bad("review.checkpoint", e))?;
        pr.review.checkpoints.push(Checkpoint { id, name, revision, document: document.into(), created_utc: utc_now() });
        Ok(id)
    })?;
    Ok(json!({"checkpoint":id,"revision":revision}))
}
fn list(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(
        json!({"checkpoints":s.project.review.checkpoints.iter().map(|c|json!({"checkpoint":c.id,"name":c.name,"revision":c.revision,"createdUtc":c.created_utc})).collect::<Vec<_>>()}),
    )
}
fn diff(s: &mut Session, p: &Value) -> Result<Value> {
    let id = checkpoint_id(p, "review.diff")?;
    let c = s.project.review.checkpoints.iter().find(|c| c.id == id).ok_or_else(|| bad("review.diff", "checkpoint is missing"))?;
    if c.document.len() > MAX_CHECKPOINT_BYTES {
        return Err(bad("review.diff", "checkpoint exceeds snapshot limit"));
    }
    let mut before: Project = serde_json::from_str(&c.document).map_err(|e| bad("review.diff", format!("checkpoint is damaged: {e}")))?;
    if !before.review.is_empty() {
        return Err(bad("review.diff", "nested checkpoint review data is not supported"));
    }
    let mut after = (*s.project).clone();
    before.review = Default::default();
    after.review = Default::default();
    Ok(
        json!({"checkpoint":id,"name":c.name,"checkpointRevision":c.revision,"currentRevision":s.revision,"diff":crate::agent_plans::project_diff(&before,&after)?}),
    )
}
fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    let id = checkpoint_id(p, "review.deleteCheckpoint")?;
    if !s.project.review.checkpoints.iter().any(|c| c.id == id) {
        return Err(bad("review.deleteCheckpoint", "checkpoint is missing"));
    }
    s.edit("Delete Checkpoint", |pr, _| {
        pr.review.checkpoints.retain(|c| c.id != id);
        Ok(())
    })?;
    Ok(json!({"deleted":id}))
}
fn annotate(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 32_768)?;
    let text = p["text"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 16_384)
        .ok_or_else(|| bad("review.annotate", "text must contain 1–16384 bytes"))?
        .to_string();
    let sequence = p["sequence"].as_u64().map(ItemId).or(s.state.active_sequence).ok_or(crate::EngineError::NoSequence)?;
    if s.project.sequence(sequence).is_none() {
        return Err(bad("review.annotate", "sequence does not exist"));
    }
    let start = Tick(p["start"].as_i64().unwrap_or(s.playhead().0));
    let end = Tick(p["end"].as_i64().unwrap_or(start.0));
    let kind = p["kind"].as_str().unwrap_or("note").to_string();
    let author = p["author"].as_str().unwrap_or("").to_string();
    let update = p["annotation"].as_u64();
    let status = p["status"].as_str().unwrap_or("open").to_string();
    if start.0 < 0
        || end < start
        || end > Tick::MAX
        || author.len() > 256
        || !["note", "suggestion", "finding"].contains(&kind.as_str())
        || !["open", "accepted", "dismissed"].contains(&status.as_str())
    {
        return Err(bad("review.annotate", "invalid annotation bounds, kind, status or author"));
    }
    let id = s.edit("Timeline Annotation", move |pr, _| {
        let id = if let Some(id) = update {
            if !pr.review.annotations.iter().any(|a| a.id == id) {
                return Err(bad("review.annotate", "annotation is missing"));
            }
            id
        } else {
            if pr.review.annotations.len() >= MAX_ANNOTATIONS {
                return Err(bad("review.annotate", "annotation limit reached"));
            }
            pr.review.alloc().map_err(|e| bad("review.annotate", e))?
        };
        let a = Annotation { id, sequence, start, end, text, kind, author, status, created_utc: utc_now() };
        if let Some(old) = pr.review.annotations.iter_mut().find(|a| a.id == id) {
            *old = a
        } else {
            pr.review.annotations.push(a)
        };
        Ok(id)
    })?;
    Ok(json!({"annotation":id}))
}
fn annotations(s: &mut Session, p: &Value) -> Result<Value> {
    let seq = p["sequence"].as_u64();
    Ok(json!({"annotations":s.project.review.annotations.iter().filter(|a|seq.is_none_or(|id|id==a.sequence.0)).collect::<Vec<_>>()}))
}
pub fn commands() -> Vec<CommandSpec> {
    let spec = |id, label, params, run, journal| CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal };
    vec![
        spec("review.checkpoint", "Save Named Checkpoint", r#"{"name":str}"#, create, true),
        spec("review.checkpoints", "List Checkpoints", "{}", list, false),
        spec("review.diff", "Compare Checkpoint", r#"{"checkpoint":id}"#, diff, false),
        spec("review.deleteCheckpoint", "Delete Checkpoint", r#"{"checkpoint":id}"#, delete, true),
        spec(
            "review.annotate",
            "Save Timeline Annotation",
            r#"{"annotation":id?,"sequence":id?,"start":ticks?,"end":ticks?,"text":str,"kind":"note|suggestion|finding"?,"author":str?,"status":"open|accepted|dismissed"?}"#,
            annotate,
            true,
        ),
        spec("review.annotations", "List Timeline Annotations", r#"{"sequence":id?}"#, annotations, false),
        spec("review.activity", "Project Activity Ledger", "{}", |s, _| Ok(json!({"activity":s.project.review.activity})), false),
    ]
}
