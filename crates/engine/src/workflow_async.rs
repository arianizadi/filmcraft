//! Agent preparation never needs to block the desktop event loop. Completed workers stage an
//! opaque proposal only after the original document/revision is checked on the owning thread.
use crate::{
    Result, Session,
    agent_plans::PreparedEditPlan,
    commands::{CommandSpec, always, bad},
    workflow::{self, BackgroundJob},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::mpsc::TryRecvError};

#[derive(Default)]
pub struct Tasks {
    pending: Vec<BackgroundJob<PreparedEditPlan>>,
    results: BTreeMap<u64, Value>,
}

pub fn poll(s: &mut Session) {
    let mut waiting = Vec::new();
    for task in std::mem::take(&mut s.workflow_tasks.pending) {
        let result = match task.receiver.try_recv() {
            Ok(value) => value,
            Err(TryRecvError::Empty) => {
                waiting.push(task);
                continue;
            }
            Err(TryRecvError::Disconnected) => Err(bad("workflow.prepareAsync", "preparation worker disconnected")),
        };
        let result = workflow::adopt_background_plan(s, &task, result);
        let mut summary = json!({"job":task.job.id});
        if let Some(receipt) = s.job_registry.get(task.job.id) {
            summary["projectPath"] = receipt["projectPath"].clone();
            summary["inputRevision"] = receipt["inputRevision"].clone();
        }
        match result {
            Ok(report) => {
                summary["status"] = json!("staged");
                summary["plan"] = report["plan"].clone();
                summary["next"] = json!("agent.preview and render.frames; native approval required");
            }
            Err(error) => {
                summary["status"] = json!("failed");
                summary["error"] = json!(error.to_string().chars().take(4096).collect::<String>());
            }
        }
        s.workflow_tasks.results.insert(task.job.id, summary);
    }
    s.workflow_tasks.pending = waiting;
    while s.workflow_tasks.results.len() > 128 {
        s.workflow_tasks.results.pop_first();
    }
}

fn prepare(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 262_144)?;
    if !p.is_object() || p.get("apply").is_some_and(|v| v != false) {
        return Err(bad("workflow.prepareAsync", "parameters must be an object; asynchronous preparation requires apply:false"));
    }
    if s.workflow_tasks.pending.len() >= 2 {
        return Err(bad("workflow.prepareAsync", "two preparations are pending; inspect or cancel them first"));
    }
    let task = workflow::prepare_background(s, p.clone())?;
    let id = task.job.id;
    s.workflow_tasks.pending.push(task);
    Ok(json!({"job":id,"status":"preparing","inputRevision":s.revision,"poll":"workflow.status","cancel":"jobs.cancel","approvalRequired":true}))
}

fn entry(s: &Session, id: u64) -> Result<Value> {
    if let Some(result) = s.workflow_tasks.results.get(&id) {
        return Ok(result.clone());
    }
    let receipt = s.job_registry.get(id).ok_or_else(|| bad("workflow.status", "unknown preparation job"))?;
    if !s.workflow_tasks.pending.iter().any(|task| task.job.id == id) {
        return Err(bad("workflow.status", "no session proposal for this job; inspect jobs.get for interrupted or retired history"));
    }
    Ok(
        json!({"job":id,"status":receipt["state"],"projectPath":receipt["projectPath"],"inputRevision":receipt["inputRevision"],"progress":receipt["progress"],"message":receipt["status"]}),
    )
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "workflow.prepareAsync",
            label: "Prepare Automatic Proposal in Background",
            menu: &[],
            shortcut: None,
            params: r#"{"paths":[absolutePath]?,"items":[id]?,"profile":str?,"mode":"talkingHead|dj|selects"?,"transcribe":bool=true,"analyze":bool=true,"apply":false}"#,
            enabled: always,
            journal: false,
            run: prepare,
        },
        CommandSpec {
            id: "workflow.status",
            label: "Inspect Automatic Preparation",
            menu: &[],
            shortcut: None,
            params: r#"{"job":id?}"#,
            enabled: always,
            journal: false,
            run: |s, p| {
                if let Some(id) = p.get("job").and_then(Value::as_u64) {
                    return entry(s, id);
                }
                if p.get("job").is_some_and(|v| !v.is_null()) {
                    return Err(bad("workflow.status", "job must be an unsigned integer"));
                }
                let ids: std::collections::BTreeSet<_> =
                    s.workflow_tasks.pending.iter().map(|task| task.job.id).chain(s.workflow_tasks.results.keys().copied()).collect();
                Ok(json!({"jobs":ids.into_iter().filter_map(|id|entry(s,id).ok()).collect::<Vec<_>>()}))
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wait(s: &mut Session, id: u64) -> Value {
        let deadline = web_time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let report = s.execute("workflow.status", json!({"job":id})).unwrap();
            if matches!(report["status"].as_str(), Some("staged" | "failed")) {
                return report;
            }
            assert!(web_time::Instant::now() < deadline, "preparation timed out: {report}");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    #[test]
    fn agent_preparation_is_pollable_and_adopts_only_an_unchanged_document() {
        let mut s = Session::default();
        let item = s.execute("file.newBarsAndTone", json!({"seconds":1})).unwrap()["item"].clone();
        let before = s.project.clone();
        let revision = s.revision;
        let request = json!({"items":[item],"transcribe":false,"analyze":false,"normalizeAudio":false,"mode":"selects"});
        let id = s.execute("workflow.prepareAsync", request.clone()).unwrap()["job"].as_u64().unwrap();
        assert_eq!(s.project, before);
        let report = wait(&mut s, id);
        assert_eq!(report["status"], "staged", "{report}");
        assert_eq!(s.project, before);
        assert_eq!(s.revision, revision);
        assert_eq!(s.job_registry.get(id).unwrap()["state"], "completed");
        let id = s.execute("workflow.prepareAsync", request).unwrap()["job"].as_u64().unwrap();
        s.edit("Owner continues editing", |project, _| {
            project.notes = "Owner continues editing".into();
            Ok(())
        })
        .unwrap();
        let changed = s.project.clone();
        let report = wait(&mut s, id);
        assert_eq!(report["status"], "failed", "{report}");
        assert!(report["error"].as_str().unwrap().contains("stale"));
        assert_eq!(s.project, changed);
    }
}
