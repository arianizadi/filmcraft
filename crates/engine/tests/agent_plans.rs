use filmcraft_engine::{Services, Session};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn plan(s: &mut Session, steps: Value) -> u64 {
    s.execute("agent.plan", json!({"label":"Reviewed edit","baseRevision":s.revision,"steps":steps})).unwrap()["plan"].as_u64().unwrap()
}

#[test]
fn preview_is_read_only_apply_is_exactly_once_and_one_undo_step() {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"name":"Dialogue"})).unwrap();
    let before = s.project.clone();
    let revision = s.revision;
    let undo = s.history.undo.len();
    let id = plan(
        &mut s,
        json!([
            {"id":"markers.add","params":{"frame":0,"name":"First cut"}},
            {"id":"markers.add","params":{"frame":24,"name":"Second cut"}},
            {"id":"project.setNotes","params":{"text":"Match camera B to the hero"}}
        ]),
    );
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    assert_eq!(s.history.undo.len(), undo);
    let preview = s.execute("agent.preview", json!({"plan":id})).unwrap();
    assert_eq!(preview["stale"], false);
    assert_eq!(preview["diff"]["sequences"][0]["markersChanged"], true);
    let (proposed, _, base) = s.agent_plan_snapshot(id, false).unwrap();
    assert_eq!(base, revision);
    assert_eq!(proposed.sequence(s.state.active_sequence.unwrap()).unwrap().markers.len(), 2);
    let receipt = s.execute("agent.apply", json!({"plan":id})).unwrap();
    assert_eq!(receipt["undoSteps"], 1);
    assert_eq!(s.project, proposed);
    assert_eq!(s.history.undo.len(), undo + 1);
    let revision = s.revision;
    assert_eq!(s.execute("agent.apply", json!({"plan":id})).unwrap(), receipt);
    assert_eq!(s.revision, revision);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
    // A retry after undo still returns the original receipt rather than resurrecting the edit.
    assert_eq!(s.execute("agent.apply", json!({"plan":id})).unwrap(), receipt);
    assert_eq!(s.project, before);
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(s.project, proposed);
}

#[test]
fn failed_step_and_stale_plans_preserve_project_history_and_selection() {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({})).unwrap();
    s.execute("markers.add", json!({"name":"Existing"})).unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    let (project, revision, undo, redo, state) =
        (s.project.clone(), s.revision, s.history.undo.len(), s.history.redo.len(), serde_json::to_value(&s.state).unwrap());
    let result = s.execute(
        "agent.plan",
        json!({"steps":[
            {"id":"markers.add","params":{"name":"Must roll back"}},
            {"id":"timeline.trim","params":{"clip":u64::MAX,"edge":"in","deltaFrames":1}}
        ]}),
    );
    assert!(result.is_err());
    assert_eq!(s.project, project);
    assert_eq!(s.revision, revision);
    assert_eq!((s.history.undo.len(), s.history.redo.len()), (undo, redo));
    assert_eq!(serde_json::to_value(&s.state).unwrap(), state);
    let id = plan(&mut s, json!([{"id":"markers.add","params":{"name":"Proposal"}}]));
    s.execute("markers.add", json!({"name":"Manual edit"})).unwrap();
    let (project, revision, history) = (s.project.clone(), s.revision, s.history.undo.len());
    assert!(s.execute("agent.apply", json!({"plan":id})).unwrap_err().to_string().contains("stale_plan"));
    assert_eq!(s.project, project);
    assert_eq!(s.revision, revision);
    assert_eq!(s.history.undo.len(), history);
    assert_eq!(s.execute("agent.preview", json!({"plan":id})).unwrap()["stale"], true);
}

