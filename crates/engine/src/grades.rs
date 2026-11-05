//! Typed, revision-guarded access to an explicitly addressed Lumetri instance.
//!
//! `effectInstance` is `lumetri:<ordinal>` among the clip's Lumetri instances, matching
//! `EffectSel`. It is valid at the returned revision, not a persistent project identity.

use std::collections::{BTreeMap, BTreeSet};

use filmcraft_project::{ClipId, EffectInstance, ItemId, Keyframe, Mask, Param, ParamDef, ParamKind, ParamValue, TrackKind};
use filmcraft_time::Tick;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};

const MAX_KEYS: usize = 16_384;
const MAX_POINTS: usize = 4_096;
const MAX_TARGETS: usize = 256;

pub(crate) fn commands() -> Vec<CommandSpec> {
    [
        ("grade.get", "Inspect Grade", r#"{"clip":id,"sequence":id?,"effectInstance":"lumetri:0"?}"#, false, get as fn(&mut Session, &Value) -> Result<Value>),
        ("grade.patch", "Patch Grade", r#"{"clip":id,"sequence":id?,"effectInstance":"lumetri:0"|"new","expectedRevision":n,"params":{id:{"value":ParamValue?,"keyframes":[Keyframe]?}}?,"enabled":bool?,"masks":[Mask]?,"replaceKeyframes":bool=false}"#, true, patch),
        ("grade.copy", "Copy Grade", r#"{"source":{"clip":id,"sequence":id?,"effectInstance":"lumetri:0"},"targets":[{"clip":id,"sequence":id?,"effectInstance":"lumetri:0"|"new"}],"expectedRevision":n,"replaceKeyframes":bool=false}"#, true, copy),
        ("grade.reset", "Reset Grade", r#"{"clip":id,"sequence":id?,"effectInstance":"lumetri:0","expectedRevision":n,"params":[id]?,"replaceKeyframes":bool=false}"#, true, reset),
        ("grade.matchBatch", "Match Shots to Hero", r#"{"reference":{"sequence":id?,"clip":id?,"item":id?,"frames":[n]?},"targets":[{"clip":id,"sequence":id?,"effectInstance":"lumetri:0"|"new","frames":[n]?}],"expectedRevision":n,"mode":"full"|"exposure"|"white_balance"?,"samplesPerClip":n=3,"minConfidence":n=0.25,"skinProtection":bool=true,"replaceKeyframes":bool=false}"#, true, match_batch),
    ].into_iter().map(|(id, label, params, journal, run)| CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal }).collect()
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Address {
    clip: u64,
    sequence: Option<u64>,
    effect_instance: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParamPatch {
    value: Option<ParamValue>,
    keyframes: Option<Vec<Keyframe>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Patch {
    #[serde(flatten)]
    address: Address,
    expected_revision: u64,
    #[serde(default)]
    params: BTreeMap<String, ParamPatch>,
    enabled: Option<bool>,
    masks: Option<Vec<Mask>>,
    #[serde(default)]
    replace_keyframes: bool,
}

fn decode<T: for<'de> Deserialize<'de>>(p: &Value, cmd: &str) -> Result<T> {
    // Bound nesting and size before recursively cloning/deserializing an engine-supplied Value.
    // The TCP parser also bounds JSON depth, but direct engine callers need the same protection.
    let mut pending = vec![(p, 0usize)];
    let mut budget = 4usize * 1024 * 1024;
    while let Some((v, depth)) = pending.pop() {
        if depth > 32 || pending.len() > 262_144 {
            return Err(bad(cmd, "grade request is too deeply nested or large"));
        }
        let cost = match v {
            Value::String(v) => v.len().saturating_mul(6).saturating_add(2),
            Value::Array(v) => {
                if v.len() > 65_536 {
                    return Err(bad(cmd, "grade array exceeds 65536 entries"));
                }
                pending.extend(v.iter().map(|v| (v, depth + 1)));
                v.len().saturating_add(2)
            }
            Value::Object(v) => {
                if v.len() > 1024 {
                    return Err(bad(cmd, "grade object exceeds 1024 fields"));
                }
                pending.extend(v.values().map(|v| (v, depth + 1)));
                v.keys().fold(2usize, |n, key| n.saturating_add(key.len().saturating_mul(6).saturating_add(4)))
            }
            _ => 32,
        };
        budget = budget.checked_sub(cost).ok_or_else(|| bad(cmd, "grade request exceeds the 4 MiB structural budget"))?;
    }
    serde_json::from_value(p.clone()).map_err(|e| bad(cmd, e.to_string()))
}

fn guard(s: &Session, revision: u64, cmd: &str) -> Result<()> {
    if revision != s.revision {
        return Err(bad(cmd, format!("stale_revision: expected {revision}, current {}; inspect the grade again", s.revision)));
    }
    Ok(())
}

struct Resolved {
    sequence: ItemId,
    clip: ClipId,
    index: Option<usize>,
    ordinal: usize,
    duration: Tick,
    effect: EffectInstance,
}

fn resolve(s: &Session, a: &Address, write: bool, cmd: &str) -> Result<Resolved> {
    let sequence = a.sequence.map(ItemId).or(s.state.active_sequence).ok_or_else(|| bad(cmd, "need `sequence` or an active sequence"))?;
    let q = s.project.sequence(sequence).ok_or_else(|| bad(cmd, "no such sequence"))?;
    let clip = ClipId(a.clip);
    let (track, it) = q.find_item(clip).ok_or_else(|| bad(cmd, "no such clip in the requested sequence"))?;
    let track = q.track(track).ok_or_else(|| bad(cmd, "no such track"))?;
    if track.kind != TrackKind::Video {
        return Err(bad(cmd, "grade target must be a video clip"));
    }
    if write && track.locked {
        return Err(bad(cmd, "the target track is locked"));
    }
    if it.duration <= Tick::ZERO {
        return Err(bad(cmd, "clip duration must be positive"));
    }
    let (index, ordinal, effect) = if a.effect_instance == "new" && write {
        let effect = filmcraft_project::find_effect("lumetri").ok_or_else(|| bad(cmd, "Lumetri definition unavailable"))?.instance();
        (None, it.effects.iter().filter(|e| e.effect == "lumetri").count(), effect)
    } else {
        let ordinal = a
            .effect_instance
            .strip_prefix("lumetri:")
            .and_then(|i| i.parse::<usize>().ok())
            .ok_or_else(|| bad(cmd, "`effectInstance` must be `lumetri:<ordinal>` (or `new` when writing)"))?;
        let (index, effect) = it
            .effects
            .iter()
            .enumerate()
            .filter(|(_, e)| e.effect == "lumetri")
            .nth(ordinal)
            .ok_or_else(|| bad(cmd, "no such Lumetri instance at this revision"))?;
        (Some(index), ordinal, effect.clone())
    };
    Ok(Resolved { sequence, clip, index, ordinal, duration: it.duration, effect })
}

fn describe(s: &Session, r: &Resolved) -> Value {
    json!({"sequence":r.sequence.0,"clip":r.clip.0,"effectInstance":format!("lumetri:{}",r.ordinal),
        "revision":s.revision,"enabled":r.effect.enabled,"params":r.effect.params,"masks":r.effect.masks,
        "schema":filmcraft_project::find_effect("lumetri").map(|d| &d.params),"timeUnit":"ticks","keyframeTimeOrigin":"source_media"})
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Get {
        clip: u64,
        sequence: Option<u64>,
        effect_instance: Option<String>,
    }
    let p: Get = decode(p, "grade.get")?;
    if let Some(instance) = p.effect_instance {
        return Ok(describe(s, &resolve(s, &Address { clip: p.clip, sequence: p.sequence, effect_instance: instance }, false, "grade.get")?));
    }
    let sequence = p.sequence.map(ItemId).or(s.state.active_sequence).ok_or_else(|| bad("grade.get", "no active sequence"))?;
    let q = s.project.sequence(sequence).ok_or_else(|| bad("grade.get", "no such sequence"))?;
    let (_, it) = q.find_item(ClipId(p.clip)).ok_or_else(|| bad("grade.get", "no such clip"))?;
    let count = it.effects.iter().filter(|e| e.effect == "lumetri").count();
    let instances = (0..count)
        .map(|n| {
            resolve(s, &Address { clip: p.clip, sequence: Some(sequence.0), effect_instance: format!("lumetri:{n}") }, false, "grade.get")
                .map(|r| describe(s, &r))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"sequence":sequence.0,"clip":p.clip,"revision":s.revision,"instances":instances}))
}

fn validate_value(s: &Session, value: &ParamValue, def: &ParamDef, cmd: &str) -> Result<()> {
    let invalid = || bad(cmd, format!("invalid typed value for `{}`", def.id));
    let valid = match (&def.kind, value) {
        (ParamKind::Float { min, max, .. }, ParamValue::Float(v)) => v.is_finite() && v >= min && v <= max,
        (ParamKind::Angle, ParamValue::Float(v)) => v.is_finite(),
        (ParamKind::Bool, ParamValue::Bool(_)) => true,
        (ParamKind::Choice(choices), ParamValue::Choice(v)) => usize::try_from(*v).is_ok_and(|v| v < choices.len()),
        (ParamKind::Point, ParamValue::Vec2(v)) => v.x.is_finite() && v.y.is_finite(),
        (ParamKind::Wheel, ParamValue::Vec2(v)) => v.x.is_finite() && v.y.is_finite() && (-1.0..=1.0).contains(&v.x) && (-1.0..=1.0).contains(&v.y),
        (ParamKind::Color, ParamValue::Color(v)) => v.iter().all(|v| v.is_finite()),
        (ParamKind::Text, ParamValue::Text(v)) => {
            v.len() <= 65_536 && (!matches!(def.id, "input_lut" | "look_lut") || v.is_empty() || filmcraft_render::luts::resolve(Some(&s.project), v).is_some())
        }
        (ParamKind::Curve { hue }, ParamValue::Curve(v)) => {
            ((*hue && v.is_empty()) || (2..=MAX_POINTS).contains(&v.len()))
                && v.iter().flatten().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
                && v.windows(2).all(|w| w.first().zip(w.get(1)).is_some_and(|(a, b)| a[0] < b[0]))
        }
        (ParamKind::Path, ParamValue::Path(path)) => {
            (3..=MAX_POINTS).contains(&path.vertices.len())
                && path
                    .vertices
                    .iter()
                    .all(|v| [v.p, v.t_in, v.t_out].iter().all(|p| p.x.is_finite() && p.y.is_finite() && p.x.abs() <= 1e7 && p.y.abs() <= 1e7))
        }
        _ => false,
    };
    if !valid {
        return Err(invalid());
    }
    Ok(())
}

fn validate_param(s: &Session, param: &Param, def: &ParamDef, _duration: Tick, cmd: &str) -> Result<()> {
    validate_value(s, &param.value, def, cmd)?;
    if param.keyframes.len() > MAX_KEYS || (!def.animatable && !param.keyframes.is_empty()) {
        return Err(bad(cmd, format!("too many keyframes or non-animatable parameter `{}`", def.id)));
    }
    let mut previous = None;
    for k in &param.keyframes {
        if k.time < Tick::ZERO
            || k.time > Tick::MAX
            || previous.is_some_and(|t| k.time <= t)
            || !k.out_influence.is_finite()
            || !k.in_influence.is_finite()
            || !(0.0..=1.0).contains(&k.out_influence)
            || !(0.0..=1.0).contains(&k.in_influence)
        {
            return Err(bad(cmd, format!("invalid or unordered keyframes for `{}`", def.id)));
        }
        validate_value(s, &k.value, def, cmd)?;
        previous = Some(k.time);
    }
    Ok(())
}

fn validate_masks(s: &Session, masks: &[Mask], duration: Tick, cmd: &str) -> Result<()> {
    if masks.len() > 64 {
        return Err(bad(cmd, "at most 64 masks per grade"));
    }
    for mask in masks {
        if mask.name.len() > 1024 {
            return Err(bad(cmd, "mask name exceeds 1024 bytes"));
        }
        for def in filmcraft_project::mask::mask_param_defs() {
            let p = mask.param(def.id).ok_or_else(|| bad(cmd, "missing mask parameter"))?;
            validate_param(s, p, def, duration, cmd)?;
        }
    }
    Ok(())
}

fn validate_effect(s: &Session, effect: &EffectInstance, duration: Tick, cmd: &str) -> Result<()> {
    let def = filmcraft_project::find_effect("lumetri").ok_or_else(|| bad(cmd, "Lumetri unavailable"))?;
    for (id, p) in &effect.params {
        let d = def.param(id).ok_or_else(|| bad(cmd, format!("unknown Lumetri parameter `{id}`")))?;
        validate_param(s, p, d, duration, cmd)?;
    }
    validate_masks(s, &effect.masks, duration, cmd)
}

fn install(s: &mut Session, label: &str, resolved: &[Resolved]) -> Result<()> {
    s.edit(label, |project, _| {
        for r in resolved {
            let (_, it) = project.sequence_mut(r.sequence).and_then(|q| q.find_item_mut(r.clip)).ok_or_else(|| bad("grade", "target clip disappeared"))?;
            if let Some(index) = r.index {
                let effect = it.effects.get_mut(index).ok_or_else(|| bad("grade", "target instance disappeared"))?;
                *effect = r.effect.clone();
            } else {
                it.effects.push(r.effect.clone());
            }
        }
        Ok(())
    })
}

fn patch(s: &mut Session, p: &Value) -> Result<Value> {
    let p: Patch = decode(p, "grade.patch")?;
    guard(s, p.expected_revision, "grade.patch")?;
    if p.params.is_empty() && p.enabled.is_none() && p.masks.is_none() {
        return Err(bad("grade.patch", "empty grade patch"));
    }
    let mut r = resolve(s, &p.address, true, "grade.patch")?;
    let def = filmcraft_project::find_effect("lumetri").ok_or_else(|| bad("grade.patch", "Lumetri unavailable"))?;
    for (id, patch) in p.params {
        let d = def.param(&id).ok_or_else(|| bad("grade.patch", format!("unknown Lumetri parameter `{id}`")))?;
        if patch.value.is_none() && patch.keyframes.is_none() {
            return Err(bad("grade.patch", format!("empty patch for `{id}`")));
        }
        let prm = r.effect.params.entry(id.clone()).or_insert_with(|| Param::new(d.default.clone()));
        if let Some(value) = patch.value {
            prm.value = value;
        }
        if let Some(keys) = patch.keyframes {
            if !prm.keyframes.is_empty() && prm.keyframes != keys && !p.replace_keyframes {
                return Err(bad("grade.patch", format!("`{id}` is animated; set replaceKeyframes:true to replace its keyframes")));
            }
            prm.keyframes = keys;
        }
        validate_param(s, prm, d, r.duration, "grade.patch")?;
    }
    if let Some(masks) = p.masks {
        if r.effect.masks != masks && r.effect.masks.iter().any(Mask::is_animated) && !p.replace_keyframes {
            return Err(bad("grade.patch", "animated masks require replaceKeyframes:true"));
        }
        validate_masks(s, &masks, r.duration, "grade.patch")?;
        r.effect.masks = masks;
    }
    if let Some(enabled) = p.enabled {
        r.effect.enabled = enabled;
    }
    install(s, "Patch Grade", std::slice::from_ref(&r))?;
    Ok(describe(s, &r))
}

fn copy(s: &mut Session, p: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Copy {
        source: Address,
        targets: Vec<Address>,
        expected_revision: u64,
        #[serde(default)]
        replace_keyframes: bool,
    }
    let p: Copy = decode(p, "grade.copy")?;
    guard(s, p.expected_revision, "grade.copy")?;
    if p.targets.is_empty() || p.targets.len() > MAX_TARGETS {
        return Err(bad("grade.copy", "need 1..256 target instances"));
    }
    let source = resolve(s, &p.source, false, "grade.copy")?;
    let mut targets = Vec::new();
    let mut seen = BTreeSet::new();
    for a in &p.targets {
        let mut r = resolve(s, a, true, "grade.copy")?;
        if !seen.insert((r.sequence, r.clip, r.ordinal)) {
            return Err(bad("grade.copy", "duplicate target instance"));
        }
        if r.effect != source.effect && r.effect.is_animated() && !p.replace_keyframes {
            return Err(bad("grade.copy", "target is animated; set replaceKeyframes:true before replacing it"));
        }
        r.effect = source.effect.clone();
        validate_effect(s, &r.effect, r.duration, "grade.copy")?;
        targets.push(r);
    }
    install(s, "Copy Grade", &targets)?;
    Ok(json!({"revision":s.revision,"targets":targets.iter().map(|r|describe(s,r)).collect::<Vec<_>>()}))
}

fn reset(s: &mut Session, p: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Reset {
        #[serde(flatten)]
        address: Address,
        expected_revision: u64,
        params: Option<Vec<String>>,
        #[serde(default)]
        replace_keyframes: bool,
    }
    let p: Reset = decode(p, "grade.reset")?;
    guard(s, p.expected_revision, "grade.reset")?;
    if p.address.effect_instance == "new" {
        return Err(bad("grade.reset", "reset requires an existing instance"));
    }
    let mut r = resolve(s, &p.address, true, "grade.reset")?;
    let defaults = filmcraft_project::find_effect("lumetri").ok_or_else(|| bad("grade.reset", "Lumetri unavailable"))?.instance();
    if let Some(ids) = p.params {
        if ids.is_empty() || ids.len() > defaults.params.len() {
            return Err(bad("grade.reset", "need a bounded, nonempty list of parameter ids"));
        }
        let mut seen = BTreeSet::new();
        for id in ids {
            if !seen.insert(id.clone()) {
                return Err(bad("grade.reset", "duplicate parameter id"));
            }
            let value = defaults.params.get(&id).ok_or_else(|| bad("grade.reset", format!("unknown parameter `{id}`")))?;
            if r.effect.params.get(&id).is_some_and(Param::is_animated) && !p.replace_keyframes {
                return Err(bad("grade.reset", "animated parameters require replaceKeyframes:true"));
            }
            r.effect.params.insert(id, value.clone());
        }
    } else {
        if r.effect.is_animated() && !p.replace_keyframes {
            return Err(bad("grade.reset", "animated grade requires replaceKeyframes:true"));
        }
        r.effect = defaults;
    }
    install(s, "Reset Grade", std::slice::from_ref(&r))?;
    Ok(describe(s, &r))
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MatchMode {
    #[default]
    Full,
    #[serde(alias = "exposure-only")]
    Exposure,
    #[serde(alias = "white-balance-only")]
    WhiteBalance,
}

fn sample_times(settings: &filmcraft_project::SequenceSettings, start: Tick, duration: Tick, frames: Option<&[i64]>, samples: usize) -> Result<Vec<Tick>> {
    const CMD: &str = "grade.matchBatch";
    settings.validate().map_err(|e| bad(CMD, e.to_string()))?;
    if !settings.color.is_plain() {
        return Err(bad(CMD, "batch matching currently requires an SDR Rec.709 sequence; HDR and wide-gamut matching is unsupported"));
    }
    let end = start.0.checked_add(duration.0).filter(|v| *v > start.0).map(Tick).ok_or_else(|| bad(CMD, "invalid clip range"))?;
    if let Some(frames) = frames {
        if frames.is_empty() || frames.len() > 8 {
            return Err(bad(CMD, "provide 1..8 frames per shot"));
        }
        let mut seen = BTreeSet::new();
        return frames
            .iter()
            .map(|f| {
                if *f < 0 {
                    return Err(bad(CMD, "frame must be nonnegative"));
                }
                let t = settings.frame_rate.tick_of(*f);
                if t < start || t >= end || !seen.insert(t) {
                    return Err(bad(CMD, "sample frames must be unique and inside the addressed clip"));
                }
                Ok(t)
            })
            .collect();
    }
    let mut times = Vec::new();
    for n in 0..samples {
        let offset = (i128::from(duration.0) * i128::try_from(n + 1).map_err(|e| bad(CMD, e.to_string()))?)
            / i128::try_from(samples + 1).map_err(|e| bad(CMD, e.to_string()))?;
        let offset = i64::try_from(offset).map_err(|e| bad(CMD, e.to_string()))?;
        let t = settings.frame_rate.snap(Tick(start.0.saturating_add(offset))).max(start).min(Tick(end.0.saturating_sub(1)));
        if !times.contains(&t) {
            times.push(t);
        }
    }
    Ok(times)
}

fn sample_atlas(s: &Session, project: &filmcraft_project::Project, sequence: ItemId, clip: ClipId, times: &[Tick]) -> Result<filmcraft_render::Image> {
    const CMD: &str = "grade.matchBatch";
    let q = project.sequence(sequence).ok_or_else(|| bad(CMD, "no such sequence"))?;
    q.settings.validate().map_err(|e| bad(CMD, e.to_string()))?;
    let scale = (192.0 / q.settings.width.max(q.settings.height) as f32).min(1.0);
    let provider = s.media.full_res_provider(std::sync::Arc::new(project.clone()), s.services.clone()).own_sources().recording();
    let mut atlas = filmcraft_render::Image::new(96, 54usize.saturating_mul(times.len()));
    if atlas.px.is_empty() {
        return Err(bad(CMD, "could not allocate sample atlas"));
    }
    for (n, t) in times.iter().enumerate() {
        let image = filmcraft_render::render_clip(
            project,
            sequence,
            clip,
            *t,
            filmcraft_render::RenderOptions { scale, working_output: true, ..Default::default() },
            &provider,
        )
        .ok_or_else(|| bad(CMD, "sample clip has no picture"))?;
        if image.px.is_empty() || image.w == 0 || image.h == 0 {
            return Err(bad(CMD, "empty rendered sample"));
        }
        for (y, row) in atlas.px.as_chunks_mut::<{ 96 * 4 }>().0.iter_mut().skip(n * 54).take(54).enumerate() {
            for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let sample = image.sample_bilinear_clamped((x as f32 + 0.5) * image.w as f32 / 96.0, (y as f32 + 0.5) * image.h as f32 / 54.0);
                pixel.copy_from_slice(&sample);
            }
        }
    }
    if !provider.offline_used().is_empty() {
        return Err(bad(CMD, "sample media is offline or unreadable; relink it before matching"));
    }
    Ok(atlas)
}

fn stats_json(stats: &filmcraft_render::color_match::Stats) -> Value {
    json!({"tonalBands":stats.bands,"chroma":stats.chroma,"skin":stats.skin,"colorSpace":"linear-rec709-oklab","bandFields":["coverage","L","a","b"]})
}

fn stats_distance(a: &filmcraft_render::color_match::Stats, b: &filmcraft_render::color_match::Stats, mode: MatchMode) -> f32 {
    let mut sum = 0.0;
    for (a, b) in a.bands.iter().zip(&b.bands) {
        let weight = (a[0].min(b[0]) * 3.0).clamp(0.0, 1.0);
        if !matches!(mode, MatchMode::WhiteBalance) {
            sum += (a[1] - b[1]).powi(2) * weight;
        }
        if !matches!(mode, MatchMode::Exposure) {
            sum += 4.0 * ((a[2] - b[2]).powi(2) + (a[3] - b[3]).powi(2)) * weight;
        }
    }
    if matches!(mode, MatchMode::Full) {
        sum += 4.0 * (a.chroma - b.chroma).powi(2);
    }
    sum.sqrt()
}

fn put(effect: &mut EffectInstance, id: &str, value: ParamValue) {
    let param = effect.params.entry(id.to_string()).or_insert_with(|| Param::new(value.clone()));
    param.value = value;
    param.keyframes.clear();
}

fn apply_sample(image: &filmcraft_render::Image, effect: &EffectInstance, project: &filmcraft_project::Project) -> filmcraft_render::Image {
    let mut image = image.clone();
    let cx = filmcraft_render::effects::FxCtx {
        t: Tick::ZERO,
        px_scale: 0.05,
        seconds: 0.0,
        timecode: "",
        clip_name: "",
        project: Some(project),
        env: None,
        frame_rate: 24.0,
        working: filmcraft_color::WorkingSpace::Rec709,
    };
    filmcraft_render::effects::apply(&mut image, effect, &cx);
    image
}

fn solve_batch(
    s: &Session,
    current: &filmcraft_render::Image,
    reference: &filmcraft_render::Image,
    base: &EffectInstance,
    mode: MatchMode,
    skin: bool,
) -> EffectInstance {
    let mut candidate = base.clone();
    put(&mut candidate, "basic_on", ParamValue::Bool(true));
    candidate.enabled = true;
    match mode {
        MatchMode::Full => {
            let m = filmcraft_render::color_match::solve(current, reference, base, skin);
            let vector = |v: [f32; 2]| ParamValue::Vec2(filmcraft_geom::Vec2::new(f64::from(v[0]), f64::from(v[1])));
            for (id, value) in [
                ("wheel_shadows", vector(m.shadows)),
                ("wheel_midtones", vector(m.midtones)),
                ("wheel_highlights", vector(m.highlights)),
                ("wheel_shadows_l", ParamValue::Float(f64::from(m.lightness[0]))),
                ("wheel_midtones_l", ParamValue::Float(f64::from(m.lightness[1]))),
                ("wheel_highlights_l", ParamValue::Float(f64::from(m.lightness[2]))),
                ("saturation", ParamValue::Float(f64::from(m.saturation))),
                ("wheels_on", ParamValue::Bool(true)),
            ] {
                put(&mut candidate, id, value);
            }
        }
        mode => {
            let refs = filmcraft_render::color_match::stats(reference, skin);
            let cost = |e: &EffectInstance| stats_distance(&filmcraft_render::color_match::stats(&apply_sample(current, e, &s.project), skin), &refs, mode);
            let ids: &[&str] = if matches!(mode, MatchMode::Exposure) { &["exposure"] } else { &["temperature", "tint"] };
            let bound = if matches!(mode, MatchMode::Exposure) { 5.0_f64 } else { 100.0_f64 };
            // A bounded deterministic search over the actual FilmCraft effect. Each successive
            // pass halves the step; only the requested control family is changed.
            let mut step = bound;
            for _ in 0..12 {
                for id in ids {
                    let center = candidate.params.get(*id).and_then(|p| p.value.as_f64()).unwrap_or(0.0).clamp(-bound, bound);
                    let mut best = cost(&candidate);
                    let mut winner = center;
                    for value in [(center - step).max(-bound), (center + step).min(bound)] {
                        let mut trial = candidate.clone();
                        put(&mut trial, id, ParamValue::Float(value));
                        let score = cost(&trial);
                        if score < best {
                            best = score;
                            winner = value;
                        }
                    }
                    put(&mut candidate, id, ParamValue::Float(winner));
                }
                step *= 0.5;
            }
        }
    }
    candidate
}

fn match_batch(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.matchBatch";
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reference {
        sequence: Option<u64>,
        clip: Option<u64>,
        item: Option<u64>,
        frames: Option<Vec<i64>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Target {
        #[serde(flatten)]
        address: Address,
        frames: Option<Vec<i64>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Batch {
        reference: Reference,
        targets: Vec<Target>,
        expected_revision: u64,
        #[serde(default)]
        mode: MatchMode,
        samples_per_clip: Option<usize>,
        min_confidence: Option<f32>,
        skin_protection: Option<bool>,
        #[serde(default)]
        replace_keyframes: bool,
    }
    let p: Batch = decode(p, CMD)?;
    guard(s, p.expected_revision, CMD)?;
    let samples = p.samples_per_clip.unwrap_or(3);
    let confidence_min = p.min_confidence.unwrap_or(0.25);
    if !(1..=8).contains(&samples) || !confidence_min.is_finite() || !(0.0..=1.0).contains(&confidence_min) || p.targets.is_empty() || p.targets.len() > 64 {
        return Err(bad(CMD, "need 1..64 targets, 1..8 samples, and minConfidence in 0..1"));
    }
    let skin = p.skin_protection.unwrap_or(true);
    let keys: &[&str] = match p.mode {
        MatchMode::Full => &[
            "wheel_shadows",
            "wheel_midtones",
            "wheel_highlights",
            "wheel_shadows_l",
            "wheel_midtones_l",
            "wheel_highlights_l",
            "saturation",
            "wheels_on",
            "basic_on",
        ],
        MatchMode::Exposure => &["exposure", "basic_on"],
        MatchMode::WhiteBalance => &["temperature", "tint", "basic_on"],
    };
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    for target in &p.targets {
        let r = resolve(s, &target.address, true, CMD)?;
        if !seen.insert((r.sequence, r.clip)) {
            return Err(bad(CMD, "each target clip may occur only once"));
        }
        if !p.replace_keyframes && keys.iter().any(|k| r.effect.params.get(*k).is_some_and(Param::is_animated)) {
            return Err(bad(CMD, "matching would replace animation; set replaceKeyframes:true explicitly"));
        }
        let q = s.project.sequence(r.sequence).ok_or_else(|| bad(CMD, "no such target sequence"))?;
        let (_, it) = q.find_item(r.clip).ok_or_else(|| bad(CMD, "no such target clip"))?;
        let times = sample_times(&q.settings, it.start, it.duration, target.frames.as_deref(), samples)?;
        targets.push((r, times));
    }
    let mut reference_project = (*s.project).clone();
    let (ref_seq, ref_clip) = match (p.reference.item, p.reference.clip, p.reference.sequence) {
        (Some(item), None, None) => {
            let item = ItemId(item);
            let source = reference_project.item(item).ok_or_else(|| bad(CMD, "no such reference item"))?;
            let rate = source.frame_rate();
            let duration = source.duration();
            let media = reference_project.resolve_media(item).ok_or_else(|| bad(CMD, "reference item must be media"))?.1;
            let video = media.info.video.as_ref().ok_or_else(|| bad(CMD, "reference item has no picture"))?;
            let settings = filmcraft_project::SequenceSettings { width: video.width, height: video.height, frame_rate: rate, ..Default::default() };
            settings.validate().map_err(|e| bad(CMD, e.to_string()))?;
            if duration <= Tick::ZERO || duration > Tick::MAX {
                return Err(bad(CMD, "reference media duration is invalid"));
            }
            let sequence = reference_project.new_sequence("Match Reference", settings, 1, 0, None);
            let clip = reference_project
                .make_track_item(item, TrackKind::Video, Tick::ZERO, filmcraft_time::TimeRange::new(Tick::ZERO, duration), rate)
                .ok_or_else(|| bad(CMD, "could not address reference media"))?;
            let id = clip.id;
            let q = reference_project.sequence_mut(sequence).ok_or_else(|| bad(CMD, "could not address reference sequence"))?;
            q.video_tracks.first_mut().ok_or_else(|| bad(CMD, "reference track unavailable"))?.items.push(clip);
            (sequence, id)
        }
        (None, Some(clip), sequence) => (sequence.map(ItemId).or(s.state.active_sequence).ok_or_else(|| bad(CMD, "reference needs a sequence"))?, ClipId(clip)),
        _ => return Err(bad(CMD, "reference must address either a media `item`, or a timeline `clip` with optional `sequence`")),
    };
    let q = reference_project.sequence(ref_seq).ok_or_else(|| bad(CMD, "no such reference sequence"))?;
    let (_, it) = q.find_item(ref_clip).ok_or_else(|| bad(CMD, "no such reference clip"))?;
    let reference_times = sample_times(&q.settings, it.start, it.duration, p.reference.frames.as_deref(), samples)?;
    if targets.iter().any(|(r, _)| r.sequence == ref_seq && r.clip == ref_clip && p.reference.item.is_none()) {
        return Err(bad(CMD, "a target cannot be its own hero reference"));
    }
    let reference = sample_atlas(s, &reference_project, ref_seq, ref_clip, &reference_times)?;
    let reference_stats = filmcraft_render::color_match::stats(&reference, skin);
    let mut reports = Vec::new();
    let mut accepted = Vec::new();
    for (mut r, times) in targets {
        let before = sample_atlas(s, &s.project, r.sequence, r.clip, &times)?;
        let before_stats = filmcraft_render::color_match::stats(&before, skin);
        let overlap = before_stats.bands.iter().zip(&reference_stats.bands).map(|(a, b)| a[0].min(b[0])).sum::<f32>();
        let clipped = before.px.as_chunks::<4>().0.iter().filter(|p| p.iter().take(3).all(|v| *v <= 0.001) || p.iter().take(3).any(|v| *v >= 0.999)).count()
            as f32
            / (before.w.saturating_mul(before.h).max(1) as f32);
        let confidence = (overlap * (1.0 - clipped)).clamp(0.0, 1.0);
        let distance_before = stats_distance(&before_stats, &reference_stats, p.mode);
        let mut report = json!({"clip":r.clip.0,"sequence":r.sequence.0,"effectInstance":format!("lumetri:{}",r.ordinal),"sampleTimes":times.iter().map(|t|t.0).collect::<Vec<_>>(),"confidence":confidence,"confidenceKind":"heuristic_tonal_overlap_and_unclipped_fraction","before":stats_json(&before_stats),"distanceBefore":distance_before});
        if confidence < confidence_min || distance_before <= 1e-5 {
            report["status"] = json!("skipped");
            report["reason"] = json!(if confidence < confidence_min { "low_confidence" } else { "already_matched" });
            reports.push(report);
            continue;
        }
        let mut probe = (*s.project).clone();
        if let Some(e) = probe.sequence_mut(r.sequence).and_then(|q| q.find_item_mut(r.clip)).and_then(|(_, it)| r.index.and_then(|i| it.effects.get_mut(i))) {
            e.enabled = false;
        }
        let current = sample_atlas(s, &probe, r.sequence, r.clip, &times)?;
        let mut base = r.effect.clone();
        let mt = s
            .project
            .sequence(r.sequence)
            .and_then(|q| q.find_item(r.clip))
            .map(|(_, it)| it.effect_time_at(times.first().copied().unwrap_or(it.start)))
            .ok_or_else(|| bad(CMD, "target disappeared"))?;
        for prm in base.params.values_mut() {
            prm.value = prm.value_at(mt);
            prm.keyframes.clear();
        }
        let solved = solve_batch(s, &current, &reference, &base, p.mode, skin);
        for key in keys {
            let value = solved.params.get(*key).ok_or_else(|| bad(CMD, "solver omitted parameter"))?.value.clone();
            put(&mut r.effect, key, value);
        }
        r.effect.enabled = true;
        validate_effect(s, &r.effect, r.duration, CMD)?;
        let mut after_project = (*s.project).clone();
        let (_, it) = after_project.sequence_mut(r.sequence).and_then(|q| q.find_item_mut(r.clip)).ok_or_else(|| bad(CMD, "target disappeared"))?;
        if let Some(index) = r.index {
            *it.effects.get_mut(index).ok_or_else(|| bad(CMD, "target instance disappeared"))? = r.effect.clone();
        } else {
            it.effects.push(r.effect.clone());
        }
        let after = sample_atlas(s, &after_project, r.sequence, r.clip, &times)?;
        let after_stats = filmcraft_render::color_match::stats(&after, skin);
        let distance_after = stats_distance(&after_stats, &reference_stats, p.mode);
        report["after"] = stats_json(&after_stats);
        report["distanceAfter"] = json!(distance_after);
        report["scopeDelta"] = json!({"chroma":after_stats.chroma-before_stats.chroma,"distance":distance_after-distance_before});
        if distance_after.is_finite() && distance_after < distance_before * 0.9999 {
            report["status"] = json!("matched");
            report["grade"] = json!(r.effect);
            accepted.push(r);
        } else {
            report["status"] = json!("skipped");
            report["reason"] = json!("no_verified_improvement");
        }
        reports.push(report);
    }
    let before_revision = s.revision;
    if !accepted.is_empty() {
        install(s, "Match Shots to Hero", &accepted)?;
    }
    Ok(
        json!({"beforeRevision":before_revision,"afterRevision":s.revision,"matched":accepted.len(),"skipped":reports.len().saturating_sub(accepted.len()),"reference":p.reference.item.map(|item|json!({"item":item})).unwrap_or_else(||json!({"sequence":ref_seq.0,"clip":ref_clip.0})),"referenceTimes":reference_times.iter().map(|t|t.0).collect::<Vec<_>>(),"referenceStats":stats_json(&reference_stats),"results":reports,"timeUnit":"ticks","sampleTimeOrigin":"sequence","colorSpace":"linear-rec709"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture() -> (Session, u64, u64) {
        use filmcraft_media::{Generator, generators::GeneratorSource};
        use filmcraft_project::{Label, Project, SequenceSettings};
        use filmcraft_time::{FrameRate, TICKS_PER_SECOND, TimeRange};
        let mut s = Session::default();
        let mut project = Project::new("Grade Tests");
        let settings = SequenceSettings { width: 64, height: 36, frame_rate: FrameRate::FPS_24, ..Default::default() };
        let rate = settings.frame_rate;
        let seq = project.new_sequence("Shots", settings, 1, 0, None);
        let duration = Tick(2 * TICKS_PER_SECOND);
        let mut clips = Vec::new();
        for (n, color) in [[0.18, 0.18, 0.18, 1.0], [0.08, 0.11, 0.15, 1.0]].into_iter().enumerate() {
            let source = GeneratorSource::new(Generator::ColorMatte { color }, 64, 36, rate, duration);
            let item = crate::demo::add_generator(&mut project, &s.media, source, "Shot", Label::Iris, None);
            let mut clip = project.make_track_item(item, TrackKind::Video, Tick(duration.0 * n as i64), TimeRange::new(Tick::ZERO, duration), rate).unwrap();
            for effect in &mut clip.effects {
                filmcraft_project::resolve_auto_points(effect, (64, 36), (64, 36));
            }
            clips.push(clip.id.0);
            project.sequence_mut(seq).unwrap().video_tracks[0].items.push(clip);
        }
        s.project = Arc::new(project);
        s.state.active_sequence = Some(seq);
        (s, clips[0], clips[1])
    }

    fn add(s: &mut Session, clip: u64) -> Value {
        s.execute("grade.patch", json!({"clip":clip,"effectInstance":"new","expectedRevision":s.revision,"params":{"exposure":{"value":{"Float":0.5}}}}))
            .unwrap()
    }

    #[test]
    fn typed_partial_patch_preserves_other_controls_masks_and_animation() {
        let (mut s, clip, _) = fixture();
        let effect = add(&mut s, clip);
        assert_eq!(effect["effectInstance"], "lumetri:0");
        let duration = s.active_sequence().unwrap().find_item(ClipId(clip)).unwrap().1.duration;
        let keys = vec![Keyframe::new(Tick::ZERO, ParamValue::Float(0.5)), Keyframe::new(duration, ParamValue::Float(1.0))];
        let masks = vec![Mask::new("Face", filmcraft_project::MaskPath::rect(0.0, 0.0, 20.0, 20.0))];
        s.execute(
            "grade.patch",
            json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":s.revision,
            "params":{"exposure":{"keyframes":keys}},"masks":masks}),
        )
        .unwrap();
        let history = s.history.undo.len();
        s.execute("grade.patch",json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":s.revision,
            "params":{"temperature":{"value":{"Float":12.0}},"wheel_midtones":{"value":{"Vec2":{"x":0.2,"y":-0.1}}},"curve_luma":{"value":{"Curve":[[0.0,0.0],[0.4,0.5],[1.0,1.0]]}}}})).unwrap();
        let out = s.execute("grade.get", json!({"clip":clip,"effectInstance":"lumetri:0"})).unwrap();
        assert_eq!(out["params"]["exposure"]["keyframes"], json!(keys));
        assert_eq!(out["masks"], json!(masks));
        assert_eq!(out["params"]["temperature"]["value"], json!({"Float":12.0}));
        assert_eq!(out["params"]["wheel_midtones"]["value"], json!({"Vec2":{"x":0.2,"y":-0.1}}));
        assert_eq!(s.history.undo.len(), history + 1);
        let before = s.project.to_json();
        let rev = s.revision;
        assert!(
            s.execute(
                "grade.patch",
                json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":rev,
            "params":{"exposure":{"keyframes":[]},"temperature":{"value":{"Float":99.0}}}})
            )
            .is_err()
        );
        assert_eq!(s.project.to_json(), before);
        assert_eq!(s.revision, rev);
        s.execute(
            "grade.patch",
            json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":rev,"replaceKeyframes":true,"params":{"exposure":{"keyframes":[]}}}),
        )
        .unwrap();
    }

    #[test]
    fn stale_ordinal_and_hostile_typed_parameters_do_not_mutate() {
        let (mut s, clip, _) = fixture();
        let first = add(&mut s, clip);
        add(&mut s, clip);
        let baseline = s.project.to_json();
        let history = s.history.undo.len();
        let rev = s.revision;
        assert!(s.execute("grade.patch", json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":first["revision"],"enabled":false})).is_err());
        for params in [
            json!({"exposure":{"value":{"Text":"bad"}}}),
            json!({"exposure":{"value":{"Float":1e300}}}),
            json!({"curve_luma":{"value":{"Curve":[[0.7,0.7],[0.1,0.3]]}}}),
            json!({"curve_luma":{"value":{"Curve":[[0.0,0.0]]}}}),
            json!({"curve_luma":{"value":{"Curve":[[0.0,1e300],[1.0,1.0]]}}}),
            json!({"wheel_midtones":{"value":{"Vec2":{"x":null,"y":0.0}}}}),
            json!({"input_lut":{"value":{"Text":"lib:missing"}}}),
            json!({"exposure":{"keyframes":[Keyframe::new(Tick(-1),ParamValue::Float(0.0))]}}),
            json!({"exposure":{"keyframes":[Keyframe::new(Tick::ZERO,ParamValue::Float(0.0)),Keyframe::new(Tick::ZERO,ParamValue::Float(0.0))]}}),
            json!({"nonexistent":{"value":{"Float":0.0}}}),
            json!({"exposure":{}}),
        ] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                s.execute("grade.patch", json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":rev,"params":params}))
            }));
            assert!(result.unwrap().is_err());
            assert_eq!(s.project.to_json(), baseline);
            assert_eq!(s.history.undo.len(), history);
            assert_eq!(s.revision, rev);
        }
        assert!(s.execute("grade.patch", json!({"clip":clip,"effectInstance":"lumetri:0","enabled":false})).is_err());
        assert!(s.execute("grade.patch", json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":rev,"enabled":false,"typo":true})).is_err());
    }

    #[test]
    fn copy_and_reset_are_atomic_and_respect_locked_tracks() {
        let (mut s, clip, other) = fixture();
        add(&mut s, clip);
        let before = s.project.to_json();
        let history = s.history.undo.len();
        let source = json!({"clip":clip,"effectInstance":"lumetri:0"});
        s.execute("grade.copy", json!({"source":source,"targets":[{"clip":other,"effectInstance":"new"}],"expectedRevision":s.revision})).unwrap();
        assert_eq!(s.history.undo.len(), history + 1);
        let copied = s.execute("grade.get", json!({"clip":other,"effectInstance":"lumetri:0"})).unwrap();
        assert_eq!(copied["params"]["exposure"]["value"], json!({"Float":0.5}));
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.project.to_json(), before);
        let before = s.project.to_json();
        assert!(
            s.execute(
                "grade.copy",
                json!({"source":source,"targets":[{"clip":other,"effectInstance":"new"},{"clip":other,"effectInstance":"new"}],"expectedRevision":s.revision})
            )
            .is_err()
        );
        assert_eq!(s.project.to_json(), before);
        s.execute("grade.reset", json!({"clip":clip,"effectInstance":"lumetri:0","params":["exposure"],"expectedRevision":s.revision})).unwrap();
        let out = s.execute("grade.get", json!({"clip":clip,"effectInstance":"lumetri:0"})).unwrap();
        assert_eq!(out["params"]["exposure"]["value"], json!({"Float":0.0}));
        let seq = s.state.active_sequence.unwrap();
        s.edit("Lock Track", |p, _| {
            p.sequence_mut(seq).unwrap().video_tracks[0].locked = true;
            Ok(())
        })
        .unwrap();
        let before = s.project.to_json();
        assert!(s.execute("grade.patch", json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":s.revision,"enabled":false})).is_err());
        assert_eq!(s.project.to_json(), before);
    }

    #[test]
    fn invalid_match_reference_never_adds_effect_or_undo_step() {
        let (mut s, clip, _) = fixture();
        let before = s.project.to_json();
        let history = s.history.undo.len();
        let rev = s.revision;
        for p in [
            json!({"clip":clip}),
            json!({"clip":clip,"referenceFrame":-1}),
            json!({"clip":clip,"referenceFrame":i64::MAX}),
            json!({"clip":clip,"referenceFrame":0}),
        ] {
            assert!(s.execute("lumetri.applyMatch", p).is_err());
            assert_eq!(s.project.to_json(), before);
            assert_eq!(s.history.undo.len(), history);
            assert_eq!(s.revision, rev);
        }
    }

    #[test]
    fn match_rejects_corrupt_empty_or_overflowing_target_range_without_panicking() {
        for (start, duration) in [(Tick::ZERO, Tick::ZERO), (Tick::ZERO, Tick(-1)), (Tick(i64::MAX), Tick(1))] {
            let (mut s, clip, other) = fixture();
            let sequence = s.state.active_sequence.unwrap();
            let reference = s.active_sequence().unwrap().find_item(ClipId(other)).unwrap().1.start;
            Arc::make_mut(&mut s.project).sequence_mut(sequence).unwrap().find_item_mut(ClipId(clip)).unwrap().1.start = start;
            Arc::make_mut(&mut s.project).sequence_mut(sequence).unwrap().find_item_mut(ClipId(clip)).unwrap().1.duration = duration;
            let before = s.project.to_json();
            let revision = s.revision;
            let history = s.history.undo.len();
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("lumetri.applyMatch", json!({"clip":clip,"referenceTime":reference.0}))));
            assert!(result.unwrap().is_err());
            assert_eq!(s.project.to_json(), before);
            assert_eq!(s.revision, revision);
            assert_eq!(s.history.undo.len(), history);
        }
    }

    #[test]
    fn match_requires_explicit_animation_replacement_and_is_one_undo() {
        let (mut s, clip, other) = fixture();
        let reference = s.active_sequence().unwrap().find_item(ClipId(other)).unwrap().1.start;
        let before = s.project.to_json();
        let history = s.history.undo.len();
        s.execute("lumetri.applyMatch", json!({"clip":clip,"referenceTime":reference.0})).unwrap();
        assert_eq!(s.history.undo.len(), history + 1);
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.project.to_json(), before);
        add(&mut s, clip);
        s.execute("grade.patch",json!({"clip":clip,"effectInstance":"lumetri:0","expectedRevision":s.revision,"params":{"saturation":{"keyframes":[Keyframe::new(Tick::ZERO,ParamValue::Float(100.0))]}}})).unwrap();
        let before = s.project.to_json();
        let rev = s.revision;
        assert!(s.execute("lumetri.applyMatch", json!({"clip":clip,"referenceTime":reference.0})).is_err());
        assert_eq!(s.project.to_json(), before);
        assert_eq!(s.revision, rev);
        s.execute("lumetri.applyMatch", json!({"clip":clip,"referenceTime":reference.0,"replaceKeyframes":true})).unwrap();
        let out = s.execute("grade.get", json!({"clip":clip,"effectInstance":"lumetri:0"})).unwrap();
        assert!(out["params"]["saturation"].get("keyframes").is_none());
    }

    #[test]
    fn batch_matches_multiple_samples_and_verifies_actual_delta_in_one_undo() {
        for mode in ["full", "exposure", "white_balance"] {
            let (mut s, hero, target) = fixture();
            let before = s.project.to_json();
            let history = s.history.undo.len();
            let out = s
                .execute(
                    "grade.matchBatch",
                    json!({"reference":{"clip":hero},"targets":[{"clip":target,"effectInstance":"new"}],
                "expectedRevision":s.revision,"mode":mode,"samplesPerClip":3,"skinProtection":false}),
                )
                .unwrap();
            assert_eq!(out["matched"], 1, "{mode}: {out}");
            let result = &out["results"][0];
            assert_eq!(result["sampleTimes"].as_array().unwrap().len(), 3);
            assert!(result["distanceAfter"].as_f64().unwrap() < result["distanceBefore"].as_f64().unwrap(), "{mode}: {out}");
            assert_eq!(s.history.undo.len(), history + 1);
            let grade = s.execute("grade.get", json!({"clip":target,"effectInstance":"lumetri:0"})).unwrap();
            if mode == "exposure" {
                assert_eq!(grade["params"]["temperature"]["value"], json!({"Float":0.0}));
                assert_eq!(grade["params"]["tint"]["value"], json!({"Float":0.0}));
            } else if mode == "white_balance" {
                assert_eq!(grade["params"]["exposure"]["value"], json!({"Float":0.0}));
            }
            s.execute("edit.undo", json!({})).unwrap();
            assert_eq!(s.project.to_json(), before);
        }
    }

    #[test]
    fn batch_accepts_imported_reference_item_and_skips_low_confidence_without_edit() {
        let (mut s, hero, target) = fixture();
        let item = s.active_sequence().unwrap().find_item(ClipId(hero)).unwrap().1.item.0;
        let before = s.project.to_json();
        let out = s
            .execute(
                "grade.matchBatch",
                json!({"reference":{"item":item},"targets":[{"clip":target,"effectInstance":"new"}],"expectedRevision":s.revision,"mode":"exposure"}),
            )
            .unwrap();
        assert_eq!(out["matched"], 1, "{out}");
        s.execute("edit.undo", json!({})).unwrap();
        assert_eq!(s.project.to_json(), before);
        // A pure black shot has no usable color evidence and must not receive a speculative grade.
        let seq = s.state.active_sequence.unwrap();
        let target_item = s.active_sequence().unwrap().find_item(ClipId(target)).unwrap().1.item;
        s.edit("Dark Shot", |project, _| {
            let media = project.item_mut(target_item).unwrap().as_media_mut().unwrap();
            media.media = filmcraft_project::MediaRef::Generator(filmcraft_media::Generator::ColorMatte { color: [0.0, 0.0, 0.0, 1.0] });
            Ok(())
        })
        .unwrap();
        let before = s.project.to_json();
        let rev = s.revision;
        let history = s.history.undo.len();
        let out = s
            .execute(
                "grade.matchBatch",
                json!({"reference":{"clip":hero,"sequence":seq.0},"targets":[{"clip":target,"effectInstance":"new"}],"expectedRevision":rev}),
            )
            .unwrap();
        assert_eq!(out["matched"], 0);
        assert_eq!(out["results"][0]["reason"], "low_confidence");
        assert_eq!(s.project.to_json(), before);
        assert_eq!(s.revision, rev);
        assert_eq!(s.history.undo.len(), history);
    }

    #[test]
    fn batch_invalid_targets_and_reference_are_atomic() {
        let (mut s, hero, target) = fixture();
        let before = s.project.to_json();
        let history = s.history.undo.len();
        let rev = s.revision;
        for request in [
            json!({"reference":{"clip":u64::MAX},"targets":[{"clip":target,"effectInstance":"new"}]}),
            json!({"reference":{"item":u64::MAX},"targets":[{"clip":target,"effectInstance":"new"}]}),
            json!({"reference":{"clip":hero},"targets":[{"clip":target,"effectInstance":"new"},{"clip":u64::MAX,"effectInstance":"new"}]}),
            json!({"reference":{"clip":hero},"targets":[{"clip":target,"effectInstance":"new"},{"clip":target,"effectInstance":"new"}]}),
            json!({"reference":{"clip":hero},"targets":[{"clip":target,"effectInstance":"new","frames":[i64::MAX]}]}),
            json!({"reference":{"clip":hero},"targets":[{"clip":target,"effectInstance":"new"}],"minConfidence":-1.0}),
            json!({"reference":{"clip":hero},"targets":[{"clip":target,"effectInstance":"new"}],"samplesPerClip":0}),
        ] {
            let mut request = request;
            request["expectedRevision"] = json!(rev);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("grade.matchBatch", request)));
            assert!(result.unwrap().is_err());
            assert_eq!(s.project.to_json(), before);
            assert_eq!(s.revision, rev);
            assert_eq!(s.history.undo.len(), history);
        }
    }

    #[test]
    fn matching_rejects_hdr_and_wide_gamut_working_spaces_before_mutation() {
        for (space, wide) in [("rec2100-hlg", false), ("rec2100-pq", false), ("rec709", true)] {
            let (mut s, hero, target) = fixture();
            s.execute("sequence.colorSettings", json!({"workingSpace":space,"wideGamut":wide})).unwrap();
            let before = s.project.to_json();
            let rev = s.revision;
            let history = s.history.undo.len();
            for (command, params) in [
                ("lumetri.applyMatch", json!({"clip":target,"referenceFrame":0})),
                ("grade.matchBatch", json!({"reference":{"clip":hero},"targets":[{"clip":target,"effectInstance":"new"}],"expectedRevision":rev})),
            ] {
                let error = s.execute(command, params).unwrap_err().to_string();
                assert!(error.contains("unsupported"), "{error}");
                assert_eq!(s.project.to_json(), before);
                assert_eq!(s.revision, rev);
                assert_eq!(s.history.undo.len(), history);
            }
        }
    }
}
