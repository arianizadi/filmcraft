use super::*;
use filmcraft_export::SampleFormat;
use filmcraft_project::Keyframe;

struct FixtureDir(std::path::PathBuf);
impl FixtureDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let stamp = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap().as_nanos();
        let path = crate::temp_dir().join(format!("filmcraft-audio-channels-{stamp}-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(format: SampleFormat, left: f32, right: f32) -> (Session, u64, FixtureDir) {
    let dir = FixtureDir::new();
    let path = dir.path().join("DJI-style.wav");
    let samples: Vec<f32> = (0..96_000)
        .flat_map(|i| {
            let phase = std::f32::consts::TAU * i as f32 / 48_000.0;
            [(phase * 437.0).sin() * left, (phase * 719.0).sin() * right]
        })
        .collect();
    std::fs::write(&path, filmcraft_export::write_wav(&samples, 2, 48_000, format).unwrap()).unwrap();
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"name":"Audio toolkit","fps":24,"audio":2,"video":1})).unwrap();
    let item = s.execute("file.import", json!({"paths":[path]})).unwrap()["items"][0].clone();
    let placed = s.execute("timeline.place", json!({"item":item,"track":"A1","audio":true,"video":false})).unwrap();
    (s, placed["clips"][0].as_u64().unwrap(), dir)
}
fn audio(s: &Session) -> filmcraft_frame::AudioBuffer {
    let provider = s.media.full_res_provider(s.project.clone(), s.services.clone());
    filmcraft_render::audio::mix_sequence(&s.project, s.active_sequence().unwrap(), 0, 48_000, &provider)
}
fn clip(s: &Session, id: u64) -> &TrackItem {
    s.active_sequence().unwrap().find_item(ClipId(id)).unwrap().1
}

#[test]
fn pcm24_and_float32_left_mic_reach_both_speakers_without_losing_float_headroom() {
    for (format, level) in [(SampleFormat::Int24, 0.7), (SampleFormat::Float32, 2.5)] {
        let (mut s, id, dir) = fixture(format, level, 0.0);
        let path = dir.path().join("DJI-style.wav");
        let bytes = std::fs::read(&path).unwrap();
        let before = audio(&s);
        assert!(before.channels[1].iter().all(|v| v.abs() < 1e-6));
        s.execute("audio.channels.set", json!({"clip":id,"mode":"left"})).unwrap();
        let after = audio(&s);
        assert_eq!(after.channels[0], after.channels[1]);
        assert!(after.channels[0].iter().zip(&before.channels[0]).all(|(a, b)| (a - b).abs() < 1e-6));
        if matches!(format, SampleFormat::Float32) {
            assert!(after.channels[0].iter().any(|v| v.abs() > 2.0));
        }
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(audio(&s).channels, before.channels);
    }
}

#[test]
fn right_swap_and_safe_mono_are_real_routing_not_synthetic_stereo() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.7);
    let before = audio(&s);
    s.execute("audio.channels.set", json!({"clip":id,"mode":"right"})).unwrap();
    let after = audio(&s);
    assert_eq!(after.channels[0], before.channels[1]);
    assert_eq!(after.channels[1], before.channels[1]);
    s.execute("audio.channels.set", json!({"clip":id,"mode":"swap"})).unwrap();
    let after = audio(&s);
    assert_eq!(after.channels[0], before.channels[1]);
    assert_eq!(after.channels[1], before.channels[0]);
    s.execute("audio.channels.set", json!({"clip":id,"mode":"mono"})).unwrap();
    let after = audio(&s);
    for i in 0..after.frames() {
        assert!((after.channels[0][i] - (before.channels[0][i] + before.channels[1][i]) * 0.5).abs() < 1e-6);
    }
    assert_eq!(after.channels[0], after.channels[1]);
}

