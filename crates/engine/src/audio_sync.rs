//! Reviewable external-microphone alignment. Raw original-source audio is measured without
//! changing media, the camera stays fixed, and the eventual move is one atomic in-memory edit.
//! Waveform correlation is not semantic clap detection. Ambiguous material needs source anchors.
use std::collections::BTreeSet;

use filmcraft_audio_dsp::{
    inspect::Analyzer,
    sync::{SyncOptions, find_offset},
};
use filmcraft_project::{AudioChannels, ClipId, ItemId, ItemKind, Project, Sequence, TrackId, TrackItem};
use filmcraft_render::SourceProvider;
use filmcraft_time::{Tick, TimeRange};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Result, Session,
    analysis_support::{SubjectArgs, Target, bounded_decode},
    commands::{CommandSpec, always, bad},
};

const RATE: u32 = 48_000;
const MAX_SECONDS: f64 = 120.0;
const MAX_OFFSET_SAMPLES: i64 = 120 * RATE as i64;
const MAX_LINKED: usize = 256;
const BLOCK: usize = 16_384;
const COMMAND: &str = "audio.sync";

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Method {
    #[default]
    Waveform,
    Manual,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Params {
    reference: u64,
    clip: u64,
    #[serde(default)]
    method: Method,
    reference_time: Option<i64>,
    clip_time: Option<i64>,
    window_seconds: Option<f64>,
    max_lag_seconds: Option<f64>,
    #[serde(default)]
    offset_samples: i64,
    #[serde(default)]
    mute_reference: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AlignParams {
    reference: u64,
    clip: u64,
    offset_ticks: i64,
    #[serde(default)]
    mute_reference: bool,
}

fn error(reason: impl std::fmt::Display) -> crate::EngineError {
    bad(COMMAND, reason.to_string())
}
fn tick(value: i128) -> Result<Tick> {
    if !(i128::from(Tick::MIN.0)..=i128::from(Tick::MAX.0)).contains(&value) {
        return Err(error("time is outside the supported timeline range"));
    }
    Ok(Tick(i64::try_from(value).map_err(error)?))
}
fn range(start: Tick, duration: Tick) -> Result<TimeRange> {
    if start < Tick::ZERO || duration <= Tick::ZERO {
        return Err(error("clip and source ranges must be positive and start at or after zero"));
    }
    let end = tick(i128::from(start.0) + i128::from(duration.0))?;
    Ok(TimeRange::from_bounds(start, end))
}

/// Check every restricted subclip ancestor and the original media. Unrestricted subclips may
/// legitimately be trimmed beyond their declared span, as in the timeline's normal trim tools.
fn source_range(project: &Project, clip: &TrackItem) -> Result<TimeRange> {
    let used = range(clip.source_in, clip.duration)?;
    let (mut id, mut seen) = (clip.item, BTreeSet::new());
    for _ in 0..16 {
        if !seen.insert(id) {
            return Err(error("cyclic source item"));
        }
        match project.item(id).map(|item| &item.kind) {
            Some(ItemKind::Subclip { parent, range: bounds, restrict_trims }) => {
                let bounds = range(bounds.start, bounds.duration)?;
                if *restrict_trims && (used.start < bounds.start || used.end() > bounds.end()) {
                    return Err(error("clip source range extends outside its subclip; trim it before synchronizing"));
                }
                id = *parent;
            }
            Some(ItemKind::Media(media)) => {
                let bounds = range(Tick::ZERO, media.duration())?;
                if used.end() > bounds.end() {
                    return Err(error("clip source range extends outside the original recording"));
                }
                return Ok(used);
            }
            _ => return Err(error("sync needs clips backed by original media, not nested sequences or generated items")),
        }
    }
    Err(error("source nesting exceeds sixteen items"))
}
fn normal_speed(clip: &TrackItem) -> Result<()> {
    if clip.speed != 1.0 || clip.reverse || clip.frame_hold.is_some() {
        return Err(error("sync requires forward clips at 100% speed without frame hold; restore those settings first"));
    }
    Ok(())
}
fn locate(seq: &Sequence, id: u64) -> Result<(TrackId, &TrackItem)> {
    if id == 0 {
        return Err(error("reference and clip must be nonzero timeline clip IDs"));
    }
    let mut matches = seq.all_tracks().flat_map(|t| t.items.iter().filter(move |c| c.id.0 == id).map(move |c| (t.id, c)));
    let found = matches.next().ok_or_else(|| error(format!("clip {id} is not in the active sequence")))?;
    if matches.next().is_some() {
        return Err(error("duplicate clip identity in project; repair the project before synchronizing"));
    }
    normal_speed(found.1)?;
    range(found.1.start, found.1.duration)?;
    Ok(found)
}
fn endpoints(s: &Session, reference: u64, clip: u64, reference_audio: bool) -> Result<(ItemId, TrackItem, TrackItem)> {
    if reference == clip {
        return Err(error("reference and target must be different timeline clips"));
    }
    let sequence = s.state.active_sequence.ok_or(crate::EngineError::NoSequence)?;
    let seq = s.project.sequence(sequence).ok_or(crate::EngineError::NoSequence)?;
    seq.settings.validate().map_err(error)?;
    let (reference_track, reference) = locate(seq, reference)?;
    let (track, target) = locate(seq, clip)?;
    if !seq.audio_tracks.iter().any(|t| t.id == track) {
        return Err(error("target clip must be on an audio track; keep the camera as the reference"));
    }
    source_range(&s.project, reference)?;
    source_range(&s.project, target)?;
    for it in [reference, target].into_iter().filter(|it| reference_audio || it.id == target.id) {
        let (_, media, _) = s.project.resolve_media(it.item).ok_or_else(|| error("source media unavailable"))?;
        if media.info.audio_streams.get(it.audio_stream).is_none() {
            return Err(error(format!("clip {} has no selected audio stream {}; select the correct recording/stream", it.id.0, it.audio_stream)));
        }
    }
    let mut reference = reference.clone();
    // A video reference has no independent stream picker in the sync form. A container
    // can have several native recordings. Use secondary stream 1 when present and report it;
    // selecting an explicit audio timeline clip remains authoritative for any other stream.
    if seq.video_tracks.iter().any(|t| t.id == reference_track)
        && reference.audio_stream == 0
        && s.project.resolve_media(reference.item).is_some_and(|(_, media, _)| media.info.audio_streams.len() > 1)
    {
        reference.audio_stream = 1;
    }
    Ok((sequence, reference, target.clone()))
}
fn metadata(clip: &TrackItem) -> Value {
    let name: String = clip.name.chars().take(256).collect();
    json!({"clip":clip.id.0,"item":clip.item.0,"name":name,"start":clip.start.0,
        "duration":clip.duration.0,"sourceIn":clip.source_in.0,"sourceEnd":(clip.source_in + clip.duration).0,
        "audioStream":clip.audio_stream,"sourceChannels":clip.source_channels})
}
fn warning(code: &str, message: impl Into<String>) -> Value {
    json!({"code":code,"message":message.into()})
}

/// Effective source picks follow the renderer, without silently falling back for invalid picks.
fn played(clip: &TrackItem, channels: usize, surround: bool) -> Result<Vec<usize>> {
    if channels == 0 || channels > 64 || clip.source_channels.len() > 64 {
        return Err(error("audio stream/picks must contain 1..64 channels"));
    }
    if clip.source_channels.iter().any(|c| usize::from(*c) >= channels) {
        return Err(error("selected source channel is outside the recording; correct Audio Channels first"));
    }
    let mut picks = if surround || (clip.source_channels.is_empty() && channels == 6) || clip.source_channels.len() >= 6 {
        filmcraft_render::audio::source_51(clip, channels)
    } else {
        let (l, r) = filmcraft_render::audio::source_pair(clip, channels);
        vec![l, r]
    };
    picks.sort_unstable();
    picks.dedup();
    Ok(picks)
}
struct Waveform {
    data: Vec<f32>,
    channel: usize,
    first: Tick,
    coverage: Value,
    peak: f64,
}

#[derive(Clone, Copy)]
struct PairStats {
    correlation: Option<f64>,
    left_rms: f64,
    right_rms: f64,
}
impl PairStats {
    fn read(left: &[f32], right: &[f32]) -> Option<Self> {
        if left.is_empty() || left.len() != right.len() {
            return None;
        }
        let (mut n, mut ml, mut mr, mut ll, mut rr, mut lr) = (0usize, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (&l, &r) in left.iter().zip(right) {
            if !l.is_finite() || !r.is_finite() {
                return None;
            }
            n += 1;
            let (l, r) = (f64::from(l), f64::from(r));
            let (dl, dr) = (l - ml, r - mr);
            ml += dl / n as f64;
            mr += dr / n as f64;
            ll += dl * (l - ml);
            rr += dr * (r - mr);
            lr += dl * (r - mr);
        }
        let product = ll.max(0.0) * rr.max(0.0);
        Some(Self {
            correlation: (product > 0.0).then(|| (lr / product.sqrt()).clamp(-1.0, 1.0)),
            left_rms: (ll.max(0.0) / n as f64).sqrt(),
            right_rms: (rr.max(0.0) / n as f64).sqrt(),
        })
    }
    fn energy(self) -> f64 {
        self.left_rms * self.right_rms
    }
    fn verified(self) -> bool {
        // A quarter of normalized covariance is deliberately conservative for differently
        // filtered microphones. Noise, hum and a coincident amplitude envelope are insufficient;
        // coarse confidence/distinctness must independently pass as well.
        self.left_rms >= 1e-5 && self.right_rms >= 1e-5 && self.correlation.is_some_and(|c| c.abs() >= 0.25)
    }
    fn report(self) -> Value {
        json!({"correlation":self.correlation,"referenceCenteredRms":self.left_rms,"targetCenteredRms":self.right_rms})
    }
}

/// Distinctness of an intrinsic waveform, independent of GCC's finite-window edge peak. Only
/// the already-selected shared-event window is sampled (at most 1s). Roughly twenty million
/// scalar products cover BOTH recordings' worst-case 50..250ms searches, regardless of source
/// length. Picking the highest-variance decimation phase avoids sampling only a zero crossing.
fn periodicity(signal: &[f32], raw_rms: f64) -> (bool, Value) {
    const STEP: usize = 6;
    const COARSE_RATE: usize = RATE as usize / STEP;
    const MIN_LAG: usize = COARSE_RATE / 20;
    const MAX_LAG: usize = COARSE_RATE / 4;
    let signal = &signal[..signal.len().min(RATE as usize)];
    let mut report = json!({"status":"insufficientSignal","periodic":false,"sampleRate":COARSE_RATE,"decimation":STEP,
        "minimumAbsCorrelation":0.995,"minimumLagSamples":MIN_LAG,"maximumLagSamples":MAX_LAG});
    if signal.len() < RATE as usize / 10 || !raw_rms.is_finite() || raw_rms < 1e-5 {
        return (false, report);
    }
    let mut best_phase = (0, 0.0f64);
    for phase in 0..STEP {
        let (mut n, mut mean, mut variance) = (0usize, 0.0, 0.0);
        for &x in signal.iter().skip(phase).step_by(STEP) {
            if !x.is_finite() {
                return (false, report);
            }
            n += 1;
            let delta = f64::from(x) - mean;
            mean += delta / n as f64;
            variance += delta * (f64::from(x) - mean);
        }
        let rms = if n > 0 { (variance.max(0.0) / n as f64).sqrt() } else { 0.0 };
        if rms > best_phase.1 {
            best_phase = (phase, rms);
        }
    }
    report["phase"] = json!(best_phase.0);
    report["centeredRms"] = json!(best_phase.1);
    report["samples"] = json!((signal.len() - best_phase.0).div_ceil(STEP));
    // If every phase is constant but the full-rate waveform has substantial AC, it repeats
    // every six native samples (e.g. an 8kHz tone at 48kHz). Ordinary decimation aliases it to DC;
    // that must not accidentally turn an intrinsically ambiguous tone into trusted alignment.
    if best_phase.1 <= raw_rms * 1e-4 {
        report["status"] = json!("repeatedSubsampleCycle");
        report["periodic"] = json!(true);
        report["nativePeriodSamples"] = json!(STEP);
        return (true, report);
    }
    let samples: Vec<f64> = signal.iter().skip(best_phase.0).step_by(STEP).take(8192).map(|x| f64::from(*x)).collect();
    let mut sums = Vec::with_capacity(samples.len() + 1);
    let mut squares = Vec::with_capacity(samples.len() + 1);
    sums.push(0.0);
    squares.push(0.0);
    let (mut sum, mut square) = (0.0, 0.0);
    for &x in &samples {
        sum += x;
        square += x * x;
        sums.push(sum);
        squares.push(square);
    }
    // samples<=8192 and both prefixes have samples.len()+1 entries. Lag bounds leave at least
    // 50ms of paired samples, so every index/count and denominator is checked once up front.
    let n = samples.len();
    let maximum = MAX_LAG.min(n.saturating_sub(MIN_LAG));
    if maximum < MIN_LAG {
        return (false, report);
    }
    let mut best = (0usize, 0.0f64);
    for lag in MIN_LAG..=maximum {
        let count = n - lag;
        let (left_sum, right_sum) = (sums[count], sums[n] - sums[lag]);
        let left_energy = (squares[count] - left_sum * left_sum / count as f64).max(0.0);
        let right_energy = (squares[n] - squares[lag] - right_sum * right_sum / count as f64).max(0.0);
        let denominator = (left_energy * right_energy).sqrt();
        if denominator <= 0.0 {
            continue;
        }
        let product: f64 = samples[..count].iter().zip(&samples[lag..]).map(|(a, b)| a * b).sum();
        let correlation = ((product - left_sum * right_sum / count as f64) / denominator).clamp(-1.0, 1.0);
        if correlation.abs() > best.1.abs() {
            best = (lag, correlation);
        }
        if best.1.abs() >= 0.995 {
            break;
        }
    }
    let periodic = best.1.abs() >= 0.995;
    report["status"] = json!(if periodic { "periodic" } else { "distinct" });
    report["periodic"] = json!(periodic);
    report["correlation"] = json!(best.1);
    report["lagSamples"] = json!(best.0);
    report["samples"] = json!(n);
    (periodic, report)
}

/// A sharp GCC peak is not proof that the recordings share content. Verify actual aligned,
/// DC-removed sample pairs over at least 0.1 seconds. A short clap can be diluted by long unrelated
/// background activity, so also inspect ONE strongest shared-energy window (at most one second).
/// That window is chosen by joint centred energy, never by maximizing correlation over windows.
fn verify_waveform(a: &[f32], b: &[f32], lag: i64) -> (bool, Value) {
    let mut report = json!({"method":"dcRemovedPearson","status":"insufficientOverlap","minimumSamples":RATE/10,
        "minimumAbsCorrelation":0.25,"minimumCenteredRms":1e-5,"amplitudeUnits":"peak-normalized analysis copies"});
    let start_a = usize::try_from(lag.max(0)).ok();
    let start_b = lag.checked_neg().and_then(|n| usize::try_from(n.max(0)).ok());
    let Some((start_a, start_b)) = start_a.zip(start_b) else { return (false, report) };
    let Some((a, b)) = a.get(start_a..).zip(b.get(start_b..)) else { return (false, report) };
    let overlap = a.len().min(b.len());
    report["overlapSamples"] = json!(overlap);
    if overlap < RATE as usize / 10 {
        return (false, report);
    }
    // Both slices contain overlap samples, validated once before these bounded reads.
    let (a, b) = (&a[..overlap], &b[..overlap]);
    let Some(full) = PairStats::read(a, b) else {
        report["status"] = json!("nonFiniteSource");
        return (false, report);
    };
    let window = overlap.min(RATE as usize);
    let last = overlap - window;
    let mut strongest: Option<(usize, PairStats)> = None;
    for start in (0..=last).step_by(window).chain((last % window != 0).then_some(last)) {
        let Some(stats) = PairStats::read(&a[start..start + window], &b[start..start + window]) else {
            report["status"] = json!("nonFiniteSource");
            return (false, report);
        };
        if strongest.is_none_or(|(_, current)| stats.energy() > current.energy()) {
            strongest = Some((start, stats));
        }
    }
    let Some((start, event)) = strongest else { return (false, report) };
    let correlation_pass = full.verified() || event.verified();
    let periodic = if correlation_pass {
        let (reference_periodic, reference_report) = periodicity(&a[start..start + window], event.left_rms);
        let (target_periodic, target_report) = periodicity(&b[start..start + window], event.right_rms);
        report["periodicity"] = json!({"reference":reference_report,"target":target_report,"bothPeriodic":reference_periodic&&target_periodic});
        reference_periodic && target_periodic
    } else {
        false
    };
    let verified = correlation_pass && !periodic;
    let chosen = if full.verified() { full } else { event };
    report["status"] = json!(if periodic {
        "periodicAmbiguity"
    } else if verified {
        "verified"
    } else {
        "poorAlignedCorrelation"
    });
    report["correlation"] = json!(chosen.correlation);
    report["acceptedBy"] = json!(if !verified {
        "none"
    } else if full.verified() {
        "wholeOverlap"
    } else {
        "sharedEvent"
    });
    report["wholeOverlap"] = full.report();
    report["sharedEvent"] = event.report();
    report["sharedEvent"]["referenceWindowStartSample"] = json!(start_a + start);
    report["sharedEvent"]["targetWindowStartSample"] = json!(start_b + start);
    report["sharedEvent"]["samples"] = json!(window);
    (verified, report)
}

fn waveform(s: &Session, sequence: ItemId, clip: &TrackItem, seconds: f64) -> Result<Waveform> {
    let target =
        Target::resolve(s, &SubjectArgs { target: Some("source".into()), item: Some(clip.item.0), effects: Some("pre".into()), ..Default::default() })?;
    let provider = target.audio_reader(s);
    let source = provider.source(clip.item).ok_or_else(|| error("original source audio is unavailable"))?;
    let stream = source.info().audio_streams.get(clip.audio_stream).ok_or_else(|| error("original source audio stream is unavailable"))?;
    let channels = usize::try_from(stream.channels).map_err(error)?;
    if stream.sample_rate == 0 || stream.sample_rate > filmcraft_media::MAX_AUDIO_SAMPLE_RATE {
        return Err(error("audio sample rate must be 1..384000 Hz"));
    }
    let surround = s
        .project
        .sequence(sequence)
        .and_then(|q| q.find_item(clip.id).and_then(|(track, _)| q.track(track)))
        .is_some_and(|t| t.channels == AudioChannels::Surround51);
    let picks = played(clip, channels, surround)?;
    let mut start = clip.source_in.to_units_floor(i64::from(RATE));
    if Tick::from_units(start, i64::from(RATE)) < clip.source_in {
        start = start.checked_add(1).ok_or_else(|| error("source position overflow"))?;
    }
    // Only complete output-rate samples within the used source span are read; no padded tail is
    // mistaken for measured source coverage. The initial sample-grid phase is reported exactly.
    let end_tick = tick(i128::from(clip.source_in.0) + i128::from(clip.duration.min(Tick::from_seconds_f64(seconds)).0))?;
    let end = end_tick.to_units_floor(i64::from(RATE));
    let count = usize::try_from(end.checked_sub(start).ok_or_else(|| error("source window overflow"))?).map_err(error)?;
    if count < RATE as usize / 10 || count > MAX_OFFSET_SAMPLES as usize {
        return Err(error("waveform sync needs at least 0.1 seconds of source audio and at most 120 seconds; use manual anchors for shorter clips"));
    }
    let mut meter = Analyzer::new(channels).map_err(error)?;
    let decode = |at: usize, n: usize| -> Result<filmcraft_frame::AudioBuffer> {
        let at = start.checked_add(i64::try_from(at).map_err(error)?).ok_or_else(|| error("audio position overflow"))?;
        let buffer = source.audio_stream(clip.audio_stream, at, n, RATE)?;
        provider.check()?;
        if buffer.sample_rate != RATE || buffer.channels.len() != channels || buffer.channels.iter().any(|c| c.len() != n || c.iter().any(|x| !x.is_finite())) {
            return Err(error("decoder returned incomplete, inconsistent or non-finite original audio"));
        }
        Ok(buffer)
    };
    let mut at = 0;
    while at < count {
        let n = BLOCK.min(count - at);
        let buffer = decode(at, n)?;
        let slices: Vec<&[f32]> = buffer.channels.iter().map(Vec::as_slice).collect();
        meter.push(&slices).map_err(error)?;
        at += n;
    }
    let stats = meter.report();
    let channel = picks
        .into_iter()
        .max_by(|a, b| {
            stats
                .channels
                .get(*a)
                .and_then(|s| s.ac_rms)
                .unwrap_or(0.0)
                .total_cmp(&stats.channels.get(*b).and_then(|s| s.ac_rms).unwrap_or(0.0))
                // Deterministically prefer the first physical channel when energy is equal.
                .then_with(|| b.cmp(a))
        })
        .ok_or_else(|| error("clip has no effective source channels"))?;
    let stats = stats.channels.get(channel).ok_or_else(|| error("selected source channel unavailable"))?;
    if stats.ac_rms.is_none_or(|r| r <= 1e-9) {
        return Err(error(format!(
            "clip {} has only silence/DC in its selected source channels/window; choose another window/stream or manual source anchors",
            clip.id.0
        )));
    }
    let mut data = Vec::with_capacity(count);
    at = 0;
    while at < count {
        let n = BLOCK.min(count - at);
        let buffer = decode(at, n)?;
        let selected = buffer.channels.get(channel).ok_or_else(|| error("selected source channel unavailable"))?;
        // Correlation is gain invariant. Scale safely in f64 before the DSP's normalization so
        // even extreme finite float headroom cannot overflow intermediate f32 subtraction.
        data.extend(selected.iter().map(|x| (f64::from(*x) / stats.peak) as f32));
        at += n;
    }
    let first = Tick::from_units(start, i64::from(RATE));
    Ok(Waveform {
        data,
        channel,
        first,
        peak: stats.peak,
        coverage: json!({"sourceStart":first.0,"sourceEnd":Tick::from_units(end,i64::from(RATE)).0,
            "requestedSourceStart":clip.source_in.0,"samples":count,"stream":clip.audio_stream,
            "sourceChannel":channel,"peak":stats.peak,"acRms":stats.ac_rms,"originalResolution":true,"effects":"pre"}),
    })
}

struct Move {
    id: ClipId,
    track: TrackId,
    start: Tick,
    duration: Tick,
    source_in: Tick,
}
struct Alignment {
    sequence: ItemId,
    moves: Vec<Move>,
    muted: Vec<ClipId>,
    report: Value,
    changed: bool,
}
fn alignment(s: &Session, p: &AlignParams) -> Result<Alignment> {
    let (sequence, reference, target) = endpoints(s, p.reference, p.clip, false)?;
    let delta = tick(i128::from(p.offset_ticks))?;
    let seq = s.project.sequence(sequence).ok_or(crate::EngineError::NoSequence)?;
    let mut moved = BTreeSet::from([target.id]);
    if let Some(link) = target.link {
        for it in seq.all_tracks().flat_map(|t| &t.items).filter(|it| it.link == Some(link)) {
            moved.insert(it.id);
            if moved.len() > MAX_LINKED {
                return Err(error("linked target group exceeds 256 clips; unlink a smaller group before syncing"));
            }
        }
    }
    if moved.contains(&reference.id) {
        return Err(error("reference belongs to the target's linked group; unlink the external microphone first so the camera stays fixed"));
    }
    let (mut moves, mut changes, mut warnings) = (Vec::new(), Vec::new(), Vec::new());
    for id in &moved {
        let (track_id, it) = locate(seq, id.0)?;
        source_range(&s.project, it)?;
        let track = seq.track(track_id).ok_or_else(|| error("track disappeared"))?;
        if track.locked {
            return Err(error(format!("track {} is locked; unlock it before syncing linked clip {}", track.name, id.0)));
        }
        let raw_start = tick(i128::from(it.start.0) + i128::from(delta.0))?;
        let trim = if raw_start < Tick::ZERO { Tick(raw_start.0.checked_neg().ok_or_else(|| error("leading trim overflow"))?) } else { Tick::ZERO };
        if trim >= it.duration {
            return Err(error(format!("sync would remove all of clip {}; use a different anchor or trim the linked group first", id.0)));
        }
        let start = raw_start.max(Tick::ZERO);
        let duration = tick(i128::from(it.duration.0) - i128::from(trim.0))?;
        let source_in = tick(i128::from(it.source_in.0) + i128::from(trim.0))?;
        let after = range(start, duration)?;
        let before = range(it.start, it.duration)?;
        for transition in &track.transitions {
            let transition_range = range(transition.start, transition.duration)?;
            if transition.from == Some(*id)
                || transition.to == Some(*id)
                || [before, after].iter().any(|span| transition_range.start <= span.end() && transition_range.end() >= span.start)
            {
                return Err(error(format!("transition {} touches moved clip {}; remove or revise that transition before syncing", transition.id.0, id.0)));
            }
        }
        for other in track.items.iter().filter(|other| !moved.contains(&other.id)) {
            if after.overlaps(&range(other.start, other.duration)?) {
                return Err(error(format!("sync would overlap clip {} on track {}; move the obstruction or use a separate track", other.id.0, track.name)));
            }
        }
        if trim > Tick::ZERO {
            warnings.push(warning(
                "leading_trim",
                format!(
                    "Clip {} loses exactly {} leading source/timeline ticks ({} at 48 kHz) to keep its timeline start at zero",
                    id.0,
                    trim.0,
                    trim.to_units_floor(i64::from(RATE))
                ),
            ));
        }
        changes.push(json!({"clip":id.0,"track":track_id.0,"before":{"start":it.start.0,"duration":it.duration.0,"sourceIn":it.source_in.0},
            "after":{"start":start.0,"duration":duration.0,"sourceIn":source_in.0},"trimmedLeadingTicks":trim.0}));
        moves.push(Move { id: *id, track: track_id, start, duration, source_in });
    }
    let mut muted = Vec::new();
    if p.mute_reference {
        let reference_is_audio = seq.audio_tracks.iter().any(|track| track.item(reference.id).is_some());
        let reference_source = s.project.resolve_media(reference.item).map(|(id, _, _)| id);
        for track in &seq.audio_tracks {
            for it in &track.items {
                // An explicitly selected audio reference mutes only that clip. For video,
                // mute only linked scratch audio in the selected stream of that same source.
                let selected = if reference_is_audio {
                    it.id == reference.id
                } else {
                    reference.link.is_some()
                        && it.link == reference.link
                        && it.audio_stream == reference.audio_stream
                        && s.project.resolve_media(it.item).map(|(id, _, _)| id) == reference_source
                };
                if selected {
                    if moved.contains(&it.id) {
                        return Err(error("camera/reference audio is in the moved group; unlink the external microphone first"));
                    }
                    if track.locked {
                        return Err(error(format!("camera/reference audio track {} is locked; unlock it or disable muteReference", track.name)));
                    }
                    muted.push(it.id);
                    if muted.len() > MAX_LINKED {
                        return Err(error("reference audio link group exceeds 256 clips"));
                    }
                }
            }
        }
        warnings.push(if muted.is_empty() {
            warning("no_reference_audio", "Reference has no audio-track clip to mute; camera video remains enabled")
        } else {
            warning(
                "mute_reference_audio",
                format!("Disable only reference/camera audio clips {:?}; camera video stays enabled", muted.iter().map(|c| c.0).collect::<Vec<_>>()),
            )
        });
    }
    if moves.len() > 1 {
        warnings.push(warning("linked_movement", format!("Move {} linked clips together; the reference/camera remains fixed", moves.len())));
    }
    let changed = delta != Tick::ZERO || muted.iter().any(|id| seq.find_item(*id).is_some_and(|(_, it)| it.enabled));
    let report = json!({"reference":reference.id.0,"clip":target.id.0,"sequence":sequence.0,"offsetTicks":delta.0,
        "movedClips":changes,"mutedReferenceClips":muted.iter().map(|c|c.0).collect::<Vec<_>>(),"warnings":warnings,"noChange":!changed});
    Ok(Alignment { sequence, moves, muted, report, changed })
}

fn analyze(s: &Session, p: &Params) -> Result<Value> {
    let window = p.window_seconds.unwrap_or(30.0);
    let max_lag = p.max_lag_seconds.unwrap_or(30.0);
    if !window.is_finite()
        || !(0.1..=MAX_SECONDS).contains(&window)
        || !max_lag.is_finite()
        || !(0.0..=MAX_SECONDS).contains(&max_lag)
        || !(-MAX_OFFSET_SAMPLES..=MAX_OFFSET_SAMPLES).contains(&p.offset_samples)
    {
        return Err(error("windowSeconds must be 0.1..120, maxLagSeconds 0..120, offsetSamples within ±5760000 at 48000 Hz"));
    }
    let (sequence, reference, target) = endpoints(s, p.reference, p.clip, matches!(p.method, Method::Waveform))?;
    let correction = Tick::from_units(p.offset_samples, i64::from(RATE));
    let (mut warnings, mut coverage) = (Vec::new(), json!({"sampleRate":RATE,"effects":"pre","originalResolution":true}));
    let mut verification = json!({"status":"notMeasured","method":"manualAnchors"});
    if s.project.sequence(sequence).and_then(|q| q.find_item(reference.id)).is_some_and(|(_, it)| it.audio_stream != reference.audio_stream) {
        warnings.push(warning("reference_stream", "Video reference uses native audio stream1; microphone identity is unknown. Select an explicit audio timeline clip to choose the reference recording. Stream0 stays enabled"));
    }
    let (delta, lag, confidence, distinct, reliable, inverted) = match p.method {
        Method::Manual => {
            let r = Tick(p.reference_time.ok_or_else(|| error("manual sync requires referenceTime in source ticks"))?);
            let t = Tick(p.clip_time.ok_or_else(|| error("manual sync requires clipTime in source ticks"))?);
            if !source_range(&s.project, &reference)?.contains(r) || !source_range(&s.project, &target)?.contains(t) {
                return Err(error("manual anchors must be inside the clips' used half-open SOURCE ranges (end is excluded)"));
            }
            let delta = tick(
                i128::from(reference.start.0) + i128::from(r.0) - i128::from(reference.source_in.0) - i128::from(target.start.0) - i128::from(t.0)
                    + i128::from(target.source_in.0)
                    + i128::from(correction.0),
            )?;
            coverage["referenceAnchor"] = json!(r.0);
            coverage["targetAnchor"] = json!(t.0);
            warnings.push(warning("manual_anchors", "Alignment uses your exact source anchors; waveform confidence was not measured"));
            (Some(delta), None, None, None, true, false)
        }
        Method::Waveform => {
            if p.reference_time.is_some() || p.clip_time.is_some() {
                return Err(error("referenceTime/clipTime require method manual; waveform analyzes the used source ranges from their starts"));
            }
            let r = waveform(s, sequence, &reference, window)?;
            let t = waveform(s, sequence, &target, window)?;
            coverage["reference"] = r.coverage;
            coverage["target"] = t.coverage;
            coverage["windowSeconds"] = json!(window);
            coverage["maxLagSeconds"] = json!(max_lag);
            warnings.push(warning(
                "waveform_window",
                "Measured the full waveform from each clip's source In; no semantic clap recognition. A clap/event must occur in both analyzed windows",
            ));
            if r.peak > 1.0 || t.peak > 1.0 {
                warnings
                    .push(warning("float_headroom", "Original samples above nominal full scale are preserved; correlation normalizes analysis copies only"));
            }
            let max_lag_samples = (max_lag * f64::from(RATE)).round() as i64;
            let result = find_offset(&r.data, &t.data, RATE, &SyncOptions { max_lag: Some(max_lag_samples), ..Default::default() });
            coverage["referenceChannel"] = json!(r.channel);
            coverage["targetChannel"] = json!(t.channel);
            if let Some(result) = result {
                let (verified, measured) = verify_waveform(&r.data, &t.data, result.lag);
                verification = measured;
                let delta = tick(
                    i128::from(reference.start.0) - i128::from(target.start.0)
                        + i128::from(Tick::from_units(result.lag, i64::from(RATE)).0)
                        + i128::from(r.first.0)
                        - i128::from(reference.source_in.0)
                        - i128::from(t.first.0)
                        + i128::from(target.source_in.0)
                        + i128::from(correction.0),
                )?;
                let boundary = max_lag_samples > 0 && result.lag.unsigned_abs() >= max_lag_samples as u64;
                let reliable = result.reliable() && result.confidence.is_finite() && result.distinct.is_finite() && !boundary && verified;
                if !reliable {
                    warnings.push(warning(
                        "ambiguous_waveform",
                        "No distinct confident waveform alignment; choose manual source anchors, a longer shared event window, or a wider lag search",
                    ));
                }
                if !verified {
                    warnings.push(if verification["status"] == "periodicAmbiguity" {
                        warning(
                            "periodic_waveform",
                            "Both recordings have strongly repeated waveforms; their timing is intrinsically ambiguous. Use manual source anchors",
                        )
                    } else {
                        warning(
                            "poor_aligned_waveform",
                            "The proposed GCC lag does not have sufficient normalized aligned waveform correlation; use manual source anchors",
                        )
                    });
                }
                if result.inverted && verified {
                    warnings.push(warning("opposite_polarity", "Recordings correlate with opposite polarity; sync does not invert either recording"));
                }
                (Some(delta), Some(result.lag), Some(result.confidence), Some(result.distinct), reliable, result.inverted)
            } else {
                verification = json!({"status":"noCandidate","method":"dcRemovedPearson"});
                warnings.push(warning("no_common_waveform", "No common waveform alignment in these bounded windows; choose manual source anchors"));
                (None, None, None, None, false, false)
            }
        }
    };
    let planned = if reliable {
        let delta = delta.ok_or_else(|| error("reliable alignment has no offset"))?;
        let planned = alignment(s, &AlignParams { reference: p.reference, clip: p.clip, offset_ticks: delta.0, mute_reference: p.mute_reference })?;
        if let Some(w) = planned.report["warnings"].as_array() {
            warnings.extend(w.iter().cloned());
        }
        Some(planned.report)
    } else {
        None
    };
    Ok(json!({"status":if reliable {"ready"}else{"needsManual"},"method":if matches!(p.method,Method::Manual){"manual"}else{"waveform"},
        "offsetTicks":delta.map(|d|d.0),"samples":delta.map(|d|d.to_units_floor(i64::from(RATE))),"sampleRate":RATE,
        "lagSamples":lag,"confidence":confidence,"distinct":distinct,"inverted":inverted,
        "referenceChannel":coverage.get("referenceChannel"),"targetChannel":coverage.get("targetChannel"),
        "reference":metadata(&reference),"target":metadata(&target),"coverage":coverage,"verification":verification,"warnings":warnings,"alignment":planned}))
}

fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    analyze(s, &bounded_decode(p, "audio.sync.inspect")?)
}
fn prepare(s: &mut Session, p: &Value) -> Result<Value> {
    let p: Params = bounded_decode(p, "audio.sync.prepare")?;
    let evidence = analyze(s, &p)?;
    if evidence["status"] != "ready" {
        return Err(error(
            "needsManual: waveform alignment is ambiguous; inspect the evidence and supply method manual with referenceTime/clipTime SOURCE ticks",
        ));
    }
    let offset = evidence["offsetTicks"].as_i64().ok_or_else(|| error("alignment has no exact offset"))?;
    let plan = s.execute(
        "agent.plan",
        json!({"label":"Synchronize external microphone","baseRevision":s.revision,
        "steps":[{"id":"audio.sync.align","params":{"reference":p.reference,"clip":p.clip,"offsetTicks":offset,"muteReference":p.mute_reference}}]}),
    )?;
    let id = plan["plan"].as_u64().ok_or_else(|| error("prepared edit has no plan ID"))?;
    s.agent_annotate_plan(id, &json!({"preparation":{"kind":"audio.sync","evidence":evidence}}))
}
fn align(s: &mut Session, p: &Value) -> Result<Value> {
    let p: AlignParams = bounded_decode(p, "audio.sync.align")?;
    let planned = alignment(s, &p)?;
    if !planned.changed {
        return Ok(planned.report);
    }
    s.edit("Synchronize external microphone", |project, _| {
        let seq = project.sequence_mut(planned.sequence).ok_or(crate::EngineError::NoSequence)?;
        for moved in &planned.moves {
            let it = seq.track_mut(moved.track).and_then(|t| t.item_mut(moved.id)).ok_or_else(|| error("clip disappeared before sync commit"))?;
            it.start = moved.start;
            it.duration = moved.duration;
            it.source_in = moved.source_in;
        }
        for id in &planned.muted {
            let (_, it) = seq.find_item_mut(*id).ok_or_else(|| error("reference audio disappeared before sync commit"))?;
            it.enabled = false;
        }
        let tracks: BTreeSet<_> = planned.moves.iter().map(|m| m.track).collect();
        for id in tracks {
            if let Some(track) = seq.track_mut(id) {
                track.items.sort_by_key(|it| it.start);
            }
        }
        Ok(())
    })?;
    Ok(planned.report)
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    let params = r#"{"reference":clipId,"clip":clipId,"method":"waveform|manual"="waveform","referenceTime":sourceTicks?,"clipTime":sourceTicks?,"windowSeconds":0.1..120=30,"maxLagSeconds":0..120=30,"offsetSamples":i64=0,"muteReference":bool=false}"#;
    vec![
        CommandSpec {
            id: "audio.sync.inspect",
            label: "Inspect external microphone synchronization",
            menu: &[],
            shortcut: None,
            params,
            enabled: always,
            run: inspect,
            journal: false,
        },
        CommandSpec {
            id: "audio.sync.prepare",
            label: "Propose external microphone synchronization",
            menu: &[],
            shortcut: None,
            params,
            enabled: always,
            run: prepare,
            journal: false,
        },
        CommandSpec {
            id: "audio.sync.align",
            label: "Align external microphone clips",
            menu: &[],
            shortcut: None,
            params: r#"{"reference":clipId,"clip":clipId,"offsetTicks":ticks,"muteReference":bool=false}"#,
            enabled: always,
            run: align,
            journal: true,
        },
    ]
}

#[cfg(test)]
mod tests;
