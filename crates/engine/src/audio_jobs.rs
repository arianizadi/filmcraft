//! Optional local dialogue processing and harmonic/percussive or center/side stem jobs.
//! Files stream through bounded blocks; originals and the live timeline are never rewritten.
use crate::{
    EngineError, Job, Result, Session,
    commands::{CommandSpec, always},
};
use filmcraft_media::SharedSource;
use filmcraft_project::ItemId;
use filmcraft_time::Tick;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Params {
    item: u64,
    path: Option<String>,
    directory: Option<String>,
    start: Option<i64>,
    end: Option<i64>,
    sample_rate: Option<u32>,
    stream: Option<usize>,
    method: Option<String>,
    #[serde(default)]
    wait: bool,
}
#[derive(Clone, Copy)]
enum Mode {
    Enhance,
    Hpss,
    MidSide,
}
impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Enhance => "Local dialogue enhancement",
            Self::Hpss => "Harmonic/percussive separation",
            Self::MidSide => "Center/side separation",
        }
    }
}
struct AudioTask {
    source: SharedSource,
    stream: usize,
    rate: u32,
    channels: usize,
    start: i64,
    frames: u64,
    mode: Mode,
    paths: Vec<String>,
    provenance: Value,
    services: Arc<dyn crate::Services>,
}
fn error(e: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("audio job: {e}"))
}

