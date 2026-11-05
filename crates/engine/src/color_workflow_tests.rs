use crate::Session;
use filmcraft_project::{ClipId, EffectInstance, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::{Value, json};
use std::sync::Arc;

fn fixture() -> (Session, u64, u64) {
    use filmcraft_media::{Generator, generators::GeneratorSource};
    let mut s = Session::default();
    let mut project = filmcraft_project::Project::new("Color workflow");
    let settings = filmcraft_project::SequenceSettings { width: 32, height: 18, frame_rate: filmcraft_time::FrameRate::FPS_24, ..Default::default() };
    let rate = settings.frame_rate;
    let seq = project.new_sequence("Shots", settings, 1, 0, None);
    let duration = Tick(TICKS_PER_SECOND);
    let mut clips = Vec::new();
    for (n, color) in [[0.18, 0.18, 0.18, 1.0], [0.08, 0.11, 0.15, 1.0]].into_iter().enumerate() {
        let source = GeneratorSource::new(Generator::ColorMatte { color }, 32, 18, rate, duration);
        let item = crate::demo::add_generator(&mut project, &s.media, source, "Source", filmcraft_project::Label::Iris, None);
        let mut clip = project.make_track_item(item, TrackKind::Video, Tick(n as i64 * TICKS_PER_SECOND), TimeRange::new(Tick::ZERO, duration), rate).unwrap();
        for effect in &mut clip.effects {
            filmcraft_project::resolve_auto_points(effect, (32, 18), (32, 18));
        }
        clips.push(clip.id.0);
        project.sequence_mut(seq).unwrap().video_tracks[0].items.push(clip);
    }
    s.project = Arc::new(project);
    s.state.active_sequence = Some(seq);
    (s, clips[0], clips[1])
}
fn run(s: &mut Session, cmd: &str, mut p: Value) -> Value {
    p["expectedRevision"] = json!(s.revision);
    s.execute(cmd, p).unwrap()
}
fn grade(s: &mut Session, clip: u64, exposure: f64) {
    run(s, "grade.patch", json!({"clip":clip,"effectInstance":"new","params":{"exposure":{"value":{"Float":exposure}}}}));
}
fn image(s: &Session, clip: u64) -> filmcraft_render::Image {
    let q = s.active_sequence().unwrap();
    let (_, it) = q.find_item(ClipId(clip)).unwrap();
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    filmcraft_render::render_clip(
        &s.project,
        s.state.active_sequence.unwrap(),
        ClipId(clip),
        it.start,
        filmcraft_render::RenderOptions { working_output: true, ..Default::default() },
        &provider,
    )
    .unwrap()
}
fn local(s: &Session, clip: u64) -> Vec<EffectInstance> {
    s.active_sequence().unwrap().find_item(ClipId(clip)).unwrap().1.effects.clone()
}

#[test]
fn still_snapshot_versions_and_shared_look_persist_and_render() {
    let (mut s, a, b) = fixture();
    grade(&mut s, a, 0.6);
    grade(&mut s, b, -0.2);
    let still = run(&mut s, "gallery.save", json!({"name":"Hero","target":"clip","clip":a,"frame":0,"maxSide":32}))["still"].as_u64().unwrap();
    let saved = s.execute("gallery.get", json!({"still":"hero"})).unwrap();
    let png = saved["pngBase64"].as_str().unwrap();
    assert!(png.starts_with("iVBOR"));
    let look = run(&mut s, "grade.look.save", json!({"name":"Camera A","group":"Camera","clip":a}))["look"].as_u64().unwrap();
    let trim = local(&s, b);
    let before = image(&s, b);
    run(&mut s, "grade.look.link", json!({"look":look,"clip":b}));
    let linked = image(&s, b);
    let revision = s.revision;
    let history = s.history.undo.len();
    let again = run(&mut s, "grade.look.link", json!({"look":look,"clip":b}));
    assert_eq!(again["unchanged"], true);
    assert_eq!(s.revision, revision);
    assert_eq!(s.history.undo.len(), history);
    assert_ne!(before.px, linked.px);
    assert_eq!(local(&s, b), trim);
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    let seq = s.state.active_sequence.unwrap();
    let time = s.active_sequence().unwrap().find_item(ClipId(b)).unwrap().1.start;
    let plan =
        filmcraft_render::plan::plan_frame(&s.project, seq, time, filmcraft_render::RenderOptions { working_output: true, ..Default::default() }, &provider);
    let planned = filmcraft_render::plan::execute_cpu(&plan);
    assert!(linked.px.iter().zip(&planned.px).all(|(a, b)| (a - b).abs() < 0.004));
    run(&mut s, "grade.version.save", json!({"name":"Matched","clip":b}));
    run(&mut s, "grade.patch", json!({"clip":a,"effectInstance":"lumetri:0","params":{"exposure":{"value":{"Float":1.0}}}}));
    run(&mut s, "grade.look.update", json!({"look":look,"clip":a}));
    assert_ne!(image(&s, b).px, linked.px);
    assert_eq!(local(&s, b), trim);
    let before_unlink = image(&s, b);
    run(&mut s, "grade.look.unlink", json!({"clip":b}));
    assert_eq!(before_unlink.px, image(&s, b).px);
    run(&mut s, "grade.version.apply", json!({"clip":b,"version":"matched"}));
    assert_eq!(image(&s, b).px, linked.px);
    assert_eq!(s.execute("gallery.get", json!({"still":still})).unwrap()["pngBase64"], saved["pngBase64"]);
    let reopened = filmcraft_format::decode(&filmcraft_format::encode(&s.project, false)).unwrap().project;
    assert_eq!(reopened.color_workflow, s.project.color_workflow);
    assert!(reopened.next_id > reopened.color_workflow.max_id());
    let inspected = s.execute("grade.get", json!({"clip":b})).unwrap();
    assert!(inspected["sharedEffects"].as_array().unwrap().is_empty());
}
#[test]
fn gallery_matching_uses_frozen_sample_after_original_changes() {
    let (mut s, a, b) = fixture();
    run(&mut s, "gallery.save", json!({"name":"Original","clip":a,"target":"clip","frame":0,"maxSide":32}));
    let frozen = s.project.color_workflow.gallery.values().next().unwrap().reference.clone();
    grade(&mut s, a, 2.0);
    let matched = run(
        &mut s,
        "grade.matchBatch",
        json!({"reference":{"gallery":"Original"},"targets":[{"clip":b,"effectInstance":"new"}],"mode":"exposure","minConfidence":0.0}),
    );
    assert_eq!(matched["reference"]["immutable"], true);
    assert_eq!(matched["matched"], 1);
    let result = &matched["results"][0];
    assert!(result["distanceAfter"].as_f64().unwrap() < result["distanceBefore"].as_f64().unwrap());
    assert_eq!(s.project.color_workflow.gallery.values().next().unwrap().reference, frozen);
}
#[test]
fn color_library_rejections_are_atomic_and_versions_preserve_noncolor_order() {
    let (mut s, a, b) = fixture();
    grade(&mut s, a, 0.5);
    let seq = s.state.active_sequence.unwrap();
    s.edit("Add blur", |p, _| {
        let (_, clip) = p.sequence_mut(seq).unwrap().find_item_mut(ClipId(a)).unwrap();
        clip.effects.push(filmcraft_project::find_effect("gaussian_blur").unwrap().instance());
        Ok(())
    })
    .unwrap();
    grade(&mut s, a, 0.2);
    let original = local(&s, a);
    run(&mut s, "grade.version.save", json!({"clip":a,"name":"Ordered"}));
    run(&mut s, "grade.reset", json!({"clip":a,"effectInstance":"lumetri:1"}));
    run(&mut s, "grade.version.apply", json!({"clip":a,"version":"Ordered"}));
    assert_eq!(local(&s, a), original);
    let mut attempts = vec![
        ("grade.look.link", json!({"clip":b,"look":999})),
        ("gallery.save", json!({"name":"bad","clip":a,"frame":999999999999u64})),
        ("grade.version.apply", json!({"clip":a,"version":"none"})),
        ("grade.look.save", json!({"name":"","clip":a})),
    ];
    for (cmd, mut params) in attempts.drain(..) {
        params["expectedRevision"] = json!(s.revision);
        let before = s.project.to_json();
        let history = s.history.undo.len();
        assert!(s.execute(cmd, params).is_err());
        assert_eq!(before, s.project.to_json());
        assert_eq!(history, s.history.undo.len());
    }
    let look = run(&mut s, "grade.look.save", json!({"name":"Master","clip":a}))["look"].as_u64().unwrap();
    run(&mut s, "grade.look.link", json!({"clip":b,"look":look}));
    s.edit("Lock", |p, _| {
        p.sequence_mut(seq).unwrap().video_tracks[0].locked = true;
        Ok(())
    })
    .unwrap();
    let before = s.project.to_json();
    assert!(s.execute("grade.look.update", json!({"look":look,"clip":a,"expectedRevision":s.revision})).is_err());
    assert_eq!(s.project.to_json(), before);
}
#[test]
fn cdl_roundtrip_and_lut_bake_match_actual_color_effects() {
    let (mut s, a, _) = fixture();
    let c = filmcraft_color::cdl::AscCdl {
        id: "shot-1".into(),
        description: "Warm skin".into(),
        slope: [1.12, 0.96, 0.89],
        offset: [-0.01, 0.02, 0.0],
        power: [1.0, 1.1, 0.93],
        saturation: 0.87,
    };
    run(&mut s, "grade.cdl.import", json!({"clip":a,"effectInstance":"new","xml":c.to_xml().unwrap()}));
    let out = s.execute("grade.cdl.export", json!({"clip":a,"effectInstance":"asc_cdl:0","id":c.id,"description":c.description})).unwrap();
    assert_eq!(filmcraft_color::cdl::parse_xml(out["text"].as_str().unwrap()).unwrap(), vec![c]);
    grade(&mut s, a, 0.3);
    run(
        &mut s,
        "grade.patch",
        json!({"clip":a,"effectInstance":"lumetri:0","params":{"wheel_midtones":{"value":{"Vec2":{"x":0.04,"y":-0.03}}},"curve_red":{"value":{"Curve":[[0.0,0.0],[0.5,0.56],[1.0,1.0]]}}}}),
    );
    let bake = s.execute("grade.bakeLut", json!({"clip":a,"size":33,"encoding":"working-linear"})).unwrap();
    assert_eq!(bake["status"], "baked");
    let lut = filmcraft_color::Lut3d::parse_cube(bake["text"].as_str().unwrap()).unwrap();
    let q = s.active_sequence().unwrap();
    let it = q.find_item(ClipId(a)).unwrap().1;
    let cx = filmcraft_render::effects::FxCtx {
        t: Tick::ZERO,
        px_scale: 1.0,
        seconds: 0.0,
        timecode: "",
        clip_name: "",
        project: Some(&s.project),
        env: None,
        frame_rate: 24.0,
        working: q.settings.color.working,
    };
    for rgb in [[0.25, 0.5, 0.75], [0.16, 0.31, 0.52], [0.8, 0.1, 0.3]] {
        let mut actual = filmcraft_render::Image::filled(1, 1, [rgb[0], rgb[1], rgb[2], 1.0]);
        for e in it.effects.iter().filter(|e| filmcraft_project::color_workflow::is_color_effect(&e.effect)) {
            filmcraft_render::effects::apply(&mut actual, e, &cx);
        }
        let baked = lut.apply(rgb);
        assert!(baked.iter().zip(&actual.px).all(|(a, b)| (a - b).abs() < 0.006), "{baked:?} {:?}", actual.px);
    }
}
#[test]
fn bake_reports_spatial_controls_and_never_writes_without_explicit_partial_consent() {
    let (mut s, a, _) = fixture();
    grade(&mut s, a, 0.2);
    run(&mut s, "grade.patch", json!({"clip":a,"effectInstance":"lumetri:0","params":{"vignette_amount":{"value":{"Float":-1.0}}}}));
    let path = std::env::temp_dir().join(format!("filmcraft-partial-{}.cube", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let result = s.execute("grade.bakeLut", json!({"clip":a,"size":5,"path":path})).unwrap();
    assert_eq!(result["status"], "needs_explicit_partial_consent");
    assert!(!path.exists());
    let baked = s.execute("grade.bakeLut", json!({"clip":a,"size":5,"allowPartial":true,"path":path})).unwrap();
    assert_eq!(baked["status"], "baked");
    assert!(path.exists());
    assert!(s.execute("grade.bakeLut", json!({"clip":a,"size":5,"allowPartial":true,"path":path})).is_err());
    std::fs::remove_file(path).unwrap();
    for request in [
        json!({"clip":a,"size":0}),
        json!({"clip":a,"size":1000000000u64}),
        json!({"clip":a,"domainMin":[1,0,0],"domainMax":[0,1,1]}),
        json!({"clip":a,"sampleAnimation":"yes"}),
    ] {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("grade.bakeLut", request))).unwrap().is_err());
    }
    let before = s.project.to_json();
    assert!(s.execute("grade.cdl.import", json!({"clip":a,"effectInstance":"new","xml":"<!DOCTYPE x><x/>","expectedRevision":s.revision})).is_err());
    assert_eq!(s.project.to_json(), before);
}

