//! Explicit microphone/backup/music roles and targeted, reviewable music ducking.
use crate::{
    EngineError, Result, Session,
    commands::{CommandSpec, always, bad},
};
use filmcraft_project::{ClipId, ParamValue, Track, TrackId, TrackItem, TrackKind};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
const RATE: u32 = 48_000;
const HOP: usize = 480;
fn yes() -> bool {
    true
}
fn tone() -> String {
    "neutral".into()
}
fn intensity() -> f64 {
    0.35
}
fn duck_db() -> f64 {
    -12.0
}
fn fade() -> f64 {
    0.4
}
fn ceiling() -> f64 {
    -1.5
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    main: Vec<u64>,
    #[serde(default)]
    backup: Vec<u64>,
    #[serde(default)]
    music: Vec<u64>,
    #[serde(default = "tone")]
    tone: String,
    #[serde(default = "intensity")]
    intensity: f64,
    #[serde(default = "yes")]
    repair_channels: bool,
    #[serde(default = "yes")]
    normalize: bool,
    target_lufs: Option<f64>,
    max_gain_db: Option<f64>,
    true_peak_dbtp: Option<f64>,
    #[serde(default = "yes")]
    duck_music: bool,
    #[serde(default = "duck_db")]
    duck_db: f64,
    #[serde(default = "fade")]
    duck_fade_seconds: f64,
    #[serde(default)]
    replace_animation: bool,
    base_revision: Option<u64>,
    label: Option<String>,
    #[serde(default = "yes")]
    final_limiter: bool,
    #[serde(default = "ceiling")]
    limiter_ceiling_dbfs: f64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Roles {
    main: Vec<u64>,
    #[serde(default)]
    backup: Vec<u64>,
    #[serde(default)]
    music: Vec<u64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Duck {
    main: Vec<u64>,
    music: Vec<u64>,
    #[serde(default = "duck_db")]
    duck_db: f64,
    #[serde(default = "fade")]
    duck_fade_seconds: f64,
    #[serde(default)]
    replace_animation: bool,
}
fn parse<T: for<'de> Deserialize<'de>>(p: &Value, id: &str) -> Result<T> {
    crate::analysis_support::bounded_decode(p, id)
}
fn number(value: f64, min: f64, max: f64, name: &str, id: &str) -> Result<()> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(bad(id, format!("{name} must be finite in {min}..={max}")));
    }
    Ok(())
}
fn check(s: &Session) -> Result<()> {
    if filmcraft_media::cancel::cancelled() {
        return Err(bad("audio.mix", "cancelled; live project unchanged"));
    }
    crate::workflow::check_cancelled(s)
}
fn selected(s: &Session, main: &[u64], backup: &[u64], music: &[u64], id: &str, unlocked: bool) -> Result<Vec<(String, TrackItem)>> {
    if main.is_empty() || main.len() > 16 || backup.len() > 32 || music.len() > 16 {
        return Err(bad(id, "provide 1..16 main, 0..32 backup and 0..16 music audio IDs"));
    }
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (role, ids) in [("main", main), ("backup", backup), ("music", music)] {
        for id_value in ids {
            if !seen.insert(*id_value) {
                return Err(bad(id, "main, backup and music IDs must be unique and disjoint"));
            }
            let (track, it) = q.find_item(ClipId(*id_value)).ok_or_else(|| bad(id, "a role clip is not in the active sequence"))?;
            let track = q
                .audio_tracks
                .iter()
                .find(|t| t.id == track)
                .ok_or_else(|| bad(id, "mix roles require explicit AUDIO clip IDs; video is never implicitly changed"))?;
            if unlocked && track.locked {
                return Err(bad(id, format!("{} is locked", track.name)));
            }
            if unlocked
                && role != "backup"
                && let Some(reason) = route_warning(q, track)
            {
                return Err(bad(id, reason));
            }
            out.push((role.to_string(), it.clone()));
        }
    }
    Ok(out)
}
fn request(s: &Session, p: &Value, id: &str, unlocked: bool) -> Result<(Request, Vec<(String, TrackItem)>)> {
    let p: Request = parse(p, id)?;
    number(p.intensity, 0.0, 1.0, "intensity", id)?;
    number(p.duck_db, -40.0, 0.0, "duckDb", id)?;
    number(p.duck_fade_seconds, 0.0, 5.0, "duckFadeSeconds", id)?;
    number(p.limiter_ceiling_dbfs, -12.0, 0.0, "limiterCeilingDbfs", id)?;
    if let Some(v) = p.target_lufs {
        number(v, -60.0, 0.0, "targetLufs", id)?;
    }
    if let Some(v) = p.max_gain_db {
        number(v, 0.0, 24.0, "maxGainDb", id)?;
    }
    if let Some(v) = p.true_peak_dbtp {
        number(v, -12.0, 0.0, "truePeakDbtp", id)?;
    }
    if !["neutral", "warm", "bright"].contains(&p.tone.as_str()) {
        return Err(bad(id, "tone must be neutral, warm or bright"));
    }
    if p.base_revision.is_some_and(|r| r != s.revision) {
        return Err(bad(id, "stale revision; inspect again"));
    }
    if p.label.as_ref().is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control)) {
        return Err(bad(id, "label must be a short nonempty single line"));
    }
    let clips = selected(s, &p.main, &p.backup, &p.music, id, unlocked)?;
    Ok((p, clips))
}
/// Conservative audibility preflight: never change shared mixer routing to make a role audible.
fn route_warning(q: &filmcraft_project::Sequence, track: &Track) -> Option<String> {
    let tracks = q.audio_tracks.iter().chain(q.submix_tracks.iter()).collect::<Vec<_>>();
    let any_solo = tracks.iter().any(|t| t.solo);
    let mut solo_kept = false;
    let mut seen = BTreeSet::new();
    let mut current = Some(track);
    while let Some(t) = current {
        if seen.len() >= 64 || !seen.insert(t.id) {
            return Some("audio output route is cyclic or exceeds 64 strips; review mixer routing first".into());
        }
        if !t.enabled || t.muted {
            return Some(format!("{} is disabled or muted; unmute/enable its mixer track explicitly first", t.name));
        }
        if t.mixer.mode.reads()
            && t.mixer
                .lanes
                .get("mute")
                .is_some_and(|p| p.is_animated() && p.keyframes.iter().any(|k| k.value.as_f64().is_none_or(|v| !v.is_finite() || v >= 0.5)))
        {
            return Some(format!("{} has mute automation; review that shared track before preparing the mix", t.name));
        }
        solo_kept |= t.solo || t.mixer.solo_safe;
        current = match t.mixer.output {
            Some(id) => match tracks.iter().copied().find(|t| t.id == id) {
                Some(t) => Some(t),
                None => return Some("audio output route is missing; review mixer routing first".into()),
            },
            None => None,
        };
    }
    if any_solo && !solo_kept {
        return Some(format!("{} is excluded from the direct solo route; solo this track or review mixer solo/send routing first", track.name));
    }
    None
}
fn summary(s: &Session, clips: &[(String, TrackItem)]) -> Value {
    let q = s.active_sequence();
    json!(clips.iter().map(|(role,old)|{
        let live=q.and_then(|q|q.find_item(old.id));let it=live.map(|(_,it)|it).unwrap_or(old);
        let track=live.and_then(|(id,_)|q.and_then(|q|q.audio_tracks.iter().find(|t|t.id==id)));
        json!({"clip":it.id.0,"name":it.name,"role":role,"stream":it.audio_stream,"sourceChannels":it.source_channels,"enabled":it.enabled,"track":track.map(|t|&t.name),"locked":track.is_some_and(|t|t.locked),"audibilityWarning":q.and_then(|q|track.and_then(|t|route_warning(q,t))),"start":it.start.0,"end":it.end().0})
    }).collect::<Vec<_>>())
}
fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let (p, clips) = request(s, p, "audio.mix.inspect", false)?;
    let mut measurements = Vec::new();
    for (role, it) in &clips {
        check(s)?;
        if role == "backup" {
            continue;
        }
        let end = it.end().min(Tick(it.start.0.saturating_add(TICKS_PER_SECOND * 300)));
        let report = s.execute("audio.inspect", json!({"subject":{"target":"clip","clip":it.id.0,"effects":"post"},"start":it.start.0,"end":end.0}))?;
        measurements.push(json!({"clip":it.id.0,"role":role,"loudness":report["loudness"],"recommendation":report["recommendation"],"coverage":report["coverage"],"sourceFormat":report["sourceFormat"]}));
    }
    Ok(json!({"revision":s.revision,"roles":summary(s,&clips),"measurements":measurements,"duckDetector":p.main,"projectChanged":false,
        "limitations":["Roles are explicit caller choices; hardware is not inferred.","Only specified backups will be muted. Other enabled audio stays unchanged.","Individual clip ceilings do not certify summed mix true peaks; audition the sequence and verify delivery.","Voice polish is a fixed gentle starting point; quiet input may not drive fixed compression thresholds.","Ducking measures isolated main clip output before shared track/master processing; review sequence A/B and final delivery."]}))
}
fn roles(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.mix.roles";
    let p: Roles = parse(p, id)?;
    let clips = selected(s, &p.main, &p.backup, &p.music, id, true)?;
    let seq = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
    let changes = clips
        .iter()
        .filter_map(|(role, it)| {
            let enabled = role != "backup";
            if it.enabled == enabled && it.essential.as_ref().is_none_or(|e| e.mute == !enabled) { None } else { Some((it.id, enabled)) }
        })
        .collect::<Vec<_>>();
    if !changes.is_empty() {
        s.edit("Set Explicit Audio Mix Roles", |project, _| {
            let q = project.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
            for (clip, enabled) in &changes {
                let (_, it) = q.find_item_mut(*clip).ok_or_else(|| bad(id, "role clip disappeared"))?;
                it.enabled = *enabled;
                if let Some(e) = it.essential.as_mut() {
                    e.mute = !*enabled;
                }
            }
            Ok(())
        })?;
    }
    Ok(json!({"changed":changes.len(),"roles":summary(s,&clips),"onlyExplicitIdsChanged":true,"mainPositionsChanged":false,"videoChanged":false}))
}
/// Sample-grid activity in just one explicit main, excluding every unrelated Dialogue clip.
fn activity(s: &Session, it: &TrackItem) -> Result<Vec<(Tick, Tick)>> {
    let id = "audio.mix.duck";
    let duration = it.end().0.checked_sub(it.start.0).ok_or_else(|| bad(id, "clip duration overflows"))?;
    if duration <= 0 || duration > TICKS_PER_SECOND * 300 {
        return Err(bad(id, "duck detector requires complete main clips up to 300s; trim deliberate ranges first"));
    }
    let args: crate::analysis_support::SubjectArgs =
        serde_json::from_value(json!({"target":"clip","clip":it.id.0,"effects":"post"})).map_err(|e| bad(id, e.to_string()))?;
    let target = crate::analysis_support::Target::resolve(s, &args)?;
    let provider = target.audio_reader(s);
    let mut track = Track::new(TrackId(0), TrackKind::Audio, "Explicit main detector".into());
    track.items.push(it.clone());
    let mut at = it.start.to_units_floor(i64::from(RATE));
    let first = at;
    let end = it.end().to_units_floor(i64::from(RATE));
    let mut power = Vec::new();
    while at < end {
        check(s)?;
        let n = usize::try_from((end - at).min(i64::from(RATE))).map_err(|e| bad(id, e.to_string()))?;
        let data = filmcraft_render::audio::track_input(&s.project, &track, at, n, RATE, &provider);
        provider.check()?;
        if data.channels.len() != 2 || data.channels.iter().any(|c| c.len() != n || c.iter().any(|v| !v.is_finite())) {
            return Err(bad(id, "detector audio contains invalid samples"));
        }
        for offset in (0..n).step_by(HOP) {
            let stop = offset.saturating_add(HOP).min(n);
            let mut sum = 0.0;
            for channel in &data.channels {
                for value in channel.get(offset..stop).ok_or_else(|| bad(id, "detector shape mismatch"))? {
                    sum += f64::from(*value).powi(2);
                }
            }
            power.push(sum / ((stop - offset) * 2) as f64);
        }
        at = at.checked_add(n as i64).ok_or_else(|| bad(id, "sample position overflows"))?;
    }
    let mut raw = Vec::new();
    let mut start = None;
    for index in 0..power.len() {
        let lo = index.saturating_sub(2);
        let hi = index.saturating_add(3).min(power.len());
        let mean = power.get(lo..hi).ok_or_else(|| bad(id, "detector window mismatch"))?.iter().sum::<f64>() / (hi - lo) as f64;
        if mean > 0.0001 {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(begin) = start.take() {
            raw.push((begin, index));
        }
    }
    if let Some(begin) = start {
        raw.push((begin, power.len()));
    }
    let mut bridged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in raw {
        if let Some(last) = bridged.last_mut().filter(|last| a.saturating_sub(last.1) <= 25) {
            last.1 = b;
        } else {
            bridged.push((a, b));
        }
    }
    let mut out = Vec::new();
    for (a, b) in bridged.into_iter().filter(|(a, b)| b.saturating_sub(*a) >= 10) {
        let begin = Tick::from_units(first.saturating_add(a.saturating_mul(HOP) as i64), i64::from(RATE)).max(it.start);
        let end = Tick::from_units(first.saturating_add(b.saturating_mul(HOP) as i64), i64::from(RATE)).min(it.end());
        if end > begin {
            out.push((begin, end));
        }
    }
    Ok(out)
}
fn curve(it: &TrackItem, regions: &[(Tick, Tick)], base: f64, db: f64, fade: Tick) -> Result<Vec<(Tick, f64)>> {
    let id = "audio.mix.duck";
    let mut regions =
        regions.iter().copied().filter(|(a, b)| *b > it.start && *a < it.end()).map(|(a, b)| (a.max(it.start), b.min(it.end()))).collect::<Vec<_>>();
    regions.sort_by_key(|r| r.0);
    let mut merged: Vec<(Tick, Tick)> = Vec::new();
    for (a, b) in regions {
        if let Some(last) = merged.last_mut().filter(|last| a.0.saturating_sub(fade.0) <= last.1.0.saturating_add(fade.0)) {
            last.1 = last.1.max(b);
        } else {
            merged.push((a, b));
        }
    }
    let mut points = Vec::new();
    for (a, b) in merged {
        points.push((Tick(a.0.saturating_sub(fade.0)).max(it.start), base));
        points.push((a, base + db));
        points.push((b, base + db));
        if b < it.end() {
            points.push((Tick(b.0.saturating_add(fade.0)).min(it.end()), base));
        }
    }
    if points.len() > 4096 {
        return Err(bad(id, "ducking exceeds 4096 curve points for one clip; choose a shorter mix range"));
    }
    points.sort_by_key(|p| p.0);
    let mut out: Vec<(Tick, f64)> = Vec::new();
    for point in points {
        if let Some(last) = out.last_mut().filter(|p| p.0 == point.0) {
            last.1 = last.1.min(point.1);
        } else {
            out.push(point);
        }
    }
    Ok(out)
}
fn duck(s: &mut Session, p: &Value) -> Result<Value> {
    let id = "audio.mix.duck";
    let p: Duck = parse(p, id)?;
    number(p.duck_db, -40.0, 0.0, "duckDb", id)?;
    number(p.duck_fade_seconds, 0.0, 5.0, "duckFadeSeconds", id)?;
    let clips = selected(s, &p.main, &[], &p.music, id, true)?;
    let mut regions = Vec::new();
    for (role, it) in &clips {
        if role == "main" {
            if !it.enabled || it.essential.as_ref().is_some_and(|e| e.mute) {
                return Err(bad(id, "main clip is disabled or Essential Sound muted; explicitly enable it first"));
            }
            regions.extend(activity(s, it)?);
        }
    }
    let fade = Tick::from_seconds_f64(p.duck_fade_seconds).max(Tick(1));
    let mut changes = Vec::new();
    let mut reports = Vec::new();
    for (role, it) in &clips {
        if role != "music" {
            continue;
        }
        check(s)?;
        if it.frame_hold.is_some() || !it.speed.is_finite() || it.reverse || (it.speed - 1.0).abs() > 1e-9 || filmcraft_render::remix::is_remixed(it) {
            return Err(bad(id, "duck music at finite original speed without frame hold/reverse/remix; render deliberate timing first"));
        }
        let volume = it.effect("volume").ok_or_else(|| bad(id, "music needs an active Volume control"))?;
        if !volume.enabled || volume.param("bypass").and_then(|p| p.value.as_bool()).unwrap_or(false) {
            return Err(bad(id, "music Volume is disabled or bypassed"));
        }
        let level = volume.param("level").ok_or_else(|| bad(id, "music Volume level is unavailable"))?;
        let overlap = regions.iter().any(|(a, b)| *b > it.start && *a < it.end());
        if !overlap || p.duck_db == 0.0 {
            reports.push(json!({"clip":it.id.0,"changed":false,"reason":"no main activity overlaps this music clip"}));
            continue;
        }
        if level.is_animated() && !p.replace_animation {
            return Err(bad(id, "music Volume already has animation; set replaceAnimation:true to replace it or duckMusic:false"));
        }
        let values = std::iter::once(&level.value)
            .chain(level.keyframes.iter().map(|k| &k.value))
            .map(|v| v.as_f64().filter(|v| v.is_finite()).ok_or_else(|| bad(id, "music Volume contains invalid values")))
            .collect::<Result<Vec<_>>>()?;
        let base = values.into_iter().fold(f64::NEG_INFINITY, f64::max);
        if base + p.duck_db < -287.5 || base > 15.0 {
            return Err(bad(id, "ducking level exceeds Volume range"));
        }
        let points = curve(it, &regions, base, p.duck_db, fade)?;
        reports.push(json!({"clip":it.id.0,"detector":p.main,"baseDb":base,"duckDb":p.duck_db,"keyframes":points.iter().map(|(t,v)|json!({"timelineTime":t.0,"sourceTime":it.source_time_at(*t).0,"levelDb":v})).collect::<Vec<_>>(),"replacedAnimation":level.is_animated()}));
        changes.push((it.id, base, points));
    }
    if !changes.is_empty() {
        let seq = s.state.active_sequence.ok_or(EngineError::NoSequence)?;
        s.edit("Duck Music under Explicit Main Clips", |project, _| {
            let q = project.sequence_mut(seq).ok_or(EngineError::NoSequence)?;
            for (clip, base, points) in &changes {
                let (_, it) = q.find_item_mut(*clip).ok_or_else(|| bad(id, "music clip disappeared"))?;
                let mapped = points.iter().map(|(t, v)| (it.source_time_at(*t), *v)).collect::<Vec<_>>();
                let param = it.effect_mut("volume").and_then(|e| e.param_mut("level")).ok_or_else(|| bad(id, "music Volume disappeared"))?;
                param.keyframes.clear();
                param.value = ParamValue::Float(*base);
                for (time, value) in mapped {
                    param.put_keyframe(time, ParamValue::Float(value));
                }
            }
            Ok(())
        })?;
    }
    Ok(
        json!({"changed":changes.len(),"clips":reports,"detector":p.main,"method":"Only explicit main post-clip signal;10ms samples/50ms RMS,-40dBFS activity,100ms minimum/250ms bridge; not speech recognition","otherDialogueIgnored":true,"detectorScope":"isolated clip effects and clip output controls; track/master faders, inserts, sends and summing are excluded"}),
    )
}
fn prepare(s: &mut Session, p: &Value) -> Result<Value> {
    let (p, clips) = request(s, p, "audio.mix.prepare", true)?;
    let mut steps = vec![json!({"id":"audio.mix.roles","params":{"main":p.main,"backup":p.backup,"music":p.music}})];
    if p.repair_channels {
        steps.push(json!({"id":"audio.channels.set","params":{"clips":p.main,"mode":"auto","replaceAnimation":p.replace_animation}}));
    }
    steps.push(json!({"id":"audio.voice.polish","params":{"clips":p.main,"tone":p.tone,"intensity":p.intensity,"replaceAnimation":p.replace_animation}}));
    if p.normalize {
        for ids in [&p.main, &p.music].into_iter().filter(|ids| !ids.is_empty()) {
            steps.push(
                json!({"id":"audio.normalize","params":{"clips":ids,"targetLufs":p.target_lufs,"maxGainDb":p.max_gain_db,"truePeakDbtp":p.true_peak_dbtp}}),
            );
        }
    }
    if p.duck_music && !p.music.is_empty() {
        steps.push(json!({"id":"audio.mix.duck","params":{"main":p.main,"music":p.music,"duckDb":p.duck_db,"duckFadeSeconds":p.duck_fade_seconds,"replaceAnimation":p.replace_animation}}));
    }
    if p.final_limiter {
        steps.push(json!({"id":"audio.mix.limit","params":{"ceilingDbfs":p.limiter_ceiling_dbfs,"replaceAnimation":p.replace_animation}}));
    }
    for (index, step) in steps.iter_mut().enumerate() {
        if index > 0 {
            step["dependsOn"] = json!([index - 1]);
        }
    }
    let report = s.execute(
        "agent.plan",
        json!({"label":p.label.as_deref().unwrap_or("Mix main microphone, backup and music"),"baseRevision":s.revision,"steps":steps}),
    )?;
    let plan = report["plan"].as_u64().ok_or_else(|| bad("audio.mix.prepare", "missing plan identity"))?;
    s.agent_annotate_plan(plan,&json!({"preparation":{"kind":"audio.mix","roles":summary(s,&clips),"detector":p.main,"backupMutedOnlyExplicitIds":p.backup,"mainPositionsChanged":false,"musicNominalTargetLufs":p.target_lufs.unwrap_or(-16.0),"duckMusic":p.duck_music,"finalLimiter":p.final_limiter,"finalLimiterScope":"sample-peak safety; actual true peak and delivery verification remain required","voiceRecipeScope":"fixed gentle starting point; quiet input may not drive fixed compressor thresholds","originalFilesChanged":false}}))?;
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
        make("audio.mix.inspect", "Inspect Explicit Audio Mix Roles", r#"{"main":[audioId],"backup":[audioId]?,"music":[audioId]?}"#, true, inspect),
        make(
            "audio.mix.prepare",
            "Prepare Microphone and Music Mix",
            r#"{"main":[audioId],"backup":[audioId]?,"music":[audioId]?,"tone":"neutral|warm|bright"?,"intensity":0..1=.35,"repairChannels":bool=true,"normalize":bool=true,"targetLufs":LUFS=-16,"maxGainDb":dB=12,"truePeakDbtp":dBTP=-1,"duckMusic":bool=true,"duckDb":dB=-12,"duckFadeSeconds":0..5=.4,"replaceAnimation":bool=false,"finalLimiter":bool=true,"limiterCeilingDbfs":dBFS=-1.5,"baseRevision":revision?,"label":str?}"#,
            true,
            prepare,
        ),
        make("audio.mix.roles", "Set Explicit Audio Mix Roles", r#"{"main":[audioId],"backup":[audioId]?,"music":[audioId]?}"#, false, roles),
        make(
            "audio.mix.duck",
            "Duck Music under Explicit Main Clips",
            r#"{"main":[audioId],"music":[audioId],"duckDb":dB=-12,"duckFadeSeconds":0..5=.4,"replaceAnimation":bool=false}"#,
            false,
            duck,
        ),
    ]
}

#[cfg(test)]
mod tests;
