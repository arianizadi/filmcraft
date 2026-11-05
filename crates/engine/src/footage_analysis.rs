//! Read-only, bounded native footage measurements. Numeric descriptors are observations, not
//! object recognition. Sparse video samples report their uncertainty; audio decode failures never
//! become a successful silence finding. Optional speech inference uses installed local models.
use base64::Engine as _;
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::analysis_support::{AUDIO_RATE, Position, SubjectArgs, Target, bounded_decode};
use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};

const MAX_FRAMES: usize = 1200;
const MAX_WORDS: usize = 100_000;
const MAX_FINDINGS: usize = 4096;

fn yes() -> bool {
    true
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub subject: SubjectArgs,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub stride_frames: Option<u32>,
    pub max_frames: Option<usize>,
    #[serde(default = "yes")]
    pub video: bool,
    #[serde(default = "yes")]
    pub audio: bool,
    #[serde(default)]
    pub speech: bool,
    pub model: Option<String>,
    #[serde(default = "yes")]
    pub music: bool,
    #[serde(default = "yes")]
    pub waveform: bool,
    #[serde(default)]
    pub details: bool,
    pub black_threshold: Option<f32>,
    pub freeze_threshold: Option<f32>,
    pub shot_threshold: Option<f32>,
    pub silence_db: Option<f32>,
    pub click_threshold: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameMetrics {
    pub mean_luma: f32,
    pub black_fraction: f32,
    pub white_fraction: f32,
    pub rgb_mean: [f32; 3],
    pub saturation: f32,
    pub sharpness: f32,
    pub motion: Option<f32>,
    pub scene_difference: Option<f32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameMeasurement {
    pub position: Position,
    pub metrics: FrameMetrics,
    pub descriptors: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub kind: String,
    pub start: Position,
    pub end: Position,
    pub confidence: f32,
    pub evidence: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechWord {
    pub text: String,
    pub start: Position,
    pub end: Position,
    pub item: Option<u64>,
    pub source_word_index: Option<usize>,
    pub evidence_id: String,
    pub confidence: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisReport {
    pub revision: u64,
    pub subject: SubjectArgs,
    pub start: Position,
    pub end: Position,
    pub video: Value,
    pub frames: Vec<FrameMeasurement>,
    pub findings: Vec<Finding>,
    pub audio: Value,
    pub speech: Value,
    pub speech_words: Vec<SpeechWord>,
    pub music: Value,
    pub waveform: Option<Value>,
    pub methods: Vec<String>,
}

fn threshold(value: Option<f32>, default: f32, range: std::ops::RangeInclusive<f32>, name: &str) -> Result<f32> {
    let value = value.unwrap_or(default);
    if !value.is_finite() || !range.contains(&value) {
        return Err(bad("analysis.footage", format!("{name} is outside its finite range")));
    }
    Ok(value)
}

pub(crate) fn measure(
    image: &filmcraft_render::Image,
    previous: Option<&[u8]>,
    signature_previous: Option<&filmcraft_render::scene::Signature>,
) -> (FrameMetrics, Vec<u8>, filmcraft_render::scene::Signature) {
    let rgba = image.over_black_rgba8();
    let mut rgb = [0f64; 3];
    let (mut black, mut white, mut saturation, mut luma) = (0usize, 0usize, 0f64, 0f64);
    let mut lum = Vec::with_capacity(image.w.saturating_mul(image.h));
    for pixel in rgba.as_chunks::<4>().0.iter() {
        let [r, g, b] = [f32::from(pixel[0]) / 255.0, f32::from(pixel[1]) / 255.0, f32::from(pixel[2]) / 255.0];
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        lum.push(y);
        luma += f64::from(y);
        if y < 0.02 {
            black += 1;
        }
        if r.max(g).max(b) >= 254.5 / 255.0 {
            white += 1;
        }
        for (sum, value) in rgb.iter_mut().zip([r, g, b]) {
            *sum += f64::from(value);
        }
        saturation += f64::from(r.max(g).max(b) - r.min(g).min(b));
    }
    let n = lum.len().max(1) as f64;
    let (mut lap_sum, mut lap_sq, mut count) = (0f64, 0f64, 0usize);
    if image.w >= 3 && image.h >= 3 {
        for y in 1..image.h - 1 {
            for x in 1..image.w - 1 {
                let i = y * image.w + x;
                if let (Some(c), Some(l), Some(r), Some(u), Some(d)) = (lum.get(i), lum.get(i - 1), lum.get(i + 1), lum.get(i - image.w), lum.get(i + image.w))
                {
                    let lap = f64::from(4.0 * c - l - r - u - d);
                    lap_sum += lap;
                    lap_sq += lap * lap;
                    count += 1;
                }
            }
        }
    }
    let sharpness = if count > 0 { (lap_sq / count as f64 - (lap_sum / count as f64).powi(2)).max(0.0) as f32 } else { 0.0 };
    let motion = previous.filter(|p| p.len() == rgba.len()).map(|p| {
        p.as_chunks::<4>()
            .0
            .iter()
            .zip(rgba.as_chunks::<4>().0.iter())
            .map(|(a, b)| a.iter().zip(b).take(3).map(|(a, b)| f64::from(a.abs_diff(*b)) / 255.0).sum::<f64>())
            .sum::<f64>()
            / (n * 3.0)
    });
    let signature = filmcraft_render::scene::signature(&rgba, image.w, image.h);
    let diff = signature_previous.map(|previous| filmcraft_render::scene::difference(previous, &signature));
    (
        FrameMetrics {
            mean_luma: (luma / n) as f32,
            black_fraction: (black as f64 / n) as f32,
            white_fraction: (white as f64 / n) as f32,
            rgb_mean: rgb.map(|v| (v / n) as f32),
            saturation: (saturation / n) as f32,
            sharpness,
            motion: motion.map(|v| v as f32),
            scene_difference: diff,
        },
        rgba,
        signature,
    )
}

pub(crate) fn descriptors(metrics: &FrameMetrics, aspect: f64) -> Vec<String> {
    let mut labels = Vec::new();
    if metrics.mean_luma < 0.15 {
        labels.push("dark image");
    } else if metrics.mean_luma > 0.7 {
        labels.push("bright image");
    }
    if metrics.black_fraction > 0.98 {
        labels.push("black frame");
    }
    if metrics.saturation < 0.08 {
        labels.push("low color saturation");
    } else if metrics.saturation > 0.35 {
        labels.push("colorful image");
    }
    if metrics.sharpness < 0.001 {
        labels.push("soft low-detail image");
    } else if metrics.sharpness > 0.02 {
        labels.push("sharp detailed image");
    }
    if let Some(motion) = metrics.motion {
        if motion < 0.005 {
            labels.push("static low motion");
        } else if motion > 0.12 {
            labels.push("high visual motion");
        }
    }
    let [r, g, b] = metrics.rgb_mean;
    if r > g * 1.3 && r > b * 1.3 {
        labels.push("red dominant");
    } else if g > r * 1.3 && g > b * 1.3 {
        labels.push("green dominant");
    } else if b > r * 1.3 && b > g * 1.3 {
        labels.push("blue dominant");
    }
    if aspect > 1.2 {
        labels.push("landscape aspect");
    } else if aspect < 0.8 {
        labels.push("portrait aspect");
    }
    labels.into_iter().map(str::to_string).collect()
}

fn spans(target: &Target, flags: &[(Tick, Tick, bool)], kind: &str, confidence: f32, evidence: Value) -> Vec<Finding> {
    let mut ranges = Vec::new();
    let mut open = None;
    for &(a, b, flag) in flags {
        if flag {
            if open.is_none() {
                open = Some(a);
            }
        } else if let Some(start) = open.take() {
            ranges.push((start, a));
        }
        if flag
            && flags.last().is_some_and(|last| last.0 == a)
            && let Some(start) = open.take()
        {
            ranges.push((start, b));
        }
    }
    ranges
        .into_iter()
        .take(MAX_FINDINGS)
        .map(|(a, b)| Finding { kind: kind.into(), start: target.position(a), end: target.position(b), confidence, evidence: evidence.clone() })
        .collect()
}

fn video_analysis(s: &Session, target: &Target, p: &Request, a: Tick, b: Tick) -> Result<(Value, Vec<FrameMeasurement>, Vec<Finding>)> {
    if !p.video || !target.video {
        return Ok((json!({"status":if !p.video{"not_requested"}else{"no_video"}}), Vec::new(), Vec::new()));
    }
    let max = p.max_frames.unwrap_or(120);
    if !(1..=MAX_FRAMES).contains(&max) {
        return Err(bad("analysis.footage", "maxFrames must be 1..1200"));
    }
    let first = target.rate().frame_at(a);
    let last = target.rate().frame_at(b - Tick(1));
    let count = last.checked_sub(first).and_then(|v| v.checked_add(1)).ok_or_else(|| bad("analysis.footage", "frame range overflow"))?;
    let stride = match p.stride_frames {
        Some(0) => return Err(bad("analysis.footage", "strideFrames must be positive")),
        Some(n) => i64::from(n),
        None => (count.saturating_add(max as i64 - 1) / max as i64).max(1),
    };
    let work = count.saturating_add(stride - 1) / stride;
    if usize::try_from(work).ok().is_none_or(|n| n > max) {
        return Err(bad("analysis.footage", "range/stride exceeds maxFrames; increase stride or shorten range"));
    }
    let black = threshold(p.black_threshold, 0.98, 0.0..=1.0, "blackThreshold")?;
    let freeze = threshold(p.freeze_threshold, 0.001, 0.0..=1.0, "freezeThreshold")?;
    let shot = threshold(p.shot_threshold, 0.28, 0.01..=1.0, "shotThreshold")?;
    let subject = target.video_subject(s)?;
    let scale = subject.scale(&json!({"maxSide":192}), 1)?;
    let (mut rows, mut findings, mut black_flags, mut freeze_flags) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut previous, mut signature_previous, mut prior_time) = (None::<Vec<u8>>, None, None::<Tick>);
    for index in 0..work {
        crate::workflow::check_cancelled(s)?;
        let frame = first
            .checked_add(index.checked_mul(stride).ok_or_else(|| bad("analysis.footage", "frame offset overflow"))?)
            .ok_or_else(|| bad("analysis.footage", "frame position overflow"))?;
        let time = target.rate().tick_of(frame).max(a);
        let next = target.rate().tick_of(frame.saturating_add(stride)).min(b);
        let time = subject.validate_time(time)?;
        let (image, offline) = subject.render(s, time, scale, false)?;
        if offline.as_array().is_some_and(|v| !v.is_empty()) {
            return Err(bad("analysis.footage", format!("offline media cannot be analyzed: {offline}")));
        }
        let (metrics, pixels, signature) = measure(&image, previous.as_deref(), signature_previous.as_ref());
        black_flags.push((time, next, metrics.black_fraction >= black));
        freeze_flags.push((time, next, metrics.motion.is_some_and(|m| m <= freeze)));
        if metrics.scene_difference.is_some_and(|d| d >= shot) {
            findings.push(Finding{kind:"shot_boundary".into(),start:target.position(time),end:target.position(time),confidence:metrics.scene_difference.unwrap_or(0.0),
                evidence:json!({"method":"filmcraft_scene_histogram_and_luma","difference":metrics.scene_difference,"threshold":shot,"earliestPossibleCut":prior_time.map(|t|t.0),"uncertaintyFrames":stride.saturating_sub(1)})});
        }
        let labels = descriptors(&metrics, f64::from(target.settings.width) / f64::from(target.settings.height.max(1)));
        rows.push(FrameMeasurement { position: target.position(time), metrics, descriptors: labels });
        previous = Some(pixels);
        signature_previous = Some(signature);
        prior_time = Some(time);
    }
    findings.extend(spans(
        target,
        &black_flags,
        "black_frames",
        0.9,
        json!({"method":"sampled_sRGB_luma_below_2_percent","minimumBlackFraction":black,"strideFrames":stride}),
    ));
    findings.extend(spans(target,&freeze_flags,"frozen_or_static_frames",0.7,json!({"method":"sampled_mean_absolute_RGB_difference","threshold":freeze,"strideFrames":stride,"limitation":"intentional static shots and freeze frames cannot be distinguished by this measurement"})));
    Ok((
        json!({"status":"measured","totalFrames":count,"sampledFrames":rows.len(),"strideFrames":stride,"dense":stride==1,"analysisMaxSide":192,
        "signal":"FilmCraft sRGB/Rec709 display transform","limitations":if stride>1{"events between samples can be missed; boundary brackets report uncertainty"}else{"pixel metrics are measured at reduced resolution"}}),
        rows,
        findings,
    ))
}

#[derive(Clone)]
struct AudioWindow {
    a: Tick,
    b: Tick,
    rms: f32,
    peak: f32,
}
struct AudioAnalysis {
    summary: Value,
    mono: Vec<f32>,
    findings: Vec<Finding>,
    waveform: Option<Value>,
}

fn waveform_image(peaks: &[(f32, f32)]) -> Result<Value> {
    let w = peaks.len().clamp(1, 2048);
    let h = 128usize;
    let mut image = filmcraft_render::Image::filled(w, h, [0.015, 0.015, 0.015, 1.0]);
    for (x, (lo, hi)) in peaks.iter().take(w).enumerate() {
        let top = ((1.0 - hi.clamp(-1.0, 1.0)) * 0.5 * (h - 1) as f32).round() as usize;
        let bottom = ((1.0 - lo.clamp(-1.0, 1.0)) * 0.5 * (h - 1) as f32).round() as usize;
        for y in top.min(bottom)..=top.max(bottom).min(h - 1) {
            let i = (y * w + x) * 4;
            if let Some(pixel) = image.px.get_mut(i..i + 4) {
                pixel.copy_from_slice(&[0.2, 0.65, 0.85, 1.0]);
            }
        }
    }
    let bytes = crate::inspection::png_image(&image)?;
    Ok(
        json!({"mimeType":"image/png","width":w,"height":h,"base64":base64::engine::general_purpose::STANDARD.encode(bytes),"signal":"min/max across decoded channels, linear amplitude"}),
    )
}

fn audio_analysis(s: &Session, target: &Target, p: &Request, a: Tick, b: Tick) -> Result<AudioAnalysis> {
    if !p.audio || !target.audio {
        return Ok(AudioAnalysis {
            summary: json!({"status":if !p.audio{"not_requested"}else{"no_audio"}}),
            mono: Vec::new(),
            findings: Vec::new(),
            waveform: None,
        });
    }
    let sr = AUDIO_RATE;
    let start = a.to_units_floor(i64::from(sr));
    let end = b.to_units_floor(i64::from(sr));
    let len = usize::try_from(end.checked_sub(start).ok_or_else(|| bad("analysis.footage", "audio window overflow"))?)
        .map_err(|_| bad("analysis.footage", "audio length overflow"))?;
    if len > sr as usize * 300 {
        return Err(bad("analysis.footage", "audio analysis exceeds 300 seconds"));
    }
    let silence = threshold(p.silence_db, -42.0, -120.0..=0.0, "silenceDb")?;
    let click = threshold(p.click_threshold, 0.8, 0.05..=2.0, "clickThreshold")?;
    let provider = target.audio_reader(s);
    let mut mono = Vec::new();
    mono.try_reserve_exact(len).map_err(|e| bad("analysis.footage", format!("cannot allocate analysis audio: {e}")))?;
    let (mut windows, mut findings, mut waveform) = (Vec::new(), Vec::new(), vec![(1.0f32, -1.0f32); 1024]);
    let mut previous = Vec::<f32>::new();
    let (mut total_energy, mut count, mut peak) = (0f64, 0usize, 0f32);
    for offset in (0..len).step_by(320) {
        let n = (len - offset).min(320);
        let position = start.checked_add(offset as i64).ok_or_else(|| bad("analysis.footage", "audio sample offset overflow"))?;
        let time = Tick::from_units(position, i64::from(sr));
        let buffer = target.audio_block(&provider, time, n, sr)?;
        let loudest = buffer
            .channels
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>()))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(0, |x| x.0);
        let channel = buffer.channels.get(loudest).ok_or_else(|| bad("analysis.footage", "empty decoded audio"))?;
        mono.extend_from_slice(channel);
        let (mut energy, mut window_peak) = (0f64, 0f32);
        previous.resize(buffer.channels.len(), 0.0);
        for (ch, channel) in buffer.channels.iter().enumerate() {
            for (i, &sample) in channel.iter().enumerate() {
                energy += f64::from(sample).powi(2);
                window_peak = window_peak.max(sample.abs());
                let absolute = offset + i;
                let bucket = absolute.saturating_mul(1024) / len.max(1);
                if let Some((lo, hi)) = waveform.get_mut(bucket.min(1023)) {
                    *lo = lo.min(sample);
                    *hi = hi.max(sample);
                }
                if let Some(prior) = previous.get_mut(ch) {
                    let delta = (sample - *prior).abs();
                    if (offset > 0 || i > 0) && delta > click && findings.len() < 256 {
                        let tick = Tick::from_units(position.saturating_add(i as i64), i64::from(sr));
                        findings.push(Finding{kind:"possible_audio_click".into(),start:target.position(tick),end:target.position(tick),confidence:0.6,
                        evidence:json!({"channel":ch,"adjacentSampleDelta":delta,"threshold":click,"analysisSampleRate":sr,"limitation":"transients can be intentional; inspect the waveform"})});
                    }
                    *prior = sample;
                }
            }
        }
        let samples = n.saturating_mul(buffer.channels.len()).max(1);
        let rms = (energy / samples as f64).sqrt() as f32;
        windows.push(AudioWindow {
            a: time,
            b: Tick::from_units(position.saturating_add(n as i64), i64::from(sr)).min(b),
            rms: 20.0 * rms.max(1e-9).log10(),
            peak: window_peak,
        });
        total_energy += energy;
        count = count.saturating_add(samples);
        peak = peak.max(window_peak);
    }
    let silence_flags: Vec<_> = windows.iter().map(|w| (w.a, w.b, w.rms <= silence)).collect();
    findings.extend(spans(
        target,
        &silence_flags,
        "audio_silence",
        0.95,
        json!({"method":"all-channel_20ms_RMS","thresholdDbFS":silence,"analysisSampleRate":sr}),
    ));
    let activity: Vec<_> = windows.iter().map(|w| (w.a, w.b, w.rms > silence)).collect();
    let activity = spans(target, &activity, "audio_activity", 0.9, json!({"method":"energy_activity_is_not_speech_recognition"}));
    let waveform = if p.waveform {
        for pair in &mut waveform {
            if pair.0 > pair.1 {
                *pair = (0.0, 0.0);
            }
        }
        Some(waveform_image(&waveform)?)
    } else {
        None
    };
    let detail = if p.details {
        Some(windows.iter().map(|w| json!({"start":target.position(w.a),"end":target.position(w.b),"rmsDbFS":w.rms,"peak":w.peak})).collect::<Vec<_>>())
    } else {
        None
    };
    Ok(AudioAnalysis {
        summary: json!({"status":"measured","sampleRate":sr,"windows":windows.len(),"windowSamples":320,"peak":peak,
        "rmsDbFS":20.0*((total_energy/count.max(1)as f64).sqrt().max(1e-9)).log10(),"activitySpans":activity,"windowDetails":detail,
        "speechMusicMono":"loudest decoded channel in each block; activity energy includes all channels","durationSamples":len}),
        mono,
        findings,
        waveform,
    })
}

pub(crate) fn evidence_id(item: u64, index: usize, text: &str, start: Tick, end: Tick) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in item.to_le_bytes().into_iter().chain(start.0.to_le_bytes()).chain(end.0.to_le_bytes()).chain(text.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x1000_0000_01b3);
    }
    format!("word-{item}-{index}-{hash:016x}")
}

