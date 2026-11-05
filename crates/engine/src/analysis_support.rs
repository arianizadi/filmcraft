//! Shared bounded snapshot and strict native-audio access for analysis, search and QC.
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::{FrameRequest, MediaInfo, MediaSource, SharedSource};
use filmcraft_project::{ClipId, ItemId, ItemKind, Project, SequenceSettings, TrackItem};
use filmcraft_render::SourceProvider;
use filmcraft_time::{FrameRate, Tick};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Result, Session, commands::bad};

pub(crate) const MAX_SECONDS: i64 = 300;
pub(crate) const AUDIO_RATE: u32 = 16_000;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubjectArgs {
    pub target: Option<String>,
    pub item: Option<u64>,
    pub clip: Option<u64>,
    pub sequence: Option<u64>,
    pub plan: Option<u64>,
    pub effects: Option<String>,
}

pub(crate) struct Target {
    pub project: Arc<Project>,
    pub revision: u64,
    pub args: SubjectArgs,
    pub item: Option<ItemId>,
    pub sequence: Option<ItemId>,
    pub clip: Option<TrackItem>,
    pub settings: SequenceSettings,
    pub first: Tick,
    pub end: Tick,
    pub video: bool,
    pub audio: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePosition {
    pub item: u64,
    pub clip: Option<u64>,
    pub time: i64,
    pub frame: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelinePosition {
    pub sequence: u64,
    pub clip: Option<u64>,
    pub time: i64,
    pub frame: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub time: i64,
    pub frame: i64,
    pub sources: Vec<SourcePosition>,
    pub timeline: Vec<TimelinePosition>,
    pub mapping_truncated: bool,
}

impl Target {
    pub fn resolve(s: &Session, args: &SubjectArgs) -> Result<Self> {
        if args.effects.as_deref().is_some_and(|e| !matches!(e, "pre" | "post")) {
            return Err(bad("analysis", "effects must be pre or post"));
        }
        let (project, state, revision) =
            if let Some(plan) = args.plan { s.agent_plan_snapshot(plan, false)? } else { (s.project.clone(), s.state.clone(), s.revision) };
        let kind = args.target.as_deref().unwrap_or(if args.item.is_some() {
            "source"
        } else if args.clip.is_some() {
            "clip"
        } else {
            "sequence"
        });
        if kind == "source" {
            if args.clip.is_some() || args.sequence.is_some() {
                return Err(bad("analysis", "source subject takes item only"));
            }
            let id = ItemId(args.item.ok_or_else(|| bad("analysis", "source subject needs item"))?);
            let (_, media, _) = project.resolve_media(id).ok_or_else(|| bad("analysis", "source item does not resolve to media"))?;
            let (first, end) = match project.item(id).map(|i| &i.kind) {
                Some(ItemKind::Subclip { range, .. }) => (range.start, range.end()),
                _ => (Tick::ZERO, media.duration()),
            };
            let settings = crate::commands::default_seq_settings_for(media);
            settings.validate().map_err(|e| bad("analysis", e))?;
            return Ok(Self {
                project: project.clone(),
                revision,
                args: args.clone(),
                item: Some(id),
                sequence: None,
                clip: None,
                settings,
                first,
                end,
                video: media.info.has_video(),
                audio: media.info.has_audio(),
            });
        }
        if !matches!(kind, "sequence" | "composite" | "clip") || args.item.is_some() {
            return Err(bad("analysis", "target must be source, clip or sequence"));
        }
        let id = args.sequence.map(ItemId).or(state.active_sequence).ok_or(crate::EngineError::NoSequence)?;
        let seq = project.sequence(id).ok_or_else(|| bad("analysis", "unknown sequence"))?;
        seq.settings.validate().map_err(|e| bad("analysis", e))?;
        let settings = seq.settings.clone();
        let (clip, first, end, video, audio) = if kind == "clip" {
            let clip = ClipId(args.clip.ok_or_else(|| bad("analysis", "clip subject needs clip"))?);
            let (track, it) = seq.find_item(clip).ok_or_else(|| bad("analysis", "unknown clip in requested sequence"))?;
            let (v, a) = project.source_streams(it.item);
            (Some(it.clone()), it.start, it.end(), v && seq.video_tracks.iter().any(|t| t.id == track), a)
        } else {
            if args.clip.is_some() {
                return Err(bad("analysis", "clip ID requires clip target"));
            }
            (None, Tick::ZERO, seq.duration(), !seq.video_tracks.is_empty(), !seq.audio_tracks.is_empty())
        };
        Ok(Self { project, revision, args: args.clone(), item: None, sequence: Some(id), clip, settings, first, end, video, audio })
    }
    pub fn window(&self, start: Option<i64>, end: Option<i64>) -> Result<(Tick, Tick)> {
        let a = start.map(Tick).unwrap_or(self.first);
        let b = end.map(Tick).unwrap_or(self.end);
        if a < Tick::ZERO || a < self.first || b > self.end || b > Tick::MAX || b <= a {
            return Err(bad("analysis", "range must be a positive half-open interval inside the subject"));
        }
        if b.0.checked_sub(a.0).is_none_or(|d| d > Tick::from_units(MAX_SECONDS, 1).0) {
            return Err(bad("analysis", format!("analyze at most {MAX_SECONDS} seconds per request; use bounded windows")));
        }
        Ok((a, b))
    }
    pub fn rate(&self) -> FrameRate {
        self.settings.frame_rate
    }
    pub fn video_subject(&self, s: &Session) -> Result<crate::inspection::Subject> {
        let mut p = serde_json::to_value(&self.args).map_err(|e| bad("analysis", e.to_string()))?;
        if let Some(o) = p.as_object_mut() {
            o.retain(|_, v| !v.is_null());
        }
        crate::inspection::subject(s, &p, false)
    }
    pub fn position(&self, t: Tick) -> Position {
        let mut sources = Vec::new();
        let mut timeline = Vec::new();
        let mut truncated = false;
        if let Some(id) = self.item {
            sources.push(SourcePosition { item: id.0, clip: None, time: t.0, frame: self.rate().frame_at(t) });
            for seq_item in self.project.sequences() {
                if let Some(q) = seq_item.as_sequence() {
                    for it in q.all_tracks().flat_map(|track| &track.items).filter(|it| it.item == id) {
                        if let Some(time) = source_to_timeline(it, t) {
                            if timeline.len() >= 32 {
                                truncated = true;
                                break;
                            }
                            timeline.push(TimelinePosition {
                                sequence: seq_item.id.0,
                                clip: Some(it.id.0),
                                time: time.0,
                                frame: q.settings.frame_rate.frame_at(time),
                            });
                        }
                    }
                }
                if truncated {
                    break;
                }
            }
        } else if let Some(id) = self.sequence {
            timeline.push(TimelinePosition { sequence: id.0, clip: self.clip.as_ref().map(|c| c.id.0), time: t.0, frame: self.rate().frame_at(t) });
            if let Some(q) = self.project.sequence(id) {
                for it in
                    q.all_tracks().flat_map(|track| &track.items).filter(|it| self.clip.as_ref().map_or_else(|| it.range().contains(t), |c| it.id == c.id))
                {
                    if sources.len() >= 32 {
                        truncated = true;
                        break;
                    }
                    let time = it.source_time_at(t);
                    let rate = self.project.resolve_media(it.item).map_or(self.rate(), |(_, m, _)| m.frame_rate());
                    sources.push(SourcePosition { item: it.item.0, clip: Some(it.id.0), time: time.0, frame: rate.frame_at(time) });
                }
            }
        }
        Position { time: t.0, frame: self.rate().frame_at(t), sources, timeline, mapping_truncated: truncated }
    }
    pub fn audio_reader(&self, s: &Session) -> StrictProvider {
        StrictProvider::new(s, self.project.clone())
    }
    pub fn audio_block(&self, provider: &StrictProvider, start: Tick, frames: usize, rate: u32) -> Result<AudioBuffer> {
        filmcraft_media::validate_audio_buffer_shape(2, frames)?;
        let at = start.to_units_floor(i64::from(rate));
        let count = i64::try_from(frames).map_err(|_| bad("analysis", "audio length overflow"))?;
        at.checked_add(count).ok_or_else(|| bad("analysis", "audio position overflow"))?;
        let out = if let Some(id) = self.item {
            let source = provider.source(id).ok_or_else(|| bad("analysis", "source audio unavailable"))?;
            source.audio(at, frames, rate)?
        } else if let Some(it) = &self.clip {
            let mut it = it.clone();
            if self.args.effects.as_deref() == Some("pre") {
                it.effects.clear();
                it.gain_db = 0.0;
            }
            let channels =
                filmcraft_render::audio::clip_signal(&it, at, frames, rate, provider).ok_or_else(|| bad("analysis", "clip has no analyzable audio"))?;
            AudioBuffer { sample_rate: rate, channels: channels.into_iter().collect() }
        } else {
            let id = self.sequence.ok_or(crate::EngineError::NoSequence)?;
            let mut seq = self.project.sequence(id).ok_or(crate::EngineError::NoSequence)?.clone();
            seq.settings.sample_rate = rate;
            if self.args.effects.as_deref() == Some("pre") {
                seq.master_effects.clear();
                for track in seq.all_tracks_mut() {
                    track.effects.clear();
                    for it in &mut track.items {
                        it.effects.clear();
                        it.gain_db = 0.0;
                    }
                }
            }
            filmcraft_render::audio::mix_sequence(&self.project, &seq, at, frames, provider)
        };
        provider.check()?;
        if out.sample_rate != rate
            || out.channels.is_empty()
            || out.channels.len() > 64
            || out.channels.iter().any(|c| c.len() != frames || c.iter().any(|v| !v.is_finite()))
        {
            return Err(bad("analysis", "audio decoder returned inconsistent or non-finite samples"));
        }
        Ok(out)
    }
}

/// Invert the editor's monotone Tick mapping without converting edit time through float seconds.
pub(crate) fn source_to_timeline(it: &TrackItem, source: Tick) -> Option<Tick> {
    if it.duration <= Tick::ZERO || !it.speed.is_finite() || it.speed == 0.0 {
        return None;
    }
    let mut lo = it.start.0;
    let mut hi = it.end().0.checked_sub(1)?;
    let first = it.source_time_at(Tick(lo));
    let last = it.source_time_at(Tick(hi));
    if source < first.min(last) || source > first.max(last) {
        return None;
    }
    let forward = last >= first;
    for _ in 0..64 {
        if lo >= hi {
            break;
        }
        let mid = lo.checked_add(hi.checked_sub(lo)? / 2)?;
        let value = it.source_time_at(Tick(mid));
        if (forward && value < source) || (!forward && value > source) {
            lo = mid.checked_add(1)?;
        } else {
            hi = mid;
        }
    }
    Some(Tick(lo))
}

pub(crate) struct StrictProvider {
    inner: crate::media_pool::PoolProvider,
    errors: Arc<Mutex<Vec<String>>>,
}
impl StrictProvider {
    fn new(s: &Session, project: Arc<Project>) -> Self {
        Self { inner: s.media.full_res_provider(project, s.services.clone()).own_sources().recording(), errors: Arc::default() }
    }
    pub fn check(&self) -> Result<()> {
        let errors = self.errors.lock().unwrap_or_else(PoisonError::into_inner);
        if !errors.is_empty() {
            return Err(bad("analysis", errors.join("; ")));
        }
        let offline = self.inner.offline_used();
        if !offline.is_empty() {
            return Err(bad("analysis", format!("offline/unreadable media: {}", json!(offline))));
        }
        Ok(())
    }
}
impl SourceProvider for StrictProvider {
    fn source(&self, id: ItemId) -> Option<SharedSource> {
        self.inner.source(id).map(|source| Arc::new(StrictSource { source, errors: self.errors.clone() }) as SharedSource)
    }
}
struct StrictSource {
    source: SharedSource,
    errors: Arc<Mutex<Vec<String>>>,
}
impl StrictSource {
    fn record<T>(&self, result: filmcraft_media::Result<T>) -> filmcraft_media::Result<T> {
        if let Err(error) = &result {
            let mut errors = self.errors.lock().unwrap_or_else(PoisonError::into_inner);
            if errors.len() < 8 {
                errors.push(error.to_string());
            }
        }
        result
    }
}
impl MediaSource for StrictSource {
    fn info(&self) -> &MediaInfo {
        self.source.info()
    }
    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        self.record(self.source.video_frame(req))
    }
    fn video_frame_unturned(&self, req: FrameRequest) -> filmcraft_media::Result<(Arc<VideoFrame>, u8)> {
        self.record(self.source.video_frame_unturned(req))
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        self.record(self.source.audio(start, frames, rate))
    }
    fn audio_stream(&self, stream: usize, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        self.record(self.source.audio_stream(stream, start, frames, rate))
    }
}

pub(crate) fn bounded_decode<T: for<'de> Deserialize<'de>>(p: &Value, command: &str) -> Result<T> {
    let mut stack = vec![(p, 0usize)];
    let (mut nodes, mut bytes) = (0usize, 0usize);
    while let Some((v, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if depth > 32 || nodes > 100_000 {
            return Err(bad(command, "request is too deeply nested or large"));
        }
        match v {
            Value::String(s) => bytes = bytes.saturating_add(s.len()),
            Value::Array(a) => {
                if a.len() > 100_000 {
                    return Err(bad(command, "array exceeds limit"));
                }
                stack.extend(a.iter().map(|v| (v, depth + 1)));
            }
            Value::Object(o) => {
                if o.len() > 4096 {
                    return Err(bad(command, "object exceeds limit"));
                }
                for (k, v) in o {
                    bytes = bytes.saturating_add(k.len());
                    stack.push((v, depth + 1));
                }
            }
            _ => bytes = bytes.saturating_add(16),
        }
        if bytes > 8 * 1024 * 1024 {
            return Err(bad(command, "request exceeds 8 MiB structural budget"));
        }
    }
    serde_json::from_value(p.clone()).map_err(|e| bad(command, e.to_string()))
}
