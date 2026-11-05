//! Timeline findings and independent native delivery-file verification. A structural or sampled
//! check states its coverage. Decode errors and unknown color/caption tags never become passes.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use filmcraft_project::{ItemId, Sequence};
use filmcraft_time::{FrameRate, Tick};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::analysis_support::{SubjectArgs, Target, bounded_decode};
use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};
const MAX_CLIPS: usize = 5000;
const MAX_FINDINGS: usize = 4096;
fn yes() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TimelineRequest {
    sequence: Option<u64>,
    plan: Option<u64>,
    start: Option<i64>,
    end: Option<i64>,
    max_frames: Option<usize>,
    stride_frames: Option<u32>,
    #[serde(default = "yes")]
    video: bool,
    #[serde(default = "yes")]
    audio: bool,
    flash_frames: Option<u32>,
    caption_tolerance_frames: Option<u32>,
    clipping_fraction: Option<f32>,
}
fn finding(target: &Target, kind: &str, a: Tick, b: Tick, confidence: f32, evidence: Value) -> Value {
    json!({"kind":kind,"start":target.position(a),"end":target.position(b),"confidence":confidence,"evidence":evidence})
}
fn structural(target: &Target, seq: &Sequence, a: Tick, b: Tick, flash: u32) -> Result<Vec<Value>> {
    let clips = seq.all_tracks().map(|t| t.items.len()).fold(0usize, usize::saturating_add);
    if clips > MAX_CLIPS {
        return Err(bad("qc.timeline", "at most 5000 clip placements per scan; use a smaller sequence"));
    }
    let mut out = Vec::new();
    let mut coverage = Vec::new();
    for track in &seq.video_tracks {
        if !track.enabled || track.muted {
            continue;
        }
        for it in track.items.iter().filter(|it| it.enabled && it.end() > a && it.start < b) {
            coverage.push((it.start.max(a), it.end().min(b)));
            if it.duration > Tick::ZERO && it.duration <= target.rate().tick_of(i64::from(flash)) {
                out.push(finding(target,"possible_flash_frame",it.start,it.end(),0.8,json!({"clip":it.id.0,"track":track.id.0,"durationFrames":target.rate().frame_at(it.duration),"thresholdFrames":flash,"limitation":"a short clip can be intentional"})));
            }
        }
    }
    coverage.sort_unstable();
    let mut cursor = a;
    for (start, end) in coverage {
        if start > cursor {
            out.push(finding(target,"timeline_black_gap",cursor,start,1.0,json!({"method":"no enabled video placement covers this interval","limitation":"nested transparent media is checked by rendered samples separately"})));
        }
        cursor = cursor.max(end);
    }
    if cursor < b {
        out.push(finding(target, "timeline_black_gap", cursor, b, 1.0, json!({"method":"no enabled video placement covers this interval"})));
    }
    Ok(out)
}
fn offline_media(s: &Session, target: &Target, seq: &Sequence, a: Tick, b: Tick) -> Vec<Value> {
    let mut findings = Vec::new();
    let mut seen = BTreeSet::new();
    for it in seq.all_tracks().filter(|t| t.enabled && !t.muted).flat_map(|t| &t.items).filter(|it| it.enabled && it.end() > a && it.start < b) {
        let Some((root, media, _)) = target.project.resolve_media(it.item) else {
            continue;
        };
        if !seen.insert(root) {
            continue;
        }
        let missing = match &media.media {
            filmcraft_project::MediaRef::File { path } => s.services.file_size(path).err().map(|e| e.to_string()),
            _ => None,
        };
        if media.offline || missing.is_some() {
            findings.push(finding(
                target,
                "missing_or_offline_media",
                it.start.max(a),
                it.end().min(b),
                1.0,
                json!({"item":it.item.0,"clip":it.id.0,"explicitOffline":media.offline,"error":missing}),
            ));
        }
    }
    findings
}
fn word_cuts(target: &Target, seq: &Sequence, a: Tick, b: Tick) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut examined = 0usize;
    let mut seen = BTreeSet::new();
    for it in seq.audio_tracks.iter().filter(|t| t.enabled && !t.muted).flat_map(|t| &t.items).filter(|it| it.enabled && it.end() >= a && it.start <= b) {
        let root = target.project.resolve_media(it.item).map_or(it.item, |r| r.0);
        let Some(transcript) = target.project.transcripts.get(&it.item).or_else(|| target.project.transcripts.get(&root)) else {
            continue;
        };
        examined = examined.saturating_add(transcript.words.len());
        if examined > 2_000_000 {
            return Err(bad("qc.timeline", "word-boundary scan exceeds 2000000 comparisons; narrow the range"));
        }
        for (edge, at) in [("in", it.start), ("out", it.end())] {
            if at < a || at > b {
                continue;
            }
            let source = it.source_time_at(at);
            for (index, word) in transcript.words.iter().enumerate().filter(|(_, w)| w.start < source && w.end > source) {
                let key = (it.item.0, index, at.0);
                if !seen.insert(key) {
                    continue;
                }
                out.push(finding(target,"cut_inside_word",at,at,0.98,json!({"clip":it.id.0,"edge":edge,"sourceItem":it.item.0,"sourceTime":source.0,"word":word.text,"wordStart":word.start.0,"wordEnd":word.end.0,
                    "wordEvidenceId":crate::footage_analysis::evidence_id(it.item.0,index,&word.text,word.start,word.end),"method":"actual source word timing crosses edit boundary"})));
                if out.len() >= MAX_FINDINGS {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}
/// Delivery compares every normalized token. Timeline phrase matching may separately cap its
/// search window, but a shared prefix never certifies the remainder of an exported cue.
fn caption_text_equal(a: &str, b: &str) -> Result<bool> {
    if a.len() > 1024 * 1024 || b.len() > 1024 * 1024 {
        return Err(bad("qc.export", "caption cue exceeds the 1 MiB complete-text verification budget"));
    }
    let a = filmcraft_project::caption::plain_text(a);
    let b = filmcraft_project::caption::plain_text(b);
    Ok(a.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .eq(b.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase)))
}
fn caption_tokens(text: &str) -> Vec<String> {
    filmcraft_project::caption::plain_text(text).split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).take(256).map(str::to_lowercase).collect()
}
fn caption_qc(target: &Target, seq: &Sequence, a: Tick, b: Tick, tolerance: u32) -> Result<(Vec<Value>, Value)> {
    let cues = seq.caption_tracks.iter().map(|t| t.captions.len()).fold(0usize, usize::saturating_add);
    if cues > 5000 {
        return Err(bad("qc.timeline", "caption scan exceeds 5000 cues"));
    }
    let estimated = seq
        .audio_tracks
        .iter()
        .flat_map(|t| &t.items)
        .map(|it| target.project.transcripts.get(&it.item).map_or(0, |t| t.words.len()))
        .fold(0usize, usize::saturating_add);
    if estimated > 100_000 {
        return Err(bad("qc.timeline", "caption scan exceeds 100000 word placements"));
    }
    let words = filmcraft_edit::transcript::sequence_words(seq, &target.project.transcripts);
    let normalized: Vec<_> = words.iter().map(|w| caption_tokens(&w.text).join("")).collect();
    let mut starts = BTreeMap::<&str, Vec<usize>>::new();
    for (index, text) in normalized.iter().enumerate() {
        starts.entry(text).or_default().push(index);
    }
    let mut out = Vec::new();
    let (mut matched, mut unknown, mut comparisons) = (0usize, 0usize, 0usize);
    let threshold = target.rate().tick_of(i64::from(tolerance));
    for track in seq.caption_tracks.iter().filter(|t| t.enabled) {
        for cue in track.captions.iter().filter(|c| c.end() > a && c.start < b) {
            if cue.start < Tick::ZERO || cue.duration <= Tick::ZERO || cue.end() > seq.duration() {
                out.push(finding(
                    target,
                    "caption_out_of_bounds",
                    cue.start,
                    cue.end(),
                    1.0,
                    json!({"caption":cue.id.0,"track":track.id.0,"sequenceDuration":seq.duration().0}),
                ));
            }
            let tokens = caption_tokens(&cue.text);
            let Some(first) = tokens.first() else {
                continue;
            };
            let mut best = None::<(Tick, Tick, i64)>;
            if let Some(indices) = starts.get(first.as_str()) {
                for &index in indices.iter().take(1000) {
                    comparisons = comparisons.saturating_add(tokens.len());
                    if comparisons > 2_000_000 {
                        return Err(bad("qc.timeline", "caption matching comparison budget exceeded"));
                    }
                    let Some(end) = index.checked_add(tokens.len()) else {
                        continue;
                    };
                    let Some(candidate) = normalized.get(index..end) else {
                        continue;
                    };
                    if candidate != tokens {
                        continue;
                    }
                    let Some(start) = words.get(index).map(|w| w.start) else {
                        continue;
                    };
                    let Some(end) = words.get(end.saturating_sub(1)).map(|w| w.end) else {
                        continue;
                    };
                    let distance = (cue.start.0 - start.0).unsigned_abs().saturating_add((cue.end().0 - end.0).unsigned_abs()).min(i64::MAX as u64) as i64;
                    if best.as_ref().is_none_or(|(_, _, d)| distance < *d) {
                        best = Some((start, end, distance));
                    }
                }
            }
            if let Some((start, end, _)) = best {
                matched += 1;
                let delta_start = cue.start - start;
                let delta_end = cue.end() - end;
                if delta_start.0.unsigned_abs() > threshold.0 as u64 || delta_end.0.unsigned_abs() > threshold.0 as u64 {
                    out.push(finding(target,"possible_caption_drift",cue.start,cue.end(),0.85,json!({"caption":cue.id.0,"track":track.id.0,"matchedWordStart":start.0,"matchedWordEnd":end.0,"startDeltaTicks":delta_start.0,"endDeltaTicks":delta_end.0,"toleranceFrames":tolerance,"method":"exact normalized transcript phrase nearest in time","limitation":"intentional cue padding may exceed the tolerance"})));
                }
            } else {
                unknown += 1;
            }
        }
    }
    Ok((
        out,
        json!({"cues":cues,"matchedTranscriptPhrases":matched,"unmatchedOrTranslated":unknown,"status":if words.is_empty(){"transcript_required_for_drift"}else{"checked_exact_phrase_matches"},"limitation":"Unmatched or translated captions are not certified against speech timing."}),
    ))
}
fn edit_clicks(s: &Session, target: &Target, seq: &Sequence, a: Tick, b: Tick) -> Result<Vec<Value>> {
    if !target.audio {
        return Ok(Vec::new());
    }
    let mut edges = BTreeSet::new();
    for it in seq.audio_tracks.iter().filter(|t| t.enabled && !t.muted).flat_map(|t| &t.items).filter(|it| it.enabled) {
        for tick in [it.start, it.end()] {
            if tick >= a && tick < b {
                edges.insert(tick);
            }
        }
    }
    if edges.len() > 1000 {
        return Err(bad("qc.timeline", "at most 1000 audio edit edges per window"));
    }
    let provider = target.audio_reader(s);
    let rate = seq.settings.sample_rate;
    let mut out = Vec::new();
    for time in edges {
        crate::workflow::check_cancelled(s)?;
        let sample = time.to_units_floor(i64::from(rate));
        if sample < 1 {
            continue;
        }
        let at = Tick::from_units(sample - 1, i64::from(rate));
        let buffer = target.audio_block(&provider, at, 2, rate)?;
        for (channel, samples) in buffer.channels.iter().enumerate() {
            let Some((&left, &right)) = samples.first().zip(samples.get(1)) else {
                continue;
            };
            let delta = (right - left).abs();
            if delta > 0.25 {
                out.push(finding(target,"possible_edit_audio_click",time,time,0.75,json!({"channel":channel,"left":left,"right":right,"adjacentSampleDelta":delta,"threshold":0.25,"sampleRate":rate,"method":"native master output on the two samples straddling an edit","limitation":"intentional transients can have discontinuities"})));
            }
        }
    }
    Ok(out)
}
fn timeline(s: &mut Session, p: &Value) -> Result<Value> {
    let p: TimelineRequest = bounded_decode(p, "qc.timeline")?;
    let target = Target::resolve(s, &SubjectArgs { target: Some("sequence".into()), sequence: p.sequence, plan: p.plan, ..Default::default() })?;
    let (a, b) = target.window(p.start, p.end)?;
    let seq = target.sequence.and_then(|id| target.project.sequence(id)).ok_or(crate::EngineError::NoSequence)?;
    let flash = p.flash_frames.unwrap_or(2);
    let tolerance = p.caption_tolerance_frames.unwrap_or(4);
    if flash == 0 || flash > 24 || tolerance > 960 {
        return Err(bad("qc.timeline", "flashFrames 1..24 and captionToleranceFrames 0..960 are supported"));
    }
    let clipping = p.clipping_fraction.unwrap_or(0.01);
    if !clipping.is_finite() || !(0.0..=1.0).contains(&clipping) {
        return Err(bad("qc.timeline", "clippingFraction must be finite 0..1"));
    }
    let mut findings = structural(&target, seq, a, b, flash)?;
    findings.extend(offline_media(s, &target, seq, a, b));
    findings.extend(word_cuts(&target, seq, a, b)?);
    let (caption_findings, captions) = caption_qc(&target, seq, a, b, tolerance)?;
    findings.extend(caption_findings);
    let request:crate::footage_analysis::Request=serde_json::from_value(json!({"subject":target.args,"start":a.0,"end":b.0,"maxFrames":p.max_frames.unwrap_or(120),"strideFrames":p.stride_frames,"video":p.video,"audio":p.audio,"music":false,"waveform":false})).map_err(|e|bad("qc.timeline",e.to_string()))?;
    // An offline decode must still return structural findings. It is an explicit failed media
    // check, rather than a black/silence finding derived from FilmCraft's offline slate.
    let analysis = crate::footage_analysis::analyze(s, &request);
    let coverage = match analysis {
        Ok(report) => {
            for f in report.findings.iter().filter(|f| matches!(f.kind.as_str(), "black_frames" | "possible_audio_click")) {
                findings.push(serde_json::to_value(f).map_err(|e| bad("qc.timeline", e.to_string()))?);
            }
            for row in &report.frames {
                if row.metrics.white_fraction >= clipping {
                    findings.push(finding(&target,"possible_highlight_clipping",Tick(row.position.time),Tick(row.position.time),0.6,json!({"fractionAtDisplayWhite":row.metrics.white_fraction,"threshold":clipping,"colorSpace":"sRGB display transform","limitation":"Display-white pixels are a clipping candidate, not proof of sensor or HDR highlight loss; use scopes.read in the declared working color space."})));
                }
            }
            if p.audio {
                match edit_clicks(s, &target, seq, a, b) {
                    Ok(clicks) => findings.extend(clicks),
                    Err(error) => findings.push(finding(&target, "audio_decode_failed", a, b, 1.0, json!({"error":error.to_string()}))),
                }
            }
            json!({"video":report.video,"audio":report.audio,"decode":"completed"})
        }
        Err(error) => {
            findings.push(finding(
                &target,
                "offline_or_decode_failed",
                a,
                b,
                1.0,
                json!({"error":error.to_string(),"renderedBlackAndSilenceChecks":"not_certified"}),
            ));
            json!({"decode":"failed","error":error.to_string()})
        }
    };
    let truncated = findings.len() > MAX_FINDINGS;
    findings.truncate(MAX_FINDINGS);
    Ok(
        json!({"revision":target.revision,"sequence":target.sequence.map(|id|id.0),"plan":p.plan,"start":target.position(a),"end":target.position(b),"findings":findings,"truncated":truncated,"coverage":coverage,"captions":captions,"projectChanged":false}),
    )
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExportRequest {
    path: String,
    expected: DeliverySpec,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliverySpec {
    duration: Option<i64>,
    frame_rate: Option<FrameRate>,
    width: Option<u32>,
    height: Option<u32>,
    color_space: Option<String>,
    color: Option<filmcraft_color::ColorInfo>,
    audio: Option<AudioSpec>,
    captions: Option<CaptionSpec>,
    tolerance_frames: Option<u32>,
    sequence: Option<u64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AudioSpec {
    present: bool,
    sample_rate: Option<u32>,
    channels: Option<u32>,
    streams: Option<usize>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptionSpec {
    mode: String,
    path: Option<String>,
    format: Option<String>,
    count: Option<usize>,
}
fn check(name: &str, pass: bool, actual: impl serde::Serialize, expected: impl serde::Serialize) -> Value {
    json!({"check":name,"status":if pass{"pass"}else{"fail"},"actual":actual,"expected":expected})
}
fn validate_rate(rate: FrameRate) -> Result<()> {
    filmcraft_project::SequenceSettings { frame_rate: rate, ..Default::default() }.validate().map_err(|e| bad("qc.export", e))
}
fn video_storage_valid(frame: &filmcraft_frame::VideoFrame) -> bool {
    use filmcraft_frame::PixelData;
    let (w, h) = (frame.width as usize, frame.height as usize);
    let Some(pixels) = w.checked_mul(h).filter(|n| *n > 0 && *n <= 33_554_432) else {
        return false;
    };
    if w > 8192 || h > 8192 {
        return false;
    }
    match &frame.data {
        PixelData::Rgba8(v) => v.len() == pixels.saturating_mul(4),
        PixelData::RgbaF32(v) => v.len() == pixels.saturating_mul(4) && v.iter().all(|n| n.is_finite()),
        PixelData::Yuv8 { planes, chroma, alpha } => {
            let (sx, sy) = chroma.shifts();
            let chroma = w.div_ceil(1usize << sx).saturating_mul(h.div_ceil(1usize << sy));
            planes.first().is_some_and(|v| v.len() == pixels)
                && planes.get(1).is_some_and(|v| v.len() == chroma)
                && planes.get(2).is_some_and(|v| v.len() == chroma)
                && alpha.as_ref().is_none_or(|v| v.len() == pixels)
        }
        PixelData::Yuv16 { planes, chroma, bits, alpha } => {
            let (sx, sy) = chroma.shifts();
            let chroma = w.div_ceil(1usize << sx).saturating_mul(h.div_ceil(1usize << sy));
            (9..=16).contains(bits)
                && planes.first().is_some_and(|v| v.len() == pixels)
                && planes.get(1).is_some_and(|v| v.len() == chroma)
                && planes.get(2).is_some_and(|v| v.len() == chroma)
                && alpha.as_ref().is_none_or(|v| v.len() == pixels)
        }
    }
}
fn decode_video(source: &filmcraft_media::SharedSource) -> Vec<Value> {
    let info = source.info();
    let Some(video) = &info.video else {
        return Vec::new();
    };
    let mut checks = Vec::new();
    let mut times = BTreeSet::new();
    for time in [Tick::ZERO, Tick(info.duration.0 / 2), (info.duration - video.frame_rate.frame_duration()).max(Tick::ZERO)] {
        times.insert(video.frame_rate.tick_of(video.frame_rate.frame_at(time)));
    }
    for time in times {
        let side = video.width.max(video.height).max(1);
        let scale = (192.0 / side as f32).min(1.0);
        match source.video_frame(filmcraft_media::FrameRequest { time, scale }) {
            Ok(frame) => {
                let mut shape = video_storage_valid(&frame);
                if shape {
                    let n = (frame.width.max(frame.height) as usize).div_ceil(192).max(1);
                    let (w, h, pixels) = frame.to_linear_f32_decimated(n);
                    shape = w > 0 && h > 0 && pixels.len() == w.saturating_mul(h).saturating_mul(4) && pixels.iter().all(|n| n.is_finite());
                }
                checks.push(json!({"check":"native_video_decode","status":if shape{"pass"}else{"fail"},"requestedTime":time.0,"returnedPts":frame.pts.0,"width":frame.width,"height":frame.height,"requestedScale":scale,"limitation":"first, middle and last sample; not every encoded frame"}));
            }
            Err(error) => checks.push(json!({"check":"native_video_decode","status":"fail","requestedTime":time.0,"error":error.to_string()})),
        }
    }
    checks
}
fn decode_audio(source: &filmcraft_media::SharedSource) -> Vec<Value> {
    let info = source.info();
    let mut checks = Vec::new();
    for (stream, metadata) in info.audio_streams.iter().enumerate().take(64) {
        let rate = metadata.sample_rate;
        if rate == 0 || metadata.channels == 0 || metadata.channels > 64 {
            checks.push(json!({"check":"native_audio_decode","stream":stream,"status":"fail","error":"invalid stream shape"}));
            continue;
        }
        let length = info.duration.to_units_floor(i64::from(rate));
        let frames = (rate as usize / 10).clamp(1, 9600);
        let mut times = BTreeSet::new();
        for sample in [0, (length / 2).max(0), length.saturating_sub(frames as i64).max(0)] {
            times.insert(sample);
        }
        for start in times {
            match source.audio_stream(stream, start, frames, rate) {
                Ok(buffer) => {
                    let valid = buffer.sample_rate == rate
                        && buffer.channels.len() == metadata.channels as usize
                        && buffer.channels.iter().all(|c| c.len() == frames && c.iter().all(|v| v.is_finite()));
                    let peak = buffer.channels.iter().flatten().map(|v| v.abs()).fold(0f32, f32::max);
                    checks.push(json!({"check":"native_audio_decode","stream":stream,"status":if valid{"pass"}else{"fail"},"startSample":start,"frames":frames,"peak":peak,"sampleRate":rate,"channels":buffer.channels.len(),"limitation":"short first, middle and last windows; not every encoded sample"}));
                }
                Err(error) => checks.push(json!({"check":"native_audio_decode","stream":stream,"status":"fail","startSample":start,"error":error.to_string()})),
            }
        }
    }
    checks
}
fn audio_checks(info: &filmcraft_media::MediaInfo, spec: &AudioSpec) -> Result<Vec<Value>> {
    if spec.sample_rate == Some(0) || spec.channels.is_some_and(|n| n == 0 || n > 64) || spec.streams.is_some_and(|n| n > 64) {
        return Err(bad("qc.export", "invalid expected audio shape"));
    }
    let mut checks = vec![check("audio_present", info.has_audio() == spec.present, info.has_audio(), spec.present)];
    if let Some(n) = spec.streams {
        checks.push(check("audio_stream_count", info.audio_streams.len() == n, info.audio_streams.len(), n));
    }
    if let Some(audio) = info.audio() {
        if let Some(n) = spec.sample_rate {
            checks.push(check("audio_sample_rate", audio.sample_rate == n, audio.sample_rate, n));
        }
        if let Some(n) = spec.channels {
            checks.push(check("audio_channels", audio.channels == n, audio.channels, n));
        }
    } else if spec.sample_rate.is_some() || spec.channels.is_some() {
        checks.push(json!({"check":"audio_shape","status":"fail","error":"audio stream is missing"}));
    }
    Ok(checks)
}
fn caption_delivery(s: &Session, info: &filmcraft_media::MediaInfo, spec: &CaptionSpec, sequence: Option<u64>, tolerance: Tick) -> Result<Vec<Value>> {
    if spec.mode == "none" {
        return Ok(vec![
            json!({"check":"captions","status":"unknown","expected":"none","reason":"Native MediaInfo does not expose all embedded subtitle/closed-caption streams; absence cannot be certified."}),
        ]);
    }
    if spec.mode == "burned" || spec.mode == "embedded" {
        return Ok(vec![
            json!({"check":"captions","status":"unknown","expected":spec.mode,"reason":"Burned text recognition and embedded caption demux verification require a caption reader/model for this output; sidecar delivery is verifiable natively."}),
        ]);
    }
    if spec.mode != "sidecar" {
        return Err(bad("qc.export", "caption mode must be sidecar, embedded, burned or none"));
    }
    let path = spec.path.as_deref().ok_or_else(|| bad("qc.export", "sidecar captions need path"))?;
    if path.len() > 4096 {
        return Err(bad("qc.export", "caption path is too long"));
    }
    let size = s.services.file_size(path).map_err(|e| bad("qc.export", format!("caption sidecar: {e}")))?;
    if size > 8 * 1024 * 1024 {
        return Err(bad("qc.export", "caption sidecar exceeds 8 MiB"));
    }
    let bytes = s.services.read_range(path, 0, size as usize).map_err(|e| bad("qc.export", e.to_string()))?;
    if bytes.len() as u64 != size {
        return Err(bad("qc.export", "caption sidecar changed while reading"));
    }
    let expected =
        spec.format.as_deref().map(|name| filmcraft_captions::Format::from_name(name).ok_or_else(|| bad("qc.export", "unknown caption format"))).transpose()?;
    let actual = filmcraft_captions::detect(&bytes, Path::new(path).extension().and_then(|e| e.to_str()))
        .ok_or_else(|| bad("qc.export", "unrecognized caption sidecar"))?;
    let doc = filmcraft_captions::parse(&bytes, actual).map_err(|e| bad("qc.export", e.to_string()))?;
    if doc.cues.len() > 100_000 {
        return Err(bad("qc.export", "caption sidecar exceeds 100000 cues"));
    }
    let mut checks = vec![
        json!({"check":"caption_parse","status":if doc.warnings.is_empty(){"pass"}else{"fail"},"path":path,"format":actual.extension(),"cues":doc.cues.len(),"warnings":doc.warnings}),
    ];
    if let Some(expected) = expected {
        checks.push(check("caption_format", actual == expected, actual.extension(), expected.extension()));
    }
    if let Some(count) = spec.count {
        checks.push(check("caption_count", doc.cues.len() == count, doc.cues.len(), count));
    }
    let inside = doc.cues.iter().all(|cue| cue.start >= Tick::ZERO && cue.end > cue.start && cue.end <= info.duration + tolerance);
    checks.push(check("caption_bounds", inside, "all cue intervals", format!("within delivery duration {}", info.duration.0)));
    if let Some(id) = sequence {
        let q = s.project.sequence(ItemId(id)).ok_or_else(|| bad("qc.export", "unknown expected sequence"))?;
        let expected = filmcraft_captions::document_from_track(
            &filmcraft_project::CaptionTrack {
                captions: q.caption_tracks.iter().filter(|t| t.enabled).flat_map(|t| t.captions.iter().cloned()).collect(),
                ..filmcraft_project::CaptionTrack::new(filmcraft_project::TrackId(0), "expected".into(), filmcraft_project::CaptionFormat::Subtitle)
            },
            Tick::ZERO,
        );
        let mut same = doc.cues.len() == expected.cues.len();
        if same {
            for (a, b) in doc.cues.iter().zip(&expected.cues) {
                if !caption_text_equal(&a.text, &b.text)?
                    || (a.start.0 - b.start.0).unsigned_abs() > tolerance.0 as u64
                    || (a.end.0 - b.end.0).unsigned_abs() > tolerance.0 as u64
                {
                    same = false;
                    break;
                }
            }
        }
        checks.push(check("caption_sequence_content_and_timing", same, doc.cues.len(), expected.cues.len()));
    }
    Ok(checks)
}
fn export(s: &mut Session, p: &Value) -> Result<Value> {
    let p: ExportRequest = bounded_decode(p, "qc.export")?;
    if p.path.is_empty() || p.path.len() > 4096 {
        return Err(bad("qc.export", "path must contain 1..4096 bytes"));
    }
    if let Some(rate) = p.expected.frame_rate {
        validate_rate(rate)?;
    }
    if p.expected.duration.is_some_and(|t| !(0..=Tick::MAX.0).contains(&t)) || p.expected.width == Some(0) || p.expected.height == Some(0) {
        return Err(bad("qc.export", "invalid expected duration or dimensions"));
    }

    let tolerance = p.expected.tolerance_frames.unwrap_or(1);
    if tolerance > 960 {
        return Err(bad("qc.export", "toleranceFrames must not exceed 960"));
    }
    let source = s.media.probe_file(&p.path, &*s.services)?;
    let info = source.info();
    let rate = p.expected.frame_rate.unwrap_or_else(|| info.frame_rate());
    validate_rate(rate)?;
    let tolerance = rate.tick_of(i64::from(tolerance));
    let mut checks = Vec::new();
    if let Some(duration) = p.expected.duration {
        checks.push(check("duration", (info.duration.0 - duration).unsigned_abs() <= tolerance.0 as u64, info.duration.0, duration));
    }
    if let Some(video) = &info.video {
        if let Some(expected) = p.expected.frame_rate {
            checks.push(check(
                "frame_rate",
                i128::from(video.frame_rate.num) * i128::from(expected.den) == i128::from(expected.num) * i128::from(video.frame_rate.den),
                video.frame_rate,
                expected,
            ));
        }
        if let Some(expected) = p.expected.width {
            checks.push(check("width", video.width == expected, video.width, expected));
        }
        if let Some(expected) = p.expected.height {
            checks.push(check("height", video.height == expected, video.height, expected));
        }
        let requested = p
            .expected
            .color_space
            .as_deref()
            .map(|name| filmcraft_color::ColorSpace::parse(name).ok_or_else(|| bad("qc.export", "unknown expected colorSpace")))
            .transpose()?;
        if requested.is_some() || p.expected.color.is_some() {
            let assumed = video.color_assumed;
            let known = assumed.is_some_and(|a| !a.matrix && !a.transfer && !a.primaries);
            checks.push(json!({"check":"color_tags_known","status":if known{"pass"}else{"unknown"},"actual":video.color,"assumed":assumed,"reason":if known{None}else{Some("The importer records absent/unknown metadata separately; inferred defaults cannot certify delivery tags.")}}));
            if let Some(expected) = requested {
                let actual = filmcraft_color::ColorSpace::from_info(&video.color);
                checks.push(check("color_space", actual == expected, actual.id(), expected.id()));
            }
            if let Some(expected) = p.expected.color {
                checks.push(check("color_tags", video.color == expected, video.color, expected));
            }
        }
    } else if p.expected.frame_rate.is_some()
        || p.expected.width.is_some()
        || p.expected.height.is_some()
        || p.expected.color_space.is_some()
        || p.expected.color.is_some()
    {
        checks.push(json!({"check":"video_present","status":"fail","error":"expected video stream is missing"}));
    }
    if let Some(spec) = &p.expected.audio {
        checks.extend(audio_checks(info, spec)?);
    }
    checks.extend(decode_video(&source));
    checks.extend(decode_audio(&source));
    if let Some(spec) = &p.expected.captions {
        checks.extend(caption_delivery(s, info, spec, p.expected.sequence, tolerance)?);
    }
    let verified = checks.iter().all(|c| c.get("status").and_then(Value::as_str) == Some("pass"));
    Ok(
        json!({"revision":s.revision,"path":p.path,"verified":verified,"metadata":info,"checks":checks,"decoder":"FilmCraft native media readers; no external executable","coverage":"Metadata plus bounded first/middle/last decode samples. This does not certify every encoded frame or sample.","projectChanged":false}),
    )
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "qc.timeline",
            label: "Check the proposed or live cut",
            menu: &[],
            shortcut: None,
            params: r#"{"sequence":id?,"plan":id?,"start":ticks?,"end":ticks?,"maxFrames":1..1200=120,"strideFrames":n?,"video":bool=true,"audio":bool=true,"flashFrames":1..24=2,"captionToleranceFrames":0..960=4,"clippingFraction":0..1=0.01}"#,
            enabled: always,
            run: timeline,
            journal: false,
        },
        CommandSpec {
            id: "qc.export",
            label: "Reopen and verify delivery media",
            menu: &[],
            shortcut: None,
            params: r#"{"path":str,"expected":{"duration":ticks?,"frameRate":{"num":n,"den":n}?,"width":n?,"height":n?,"colorSpace":id?,"color":ColorInfo?,"audio":{"present":bool,"sampleRate":n?,"channels":n?,"streams":n?}?,"captions":{"mode":"sidecar|none|embedded|burned","path":str?,"format":str?,"count":n?}?,"sequence":id?,"toleranceFrames":0..960=1}}"#,
            enabled: always,
            run: export,
            journal: false,
        },
    ]
}
#[cfg(test)]
mod tests;
