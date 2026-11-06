use super::*;
use filmcraft_project::{ItemId, Keyframe, ParamKind};
use filmcraft_time::Tick;
use std::sync::Arc;

const RATE: u32 = 48_000;
fn scene(gain: f32) -> (Session, u64) {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"video":1,"audio":2,"fps":24,"width":64,"height":64})).unwrap();
    let samples: Vec<_> = (0..2 * RATE)
        .flat_map(|i| {
            let phase = std::f64::consts::TAU * f64::from(i) / f64::from(RATE);
            let left = 0.015 * (phase * 20.0).sin() + 0.025 * (phase * 100.0).sin() + 0.020 * (phase * 2000.0).sin() + 0.015 * (phase * 10000.0).sin();
            let right = 0.030 * (phase * 2300.0).sin() + 0.010 * (phase * 350.0).sin();
            [left as f32 * gain, right as f32 * gain]
        })
        .collect();
    let bytes = filmcraft_export::write_wav(&samples, 2, RATE, filmcraft_export::SampleFormat::Float32).unwrap();
    let item = crate::commands::import_bytes(&mut s, "/original-voice-test.wav", bytes.into(), None).unwrap();
    let clip = s.execute("timeline.place", json!({"item":item.0,"audioTrack":"A1","time":0})).unwrap()["clips"][0].as_u64().unwrap();
    (s, clip)
}
fn item(s: &Session, clip: u64) -> filmcraft_project::TrackItem {
    s.active_sequence().unwrap().find_item(ClipId(clip)).unwrap().1.clone()
}
fn mix(s: &Session) -> [Vec<f32>; 2] {
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    let output = filmcraft_render::audio::mix_sequence(&s.project, s.active_sequence().unwrap(), 0, 2 * RATE as usize, &provider);
    [output.channels[0].clone(), output.channels[1].clone()]
}
fn amplitude(samples: &[f32], frequency: f64) -> f64 {
    let (mut sin, mut cos) = (0.0, 0.0);
    for (i, &sample) in samples.iter().enumerate() {
        let phase = std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE);
        sin += f64::from(sample) * phase.sin();
        cos += f64::from(sample) * phase.cos();
    }
    2.0 * (sin * sin + cos * cos).sqrt() / samples.len() as f64
}
fn reject(s: &mut Session, params: Value) {
    let before = s.project.clone();
    let revision = s.revision;
    let undo = s.history.undo.len();
    let redo = s.history.redo.len();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("audio.voice.polish", params.clone())));
    assert!(result.is_ok(), "panic on {params}");
    assert!(result.unwrap().is_err(), "accepted {params}");
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    assert_eq!((s.history.undo.len(), s.history.redo.len()), (undo, redo));
}

#[test]
fn recipe_is_inspectable_and_all_actual_controls_match_the_native_catalogue() {
    let mut s = Session::default();
    let before = s.project.clone();
    let revision = s.revision;
    let report = s.execute("audio.voice.recipe", json!({})).unwrap();
    assert_eq!(report["recipe"]["tone"], "neutral");
    assert_eq!(report["recipe"]["intensity"], 0.35);
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    for tone in [Tone::Neutral, Tone::Warm, Tone::Bright] {
        for intensity in [0.0, 0.35, 1.0] {
            let recipe = recipe(tone, intensity).unwrap();
            assert_eq!(recipe.iter().map(|e| e.effect.as_str()).collect::<Vec<_>>(), IDS);
            for effect in &recipe {
                let definition = find_effect(&effect.effect).unwrap();
                for (id, param) in &effect.params {
                    let def = definition.param(id).unwrap();
                    match (&def.kind, &param.value) {
                        (ParamKind::Float { min, max, .. }, ParamValue::Float(value)) => {
                            assert!(value.is_finite() && value >= min && value <= max, "{} {id}", effect.effect)
                        }
                        (ParamKind::Bool, ParamValue::Bool(_)) => {}
                        (ParamKind::Choice(options), ParamValue::Choice(value)) => assert!((*value as usize) < options.len()),
                        _ => panic!("invalid recipe parameter {} {id}", effect.effect),
                    }
                }
            }
            assert_eq!(recipe[0].f64_at("master_gain", Tick::ZERO), 0.0);
            assert_eq!(recipe[1].f64_at("lim_on", Tick::ZERO), 0.0);
            assert_eq!(recipe[1].f64_at("output", Tick::ZERO), 0.0);
            assert_eq!(recipe[2].f64_at("max", Tick::ZERO), -1.0);
            assert_eq!(recipe[2].f64_at("boost", Tick::ZERO), 0.0);
            assert!(recipe[1].f64_at("b4_attack", Tick::ZERO) >= 8.0);
        }
    }
    assert!(s.execute("audio.voice.recipe", json!({"tone":"guessMyVoice"})).is_err());
    assert!(s.execute("audio.voice.recipe", json!({"intensity":1e300})).is_err());
    assert!(s.execute("audio.voice.recipe", json!({"unknown":true})).is_err());
}

