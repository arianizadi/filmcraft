//! A usable local footage → measured proposal → native review → export/QC entry point.
//! Selection is deterministic and explicitly heuristic. No model, media, or edit is invented.
use crate::{
    EngineError, Result, Session,
    commands::{CommandSpec, always, bad},
};
use filmcraft_project::{ItemId, Project, review::EditingProfile};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
const MAX_ITEMS: usize = 32;
const MAX_SHOTS: usize = 96;
pub(crate) fn check_cancelled(s: &Session) -> Result<()> {
    if s.workflow_progress.as_ref().is_some_and(|p| p.cancel.load(std::sync::atomic::Ordering::Relaxed)) {
        return Err(err("cancelled; live project unchanged"));
    }
    Ok(())
}
fn stage(s: &Session, done: u64, status: &str) -> Result<()> {
    check_cancelled(s)?;
    if let Some(p) = &s.workflow_progress {
        p.done.fetch_max(done, std::sync::atomic::Ordering::Relaxed);
        *p.status.lock().unwrap_or_else(|e| e.into_inner()) = status.into();
    }
    Ok(())
}
/// Carry the job's shared token into native decoders and the child analysis workers.
fn with_job_cancel<T>(progress: &Arc<filmcraft_export::Progress>, run: impl FnOnce() -> Result<T>) -> Result<T> {
    filmcraft_media::cancel::with_cancel(&progress.cancel, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|_| Err(err("background workflow worker failed; project unchanged")))
    })
}

