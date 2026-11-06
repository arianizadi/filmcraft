//! Non-destructive clip channel repair, conservative presets and reviewable audio preparation.
//! A distinct router effect keeps user channel mixers, source interpretation and animation intact.

use filmcraft_project::{
    ClipId, EffectInstance, ParamValue, TrackItem,
    essential::{self as es, AudioType, EssentialSound},
};
use filmcraft_time::Tick;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    EngineError, Result, Session,
    commands::{CommandSpec, always, bad},
};

const ROUTER: &str = "channel_router";
const MAX_CLIPS: usize = 256;
const MAX_ANALYZED_CLIPS: usize = 16;
pub const MODES: [&str; 8] = ["stereo", "left", "right", "mono", "swap", "midSideEncode", "midSideDecode", "side"];

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Selection {
    clip: Option<u64>,
    clips: Option<Vec<u64>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourcePatch {
    clip: Option<u64>,
    clips: Option<Vec<u64>>,
    stream: Option<usize>,
    channels: Option<Vec<u16>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Streams {
    clips: Vec<u64>,
    main_stream: usize,
    #[serde(default)]
    mute_secondary: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Patch {
    clip: Option<u64>,
    clips: Option<Vec<u64>>,
    mode: Option<String>,
    left_gain_db: Option<f64>,
    right_gain_db: Option<f64>,
    invert_left: Option<bool>,
    invert_right: Option<bool>,
    #[serde(default)]
    replace_animation: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reset {
    clip: Option<u64>,
    clips: Option<Vec<u64>>,
    #[serde(default)]
    replace_animation: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Preset {
    clip: Option<u64>,
    clips: Option<Vec<u64>>,
    preset: String,
    #[serde(default)]
    replace_animation: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Normalize {
    clip: Option<u64>,
    clips: Option<Vec<u64>>,
    target_lufs: Option<f64>,
    max_gain_db: Option<f64>,
    true_peak_dbtp: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Prepare {
    clip: Option<u64>,
    clips: Option<Vec<u64>>,
    routing: Option<String>,
    preset: Option<String>,
    #[serde(default)]
    normalize: bool,
    target_lufs: Option<f64>,
    max_gain_db: Option<f64>,
    true_peak_dbtp: Option<f64>,
    #[serde(default)]
    replace_animation: bool,
    base_revision: Option<u64>,
    label: Option<String>,
}

fn parse<T: for<'de> Deserialize<'de>>(p: &Value, id: &str) -> Result<T> {
    crate::analysis_support::bounded_decode(p, id)
}

fn finite(v: Option<f64>, default: f64, min: f64, max: f64, name: &str, id: &str) -> Result<f64> {
    let v = v.unwrap_or(default);
    if !v.is_finite() || !(min..=max).contains(&v) {
        return Err(bad(id, format!("{name} must be finite in {min}..={max}")));
    }
    Ok(v)
}

fn targets(s: &Session, clip: Option<u64>, clips: Option<&[u64]>, id: &str, empty_ok: bool) -> Result<Vec<ClipId>> {
    if clip.is_some() && clips.is_some() {
        return Err(bad(id, "use clip or clips, not both"));
    }
    let explicit = clip.is_some() || clips.is_some();
    let selected: Vec<ClipId> = match (clip, clips) {
        (Some(c), _) => vec![ClipId(c)],
        (_, Some(c)) => c.iter().copied().map(ClipId).collect(),
        _ => s.state.selection.clone(),
    };
    if selected.len() > MAX_CLIPS {
        return Err(bad(id, format!("at most {MAX_CLIPS} selected clips")));
    }
    let mut seen = std::collections::BTreeSet::new();
    if selected.iter().any(|c| !seen.insert(c.0)) {
        return Err(bad(id, "clip identities must be unique"));
    }
    let Some(seq) = s.active_sequence() else {
        return if empty_ok && !explicit { Ok(Vec::new()) } else { Err(EngineError::NoSequence) };
    };
    if selected.iter().any(|c| seq.find_item(*c).is_none()) {
        return Err(bad(id, "a clip is not in the active sequence"));
    }
    // A mic clip can share an AV link with camera scratch/ambience streams. Selecting that audio
    // clip must not silently repair every other stream. Video selection still reaches linked audio.
    let video: Vec<ClipId> = selected.iter().copied().filter(|c| seq.find_item(*c).is_some_and(|(t, _)| seq.video_tracks.iter().any(|v| v.id == t))).collect();
    let linked = crate::commands::with_links(s, &video);
    let out: Vec<ClipId> =
        seq.audio_tracks.iter().flat_map(|t| &t.items).filter(|it| selected.contains(&it.id) || linked.contains(&it.id)).map(|it| it.id).collect();
    if out.len() > MAX_CLIPS {
        return Err(bad(id, "linked audio expansion exceeds the clip limit"));
    }
    if out.is_empty() && !empty_ok {
        return Err(bad(id, "select at least one audio clip or its linked video"));
    }
    Ok(out)
}

fn clips(s: &Session, ids: &[ClipId], id: &str) -> Result<Vec<TrackItem>> {
    let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
    ids.iter()
        .map(|c| {
            let (track, it) = seq.find_item(*c).ok_or_else(|| bad(id, "audio clip disappeared"))?;
            let t = seq.audio_tracks.iter().find(|t| t.id == track).ok_or_else(|| bad(id, "target must be an audio clip"))?;
            if t.locked {
                return Err(bad(id, format!("audio track {} is locked", t.name)));
            }
            Ok(it.clone())
        })
        .collect()
}

fn router<'a>(it: &'a TrackItem, id: &str) -> Result<Option<&'a EffectInstance>> {
    let mut routers = it.effects.iter().filter(|e| e.effect == ROUTER);
    let first = routers.next();
    if routers.next().is_some() {
        return Err(bad(id, format!("clip {} has several channel routers; choose one in Effect Controls", it.id.0)));
    }
    Ok(first)
}

fn source_channels(s: &Session, it: &TrackItem) -> Option<usize> {
    if let Some((_, media, _)) = s.project.resolve_media(it.item) {
        return media.info.audio_streams.get(it.audio_stream).map(|a| a.channels as usize);
    }
    s.project.sequence(it.item).map(|seq| if seq.settings.audio_master == filmcraft_project::AudioChannels::Surround51 { 6 } else { 2 })
}

fn routing_value(it: &TrackItem, time: Tick) -> Value {
    let fx = it.effects.iter().find(|e| e.effect == ROUTER);
    let value = |key| fx.map(|e| e.f64_at(key, time)).unwrap_or(0.0);
    let index = value("mode");
    let mode = if index.is_finite() && index >= 0.0 { MODES.get(index as usize).copied().unwrap_or("unknown") } else { "unknown" };
    json!({"mode":mode,"leftGainDb":value("gain_l_db"),"rightGainDb":value("gain_r_db"),
        "invertLeft":value("invert_l")>=0.5,"invertRight":value("invert_r")>=0.5,
        "effectInstanceId":fx.map(|e|e.instance_id),"enabled":fx.is_none_or(|e|e.enabled),"animated":fx.is_some_and(EffectInstance::is_animated)})
}

fn get(s: &mut Session, p: &Value) -> Result<Value> {
    let p: Selection = parse(p, "audio.channels.get")?;
    let ids = targets(s, p.clip, p.clips.as_deref(), "audio.channels.get", true)?;
    let mut out = Vec::new();
    if let Some(seq) = s.active_sequence() {
        for track in &seq.audio_tracks {
            for it in track.items.iter().filter(|i| ids.contains(&i.id)) {
                out.push(json!({"clip":it.id.0,"name":it.name,"track":track.name,"trackId":track.id.0,
                    "sourceItem":it.item.0,"sourceChannels":it.source_channels,"sourceChannelCount":source_channels(s,it),
                    "stream":it.audio_stream,"sourceStreams":stream_metadata(s,it),"locked":track.locked,"routing":routing_value(it,it.source_time_at(s.playhead())),
                    "routers":it.effects.iter().filter(|e|e.effect==ROUTER).count()}));
            }
        }
    }
    Ok(json!({"revision":s.revision,"clips":out,"modes":MODES,"presets":["dialogueClean","dialogueWarm","podcast","musicGentle","reset"],
        "description":"Left or right to both produces centered dual mono; it does not synthesize a stereo recording."}))
}

fn stream_metadata(s: &Session, it: &TrackItem) -> Value {
    json!(
        s.project
            .resolve_media(it.item)
            .map(|(_, m, _)| m
                .info
                .audio_streams
                .iter()
                .enumerate()
                .map(|(stream, a)| json!({
                    "stream":stream,"channels":a.channels,"sampleRate":a.sample_rate,"codec":a.codec,"bitDepth":a.bits_per_sample
                }))
                .collect::<Vec<_>>())
            .unwrap_or_default()
    )
}

fn set_source(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.source.set";
    let p: SourcePatch = parse(p, id)?;
    if p.stream.is_none() && p.channels.is_none() {
        return Err(bad(id, "provide stream or channels"));
    }
    let ids = targets(s, p.clip, p.clips.as_deref(), id, false)?;
    let originals = clips(s, &ids, id)?;
    let mut changes = Vec::new();
    for it in originals {
        let (_, media, _) = s.project.resolve_media(it.item).ok_or_else(|| bad(id, "input routing requires a source media clip"))?;
        let stream = p.stream.unwrap_or(it.audio_stream);
        let info = media.info.audio_streams.get(stream).ok_or_else(|| bad(id, "the selected source audio stream does not exist"))?;
        let channels = p.channels.clone().unwrap_or_else(|| if stream != it.audio_stream { Vec::new() } else { it.source_channels.clone() });
        if !matches!(channels.len(), 0 | 1 | 2 | 6) || channels.iter().any(|c| u32::from(*c) >= info.channels) {
            return Err(bad(
                id,
                "channels must be [] (default), one mono channel, two stereo channels, or six surround channels, all present in the selected stream",
            ));
        }
        if stream != it.audio_stream || channels != it.source_channels {
            changes.push((it.id, stream, channels));
        }
    }
    if !changes.is_empty() {
        let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
        s.edit("Choose Audio Source Channels", |project, _| {
            let seq = project.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
            for (clip, stream, channels) in &changes {
                let (_, it) = seq.find_item_mut(*clip).ok_or_else(|| bad(id, "target clip disappeared"))?;
                it.audio_stream = *stream;
                it.source_channels = channels.clone();
            }
            Ok(())
        })?;
    }
    Ok(json!({"changed":changes.len(),"revision":s.revision,"otherStreamsPreserved":true}))
}

fn configure_streams(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.streams.configure";
    let p: Streams = parse(p, id)?;
    if p.clips.len() > MAX_CLIPS || p.clips.is_empty() {
        return Err(bad(id, "provide1..256 clip IDs"));
    }
    let selected = targets(s, None, Some(&p.clips), id, false)?;
    let seq = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let links: std::collections::BTreeSet<u64> = selected.iter().filter_map(|id| seq.find_item(*id).and_then(|(_, it)| it.link)).collect();
    let ids: Vec<ClipId> = seq
        .audio_tracks
        .iter()
        .flat_map(|t| &t.items)
        .filter(|it| selected.contains(&it.id) || it.link.is_some_and(|link| links.contains(&link)))
        .map(|it| it.id)
        .collect();
    if ids.len() > MAX_CLIPS {
        return Err(bad(id, "linked stream expansion exceeds256 clips"));
    }
    let originals = clips(s, &ids, id)?;
    let mut changes = Vec::new();
    let mut report = Vec::new();
    for it in &originals {
        let (_, media, _) = s.project.resolve_media(it.item).ok_or_else(|| bad(id, "stream configuration needs media clips"))?;
        if media.info.audio_streams.len() < 2 {
            continue;
        }
        if media.info.audio_streams.get(p.main_stream).is_none() {
            return Err(bad(id, "requested main audio stream does not exist"));
        }
        if !originals.iter().any(|candidate| {
            candidate.item == it.item
                && candidate.audio_stream == p.main_stream
                && match it.link {
                    Some(link) => candidate.link == Some(link),
                    None => candidate.id == it.id,
                }
        }) {
            return Err(bad(id, "the main stream has no linked timeline clip; place all source streams first"));
        }
        let main = it.audio_stream == p.main_stream;
        let enabled = if main {
            true
        } else if p.mute_secondary {
            false
        } else {
            it.enabled
        };
        report.push(json!({"clip":it.id.0,"stream":it.audio_stream,"role":if main{"main"}else{"camera/backup"},"enabled":enabled}));
        if enabled != it.enabled {
            changes.push((it.id, enabled));
        }
    }
    if !changes.is_empty() {
        let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
        s.edit("Choose Main and Camera Audio Streams", |project, _| {
            let seq = project.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
            for (clip, enabled) in &changes {
                let (_, it) = seq.find_item_mut(*clip).ok_or_else(|| bad(id, "target clip disappeared"))?;
                it.enabled = *enabled;
            }
            Ok(())
        })?;
    }
    Ok(json!({"changed":changes.len(),"streams":report,"mainStream":p.main_stream,"videoUnchanged":true,"originalPreserved":true,
        "warning":"Stream roles are chosen by the caller; camera/microphone hardware is not inferred from a filename."}))
}

fn mode_index(mode: &str, id: &str) -> Result<usize> {
    MODES.iter().position(|m| *m == mode).ok_or_else(|| bad(id, "unknown channel mode"))
}

fn front_router(it: &mut TrackItem) {
    if let Some(at) = it.effects.iter().position(|e| e.effect == ROUTER) {
        let effect = it.effects.remove(at);
        it.effects.insert(0, effect);
    }
}

fn set(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.channels.set";
    let p: Patch = parse(p, id)?;
    let ids = targets(s, p.clip, p.clips.as_deref(), id, false)?;
    let originals = clips(s, &ids, id)?;
    let auto = p.mode.as_deref() == Some("auto");
    if auto && ids.len() > MAX_ANALYZED_CLIPS {
        return Err(bad(id, "auto routing analyzes at most16 clips per request"));
    }
    if p.mode.is_none() && p.left_gain_db.is_none() && p.right_gain_db.is_none() && p.invert_left.is_none() && p.invert_right.is_none() {
        return Err(bad(id, "provide a routing mode, gain or polarity change"));
    }
    let mut changes = Vec::new();
    let mut reports = Vec::new();
    for it in originals {
        let mut patch: Vec<(&str, ParamValue)> = Vec::new();
        let mut chosen = p.mode.clone();
        if auto {
            let duration = it.end().0.checked_sub(it.start.0).ok_or_else(|| bad(id, "invalid clip duration"))?;
            if duration > Tick::from_units(300, 1).0 {
                reports.push(json!({"clip":it.id.0,"skipped":"automatic repair requires a complete analysis; analyze longer recordings in explicit windows and choose a channel manually"}));
                chosen = None;
            } else {
                let report =
                    s.execute("audio.inspect", json!({"subject":{"target":"clip","clip":it.id.0,"effects":"pre"},"start":it.start.0,"end":it.end().0}))?;
                chosen = report["recommendation"]["mode"].as_str().filter(|m| matches!(*m, "left" | "right")).map(str::to_string);
                if report["coverage"]["entireSubject"] != true {
                    chosen = None;
                }
                reports.push(json!({"clip":it.id.0,"recommendation":report["recommendation"],"coverage":report["coverage"],"appliedMode":chosen}));
            }
        }
        if let Some(mode) = chosen.as_deref() {
            let mapped_mono = it.source_channels.first().is_some_and(|first| it.source_channels.iter().all(|channel| channel == first));
            if matches!(mode, "right" | "swap" | "midSideDecode" | "midSideEncode" | "side") && (source_channels(s, &it) == Some(1) || mapped_mono) {
                return Err(bad(id, "this is a mono source and already plays on both sides; it has no independent right channel"));
            }
            patch.push(("mode", ParamValue::Choice(mode_index(mode, id)? as u32)));
        }
        for (name, value) in [("gain_l_db", p.left_gain_db), ("gain_r_db", p.right_gain_db)] {
            if let Some(value) = value {
                patch.push((name, ParamValue::Float(finite(Some(value), 0.0, -60.0, 24.0, name, id)?)));
            }
        }
        for (name, value) in [("invert_l", p.invert_left), ("invert_r", p.invert_right)] {
            if let Some(value) = value {
                patch.push((name, ParamValue::Bool(value)));
            }
        }
        if patch.is_empty() {
            continue;
        }
        let previous = router(&it, id)?;
        let mut next = previous
            .cloned()
            .or_else(|| filmcraft_project::find_effect(ROUTER).map(|d| d.instance()))
            .ok_or_else(|| bad(id, "channel routing effect unavailable"))?;
        next.enabled = true;
        for (key, value) in patch {
            let control = next.params.get_mut(key).ok_or_else(|| bad(id, format!("missing routing control {key}")))?;
            if control.value == value && !control.is_animated() {
                continue;
            }
            if control.is_animated() && !p.replace_animation {
                return Err(bad(id, format!("{key} is animated; explicitly set replaceAnimation:true to replace it")));
            }
            control.value = value;
            control.keyframes.clear();
        }
        if previous != Some(&next) {
            changes.push((it.id, next));
        }
    }
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let count = changes.len();
    if !changes.is_empty() {
        s.edit("Repair Audio Channels", |project, _| {
            let seq = project.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
            for (clip, next) in &changes {
                let (_, it) = seq.find_item_mut(*clip).ok_or_else(|| bad(id, "target clip disappeared"))?;
                if let Some(slot) = it.effects.iter_mut().find(|e| e.effect == ROUTER) {
                    *slot = next.clone();
                } else {
                    it.effects.insert(0, next.clone());
                }
            }
            Ok(())
        })?;
    }
    Ok(json!({"changed":count,"clips":ids.iter().map(|c|c.0).collect::<Vec<_>>(),"analysis":reports,"revision":s.revision,"originalPreserved":true}))
}

fn reset(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.channels.reset";
    let p: Reset = parse(p, id)?;
    let ids = targets(s, p.clip, p.clips.as_deref(), id, false)?;
    let originals = clips(s, &ids, id)?;
    for it in &originals {
        if router(it, id)?.is_some_and(EffectInstance::is_animated) && !p.replace_animation {
            return Err(bad(id, "router is animated; explicitly set replaceAnimation:true to remove it"));
        }
    }
    let changed = originals.iter().filter(|it| it.effects.iter().any(|e| e.effect == ROUTER)).count();
    if changed > 0 {
        let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
        s.edit("Reset Audio Channels", |project, _| {
            let seq = project.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
            for it in seq.audio_tracks.iter_mut().flat_map(|t| &mut t.items).filter(|it| ids.contains(&it.id)) {
                it.effects.retain(|e| e.effect != ROUTER);
            }
            Ok(())
        })?;
    }
    Ok(json!({"changed":changed,"revision":s.revision,"otherEffectsPreserved":true}))
}

fn settings(name: &str, old: Option<&EssentialSound>, id: &str) -> Result<Option<EssentialSound>> {
    if name == "reset" {
        return Ok(None);
    }
    let kind = if name == "musicGentle" { AudioType::Music } else { AudioType::Dialogue };
    let mut next = EssentialSound::new(kind);
    if let Some(old) = old {
        next.loudness = old.loudness.clone();
        next.volume = old.volume.clone();
        next.pan = old.pan.clone();
        next.mute = old.mute;
    }
    let kv = match name {
        "dialogueClean" => vec![
            ("repair.noise.on", json!(true)),
            ("repair.noise.amount", json!(2.0)),
            ("repair.rumble.on", json!(true)),
            ("repair.rumble.amount", json!(2.0)),
            ("repair.deess.on", json!(true)),
            ("repair.deess.amount", json!(2.0)),
            ("clarity.dynamics.on", json!(true)),
            ("clarity.dynamics.amount", json!(3.0)),
        ],
        "dialogueWarm" => vec![
            ("repair.rumble.on", json!(true)),
            ("repair.rumble.amount", json!(1.5)),
            ("repair.deess.on", json!(true)),
            ("repair.deess.amount", json!(2.0)),
            ("clarity.enhance.on", json!(true)),
            ("clarity.enhance.amount", json!(3.0)),
            ("clarity.enhanceTone", json!(0)),
            ("clarity.dynamics.on", json!(true)),
            ("clarity.dynamics.amount", json!(2.0)),
        ],
        "podcast" => vec![
            ("repair.noise.on", json!(true)),
            ("repair.noise.amount", json!(2.0)),
            ("repair.rumble.on", json!(true)),
            ("repair.rumble.amount", json!(3.0)),
            ("repair.deess.on", json!(true)),
            ("repair.deess.amount", json!(3.0)),
            ("clarity.dynamics.on", json!(true)),
            ("clarity.dynamics.amount", json!(4.0)),
            ("clarity.enhance.on", json!(true)),
            ("clarity.enhance.amount", json!(2.0)),
        ],
        "musicGentle" => Vec::new(),
        _ => return Err(bad(id, "preset must be dialogueClean, dialogueWarm, podcast, musicGentle or reset")),
    };
    for (key, value) in kv {
        next.set(key, &value).map_err(|e| bad(id, e))?;
    }
    next.preset = name.into();
    Ok(Some(next))
}

fn preset(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.preset";
    let p: Preset = parse(p, id)?;
    let ids = targets(s, p.clip, p.clips.as_deref(), id, false)?;
    let originals = clips(s, &ids, id)?;
    let mut changes = Vec::new();
    for it in originals {
        let next = settings(&p.preset, it.essential.as_ref(), id)?;
        if it.effects.iter().any(|e| e.essential && e.is_animated()) && !p.replace_animation {
            return Err(bad(id, "managed voice controls are animated; explicitly set replaceAnimation:true to replace them"));
        }
        changes.push((it.id, next));
    }
    let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let ph = s.playhead();
    s.edit("Apply Audio Toolkit Preset", |project, _| {
        let seq = project.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
        for (clip, next) in &changes {
            let (_, it) = seq.find_item_mut(*clip).ok_or_else(|| bad(id, "target clip disappeared"))?;
            let old = it.essential.clone();
            if p.replace_animation {
                for e in it.effects.iter_mut().filter(|e| e.essential) {
                    for param in e.params.values_mut() {
                        param.keyframes.clear();
                    }
                }
            }
            it.essential = next.clone();
            let mt = it.source_time_at(ph);
            es::apply(it, old.as_ref(), mt);
            front_router(it);
        }
        Ok(())
    })?;
    Ok(json!({"clips":ids.iter().map(|c|c.0).collect::<Vec<_>>(),"preset":p.preset,"revision":s.revision,"normalizationApplied":false}))
}

fn normalize(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.normalize";
    let p: Normalize = parse(p, id)?;
    let target = finite(p.target_lufs, -16.0, -60.0, 0.0, "targetLufs", id)?;
    let ceiling = finite(p.true_peak_dbtp, -1.0, -12.0, 0.0, "truePeakDbtp", id)?;
    let max_gain = finite(p.max_gain_db, 12.0, 0.0, 24.0, "maxGainDb", id)?;
    let ids = targets(s, p.clip, p.clips.as_deref(), id, false)?;
    if ids.len() > MAX_ANALYZED_CLIPS {
        return Err(bad(id, "normalize at most16 clips per request"));
    }
    let originals = clips(s, &ids, id)?;
    let mut changes = Vec::new();
    let mut reports = Vec::new();
    for it in &originals {
        if it.effect("volume").is_some_and(|effect| !effect.enabled || effect.param("bypass").and_then(|p| p.value.as_bool()).unwrap_or(false)) {
            return Err(bad(id, "Volume is disabled or bypassed; enable the output Volume control before normalization"));
        }
    }
    for it in originals {
        let duration = it.end().0.checked_sub(it.start.0).ok_or_else(|| bad(id, "invalid clip duration"))?;
        if duration > Tick::from_units(300, 1).0 {
            return Err(bad(id, "normalization requires full-clip analysis up to300s; split longer recordings into deliberate ranges"));
        }
        let report = s.execute("audio.inspect", json!({"subject":{"target":"clip","clip":it.id.0,"effects":"post"},"start":it.start.0,"end":it.end().0}))?;
        if report["coverage"]["entireSubject"] != true {
            return Err(bad(id, "full clip measurement was not available"));
        }
        let (Some(lufs), Some(peak)) = (report["loudness"]["integratedLufs"].as_f64(), report["loudness"]["truePeakDbtp"].as_f64()) else {
            reports.push(json!({"clip":it.id.0,"skipped":"no gated loudness measurement (silent or shorter than the400ms measurement window)"}));
            continue;
        };
        if !lufs.is_finite() || !peak.is_finite() {
            return Err(bad(id, "non-finite loudness measurement"));
        }
        // Volume is after the clip's nonlinear effects. Offset its entire automation shape
        // while preserving the editor's separate clip-gain setting and other channel controls.
        let volume = it.effect("volume").and_then(|e| e.param("level"));
        let values: Vec<f64> = if let Some(volume) = volume {
            std::iter::once(&volume.value)
                .chain(volume.keyframes.iter().map(|k| &k.value))
                .map(|v| v.as_f64().filter(|v| v.is_finite()).ok_or_else(|| bad(id, "invalid animated Volume control")))
                .collect::<Result<_>>()?
        } else {
            vec![0.0]
        };
        let highest = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let lowest = values.iter().copied().fold(f64::INFINITY, f64::min);
        let requested = target - lufs;
        let gain = requested.min(max_gain).min(ceiling - peak).min(15.0 - highest);
        if !gain.is_finite() || lowest + gain < -287.5 {
            return Err(bad(id, "required output gain is outside the Volume control range"));
        }
        reports.push(json!({"clip":it.id.0,"measuredLufs":lufs,"measuredTruePeakDbtp":peak,"gainDb":gain,"targetLufs":target,
            "predictedLufs":lufs+gain,"predictedTruePeakDbtp":peak+gain,"peakLimited":gain+1e-6<requested,
            "measurement":"full isolated clip after clip controls; track/master effects and overlapping clips are excluded"}));
        if gain.abs() > 1e-9 {
            changes.push((it.id, gain));
        }
    }
    if !changes.is_empty() {
        let seq_id = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
        s.edit("Normalize Audio with Peak Headroom", |project, _| {
            let seq = project.sequence_mut(seq_id).ok_or(EngineError::NoSequence)?;
            for (clip, gain) in &changes {
                let (_, it) = seq.find_item_mut(*clip).ok_or_else(|| bad(id, "target clip disappeared"))?;
                if it.effect("volume").is_none() {
                    it.effects.push(filmcraft_project::find_effect("volume").ok_or_else(|| bad(id, "Volume effect unavailable"))?.instance());
                }
                let level = it.effect_mut("volume").and_then(|e| e.param_mut("level")).ok_or_else(|| bad(id, "Volume control unavailable"))?;
                level.map_values(|v| ParamValue::Float(v.as_f64().unwrap_or(0.0) + gain));
            }
            Ok(())
        })?;
    }
    Ok(json!({"changed":changes.len(),"clips":reports,"revision":s.revision,"truePeakCeilingDbtp":ceiling,
        "warning":"Recheck the final sequence mix: overlapping clips and track/master processing can change delivery peaks."}))
}

fn prepare(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.prepare";
    let p: Prepare = parse(p, id)?;
    if p.base_revision.is_some_and(|r| r != s.revision) {
        return Err(bad(id, "stale revision; inspect the current project"));
    }
    let ids = targets(s, p.clip, p.clips.as_deref(), id, false)?;
    clips(s, &ids, id)?;
    let label = p.label.as_deref().unwrap_or("Prepare Audio Repair");
    if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
        return Err(bad(id, "label must be a short nonempty single line"));
    }
    let ids: Vec<u64> = ids.iter().map(|c| c.0).collect();
    let mut steps = Vec::new();
    steps.push(json!({"id":"audio.channels.set","params":{"clips":ids,"mode":p.routing.as_deref().unwrap_or("auto"),"replaceAnimation":p.replace_animation}}));
    if let Some(name) = p.preset {
        steps.push(json!({"id":"audio.preset","params":{"clips":ids,"preset":name,"replaceAnimation":p.replace_animation}}));
    }
    if p.normalize {
        steps.push(json!({"id":"audio.normalize","params":{"clips":ids,"targetLufs":p.target_lufs,"maxGainDb":p.max_gain_db,"truePeakDbtp":p.true_peak_dbtp}}));
    }
    let report = s.execute("agent.plan", json!({"label":label,"baseRevision":s.revision,"steps":steps}))?;
    let plan = report["plan"].as_u64().ok_or_else(|| bad(id, "plan identity unavailable"))?;
    s.agent_annotate_plan(
        plan,
        &json!({"preparation":{"kind":"audio.repair","clips":ids,"originalPreserved":true,"normalization":p.normalize,
        "workflow":"Preview and audition before/after; approval applies one undo step."}}),
    )?;
    s.execute("agent.preview", json!({"plan":plan}))
}

pub fn commands() -> Vec<CommandSpec> {
    let make = |id, label, params, query: bool, run: fn(&mut Session, &Value) -> Result<Value>| CommandSpec {
        id,
        label,
        menu: &[],
        shortcut: None,
        params,
        enabled: always,
        journal: !query,
        run,
    };
    vec![
        make("audio.channels.get", "Inspect Audio Channel Routing", r#"{"clips":[id]?,"clip":id?}"#, true, get),
        make(
            "audio.source.set",
            "Choose Audio Source Stream and Channels",
            r#"{"clips":[id]?,"clip":id?,"stream":index?,"channels":[index]?}"#,
            false,
            set_source,
        ),
        make(
            "audio.streams.configure",
            "Choose Main and Camera Audio Streams",
            r#"{"clips":[id],"mainStream":index,"muteSecondary":bool=false}"#,
            false,
            configure_streams,
        ),
        make(
            "audio.channels.set",
            "Repair Audio Channels",
            r#"{"clips":[id]?,"clip":id?,"mode":"stereo|left|right|mono|swap|midSideEncode|midSideDecode|side|auto"?,"leftGainDb":dB?,"rightGainDb":dB?,"invertLeft":bool?,"invertRight":bool?,"replaceAnimation":bool=false}"#,
            false,
            set,
        ),
        make("audio.channels.reset", "Reset Audio Channel Repair", r#"{"clips":[id]?,"clip":id?,"replaceAnimation":bool=false}"#, false, reset),
        make(
            "audio.preset",
            "Apply Audio Toolkit Preset",
            r#"{"clips":[id]?,"clip":id?,"preset":"dialogueClean|dialogueWarm|podcast|musicGentle|reset","replaceAnimation":bool=false}"#,
            false,
            preset,
        ),
        make(
            "audio.normalize",
            "Normalize Audio with Peak Headroom",
            r#"{"clips":[id]?,"clip":id?,"targetLufs":LUFS=-16,"maxGainDb":dB=12,"truePeakDbtp":dBTP=-1}"#,
            false,
            normalize,
        ),
        make(
            "audio.prepare",
            "Prepare Reviewable Audio Repair",
            r#"{"clips":[id]?,"clip":id?,"routing":"auto|stereo|left|right|mono|swap|midSideEncode|midSideDecode|side"?,"preset":str?,"normalize":bool=false,"targetLufs":LUFS?,"maxGainDb":dB?,"truePeakDbtp":dBTP?,"replaceAnimation":bool=false,"baseRevision":revision?,"label":str?}"#,
            false,
            prepare,
        ),
    ]
}

#[cfg(test)]
mod tests;