fn stored_words(target: &Target, a: Tick, b: Tick) -> Result<Vec<SpeechWord>> {
    let mut out = Vec::new();
    let mut text_bytes = 0usize;
    if let Some(item) = target.item {
        let root = target.project.resolve_media(item).map(|x| x.0).unwrap_or(item);
        if let Some(transcript) = target.project.transcripts.get(&item).or_else(|| target.project.transcripts.get(&root)) {
            for (index, word) in transcript.words.iter().enumerate().filter(|(_, w)| w.start < b && w.end > a).take(MAX_WORDS + 1) {
                text_bytes = text_bytes.saturating_add(word.text.len());
                if text_bytes > 8 * 1024 * 1024 {
                    return Err(bad("analysis.footage", "transcript evidence exceeds 8 MiB text"));
                }
                out.push(SpeechWord {
                    text: word.text.chars().take(512).collect(),
                    start: target.position(word.start.max(a)),
                    end: target.position(word.end.min(b)),
                    item: Some(item.0),
                    source_word_index: Some(index),
                    evidence_id: evidence_id(item.0, index, &word.text, word.start, word.end),
                    confidence: if word.confidence.is_finite() { word.confidence.clamp(0.0, 1.0) } else { 0.0 },
                });
            }
        }
    } else if let Some(id) = target.sequence {
        let q = target.project.sequence(id).ok_or(crate::EngineError::NoSequence)?;
        let estimated = q
            .audio_tracks
            .iter()
            .flat_map(|t| &t.items)
            .filter(|it| it.range().overlaps(&filmcraft_time::TimeRange::from_bounds(a, b)))
            .map(|it| target.project.transcripts.get(&it.item).map_or(0, |t| t.words.len()))
            .fold(0usize, usize::saturating_add);
        if estimated > MAX_WORDS {
            return Err(bad("analysis.footage", "transcript scan exceeds 100000 word placements; narrow the subject"));
        }
        for word in filmcraft_edit::transcript::sequence_words(q, &target.project.transcripts)
            .into_iter()
            .filter(|w| w.start < b && w.end > a && target.clip.as_ref().is_none_or(|c| c.id == w.clip))
        {
            text_bytes = text_bytes.saturating_add(word.text.len());
            if text_bytes > 8 * 1024 * 1024 {
                return Err(bad("analysis.footage", "transcript evidence exceeds 8 MiB text"));
            }
            let original = target.project.transcripts.get(&word.item).and_then(|tr| tr.words.get(word.index));
            let (start, end) = original.map_or((word.start, word.end), |w| (w.start, w.end));
            out.push(SpeechWord {
                text: word.text.clone(),
                start: target.position(word.start.max(a)),
                end: target.position(word.end.min(b)),
                item: Some(word.item.0),
                source_word_index: Some(word.index),
                evidence_id: evidence_id(word.item.0, word.index, &word.text, start, end),
                confidence: if word.confidence.is_finite() { word.confidence.clamp(0.0, 1.0) } else { 0.0 },
            });
        }
    }
    if out.len() > MAX_WORDS {
        return Err(bad("analysis.footage", "too many transcript words"));
    }
    Ok(out)
}