pub struct BackgroundJob<T> {
    pub job: crate::Job,
    pub receiver: std::sync::mpsc::Receiver<Result<T>>,
    registry: crate::durable_jobs::JobRegistry,
    started: web_time::Instant,
}
impl<T> BackgroundJob<T> {
    pub fn cancel(&self) {
        self.job.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        *self.job.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = "Cancelling; waiting for current bounded read".into();
        if let Err(e) = self.registry.sync(std::slice::from_ref(&self.job)) {
            log::error!("{e}");
        }
    }
}
fn finish_job(
    job: &crate::Job,
    registry: &crate::durable_jobs::JobRegistry,
    started: web_time::Instant,
    result: std::result::Result<&Value, &EngineError>,
    path: String,
) {
    use std::sync::atomic::Ordering;
    let result = match result {
        Ok(value) => {
            job.progress.done.store(job.progress.total.load(Ordering::Relaxed), Ordering::Relaxed);
            let status = if let Some(plan) = value["plan"].as_u64() {
                format!("Prepared plan #{plan}; awaiting native approval")
            } else if value["verified"] == true {
                "Export verified".into()
            } else if value.get("verified").is_some() {
                "Verification completed with failed or unknown checks".into()
            } else {
                "Completed".into()
            };
            *job.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
            Ok(filmcraft_export::Report { path, frames: 0, seconds: started.elapsed().as_secs_f64(), bytes: 0, render_fps: 0.0, extra_files: Vec::new() })
        }
        Err(error) => {
            let message = error.to_string();
            *job.progress.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message.clone());
            *job.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = message.clone();
            Err(message)
        }
    };
    job.progress.finished.store(true, Ordering::Relaxed);
    *job.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    if let Err(e) = registry.sync(std::slice::from_ref(job)) {
        log::error!("{e}");
    }
}
fn start_job_at<T>(s: &mut Session, input: Option<&Session>, label: &str, command: &str) -> Result<(BackgroundJob<T>, std::sync::mpsc::Sender<Result<T>>)> {
    let created = if let Some(input) = input {
        s.job_registry.new_job(input, label.into())
    } else {
        let origin = s.command_origin.replace(command.into());
        let created = s.job_registry.new_job(s, label.into());
        s.command_origin = origin;
        created
    };
    let job = created?;
    job.progress.total.store(1000, std::sync::atomic::Ordering::Relaxed);
    let (tx, receiver) = std::sync::mpsc::channel();
    let task = BackgroundJob { job: job.clone(), receiver, registry: s.job_registry.clone(), started: web_time::Instant::now() };
    s.jobs.push(job);
    Ok((task, tx))
}
fn start_job<T>(s: &mut Session, label: &str, command: &str) -> Result<(BackgroundJob<T>, std::sync::mpsc::Sender<Result<T>>)> {
    start_job_at(s, None, label, command)
}
pub fn adopt_background_plan(
    s: &mut Session,
    task: &BackgroundJob<crate::agent_plans::PreparedEditPlan>,
    result: Result<crate::agent_plans::PreparedEditPlan>,
) -> Result<Value> {
    let result = if task.job.progress.cancel.load(std::sync::atomic::Ordering::Relaxed) {
        Err(err("cancelled; proposal not adopted"))
    } else {
        result.and_then(|plan| s.agent_adopt_prepared_plan(plan))
    };
    finish_job(&task.job, &task.registry, task.started, result.as_ref(), String::new());
    result
}
fn validate_request(p: &Value, keys: &[&str]) -> Result<()> {
    let params = p.as_object().ok_or_else(|| err("parameters must be an object"))?;
    for (key, value) in params {
        if !keys.contains(&key.as_str()) {
            return Err(err(format!("unknown parameter `{key}`")));
        }
        if ["apply", "analyze", "captions", "normalizeAudio", "transcribe", "qc", "wait"].contains(&key.as_str()) && !value.is_boolean() {
            return Err(err(format!("{key} must be a boolean")));
        }
        if ["maxShots", "musicItem", "plan"].contains(&key.as_str()) && !value.is_u64() {
            return Err(err(format!("{key} must be an unsigned integer")));
        }
        if key == "targetDurationTicks" && !value.is_i64() {
            return Err(err("targetDurationTicks must be integer ticks"));
        }
        if ["profile", "mode", "name", "path"].contains(&key.as_str()) && !value.is_string() {
            return Err(err(format!("{key} must be text")));
        }
    }
    Ok(())
}
const EDIT_KEYS: &[&str] = &[
    "items",
    "paths",
    "profile",
    "mode",
    "musicItem",
    "name",
    "maxShots",
    "targetDurationTicks",
    "overrides",
    "analyze",
    "normalizeAudio",
    "captions",
    "apply",
    "transcribe",
    "qc",
];
fn annotate_quality(s: &mut Session, plan: u64, p: &Value, mut workflow: Value) -> Result<Value> {
    if p["qc"].as_bool().unwrap_or(p["analyze"].as_bool().unwrap_or(true)) {
        workflow["timelineQc"] =
            s.execute("qc.timeline", json!({"plan":plan,"maxFrames":120})).unwrap_or_else(|e| json!({"status":"unavailable","error":e.to_string()}));
    }
    s.agent_annotate_plan(plan, &json!({"workflow":workflow}))
}
fn err(message: impl Into<String>) -> EngineError {
    bad("workflow.autoEdit", message)
}
fn items(s: &Session, p: &Value) -> Result<Vec<ItemId>> {
    let list = p["items"].as_array().filter(|v| !v.is_empty() && v.len() <= MAX_ITEMS).ok_or_else(|| err("supply 1–32 imported source item IDs"))?;
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for v in list {
        let id = v.as_u64().map(ItemId).filter(|i| seen.insert(*i)).ok_or_else(|| err("item IDs must be unsigned and unique"))?;
        let item = s.project.item(id).filter(|i| i.as_media().is_some()).ok_or_else(|| err("each input must be a source media item"))?;
        if item.as_media().is_some_and(|m| m.offline) {
            return Err(err(format!("source {} is offline", id.0)));
        }
        if item.duration() <= Tick::ZERO {
            return Err(err(format!("source {} has no duration", id.0)));
        }
        ids.push(id)
    }
    Ok(ids)
}
fn profile(s: &mut Session, p: &Value, mode: &str) -> Result<(EditingProfile, bool)> {
    let name = p["profile"].as_str().map(str::to_string);
    if name.is_some() || s.project.review.active_profile.is_some() {
        let mut params = json!({"overrides":p.get("overrides").cloned().unwrap_or(json!({}))});
        if let Some(name) = name {
            params["name"] = json!(name)
        }
        let profile = s.execute("profile.resolve", params)?;
        return serde_json::from_value(profile).map(|v| (v, false)).map_err(|e| err(e.to_string()));
    }
    let automatic_name = format!("Automatic {mode}");
    if s.project.review.profiles.contains_key(&automatic_name) {
        let profile = s.execute("profile.resolve", json!({"name":automatic_name,"overrides":p.get("overrides").cloned().unwrap_or(json!({}))}))?;
        return serde_json::from_value(profile).map(|v| (v, false)).map_err(|e| err(e.to_string()));
    }
    let mut profile =
        EditingProfile { name: format!("Automatic {mode}"), pacing: if mode == "dj" { "fast".into() } else { "balanced".into() }, ..Default::default() };
    if let Some(overrides) = p.get("overrides") {
        let mut value = serde_json::to_value(&profile).map_err(|e| err(e.to_string()))?;
        let object = overrides.as_object().ok_or_else(|| err("overrides must be an object"))?;
        for (k, v) in object {
            if matches!(k.as_str(), "name" | "version") {
                return Err(err("profile identity cannot be overridden"));
            }
            if let Some(map) = value.as_object_mut() {
                map.insert(k.clone(), v.clone());
            }
        }
        profile = serde_json::from_value(value).map_err(|e| err(e.to_string()))?;
    }
    profile.check().map_err(err)?;
    Ok((profile, true))
}
fn reader(s: &Session) -> Session {
    let mut reader = Session::new(s.services.clone());
    reader.project = s.project.clone();
    reader.project_instance = s.project_instance.clone();
    reader.state = s.state.clone();
    reader.revision = s.revision;
    reader.effect_identity_floor = s.effect_identity_floor;
    reader.path = s.path.clone();
    reader.operations.permission = s.operations.permission;
    reader.media = s.media.clone();
    reader.transcriber = s.transcriber.clone();
    reader.prefs = s.prefs.clone();
    reader.record_review_activity = false;
    reader.workflow_progress = s.workflow_progress.clone();
    reader.prefs.media_analysis.auto_transcribe = false;
    reader
}
fn analyze_one(s: &mut Session, item: ItemId) -> Result<Value> {
    let duration = s.project.item(item).map(|i| i.duration()).ok_or_else(|| err("source missing"))?;
    s.execute("analysis.footage",json!({"subject":{"target":"source","item":item.0,"effects":"pre"},"start":0,"end":duration.0.min(TICKS_PER_SECOND*300),"maxFrames":60,"video":true,"audio":true,"speech":false,"music":true,"waveform":false,"details":false}))
}
fn analysis(s: &Session, ids: &[ItemId], enabled: bool) -> Vec<(ItemId, Result<Value>)> {
    if !enabled {
        return Vec::new();
    }
    #[cfg(target_arch = "wasm32")]
    {
        ids.iter().map(|id| (*id, analyze_one(&mut reader(s), *id))).collect()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut results = Vec::new();
        for batch in ids.chunks(4) {
            let mut workers = Vec::new();
            for id in batch {
                let id = *id;
                let services = s.services.clone();
                let project = s.project.clone();
                let state = s.state.clone();
                let media = s.media.clone();
                let prefs = s.prefs.clone();
                let revision = s.revision;
                let progress = s.workflow_progress.clone();
                let cancel = filmcraft_media::cancel::Context::current();
                let handle = std::thread::Builder::new().name("footage-analysis".into()).spawn(move || {
                    cancel
                        .run(|| {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                let mut r = Session::new(services);
                                r.project = project;
                                r.state = state;
                                r.media = media;
                                r.prefs = prefs;
                                r.revision = revision;
                                r.record_review_activity = false;
                                r.workflow_progress = progress;
                                check_cancelled(&r)?;
                                analyze_one(&mut r, id)
                            }))
                        })
                        .unwrap_or_else(|_| Err(err("analysis worker failed; source and timeline unchanged")))
                });
                workers.push((id, handle));
            }
            for (id, worker) in workers {
                let result = match worker {
                    Ok(worker) => worker.join().unwrap_or_else(|_| Err(err("analysis worker interrupted"))),
                    Err(e) => Err(err(format!("could not start analysis worker: {e}"))),
                };
                results.push((id, result));
            }
        }
        results
    }
}
fn dialogue_ranges(project: &Project, item: ItemId, padding: Tick, pacing: &str) -> Vec<TimeRange> {
    let duration = project.item(item).map(|i| i.duration()).unwrap_or(Tick::ZERO);
    let Some(transcript) = project.transcripts.get(&item) else { return vec![TimeRange::new(Tick::ZERO, duration)] };
    let words = &transcript.words;
    let min_pause = match pacing {
        "fast" => TICKS_PER_SECOND / 2,
        "slow" => TICKS_PER_SECOND * 2,
        _ => TICKS_PER_SECOND,
    };
    let mut remove = Vec::new();
    for pair in words.windows(2) {
        if let (Some(a), Some(b)) = (pair.first(), pair.get(1)) {
            let start = Tick(a.end.0.saturating_add(padding.0));
            let end = Tick(b.start.0.saturating_sub(padding.0));
            if b.start.0.saturating_sub(a.end.0) >= min_pause && end > start {
                remove.push(TimeRange::from_bounds(start, end));
            }
        }
    }
    for (i, word) in words.iter().enumerate() {
        if filmcraft_edit::transcript::DEFAULT_FILLERS.contains(&word.normalized().as_str()) {
            let start = Tick(word.start.0.saturating_sub(padding.0 / 2).max(words.get(i.wrapping_sub(1)).map_or(0, |w| w.end.0)).max(0));
            let end = Tick(word.end.0.saturating_add(padding.0 / 2).min(words.get(i.saturating_add(1)).map_or(duration.0, |w| w.start.0)).min(duration.0));
            if end > start {
                remove.push(TimeRange::from_bounds(start, end));
            }
        }
    }
    let mut out = Vec::new();
    let mut cursor = Tick::ZERO;
    for span in filmcraft_edit::transcript::merge_ranges(remove) {
        let start = span.start.max(Tick::ZERO).min(duration);
        if start > cursor {
            out.push(TimeRange::from_bounds(cursor, start));
        }
        cursor = cursor.max(span.end().min(duration));
    }
    if cursor < duration {
        out.push(TimeRange::from_bounds(cursor, duration));
    }
    out
}
fn best_center(report: Option<&Value>, duration: Tick) -> Tick {
    report
        .and_then(|v| v["frames"].as_array())
        .and_then(|frames| {
            frames.iter().filter(|f| f["metrics"]["blackFraction"].as_f64().unwrap_or(0.0) < 0.95).max_by(|a, b| {
                let score = |f: &Value| {
                    f["metrics"]["sharpness"].as_f64().unwrap_or(0.0) + f["metrics"]["motion"].as_f64().unwrap_or(0.0) * 0.2
                        - f["metrics"]["whiteFraction"].as_f64().unwrap_or(0.0)
                };
                score(a).total_cmp(&score(b))
            })
        })
        .and_then(|f| f["position"]["time"].as_i64())
        .map(Tick)
        .unwrap_or(Tick(duration.0 / 2))
        .max(Tick::ZERO)
        .min(duration)
}
fn auto_options(p: &Value) -> Result<(&str, u64, i64)> {
    let mode = p["mode"].as_str().unwrap_or("talkingHead");
    if !["talkingHead", "dj", "selects"].contains(&mode) {
        return Err(err("mode is talkingHead, dj or selects"));
    }
    let max_shots = p["maxShots"].as_u64().unwrap_or(48);
    if max_shots == 0 || max_shots > MAX_SHOTS as u64 {
        return Err(err("maxShots must be 1–96"));
    }
    let target = p["targetDurationTicks"].as_i64().unwrap_or(TICKS_PER_SECOND * 60);
    if target <= 0 || target > TICKS_PER_SECOND * 3600 {
        return Err(err("targetDurationTicks must be positive and at most one hour"));
    }
    Ok((mode, max_shots, target))
}
struct Selection<'a> {
    mode: &'a str,
    profile: &'a EditingProfile,
    music: Option<ItemId>,
    beat_period: Option<i64>,
    reports: &'a [(ItemId, Result<Value>)],
}
fn choose_ranges(s: &Session, ids: &[ItemId], selection: Selection<'_>, warnings: &mut Vec<String>) -> Result<Vec<(ItemId, TimeRange)>> {
    let Selection { mode, profile, music, beat_period, reports } = selection;
    let rate = filmcraft_time::FrameRate::new(profile.delivery.fps_num, profile.delivery.fps_den);
    let mut ranges = Vec::new();
    for id in ids {
        if Some(*id) == music {
            continue;
        }
        let source = s.project.item(*id).ok_or_else(|| err("source missing"))?;
        if !source.has_video() && mode != "talkingHead" {
            continue;
        }
        let duration = source.duration();
        let report = reports.iter().find(|(i, _)| i == id).and_then(|(_, v)| v.as_ref().ok());
        if mode == "talkingHead" {
            if !s.project.transcripts.contains_key(id) {
                warnings.push(format!("Source {} has no saved transcript; dialogue and pauses were preserved.", id.0))
            }
            for span in dialogue_ranges(&s.project, *id, Tick(profile.pause_padding_ticks), &profile.pacing) {
                if span.duration >= rate.frame_duration() {
                    ranges.push((*id, span));
                }
            }
        } else {
            let length = if mode == "dj" { beat_period.unwrap_or(TICKS_PER_SECOND / 2).saturating_mul(4) } else { TICKS_PER_SECOND * 4 }.min(duration.0);
            let center = best_center(report, duration);
            let start = Tick(center.0.saturating_sub(length / 2).max(0).min(duration.0.saturating_sub(length)));
            ranges.push((*id, TimeRange::new(start, Tick(length))));
        }
    }
    Ok(ranges)
}
fn auto_edit(s: &mut Session, p: &Value) -> Result<Value> {
    stage(s, 420, "Analyzing source footage")?;
    validate_request(p, EDIT_KEYS)?;
    crate::agent_plans::bounded_json(p, 262_144)?;
    if p["apply"].as_bool() == Some(true) && s.operations.permission != crate::operations::Permission::Edit {
        return Err(err("automatic apply requires an explicit Edit grant; leave apply false for native review"));
    }
    let ids = items(s, p)?;
    let (mode, max_shots, target) = auto_options(p)?;
    let (profile, save_profile) = profile(s, p, mode)?;
    let rate = filmcraft_time::FrameRate::new(profile.delivery.fps_num, profile.delivery.fps_den);
    let reports = analysis(s, &ids, p["analyze"].as_bool().unwrap_or(true));
    check_cancelled(s)?;
    stage(s, 650, "Building timeline proposal")?;
    let mut warnings = Vec::new();
    for (id, result) in &reports {
        if let Err(e) = result {
            warnings.push(format!("Source {} analysis unavailable: {e}. Timing-only fallback used.", id.0));
        }
    }
    let music = p["musicItem"].as_u64().map(ItemId);
    if music.is_some_and(|id| !ids.contains(&id)) {
        return Err(err("musicItem must be included in items"));
    }
    let beat_source = music.or_else(|| ids.iter().copied().find(|id| s.project.item(*id).is_some_and(|i| i.has_audio())));
    let beats =
        beat_source.and_then(|id| reports.iter().find(|(i, _)| *i == id)).and_then(|(_, v)| v.as_ref().ok()).and_then(|v| v["music"]["beats"].as_array());
    let beat_period = beats
        .and_then(|b| b.first().zip(b.get(1)))
        .and_then(|(a, b)| a["time"].as_i64().zip(b["time"].as_i64()))
        .map(|(a, b)| b.saturating_sub(a))
        .filter(|d| *d > 0 && *d < TICKS_PER_SECOND * 5);
    if mode == "dj" && beat_period.is_none() {
        warnings.push("No reliable beat grid was available; cuts use a fixed two-second rhythm.".into())
    }
    let ranges = choose_ranges(s, &ids, Selection { mode, profile: &profile, music, beat_period, reports: &reports }, &mut warnings)?;
    if ranges.is_empty() {
        return Err(err("no usable video/dialogue ranges were found"));
    }
    if ranges.len() > max_shots as usize {
        return Err(err(format!("{} proposed ranges exceed maxShots; raise it to at most96 or use fewer inputs", ranges.len())));
    }
    let mut steps = vec![
        json!({"id":"file.newSequence","params":{"name":p["name"].as_str().unwrap_or("Automatic edit"),"width":profile.delivery.width,"height":profile.delivery.height,"frameRate":{"num":profile.delivery.fps_num,"den":profile.delivery.fps_den},"video":2,"audio":2}}),
    ];
    let mut cursor = Tick::ZERO;
    let mut audio = Vec::new();
    let mut video = Vec::new();
    let mut chosen = Vec::new();
    let mut truncated = false;
    for (id, mut span) in ranges {
        if mode != "talkingHead" && cursor.0 >= target {
            truncated = true;
            break;
        }
        if mode != "talkingHead" {
            span.duration = Tick(span.duration.0.min(target.saturating_sub(cursor.0)))
        }
        let duration = rate.snap(span.duration);
        if duration <= Tick::ZERO {
            continue;
        }
        let item = s.project.item(id).ok_or_else(|| err("source missing"))?;
        let index = steps.len();
        let has_video = item.has_video();
        let has_audio = item.has_audio();
        steps.push(json!({"id":"timeline.place","params":{"item":id.0,"time":cursor.0,"sourceIn":span.start.0,"duration":duration.0,"track":"V1","audioTrack":"A1","video":has_video,"audio":has_audio&&(mode!="dj"||music.is_none())}}));
        if has_video {
            video.push(json!({"$step":index,"path":"/clips/0"}));
        }
        if has_audio && (mode != "dj" || music.is_none()) {
            audio.push(json!({"$step":index,"path":if has_video{"/clips/1"}else{"/clips/0"}}));
        }
        chosen.push(json!({"item":id.0,"sourceStart":span.start.0,"sourceEnd":span.start.0.saturating_add(duration.0),"timelineStart":cursor.0,"timelineEnd":cursor.0.saturating_add(duration.0),"basis":if mode=="talkingHead"{"saved transcript pauses/fillers where available"}else{"measured sharpness/motion sample; no semantic judgment"}}));
        cursor = Tick(cursor.0.checked_add(duration.0).filter(|n| *n <= Tick::MAX.0).ok_or_else(|| err("timeline duration exceeds supported bounds"))?);
    }
    if let Some(music) = music {
        let source = s.project.item(music).ok_or_else(|| err("music source is missing"))?;
        if !source.has_audio() {
            return Err(err("musicItem has no audio"));
        }
        let duration = cursor.min(source.duration());
        let index = steps.len();
        steps.push(json!({"id":"timeline.place","params":{"item":music.0,"time":0,"sourceIn":0,"duration":duration.0,"track":"V2","audioTrack":"A2","video":false,"audio":true}}));
        audio.push(json!({"$step":index,"path":"/clips/0"}));
        if source.duration() < cursor {
            warnings.push("Music is shorter than the cut; the final section will have no music.".into())
        }
    }
    if save_profile {
        steps.push(json!({"id":"profile.save","params":{"profile":profile}}))
    }
    steps.push(json!({"id":"sequence.colorSettings","params":{"workingSpace":profile.delivery.color_space}}));
    if p["captions"].as_bool().unwrap_or(mode == "talkingHead") && ids.iter().any(|id| s.project.transcripts.contains_key(id)) {
        steps.push(json!({"id":"transcript.createCaptions","params":{"name":"Dialogue captions"}}));
    }
    steps.push(
        json!({"id":"profile.apply","params":{"name":profile.name,"version":profile.version,"overrides":p.get("overrides").cloned().unwrap_or(json!({}))}}),
    );
    if !audio.is_empty() && p["normalizeAudio"].as_bool().unwrap_or(true) {
        steps.push(json!({"id":"essentialSound.setType","params":{"clips":audio,"type":if mode=="dj"{"music"}else{"dialogue"}}}));
        steps.push(json!({"id":"workflow.normalizeAudio","params":{"clips":audio,"target":profile.loudness_lufs}}));
    }
    if let Some(look) = &profile.reference_look {
        for clip in &video {
            steps.push(json!({"id":"grade.look.link","params":{"look":look,"clip":clip}}));
        }
    }
    if steps.len() > 256 {
        return Err(err("automatic edit exceeds 256 staged commands; reduce maxShots"));
    }
    for (index, step) in steps.iter_mut().enumerate() {
        if index > 0 {
            step["dependsOn"] = json!([0]);
        }
    }
    let mut report = s.execute("agent.plan", json!({"label":format!("Automatic {mode} edit"),"baseRevision":s.revision,"steps":steps}))?;
    report["workflow"] = json!({"mode":mode,"selection":"deterministic local heuristic","ranges":chosen,"warnings":warnings,"truncated":truncated,"profile":profile,"duration":cursor.0,"sourcesPreserved":true,"approvalRequired":true});
    if let Some(id) = report["plan"].as_u64() {
        report = annotate_quality(s, id, p, report["workflow"].clone())?;
    }
    if p["apply"].as_bool() == Some(true) {
        report["applied"] = s.execute("agent.apply", json!({"plan":report["plan"]}))?;
        report["workflow"]["approvalRequired"] = json!(false);
    }
    Ok(report)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IncomingSource {
    item: filmcraft_project::ProjectItem,
    #[serde(default)]
    transcript: Option<filmcraft_project::Transcript>,
}
fn import_sources(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 1_048_576)?;
    let sources: Vec<IncomingSource> = serde_json::from_value(p["sources"].clone()).map_err(|e| err(e.to_string()))?;
    if sources.is_empty() || sources.len() > MAX_ITEMS {
        return Err(err("source metadata must contain1–32 items"));
    }
    let mut validated = Vec::new();
    for source in sources {
        let filmcraft_project::ItemKind::Media(media) = source.item.kind else { return Err(err("only source media can be prepared")) };
        if source.item.name.len() > 1024 || media.info.duration <= Tick::ZERO || media.info.duration > Tick::MAX {
            return Err(err("invalid source name or duration"));
        }
        if let Some(video) = &media.info.video {
            filmcraft_project::validate_frame_size(video.width, video.height).map_err(err)?;
            filmcraft_project::SequenceSettings { width: video.width, height: video.height, frame_rate: video.frame_rate, ..Default::default() }
                .validate()
                .map_err(err)?;
        }
        if let Some(rate) = media.interpret.frame_rate {
            filmcraft_project::SequenceSettings { frame_rate: rate, ..Default::default() }.validate().map_err(err)?;
        }
        if media.interpret.par.is_some_and(|par| filmcraft_project::checked_par(par).is_none()) {
            return Err(err("invalid interpreted pixel aspect"));
        }
        if media.markers.len() > 4096
            || media.markers.iter().any(|m| {
                m.start < Tick::ZERO
                    || m.duration < Tick::ZERO
                    || m.start > Tick::MAX
                    || m.duration.0 > Tick::MAX.0.saturating_sub(m.start.0)
                    || m.name.len() > 16384
                    || m.comment.len() > 16384
            })
        {
            return Err(err("invalid source markers"));
        }
        if media.info.audio_streams.len() > filmcraft_media::MAX_AUDIO_STREAMS
            || media.info.audio_streams.iter().any(|a| a.sample_rate == 0 || a.channels == 0 || a.channels > 256)
        {
            return Err(err("invalid source audio metadata"));
        }
        if let filmcraft_project::MediaRef::File { path } = &media.media
            && (path.len() > 8192 || path.contains('\0') || !std::path::Path::new(path).is_absolute())
        {
            return Err(err("invalid absolute media path"));
        }
        let mut transcript = source.transcript;
        if let Some(t) = &mut transcript {
            t.ensure_word_ids().map_err(err)?;
            t.check().map_err(err)?;
        }
        validated.push((source.item.name, source.item.label, source.item.custom_label_color, source.item.metadata, media, transcript));
    }
    let ids = s.edit("Prepare Imported Sources", move |pr, state| {
        let mut ids = Vec::new();
        for (name, label, color, metadata, mut media, transcript) in validated {
            for marker in &mut media.markers {
                marker.id = filmcraft_project::MarkerId(pr.alloc_id());
            }
            let id = pr.add_item(&name, label, filmcraft_project::ItemKind::Media(media), None);
            if let Some(item) = pr.item_mut(id) {
                item.custom_label_color = color;
                item.metadata = metadata;
            }
            if let Some(transcript) = transcript {
                pr.store_transcript(id, transcript);
            }
            ids.push(id.0);
        }
        state.project_selection = ids.iter().copied().map(ItemId).collect();
        Ok(ids)
    })?;
    Ok(json!({"items":ids,"sourceFilesChanged":false}))
}
fn offset_refs(value: &mut Value, offset: usize, depth: usize) -> Result<()> {
    if depth > 32 {
        return Err(err("proposal nesting limit"));
    }
    match value {
        Value::Object(map) => {
            if let Some(index) = map.get("$step") {
                let index = index
                    .as_u64()
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| i.checked_add(offset))
                    .ok_or_else(|| err("invalid proposal reference"))?;
                map.insert("$step".into(), json!(index));
            } else {
                for value in map.values_mut() {
                    offset_refs(value, offset, depth + 1)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                offset_refs(value, offset, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn prepare(s: &mut Session, p: &Value) -> Result<Value> {
    stage(s, 50, "Preparing original source footage")?;
    validate_request(p, EDIT_KEYS)?;
    crate::agent_plans::bounded_json(p, 262_144)?;
    if p["apply"].as_bool() == Some(true) && s.operations.permission != crate::operations::Permission::Edit {
        return Err(err("automatic apply requires Edit permission"));
    }
    let mut sandbox = reader(s);
    let mut request = p.clone();
    request["apply"] = json!(false);
    request["qc"] = json!(false);
    let mut imported = json!({});
    let mut imported_ids = Vec::new();
    if let Some(paths) = p.get("paths") {
        let paths = paths.as_array().filter(|a| !a.is_empty() && a.len() <= MAX_ITEMS).ok_or_else(|| err("paths must contain1–32 absolute source paths"))?;
        for path in paths {
            let path = path
                .as_str()
                .filter(|p| p.len() <= 8192 && std::path::Path::new(p).is_absolute())
                .ok_or_else(|| err("each path must be absolute and at most8192 bytes"))?;
            if path.contains('\0') {
                return Err(err("invalid source path"));
            }
            if std::path::Path::new(path).is_dir() {
                return Err(err("automatic preparation accepts at most32 source files; choose individual footage files instead of a directory"));
            }
        }
        Arc::make_mut(&mut sandbox.project).settings.ingest.enabled = false;
        imported = sandbox.execute("file.import", json!({"paths":paths}))?;
        imported_ids = imported["items"].as_array().cloned().unwrap_or_default();
        if imported_ids.is_empty() {
            return Err(err("no source footage was imported"));
        }
        Arc::make_mut(&mut sandbox.project).settings.ingest = s.project.settings.ingest.clone();
        request["items"] = json!(imported_ids);
    }
    stage(s, 180, "Recognizing local dialogue where available")?;
    let ids = items(&sandbox, &request)?;
    let mut warnings = Vec::new();
    if p["transcribe"].as_bool().unwrap_or(true) {
        let missing = ids
            .iter()
            .filter(|id| sandbox.project.item(**id).is_some_and(|i| i.has_audio()) && !sandbox.project.transcripts.contains_key(id))
            .map(|i| i.0)
            .collect::<Vec<_>>();
        if !missing.is_empty()
            && let Err(e) = sandbox.execute("transcript.generate", json!({"items":missing}))
        {
            warnings.push(format!("Local transcription unavailable: {e}. No model was downloaded; dialogue was preserved."));
        }
    }
    stage(s, 410, "Selecting source ranges")?;
    let original = auto_edit(&mut sandbox, &request)?;
    check_cancelled(s)?;
    let mut prefix = Vec::new();
    let mut remap = std::collections::BTreeMap::new();
    if !imported_ids.is_empty() {
        let mut sources = Vec::new();
        for (index, id) in imported_ids.iter().enumerate() {
            let id = id.as_u64().map(ItemId).ok_or_else(|| err("invalid imported ID"))?;
            let item = sandbox.project.item(id).ok_or_else(|| err("imported source is missing"))?;
            sources.push(json!({"item":item,"transcript":sandbox.project.transcripts.get(&id)}));
            remap.insert(id.0, index);
        }
        prefix.push(json!({"id":"workflow.importSources","params":{"sources":sources}}));
    }
    for id in &ids {
        if !remap.contains_key(&id.0)
            && sandbox.project.transcripts.get(id) != s.project.transcripts.get(id)
            && let Some(transcript) = sandbox.project.transcripts.get(id)
        {
            prefix.push(json!({"id":"transcript.set","params":{"item":id.0,"transcript":transcript}}));
        }
    }
    let offset = prefix.len();
    let mut steps = prefix;
    let commands = original["commands"].as_array().ok_or_else(|| err("prepared command list missing"))?;
    for (index, command) in commands.iter().enumerate() {
        let mut command = command.clone();
        offset_refs(&mut command, offset, 0)?;
        let mut deps =
            command["dependsOn"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(|i| i.saturating_add(offset as u64)).collect::<Vec<_>>();
        if index == 0 {
            deps.extend((0..offset).map(|i| i as u64));
        } else if !deps.contains(&(offset as u64)) {
            deps.push(offset as u64);
        }
        command["dependsOn"] = json!(deps);
        if command["id"] == "timeline.place"
            && let Some(index) = command["params"]["item"].as_u64().and_then(|id| remap.get(&id))
        {
            command["params"]["item"] = json!({"$step":0,"path":format!("/items/{index}")});
        }
        steps.push(command);
    }
    let report = s.execute("agent.plan", json!({"label":original["label"],"baseRevision":s.revision,"steps":steps}))?;
    let id = report["plan"].as_u64().ok_or_else(|| err("prepared plan ID missing"))?;
    let mut workflow = original["workflow"].clone();
    if !remap.is_empty() {
        if let Some(ranges) = workflow["ranges"].as_array_mut() {
            for range in ranges {
                if let Some(index) = range["item"].as_u64().and_then(|id| remap.get(&id)) {
                    range["item"] = report["steps"][0]["items"].get(*index).cloned().unwrap_or(Value::Null);
                }
            }
        }
        imported["items"] = report["steps"][0]["items"].clone();
    }
    workflow["approvalRequired"] = json!(true);
    stage(s, 850, "Checking proposed cut")?;
    annotate_quality(s, id, p, workflow)?;
    check_cancelled(s)?;
    let mut report =
        s.agent_annotate_plan(id, &json!({"preparation":{"imported":imported,"warnings":warnings,"sourceFilesChanged":false,"liveProjectChanged":false}}))?;
    if p["apply"].as_bool() == Some(true) {
        report["applied"] = s.execute("agent.apply", json!({"plan":id}))?;
        report["workflow"]["approvalRequired"] = json!(false);
    }
    Ok(report)
}
fn normalize_audio(s: &mut Session, p: &Value) -> Result<Value> {
    let clips = p["clips"].as_array().filter(|a| a.len() <= 192).ok_or_else(|| err("supply at most192 audio clip IDs"))?;
    let ids = clips.iter().map(|v| v.as_u64().map(filmcraft_project::ClipId).ok_or_else(|| err("invalid clip ID"))).collect::<Result<Vec<_>>>()?;
    if ids.iter().copied().collect::<BTreeSet<_>>().len() != ids.len() {
        return Err(err("duplicate audio clip IDs"));
    }
    let target = p["target"].as_f64().filter(|v| v.is_finite() && (-60.0..=0.0).contains(v)).ok_or_else(|| err("target loudness must be -60–0 LUFS"))?;
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    let mut audible = Vec::new();
    let mut skipped = Vec::new();
    let mut seconds = 0i64;
    for id in ids {
        let (track, clip) = q.find_item(id).ok_or_else(|| err("audio clip missing"))?;
        if !q.audio_tracks.iter().any(|t| t.id == track) {
            return Err(err("normalization targets must be audio clips"));
        }
        if q.track(track).is_some_and(|t| t.locked) {
            return Err(err("audio target is locked"));
        }
        let duration = clip.duration.0 / TICKS_PER_SECOND;
        if seconds.saturating_add(duration) > 3600 {
            skipped.push(json!({"clip":id.0,"reason":"automatic measurement exceeds one-hour read budget; explicit loudness job required"}));
            continue;
        }
        seconds = seconds.saturating_add(duration);
        if crate::essential_sound::clip_loudness(clip, q.settings.sample_rate.max(1), &provider).is_some() {
            audible.push(id.0);
        } else {
            skipped.push(json!({"clip":id.0,"reason":"no measurable audio"}));
        }
    }
    if audible.is_empty() {
        return Ok(json!({"matches":[],"skipped":skipped,"method":"ITU-R BS.1770 integrated loudness"}));
    }
    let result = s.execute("essentialSound.autoMatch", json!({"clips":audible,"target":target}))?;
    Ok(json!({"matches":result,"skipped":skipped,"method":"ITU-R BS.1770 integrated loudness"}))
}

fn available_sequence_media(s: &Session, sequence: ItemId) -> Result<()> {
    let mut pending = vec![sequence];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if seen.len() > 10_000 {
            return Err(err("sequence source graph exceeds automatic delivery limit"));
        }
        let item = s.project.item(id).ok_or_else(|| err(format!("source {} is missing", id.0)))?;
        match &item.kind {
            filmcraft_project::ItemKind::Sequence(q) => {
                let ids = q
                    .all_tracks()
                    .filter(|t| t.enabled && !t.muted)
                    .flat_map(|t| &t.items)
                    .filter(|i| i.enabled)
                    .map(|i| i.item)
                    .take(10_001)
                    .collect::<Vec<_>>();
                if pending.len().saturating_add(ids.len()) > 10_000 {
                    return Err(err("too many source references for automatic delivery"));
                }
                pending.extend(ids);
            }
            filmcraft_project::ItemKind::Subclip { parent, .. } => pending.push(*parent),
            filmcraft_project::ItemKind::Media(media) => {
                if media.offline {
                    return Err(err(format!("source {} is offline; relink before delivery", id.0)));
                }
                if let filmcraft_project::MediaRef::File { path } = &media.media {
                    s.services.file_size(path).map_err(|e| err(format!("source {} is unavailable: {e}", id.0)))?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}
/// Retain an exclusive native file handle through the writer's finalization. A later rename or
/// replacement of the requested path cannot make the encoder truncate a different file.
fn reserve_delivery_file(path: &str) -> Result<Arc<std::fs::File>> {
    if let Some(parent) = std::path::Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| err(format!("cannot create delivery directory: {e}")))?;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map(Arc::new)
        .map_err(|e| err(format!("cannot exclusively create delivery output {path}: {e}")))
}
fn write_reserved(file: &std::fs::File, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = file.try_clone().map_err(|e| err(e.to_string()))?;
    file.write_all(bytes).map_err(|e| err(format!("delivery write failed: {e}")))?;
    file.flush().map_err(|e| err(e.to_string()))?;
    file.sync_all().map_err(|e| err(e.to_string()))
}
fn delivery_sidecar(project: &Project, sequence: ItemId, settings: &filmcraft_export::ExportSettings) -> Result<Option<(String, Vec<u8>)>> {
    let Some(kind) = settings.caption_sidecar.as_deref() else { return Ok(None) };
    let q = project.sequence(sequence).ok_or(EngineError::NoSequence)?;
    let mut tracks = q.caption_tracks.iter().filter(|t| t.enabled && !t.captions.is_empty());
    let Some(track) = tracks.next() else { return Ok(None) };
    if tracks.next().is_some() {
        return Err(err("automatic sidecar delivery requires one enabled caption language; disable other caption tracks or export them individually"));
    }
    if track.captions.len() > 100_000 || track.captions.iter().try_fold(0usize, |n, c| n.checked_add(c.text.len())).is_none_or(|n| n > 16 * 1024 * 1024) {
        return Err(err("caption delivery exceeds100000 cues or16MiB text; split the caption export"));
    }
    let format = filmcraft_captions::Format::from_name(kind).ok_or_else(|| err("unsupported caption sidecar format"))?;
    let range = filmcraft_export::export_range(project, sequence, settings).map_err(|e| err(e.to_string()))?;
    let mut doc = filmcraft_captions::document_from_track(track, range.start);
    doc.cues.retain_mut(|cue| {
        cue.end = cue.end.min(range.duration);
        cue.start < cue.end
    });
    let bytes =
        filmcraft_captions::write(&doc, format, filmcraft_captions::WriteOptions { drop_frame: q.settings.drop_frame, rate: Some(q.settings.frame_rate) });
    let path = crate::export_safety::sidecar_path(settings)?.ok_or_else(|| err("caption sidecar path is missing"))?;
    Ok(Some((path, bytes)))
}
fn spawn_delivery(s: &mut Session, snapshot: Arc<Project>, sequence: ItemId, mut settings: filmcraft_export::ExportSettings, wait: bool) -> Result<Value> {
    if cfg!(target_arch = "wasm32") || s.services.export_in_memory() {
        return Err(err("automatic delivery requires a native filesystem; use the host export flow for virtual outputs"));
    }
    if settings.is_image_sequence() || settings.format == filmcraft_export::Format::MxfOpAtom {
        return Err(err("automatic delivery currently supports single-file outputs; numbered image sequences and MXF OP-Atom require explicit export"));
    }
    if !filmcraft_export::available(settings.format) {
        return Err(err("requested delivery encoder is unavailable"));
    }
    crate::export_safety::preflight(s, &snapshot, sequence, &settings)?;
    let sidecar = delivery_sidecar(&snapshot, sequence, &settings)?;
    // Cheap rejection is useful to the user, but create_new below is the actual race guard.
    for path in std::iter::once(settings.path.as_str()).chain(sidecar.as_ref().map(|(path, _)| path.as_str())) {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Err(err(format!("delivery output already exists: {path}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(err(format!("cannot inspect delivery output {path}: {e}"))),
        }
    }
    settings.output_file = Some(reserve_delivery_file(&settings.path)?);
    let mut sidecar_path = None;
    if let Some((path, bytes)) = sidecar {
        let file = reserve_delivery_file(&path).map_err(|e| err(format!("{e}; empty reserved movie remains at {}", settings.path)))?;
        write_reserved(file.as_ref(), &bytes)?;
        sidecar_path = Some(path);
    }
    // We already wrote the sidecar through its exclusive handle. The legacy writer must not reopen it.
    settings.caption_sidecar = None;
    let path = settings.path.clone();
    let id = crate::export_tools::spawn_export(s, snapshot, sequence, settings, "Export approved edit".into(), wait)?;
    let mut result = json!({"job":id,"path":path,"poll":"jobs.list","cancel":"jobs.cancel","sidecarPath":sidecar_path});
    if wait {
        let job = s.jobs.iter().find(|j| j.id == id).ok_or_else(|| err("delivery job is missing"))?;
        let status = crate::export_tools::job_json(s, job);
        if let Some(error) = status["result"]["error"].as_str() {
            return Err(err(format!("export failed: {error}")));
        }
        result["result"] = status["result"].clone();
        result["offlineMedia"] = status["offlineMedia"].clone();
    }
    Ok(result)
}
fn deliver(s: &mut Session, p: &Value) -> Result<Value> {
    validate_request(p, &["plan", "path", "profile", "wait"])?;
    let plan = p["plan"].as_u64().ok_or_else(|| err("delivery requires an approved plan ID"))?;
    let report = s.execute("agent.preview", json!({"plan":plan}))?;
    if report["status"] != "applied" {
        return Err(err("review and approve the plan before exporting"));
    }
    let (snapshot, state, _) = s.agent_plan_snapshot(plan, false)?;
    if *snapshot != *s.project {
        return Err(err("the approved snapshot changed; prepare and approve the updated edit before delivery"));
    }
    let sequence = state.active_sequence.ok_or(EngineError::NoSequence)?;
    available_sequence_media(s, sequence)?;
    let profile = if let Some(name) = p["profile"].as_str() {
        s.execute("profile.get", json!({"name":name}))?
    } else if s.project.review.active_profile.is_some() {
        s.execute("profile.resolve", json!({}))?
    } else {
        serde_json::to_value(EditingProfile::default()).map_err(|e| err(e.to_string()))?
    };
    let profile: EditingProfile = serde_json::from_value(profile).map_err(|e| err(e.to_string()))?;
    let path = p["path"].as_str().filter(|p| !p.is_empty() && p.len() <= 8192).ok_or_else(|| err("output path is required"))?.to_string();
    let q = s.project.sequence(sequence).ok_or(EngineError::NoSequence)?;
    let duration = q.duration();
    let rate = q.settings.frame_rate;
    let width = q.settings.width;
    let height = q.settings.height;
    let sr = q.settings.sample_rate;
    let captions = q.caption_tracks.iter().filter(|t| t.enabled).flat_map(|t| &t.captions).count();
    let requested_rate = filmcraft_time::FrameRate::new(profile.delivery.fps_num, profile.delivery.fps_den);
    if profile.delivery.width != width
        || profile.delivery.height != height
        || requested_rate != filmcraft_time::FrameRate::new(rate.num, rate.den)
        || filmcraft_color::WorkingSpace::parse(&profile.delivery.color_space) != Some(q.settings.color.working)
    {
        return Err(err(
            "delivery profile geometry, frame rate or color differs from the approved sequence; prepare and approve a matching layout/color plan first",
        ));
    }

    let mut export = json!({"path":path,"sequence":sequence.0,"format":profile.delivery.format,"width":width,"height":height,"frameRate":{"num":rate.num,"den":rate.den},"loudnessLufs":profile.loudness_lufs,"wait":p["wait"].as_bool().unwrap_or(true)});
    if ["srt", "vtt"].contains(&profile.delivery.captions.as_str()) && captions > 0 {
        export["captionSidecar"] = json!(profile.delivery.captions)
    } else if profile.delivery.captions == "burn" {
        export["burnCaptions"] = json!(true)
    }
    let preflight = if p["wait"].as_bool().unwrap_or(true) {
        s.execute("qc.timeline", json!({"sequence":sequence.0,"maxFrames":120}))
    } else {
        Ok(json!({"status":"see_proposal_qc","command":"qc.timeline","params":{"sequence":sequence.0,"maxFrames":120}}))
    };
    let (_, mut settings) = crate::export_tools::settings_from_params(s, &export, "workflow.deliver")?;
    settings.path = path.clone();
    let job = spawn_delivery(s, snapshot, sequence, settings, p["wait"].as_bool().unwrap_or(true))?;
    let mut expected = json!({"duration":duration.0,"frameRate":{"num":rate.num,"den":rate.den},"width":width,"height":height,"colorSpace":profile.delivery.color_space,"audio":{"present":true,"sampleRate":sr},"sequence":sequence.0});
    if captions > 0 {
        expected["captions"] = match profile.delivery.captions.as_str() {
            "none" => json!({"mode":"none","count":0}),
            "srt" | "vtt" => {
                json!({"mode":"sidecar","path":job["sidecarPath"],"format":profile.delivery.captions,"count":captions})
            }
            "burn" => json!({"mode":"burned","count":captions}),
            _ => return Err(err("unsupported caption delivery mode")),
        };
    }
    let qc = if p["wait"].as_bool().unwrap_or(true) {
        match s.execute("qc.export", json!({"path":path,"expected":expected})) {
            Ok(qc) => qc,
            Err(e) => json!({"verified":false,"error":e.to_string()}),
        }
    } else {
        json!({"status":"pending","command":"qc.export","params":{"path":path,"expected":expected}})
    };
    Ok(json!({"plan":plan,"sequence":sequence.0,"export":job,"timelineQc":preflight.unwrap_or_else(|e|json!({"error":e.to_string()})),"fileQc":qc}))
}
pub fn commands() -> Vec<CommandSpec> {
    let spec = |id, label, params, run, journal| CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal };
    vec![
        spec("workflow.importSources", "Stage Source Metadata", r#"{"sources":[{"item":ProjectItem,"transcript":Transcript?}]}"#, import_sources, true),
        spec("workflow.normalizeAudio", "Normalize Measurable Audio", r#"{"clips":[id],"target":lufs}"#, normalize_audio, true),
        spec(
            "workflow.autoEdit",
            "Automatically Propose an Edit",
            r#"{"items":[id],"profile":str?,"mode":"talkingHead|dj|selects"?,"musicItem":id?,"targetDurationTicks":ticks?,"maxShots":1..96=48,"overrides":object?,"analyze":bool=true,"normalizeAudio":bool=true,"captions":bool?,"apply":bool=false}"#,
            auto_edit,
            false,
        ),
        spec(
            "workflow.prepare",
            "Prepare Footage and Propose",
            r#"{"paths":[absolutePath]?,"items":[id]?,"profile":str?,"mode":"talkingHead|dj|selects"?,"transcribe":bool=true,"analyze":bool=true,"apply":bool=false}"#,
            prepare,
            false,
        ),
        spec("workflow.deliver", "Export Approved Edit and Verify", r#"{"plan":id,"path":str,"profile":str?,"wait":bool=true}"#, deliver, true),
    ]
}

/// Start native preparation from an immutable baseline. The only returned mutation is a validated
/// plan artifact; adoption refuses changes made while the worker was reading source footage.
pub fn prepare_background(s: &mut Session, mut params: Value) -> Result<BackgroundJob<crate::agent_plans::PreparedEditPlan>> {
    crate::agent_plans::bounded_json(&params, 262_144)?;
    params["apply"] = json!(false);
    let services = s.services.clone();
    let project = s.project.clone();
    let identity = s.project_instance.clone();
    let state = s.state.clone();
    let media = s.media.clone();
    let prefs = s.prefs.clone();
    let transcriber = s.transcriber.clone();
    let revision = s.revision;
    let effect_identity_floor = s.effect_identity_floor;
    let permission = s.operations.permission;
    let (task, tx) = start_job(s, "Prepare automatic edit", "workflow.prepare")?;
    let job = task.job.clone();
    let registry = task.registry.clone();
    let started = task.started;
    let run = move || {
        let result = with_job_cancel(&job.progress, || {
            let mut reader = Session::new(services);
            reader.project = project;
            reader.project_instance = identity;
            reader.state = state;
            reader.media = media;
            reader.prefs = prefs;
            reader.transcriber = transcriber;
            reader.revision = revision;
            reader.effect_identity_floor = effect_identity_floor;
            reader.operations.permission = permission;
            reader.record_review_activity = false;
            reader.workflow_progress = Some(job.progress.clone());
            let report = prepare(&mut reader, &params)?;
            check_cancelled(&reader)?;
            let plan = report["plan"].as_u64().ok_or_else(|| err("prepared plan is missing"))?;
            stage(&reader, 990, "Prepared; waiting for unchanged-baseline adoption")?;
            reader.agent_export_prepared_plan(plan)
        });
        if let Err(error) = &result {
            finish_job(&job, &registry, started, Err(error), String::new());
        }
        if tx.send(result).is_err() {
            let error = err("preparation consumer closed before adoption; project unchanged");
            finish_job(&job, &registry, started, Err(&error), String::new());
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    crate::durable_jobs::spawn(s, task.job.id, "automatic-edit", run)?;
    #[cfg(target_arch = "wasm32")]
    run();
    Ok(task)
}
/// The approved delivery baseline, retained while its export is running. File QC must compare
/// captions and other sequence evidence against this project even if the owner keeps editing.
#[derive(Clone)]
pub struct QcSnapshot {
    project: Arc<Project>,
    state: crate::EditorState,
    revision: u64,
    path: Option<String>,
    session_id: String,
    services: Arc<dyn crate::Services>,
    media: Arc<crate::MediaPool>,
}
impl QcSnapshot {
    pub fn capture(s: &Session) -> Self {
        Self {
            project: s.project.clone(),
            state: s.state.clone(),
            revision: s.revision,
            path: s.path.clone(),
            session_id: s.operations.session_id.clone(),
            services: s.services.clone(),
            media: s.media.clone(),
        }
    }
    fn reader(self) -> Session {
        let mut reader = Session::new(self.services);
        reader.project = self.project;
        reader.state = self.state;
        reader.media = self.media;
        reader.revision = self.revision;
        reader.path = self.path;
        reader.operations.session_id = self.session_id;
        reader.record_review_activity = false;
        reader
    }
}
fn value_background(
    s: &mut Session,
    snapshot: QcSnapshot,
    command: &'static str,
    params: Value,
    label: &'static str,
    path: String,
) -> Result<BackgroundJob<Value>> {
    crate::agent_plans::bounded_json(&params, 65_536)?;
    let mut input = snapshot.reader();
    input.command_origin = Some(command.into());
    input.jobs = s.jobs.clone();
    let (task, tx) = start_job_at(s, Some(&input), label, command)?;
    let services = input.services.clone();
    let project = input.project.clone();
    let state = input.state.clone();
    let media = input.media.clone();
    let revision = input.revision;
    let job = task.job.clone();
    let registry = task.registry.clone();
    let started = task.started;
    let run = move || {
        let result = with_job_cancel(&job.progress, || {
            let mut reader = Session::new(services);
            reader.project = project;
            reader.state = state;
            reader.media = media;
            reader.revision = revision;
            reader.workflow_progress = Some(job.progress.clone());
            stage(&reader, 50, label)?;
            let value = if command == "transcript.downloadModel" {
                crate::transcript::download_model_now(&mut reader, &params)?
            } else {
                reader.execute(command, params)?
            };
            check_cancelled(&reader)?;
            Ok(value)
        });
        finish_job(&job, &registry, started, result.as_ref(), path);
        let _ = tx.send(result);
    };
    let run = crate::durable_jobs::guard(s, &task.job, run);
    #[cfg(not(target_arch = "wasm32"))]
    crate::durable_jobs::spawn(s, task.job.id, "workflow-read", run)?;
    #[cfg(target_arch = "wasm32")]
    run();
    Ok(task)
}
pub fn qc_background(s: &mut Session, params: Value) -> Result<BackgroundJob<Value>> {
    qc_background_from(s, QcSnapshot::capture(s), params)
}
pub fn qc_background_from(s: &mut Session, snapshot: QcSnapshot, params: Value) -> Result<BackgroundJob<Value>> {
    let path = params["path"].as_str().unwrap_or("").to_string();
    value_background(s, snapshot, "qc.export", params, "Verify delivery file", path)
}
/// Native explicit opt-in only: the UI shows catalogue source/licence/size before calling this.
pub fn download_model_background(s: &mut Session, model: String) -> Result<BackgroundJob<Value>> {
    if model.len() > 128 {
        return Err(err("model identifier is too long"));
    }
    let info = filmcraft_speech::models::find(&model).ok_or_else(|| err("unknown local speech model"))?;
    let directory = crate::transcript::models_dir().ok_or_else(|| err("no local speech model directory"))?;
    let path = filmcraft_speech::models::model_dir(&directory, info).to_string_lossy().into_owned();
    value_background(s, QcSnapshot::capture(s), "transcript.downloadModel", json!({"model":model}), "Download chosen local speech model", path)
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    #[test]
    fn automatic_sidecar_delivery_requires_one_explicit_enabled_language() {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({})).unwrap();
        s.execute("captions.add", json!({"text":"Original","time":0})).unwrap();
        s.execute("captions.newTrack", json!({"name":"Translation"})).unwrap();
        s.execute("captions.add", json!({"text":"Translation","track":"C1","time":0})).unwrap();
        let settings = filmcraft_export::ExportSettings { caption_sidecar: Some("srt".into()), ..Default::default() };
        let error = delivery_sidecar(&s.project, s.state.active_sequence.unwrap(), &settings).unwrap_err();
        assert!(error.to_string().contains("one enabled caption language"), "{error}");
    }
    #[test]
    fn exclusive_delivery_handles_cannot_truncate_a_later_path_replacement() {
        let dir = crate::temp_dir().join(format!("filmcraft-delivery-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("output.mp4");
        let reserved = reserve_delivery_file(path.to_str().unwrap()).unwrap();
        let moved = dir.join("reserved.mp4");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, b"unrelated file appeared after reservation").unwrap();
        assert!(reserve_delivery_file(path.to_str().unwrap()).is_err());
        write_reserved(&reserved, b"encoded output through retained handle").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"unrelated file appeared after reservation");
        assert_eq!(std::fs::read(&moved).unwrap(), b"encoded output through retained handle");
        drop(reserved);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn exclusive_delivery_rejects_a_dangling_symlink_without_creating_its_target() {
        let dir = crate::temp_dir().join(format!("filmcraft-delivery-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("unrelated.mp4");
        let path = dir.join("output.mp4");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(!path.exists(), "the former exists check missed this occupied name");
        assert!(reserve_delivery_file(path.to_str().unwrap()).is_err());
        assert!(!target.exists());
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