#[test]
fn audible_warm_and_bright_tones_have_distinct_native_output_without_synthesizing_stereo() {
    let (mut warm, clip) = scene(1.0);
    let original = mix(&warm);
    warm.execute("audio.voice.polish", json!({"clip":clip,"tone":"warm","intensity":1})).unwrap();
    let warm = mix(&warm);
    let (mut bright, clip) = scene(1.0);
    bright.execute("audio.voice.polish", json!({"clip":clip,"tone":"bright","intensity":1})).unwrap();
    let bright = mix(&bright);
    let stable = RATE as usize..2 * RATE as usize;
    let body_warm = amplitude(&warm[0][stable.clone()], 100.0);
    let body_bright = amplitude(&bright[0][stable.clone()], 100.0);
    let air_warm = amplitude(&warm[0][stable.clone()], 10000.0);
    let air_bright = amplitude(&bright[0][stable.clone()], 10000.0);
    assert!(body_warm > body_bright * 1.1, "warm:{body_warm} bright:{body_bright}");
    assert!(air_bright > air_warm * 1.15, "warm:{air_warm} bright:{air_bright}");
    assert!(amplitude(&bright[0][stable.clone()], 20.0) < amplitude(&original[0][stable.clone()], 20.0) * 0.25);
    assert!(amplitude(&bright[1][stable.clone()], 2300.0) > 0.01);
    assert!(amplitude(&bright[0][stable], 2300.0) < 1e-4, "right-only source content must not be copied to the left");
    assert!(warm.iter().chain(&bright).flatten().all(|x| x.is_finite()));
}

#[test]
fn hot_float_source_is_limited_at_recipe_output_without_post_limiter_boost() {
    let (mut s, clip) = scene(80.0);
    let original = mix(&s);
    assert!(original.iter().flatten().any(|sample| sample.abs() > 1.0));
    s.execute("audio.voice.polish", json!({"clip":clip,"intensity":0.35})).unwrap();
    let rendered = mix(&s);
    let ceiling = 10.0f32.powf(-1.0 / 20.0);
    assert!(rendered.iter().flatten().all(|sample| sample.is_finite() && sample.abs() <= ceiling + 2e-6));
    assert!(rendered.iter().flatten().any(|sample| sample.abs() > 0.1));
    let item = item(&s, clip);
    assert_eq!(item.effects.last().unwrap().effect, "voice_polish_limiter");
    assert_eq!(item.effects.last().unwrap().f64_at("boost", Tick::ZERO), 0.0);
    assert_eq!(item.effect("volume").unwrap().f64_at("level", Tick::ZERO), 0.0);
    assert_eq!(item.gain_db, 0.0);
}