#[test]
fn partial_routing_patch_preserves_unrelated_animation_and_user_effects() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.0);
    s.execute("effects.apply", json!({"clip":id,"effect":"channel_mixer_a"})).unwrap();
    s.execute("audio.channels.set", json!({"clip":id,"mode":"left"})).unwrap();
    let seq = s.state.active_sequence.unwrap();
    s.edit("Animate output gain", |p, _| {
        let it = p.sequence_mut(seq).unwrap().find_item_mut(ClipId(id)).unwrap().1;
        let gain = it.effects.iter_mut().find(|e| e.effect == ROUTER).unwrap().param_mut("gain_l_db").unwrap();
        gain.keyframes = vec![Keyframe::new(Tick::ZERO, ParamValue::Float(-3.0)), Keyframe::new(Tick::from_units(1, 1), ParamValue::Float(0.0))];
        Ok(())
    })
    .unwrap();
    let animated = router(clip(&s, id), "test").unwrap().unwrap().param("gain_l_db").unwrap().clone();
    s.execute("audio.channels.set", json!({"clip":id,"invertRight":true})).unwrap();
    assert_eq!(router(clip(&s, id), "test").unwrap().unwrap().param("gain_l_db").unwrap(), &animated);
    let before = s.project.clone();
    let revision = s.revision;
    assert!(s.execute("audio.channels.set", json!({"clip":id,"leftGainDb":-6.0})).is_err());
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    assert!(s.execute("audio.channels.reset", json!({"clip":id})).is_err());
    s.execute("audio.channels.reset", json!({"clip":id,"replaceAnimation":true})).unwrap();
    assert!(clip(&s, id).effects.iter().any(|e| e.effect == "channel_mixer_a"));
    assert!(clip(&s, id).effects.iter().all(|e| e.effect != ROUTER));
}

#[test]
fn repair_rejections_preserve_project_revision_and_history() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.0);
    for p in [
        json!({"clip":id,"mode":"magic"}),
        json!({"clip":id,"leftGainDb":999}),
        json!({"clips":[id,id],"mode":"left"}),
        json!({"clip":u64::MAX,"mode":"left"}),
        json!({"clip":id,"mode":"left","bogus":true}),
    ] {
        let before = s.project.clone();
        let revision = s.revision;
        let undo = s.history.undo.len();
        assert!(s.execute("audio.channels.set", p).is_err());
        assert_eq!(s.project, before);
        assert_eq!(s.revision, revision);
        assert_eq!(s.history.undo.len(), undo);
    }
    let seq = s.state.active_sequence.unwrap();
    s.edit("Lock", |p, _| {
        p.sequence_mut(seq).unwrap().audio_tracks[0].locked = true;
        Ok(())
    })
    .unwrap();
    let before = s.project.clone();
    let revision = s.revision;
    assert!(s.execute("audio.channels.set", json!({"clip":id,"mode":"left"})).is_err());
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
}

#[test]
fn audio_plan_is_isolated_one_undo_and_stale_guarded() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.0);
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let plan = s.execute("audio.prepare", json!({"clip":id,"routing":"auto","preset":"dialogueClean"})).unwrap();
    assert_eq!(s.project, before);
    assert_eq!(s.history.undo.len(), undo);
    assert_eq!(plan["status"], "staged");
    s.execute("agent.apply", json!({"plan":plan["plan"]})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 1);
    assert_eq!(router(clip(&s, id), "test").unwrap().unwrap().f64_at("mode", Tick::ZERO), 1.0);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
    let plan = s.execute("audio.prepare", json!({"clip":id,"routing":"left"})).unwrap();
    s.edit("Another edit", |p, _| {
        p.notes = "keep current edit".into();
        Ok(())
    })
    .unwrap();
    let current = s.project.clone();
    assert!(s.execute("agent.apply", json!({"plan":plan["plan"]})).is_err());
    assert_eq!(s.project, current);
}