#[test]
fn shared_look_follows_split_and_paste_and_invalidates_preview_hashes() {
    let (mut s, a, b) = fixture();
    grade(&mut s, a, 0.4);
    let look = run(&mut s, "grade.look.save", json!({"clip":a,"name":"Scene"}))["look"].as_u64().unwrap();
    run(&mut s, "grade.look.link", json!({"clip":b,"look":look}));
    let sequence = s.state.active_sequence.unwrap();
    let oldhash = filmcraft_render::preview::video_segments(&s.project, sequence).into_iter().find(|v| v.clips.contains(&ClipId(b))).unwrap().hash;
    run(&mut s, "grade.patch", json!({"clip":a,"effectInstance":"lumetri:0","params":{"exposure":{"value":{"Float":0.8}}}}));
    run(&mut s, "grade.look.update", json!({"clip":a,"look":look}));
    let newhash = filmcraft_render::preview::video_segments(&s.project, sequence).into_iter().find(|v| v.clips.contains(&ClipId(b))).unwrap().hash;
    assert_ne!(oldhash, newhash);
    s.execute("timeline.razor", json!({"clip":b,"time":3*TICKS_PER_SECOND/2})).unwrap();
    let linked = filmcraft_project::color_workflow::ColorWorkflow::linked_clips(&s.project, look);
    assert_eq!(linked.len(), 2);
    s.state.selection = vec![ClipId(b)];
    s.execute("edit.copy", json!({})).unwrap();
    s.set_playhead(Tick(3 * TICKS_PER_SECOND));
    s.execute("edit.paste", json!({})).unwrap();
    let linked = filmcraft_project::color_workflow::ColorWorkflow::linked_clips(&s.project, look);
    assert_eq!(linked.len(), 3);
    assert!(s.project.color_workflow.validate_references(&s.project).is_ok());
    let before = s.project.to_json();
    assert!(s.execute("grade.look.delete", json!({"look":look,"expectedRevision":s.revision})).is_err());
    assert_eq!(s.project.to_json(), before);
}