fn speech_analysis(s: &Session, target: &Target, p: &Request, a: Tick, b: Tick, audio: &[f32]) -> Result<(Value, Vec<SpeechWord>)> {
    let mut words = stored_words(target, a, b)?;
    if !words.is_empty() {
        return Ok((json!({"status":"available","source":"project_transcript","readOnly":true,"wordCount":words.len()}), words));
    }
    if !p.speech {
        return Ok((
            json!({"status":"transcript_required","source":"none","message":"Existing transcripts are read automatically; speech:true runs an installed local model without saving a transcript. Energy activity is returned separately."}),
            words,
        ));
    }
    if audio.is_empty() {
        return Err(bad("analysis.footage", "speech analysis needs decoded audio"));
    }
    let transcriber = if let Some(t) = &s.transcriber {
        t.clone()
    } else {
        let model = p.model.as_deref().unwrap_or(&s.prefs.media_analysis.whisper_model);
        if model.len() > 128 {
            return Err(bad("analysis.footage", "model ID exceeds 128 bytes"));
        }
        let dir = crate::transcript::models_dir().ok_or_else(|| bad("analysis.footage", "no local speech-model directory"))?;
        filmcraft_speech::load(&dir, model).map_err(|e| bad("analysis.footage", e.to_string()))?
    };
    let mut transcript = transcriber
        .transcribe(audio, &filmcraft_speech::Options { diarize: false, ..Default::default() }, &mut |_, _| true)
        .map_err(|e| bad("analysis.footage", e.to_string()))?;
    transcript.normalize();
    if transcript.words.len() > MAX_WORDS {
        return Err(bad("analysis.footage", "speech model returned too many words"));
    }
    for (index, word) in transcript.words.into_iter().enumerate() {
        let start = (a + word.start).clamp(a, b);
        let end = (a + word.end).clamp(start, b);
        if end > start {
            words.push(SpeechWord {
                text: word.text.clone(),
                start: target.position(start),
                end: target.position(end),
                item: target.item.map(|i| i.0),
                source_word_index: None,
                evidence_id: evidence_id(target.item.map_or(0, |i| i.0), index, &word.text, start, end),
                confidence: word.confidence,
            });
        }
    }
    Ok((
        json!({"status":"inferred","source":transcriber.id(),"readOnly":true,"wordCount":words.len(),"audioSampleRate":AUDIO_RATE,"modelDownloads":false}),
        words,
    ))
}

