//! Owned final post-fader Mix limiter. User inserts and their slot automation retain their
//! identities; malformed or overfull insert lists are refused instead of claiming protection
//! from a limiter outside the renderer's active five slots. True peak is verified separately.
use std::collections::BTreeMap;

use filmcraft_project::{
    ParamValue,
    mixer::{MAX_INSERTS, fx_lane, parse_fx_lane},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Result, Session,
    analysis_support::bounded_decode,
    commands::{CommandSpec, bad, has_seq},
};

const OWNED: &str = "audio_mix_limiter";
fn default_ceiling() -> f64 {
    -1.5
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Params {
    #[serde(default = "default_ceiling")]
    ceiling_dbfs: f64,
    #[serde(default)]
    replace_animation: bool,
}
fn error(reason: impl std::fmt::Display) -> crate::EngineError {
    bad("audio.mix.limit", reason.to_string())
}
fn ceiling(value: f64) -> Result<f64> {
    if !value.is_finite() || !(-24.0..=0.0).contains(&value) {
        return Err(error("ceilingDbfs must be finite within -24..0 dBFS"));
    }
    Ok(value)
}
fn limit(s: &mut Session, p: &Value) -> Result<Value> {
    let p: Params = bounded_decode(p, "audio.mix.limit")?;
    let ceiling = ceiling(p.ceiling_dbfs)?;
    let sequence = s.state.active_sequence.ok_or(crate::EngineError::NoSequence)?;
    let seq = s.project.sequence(sequence).ok_or(crate::EngineError::NoSequence)?;
    if seq.master_effects.len() > MAX_INSERTS {
        return Err(error("Mix has more than five insert positions; remove the excess inserts before adding output protection"));
    }
    let mut owned = seq.master_effects.iter().enumerate().filter(|(_, e)| e.effect == OWNED);
    let existing = owned.next();
    if owned.next().is_some() {
        return Err(error("Mix has duplicate owned output limiters; keep one before updating"));
    }
    if existing.is_none() && seq.master_effects.len() >= MAX_INSERTS {
        return Err(error("all five Mix insert positions are occupied, including disabled or unsupported effects; free one slot first"));
    }
    if existing.is_some_and(|(_, e)| e.is_animated()) && !p.replace_animation {
        return Err(error("owned output limiter has animation; set replaceAnimation true to replace its parameter automation"));
    }
    if seq.master_mixer.lanes.len() > 4096 {
        return Err(error("too many Mix automation lanes"));
    }
    let old_slot = existing.map(|(slot, _)| slot);
    let new_slot = seq.master_effects.len().saturating_sub(usize::from(existing.is_some()));
    let mut lanes = BTreeMap::new();
    for (key, value) in &seq.master_mixer.lanes {
        if key.len() > 1024 {
            return Err(error("Mix automation lane name exceeds the supported length"));
        }
        let mapped = if let Some((slot, param)) = parse_fx_lane(key) {
            // Empty future slots may retain stale automation from a removed user effect. It
            // must not silently override the new limiter's ceiling or input gain.
            let belongs = old_slot == Some(slot) || (old_slot.is_none() && slot == new_slot);
            if belongs {
                if !p.replace_animation {
                    return Err(error(
                        "owned/future limiter slot has mixer automation or overrides; set replaceAnimation true to clear only those owned-slot lanes",
                    ));
                }
                continue;
            }
            let mapped = if old_slot.is_some_and(|old| slot > old) && slot < seq.master_effects.len() { slot - 1 } else { slot };
            fx_lane(mapped, param)
        } else {
            key.clone()
        };
        if lanes.insert(mapped, value.clone()).is_some() {
            return Err(error("ambiguous mixer automation lane aliases; repair them before reordering inserts"));
        }
    }
    let mut output = existing
        .map(|(_, e)| e.clone())
        .or_else(|| filmcraft_project::find_effect(OWNED).map(|d| d.instance()))
        .ok_or_else(|| error("owned output limiter definition is unavailable"))?;
    if p.replace_animation {
        for param in output.params.values_mut() {
            param.keyframes.clear();
        }
    }
    for (id, value) in [("max", ceiling), ("boost", 0.0), ("lookahead", 5.0)] {
        let param = output.param_mut(id).ok_or_else(|| error(format!("output limiter is missing {id}")))?;
        param.value = ParamValue::Float(value);
        param.keyframes.clear();
    }
    output.enabled = true;
    output.post_fader = true;
    let mut effects: Vec<_> = seq.master_effects.iter().filter(|e| e.effect != OWNED).cloned().collect();
    effects.push(output);
    let changed = effects != seq.master_effects || lanes != seq.master_mixer.lanes;
    if changed {
        s.edit("Protect Final Mix Output", |project, _| {
            let seq = project.sequence_mut(sequence).ok_or(crate::EngineError::NoSequence)?;
            seq.master_effects = effects;
            seq.master_mixer.lanes = lanes;
            Ok(())
        })?;
    }
    let effect = s.project.sequence(sequence).and_then(|q| q.master_effects.last()).ok_or_else(|| error("output limiter disappeared"))?;
    Ok(json!({"sequence":sequence.0,"revision":s.revision,"changed":changed,"effect":OWNED,"instance":effect.instance_id,
        "slot":new_slot,"postFader":true,"ceilingDbfs":ceiling,"inputBoostDb":0.0,
        "warning":"Final native sample-peak limiter follows summed sources, user Mix inserts and Mix fader. True peak, export conversion and loudness require independent final-output verification."}))
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "audio.mix.limit",
        label: "Protect Final Mix Output",
        menu: &[],
        shortcut: None,
        params: r#"{"ceilingDbfs":-24..0=-1.5,"replaceAnimation":bool=false}"#,
        enabled: has_seq,
        run: limit,
        journal: true,
    }]
}

#[cfg(test)]
mod tests;
