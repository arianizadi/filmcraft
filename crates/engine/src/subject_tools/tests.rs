use super::*;
use std::sync::Arc;
fn setup() -> (Session, u64) {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":2,"width":160,"height":90})).unwrap()["item"].as_u64().unwrap();
    s.execute("file.newSequence", json!({"width":160,"height":90,"frameRate":{"num":24000,"den":1001},"video":1,"audio":1})).unwrap();
    let clip = s.execute("timeline.place", json!({"item":item,"track":"V1","frame":0,"duration":{"frame":24}})).unwrap()["clips"][0].as_u64().unwrap();
    (s, clip)
}
#[test]
fn track_preview_apply_correct_and_reframe_are_one_undo_and_preserve_source() {
    let (mut s, clip) = setup();
    let revision = s.revision;
    let before = s.project.clone();
    let seq = s.state.active_sequence.unwrap();
    // Click a real color bar; the tracker isolates its connected color region.
    let plan = s.execute("subject.trackPlan", json!({"clip":clip,"end":Tick::from_units(1,4).0,"maxFrames":6,"prompt":{"point":[0.2,0.2]}})).unwrap();
    assert_eq!(plan["status"], "staged");
    assert!(Arc::ptr_eq(&s.project, &before));
    assert_eq!(s.revision, revision);
    let staged = s.agent_plan_snapshot(plan["plan"].as_u64().unwrap(), false).unwrap().0;
    let it = staged.sequence(seq).unwrap().find_item(ClipId(clip)).unwrap().1;
    let alpha = it.effect("opacity").unwrap().masks[0].alpha.as_ref().unwrap();
    assert_eq!(alpha.frames.len(), 6);
    assert!(alpha.frames[0].decode().unwrap().contains(&0));
    assert!(alpha.frames[0].decode().unwrap().contains(&255));
    let rendered = s.execute("render.frames", json!({"plan":plan["plan"],"sequence":seq.0,"frames":[0],"maxSide":160})).unwrap();
    assert!(!rendered["frames"].as_array().unwrap().is_empty());
    let receipt = s.execute("agent.apply", json!({"plan":plan["plan"]})).unwrap();
    assert_eq!(receipt["undoSteps"], 1);
    let tracked = s.project.clone();
    let correction = s.execute("subject.correctPlan", json!({"clip":clip,"mask":0,"mediaTime":0,"prompt":{"point":[0.2,0.2]}})).unwrap();
    assert!(Arc::ptr_eq(&tracked, &s.project));
    s.execute("agent.apply", json!({"plan":correction["plan"]})).unwrap();
    assert!(s.active_sequence().unwrap().find_item(ClipId(clip)).unwrap().1.effect("opacity").unwrap().masks[0].alpha.as_ref().unwrap().frames[0].corrected);
    let original = s.project.clone();
    let variant = s.execute("subject.reframePlan", json!({"clips":[clip],"width":90,"height":160})).unwrap();
    assert_eq!(variant["status"], "staged");
    assert!(Arc::ptr_eq(&original, &s.project));
    let receipt = s.execute("agent.apply", json!({"plan":variant["plan"]})).unwrap();
    assert_eq!(receipt["undoSteps"], 1);
    assert_eq!(s.project.sequences().count(), 2);
    assert_ne!(s.state.active_sequence, Some(seq));
    let reframed = s.active_sequence().unwrap().find_item(ClipId(clip)).unwrap().1;
    assert!(reframed.effect("motion").unwrap().param("position").unwrap().is_animated());
    assert_eq!(reframed.effect("opacity").unwrap().masks[0].mode, MaskMode::None);
    assert_eq!(s.project.sequence(seq), original.sequence(seq));
}
#[test]
fn hostile_subject_requests_never_panic_and_do_not_edit() {
    let (mut s, clip) = setup();
    let before = s.project.clone();
    for (command, p) in [
        ("subject.trackPlan", json!({"clip":clip,"prompt":{"rect":[0,0,2,1]}})),
        ("subject.trackPlan", json!({"clip":clip,"prompt":{"point":[0.5,0.5]},"maxFrames":usize::MAX})),
        ("subject.trackPlan", json!({"clip":clip,"prompt":{"point":[0.5,0.5]},"strideFrames":0})),
        ("subject.correctPlan", json!({"clip":clip,"mask":0,"mediaTime":i64::MAX,"prompt":{"point":[0.5,0.5]}})),
        ("subject.reframePlan", json!({"clips":[clip,clip],"width":90,"height":160})),
        ("subject.reframePlan", json!({"clips":[clip],"width":0,"height":160})),
        (
            "subject.installAlpha",
            json!({"clip":clip,"expectedRevision":s.revision,"alpha":{"sourceWidth":160,"sourceHeight":90,"end":1,"method":"x","provenance":"x","frames":[{"time":0,"width":1,"height":1,"runs":[u32::MAX],"confidence":1}]}}),
        ),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute(command, p)));
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
        assert!(Arc::ptr_eq(&before, &s.project));
    }
}

