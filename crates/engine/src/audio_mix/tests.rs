use super::*;
use filmcraft_project::{
    Keyframe,
    essential::{AudioType, EssentialSound},
};

fn fixture() -> (Session, [u64; 4]) {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"video":1,"audio":4,"fps":24,"width":64,"height":64})).unwrap();
    let mut ids = [0; 4];
    for (index, id) in ids.iter_mut().enumerate() {
        let samples: Vec<f32> = (0..3 * RATE)
            .flat_map(|i| {
                let active = match index {
                    0 => (RATE..2 * RATE).contains(&i),
                    3 => !(RATE / 2..=RATE * 5 / 2).contains(&i),
                    _ => true,
                };
                let value = if active { (std::f32::consts::TAU * i as f32 * 437.0 / RATE as f32).sin() * 0.15 } else { 0.0 };
                [value, value]
            })
            .collect();
        let bytes = filmcraft_export::write_wav(&samples, 2, RATE, filmcraft_export::SampleFormat::Float32).unwrap();
        let source = crate::commands::import_bytes(&mut s, &format!("/original-mix-{index}.wav"), bytes.into(), None).unwrap();
        *id = s.execute("timeline.place", json!({"item":source.0,"audioTrack":format!("A{}",index+1),"time":0})).unwrap()["clips"][0].as_u64().unwrap();
    }
    (s, ids)
}
fn item(s: &Session, id: u64) -> &TrackItem {
    s.active_sequence().unwrap().find_item(ClipId(id)).unwrap().1
}
fn change(s: &mut Session, id: u64, f: impl FnOnce(&mut TrackItem)) {
    let seq = s.state.active_sequence.unwrap();
    s.edit("Test fixture", |p, _| {
        f(p.sequence_mut(seq).unwrap().find_item_mut(ClipId(id)).unwrap().1);
        Ok(())
    })
    .unwrap();
}
fn reject(s: &mut Session, command: &str, p: Value) {
    let project = s.project.clone();
    let rev = s.revision;
    let undo = s.history.undo.len();
    let redo = s.history.redo.len();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute(command, p)));
    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
    assert_eq!(s.project, project);
    assert_eq!(s.revision, rev);
    assert_eq!(s.history.undo.len(), undo);
    assert_eq!(s.history.redo.len(), redo);
}
#[test]
fn roles_are_explicit_live_readback_and_essential_mute_is_audibly_removed() {
    let (mut s, [main, backup, music, other]) = fixture();
    change(&mut s, main, |it| {
        it.enabled = false;
        it.essential = Some(EssentialSound::new(AudioType::Dialogue));
        it.essential.as_mut().unwrap().mute = true;
        it.link = Some(888);
    });
    change(&mut s, other, |it| it.link = Some(888));
    let other_before = item(&s, other).clone();
    let main_before = item(&s, main).clone();
    let report = s.execute("audio.mix.roles", json!({"main":[main],"backup":[backup],"music":[music]})).unwrap();
    assert_eq!(report["roles"][0]["enabled"], true);
    assert_eq!(report["roles"][1]["enabled"], false);
    assert!(item(&s, main).enabled);
    assert!(!item(&s, main).essential.as_ref().unwrap().mute);
    assert!(!item(&s, backup).enabled);
    assert_eq!(item(&s, other), &other_before);
    assert_eq!(item(&s, main).start, main_before.start);
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    let q = s.active_sequence().unwrap();
    let output = filmcraft_render::audio::track_input(&s.project, &q.audio_tracks[0], RATE as i64, 4800, RATE, &provider);
    assert!(output.channels[0].iter().any(|v| v.abs() > 0.01));
}
#[test]
fn explicit_detector_ignores_unrelated_dialogue_and_preserves_main_positions() {
    let (mut s, [main, _backup, music, other]) = fixture();
    change(&mut s, other, |it| it.essential = Some(EssentialSound::new(AudioType::Dialogue)));
    let original = item(&s, main).clone();
    let report = s.execute("audio.mix.duck", json!({"main":[main],"music":[music],"duckDb":-12,"duckFadeSeconds":0.1})).unwrap();
    assert_eq!(report["changed"], 1);
    assert_eq!(item(&s, main), &original);
    let effect = item(&s, music).effect("volume").unwrap();
    assert!((effect.f64_at("level", Tick::from_units(1, 5))).abs() < 1e-6);
    assert!((effect.f64_at("level", Tick::from_units(3, 2)) + 12.0).abs() < 1e-6);
    assert!((effect.f64_at("level", Tick::from_units(14, 5))).abs() < 1e-6);
    assert!(report["clips"][0]["keyframes"].as_array().unwrap().iter().all(|k| k["timelineTime"] == k["sourceTime"]));
}
#[test]
fn hostile_roles_locks_inaudible_tracks_and_unsupported_timing_fail_atomically() {
    let (mut s, [main, backup, music, _]) = fixture();
    for p in [
        json!({"main":[]}),
        json!({"main":[main],"backup":[main]}),
        json!({"main":[u64::MAX]}),
        json!({"main":[main],"duckDb":-41}),
        json!({"main":[main],"intensity":2}),
        json!({"main":[main],"surprise":true}),
    ] {
        reject(&mut s, "audio.mix.prepare", p);
    }
    let seq = s.state.active_sequence.unwrap();
    for kind in 0..4 {
        s.edit("Track guards", |p, _| {
            let q = p.sequence_mut(seq).unwrap();
            q.audio_tracks[0].locked = kind == 0;
            q.audio_tracks[0].muted = kind == 1;
            q.audio_tracks[0].enabled = kind != 2;
            q.audio_tracks[3].solo = kind == 3;
            Ok(())
        })
        .unwrap();
        reject(&mut s, "audio.mix.prepare", json!({"main":[main],"backup":[backup],"music":[music]}));
    }
    s.edit("Restore tracks", |p, _| {
        let q = p.sequence_mut(seq).unwrap();
        q.audio_tracks[0].enabled = true;
        q.audio_tracks[3].solo = false;
        Ok(())
    })
    .unwrap();
    for held in [false, true] {
        change(&mut s, music, |it| {
            if held {
                it.speed = 1.0;
                it.frame_hold = Some(Tick::ZERO)
            } else {
                it.speed = f64::INFINITY
            }
        });
        reject(&mut s, "audio.mix.duck", json!({"main":[main],"music":[music]}));
    }
}
#[test]
fn music_animation_requires_explicit_replacement_and_no_overlap_is_noop() {
    let (mut s, [main, _, music, _]) = fixture();
    change(&mut s, music, |it| {
        it.effect_mut("volume").unwrap().param_mut("level").unwrap().keyframes =
            vec![Keyframe::new(Tick::ZERO, ParamValue::Float(-2.0)), Keyframe::new(Tick::from_units(3, 1), ParamValue::Float(-4.0))];
    });
    reject(&mut s, "audio.mix.duck", json!({"main":[main],"music":[music]}));
    s.execute("audio.mix.duck", json!({"main":[main],"music":[music],"replaceAnimation":true})).unwrap();
    let keys = item(&s, music).effect("volume").unwrap().param("level").unwrap().keyframes.clone();
    assert!(keys.iter().any(|k| k.value.as_f64().unwrap() < -10.0));
    change(&mut s, music, |it| it.start = Tick::from_units(10, 1));
    let before = s.project.clone();
    assert_eq!(s.execute("audio.mix.duck", json!({"main":[main],"music":[music]})).unwrap()["changed"], 0);
    assert_eq!(s.project, before);
}
#[test]
fn preparation_is_isolated_one_undo_with_owned_recipe_final_limiter_and_stale_guard() {
    let (mut s, [main, backup, music, other]) = fixture();
    let before = s.project.clone();
    let original = item(&s, other).clone();
    let undo = s.history.undo.len();
    let plan = s.execute("audio.mix.prepare", json!({"main":[main],"backup":[backup],"music":[music],"repairChannels":false,"normalize":false})).unwrap();
    assert_eq!(s.project, before);
    assert_eq!(s.history.undo.len(), undo);
    let (_, _, _) = s.agent_plan_snapshot(plan["plan"].as_u64().unwrap(), false).unwrap();
    s.execute("agent.apply", json!({"plan":plan["plan"]})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 1);
    assert_eq!(item(&s, other), &original);
    assert!(!item(&s, backup).enabled);
    assert!(item(&s, main).effects.iter().any(|e| e.effect == "voice_polish_eq"));
    assert_eq!(s.active_sequence().unwrap().master_effects.last().unwrap().effect, "audio_mix_limiter");
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
    let plan = s.execute("audio.mix.prepare", json!({"main":[main],"intensity":0,"repairChannels":false,"normalize":false,"finalLimiter":false})).unwrap();
    s.edit("Concurrent edit", |p, _| {
        p.notes = "keep".into();
        Ok(())
    })
    .unwrap();
    reject(&mut s, "agent.apply", json!({"plan":plan["plan"]}));
}
#[test]
fn zero_intensity_disables_existing_owned_recipe_in_staged_snapshot() {
    let (mut s, [main, _, _, _]) = fixture();
    s.execute("audio.voice.polish", json!({"clips":[main]})).unwrap();
    let before = s.project.clone();
    let plan = s.execute("audio.mix.prepare", json!({"main":[main],"intensity":0,"repairChannels":false,"normalize":false,"finalLimiter":false})).unwrap();
    assert_eq!(s.project, before);
    s.execute("agent.apply", json!({"plan":plan["plan"]})).unwrap();
    assert!(item(&s, main).effects.iter().filter(|e| e.effect.starts_with("voice_polish_")).all(|e| !e.enabled));
}
