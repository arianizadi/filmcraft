use crate::Session;
use filmcraft_project::ClipId;
use serde_json::json;
fn session() -> (Session, u64) {
    let mut s = Session::default();
    let item = s.execute("file.newColorMatte", json!({"width":16,"height":16,"seconds":1})).unwrap()["item"].as_u64().unwrap();
    s.execute("file.newSequence", json!({"width":16,"height":16,"fps":24})).unwrap();
    let clip = s.execute("timeline.place", json!({"item":item,"video":true,"audio":false})).unwrap()["clips"][0].as_u64().unwrap();
    (s, clip)
}
fn add(s: &mut Session, clip: u64) -> String {
    s.execute("grade.patch", json!({"clip":clip,"effectInstance":"new","expectedRevision":s.revision,"enabled":true})).unwrap()["effectInstance"]
        .as_str()
        .unwrap()
        .to_string()
}
#[test]
fn stable_grade_identity_survives_reorder_and_removal_before_it() {
    let (mut s, clip) = session();
    let first = add(&mut s, clip);
    let second = add(&mut s, clip);
    let id = s.state.active_sequence.unwrap();
    s.edit("remove earlier grade", |p, _| {
        let (_, it) = p.sequence_mut(id).unwrap().find_item_mut(ClipId(clip)).unwrap();
        let first = filmcraft_project::effect_ids::find_instance(&it.effects, &first).unwrap();
        it.effects.remove(first);
        Ok(())
    })
    .unwrap();
    let result = s.execute("grade.get", json!({"clip":clip,"effectInstance":second})).unwrap();
    assert_eq!(result["effectInstance"], second);
    assert_eq!(result["legacyEffectInstance"], "lumetri:0");
    s.execute("grade.patch", json!({"clip":clip,"effectInstance":second,"expectedRevision":s.revision,"params":{"exposure":{"value":{"Float":0.25}}}}))
        .unwrap();
    assert!(s.execute("grade.get", json!({"clip":clip,"effectInstance":first})).is_err());
}
#[test]
fn undo_does_not_recycle_ids_and_save_reopen_keeps_the_floor() {
    let (mut s, clip) = session();
    let removed = add(&mut s, clip);
    s.undo().unwrap();
    let next = add(&mut s, clip);
    assert_ne!(removed, next);
    s.undo().unwrap();
    let dir = crate::temp_dir().join(format!("filmcraft-effect-id-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ids.fcproj");
    s.execute("file.saveCopy", json!({"path":file})).unwrap();
    let mut reopened = Session::default();
    reopened.execute("file.open", json!({"path":file})).unwrap();
    let fresh = add(&mut reopened, clip);
    assert_ne!(fresh, removed);
    assert_ne!(fresh, next);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn copied_grade_keeps_target_identity_and_new_copy_gets_fresh_identity() {
    let (mut s, clip) = session();
    let source = add(&mut s, clip);
    let target = add(&mut s, clip);
    let result = s
        .execute(
            "grade.copy",
            json!({"source":{"clip":clip,"effectInstance":source},"targets":[{"clip":clip,"effectInstance":target}],"expectedRevision":s.revision}),
        )
        .unwrap();
    assert_eq!(result["targets"][0]["effectInstance"], target);
    let result = s
        .execute(
            "grade.copy",
            json!({"source":{"clip":clip,"effectInstance":source},"targets":[{"clip":clip,"effectInstance":"new","enabled":true}],"expectedRevision":s.revision}),
        )
        .unwrap();
    let copied = result["targets"][0]["effectInstance"].as_str().unwrap();
    assert_ne!(copied, source);
    assert_ne!(copied, target);
    let reset = s.execute("grade.reset", json!({"clip":clip,"effectInstance":target,"expectedRevision":s.revision})).unwrap();
    assert_eq!(reset["effectInstance"], target);
}
#[test]
fn staged_grade_ids_are_exact_on_apply_and_stale_legacy_writes_refuse() {
    let (mut s, clip) = session();
    let plan=s.execute("agent.plan",json!({"steps":[{"id":"grade.patch","params":{"clip":clip,"effectInstance":"new","enabled":true}},{"id":"grade.patch","params":{"clip":clip,"effectInstance":{"$step":0,"path":"/effectInstance"},"params":{"exposure":{"value":{"Float":0.4}}}}}]})).unwrap();
    let stable = plan["steps"][0]["effectInstance"].as_str().unwrap().to_string();
    let revision = s.revision;
    s.execute("agent.apply", json!({"plan":plan["plan"]})).unwrap();
    let result = s.execute("grade.get", json!({"clip":clip,"effectInstance":stable})).unwrap();
    assert_eq!(result["effectInstance"], stable);
    assert!(s.execute("grade.patch", json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":revision,"enabled":false})).is_err());
}

#[test]
fn render_preview_hashes_ignore_identity_migration_for_video_and_audio() {
    let (mut s, _) = session();
    let tone = s.execute("file.newBarsAndTone", json!({"seconds":1})).unwrap()["item"].as_u64().unwrap();
    s.execute("timeline.place", json!({"item":tone,"video":false,"audio":true,"audioTrack":"new","frame":0})).unwrap();
    let seq = s.state.active_sequence.unwrap();
    let video = |p: &filmcraft_project::Project| filmcraft_render::preview::video_segments(p, seq).into_iter().map(|v| v.hash).collect::<Vec<_>>();
    let audio = |p: &filmcraft_project::Project| filmcraft_render::preview::audio_segments(p, seq).into_iter().map(|v| v.hash).collect::<Vec<_>>();
    let before = (video(&s.project), audio(&s.project));
    assert!(!before.0.is_empty() && !before.1.is_empty());
    let mut migrated = (*s.project).clone();
    for clip in migrated.sequence_mut(seq).unwrap().all_tracks_mut().flat_map(|t| &mut t.items) {
        for effect in &mut clip.effects {
            effect.instance_id = 0;
        }
    }
    migrated.ensure_effect_instance_ids().unwrap();
    assert_eq!((video(&migrated), audio(&migrated)), before);
}
