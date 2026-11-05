use super::*;
use filmcraft_project::{ItemId, Transcript, Word};
use std::sync::Arc;

fn scene() -> Session {
    let mut s = Session::default();
    let black = s.execute("file.newColorMatte", json!({"color":"#000000","seconds":1,"width":160,"height":90})).unwrap()["item"].as_u64().unwrap();
    let white = s.execute("file.newColorMatte", json!({"color":"#ffffff","seconds":1,"width":160,"height":90})).unwrap()["item"].as_u64().unwrap();
    s.execute("file.newSequence", json!({"width":160,"height":90,"fps":24})).unwrap();
    s.execute("timeline.place", json!({"item":black,"track":"V1","frame":0,"duration":{"frame":24},"audio":false})).unwrap();
    s.execute("timeline.place", json!({"item":white,"track":"V1","frame":24,"duration":{"frame":24},"audio":false})).unwrap();
    s
}

#[test]
fn dense_black_freeze_shot_motion_sharpness_are_read_only_and_located() {
    let mut s = scene();
    let before = s.project.clone();
    let state = serde_json::to_value(&s.state).unwrap();
    let revision = s.revision;
    let result = s.execute("analysis.footage", json!({"subject":{"target":"sequence"},"strideFrames":1,"maxFrames":48,"audio":false})).unwrap();
    assert_eq!(result["video"]["dense"], true);
    assert_eq!(result["frames"].as_array().unwrap().len(), 48);
    let findings = result["findings"].as_array().unwrap();
    assert!(findings.iter().any(|v| v["kind"] == "black_frames" && v["start"]["time"] == 0));
    assert!(findings.iter().any(|v| v["kind"] == "shot_boundary" && v["start"]["frame"] == 24));
    assert!(findings.iter().any(|v| v["kind"] == "frozen_or_static_frames"));
    assert!(!result["frames"][0]["position"]["sources"].as_array().unwrap().is_empty());
    assert!(Arc::ptr_eq(&before, &s.project));
    assert_eq!(s.revision, revision);
    assert_eq!(serde_json::to_value(&s.state).unwrap(), state);
}

#[test]
fn sparse_scan_identifies_its_uncertainty_and_source_timeline_mapping() {
    let mut s = scene();
    let item = s.active_sequence().unwrap().video_tracks[0].items[0].item;
    let result = s.execute("analysis.footage", json!({"subject":{"target":"source","item":item.0},"maxFrames":4,"audio":false})).unwrap();
    assert_eq!(result["video"]["sampledFrames"], 4);
    assert_eq!(result["video"]["dense"], false);
    assert!(!result["start"]["timeline"].as_array().unwrap().is_empty());
}

#[test]
fn actual_audio_waveform_silence_and_local_transcription_work_without_project_edits() {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":1})).unwrap()["item"].as_u64().unwrap();
    s.transcriber = Some(Arc::new(filmcraft_speech::FixedTranscriber {
        id: "test-local-model".into(),
        transcript: Transcript { words: vec![Word::new("Price", Tick::from_units(1, 10), Tick::from_units(3, 10))], ..Default::default() },
    }));
    let before = s.project.clone();
    let result = s.execute("analysis.footage", json!({"subject":{"target":"source","item":item},"video":false,"speech":true})).unwrap();
    assert_eq!(result["speech"]["status"], "inferred");
    assert_eq!(result["speechWords"][0]["text"], "Price");
    assert!(result["waveform"]["base64"].as_str().unwrap().len() > 100);
    assert!(result["audio"]["peak"].as_f64().unwrap() > 0.01);
    assert!(Arc::ptr_eq(&before, &s.project));
    assert!(!s.project.transcripts.contains_key(&ItemId(item)));
}