#[test]
fn document_replacement_discard_and_reference_errors_fail_closed() {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({})).unwrap();
    let id = plan(&mut s, json!([{"id":"markers.add","params":{"name":"Old document"}}]));
    s.execute("file.newProject", json!({})).unwrap();
    assert!(s.execute("agent.apply", json!({"plan":id})).is_err());
    assert!(s.agent_plan_snapshot(id, false).is_err());
    let id = plan(&mut s, json!([{"id":"project.setNotes","params":{"text":"New document"}}]));
    s.execute("agent.discard", json!({"plan":id})).unwrap();
    assert!(s.execute("agent.apply", json!({"plan":id})).is_err());
    assert!(s.agent_plan_snapshot(id, false).is_err());
    assert_eq!(s.execute("agent.discard", json!({"plan":id})).unwrap()["status"], "discarded");
    for steps in [
        json!([]),
        json!([{"id":"project.setNotes","params":{"text":{"$step":0,"path":"/missing"}}}]),
        json!([{"id":"project.setNotes","params":{"text":{"$step":-1}}}]),
        json!([{"id":"project.setNotes","params":{"text":{"$step":0,"extra":true}}}]),
    ] {
        assert!(s.execute("agent.plan", json!({"steps":steps})).is_err());
    }
}

#[test]
fn prior_step_references_resolve_stable_creation_ids() {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({})).unwrap();
    let id = plan(
        &mut s,
        json!([
            {"id":"markers.add","params":{"frame":8,"name":"First name"}},
            {"id":"markers.edit","params":{"marker":{"$step":0,"path":"/marker"},"name":"Approved name"}}
        ]),
    );
    let (after, _, _) = s.agent_plan_snapshot(id, false).unwrap();
    assert_eq!(after.sequence(s.state.active_sequence.unwrap()).unwrap().markers[0].name, "Approved name");
    assert!(s.active_sequence().unwrap().markers.is_empty());
}

struct CountWrites(Arc<AtomicUsize>);
impl Services for CountWrites {
    fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
        Ok(vec![])
    }
    fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[test]
fn file_jobs_and_unbounded_parameters_are_rejected_before_execution() {
    let writes = Arc::new(AtomicUsize::new(0));
    let mut s = Session::new(Arc::new(CountWrites(writes.clone())));
    for id in ["file.saveAs", "file.exportMedia", "transcript.downloadModel", "masks.track", "agent.apply", "file.newColorMatte", "file.import"] {
        assert!(s.execute("agent.plan", json!({"steps":[{"id":id,"params":{"path":"/tmp/never-written"}}]})).is_err());
    }
    assert_eq!(writes.load(Ordering::Relaxed), 0);
    for value in [
        json!({"steps":vec![json!({"id":"project.setNotes","params":{"text":"x"}});257]}),
        json!({"steps":[{"id":"project.setNotes","params":{"text":"x".repeat(1_048_576)}}]}),
        json!({"baseRevision":-1,"steps":[{"id":"project.setNotes","params":{"text":"x"}}]}),
        json!({"steps":[{"id":"project.setNotes","unexpected":true}]}),
        json!({"steps":[{"id":"markers.add","params":[]}]}),
        json!({"steps":[{"id":"markers.add","params":42}]}),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("agent.plan", value)));
        assert!(result.unwrap().is_err());
    }
    assert_eq!(writes.load(Ordering::Relaxed), 0);
    assert_eq!(s.project.notes, "");
}

#[test]
fn linked_timeline_changes_report_both_tracks_and_preserve_live_navigation() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let sequence = s.state.active_sequence.unwrap();
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id.0;
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let id = plan(
        &mut s,
        json!([
            {"id":"timeline.trim","params":{"clip":clip,"edge":"out","mode":"regular","deltaFrames":-8,"linked":true}},
            {"id":"timeline.trim","params":{"clip":clip,"edge":"in","mode":"regular","deltaFrames":4,"linked":true}}
        ]),
    );
    let report = s.execute("agent.preview", json!({"plan":id})).unwrap();
    assert_eq!(report["diff"]["affected"]["clips"].as_array().unwrap().len(), 2);
    assert_eq!(report["diff"]["affected"]["tracks"].as_array().unwrap().len(), 2);
    assert!(report["diff"]["warnings"].as_array().unwrap().iter().any(|w| w["code"] == "linked_media"));
    // Seeking while reviewing does not invalidate the plan or get overwritten by approval.
    s.execute("playhead.set", json!({"frame":42})).unwrap();
    let playhead = s.playhead();
    s.execute("agent.apply", json!({"plan":id})).unwrap();
    assert_eq!(s.playhead(), playhead);
    assert_eq!(s.state.active_sequence, Some(sequence));
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}
