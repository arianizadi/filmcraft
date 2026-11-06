//! Original, conservative voice-polish recipe using editable native effects. This is a starting
//! point for audition, not source identification, automatic restoration or a delivery guarantee.
//! The recipe never adds makeup gain after its limiter. Existing clip/track/master controls may
//! follow it; final-mix loudness/true-peak measurement remains necessary after normalization.
use std::collections::BTreeSet;

use filmcraft_project::{ClipId, EffectInstance, ParamValue, find_effect};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Result, Session,
    analysis_support::bounded_decode,
    commands::{CommandSpec, always, bad, has_seq},
};

const IDS: [&str; 3] = ["voice_polish_eq", "voice_polish_multiband", "voice_polish_limiter"];
const MAX_CLIPS: usize = 256;
const WARNING: &str = "These are fixed, gentle starting-point thresholds: quiet recordings may receive EQ without driving compression. Output loudness normalization does not increase compressor input; audition or adjust the editable thresholds. The limiter ceiling is -1 dBFS at this recipe's output. Existing clip gain, Volume, track/master processing and summed sources can change delivery peaks; measure/protect the final mix. No makeup gain is added after the limiter.";

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Tone {
    #[default]
    Neutral,
    Warm,
    Bright,
}
impl Tone {
    fn name(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Warm => "warm",
            Self::Bright => "bright",
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecipeArgs {
    #[serde(default)]
    tone: Tone,
    intensity: Option<f64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Params {
    clip: Option<u64>,
    clips: Option<Vec<u64>>,
    #[serde(default)]
    tone: Tone,
    intensity: Option<f64>,
    #[serde(default)]
    replace_animation: bool,
}
fn error(reason: impl std::fmt::Display) -> crate::EngineError {
    bad("audio.voice.polish", reason.to_string())
}
fn amount(value: Option<f64>) -> Result<f64> {
    let value = value.unwrap_or(0.35);
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(error("intensity must be finite within 0..1"));
    }
    Ok(value)
}
fn set(effect: &mut EffectInstance, id: &str, value: ParamValue) -> Result<()> {
    let param = effect.param_mut(id).ok_or_else(|| error(format!("voice recipe control {id} is unavailable")))?;
    param.value = value;
    param.keyframes.clear();
    Ok(())
}
fn recipe(tone: Tone, intensity: f64) -> Result<Vec<EffectInstance>> {
    let mut effects = IDS
        .iter()
        .map(|id| find_effect(id).map(|d| d.instance()).ok_or_else(|| error(format!("voice recipe effect {id} is unavailable"))))
        .collect::<Result<Vec<_>>>()?;
    let eq = effects.first_mut().ok_or_else(|| error("voice recipe EQ is unavailable"))?;
    let (hp, body, presence, air) = match tone {
        Tone::Neutral => (55.0 + 40.0 * intensity, 0.8, 1.5, 1.5),
        Tone::Warm => (50.0 + 25.0 * intensity, 2.0, 0.5, 0.5),
        Tone::Bright => (65.0 + 35.0 * intensity, -0.4, 2.0, 3.0),
    };
    for (id, value) in [
        ("master_gain", 0.0),
        ("hp_freq", hp),
        ("low_freq", 100.0),
        ("low_gain", body * intensity),
        ("low_q", 0.71),
        ("b1_freq", 300.0),
        ("b1_gain", -2.0 * intensity),
        ("b1_q", 0.85),
        ("mid_freq", 1100.0),
        ("mid_gain", -0.8 * intensity),
        ("mid_q", 1.0),
        ("b4_freq", 2000.0),
        ("b4_gain", presence * intensity),
        ("b4_q", 0.8),
        ("high_freq", 10000.0),
        ("high_gain", air * intensity),
        ("high_q", 0.71),
    ] {
        set(eq, id, ParamValue::Float(value))?;
    }
    set(eq, "hp_slope", ParamValue::Choice(0))?; // 12 dB/oct; preserves body below the second harmonic.
    for (id, on) in [
        ("hp_on", intensity > 0.0),
        ("low_on", true),
        ("b1_on", true),
        ("b2_on", false),
        ("mid_on", true),
        ("b4_on", true),
        ("b5_on", false),
        ("high_on", true),
        ("lp_on", false),
    ] {
        set(eq, id, ParamValue::Bool(on))?;
    }
    let mb = effects.get_mut(1).ok_or_else(|| error("voice recipe compressor is unavailable"))?;
    for (id, value) in [("xo1", 150.0), ("xo2", 1200.0), ("xo3", 6000.0), ("output", 0.0)] {
        set(mb, id, ParamValue::Float(value))?;
    }
    for (band, threshold, ratio, attack, release) in
        [(1, -24.0, 1.0, 25.0, 140.0), (2, -22.0, 1.5, 15.0, 120.0), (3, -22.0, 1.5, 12.0, 100.0), (4, -24.0, 2.0, 8.0, 80.0)]
    {
        for (suffix, value) in [("threshold", threshold), ("ratio", 1.0 + ratio * intensity), ("attack", attack), ("release", release), ("gain", 0.0)] {
            set(mb, &format!("b{band}_{suffix}"), ParamValue::Float(value))?;
        }
        set(mb, &format!("b{band}_solo"), ParamValue::Bool(false))?;
        set(mb, &format!("b{band}_bypass"), ParamValue::Bool(false))?;
    }
    set(mb, "lim_on", ParamValue::Bool(false))?;
    set(mb, "link", ParamValue::Bool(true))?;
    let limiter = effects.get_mut(2).ok_or_else(|| error("voice recipe limiter is unavailable"))?;
    for (id, value) in [("max", -1.0), ("boost", 0.0), ("lookahead", 5.0), ("release", 80.0)] {
        set(limiter, id, ParamValue::Float(value))?;
    }
    for effect in &mut effects {
        effect.enabled = intensity > 0.0;
    }
    Ok(effects)
}
fn describe(tone: Tone, intensity: f64, effects: &[EffectInstance]) -> Value {
    json!({"version":1,"tone":tone.name(),"intensity":intensity,"order":IDS,
        "effects":effects.iter().map(|effect|json!({"effect":effect.effect,"enabled":effect.enabled,
            "controls":effect.params.iter().map(|(id,p)|(id.clone(),p.value.clone())).collect::<std::collections::BTreeMap<_,_>>()})).collect::<Vec<_>>(),
        "limiterCeilingDbfs":-1.0,"warning":WARNING,
        "description":"Original gentle high-pass/body/mud/presence/air EQ, four-band linked compression with a high-band sibilance trim, then a native limiter. No learned/noise-isolation model or synthesized stereo."})
}
fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let args: RecipeArgs = bounded_decode(p, "audio.voice.recipe")?;
    let intensity = amount(args.intensity)?;
    let effects = recipe(args.tone, intensity)?;
    Ok(json!({"revision":s.revision,"recipe":describe(args.tone,intensity,&effects)}))
}
fn targets(s: &Session, p: &Params) -> Result<Vec<ClipId>> {
    if p.clip.is_some() && p.clips.is_some() {
        return Err(error("use clip or clips, not both"));
    }
    let selection = match (&p.clip, &p.clips) {
        (Some(id), _) => vec![ClipId(*id)],
        (_, Some(ids)) => ids.iter().map(|id| ClipId(*id)).collect(),
        _ => s.state.selection.clone(),
    };
    if selection.is_empty() || selection.len() > MAX_CLIPS {
        return Err(error("select 1..256 audio clips, or their linked video clips"));
    }
    if selection.iter().copied().collect::<BTreeSet<_>>().len() != selection.len() {
        return Err(error("clip IDs must be unique"));
    }
    let seq = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let mut requested = BTreeSet::new();
    for id in selection {
        let (track, item) = seq.find_item(id).ok_or_else(|| error("clip is not in the active sequence"))?;
        if seq.audio_tracks.iter().any(|t| t.id == track) {
            requested.insert(id);
        } else if let Some(link) = item.link {
            for partner in seq.audio_tracks.iter().flat_map(|t| &t.items).filter(|i| i.link == Some(link)) {
                requested.insert(partner.id);
            }
        }
        if requested.len() > MAX_CLIPS {
            return Err(error("linked audio expansion exceeds 256 clips"));
        }
    }
    if requested.is_empty() {
        return Err(error("selection has no audio-track clips"));
    }
    for id in &requested {
        let (track, item) = seq.find_item(*id).ok_or_else(|| error("audio clip disappeared"))?;
        if seq.track(track).is_some_and(|t| t.locked) {
            return Err(error("an audio track is locked"));
        }
        if item.effects.len() > 512 {
            return Err(error("voice polish supports at most 512 existing effects per clip"));
        }
        for owned in IDS {
            let matches: Vec<_> = item.effects.iter().filter(|effect| effect.effect == owned).take(2).collect();
            if matches.len() > 1 {
                return Err(error(format!("clip {} has duplicate owned recipe effect {owned}; keep one instance before updating", id.0)));
            }
            if matches.first().is_some_and(|effect| effect.is_animated()) && !p.replace_animation {
                return Err(error(
                    "owned voice recipe has animation; set replaceAnimation true to replace its parameter keyframes, or keep the existing recipe",
                ));
            }
        }
    }
    Ok(requested.into_iter().collect())
}
fn polish(s: &mut Session, p: &Value) -> Result<Value> {
    let p: Params = bounded_decode(p, "audio.voice.polish")?;
    let intensity = amount(p.intensity)?;
    let template = recipe(p.tone, intensity)?;
    let ids = targets(s, &p)?;
    let seq_id = s.state.active_sequence.ok_or(crate::EngineError::NoSequence)?;
    let seq = s.project.sequence(seq_id).ok_or(crate::EngineError::NoSequence)?;
    let mut updates = Vec::new();
    for id in &ids {
        let (_, it) = seq.find_item(*id).ok_or_else(|| error("clip disappeared"))?;
        if intensity == 0.0 && !it.effects.iter().any(|e| IDS.contains(&e.effect.as_str())) {
            continue;
        }
        let mut effects: Vec<_> = it.effects.iter().filter(|effect| !IDS.contains(&effect.effect.as_str())).cloned().collect();
        for desired in &template {
            let mut effect = it.effects.iter().find(|effect| effect.effect == desired.effect).cloned().unwrap_or_else(|| desired.clone());
            effect.params = desired.params.clone();
            effect.enabled = desired.enabled;
            effects.push(effect);
        }
        if effects != it.effects {
            updates.push((*id, effects));
        }
    }
    if !updates.is_empty() {
        s.edit("Polish Voice with Editable Native Effects", |project, _| {
            let seq = project.sequence_mut(seq_id).ok_or(crate::EngineError::NoSequence)?;
            for (id, effects) in &updates {
                let (_, it) = seq.find_item_mut(*id).ok_or_else(|| error("clip disappeared before voice commit"))?;
                it.effects = effects.clone();
            }
            Ok(())
        })?;
    }
    let seq = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let clips = ids
        .iter()
        .map(|id| {
            let owned = seq
                .find_item(*id)
                .map(|(_, it)| {
                    it.effects
                        .iter()
                        .filter(|e| IDS.contains(&e.effect.as_str()))
                        .map(|e| json!({"effect":e.effect,"instance":e.instance_id,"enabled":e.enabled}))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            json!({"clip":id.0,"ownedEffects":owned})
        })
        .collect::<Vec<_>>();
    Ok(json!({"revision":s.revision,"changed":updates.len(),"clips":clips,"recipe":describe(p.tone,intensity,&template),"warning":WARNING}))
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "audio.voice.recipe",
            label: "Inspect Voice Polish Recipe",
            menu: &[],
            shortcut: None,
            params: r#"{"tone":"neutral|warm|bright"="neutral","intensity":0..1=0.35}"#,
            enabled: always,
            run: inspect,
            journal: false,
        },
        CommandSpec {
            id: "audio.voice.polish",
            label: "Polish Voice with Editable Native Effects",
            menu: &[],
            shortcut: None,
            params: r#"{"clip":id?,"clips":[id]?,"tone":"neutral|warm|bright"="neutral","intensity":0..1=0.35,"replaceAnimation":bool=false}"#,
            enabled: has_seq,
            run: polish,
            journal: true,
        },
    ]
}

#[cfg(test)]
mod tests;
