use super::*;
use filmcraft_project::{Caption, CaptionFormat, CaptionTrack, ClipId, TrackId, Transcript, Word};
use std::sync::Arc;
fn setup() -> Session {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":2,"width":160,"height":90})).unwrap()["item"].as_u64().unwrap();
    s.execute("file.newSequence", json!({"width":160,"height":90,"fps":24,"video":1,"audio":1})).unwrap();
    s.execute("timeline.place", json!({"item":item,"track":"V1","frame":0,"sourceIn":{"frame":0},"duration":{"frame":12}})).unwrap();
    s.execute("timeline.place", json!({"item":item,"track":"V1","frame":13,"sourceIn":{"frame":13},"duration":{"frame":1}})).unwrap();
    s.execute("timeline.place", json!({"item":item,"track":"V1","frame":14,"sourceIn":{"frame":14},"duration":{"frame":10}})).unwrap();
    Arc::make_mut(&mut s.project).transcripts.insert(
        ItemId(item),
        Arc::new(Transcript {
            source: "manual".into(),
            words: vec![
                Word::new("hello", Tick::from_units(1, 10), Tick::from_units(1, 5)),
                Word::new("price", Tick::from_units(1, 4), Tick::from_units(3, 4)),
            ],
            ..Default::default()
        }),
    );
    let active = s.state.active_sequence.unwrap();
    let seq = Arc::make_mut(&mut s.project).sequence_mut(active).unwrap();
    let mut track = CaptionTrack::new(TrackId(1000), "captions".into(), CaptionFormat::Subtitle);
    track.captions.push(Caption {
        id: ClipId(1001),
        start: Tick::from_units(3, 5),
        duration: Tick::from_units(1, 8),
        text: "hello".into(),
        speaker: None,
        cue_id: None,
        settings: String::new(),
    });
    seq.caption_tracks.push(track);
    s
}
#[test]
fn finds_structural_gaps_flashes_word_cuts_and_caption_drift_without_edits() {
    let mut s = setup();
    let project = s.project.clone();
    let revision = s.revision;
    let result = s.execute("qc.timeline", json!({"strideFrames":1,"maxFrames":24})).unwrap();
    let findings = result["findings"].as_array().unwrap();
    for kind in ["timeline_black_gap", "possible_flash_frame", "cut_inside_word", "possible_caption_drift"] {
        assert!(findings.iter().any(|f| f["kind"] == kind), "missing {kind}: {result}");
    }
    assert!(Arc::ptr_eq(&project, &s.project));
    assert_eq!(s.revision, revision);
    assert_eq!(result["coverage"]["decode"], "completed");
}
#[test]
fn native_delivery_reopen_checks_wav_audio_and_srt_and_reports_mismatch() {
    let mut s = Session::default();
    let dir = std::env::temp_dir().join(format!("filmcraft-native-qc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tone.wav");
    let captions = dir.join("tone.srt");
    let samples: Vec<_> = (0..4800).map(|n| ((n as f32) * std::f32::consts::TAU * 440.0 / 48000.0).sin() * 0.1).collect();
    std::fs::write(&path, filmcraft_media::wav::write_wav16(&samples, 1, 48000)).unwrap();
    std::fs::write(&captions, b"1\n00:00:00,000 --> 00:00:00,080\ntest\n").unwrap();
    let project = s.project.clone();
    let result=s.execute("qc.export",json!({"path":path,"expected":{"duration":Tick::from_units(1,10).0,"audio":{"present":true,"sampleRate":48000,"channels":1,"streams":1},"captions":{"mode":"sidecar","path":captions,"format":"srt","count":1}}})).unwrap();
    assert_eq!(result["verified"], true, "{result}");
    assert!(result["checks"].as_array().unwrap().iter().any(|c| c["check"] == "native_audio_decode" && c["status"] == "pass"));
    assert!(Arc::ptr_eq(&s.project, &project));
    let result = s
        .execute("qc.export", json!({"path":path,"expected":{"duration":Tick::from_units(1,1).0,"audio":{"present":true,"sampleRate":44100,"channels":2}}}))
        .unwrap();
    assert_eq!(result["verified"], false);
    let result = s.execute("qc.export", json!({"path":path,"expected":{"captions":{"mode":"embedded"}}})).unwrap();
    assert_eq!(result["verified"], false);
    assert_eq!(result["checks"].as_array().unwrap().last().unwrap()["status"], "unknown");
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn hostile_qc_requests_never_panic() {
    let mut s = setup();
    let project = s.project.clone();
    for (command, params) in [
        ("qc.timeline", json!({"sequence":u64::MAX})),
        ("qc.timeline", json!({"start":-1})),
        ("qc.timeline", json!({"flashFrames":0})),
        ("qc.timeline", json!({"clippingFraction":-0.1})),
        ("qc.export", json!({"path":"","expected":{}})),
        ("qc.export", json!({"path":"/does-not-exist","expected":{"frameRate":{"num":0,"den":0}}})),
        ("qc.export", json!({"path":"/does-not-exist","expected":{"duration":i64::MAX}})),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute(command, params)));
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
        assert!(Arc::ptr_eq(&s.project, &project));
    }
}

#[test]
fn exported_caption_difference_after_256_word_prefix_never_passes() {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"width":16,"height":16,"fps":24})).unwrap();
    let active = s.state.active_sequence.unwrap();
    let expected = format!("{} expected", std::iter::repeat_n("same", 256).collect::<Vec<_>>().join(" "));
    let actual = format!("{} wrong", std::iter::repeat_n("same", 256).collect::<Vec<_>>().join(" "));
    let mut track = CaptionTrack::new(TrackId(888), "captions".into(), CaptionFormat::Subtitle);
    track.captions.push(Caption {
        id: ClipId(889),
        start: Tick::ZERO,
        duration: Tick::from_units(1, 1),
        text: expected.clone(),
        speaker: None,
        cue_id: None,
        settings: String::new(),
    });
    Arc::make_mut(&mut s.project).sequence_mut(active).unwrap().caption_tracks.push(track);
    assert!(caption_text_equal(&expected, &expected).unwrap());
    assert!(!caption_text_equal(&actual, &expected).unwrap());
    let dir = std::env::temp_dir().join(format!("filmcraft-complete-caption-qc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("captions.srt");
    std::fs::write(&path, format!("1\n00:00:00,000 --> 00:00:01,000\n{actual}\n")).unwrap();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":2,"width":16,"height":16})).unwrap()["item"].as_u64().unwrap();
    let info = s.project.item(ItemId(item)).unwrap().as_media().unwrap().info.clone();

    let checks = caption_delivery(
        &s,
        &info,
        &CaptionSpec { mode: "sidecar".into(), path: Some(path.to_string_lossy().into_owned()), format: Some("srt".into()), count: Some(1) },
        Some(active.0),
        Tick::ZERO,
    )
    .unwrap();
    assert!(checks.iter().any(|c| c["check"] == "caption_sequence_content_and_timing" && c["status"] == "fail"));
    std::fs::remove_dir_all(dir).unwrap();
}