#[test]
fn stored_transcript_speech_evidence_does_not_claim_energy_is_speech() {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":1})).unwrap()["item"].as_u64().unwrap();
    Arc::make_mut(&mut s.project).transcripts.insert(
        ItemId(item),
        Arc::new(Transcript { source: "manual".into(), words: vec![Word::new("Hello", Tick::ZERO, Tick::from_units(1, 4))], ..Default::default() }),
    );
    let result = s.execute("analysis.footage", json!({"subject":{"target":"source","item":item},"video":false})).unwrap();
    assert_eq!(result["speech"]["source"], "project_transcript");
    assert!(result["speechWords"][0]["evidenceId"].as_str().unwrap().contains("word-"));
}

#[test]
fn hostile_analysis_requests_never_panic_or_modify() {
    let mut s = scene();
    let revision = s.revision;
    for params in [
        json!({"subject":{"item":u64::MAX}}),
        json!({"subject":{},"maxFrames":usize::MAX}),
        json!({"subject":{},"strideFrames":0}),
        json!({"subject":{},"start":-1}),
        json!({"subject":{},"end":i64::MAX}),
        json!({"subject":{},"blackThreshold":-1}),
        json!({"subject":{},"freezeThreshold":1e300}),
        json!({"subject":{"target":"wrong"}}),
        json!({"subject":{},"strideFrames":1,"maxFrames":1}),
        json!({"subject":{},"unknown":true}),
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("analysis.footage", params.clone())));
        assert!(result.is_ok(), "panic {params}");
        assert!(result.unwrap().is_err(), "accepted {params}");
        assert_eq!(s.revision, revision);
    }
}

#[test]
fn native_audio_silence_onsets_and_decode_failure_are_distinct() {
    let mut s = Session::default();
    let dir = std::env::temp_dir().join(format!("filmcraft-analysis-audio-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pulses.wav");
    let samples: Vec<_> = (0..6 * 16000)
        .map(|n| {
            let in_pulse = n % 8000 < 1600 && n < 5 * 16000;
            if in_pulse { (n as f32 * std::f32::consts::TAU * 440.0 / 16000.0).sin() * 0.8 } else { 0.0 }
        })
        .collect();
    std::fs::write(&path, filmcraft_media::wav::write_wav16(&samples, 1, 16000)).unwrap();
    let id = s.execute("file.import", json!({"paths":[path]})).unwrap()["items"][0].as_u64().unwrap();
    let result = s.execute("analysis.footage", json!({"subject":{"item":id},"video":false})).unwrap();
    assert!(result["findings"].as_array().unwrap().iter().any(|f| f["kind"] == "audio_silence"));
    assert!(!result["music"]["onsets"].as_array().unwrap().is_empty());
    assert_eq!(result["speech"]["status"], "transcript_required");
    let root = Arc::make_mut(&mut s.project).item_mut(ItemId(id)).unwrap().as_media_mut().unwrap();
    root.offline = true;
    assert!(s.execute("analysis.footage", json!({"subject":{"item":id},"video":false})).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "Set FILMCRAFT_ANALYSIS_REAL_SOURCE to an authorized local file; preserves source"]
fn real_source_native_analysis_and_index() {
    let path = std::env::var("FILMCRAFT_ANALYSIS_REAL_SOURCE").unwrap();
    let mut s = Session::default();
    let imported = s.execute("file.import", json!({"paths":[path]})).unwrap();
    let item = imported["items"][0].as_u64().unwrap();
    let end = Tick::from_units(1, 10).0;
    let revision = s.revision;
    let project = s.project.clone();
    let result = s.execute("analysis.footage", json!({"subject":{"item":item},"end":end,"maxFrames":3,"music":false})).unwrap();
    assert_eq!(result["video"]["status"], "measured");
    assert!(!result["frames"].as_array().unwrap().is_empty());
    assert_eq!(result["audio"]["status"], "measured");
    s.execute("library.index", json!({"items":[item],"end":end,"maxFrames":3})).unwrap();
    let hits = s.execute("library.search", json!({"query":"landscape","thumbnails":true,"limit":3})).unwrap();
    assert!(!hits["hits"].as_array().unwrap().is_empty());
    assert!(Arc::ptr_eq(&s.project, &project));
    assert_eq!(s.revision, revision);
}