#[test]
fn normalization_is_after_nonlinear_effects_and_preserves_volume_animation() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 2.5, 2.5);
    s.execute("effects.apply", json!({"clip":id,"effect":"dynamics"})).unwrap();
    let seq = s.state.active_sequence.unwrap();
    s.edit("Volume automation", |p, _| {
        let it = p.sequence_mut(seq).unwrap().find_item_mut(ClipId(id)).unwrap().1;
        let volume = it.effect_mut("volume").unwrap().param_mut("level").unwrap();
        volume.keyframes = vec![Keyframe::new(Tick::ZERO, ParamValue::Float(0.0)), Keyframe::new(Tick::from_units(2, 1), ParamValue::Float(-3.0))];
        Ok(())
    })
    .unwrap();
    let old_gain = clip(&s, id).gain_db;
    let old_keys = clip(&s, id).effect("volume").unwrap().param("level").unwrap().keyframes.clone();
    let report = s.execute("audio.normalize", json!({"clip":id,"targetLufs":-16.0,"truePeakDbtp":-1.0})).unwrap();
    assert_eq!(report["changed"], 1);
    assert_eq!(clip(&s, id).gain_db, old_gain);
    let keys = &clip(&s, id).effect("volume").unwrap().param("level").unwrap().keyframes;
    assert_eq!(keys.iter().map(|k| k.time).collect::<Vec<_>>(), old_keys.iter().map(|k| k.time).collect::<Vec<_>>());
    let measured = s.execute("audio.inspect", json!({"subject":{"clip":id},"start":0,"end":Tick::from_units(2,1).0})).unwrap();
    assert!(measured["loudness"]["truePeakDbtp"].as_f64().unwrap() <= -0.95, "{measured}");
    assert!((measured["loudness"]["integratedLufs"].as_f64().unwrap() - report["clips"][0]["predictedLufs"].as_f64().unwrap()).abs() < 0.1);
}

#[test]
fn silence_is_never_amplified_and_independent_stereo_is_not_auto_replaced() {
    let (mut silent, id, _dir) = fixture(SampleFormat::Float32, 0.0, 0.0);
    let revision = silent.revision;
    assert_eq!(silent.execute("audio.normalize", json!({"clip":id})).unwrap()["changed"], 0);
    assert_eq!(silent.revision, revision);
    let (mut stereo, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.7);
    let before = stereo.project.clone();
    assert_eq!(stereo.execute("audio.channels.set", json!({"clip":id,"mode":"auto"})).unwrap()["changed"], 0);
    assert_eq!(stereo.project, before);
}

#[test]
fn main_and_camera_streams_stay_separate_and_explicit_audio_targets_do_not_expand() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.0);
    let sequence = s.state.active_sequence.unwrap();
    let mut second = 0;
    s.edit("Camera backup stream", |p, _| {
        let source = p.sequence(sequence).unwrap().find_item(ClipId(id)).unwrap().1.item;
        p.item_mut(source).unwrap().as_media_mut().unwrap().info.audio_streams.push(filmcraft_media::AudioStreamInfo {
            sample_rate: 48_000,
            channels: 2,
            codec: "PCM float".into(),
            bits_per_sample: Some(32),
        });
        second = p.alloc_id();
        let q = p.sequence_mut(sequence).unwrap();
        let mut camera = q.find_item(ClipId(id)).unwrap().1.clone();
        q.find_item_mut(ClipId(id)).unwrap().1.link = Some(998);
        camera.id = ClipId(second);
        camera.link = Some(998);
        camera.audio_stream = 1;
        q.audio_tracks[1].items.push(camera);
        Ok(())
    })
    .unwrap();
    s.execute("audio.channels.set", json!({"clip":id,"mode":"left"})).unwrap();
    assert!(clip(&s, second).effects.iter().all(|e| e.effect != ROUTER));
    s.execute("audio.streams.configure", json!({"clips":[id],"mainStream":0,"muteSecondary":true})).unwrap();
    assert!(clip(&s, id).enabled);
    assert!(!clip(&s, second).enabled);
    assert_eq!(clip(&s, second).audio_stream, 1);
    let info = s.execute("audio.channels.get", json!({"clip":id})).unwrap();
    assert_eq!(info["clips"].as_array().unwrap().len(), 1);
    assert_eq!(info["clips"][0]["sourceStreams"].as_array().unwrap().len(), 2);
}