fn music_analysis(target: &Target, p: &Request, a: Tick, mono: &[f32]) -> Value {
    if !p.music || mono.is_empty() {
        return json!({"status":"not_requested_or_no_audio"});
    }
    let (envelope, n, hop) = filmcraft_audio_dsp::remix::onset_envelope(mono, AUDIO_RATE);
    let mut onsets = Vec::new();
    let mut prior = 0usize;
    for (index, window) in envelope.windows(3).enumerate() {
        let at = index + 1;
        let strength = window.get(1).copied().unwrap_or(0.0);
        if strength >= 1.5
            && strength > window.first().copied().unwrap_or(0.0)
            && strength >= window.get(2).copied().unwrap_or(0.0)
            && at.saturating_sub(prior).saturating_mul(hop) >= AUDIO_RATE as usize / 20
        {
            let sample = at.saturating_mul(hop).saturating_add(n / 2).min(mono.len());
            let time = a + Tick::from_units(sample as i64, i64::from(AUDIO_RATE));
            onsets.push(json!({"position":target.position(time),"strength":strength}));
            prior = at;
            if onsets.len() >= MAX_FINDINGS {
                break;
            }
        }
    }
    match filmcraft_audio_dsp::remix::analyze(&[mono], AUDIO_RATE) {
        Ok(analysis) => json!({"status":"measured","method":"existing_FilmCraft_remix_STFT_flux_and_dynamic_programming_beat_grid","bpm":analysis.bpm,
            "beats":analysis.beats.iter().take(MAX_FINDINGS).map(|sample|target.position(a+Tick::from_units(*sample,i64::from(AUDIO_RATE)))).collect::<Vec<_>>(),"onsets":onsets,"sampleRate":AUDIO_RATE}),
        Err(error) => json!({"status":"no_reliable_beat_grid","reason":error.to_string(),"onsets":onsets,"sampleRate":AUDIO_RATE}),
    }
}