#[test]
#[ignore = "Set FILMCRAFT_ANALYSIS_REAL_SOURCE and FILMCRAFT_SUBJECT_TEST_OUTPUT to authorized local paths"]
fn real_8k_source_pixel_subject_mask_review() {
    use base64::Engine as _;
    let path = std::env::var("FILMCRAFT_ANALYSIS_REAL_SOURCE").unwrap();
    let output = std::env::var("FILMCRAFT_SUBJECT_TEST_OUTPUT").unwrap();
    std::fs::create_dir_all(&output).unwrap();
    let mut s = Session::default();
    let item = s.execute("file.import", json!({"paths":[path]})).unwrap()["items"][0].as_u64().unwrap();
    let media = s.project.resolve_media(ItemId(item)).unwrap().1;
    let settings = crate::commands::default_seq_settings_for(media);
    let duration = settings.frame_rate.tick_of(3).0;
    s.execute("file.newSequence", json!({"width":settings.width,"height":settings.height,"frameRate":settings.frame_rate,"video":1,"audio":1})).unwrap();
    let clip = s.execute("timeline.place", json!({"item":item,"frame":0,"duration":duration})).unwrap()["clips"][0].as_u64().unwrap();
    let before = s.project.clone();
    let revision = s.revision;
    // Source inspected visually: this box surrounds the foreground red cup body on the desk.
    // It is a first-party color-segmentation exercise, not a semantic person-mask benchmark.
    let plan = s.execute("subject.trackPlan", json!({"clip":clip,"maxFrames":3,"maxSide":192,"prompt":{"rect":[0.75,0.36,0.15,0.27]}})).unwrap();
    assert_eq!(plan["status"], "staged");
    assert!(Arc::ptr_eq(&before, &s.project));
    assert_eq!(s.revision, revision);
    let preview = s.execute("render.frames", json!({"plan":plan["plan"],"comparison":true,"frames":[0,1,2],"maxSide":960,"layout":"contactSheet"})).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(preview["contactSheet"]["base64"].as_str().unwrap()).unwrap();
    std::fs::write(std::path::Path::new(&output).join("subject-before-after.png"), bytes).unwrap();
    std::fs::write(std::path::Path::new(&output).join("subject-tracking-plan.json"), serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    s.execute("agent.apply", json!({"plan":plan["plan"]})).unwrap();
    let inspected = s.execute("subject.inspect", json!({"clip":clip,"mediaTime":0})).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(inspected["preview"]["base64"].as_str().unwrap()).unwrap();
    std::fs::write(std::path::Path::new(&output).join("subject-alpha.png"), bytes).unwrap();
    let failed = inspected["frames"].as_array().unwrap().iter().find(|frame| frame["failed"] == true).and_then(|f| f["mediaTime"].as_i64());
    if let Some(time) = failed {
        let corrected =
            s.execute("subject.correctPlan", json!({"clip":clip,"mask":0,"mediaTime":time,"maxSide":192,"prompt":{"rect":[0.75,0.36,0.15,0.27]}})).unwrap();
        let correction_preview =
            s.execute("render.frames", json!({"plan":corrected["plan"],"comparison":true,"time":time,"maxSide":960,"layout":"contactSheet"})).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD.decode(correction_preview["contactSheet"]["base64"].as_str().unwrap()).unwrap();
        std::fs::write(std::path::Path::new(&output).join("subject-correction-before-after.png"), bytes).unwrap();
        s.execute("agent.apply", json!({"plan":corrected["plan"]})).unwrap();
        let corrected = s.execute("subject.inspect", json!({"clip":clip})).unwrap();
        assert!(corrected["frames"].as_array().unwrap().iter().any(|f| f["mediaTime"] == time && f["corrected"] == true && f["failed"] == false));
    }
    let variant = s.execute("subject.reframePlan", json!({"clips":[clip],"width":1080,"height":1920})).unwrap();
    let rendered = s.execute("render.frames", json!({"plan":variant["plan"],"frames":[0],"maxSide":960})).unwrap();
    for (index, frame) in rendered["frames"].as_array().unwrap().iter().enumerate() {
        let image = &frame["image"];
        let bytes = base64::engine::general_purpose::STANDARD.decode(image["base64"].as_str().unwrap()).unwrap();
        std::fs::write(std::path::Path::new(&output).join(format!("subject-vertical-{index}.png")), bytes).unwrap();
    }
}