struct Output {
    file: Option<File>,
    temporary: PathBuf,
    destination: PathBuf,
}
impl Output {
    fn create(path: &str, id: u64, header: &[u8]) -> std::result::Result<Self, String> {
        let destination = PathBuf::from(path);
        if std::fs::symlink_metadata(&destination).is_ok() {
            return Err(format!("output already exists: {path}"));
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let temporary =
            destination.with_file_name(format!("{}.filmcraft-job-{id}.part", destination.file_name().ok_or("output needs a filename")?.to_string_lossy()));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&temporary).map_err(|e| format!("{}: {e}", temporary.display()))?;
        let mut result = Self { file: Some(file), temporary, destination };
        result.write(header)?;
        Ok(result)
    }
    fn write(&mut self, bytes: &[u8]) -> std::result::Result<(), String> {
        self.file.as_mut().ok_or("output already closed")?.write_all(bytes).map_err(|e| e.to_string())
    }
    fn publish(&mut self) -> std::result::Result<(), String> {
        self.file.take().ok_or("output already closed")?.sync_all().map_err(|e| e.to_string())?;
        // Same-directory hard-link publication is atomic and cannot overwrite an existing file.
        std::fs::hard_link(&self.temporary, &self.destination).map_err(|e| format!("could not publish {} exclusively: {e}", self.destination.display()))?;
        if let Err(e) = std::fs::remove_file(&self.temporary) {
            log::warn!("could not remove completed job temporary {}: {e}", self.temporary.display());
        }
        Ok(())
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.temporary);
    }
}
impl AudioTask {
    fn read(&self, start: i64, len: usize) -> std::result::Result<Vec<Vec<f32>>, String> {
        if len > filmcraft_audio_dsp::separate::MAX_BLOCK {
            return Err("audio block exceeds limit".into());
        }
        let leading = if start < 0 { usize::try_from(start.saturating_neg()).unwrap_or(len).min(len) } else { 0 };
        let mut result = vec![vec![0f32; len]; self.channels];
        if leading == len {
            return Ok(result);
        }
        let buffer = self.source.audio_stream(self.stream, start.max(0), len - leading, self.rate).map_err(|e| e.to_string())?;
        if buffer.sample_rate != self.rate
            || buffer.channels.len() != self.channels
            || buffer.channels.iter().any(|c| c.len() != len - leading || c.iter().any(|x| !x.is_finite()))
        {
            return Err("audio decoder returned invalid channel/sample data".into());
        }
        for (dst, src) in result.iter_mut().zip(&buffer.channels) {
            let dst = dst.get_mut(leading..).ok_or("audio padding exceeds block")?;
            dst.copy_from_slice(src);
        }
        Ok(result)
    }
    fn process(self, job: &Job) -> std::result::Result<filmcraft_export::Report, String> {
        let started = web_time::Instant::now();
        let header = filmcraft_export::pcm_header(
            filmcraft_export::PcmContainer::Wav,
            self.channels as u16,
            self.rate,
            filmcraft_export::SampleFormat::Float32,
            self.frames,
        )?;
        let mut outputs: Vec<Output> = self.paths.iter().map(|path| Output::create(path, job.id, &header)).collect::<std::result::Result<_, _>>()?;
        let mut effects = Vec::new();
        if matches!(self.mode, Mode::Enhance) {
            for id in ["denoise", "speech_enhance", "compressor"] {
                effects.push(filmcraft_audio_dsp::create_effect(id, self.rate as f32, self.channels).ok_or_else(|| format!("missing local effect {id}"))?);
            }
        }
        let latency = effects.iter().try_fold(0usize, |n, e| n.checked_add(e.latency()).ok_or("audio effect latency overflows"))?;
        if latency > self.rate as usize * 2 {
            return Err("audio effect latency exceeds two seconds".into());
        }
        let preroll = if effects.is_empty() { 0 } else { (self.rate / 2) as i64 };
        let mut skip = usize::try_from(preroll).map_err(|_| "audio preroll overflows")?.checked_add(latency).ok_or("audio preroll overflows")?;
        let mut consumed = 0u64;
        let mut written = 0u64;
        let mut total_bytes = 0u64;
        let total = self.frames.checked_add(skip as u64).ok_or("audio duration overflows")?;
        let mut previews = vec![Vec::<f32>::new(); outputs.len()];
        while consumed < total {
            if job.progress.cancel.load(Ordering::Relaxed) {
                return Err("cancelled; no incomplete output was published".into());
            }
            let count = usize::try_from((total - consumed).min(4096)).map_err(|_| "audio block overflows")?;
            let offset = i64::try_from(consumed).map_err(|_| "audio position overflows")?;
            let position = self.start.checked_sub(preroll).and_then(|s| s.checked_add(offset)).ok_or("audio position overflows")?;
            let mut input = self.read(position, count)?;
            let stems = match self.mode {
                Mode::Enhance => {
                    for effect in &mut effects {
                        let mut channels: Vec<&mut [f32]> = input.iter_mut().map(Vec::as_mut_slice).collect();
                        effect.process(&mut channels);
                    }
                    vec![input]
                }
                Mode::MidSide => {
                    let (a, b) = filmcraft_audio_dsp::separate::mid_side(&input)?;
                    vec![a, b]
                }
                Mode::Hpss => {
                    let context = filmcraft_audio_dsp::separate::CONTEXT;
                    let window = self.read(position.checked_sub(context as i64).ok_or("audio context overflows")?, count + 2 * context)?;
                    let (a, b) = filmcraft_audio_dsp::separate::hpss(&window)?;
                    [a, b]
                        .into_iter()
                        .map(|channels| {
                            channels
                                .into_iter()
                                .map(|c| c.get(context..context + count).map(<[f32]>::to_vec).ok_or_else(|| "missing separated samples".to_string()))
                                .collect::<std::result::Result<Vec<_>, _>>()
                        })
                        .collect::<std::result::Result<Vec<_>, _>>()?
                }
            };
            let trim = skip.min(count);
            skip -= trim;
            let emit = (count - trim).min(usize::try_from(self.frames - written).map_err(|_| "audio remaining length overflows")?);
            for (index, (output, channels)) in outputs.iter_mut().zip(stems).enumerate() {
                let mut bytes = Vec::with_capacity(emit * self.channels * 4);
                let preview = previews.get_mut(index).ok_or("missing preview output")?;
                for i in trim..trim + emit {
                    for channel in &channels {
                        let sample = *channel.get(i).ok_or("audio output was shorter than requested")?;
                        if !sample.is_finite() {
                            return Err("effect produced a non-finite sample".into());
                        }
                        bytes.extend(sample.to_le_bytes());
                        if preview.len() < (self.rate as usize).saturating_mul(self.channels).saturating_mul(3) {
                            preview.push(sample);
                        }
                    }
                }
                output.write(&bytes)?;
                total_bytes = total_bytes.saturating_add(bytes.len() as u64);
            }
            written = written.saturating_add(emit as u64);
            consumed = consumed.saturating_add(count as u64);
            job.progress.done.store(written, Ordering::Relaxed);
        }
        if written != self.frames {
            return Err("audio output sample count does not match requested range".into());
        }
        let mut published = Vec::new();
        for output in &mut outputs {
            output.publish().map_err(|e| format!("{e}; completed outputs: {published:?}"))?;
            published.push(output.destination.to_string_lossy().into_owned());
        }
        let mut extra = self.paths.iter().skip(1).cloned().collect::<Vec<_>>();
        for (path, preview) in self.paths.iter().zip(previews) {
            let preview_path = format!("{path}.preview.wav");
            let bytes = filmcraft_export::write_wav(&preview, self.channels as u16, self.rate, filmcraft_export::SampleFormat::Float32)?;
            self.services.write_file_new(&preview_path, &bytes).map_err(|e| format!("{preview_path}: {e}; completed outputs: {published:?}"))?;
            extra.push(preview_path);
        }
        let provenance_path = format!("{}.provenance.json", self.paths.first().ok_or("missing primary output")?);
        self.services
            .write_file_new(&provenance_path, &serde_json::to_vec_pretty(&self.provenance).map_err(|e| e.to_string())?)
            .map_err(|e| format!("{provenance_path}: {e}; completed outputs: {published:?}"))?;
        extra.push(provenance_path);
        Ok(filmcraft_export::Report {
            path: self.paths.first().cloned().unwrap_or_default(),
            frames: self.frames,
            seconds: started.elapsed().as_secs_f64(),
            bytes: total_bytes,
            render_fps: 0.0,
            extra_files: extra,
        })
    }
}
fn run(s: &mut Session, p: &Value, separate: bool) -> Result<Value> {
    if cfg!(target_arch = "wasm32") || s.services.export_in_memory() {
        return Err(error("streamed local audio jobs need native filesystem output"));
    }
    crate::agent_plans::bounded_json(p, 65536)?;
    let p: Params = serde_json::from_value(p.clone()).map_err(error)?;
    let mode = if separate {
        match p.method.as_deref().unwrap_or("hpss") {
            "hpss" => Mode::Hpss,
            "mid_side" => Mode::MidSide,
            _ => return Err(error("method must be hpss or mid_side")),
        }
    } else {
        if p.method.is_some() {
            return Err(error("dialogue enhancement uses its declared local effect chain"));
        }
        Mode::Enhance
    };
    let item = ItemId(p.item);
    let (_, media, subclip) = s.project.resolve_media(item).ok_or_else(|| error("item is not source media"))?;
    if media.offline {
        return Err(error("source is explicitly offline"));
    }
    if let filmcraft_project::MediaRef::File { path } = &media.media {
        let path = s.media.follow(path).map_err(error)?;
        s.services.file_size(&path).map_err(error)?;
    }
    let source = s.media.full_res_source(&s.project, item, &*s.services).ok_or_else(|| error("cannot open source"))?;
    if s.media.offline_status(item).is_some() {
        return Err(error("source is offline or cannot decode"));
    }
    let stream = p.stream.unwrap_or(0);
    let audio = source.info().audio_streams.get(stream).ok_or_else(|| error("audio stream does not exist"))?;
    let channels = usize::try_from(audio.channels).map_err(error)?;
    if channels == 0 || channels > 8 || separate && channels > 2 || matches!(mode, Mode::MidSide) && channels != 2 {
        return Err(error("enhancement accepts 1..8 channels; HPSS mono/stereo and mid_side stereo"));
    }
    let rate = p.sample_rate.unwrap_or(audio.sample_rate);
    if !(8000..=96000).contains(&rate) {
        return Err(error("sampleRate must be 8000..96000"));
    }
    let allowed = subclip.unwrap_or_else(|| filmcraft_time::TimeRange::new(Tick::ZERO, source.info().duration));
    let allowed_end = allowed
        .start
        .0
        .checked_add(allowed.duration.0)
        .map(Tick)
        .filter(|end| allowed.start >= Tick::ZERO && allowed.duration > Tick::ZERO && *end <= source.info().duration && *end <= Tick::MAX)
        .ok_or_else(|| error("invalid source or subclip bounds"))?;
    let (start, end) = (p.start.map(Tick).unwrap_or(allowed.start), p.end.map(Tick).unwrap_or(allowed_end));
    if start < allowed.start || end <= start || end > allowed_end {
        return Err(error("source range must stay inside the selected media or subclip bounds"));
    }
    let begin = start.to_units_floor(i64::from(rate));
    let finish = end.to_units_floor(i64::from(rate));
    let frames = u64::try_from(finish.checked_sub(begin).ok_or_else(|| error("source range overflows"))?).map_err(error)?;
    if frames == 0 || frames > u64::from(rate) * 24 * 3600 {
        return Err(error("range must contain audio and be at most 24 hours"));
    }
    let paths = if separate {
        if p.path.is_some() {
            return Err(error("stem outputs use directory, not path"));
        }
        let directory = p.directory.as_deref().filter(|s| !s.is_empty() && s.len() <= 8192).ok_or_else(|| error("directory is required"))?;
        let names = if matches!(mode, Mode::Hpss) { ["harmonic.wav", "percussive.wav"] } else { ["center.wav", "side.wav"] };
        names.into_iter().map(|name| Path::new(directory).join(name).to_string_lossy().into_owned()).collect::<Vec<_>>()
    } else {
        if p.directory.is_some() {
            return Err(error("enhancement uses path, not directory"));
        }
        vec![p.path.filter(|s| !s.is_empty() && s.len() <= 8192).ok_or_else(|| error("path is required"))?]
    };
    for path in &paths {
        for candidate in [path.clone(), format!("{path}.preview.wav"), format!("{path}.provenance.json")] {
            if std::fs::symlink_metadata(&candidate).is_ok() {
                return Err(error(format!("output already exists: {candidate}")));
            }
        }
    }
    let job = s.job_registry.new_job(s, mode.label().into())?;
    let id = job.id;
    job.progress.total.store(frames, Ordering::Relaxed);
    let provenance = json!({"schemaVersion":1,"job":id,"inputRevision":s.revision,"sourceItem":p.item,"sourceIdentity":media.identity,"source":source.info(),"sourceStartTick":start.0,"sourceEndTick":end.0,"sourceStartSample":begin,"samples":frames,"sampleRate":rate,"channels":channels,"method":mode.label(),"model":null,"originalPreserved":true,"outputs":paths,"limitations":match mode{Mode::Enhance=>"Deterministic DeNoise, Enhance Speech and Compressor; no learned voice reconstruction.",Mode::Hpss=>"Median-filtered STFT harmonic/percussive stems; instruments and voices can overlap both stems.",Mode::MidSide=>"Center/side decomposition; all centered sounds share the center stem, not only vocals."}});
    let task = AudioTask { source, stream, rate, channels, start: begin, frames, mode, paths: paths.clone(), provenance, services: s.services.clone() };
    let working = job.clone();
    let run = crate::durable_jobs::guard(s, &job, move || {
        let result = task.process(&working);
        if let Err(e) = &result {
            *working.progress.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.clone());
        }
        working.progress.finished.store(true, Ordering::Relaxed);
        *working.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    });
    s.jobs.push(job);
    if p.wait {
        run();
    } else {
        crate::durable_jobs::spawn(s, id, "filmcraft-audio-job", run)?;
    }
    Ok(json!({"job":id,"paths":paths,"inputRevision":s.revision,"originalPreserved":true,"poll":"jobs.get","cancel":"jobs.cancel"}))
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "audio.dialogueEnhance",
            label: "Enhance Dialogue Locally",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"path":str,"start":ticks?,"end":ticks?,"stream":n=0,"sampleRate":8000..96000?,"wait":bool=false}"#,
            enabled: always,
            journal: true,
            run: |s, p| run(s, p, false),
        },
        CommandSpec {
            id: "audio.separateStems",
            label: "Separate Local Audio Stems",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"directory":str,"method":"hpss|mid_side"="hpss","start":ticks?,"end":ticks?,"stream":n=0,"sampleRate":8000..96000?,"wait":bool=false}"#,
            enabled: always,
            journal: true,
            run: |s, p| run(s, p, true),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_stem_job_roundtrips_exact_samples_and_does_not_overwrite_sources() {
        let dir =
            crate::temp_dir().join(format!("filmcraft-audio-job-{}", web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("original.wav");
        let samples: Vec<f32> = (0..16000)
            .flat_map(|i| {
                let a = (std::f32::consts::TAU * 440.0 * i as f32 / 16000.0).sin() * 0.25;
                [a, a * 0.7]
            })
            .collect();
        let bytes = filmcraft_export::write_wav(&samples, 2, 16000, filmcraft_export::SampleFormat::Float32).unwrap();
        std::fs::write(&source, &bytes).unwrap();
        let mut s = Session::default();
        let imported = s.execute("file.import", json!({"paths":[source]})).unwrap();
        let item = imported["items"][0].as_u64().unwrap();
        let project = s.project.clone();
        let revision = s.revision;
        let result = s.execute("audio.separateStems", json!({"item":item,"directory":dir.join("stems"),"method":"mid_side","wait":true})).unwrap();
        let job = s.job_registry.get(result["job"].as_u64().unwrap()).unwrap();
        assert_eq!(job["state"], "completed", "{job}");
        assert_eq!(*project, *s.project);
        assert_eq!(revision, s.revision);
        let a =
            filmcraft_codecs::open_bytes("center.wav", std::fs::read(dir.join("stems/center.wav")).unwrap().into()).unwrap().audio(0, 16000, 16000).unwrap();
        let b = filmcraft_codecs::open_bytes("side.wav", std::fs::read(dir.join("stems/side.wav")).unwrap().into()).unwrap().audio(0, 16000, 16000).unwrap();
        for channel in 0..2 {
            for i in 0..16000 {
                assert!((a.channels[channel][i] + b.channels[channel][i] - samples[i * 2 + channel]).abs() < 1e-6);
            }
        }
        assert!(dir.join("stems/center.wav.preview.wav").exists());
        assert!(dir.join("stems/center.wav.provenance.json").exists());
        assert!(s.execute("audio.separateStems", json!({"item":item,"directory":dir.join("stems"),"method":"mid_side","wait":true})).is_err());
        assert!(s.execute("audio.dialogueEnhance", json!({"item":item,"path":source,"wait":true})).is_err());
        let enhanced = s.execute("audio.dialogueEnhance", json!({"item":item,"path":dir.join("dialogue.wav"),"wait":true})).unwrap();
        assert_eq!(s.job_registry.get(enhanced["job"].as_u64().unwrap()).unwrap()["state"], "completed");
        let enhanced = filmcraft_codecs::open_bytes("dialogue.wav", std::fs::read(dir.join("dialogue.wav")).unwrap().into()).unwrap();
        assert_eq!(enhanced.info().duration, Tick::from_units(1, 1));
        assert_eq!(std::fs::read(source).unwrap(), bytes);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn audio_jobs_respect_selected_subclip_bounds_and_reject_escape_without_outputs() {
        let dir =
            crate::temp_dir().join(format!("filmcraft-audio-subclip-{}", web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("original.wav");
        let samples: Vec<f32> = (0..16000)
            .flat_map(|i| {
                let a = i as f32 / 32000.0;
                [a, a * 0.7]
            })
            .collect();
        let original = filmcraft_export::write_wav(&samples, 2, 16000, filmcraft_export::SampleFormat::Float32).unwrap();
        std::fs::write(&path, &original).unwrap();
        let mut s = Session::default();
        let item = s.execute("file.import", json!({"paths":[path]})).unwrap()["items"][0].as_u64().unwrap();
        let start = Tick::from_units(1, 4);
        let duration = Tick::from_units(1, 4);
        let subclip = Arc::make_mut(&mut s.project).add_item(
            "Quarter-second take",
            filmcraft_project::Label::Iris,
            filmcraft_project::ItemKind::Subclip { parent: ItemId(item), range: filmcraft_time::TimeRange::new(start, duration), restrict_trims: false },
            None,
        );
        let before = s.project.clone();
        let history = (s.history.undo.len(), s.history.redo.len());
        let revision = s.revision;
        for (command, output) in [("audio.dialogueEnhance", dir.join("dialogue.wav")), ("audio.separateStems", dir.join("stems"))] {
            let mut params = json!({"item":subclip.0,"wait":true});
            if command == "audio.separateStems" {
                params["directory"] = json!(output);
                params["method"] = json!("mid_side");
            } else {
                params["path"] = json!(output);
            }
            for (begin, end) in [(0, Tick::from_units(1, 2).0), (start.0, Tick::from_units(3, 4).0), (i64::MIN, i64::MAX)] {
                let mut escaped = params.clone();
                escaped["start"] = json!(begin);
                escaped["end"] = json!(end);
                assert!(s.execute(command, escaped).is_err());
                assert!(!output.exists());
                assert!(s.jobs.is_empty());
            }
        }
        let result = s.execute("audio.separateStems", json!({"item":subclip.0,"directory":dir.join("stems"),"method":"mid_side","wait":true})).unwrap();
        assert_eq!(s.job_registry.get(result["job"].as_u64().unwrap()).unwrap()["state"], "completed");
        let center = filmcraft_codecs::open_bytes("center.wav", std::fs::read(dir.join("stems/center.wav")).unwrap().into()).unwrap();
        let side = filmcraft_codecs::open_bytes("side.wav", std::fs::read(dir.join("stems/side.wav")).unwrap().into()).unwrap();
        assert_eq!(center.info().duration, duration);
        assert_eq!(side.info().duration, duration);
        let a = center.audio(0, 4000, 16000).unwrap();
        let b = side.audio(0, 4000, 16000).unwrap();
        for c in 0..2 {
            for i in 0..4000 {
                assert!((a.channels[c][i] + b.channels[c][i] - samples[(i + 4000) * 2 + c]).abs() < 1e-6);
            }
        }
        let result = s.execute("audio.dialogueEnhance", json!({"item":subclip.0,"path":dir.join("dialogue.wav"),"wait":true})).unwrap();
        assert_eq!(s.job_registry.get(result["job"].as_u64().unwrap()).unwrap()["state"], "completed");
        let dialogue = filmcraft_codecs::open_bytes("dialogue.wav", std::fs::read(dir.join("dialogue.wav")).unwrap().into()).unwrap();
        assert_eq!(dialogue.info().duration, duration);
        let provenance: Value = serde_json::from_slice(&std::fs::read(dir.join("dialogue.wav.provenance.json")).unwrap()).unwrap();
        assert_eq!(provenance["sourceStartTick"], start.0);
        assert_eq!(provenance["sourceEndTick"], (start + duration).0);
        assert_eq!(provenance["sourceStartSample"], 4000);
        assert_eq!(*s.project, *before);
        assert_eq!(s.revision, revision);
        assert_eq!((s.history.undo.len(), s.history.redo.len()), history);
        assert_eq!(std::fs::read(path).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn missing_hostile_parameters_never_start_jobs() {
        let mut session = Session::default();
        let project = session.project.clone();
        for params in [json!({}), json!({"item":-1,"path":"x"}), json!({"item":0,"path":"x","sampleRate":0}), json!({"item":0,"path":"x","wait":"yes"})] {
            assert!(session.execute("audio.dialogueEnhance", params).is_err());
        }
        assert!(session.jobs.is_empty());
        assert_eq!(*session.project, *project);
    }
}
