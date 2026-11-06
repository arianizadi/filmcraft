use super::*;
use filmcraft_project::{Keyframe, Param, find_effect};
use filmcraft_time::Tick;
use std::sync::Arc;

fn scene() -> Session {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({"audio":2,"video":1,"fps":24,"width":64,"height":64})).unwrap();
    for track in ["A1", "A2"] {
        let source = s.execute("file.newBarsAndTone", json!({"seconds":2})).unwrap()["item"].as_u64().unwrap();
        s.execute("timeline.place", json!({"item":source,"audioTrack":track,"video":false,"time":0})).unwrap();
    }
    let id = s.state.active_sequence.unwrap();
    s.edit("User hot microphone and music mix", |project, _| {
        let seq = project.sequence_mut(id).unwrap();
        seq.master_volume_db = 12.0;
        for track in &mut seq.audio_tracks {
            track.volume_db = 12.0;
            for clip in &mut track.items {
                clip.gain_db = 24.0;
            }
        }
        Ok(())
    })
    .unwrap();
    s
}
fn mix(s: &Session) -> Vec<f32> {
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    filmcraft_render::audio::mix_sequence(&s.project, s.active_sequence().unwrap(), 0, 96_000, &provider).channels.into_iter().flatten().collect()
}
fn reject(s: &mut Session, p: Value) {
    let before = s.project.clone();
    let revision = s.revision;
    let history = (s.history.undo.len(), s.history.redo.len());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("audio.mix.limit", p.clone())));
    assert!(result.is_ok(), "panic {p}");
    assert!(result.unwrap().is_err(), "accepted {p}");
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    assert_eq!((s.history.undo.len(), s.history.redo.len()), history);
}

#[test]
fn final_post_fader_limiter_protects_hot_summed_sources_and_master_boost_as_one_undo() {
    let mut s = scene();
    assert!(mix(&s).iter().any(|x| x.abs() > 1.0));
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let plan = s.execute("agent.plan", json!({"steps":[{"id":"audio.mix.limit","params":{}}]})).unwrap();
    assert_eq!(s.project, before);
    s.execute("agent.apply", json!({"plan":plan["plan"]})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 1);
    let effect = s.active_sequence().unwrap().master_effects.last().unwrap();
    assert_eq!(effect.effect, OWNED);
    assert!(effect.post_fader && effect.enabled);
    assert_eq!(effect.f64_at("boost", Tick::ZERO), 0.0);
    let output = mix(&s);
    let ceiling = 10.0f32.powf(-1.5 / 20.0);
    assert!(output.iter().all(|x| x.is_finite() && x.abs() <= ceiling + 2e-6));
    assert!(output.iter().any(|x| x.abs() > 0.1));
    let revision = s.revision;
    let undo = s.history.undo.len();
    assert_eq!(s.execute("audio.mix.limit", json!({})).unwrap()["changed"], false);
    assert_eq!(s.revision, revision);
    assert_eq!(s.history.undo.len(), undo);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}

#[test]
fn reordered_owned_limiter_preserves_user_insert_identity_and_remaps_user_slot_automation() {
    let mut s = scene();
    s.execute("audio.mix.limit", json!({})).unwrap();
    let id = s.state.active_sequence.unwrap();
    let own_id = s.active_sequence().unwrap().master_effects[0].instance_id;
    let parameter = Param {
        value: ParamValue::Float(3.0),
        keyframes: vec![Keyframe::new(Tick::ZERO, ParamValue::Float(3.0)), Keyframe::new(Tick::from_units(1, 1), ParamValue::Float(6.0))],
    };
    s.edit("User post-fader gain", |project, _| {
        let mut effect = find_effect("amplify").unwrap().instance();
        effect.post_fader = true;
        // Actual insert automation is stored in the effect parameter, not only the auxiliary
        // lane map. Preserve its exact values/times while moving its slot before the limiter.
        *effect.param_mut("gain").unwrap() = parameter.clone();
        let seq = project.sequence_mut(id).unwrap();
        seq.master_effects.push(effect);
        seq.master_mixer.lanes.insert(fx_lane(1, "gain"), parameter.clone());
        Ok(())
    })
    .unwrap();
    let user = s.active_sequence().unwrap().master_effects[1].clone();
    s.execute("audio.mix.limit", json!({"ceilingDbfs":-2.0})).unwrap();
    let seq = s.active_sequence().unwrap();
    assert_eq!(seq.master_effects[0], user);
    assert_eq!(seq.master_effects[1].instance_id, own_id);
    assert_eq!(seq.master_mixer.lanes.get(&fx_lane(0, "gain")), Some(&parameter));
    assert!(!seq.master_mixer.lanes.contains_key(&fx_lane(1, "gain")));
    let ceiling = 10.0f32.powf(-2.0 / 20.0);
    assert!(mix(&s).iter().all(|x| x.abs() <= ceiling + 2e-6));
}

#[test]
fn occupied_invalid_positions_duplicate_owned_and_parameter_or_mixer_animation_fail_atomically() {
    let mut s = scene();
    let id = s.state.active_sequence.unwrap();
    let unknown = filmcraft_project::EffectInstance { effect: "unsupported-user-plugin".into(), ..find_effect("amplify").unwrap().instance() };
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().master_effects = vec![unknown; MAX_INSERTS];
    reject(&mut s, json!({}));
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().master_effects.clear();
    s.execute("audio.mix.limit", json!({})).unwrap();
    let seq = Arc::make_mut(&mut s.project).sequence_mut(id).unwrap();
    seq.master_effects[0].param_mut("max").unwrap().keyframes = vec![Keyframe::new(Tick::ZERO, ParamValue::Float(-0.5))];
    reject(&mut s, json!({}));
    s.execute("audio.mix.limit", json!({"replaceAnimation":true})).unwrap();
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().master_mixer.lanes.insert(fx_lane(0, "boost"), Param::new(ParamValue::Float(24.0)));
    reject(&mut s, json!({}));
    s.execute("audio.mix.limit", json!({"replaceAnimation":true})).unwrap();
    assert!(s.active_sequence().unwrap().master_mixer.lanes.is_empty());
    let duplicate = s.active_sequence().unwrap().master_effects[0].clone();
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().master_effects.push(duplicate);
    reject(&mut s, json!({}));
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().master_effects.clear();
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().master_mixer.lanes.insert(fx_lane(0, "max"), Param::new(ParamValue::Float(0.0)));
    reject(&mut s, json!({}));
    s.execute("audio.mix.limit", json!({"replaceAnimation":true})).unwrap();
    assert!(s.active_sequence().unwrap().master_mixer.lanes.is_empty());
}

#[test]
fn invalid_output_ceiling_or_unknown_params_never_mutate() {
    let mut s = scene();
    for p in [
        json!({"ceilingDbfs":-25}),
        json!({"ceilingDbfs":0.1}),
        json!({"ceilingDbfs":1e300}),
        json!({"ceilingDbfs":null}),
        json!({"ceilingDbfs":"-1"}),
        json!({"replaceAnimation":"yes"}),
        json!({"unknown":true}),
    ] {
        reject(&mut s, p);
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(std::panic::catch_unwind(|| ceiling(value)).unwrap().is_err());
    }
}
