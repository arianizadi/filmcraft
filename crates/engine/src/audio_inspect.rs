//! Bounded, immutable native audio measurements and unclipped float-WAV auditions.
use crate::{
    Result, Session,
    analysis_support::{StrictProvider, SubjectArgs, Target, bounded_decode},
    commands::{CommandSpec, always, bad},
};
use base64::Engine as _;
use filmcraft_frame::AudioBuffer;
use filmcraft_project::{ItemId, ItemKind, Sequence, Track, TrackKind};
use filmcraft_render::SourceProvider;
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::Ordering},
};
const RATE: u32 = 48_000;
const BLOCK: usize = 4096;
const MAX_SAMPLE_VALUES: u64 = 256 * 1024 * 1024;
const MAX_TASKS: usize = 32;
fn after() -> String {
    "after".into()
}
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    #[serde(default)]
    subject: SubjectArgs,
    start: Option<i64>,
    end: Option<i64>,
    stream: Option<u32>,
    #[serde(default = "after")]
    snapshot: String,
    #[serde(default)]
    compare: bool,
}
fn err(e: impl std::fmt::Display) -> crate::EngineError {
    bad("audio.inspect", e.to_string())
}
fn number(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}
fn db(v: f64) -> Option<f64> {
    (v > 0.0 && v.is_finite()).then(|| 20.0 * v.log10()).and_then(number)
}
fn cancelled(progress: Option<&filmcraft_export::Progress>) -> Result<()> {
    if progress.is_some_and(|p| p.cancel.load(Ordering::Relaxed)) { Err(err("cancelled; project unchanged")) } else { Ok(()) }
}
fn resolve(s: &Session, p: &Request, before: bool) -> Result<Target> {
    if !matches!(p.snapshot.as_str(), "before" | "after") {
        return Err(err("snapshot must be before or after"));
    }
    if (before || p.compare) && p.subject.plan.is_none() {
        return Err(err("before/comparison requires a frozen edit plan"));
    }
    let mut args = p.subject.clone();
    args.effects.get_or_insert_with(|| "post".into());
    if let Some(plan) = args.plan {
        let (project, state, revision) = s.agent_plan_snapshot(plan, before)?;
        let mut reader = Session::new(s.services.clone());
        reader.project = project;
        reader.state = state;
        reader.revision = revision;
        args.plan = None;
        let mut target = Target::resolve(&reader, &args)?;
        target.args.plan = Some(plan);
        Ok(target)
    } else {
        Target::resolve(s, &args)
    }
}
fn source_format(info: &filmcraft_media::AudioStreamInfo) -> Value {
    let codec: String = info.codec.chars().take(1024).collect();
    let stored = if codec.to_ascii_lowercase().contains("float") {
        Some("floating_point")
    } else if codec == "PCM" {
        Some("integer_pcm")
    } else {
        None
    };
    json!({"codec":codec,"bitDepth":info.bits_per_sample,"sampleFormat":stored,"nativeSampleRate":info.sample_rate,"nativeChannels":info.channels,"decodedSampleFormat":"float32","irreversibleClippingAssessed":false})
}
fn routing_values(effect: &filmcraft_project::EffectInstance, time: Tick) -> Value {
    json!({"mode":number(effect.f64_at("mode",time)),"gain_l_db":number(effect.f64_at("gain_l_db",time)),"gain_r_db":number(effect.f64_at("gain_r_db",time)),"invert_l":number(effect.f64_at("invert_l",time)),"invert_r":number(effect.f64_at("invert_r",time))})
}
// The renderer intentionally skips no-audio sources. An inspector must distinguish an actual
// video-only clip from a previously-declared audio stream that disappeared after import.
struct CheckedProvider<'a> {
    inner: &'a StrictProvider,
    project: &'a filmcraft_project::Project,
    error: Mutex<Option<String>>,
}
impl SourceProvider for CheckedProvider<'_> {
    fn source(&self, id: ItemId) -> Option<filmcraft_media::SharedSource> {
        let source = self.inner.source(id)?;
        if source.info().audio_streams.iter().any(|a| a.channels > 64 || filmcraft_media::bounded_audio_dimensions(a.sample_rate, a.channels).is_err()) {
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("source {} audio metadata exceeds inspection channel/rate limits", id.0));
            return None;
        }
        if self.project.resolve_media(id).is_some_and(|(_, m, _)| m.info.has_audio()) && !source.info().has_audio() {
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("source {} no longer has its declared audio stream", id.0));
            return None;
        }
        Some(source)
    }
}
impl CheckedProvider<'_> {
    fn check(&self) -> Result<()> {
        self.inner.check()?;
        if let Some(message) = self.error.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            return Err(err(message));
        }
        Ok(())
    }
}
enum Signal {
    Source { id: ItemId, stream: usize },
    Clip(Box<Track>),
    Sequence(Arc<Sequence>),
}
struct Reader {
    target: Target,
    provider: StrictProvider,
    signal: Signal,
    start: Tick,
    end: Tick,
    sample_start: i64,
    frames: u64,
    routing: Value,
    before: bool,
}
impl Reader {
    fn new(s: &Session, request: &Request, preview: bool, before: bool) -> Result<Self> {
        let target = resolve(s, request, before)?;
        if !target.audio {
            return Err(err("subject has no audio"));
        }
        let start = request.start.map(Tick).unwrap_or(target.first);
        let default_duration = Tick::from_units(if preview { 5 } else { 30 }, 1);
        let default_end = Tick(start.0.checked_add(default_duration.0).ok_or_else(|| err("range overflows"))?).min(target.end);
        let (start, end) = target.window(Some(start.0), Some(request.end.unwrap_or(default_end.0)))?;
        if preview && end.0 - start.0 > Tick::from_units(10, 1).0 {
            return Err(err("preview at most 10 seconds"));
        }
        let sample_start = start.to_units_floor(i64::from(RATE));
        let sample_end = end.to_units_floor(i64::from(RATE));
        let frames = u64::try_from(sample_end.checked_sub(sample_start).ok_or_else(|| err("audio range overflows"))?).map_err(err)?;
        if frames == 0 {
            return Err(err("range contains no complete audio samples"));
        }
        let pre = target.args.effects.as_deref() == Some("pre");
        let provider = target.audio_reader(s);
        let (signal, routing) = if let Some(id) = target.item {
            let stream = usize::try_from(request.stream.unwrap_or(0)).map_err(err)?;
            let source = provider.source(id).ok_or_else(|| err("source audio unavailable"))?;
            provider.check()?;
            let info = source.info().audio_streams.get(stream).ok_or_else(|| err("requested audio stream does not exist"))?;
            if info.channels == 0 || info.channels > 64 {
                return Err(err("audio stream channel count must be 1..64"));
            }
            filmcraft_media::bounded_audio_dimensions(info.sample_rate, info.channels)?;
            (
                Signal::Source { id, stream },
                json!({"stream":stream,"nativeChannels":info.channels,"sourceFormat":source_format(info),"sourceChannels":null,"boundary":"native selected source stream; no timeline routing, effects or gain"}),
            )
        } else if let Some(it) = &target.clip {
            if request.stream.is_some() {
                return Err(err("stream override is source-only; a clip uses its selected audioStream"));
            }
            let q = target.sequence.and_then(|id| target.project.sequence(id)).ok_or_else(|| err("sequence unavailable"))?;
            let (tid, _) = q.find_item(it.id).ok_or_else(|| err("clip unavailable"))?;
            let original = q.track(tid).ok_or_else(|| err("track unavailable"))?;
            let mut track = Track::new(tid, TrackKind::Audio, String::new());
            track.channels = original.channels;
            let source = provider.source(it.item).ok_or_else(|| err("clip source audio unavailable; nested clip audio is unsupported"))?;
            provider.check()?;
            let info = source.info().audio_streams.get(it.audio_stream).ok_or_else(|| err("clip's selected audio stream does not exist"))?;
            if info.channels == 0 || info.channels > 64 {
                return Err(err("clip stream channel count must be 1..64"));
            }
            filmcraft_media::bounded_audio_dimensions(info.sample_rate, info.channels)?;
            if it.source_channels.len() > 64 || it.source_channels.iter().any(|&channel| u32::from(channel) >= info.channels) {
                return Err(err("clip source-channel mapping is outside the selected stream"));
            }
            if !it.speed.is_finite() || it.speed == 0.0 || it.source_in < Tick::ZERO {
                return Err(err("clip source timing/speed is invalid"));
            }
            let routing = json!({"stream":it.audio_stream,"nativeChannels":info.channels,"sourceFormat":source_format(info),"sourceChannels":it.source_channels,"clipGainDb":it.gain_db,"speed":it.speed,"reverse":it.reverse,"routingEffects":it.effects.iter().filter(|e|e.effect=="channel_router").take(16).map(|e|json!({"effectInstanceId":e.instance_id,"enabled":e.enabled,"animated":e.is_animated(),"valuesAtStart":routing_values(e,it.source_time_at(start))})).collect::<Vec<_>>(),"boundary":if pre {"isolated clip before clip gain/effects/volume/pan; source-channel mapping and speed retained"}else{"isolated clip including clip gain/effects/volume/pan; excludes track/master mixing and transitions"}});
            track.items = vec![it.clone()];
            track.transitions.clear();
            track.kind = TrackKind::Audio;
            if pre {
                for it in &mut track.items {
                    it.effects.clear();
                    it.gain_db = 0.0;
                }
            }
            (Signal::Clip(Box::new(track)), routing)
        } else {
            if request.stream.is_some() {
                return Err(err("stream override is source-only"));
            }
            let mut q = target
                .sequence
                .and_then(|id| target.project.item(id))
                .and_then(|item| match &item.kind {
                    ItemKind::Sequence(q) => Some(q.clone()),
                    _ => None,
                })
                .ok_or_else(|| err("sequence unavailable"))?;
            if q.settings.sample_rate != RATE || pre {
                Arc::make_mut(&mut q).settings.sample_rate = RATE;
            }
            if pre {
                let q = Arc::make_mut(&mut q);
                q.master_effects.clear();
                for track in q.all_tracks_mut() {
                    track.effects.clear();
                    for it in &mut track.items {
                        it.effects.clear();
                        it.gain_db = 0.0;
                    }
                }
                for track in &mut q.submix_tracks {
                    track.effects.clear();
                }
            }
            (
                Signal::Sequence(q),
                json!({"boundary":if pre {"sequence stereo monitor mix without clip gain/effects and track/master inserts; faders, pans, transitions and channel mappings retained"}else{"sequence stereo monitor mix including clip, track, submix and master processing"}}),
            )
        };
        Ok(Self { target, provider, signal, start, end, sample_start, frames, routing, before })
    }
    fn block(&self, offset: u64, n: usize) -> Result<AudioBuffer> {
        let at = self.sample_start.checked_add(i64::try_from(offset).map_err(err)?).ok_or_else(|| err("sample position overflows"))?;
        at.checked_add(i64::try_from(n).map_err(err)?).ok_or_else(|| err("sample range overflows"))?;
        let provider = CheckedProvider { inner: &self.provider, project: &self.target.project, error: Mutex::new(None) };
        let out = match &self.signal {
            Signal::Source { id, stream } => provider.source(*id).ok_or_else(|| err("source unavailable"))?.audio_stream(*stream, at, n, RATE)?,
            Signal::Clip(track) => filmcraft_render::audio::track_input(&self.target.project, track, at, n, RATE, &provider),
            Signal::Sequence(q) => filmcraft_render::audio::mix_sequence(&self.target.project, q, at, n, &provider),
        };
        provider.check()?;
        if out.sample_rate != RATE
            || out.channels.is_empty()
            || out.channels.len() > 64
            || out.channels.iter().any(|c| c.len() != n || c.iter().any(|x| !x.is_finite()))
        {
            return Err(err("decoder/mixer returned inconsistent or non-finite audio"));
        }
        filmcraft_media::validate_audio_buffer_shape(out.channels.len(), n)?;
        if self.frames.checked_mul(out.channels.len() as u64).is_none_or(|n| n > MAX_SAMPLE_VALUES) {
            return Err(err("audio window exceeds 268435456 channel-samples; select a smaller range"));
        }
        Ok(out)
    }
    fn metadata(&self) -> Value {
        json!({"revision":self.target.revision,"subject":self.target.args,"sequence":self.target.sequence.map(|i|i.0),"clip":self.target.clip.as_ref().map(|i|i.id.0),"snapshot":if self.before {"before"}else{"after"},"plan":self.target.args.plan,"effects":self.target.args.effects,"start":self.start.0,"end":self.end.0,"sampleStart":self.sample_start,"sampledStart":Tick::from_units(self.sample_start,i64::from(RATE)).0,"sampledEnd":Tick::from_units(self.sample_start.saturating_add(self.frames as i64),i64::from(RATE)).0,"sampleRate":RATE,"frames":self.frames,"routing":self.routing,"sourceFormat":self.routing.get("sourceFormat"),"startPosition":self.target.position(self.start),"endPosition":self.target.position(self.end),"projectChanged":false,"coverage":{"durationSeconds":self.frames as f64/f64::from(RATE),"subjectDurationSeconds":(self.target.end-self.target.first).0 as f64/TICKS_PER_SECOND as f64,"entireSubject":self.start==self.target.first&&self.end==self.target.end,"method":"continuous decoded samples in bounded 4096-frame blocks; no proxy audio"}})
    }
}
fn complete_true_peak(meter: &filmcraft_audio_dsp::LoudnessMeter) -> Option<f64> {
    // Drain only a copy: the FIR tail belongs to the measured window, zero padding must not
    // create an extra gated loudness block in the actual program measurement.
    let mut tail = meter.clone();
    let zero = [0.0f32; filmcraft_audio_dsp::oversample::TAPS_PER_PHASE];
    let channels = vec![zero.as_slice(); meter.channels()];
    tail.process(&channels);
    number(tail.true_peak_dbtp())
}
fn loudness_range(meter: &filmcraft_audio_dsp::LoudnessMeter, frames: u64) -> Option<f64> {
    if frames < u64::from(RATE) * 3 || !meter.integrated().is_finite() { None } else { number(meter.loudness_range()) }
}
fn inspect_reader(reader: &Reader, progress: Option<&filmcraft_export::Progress>) -> Result<Value> {
    use filmcraft_audio_dsp::{LoudnessMeter, inspect::Analyzer};
    let mut offset = 0u64;
    let mut analyzer = None;
    let mut meters: Vec<LoudnessMeter> = Vec::new();
    let mut full_meter = None;
    while offset < reader.frames {
        cancelled(progress)?;
        let n = usize::try_from((reader.frames - offset).min(BLOCK as u64)).map_err(err)?;
        let buffer = reader.block(offset, n)?;
        let slices: Vec<&[f32]> = buffer.channels.iter().map(Vec::as_slice).collect();
        if analyzer.is_none() {
            analyzer = Some(Analyzer::new(slices.len()).map_err(err)?);
            meters = (0..slices.len()).map(|_| LoudnessMeter::new(f64::from(RATE), 1)).collect();
            full_meter = Some(LoudnessMeter::new(f64::from(RATE), slices.len()));
        }
        let analyzer = analyzer.as_mut().ok_or_else(|| err("audio analyzer unavailable"))?;
        analyzer.push(&slices).map_err(err)?;
        for (meter, channel) in meters.iter_mut().zip(&slices) {
            meter.process(&[*channel]);
        }
        if let Some(meter) = &mut full_meter {
            meter.process(&slices);
        }
        offset = offset.checked_add(n as u64).ok_or_else(|| err("sample counter overflows"))?;
        if let Some(p) = progress {
            p.done.fetch_add(n as u64, Ordering::Relaxed);
        }
    }
    cancelled(progress)?;
    let stats = analyzer.ok_or_else(|| err("no audio was analyzed"))?.report();
    let mut value = reader.metadata();
    value["channels"]=json!(stats.channels.iter().zip(&meters).enumerate().map(|(i,(c,m))|json!({"channel":i,"samples":c.samples,"finiteSamples":c.finite_samples,"nonFiniteSamples":c.non_finite_samples,"peak":c.peak,"peakDbfs":db(c.peak),"rms":c.rms,"rmsDbfs":c.rms.and_then(db),"dcOffset":c.dc,"acRmsDbfs":c.ac_rms.and_then(db),"overFullScaleSamples":c.clipped_samples,"nearSilentSamples":c.near_silent_samples,"nearSilentFraction":if c.samples>0 {Some(c.near_silent_samples as f64/c.samples as f64)}else{None},"integratedLufs":number(m.integrated()),"loudnessRangeLu":loudness_range(m,reader.frames),"truePeakDbtp":complete_true_peak(m),"headroomDb":db(c.peak).map(|p|-p)})).collect::<Vec<_>>());
    value["loudness"]=full_meter.as_ref().map(|m|json!({"integratedLufs":number(m.integrated()),"momentaryLufs":(reader.frames>=u64::from(RATE)*4/10).then(||m.momentary()).and_then(number),"shortTermLufs":(reader.frames>=u64::from(RATE)*3).then(||m.short_term()).and_then(number),"loudnessRangeLu":loudness_range(m,reader.frames),"truePeakDbtp":complete_true_peak(m),"method":"ITU-R BS.1770-4; integrated requires a gated 400ms block, short-term requires3s; null means unavailable"})).unwrap_or(Value::Null);
    value["stereo"]=stats.stereo.as_ref().map(|st|json!({"correlation":st.correlation,"monoPowerRatio":st.mono_power_ratio,"monoLossDb":st.mono_power_ratio.and_then(|r|if r>0.0 {number(-10.0*r.log10())}else{None}),"completeMonoCancellation":st.mono_power_ratio==Some(0.0),"midRms":st.mid_rms,"sideRms":st.side_rms,"differenceRms":st.difference_rms,"definition":"M=(L+R)/2; monoLossDb=-10log10(Mpower/mean(Lpower,Rpower)); exact cancellation reports null dB plus completeMonoCancellation:true"})).unwrap_or(Value::Null);
    let e = &stats.evidence;
    let mut findings = Vec::new();
    for (kind, condition) in [
        ("silent", e.silent),
        ("left_only", e.left_only),
        ("right_only", e.right_only),
        ("dual_mono", e.dual_mono),
        ("inverted_polarity", e.inverted),
        ("uncorrelated_stereo", e.unrelated),
        ("dc_offset", e.dc_offset),
        ("insufficient_signal", e.insufficient_signal),
    ] {
        if condition {
            findings.push(json!({"kind":kind,"start":reader.start.0,"end":reader.end.0}));
        }
    }
    if stats.channels.iter().any(|c| c.clipped_samples > 0) {
        findings.push(json!({"kind":"samples_at_or_above_full_scale","start":reader.start.0,"end":reader.end.0}));
    }
    let (mode, reason) = if e.reliable && e.left_only {
        (Some("left"), "Left signal is present while the right channel is conservatively near-silent; audition routing left to both sides.")
    } else if e.reliable && e.right_only {
        (Some("right"), "Right signal is present while the left channel is conservatively near-silent; audition routing right to both sides.")
    } else {
        (None, "No clear one-sided microphone defect; do not automatically downmix or invert stereo.")
    };
    value["findings"] = json!(findings);
    value["recommendation"] = json!({"mode":mode,"confidence":if mode.is_some(){0.95}else{0.0},"reason":reason,"basis":"conservative signal thresholds on the declared analysis boundary; heuristic confidence is not a calibrated probability","analysisEffects":reader.target.args.effects,"reliable":e.reliable});
    value["limitations"] = json!([
        "Measurements do not identify speech, speaker, or musical intent. Silence may be intentional.",
        "Sample clipping does not prove how a recording was clipped before import; true peak can exceed sample peak.",
        "An isolated clip omits track/master processing and transitions. Sequence measurements are the stereo monitor mix.",
        "Existing channel mapping is retained even for pre-effects measurements. Source mode measures the selected native stream."
    ]);
    Ok(value)
}
fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let request: Request = bounded_decode(p, "audio.inspect")?;
    if request.compare {
        let before = Reader::new(s, &request, false, true)?;
        let after = Reader::new(s, &request, false, false)?;
        Ok(
            json!({"plan":request.subject.plan,"before":inspect_reader(&before,s.workflow_progress.as_deref())?,"after":inspect_reader(&after,s.workflow_progress.as_deref())?,"projectChanged":false}),
        )
    } else {
        inspect_reader(&Reader::new(s, &request, false, request.snapshot == "before")?, s.workflow_progress.as_deref())
    }
}
fn stereo(buffer: AudioBuffer) -> Result<AudioBuffer> {
    match buffer.channels.len() {
        1 | 2 | 6 => {
            Ok(filmcraft_render::audio::to_layout(buffer, filmcraft_audio_dsp::channels::Layout::Stereo, filmcraft_audio_dsp::channels::Mixdown::FrontRear))
        }
        _ => Err(err("preview supports mono, stereo or declared SMPTE 5.1 sources; select an isolated clip with explicit channel mapping for other layouts")),
    }
}
fn float_wav(channels: &[Vec<f32>]) -> Result<Vec<u8>> {
    if channels.len() != 2 {
        return Err(err("audition requires two output channels"));
    }
    let frames = channels.first().map(Vec::len).unwrap_or(0);
    if frames == 0 || frames > RATE as usize * 10 || channels.iter().any(|c| c.len() != frames || c.iter().any(|x| !x.is_finite())) {
        return Err(err("invalid preview audio shape"));
    }
    let bytes = frames.checked_mul(8).and_then(|n| u32::try_from(n).ok()).ok_or_else(|| err("WAV byte count overflows"))?;
    let mut wav = Vec::with_capacity(bytes as usize + 56);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&bytes.checked_add(48).ok_or_else(|| err("WAV length overflows"))?.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&3u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 8).to_le_bytes());
    wav.extend_from_slice(&8u16.to_le_bytes());
    wav.extend_from_slice(&32u16.to_le_bytes());
    wav.extend_from_slice(b"fact");
    wav.extend_from_slice(&4u32.to_le_bytes());
    wav.extend_from_slice(&(frames as u32).to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&bytes.to_le_bytes());
    for i in 0..frames {
        for c in channels {
            wav.extend_from_slice(&c.get(i).ok_or_else(|| err("WAV sample missing"))?.to_le_bytes());
        }
    }
    Ok(wav)
}
fn preview_reader(reader: &Reader, progress: Option<&filmcraft_export::Progress>) -> Result<Value> {
    let frames = usize::try_from(reader.frames).map_err(err)?;
    let mut channels = vec![Vec::with_capacity(frames), Vec::with_capacity(frames)];
    let mut offset = 0;
    let mut peak = 0.0f64;
    let mut clipped = 0u64;
    while offset < reader.frames {
        cancelled(progress)?;
        let n = usize::try_from((reader.frames - offset).min(BLOCK as u64)).map_err(err)?;
        let b = stereo(reader.block(offset, n)?)?;
        for (dst, src) in channels.iter_mut().zip(&b.channels) {
            for &x in src {
                peak = peak.max(f64::from(x).abs());
                clipped += u64::from(x.abs() >= 1.0);
            }
            dst.extend_from_slice(src);
        }
        offset += n as u64;
    }
    cancelled(progress)?;
    let wav = float_wav(&channels)?;
    let mut value = reader.metadata();
    value["audio"] = json!({"mimeType":"audio/wav","encoding":"base64","format":"ieee_float32","sampleRate":RATE,"channels":2,"frames":frames,"bytes":wav.len(),"base64":base64::engine::general_purpose::STANDARD.encode(wav),"layout":"stereo monitor audition; mono copied to both sides; 5.1 downmixed with BS.775 front+rear, LFE omitted"});
    value["headroom"] = json!({"peak":peak,"peakDbfs":db(peak),"headroomDb":db(peak).map(|p|-p),"samplesAtOrAboveFullScale":clipped,"floatSamplesPreserved":true,"normalized":false,"limiterApplied":false,"irreversibleClippingAssessed":false,"note":"Float WAV preserves samples above full scale; the listening device/player may clip. Lower monitor gain to audition safely."});
    Ok(value)
}
fn preview(s: &mut Session, p: &Value) -> Result<Value> {
    let request: Request = bounded_decode(p, "audio.preview")?;
    if request.compare {
        let before = Reader::new(s, &request, true, true)?;
        let after = Reader::new(s, &request, true, false)?;
        Ok(
            json!({"plan":request.subject.plan,"before":preview_reader(&before,s.workflow_progress.as_deref())?,"after":preview_reader(&after,s.workflow_progress.as_deref())?,"projectChanged":false}),
        )
    } else {
        preview_reader(&Reader::new(s, &request, true, request.snapshot == "before")?, s.workflow_progress.as_deref())
    }
}
#[derive(Default)]
pub struct Tasks {
    entries: BTreeMap<u64, Task>,
}
type AnalysisOutput = Arc<Mutex<Option<std::result::Result<Value, String>>>>;
struct Task {
    document: Arc<()>,
    revision: u64,
    job: crate::Job,
    output: AnalysisOutput,
}
fn status(s: &mut Session, p: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Query {
        job: u64,
    }
    let q: Query = bounded_decode(p, "audio.inspectStatus")?;
    let task = s
        .audio_inspect_tasks
        .entries
        .get(&q.job)
        .filter(|t| Arc::ptr_eq(&t.document, &s.project_instance))
        .ok_or_else(|| err("unknown audio analysis job for this document; inspect jobs.get for retained progress"))?;
    let output = task.output.lock().unwrap_or_else(|e| e.into_inner());
    let (state, result, error) = match output.as_ref() {
        Some(Ok(value)) => ("completed", Some(value.clone()), None),
        Some(Err(e)) => (if task.job.progress.cancel.load(Ordering::Relaxed) { "cancelled" } else { "failed" }, None, Some(e.clone())),
        None => {
            let error = task.job.progress.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
            (if error.is_some() { "failed" } else { "running" }, None, error)
        }
    };
    Ok(json!({"job":q.job,"status":state,"inputRevision":task.revision,"result":result,"error":error,"projectChanged":false}))
}
fn inspect_async(s: &mut Session, p: &Value) -> Result<Value> {
    let request: Request = bounded_decode(p, "audio.inspectAsync")?;
    let before = if request.compare { Some(Reader::new(s, &request, false, true)?) } else { None };
    let after = Reader::new(s, &request, false, request.snapshot == "before" && !request.compare)?;
    if s.audio_inspect_tasks.entries.values().filter(|t| !t.job.progress.finished.load(Ordering::Relaxed)).count() >= 2 {
        return Err(err("two audio inspections are already running; cancel or wait"));
    }
    while s.audio_inspect_tasks.entries.len() >= MAX_TASKS {
        let id = s
            .audio_inspect_tasks
            .entries
            .iter()
            .find(|(_, t)| t.job.progress.finished.load(Ordering::Relaxed))
            .map(|(id, _)| *id)
            .ok_or_else(|| err("analysis receipt capacity reached"))?;
        s.audio_inspect_tasks.entries.remove(&id);
    }
    let revision = after.target.revision;
    let mut provenance = Session::new(s.services.clone());
    provenance.project = after.target.project.clone();
    provenance.revision = revision;
    provenance.path = s.path.clone();
    provenance.operations.session_id = s.operations.session_id.clone();
    provenance.command_origin = Some("audio.inspectAsync".into());
    provenance.jobs = s.jobs.clone();
    let job = s.job_registry.new_job(&provenance, "Inspect audio snapshot".into())?;
    job.progress.total.store(after.frames + before.as_ref().map_or(0, |r| r.frames), Ordering::Relaxed);
    let id = job.id;
    let work = job.clone();
    let output = Arc::new(Mutex::new(None));
    let result = output.clone();
    let run = crate::durable_jobs::guard(s, &job, move || {
        let started = web_time::Instant::now();
        let response=filmcraft_media::cancel::with_cancel(&work.progress.cancel,||std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
            if let Some(before)=before {Ok(json!({"plan":after.target.args.plan,"before":inspect_reader(&before,Some(&work.progress))?,"after":inspect_reader(&after,Some(&work.progress))?,"projectChanged":false}))}else{inspect_reader(&after,Some(&work.progress))}
        })).unwrap_or_else(|_|Err(err("audio worker failed; live project unchanged")))).map_err(|e|e.to_string());
        if let Err(e) = &response {
            *work.progress.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.clone());
        }
        *work.progress.status.lock().unwrap_or_else(|p| p.into_inner()) =
            if response.is_ok() { "Audio inspection completed".into() } else { "Audio inspection failed or cancelled".into() };
        *work.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(
            response
                .as_ref()
                .map(|_| filmcraft_export::Report {
                    path: String::new(),
                    frames: 0,
                    seconds: started.elapsed().as_secs_f64(),
                    bytes: 0,
                    render_fps: 0.0,
                    extra_files: Vec::new(),
                })
                .map_err(Clone::clone),
        );
        *result.lock().unwrap_or_else(|p| p.into_inner()) = Some(response);
        work.progress.finished.store(true, Ordering::Relaxed);
    });
    s.jobs.push(job.clone());
    crate::durable_jobs::spawn(s, id, "filmcraft-audio-inspect", run)?;
    s.audio_inspect_tasks.entries.insert(id, Task { document: s.project_instance.clone(), revision, job, output });
    Ok(json!({"job":id,"status":"running","inputRevision":revision,"poll":"audio.inspectStatus","cancel":"jobs.cancel","projectChanged":false}))
}
pub fn commands() -> Vec<CommandSpec> {
    [("audio.inspect","Inspect Audio",inspect as fn(&mut Session,&Value)->Result<Value>),("audio.inspectAsync","Inspect Audio in Background",inspect_async),("audio.preview","Audition Audio Snapshot",preview),("audio.inspectStatus","Audio Inspection Result",status)].into_iter().map(|(id,label,run)|CommandSpec{id,label,menu:&[],shortcut:None,params:if id=="audio.inspectStatus" {r#"{"job":id}"#}else{r#"{"subject":{"target":"source|clip|sequence","item":id?,"clip":id?,"sequence":id?,"plan":id?,"effects":"pre|post"?},"start":ticks?,"end":ticks?,"snapshot":"before|after"?,"compare":bool=false,"stream":u32?}"#},enabled:always,journal:false,run}).collect()
}
#[cfg(test)]
mod tests;