pub(crate) fn analyze(s: &Session, p: &Request) -> Result<AnalysisReport> {
    let target = Target::resolve(s, &p.subject)?;
    let (a, b) = target.window(p.start, p.end)?;
    let (video, frames, mut findings) = video_analysis(s, &target, p, a, b)?;
    let audio = audio_analysis(s, &target, p, a, b)?;
    let (speech, speech_words) = speech_analysis(s, &target, p, a, b, &audio.mono)?;
    let music = music_analysis(&target, p, a, &audio.mono);
    findings.extend(audio.findings);
    findings.truncate(MAX_FINDINGS);
    Ok(AnalysisReport {
        revision: target.revision,
        subject: p.subject.clone(),
        start: target.position(a),
        end: target.position(b),
        video,
        frames,
        findings,
        audio: audio.summary,
        speech,
        speech_words,
        music,
        waveform: audio.waveform,
        methods: vec![
            "Video is sampled on the subject frame grid at up to 192px; no object/face/shot-scale model is implied.".into(),
            "Every finding reports source and/or composite timeline mappings; sparse boundaries are bracketed, not claimed frame-exact.".into(),
            "Audio silence/activity uses all-channel RMS; speech uses actual transcript evidence or explicitly requested local inference.".into(),
        ],
    })
}

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let request: Request = bounded_decode(p, "analysis.footage")?;
    serde_json::to_value(analyze(s, &request)?).map_err(|e| bad("analysis.footage", e.to_string()))
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "analysis.footage",
        label: "Analyze footage without editing",
        menu: &[],
        shortcut: None,
        params: r#"{"subject":{"target":"source|clip|sequence","item":id?,"clip":id?,"sequence":id?,"plan":id?,"effects":"pre|post"?},"start":ticks?,"end":ticks?,"maxFrames":1..1200=120,"strideFrames":n?,"video":bool=true,"audio":bool=true,"speech":bool=false,"model":id?,"music":bool=true,"waveform":bool=true,"details":bool=false}"#,
        enabled: always,
        run,
        journal: false,
    }]
}

#[cfg(test)]
mod tests;