#[test]
fn source_mapping_validates_before_mutation_and_retains_other_controls() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.7);
    s.execute("audio.channels.set", json!({"clip":id,"mode":"stereo","leftGainDb":-3.0})).unwrap();
    let effect = router(clip(&s, id), "test").unwrap().cloned();
    s.execute("audio.source.set", json!({"clip":id,"channels":[1]})).unwrap();
    assert_eq!(clip(&s, id).source_channels, vec![1]);
    assert_eq!(router(clip(&s, id), "test").unwrap().cloned(), effect);
    let before = s.project.clone();
    let revision = s.revision;
    for p in [json!({"clip":id,"channels":[99]}), json!({"clip":id,"stream":99}), json!({"clip":id,"channels":[0,1,0]})] {
        assert!(s.execute("audio.source.set", p).is_err());
        assert_eq!(s.project, before);
        assert_eq!(s.revision, revision);
    }
}

#[test]
fn inactive_output_volume_normalization_rejects_atomically() {
    for bypass in [false, true] {
        let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.7);
        let seq = s.state.active_sequence.unwrap();
        s.edit("Bypass output Volume", |p, _| {
            let volume = p.sequence_mut(seq).unwrap().find_item_mut(ClipId(id)).unwrap().1.effect_mut("volume").unwrap();
            if bypass {
                volume.param_mut("bypass").unwrap().value = ParamValue::Bool(true)
            } else {
                volume.enabled = false;
            }
            Ok(())
        })
        .unwrap();
        let before = s.project.clone();
        let revision = s.revision;
        let undo = s.history.undo.len();
        let old_audio = audio(&s);
        let error = s.execute("audio.normalize", json!({"clip":id})).unwrap_err();
        assert!(error.to_string().contains("Volume is disabled or bypassed"), "{error}");
        assert_eq!(s.project, before);
        assert_eq!(s.revision, revision);
        assert_eq!(s.history.undo.len(), undo);
        assert_eq!(audio(&s).channels, old_audio.channels);
    }
}

#[test]
fn unlinked_occurrences_cannot_validate_each_others_main_stream() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.0);
    let sequence = s.state.active_sequence.unwrap();
    let mut scratch = 0;
    s.edit("Unlinked scratch at another occurrence", |p, _| {
        let source = p.sequence(sequence).unwrap().find_item(ClipId(id)).unwrap().1.item;
        p.item_mut(source).unwrap().as_media_mut().unwrap().info.audio_streams.push(filmcraft_media::AudioStreamInfo {
            sample_rate: 48_000,
            channels: 2,
            codec: "PCM float".into(),
            bits_per_sample: Some(32),
        });
        scratch = p.alloc_id();
        let q = p.sequence_mut(sequence).unwrap();
        let mut other = q.find_item(ClipId(id)).unwrap().1.clone();
        other.id = ClipId(scratch);
        other.link = None;
        other.audio_stream = 1;
        other.start = Tick::from_units(5, 1);
        q.find_item_mut(ClipId(id)).unwrap().1.link = None;
        q.audio_tracks[1].items.push(other);
        Ok(())
    })
    .unwrap();
    let before = s.project.clone();
    let revision = s.revision;
    let undo = s.history.undo.len();
    assert!(s.execute("audio.streams.configure", json!({"clips":[id,scratch],"mainStream":0,"muteSecondary":true})).is_err());
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    assert_eq!(s.history.undo.len(), undo);
    assert!(clip(&s, scratch).enabled);
}

#[test]
fn normalization_accepts_profile_loudness_target_range() {
    for target in [-60.0, 0.0] {
        let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.7);
        let result = s.execute("audio.normalize", json!({"clip":id,"targetLufs":target})).unwrap();
        assert_eq!(result["clips"][0]["targetLufs"], target);
    }
}

#[test]
fn one_channel_or_duplicated_channel_mappings_cannot_be_silenced_by_side_extraction() {
    let (mut s, id, _dir) = fixture(SampleFormat::Float32, 0.4, 0.7);
    for mapping in [vec![1u16], vec![1, 1]] {
        s.execute("audio.source.set", json!({"clip":id,"channels":mapping})).unwrap();
        let before = s.project.clone();
        let revision = s.revision;
        let undo = s.history.undo.len();
        for mode in ["side", "midSideEncode", "midSideDecode", "right", "swap"] {
            assert!(s.execute("audio.channels.set", json!({"clip":id,"mode":mode})).is_err(), "{mode}");
            assert_eq!(s.project, before);
            assert_eq!(s.revision, revision);
            assert_eq!(s.history.undo.len(), undo);
        }
    }
}