#[test]
fn selection_subset_excludes_unselected_saved_pixels_versions_and_whole_project_review() {
    use filmcraft_project::review::{Activity, Annotation, Checkpoint, EditingProfile};
    let (mut s, a, b) = fixture();
    let kept_sequence = s.state.active_sequence.unwrap();
    let mut project = (*s.project).clone();
    let settings = project.sequence(kept_sequence).unwrap().settings.clone();
    let mut excluded_clip = project.sequence_mut(kept_sequence).unwrap().video_tracks[0].items.pop().unwrap();
    let excluded_source = excluded_clip.item;
    excluded_clip.start = Tick::ZERO;
    let excluded_sequence = project.new_sequence("private-excluded-scene", settings, 1, 0, None);
    project.sequence_mut(excluded_sequence).unwrap().video_tracks[0].items.push(excluded_clip);
    s.project = Arc::new(project);
    grade(&mut s, a, 0.4);
    let look_a = run(&mut s, "grade.look.save", json!({"clip":a,"name":"Kept camera"}))["look"].as_u64().unwrap();
    run(&mut s, "grade.look.link", json!({"clip":a,"look":look_a}));
    let profile_look = run(&mut s, "grade.look.save", json!({"clip":a,"name":"Profile reference"}))["look"].as_u64().unwrap();
    run(&mut s, "grade.version.save", json!({"clip":a,"name":"Kept version"}));
    let still_a = run(&mut s, "gallery.save", json!({"clip":a,"target":"clip","frame":0,"maxSide":32,"name":"Kept still"}))["still"].as_u64().unwrap();
    s.state.active_sequence = Some(excluded_sequence);
    grade(&mut s, b, 0.7);
    let look_b = run(&mut s, "grade.look.save", json!({"clip":b,"name":"private-excluded-look"}))["look"].as_u64().unwrap();
    run(&mut s, "grade.look.link", json!({"clip":b,"look":look_b}));
    run(&mut s, "grade.version.save", json!({"clip":b,"name":"private-excluded-version"}));
    let still_b =
        run(&mut s, "gallery.save", json!({"clip":b,"target":"clip","frame":0,"maxSide":32,"name":"private-excluded-still"}))["still"].as_u64().unwrap();
    let excluded_png = s.project.color_workflow.gallery[&still_b].png_base64.clone();
    let mut project = (*s.project).clone();
    let document: Arc<str> = serde_json::to_string(&project).unwrap().into();
    project.review.checkpoints.push(Checkpoint {
        id: 1,
        name: "private-excluded-checkpoint".into(),
        created_utc: String::new(),
        revision: s.revision,
        document,
    });
    project.review.activity.push(Activity {
        id: 2,
        command: "grade.patch".into(),
        label: "private-excluded-source.mov".into(),
        before_revision: 0,
        after_revision: 1,
        created_utc: String::new(),
    });
    for (id, sequence, text) in [(3, kept_sequence, "Retained note"), (4, excluded_sequence, "private-excluded-annotation")] {
        project.review.annotations.push(Annotation {
            id,
            sequence,
            start: Tick::ZERO,
            end: Tick(TICKS_PER_SECOND),
            text: text.into(),
            kind: "note".into(),
            author: "test".into(),
            status: "open".into(),
            created_utc: String::new(),
        });
    }
    project.review.next_id = 4;
    project.review.profiles.insert(
        "Editing defaults".into(),
        vec![EditingProfile { name: "Editing defaults".into(), reference_look: Some("Profile reference".into()), ..Default::default() }],
    );
    project.review.active_profile = Some("Editing defaults".into());
    let kept_source = project.sequence(kept_sequence).unwrap().find_item(ClipId(a)).unwrap().1.item;
    let original_review = project.review.clone();
    let sub = crate::project_tools::project_subset(&project, &[kept_sequence, kept_source].into_iter().collect());
    assert!(!sub.items.contains_key(&excluded_sequence));
    assert!(!sub.items.contains_key(&excluded_source));
    assert_eq!(sub.color_workflow.gallery.keys().copied().collect::<Vec<_>>(), [still_a]);
    assert_eq!(sub.color_workflow.versions.keys().copied().collect::<Vec<_>>(), [ClipId(a)]);
    assert_eq!(sub.color_workflow.looks.keys().copied().collect::<Vec<_>>(), [look_a, profile_look]);
    assert!(sub.review.checkpoints.is_empty());
    assert!(sub.review.activity.is_empty());
    assert_eq!(sub.review.annotations.len(), 1);
    assert_eq!(sub.review.annotations[0].sequence, kept_sequence);
    assert_eq!(sub.review.profiles, project.review.profiles);
    sub.color_workflow.validate().unwrap();
    sub.color_workflow.validate_references(&sub).unwrap();
    sub.review.check().unwrap();
    let encoded = filmcraft_format::encode(&sub, false);
    let text = std::str::from_utf8(&encoded).unwrap();
    assert!(!text.contains("private-excluded"));
    assert!(!text.contains(excluded_png.as_ref()));
    filmcraft_format::decode(&encoded).unwrap();
    assert_eq!(project.color_workflow.gallery.len(), 2);
    assert_eq!(project.review.checkpoints.len(), 1);
    assert_eq!(project.review, original_review);
}