#[test]
fn repeated_recipe_preserves_user_animation_and_owned_identities_and_stages_as_one_undo() {
    let (mut s, clip) = scene(1.0);
    let sequence = s.state.active_sequence.unwrap();
    s.edit("User's custom EQ", |project, _| {
        let mut user = find_effect("parametric_eq").unwrap().instance();
        let parameter = user.param_mut("master_gain").unwrap();
        parameter.keyframes = vec![Keyframe::new(Tick::ZERO, ParamValue::Float(-1.0)), Keyframe::new(Tick::from_units(1, 1), ParamValue::Float(-2.0))];
        project.sequence_mut(sequence).unwrap().find_item_mut(ClipId(clip)).unwrap().1.effects.push(user);
        Ok(())
    })
    .unwrap();
    let user = item(&s, clip).effects.last().unwrap().clone();
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let proposal = s.execute("agent.plan", json!({"steps":[{"id":"audio.voice.polish","params":{"clip":clip,"tone":"warm"}}]})).unwrap();
    assert_eq!(s.project, before);
    s.execute("agent.apply", json!({"plan":proposal["plan"]})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 1);
    let first = item(&s, clip);
    assert_eq!(first.effects.iter().find(|e| e.effect == "parametric_eq").unwrap(), &user);
    let ids: Vec<_> = first.effects.iter().filter(|e| IDS.contains(&e.effect.as_str())).map(|e| e.instance_id).collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|id| *id > 0));
    s.execute("audio.voice.polish", json!({"clip":clip,"tone":"bright","intensity":0.8})).unwrap();
    let second = item(&s, clip);
    assert_eq!(second.effects.iter().filter(|e| IDS.contains(&e.effect.as_str())).map(|e| e.instance_id).collect::<Vec<_>>(), ids);
    assert_eq!(second.effects.iter().find(|e| e.effect == "parametric_eq").unwrap(), &user);
    let stable = s.project.clone();
    let revision = s.revision;
    let report = s.execute("audio.voice.polish", json!({"clip":clip,"tone":"bright","intensity":0.8})).unwrap();
    assert_eq!(report["changed"], 0);
    assert_eq!(s.project, stable);
    assert_eq!(s.revision, revision);
}

#[test]
fn locked_duplicate_or_animated_owned_recipes_fail_atomically_and_zero_intensity_bypasses() {
    let (mut s, clip) = scene(1.0);
    let original = s.project.clone();
    assert_eq!(s.execute("audio.voice.polish", json!({"clip":clip,"intensity":0})).unwrap()["changed"], 0);
    assert_eq!(s.project, original);
    s.execute("audio.voice.polish", json!({"clip":clip})).unwrap();
    let sequence = s.state.active_sequence.unwrap();
    Arc::make_mut(&mut s.project).sequence_mut(sequence).unwrap().audio_tracks[0].locked = true;
    reject(&mut s, json!({"clip":clip}));
    Arc::make_mut(&mut s.project).sequence_mut(sequence).unwrap().audio_tracks[0].locked = false;
    let owned = Arc::make_mut(&mut s.project)
        .sequence_mut(sequence)
        .unwrap()
        .find_item_mut(ClipId(clip))
        .unwrap()
        .1
        .effects
        .iter_mut()
        .find(|e| e.effect == IDS[0])
        .unwrap();
    owned.param_mut("low_gain").unwrap().keyframes = vec![Keyframe::new(Tick::ZERO, ParamValue::Float(2.0))];
    reject(&mut s, json!({"clip":clip}));
    s.execute("audio.voice.polish", json!({"clip":clip,"replaceAnimation":true})).unwrap();
    assert!(!item(&s, clip).effects.iter().find(|e| e.effect == IDS[0]).unwrap().is_animated());
    s.execute("audio.voice.polish", json!({"clip":clip,"intensity":0})).unwrap();
    assert!(item(&s, clip).effects.iter().filter(|e| IDS.contains(&e.effect.as_str())).all(|e| !e.enabled));
    let mut duplicate = item(&s, clip).effects.iter().find(|e| e.effect == IDS[0]).unwrap().clone();
    duplicate.instance_id = 0;
    Arc::make_mut(&mut s.project).sequence_mut(sequence).unwrap().find_item_mut(ClipId(clip)).unwrap().1.effects.push(duplicate);
    reject(&mut s, json!({"clip":clip}));
}

#[test]
fn hostile_voice_arguments_and_selection_never_change_project_or_history() {
    let (mut s, clip) = scene(1.0);
    for params in [
        json!({"clip":clip,"clips":[clip]}),
        json!({"clips":[clip,clip]}),
        json!({"clips":[]}),
        json!({"clip":u64::MAX}),
        json!({"clip":clip,"intensity":-0.1}),
        json!({"clip":clip,"intensity":1e300}),
        json!({"clip":clip,"tone":"unknown"}),
        json!({"clip":clip,"replaceAnimation":"yes"}),
        json!({"clip":clip,"unsupported":true}),
        json!({"clips":vec![clip;MAX_CLIPS+1]}),
    ] {
        reject(&mut s, params);
    }
    let source = item(&s, clip).item;
    assert!(s.project.item(ItemId(source.0)).is_some());
}
