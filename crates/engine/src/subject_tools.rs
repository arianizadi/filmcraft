//! Reviewable source-pixel subject alpha and editable reframing. Tracking uses the native local
//! color-graph implementation; it makes no semantic person-recognition or universal-occlusion claim.
use crate::analysis_support::{SubjectArgs, Target, bounded_decode};
use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};
use filmcraft_geom::Vec2;
use filmcraft_project::{AlphaFrame, AlphaTrack, ClipId, ItemId, Mask, MaskMode, MaskPath, ParamValue, TrackItem};
use filmcraft_render::subject::{Prompt, Tracker};
use filmcraft_time::Tick;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
const MAX_SAMPLES: usize = 240;
fn opacity() -> String {
    "opacity:0".into()
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Address {
    clip: u64,
    sequence: Option<u64>,
    #[serde(default = "opacity")]
    effect_instance: String,
    mask: Option<usize>,
}
fn sequence_id(s: &Session, address: &Address) -> Result<ItemId> {
    address.sequence.map(ItemId).or(s.state.active_sequence).filter(|id| s.project.sequence(*id).is_some()).ok_or(crate::EngineError::NoSequence)
}
fn locate<'a>(s: &'a Session, address: &Address) -> Result<(ItemId, &'a TrackItem, usize)> {
    let id = sequence_id(s, address)?;
    let seq = s.project.sequence(id).ok_or(crate::EngineError::NoSequence)?;
    let (track, it) = seq.find_item(ClipId(address.clip)).ok_or_else(|| bad("subject", "unknown clip in requested sequence"))?;
    if !seq.video_tracks.iter().any(|t| t.id == track) {
        return Err(bad("subject", "subject alpha needs a video clip"));
    }
    let (name, ordinal) = address.effect_instance.rsplit_once(':').ok_or_else(|| bad("subject", "effectInstance must be effect-name:ordinal"))?;
    let ordinal: usize = ordinal.parse().map_err(|_| bad("subject", "invalid effect instance ordinal"))?;
    if ordinal > 128 {
        return Err(bad("subject", "effect ordinal exceeds limit"));
    }
    let effect = it
        .effects
        .iter()
        .enumerate()
        .filter(|(_, e)| e.effect == name)
        .nth(ordinal)
        .map(|(i, _)| i)
        .ok_or_else(|| bad("subject", "unknown effect instance"))?;
    if it.effects.get(effect).and_then(|e| e.def()).is_none_or(|d| d.kind != filmcraft_project::EffectKind::Video) {
        return Err(bad("subject", "alpha masks apply to a video effect"));
    }
    Ok((id, it, effect))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InspectRequest {
    #[serde(flatten)]
    address: Address,
    #[serde(default)]
    include_data: bool,
    media_time: Option<i64>,
}
fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    use base64::Engine as _;
    let p: InspectRequest = bounded_decode(p, "subject.inspect")?;
    let (seq, it, effect) = locate(s, &p.address)?;
    let e = it.effects.get(effect).ok_or_else(|| bad("subject.inspect", "effect vanished"))?;
    let index =
        p.address.mask.or_else(|| e.masks.iter().position(|m| m.alpha.is_some())).ok_or_else(|| bad("subject.inspect", "effect has no subject alpha"))?;
    let mask = e.masks.get(index).ok_or_else(|| bad("subject.inspect", "unknown mask"))?;
    let alpha = mask.alpha.as_ref().ok_or_else(|| bad("subject.inspect", "selected mask has no subject alpha"))?;
    alpha.validate().map_err(|e| bad("subject.inspect", e))?;
    let mut keys = Vec::new();
    for frame in &alpha.frames {
        keys.push(json!({"mediaTime":frame.time.0,"width":frame.width,"height":frame.height,"confidence":frame.confidence,"failed":frame.failed,"corrected":frame.corrected,"centroidAndArea":frame.centroid().map_err(|e|bad("subject.inspect",e))?,"runs":frame.runs.len()}));
    }
    let preview = if let Some(time) = p.media_time {
        let frame = alpha.frame_at(Tick(time)).ok_or_else(|| bad("subject.inspect", "mediaTime is outside tracked interval"))?;
        let pixels = frame.decode().map_err(|e| bad("subject.inspect", e))?;
        let mut image = filmcraft_render::Image::new(usize::from(frame.width), usize::from(frame.height));
        for (pixel, &alpha) in image.px.as_chunks_mut::<4>().0.iter_mut().zip(&pixels) {
            let value = f32::from(alpha) / 255.0;
            *pixel = [value, value, value, 1.0];
        }
        Some(
            json!({"mimeType":"image/png","base64":base64::engine::general_purpose::STANDARD.encode(crate::inspection::png_image(&image)?),"sourceWidth":alpha.source_width,"sourceHeight":alpha.source_height,"mediaTime":frame.time.0,"failed":frame.failed}),
        )
    } else {
        None
    };
    Ok(
        json!({"revision":s.revision,"sequence":seq.0,"clip":it.id.0,"effectInstance":p.address.effect_instance,"mask":index,"name":mask.name,"mode":mask.mode,"method":alpha.method,"provenance":alpha.provenance,"end":alpha.end.0,"frames":keys,"alpha":if p.include_data{Some(alpha)}else{None},"preview":preview}),
    )
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Install {
    #[serde(flatten)]
    address: Address,
    expected_revision: u64,
    alpha: AlphaTrack,
    name: Option<String>,
    #[serde(default)]
    replace: bool,
}
fn install(s: &mut Session, p: &Value) -> Result<Value> {
    let p: Install = bounded_decode(p, "subject.installAlpha")?;
    if p.expected_revision != s.revision {
        return Err(bad("subject.installAlpha", "stale revision"));
    }
    p.alpha.validate().map_err(|e| bad("subject.installAlpha", e))?;
    let (seq, it, effect) = locate(s, &p.address)?;
    if s.project.sequence(seq).and_then(|q| q.find_item(it.id).and_then(|(t, _)| q.track(t))).is_some_and(|t| t.locked) {
        return Err(bad("subject.installAlpha", "target track is locked"));
    }
    let (_, media, _) = s.project.resolve_media(it.item).ok_or_else(|| bad("subject.installAlpha", "source does not resolve to media"))?;
    let video = media.info.video.as_ref().ok_or_else(|| bad("subject.installAlpha", "source has no video"))?;
    if (p.alpha.source_width, p.alpha.source_height) != (video.width, video.height) || p.alpha.end > media.duration() {
        return Err(bad("subject.installAlpha", "alpha dimensions/range do not match the source"));
    }
    let e = it.effects.get(effect).ok_or_else(|| bad("subject.installAlpha", "effect vanished"))?;
    if let Some(mask) = p.address.mask {
        let m = e.masks.get(mask).ok_or_else(|| bad("subject.installAlpha", "unknown mask"))?;
        if m.alpha.is_some() && !p.replace {
            return Err(bad("subject.installAlpha", "replacing existing subject alpha needs replace:true"));
        }
    } else if e.masks.len() >= 64 {
        return Err(bad("subject.installAlpha", "at most 64 masks per effect"));
    }
    let name = p.name.unwrap_or_else(|| "Tracked subject".into());
    if name.is_empty() || name.len() > 512 {
        return Err(bad("subject.installAlpha", "name must contain 1..512 bytes"));
    }
    let clip = ClipId(p.address.clip);
    let requested = p.address.mask;
    let count = p.alpha.frames.len();
    let mask = s.edit("Install reviewed subject alpha", move |project, _| {
        let q = project.sequence_mut(seq).ok_or(crate::EngineError::NoSequence)?;
        let (_, it) = q.find_item_mut(clip).ok_or_else(|| bad("subject.installAlpha", "clip vanished"))?;
        let e = it.effects.get_mut(effect).ok_or_else(|| bad("subject.installAlpha", "effect vanished"))?;
        let index = if let Some(index) = requested {
            index
        } else {
            let mut mask = Mask::new(name, MaskPath::default());
            mask.feather.value = ParamValue::Float(0.0);
            e.masks.push(mask);
            e.masks.len().saturating_sub(1)
        };
        let mask = e.masks.get_mut(index).ok_or_else(|| bad("subject.installAlpha", "mask vanished"))?;
        mask.alpha = Some(p.alpha);
        Ok(index)
    })?;
    Ok(json!({"sequence":seq.0,"clip":clip.0,"effectInstance":p.address.effect_instance,"mask":mask,"frames":count,"revision":s.revision}))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrackRequest {
    #[serde(flatten)]
    address: Address,
    prompt: Prompt,
    start: Option<i64>,
    end: Option<i64>,
    max_frames: Option<usize>,
    stride_frames: Option<u32>,
    max_side: Option<u32>,
    minimum_confidence: Option<f32>,
    #[serde(default)]
    replace: bool,
    name: Option<String>,
}
fn frame_count(target: &Target, a: Tick, b: Tick, max: usize, stride: Option<u32>) -> Result<(i64, i64, i64)> {
    if !(1..=MAX_SAMPLES).contains(&max) {
        return Err(bad("subject.trackPlan", "maxFrames must be 1..240"));
    }
    let first = target.rate().frame_at(a);
    let last = target.rate().frame_at(b - Tick(1));
    let count = last.checked_sub(first).and_then(|n| n.checked_add(1)).ok_or_else(|| bad("subject.trackPlan", "frame range overflow"))?;
    let stride = stride.unwrap_or(1);
    if stride == 0 {
        return Err(bad("subject.trackPlan", "strideFrames must be positive"));
    }
    let stride = i64::from(stride);
    let count = count.saturating_add(stride - 1) / stride;
    if usize::try_from(count).ok().is_none_or(|n| n > max) {
        return Err(bad("subject.trackPlan", "range exceeds maxFrames; shorten it or explicitly increase strideFrames"));
    }
    Ok((first, count, stride))
}
fn merged_track(it: &TrackItem, effect: usize, mask: Option<usize>, mut alpha: AlphaTrack, replace: bool) -> Result<AlphaTrack> {
    if let Some(mask) = mask {
        let existing = it.effects.get(effect).and_then(|e| e.masks.get(mask)).ok_or_else(|| bad("subject.trackPlan", "unknown mask"))?;
        if let Some(old) = &existing.alpha {
            if (old.source_width, old.source_height) != (alpha.source_width, alpha.source_height) {
                return Err(bad("subject.trackPlan", "source dimensions changed; create a fresh subject mask"));
            }
            if !replace {
                return Err(bad("subject.trackPlan", "updating an existing alpha needs replace:true"));
            }
            let first = alpha.frames.first().map_or(Tick::ZERO, |f| f.time);
            let end = alpha.end;
            let mut keys: BTreeMap<_, _> = old.frames.iter().filter(|f| f.time < first || f.time >= end).map(|f| (f.time, f.clone())).collect();
            for frame in alpha.frames {
                keys.insert(frame.time, frame);
            }
            alpha.frames = keys.into_values().collect();
            alpha.end = alpha.end.max(old.end);
        }
    }
    alpha.validate().map_err(|e| bad("subject.trackPlan", e))?;
    Ok(alpha)
}
fn track_plan(s: &mut Session, p: &Value) -> Result<Value> {
    let p: TrackRequest = bounded_decode(p, "subject.trackPlan")?;
    p.prompt.validate().map_err(|e| bad("subject.trackPlan", e))?;
    let (seq, it, effect) = locate(s, &p.address)?;
    let it = it.clone();
    let target = Target::resolve(s, &SubjectArgs { target: Some("clip".into()), sequence: Some(seq.0), clip: Some(it.id.0), ..Default::default() })?;
    let (a, b) = target.window(p.start, p.end)?;
    let (first, count, stride) = frame_count(&target, a, b, p.max_frames.unwrap_or(120), p.stride_frames)?;
    let side = p.max_side.unwrap_or(128);
    if !(32..=256).contains(&side) {
        return Err(bad("subject.trackPlan", "maxSide must be 32..256"));
    }
    let source = crate::inspection::subject(s, &json!({"target":"source","item":it.item.0,"effects":"pre"}), false)?;
    let scale = source.scale(&json!({"maxSide":side}), 1)?;
    let confidence = p.minimum_confidence.unwrap_or(0.35);
    let mut tracker = None;
    let mut keys = BTreeMap::new();
    let mut readings = Vec::new();
    let mut failures = Vec::new();
    let mut open_failure = None;
    let mut last_timeline = a;
    for index in 0..count {
        let frame = first
            .checked_add(index.checked_mul(stride).ok_or_else(|| bad("subject.trackPlan", "frame offset overflow"))?)
            .ok_or_else(|| bad("subject.trackPlan", "frame offset overflow"))?;
        let time = target.rate().tick_of(frame).max(a);
        last_timeline = time;
        let media_time = source.validate_time(it.source_time_at(time))?;
        if keys.contains_key(&media_time) {
            continue;
        }
        let (image, offline) = source.render(s, media_time, scale, false)?;
        if offline.as_array().is_some_and(|v| !v.is_empty()) {
            return Err(bad("subject.trackPlan", "source is offline or cannot decode"));
        }
        if tracker.is_none() {
            tracker = Some(Tracker::new(&image, &p.prompt, confidence).map_err(|e| bad("subject.trackPlan", e))?);
        }
        let tracker = tracker.as_mut().ok_or_else(|| bad("subject.trackPlan", "tracker unavailable"))?;
        let tracked = tracker.step(&image).map_err(|e| bad("subject.trackPlan", e))?;
        if index == 0 && tracked.failed {
            return Err(bad("subject.trackPlan", "seed frame has no separable subject; tighten the box/click or provide correction points"));
        }
        if tracked.failed && open_failure.is_none() {
            open_failure = Some(time);
        } else if !tracked.failed
            && let Some(start) = open_failure.take()
        {
            failures.push(json!({"start":target.position(start),"end":target.position(time)}));
        }
        readings.push(json!({"position":target.position(time),"sourceTime":media_time.0,"confidence":tracked.confidence,"failed":tracked.failed,"reacquired":tracked.reacquired,"centroid":tracked.centroid,"visibleFraction":tracked.visible_fraction}));
        let width = u16::try_from(tracked.width).map_err(|_| bad("subject.trackPlan", "raster width overflow"))?;
        let height = u16::try_from(tracked.height).map_err(|_| bad("subject.trackPlan", "raster height overflow"))?;
        keys.insert(
            media_time,
            AlphaFrame::encode(media_time, width, height, &tracked.alpha, tracked.confidence, tracked.failed, false)
                .map_err(|e| bad("subject.trackPlan", e))?,
        );
    }
    if let Some(start) = open_failure {
        failures.push(json!({"start":target.position(start),"end":target.position((last_timeline+target.rate().tick_of(stride)).min(b))}));
    }
    let end = keys
        .last_key_value()
        .map(|(t, _)| (*t + source.rate.frame_duration()).min(source.end_time.unwrap_or(Tick::MAX)))
        .ok_or_else(|| bad("subject.trackPlan", "no frames were tracked"))?;
    let alpha=AlphaTrack{source_width:source.settings.width,source_height:source.settings.height,end,method:"seeded_color_graph_v1".into(),provenance:"Original FilmCraft local color prototypes, edge-weighted Potts ICM, connected components and motion/area prior; no downloaded model or semantic person recognition".into(),frames:keys.into_values().collect()};
    let alpha = merged_track(&it, effect, p.address.mask, alpha, p.replace)?;
    let mut plan=s.execute("agent.plan",json!({"label":"Track subject alpha","baseRevision":s.revision,"steps":[{"id":"subject.installAlpha","params":{"clip":it.id.0,"sequence":seq.0,"effectInstance":p.address.effect_instance,"mask":p.address.mask,"alpha":alpha,"replace":p.replace,"name":p.name}}]}))?;
    plan["subjectTracking"] = json!({"method":"seeded_color_graph_v1","readings":readings,"failedRanges":failures,"strideFrames":stride,"temporalInterpolation":"hold preceding sampled alpha; dense stride 1 for every displayed frame","limitations":"Similar colors, camouflage, lighting changes and lookalike subjects can require correction. Confidence and failed ranges require review."});
    Ok(plan)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CorrectRequest {
    #[serde(flatten)]
    address: Address,
    media_time: i64,
    prompt: Prompt,
    max_side: Option<u32>,
    minimum_confidence: Option<f32>,
}
fn correct_plan(s: &mut Session, p: &Value) -> Result<Value> {
    let p: CorrectRequest = bounded_decode(p, "subject.correctPlan")?;
    let (seq, it, effect) = locate(s, &p.address)?;
    let index = p.address.mask.ok_or_else(|| bad("subject.correctPlan", "mask index required"))?;
    let mut alpha = it
        .effects
        .get(effect)
        .and_then(|e| e.masks.get(index))
        .and_then(|m| m.alpha.clone())
        .ok_or_else(|| bad("subject.correctPlan", "selected mask has no subject alpha"))?;
    let source = crate::inspection::subject(s, &json!({"target":"source","item":it.item.0,"effects":"pre"}), false)?;
    let time = source.validate_time(Tick(p.media_time))?;
    let side = p.max_side.unwrap_or(128);
    if !(32..=256).contains(&side) {
        return Err(bad("subject.correctPlan", "maxSide must be 32..256"));
    }
    let (image, offline) = source.render(s, time, source.scale(&json!({"maxSide":side}), 1)?, false)?;
    if offline.as_array().is_some_and(|v| !v.is_empty()) {
        return Err(bad("subject.correctPlan", "source is offline"));
    }
    let mut tracker = Tracker::new(&image, &p.prompt, p.minimum_confidence.unwrap_or(0.35)).map_err(|e| bad("subject.correctPlan", e))?;
    let result = tracker.step(&image).map_err(|e| bad("subject.correctPlan", e))?;
    if result.failed {
        return Err(bad("subject.correctPlan", "correction did not isolate a separable subject"));
    }
    let frame = AlphaFrame::encode(time, result.width as u16, result.height as u16, &result.alpha, result.confidence, false, true)
        .map_err(|e| bad("subject.correctPlan", e))?;
    alpha.frames.retain(|f| f.time != time);
    alpha.frames.push(frame);
    alpha.frames.sort_by_key(|f| f.time);
    alpha.end = alpha.end.max((time + source.rate.frame_duration()).min(source.end_time.unwrap_or(Tick::MAX)));
    alpha.validate().map_err(|e| bad("subject.correctPlan", e))?;
    s.execute("agent.plan",json!({"label":"Correct subject alpha keyframe","baseRevision":s.revision,"steps":[{"id":"subject.installAlpha","params":{"clip":p.address.clip,"sequence":seq.0,"effectInstance":p.address.effect_instance,"mask":index,"alpha":alpha,"replace":true}}]}))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReframeRequest {
    sequence: Option<u64>,
    clips: Vec<u64>,
    width: u32,
    height: u32,
    name: Option<String>,
    minimum_confidence: Option<f32>,
    #[serde(default)]
    replace_motion: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReframeInstall {
    clip: u64,
    sequence: Option<u64>,
    expected_revision: u64,
    minimum_confidence: f32,
    #[serde(default)]
    replace_motion: bool,
}
fn alpha_for(it: &TrackItem) -> Option<(usize, usize, &AlphaTrack)> {
    it.effects.iter().enumerate().find_map(|(e, effect)| effect.masks.iter().enumerate().find_map(|(m, mask)| mask.alpha.as_ref().map(|a| (e, m, a))))
}
fn install_reframe(s: &mut Session, p: &Value) -> Result<Value> {
    let p: ReframeInstall = bounded_decode(p, "subject.installReframe")?;
    if p.expected_revision != s.revision || !p.minimum_confidence.is_finite() || !(0.0..=1.0).contains(&p.minimum_confidence) {
        return Err(bad("subject.installReframe", "stale revision or invalid confidence"));
    }
    let id = p.sequence.map(ItemId).or(s.state.active_sequence).ok_or(crate::EngineError::NoSequence)?;
    let seq = s.project.sequence(id).ok_or(crate::EngineError::NoSequence)?;
    let (track, it) = seq.find_item(ClipId(p.clip)).ok_or_else(|| bad("subject.installReframe", "clip unavailable"))?;
    if seq.track(track).is_some_and(|t| t.locked) {
        return Err(bad("subject.installReframe", "track is locked"));
    }
    let (ei, mi, alpha) = alpha_for(it).ok_or_else(|| bad("subject.installReframe", "clip needs reviewed subject alpha"))?;
    alpha.validate().map_err(|e| bad("subject.installReframe", e))?;
    let motion = it.effect("motion").ok_or_else(|| bad("subject.installReframe", "clip has no Motion effect"))?;
    if motion.params.values().any(|p| p.is_animated()) && !p.replace_motion {
        return Err(bad("subject.installReframe", "replacing animated Motion needs replaceMotion:true"));
    }
    let (_, media, _) = s.project.resolve_media(it.item).ok_or_else(|| bad("subject.installReframe", "clip source unavailable"))?;
    let video = media.info.video.as_ref().ok_or_else(|| bad("subject.installReframe", "source has no video"))?;
    if (video.width, video.height) != (alpha.source_width, alpha.source_height) || alpha.end > media.duration() {
        return Err(bad("subject.installReframe", "source metadata changed; retrack the subject before reframing"));
    }
    let par = filmcraft_project::par_ratio(media.pixel_aspect());
    let width = f64::from(alpha.source_width);
    let height = f64::from(alpha.source_height);
    let sw = f64::from(seq.settings.width);
    let sh = f64::from(seq.settings.height);
    let scale = (sw / (width * par)).max(sh / height);
    if !scale.is_finite() || scale > 100.0 {
        return Err(bad("subject.installReframe", "framing would exceed Motion's 10000 percent scale limit"));
    }
    let bounded = |value: f64, lo: f64, hi: f64, center: f64| if lo <= hi { value.clamp(lo, hi) } else { center };
    let mut keys = Vec::new();
    for frame in alpha.frames.iter().filter(|f| !f.failed && f.confidence >= p.minimum_confidence) {
        if let Some((center, _)) = frame.centroid().map_err(|e| bad("subject.installReframe", e))? {
            let x = bounded(sw / 2.0 - (center[0] * width - width / 2.0) * scale * par, sw - width * scale * par / 2.0, width * scale * par / 2.0, sw / 2.0);
            let y = bounded(sh / 2.0 - (center[1] * height - height / 2.0) * scale, sh - height * scale / 2.0, height * scale / 2.0, sh / 2.0);
            keys.push((frame.time, Vec2::new(x, y)));
        }
    }
    if keys.is_empty() {
        return Err(bad("subject.installReframe", "no confident alpha keyframes for reframing"));
    }
    let count = keys.len();
    s.edit("Apply subject framing", move |project, _| {
        let q = project.sequence_mut(id).ok_or(crate::EngineError::NoSequence)?;
        q.settings.par = (1, 1);
        let (_, it) = q.find_item_mut(ClipId(p.clip)).ok_or_else(|| bad("subject.installReframe", "clip vanished"))?;
        it.scale_to_frame = false;
        let motion = it.effect_mut("motion").ok_or_else(|| bad("subject.installReframe", "Motion vanished"))?;
        for param in motion.params.values_mut() {
            param.keyframes.clear();
        }
        for (name, value) in [
            ("scale", ParamValue::Float(scale * 100.0)),
            ("scale_width", ParamValue::Float(scale * 100.0)),
            ("uniform_scale", ParamValue::Bool(true)),
            ("rotation", ParamValue::Float(0.0)),
            ("anchor", ParamValue::Vec2(Vec2::new(width / 2.0, height / 2.0))),
        ] {
            if let Some(p) = motion.params.get_mut(name) {
                p.value = value;
            }
        }
        let position = motion.params.get_mut("position").ok_or_else(|| bad("subject.installReframe", "Motion position unavailable"))?;

        for (time, point) in keys {
            position.put_keyframe(time, ParamValue::Vec2(point));
        }
        if let Some(mask) = it.effects.get_mut(ei).and_then(|e| e.masks.get_mut(mi)) {
            mask.mode = MaskMode::None;
        }
        Ok(())
    })?;
    Ok(
        json!({"clip":p.clip,"sequence":id.0,"positionKeyframes":count,"scalePercent":scale*100.0,"subjectMask":"retained for review; disabled in this framing variant to keep the background"}),
    )
}
fn reframe_plan(s: &mut Session, p: &Value) -> Result<Value> {
    let p: ReframeRequest = bounded_decode(p, "subject.reframePlan")?;
    if p.clips.is_empty() || p.clips.len() > 64 || p.clips.iter().collect::<BTreeSet<_>>().len() != p.clips.len() {
        return Err(bad("subject.reframePlan", "choose 1..64 unique clip identities"));
    }
    let id = p.sequence.map(ItemId).or(s.state.active_sequence).ok_or(crate::EngineError::NoSequence)?;
    let seq = s.project.sequence(id).ok_or(crate::EngineError::NoSequence)?;
    let mut settings = seq.settings.clone();
    settings.width = p.width;
    settings.height = p.height;
    settings.par = (1, 1);
    settings.validate().map_err(|e| bad("subject.reframePlan", e))?;
    let name = p.name.unwrap_or_else(|| format!("Subject framing {}x{}", p.width, p.height));
    if name.is_empty() || name.len() > 512 {
        return Err(bad("subject.reframePlan", "name must contain 1..512 bytes"));
    }
    let minimum = p.minimum_confidence.unwrap_or(0.5);
    if !minimum.is_finite() || !(0.0..=1.0).contains(&minimum) {
        return Err(bad("subject.reframePlan", "minimumConfidence must be finite 0..1"));
    }
    for &clip in &p.clips {
        let (_, it) = seq.find_item(ClipId(clip)).ok_or_else(|| bad("subject.reframePlan", "unknown clip"))?;
        if alpha_for(it).is_none() {
            return Err(bad("subject.reframePlan", "each clip needs reviewed subject alpha"));
        }
    }
    let mut steps = vec![
        json!({"id":"project.select","params":{"items":[id.0]}}),
        json!({"id":"edit.duplicate","params":{}}),
        json!({"id":"sequence.open","params":{"item":{"$step":1,"path":"/items/0"}}}),
        json!({"id":"sequence.settings","params":{"width":p.width,"height":p.height,"name":name}}),
    ];
    for clip in p.clips {
        steps.push(json!({"id":"subject.installReframe","params":{"clip":clip,"minimumConfidence":minimum,"replaceMotion":p.replace_motion}}));
    }
    s.execute("agent.plan", json!({"label":name,"baseRevision":s.revision,"steps":steps}))
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "subject.inspect",
            label: "Inspect subject alpha evidence",
            menu: &[],
            shortcut: None,
            params: r#"{"clip":id,"sequence":id?,"effectInstance":"opacity:0","mask":n?,"includeData":bool=false,"mediaTime":ticks?}"#,
            enabled: always,
            run: inspect,
            journal: false,
        },
        CommandSpec {
            id: "subject.trackPlan",
            label: "Propose tracked subject alpha",
            menu: &[],
            shortcut: None,
            params: r#"{"clip":id,"sequence":id?,"effectInstance":"opacity:0","mask":n?,"prompt":{"rect":[x,y,w,h]|"point":[x,y],"foreground":[[x,y]],"background":[[x,y]]},"start":ticks?,"end":ticks?,"strideFrames":n=1,"maxFrames":1..240=120,"maxSide":32..256=128,"minimumConfidence":0..1=.35,"replace":bool=false,"name":str?}"#,
            enabled: always,
            run: track_plan,
            journal: false,
        },
        CommandSpec {
            id: "subject.correctPlan",
            label: "Propose a corrected subject alpha keyframe",
            menu: &[],
            shortcut: None,
            params: r#"{"clip":id,"sequence":id?,"effectInstance":"opacity:0","mask":n,"mediaTime":ticks,"prompt":Prompt,"maxSide":32..256=128,"minimumConfidence":0..1=.35}"#,
            enabled: always,
            run: correct_plan,
            journal: false,
        },
        CommandSpec {
            id: "subject.reframePlan",
            label: "Propose a subject framing variant",
            menu: &[],
            shortcut: None,
            params: r#"{"sequence":id?,"clips":[id],"width":u32,"height":u32,"name":str?,"minimumConfidence":0..1=.5,"replaceMotion":bool=false}"#,
            enabled: always,
            run: reframe_plan,
            journal: false,
        },
        CommandSpec {
            id: "subject.installAlpha",
            label: "Install reviewed source alpha",
            menu: &[],
            shortcut: None,
            params: r#"{"clip":id,"sequence":id?,"effectInstance":"opacity:0","mask":n?,"expectedRevision":n,"alpha":AlphaTrack,"replace":bool=false,"name":str?}"#,
            enabled: always,
            run: install,
            journal: true,
        },
        CommandSpec {
            id: "subject.installReframe",
            label: "Apply reviewed subject framing",
            menu: &[],
            shortcut: None,
            params: r#"{"clip":id,"sequence":id?,"expectedRevision":n,"minimumConfidence":0..1,"replaceMotion":bool=false}"#,
            enabled: always,
            run: install_reframe,
            journal: true,
        },
    ]
}
#[cfg(test)]
mod tests;
