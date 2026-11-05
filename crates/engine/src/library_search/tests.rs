use super::*;
use filmcraft_project::ItemId;
use serde_json::json;
use std::sync::Arc;
fn session() -> (Session, u64) {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":2,"width":160,"height":90})).unwrap();
    let id = item["item"].as_u64().unwrap();
    let mut transcript = filmcraft_project::Transcript::default();
    transcript.words.push(filmcraft_project::Word {
        id: 0,
        text: "price".into(),
        start: Tick::from_units(1, 4),
        end: Tick::from_units(1, 2),
        speaker: None,
        confidence: 1.0,
    });
    Arc::make_mut(&mut s.project).transcripts.insert(ItemId(id), Arc::new(transcript));
    (s, id)
}
#[test]
fn transcript_index_incremental_search_and_selects_are_read_only() {
    let (mut s, id) = session();
    let before = s.project.clone();
    let revision = s.revision;
    let result = s.execute("library.index", json!({"items":[id],"maxFrames":4})).unwrap();
    assert_eq!(result["entries"], 4);
    let unchanged = s.execute("library.index", json!({"items":[id],"maxFrames":4})).unwrap();
    assert_eq!(unchanged["items"][0]["status"], "unchanged");
    let found = s.execute("library.search", json!({"query":"price","thumbnails":true})).unwrap();
    assert!(!found["hits"].as_array().unwrap().is_empty());
    assert_eq!(found["hits"][0]["transcriptEvidence"][0]["text"], "price");
    let hit = found["hits"][0]["id"].as_str().unwrap();
    let plan = s.execute("library.selectsPlan", json!({"hits":[hit]})).unwrap();
    assert_eq!(plan["status"], "staged");
    assert!(Arc::ptr_eq(&before, &s.project));
    assert_eq!(s.revision, revision);
    let applied = s.execute("agent.apply", json!({"plan":plan["plan"]})).unwrap();
    assert!(applied["applied"].as_bool().unwrap_or(false) || s.revision > revision);
    assert_eq!(s.project.sequences().count(), 1);
}
#[test]
fn supplied_semantic_vectors_have_real_cosine_and_provenance() {
    let (mut s, id) = session();
    s.execute("library.index", json!({"items":[id],"maxFrames":2})).unwrap();
    let ids: Vec<_> = s.library_index.entries.keys().cloned().collect();
    let model = json!({"id":"test-local-image-text","dimension":2,"provenance":"Synthetic test vectors, not semantic model inference"});
    s.execute("library.embeddings", json!({"model":model,"entries":[{"id":ids[0],"vector":[3.0,4.0]},{"id":ids[1],"vector":[-3.0,-4.0]}]})).unwrap();
    let result = s.execute("library.search", json!({"query":"wide crowd","embedding":{"model":model,"vector":[6.0,8.0]}})).unwrap();
    assert_eq!(result["hits"][0]["id"], ids[0]);
    assert!((result["hits"][0]["ranking"]["cosine"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    assert_eq!(result["semantic"]["status"], "used_compatible_model_vectors");
    assert!(s.execute("library.embeddings", json!({"model":model,"entries":[{"id":ids[0],"vector":[0.0,0.0]}]})).is_err());
}
#[test]
fn unavailable_semantics_and_changed_transcript_are_explicit() {
    let (mut s, id) = session();
    s.execute("library.index", json!({"items":[id],"maxFrames":2})).unwrap();
    let result = s.execute("library.search", json!({"query":"wide crowd"})).unwrap();
    assert_eq!(result["hits"].as_array().unwrap().len(), 0);
    assert_eq!(result["semantic"]["status"], "unavailable_for_indexed_ranges");
    Arc::make_mut(Arc::make_mut(&mut s.project).transcripts.get_mut(&ItemId(id)).unwrap()).words[0].text = "changed".into();
    let result = s.execute("library.search", json!({"query":"price"})).unwrap();
    assert_eq!(result["staleItems"][0], id);
    assert!(result["hits"].as_array().unwrap().is_empty());
}
#[test]
fn hostile_requests_do_not_mutate_project_or_index() {
    let (mut s, id) = session();
    let project = s.project.clone();
    for (command, params) in [
        ("library.index", json!({"items":[id],"maxFrames":0})),
        ("library.index", json!({"items":[id,id]})),
        ("library.search", json!({"query":""})),
        ("library.search", json!({"query":"test","limit":usize::MAX})),
        ("library.embeddings", json!({"model":{"id":"x","dimension":0,"provenance":"x"},"entries":[]})),
        ("library.selectsPlan", json!({"hits":[]})),
        ("library.clear", json!({"extra":true})),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute(command, params)));
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
        assert!(Arc::ptr_eq(&s.project, &project));
    }
}
