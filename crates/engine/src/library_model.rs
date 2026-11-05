//! Native local CLIP installation/inference connected to the incremental library index.
//! Downloads require an explicit action; long model operations use the durable job registry.
use crate::commands::{CommandSpec, always, bad};
#[cfg(all(feature = "vision", not(target_arch = "wasm32")))]
use crate::library_search::EmbeddingModel;
use crate::library_search::EmbeddingProvider;
use crate::{Result, Session};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

type ProviderResult = std::result::Result<Arc<dyn EmbeddingProvider>, String>;
#[derive(Default)]
pub struct ModelJobs {
    pending: Option<Pending>,
    pub last_error: Option<String>,
}
impl ModelJobs {
    pub(crate) fn loading(&self) -> bool {
        self.pending.is_some()
    }
}
struct Pending {
    id: u64,
    result: Arc<Mutex<Option<ProviderResult>>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    model: Option<String>,
    directory: Option<String>,
    #[serde(default)]
    accept_download: bool,
    weights: Option<String>,
    tokenizer: Option<String>,
    format: Option<String>,
    weights_bytes: Option<u64>,
    weights_sha256: Option<String>,
}
fn request(p: &Value, cmd: &str) -> Result<Request> {
    crate::agent_plans::bounded_json(p, 16 * 1024)?;
    let request: Request = serde_json::from_value(p.clone()).map_err(|e| bad(cmd, e.to_string()))?;
    if request.model.as_deref().is_some_and(|id| id != filmcraft_vision::models::ID) {
        return Err(bad(cmd, "unknown model; inspect library.model.status for the pinned catalogue"));
    }
    for path in [&request.directory, &request.weights, &request.tokenizer].into_iter().flatten() {
        if path.is_empty() || path.len() > 4096 || path.contains('\0') {
            return Err(bad(cmd, "invalid model path"));
        }
    }
    Ok(request)
}
fn directory(p: &Request) -> Result<PathBuf> {
    p.directory
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| crate::transcript::models_dir().map(|d| filmcraft_vision::models::dir(&d)))
        .ok_or_else(|| bad("library.model", "no local model data directory"))
}
fn available(_: &Session) -> std::result::Result<(), String> {
    if cfg!(all(feature = "vision", not(target_arch = "wasm32"))) {
        Ok(())
    } else {
        Err("local CLIP inference is unavailable in this build; enable the native vision feature".into())
    }
}

/// Install a completed provider on the owning session's main thread. Model state is session-wide,
/// so opening another project clears indexed ranges while keeping this verified model available.
pub fn poll(s: &mut Session) {
    crate::library_search::poll(s);
    let Some(pending) = &s.model_jobs.pending else { return };
    let outcome = pending.result.lock().unwrap_or_else(|e| e.into_inner()).take();
    let failure = if outcome.is_none() {
        s.jobs
            .iter()
            .find(|j| j.id == pending.id)
            .and_then(|job| job.result.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|r| r.as_ref().err().cloned()))
    } else {
        None
    };
    if let Some(outcome) = outcome {
        s.model_jobs.pending = None;
        match outcome {
            Ok(provider) => {
                s.library_index.provider = Some(provider);
                s.model_jobs.last_error = None;
            }
            Err(e) => s.model_jobs.last_error = Some(e),
        }
    } else if let Some(error) = failure {
        s.model_jobs.pending = None;
        s.model_jobs.last_error = Some(error);
    }
}
fn status(s: &mut Session, p: &Value) -> Result<Value> {
    poll(s);
    let p = request(p, "library.model.status")?;
    if p.accept_download || p.weights.is_some() || p.tokenizer.is_some() || p.format.is_some() || p.weights_bytes.is_some() || p.weights_sha256.is_some() {
        return Err(bad("library.model.status", "status accepts only model and directory"));
    }
    let dir = directory(&p)?;
    Ok(
        json!({"available":available(s).is_ok(),"catalogue":filmcraft_vision::models::catalogue(),"directory":dir,"filesPresentWithExpectedSizes":filmcraft_vision::models::FILES.iter().all(|file|dir.join(file.name).metadata().is_ok_and(|m|m.is_file() && m.len()==file.size)),"artifactVerification":"SHA-256 and byte count checked before provider installation","loaded":s.library_index.provider.as_ref().map(|p|p.model()),"pendingJob":s.model_jobs.pending.as_ref().map(|p|p.id),"lastError":s.model_jobs.last_error,"downloadRequires":"explicit library.model.download with acceptDownload:true; no automatic downloads","privacy":"image and text embeddings stay on this computer"}),
    )
}
#[cfg(all(feature = "vision", not(target_arch = "wasm32")))]
struct LocalClip(filmcraft_vision::ClipEncoder);
#[cfg(all(feature = "vision", not(target_arch = "wasm32")))]
impl EmbeddingProvider for LocalClip {
    fn model(&self) -> EmbeddingModel {
        EmbeddingModel { id: self.0.spec().identity(), dimension: filmcraft_vision::DIMENSION, provenance: self.0.spec().provenance() }
    }
    fn embed_image(&self, image: &filmcraft_render::Image) -> Result<Vec<f32>> {
        self.0.embed_image(image.w, image.h, &image.px).map_err(|e| bad("library.embeddings", e))
    }
    fn embed_text(&self, text: &str) -> Result<Vec<f32>> {
        self.0.embed_text(text).map_err(|e| bad("library.embeddings", e))
    }
}
fn load(s: &mut Session, p: &Value) -> Result<Value> {
    #[cfg(all(feature = "vision", not(target_arch = "wasm32")))]
    {
        const CMD: &str = "library.model.load";
        let p = request(p, CMD)?;
        poll(s);
        if s.model_jobs.pending.is_some()
            || s.library_index.index_pending()
            || s.jobs.iter().any(|j| j.label == "Load local CLIP model" && !j.progress.finished.load(Ordering::Relaxed))
        {
            return Err(bad(CMD, "a model load is already running; inspect or cancel its stable job ID"));
        }
        let dir = directory(&p)?;
        let custom = p.weights.is_some() || p.tokenizer.is_some() || p.format.is_some() || p.weights_bytes.is_some() || p.weights_sha256.is_some();
        let spec = if custom {
            if p.directory.is_some() {
                return Err(bad(CMD, "choose a pinned directory or explicit custom safetensors paths"));
            }
            if p.format.as_deref() != Some("safetensors") {
                return Err(bad(CMD, "custom models require format:safetensors"));
            }
            filmcraft_vision::ClipLoadSpec {
                weights: PathBuf::from(p.weights.ok_or_else(|| bad(CMD, "need weights"))?),
                tokenizer: PathBuf::from(p.tokenizer.ok_or_else(|| bad(CMD, "need tokenizer"))?),
                format: "safetensors".into(),
                weights_size: p
                    .weights_bytes
                    .filter(|v| *v > 0 && *v <= filmcraft_vision::models::MAX_MODEL_BYTES)
                    .ok_or_else(|| bad(CMD, "need bounded weightsBytes"))?,
                weights_sha256: p
                    .weights_sha256
                    .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| bad(CMD, "need weightsSha256"))?,
            }
        } else {
            filmcraft_vision::ClipLoadSpec::pinned(&dir)
        };
        let job = s.job_registry.new_job(s, "Load local CLIP model".into())?;
        let id = job.id;
        let work = job.clone();
        let pending = Arc::new(Mutex::new(None));
        let result = pending.clone();
        let output = spec.weights.clone();
        let bytes = spec.weights_size;
        let model = spec.identity();
        let run = crate::durable_jobs::guard(s, &job, move || {
            let start = web_time::Instant::now();
            let mut stage = 0u64;
            work.progress.total.store(4, Ordering::Relaxed);
            let loaded = filmcraft_vision::ClipEncoder::load(spec, &mut |message| {
                *work.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = message.into();
                stage = stage.saturating_add(1);
                work.progress.done.store(stage.min(4), Ordering::Relaxed);
                !work.progress.cancel.load(Ordering::Relaxed)
            })
            .map(|encoder| Arc::new(LocalClip(encoder)) as Arc<dyn EmbeddingProvider>);
            let receipt = loaded
                .as_ref()
                .map(|_| filmcraft_export::Report {
                    path: output.to_string_lossy().into(),
                    frames: 0,
                    seconds: start.elapsed().as_secs_f64(),
                    bytes,
                    render_fps: 0.0,
                    extra_files: Vec::new(),
                })
                .map_err(Clone::clone);
            *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(loaded);
            finish(&work, receipt);
        });
        s.jobs.push(job);
        s.model_jobs.pending = Some(Pending { id, result: pending });
        s.model_jobs.last_error = None;
        crate::durable_jobs::spawn(s, id, "filmcraft-clip-load", run)?;
        Ok(json!({"job":id,"model":model,"status":"loading","inputRevision":s.revision,"projectChanged":false}))
    }
    #[cfg(not(all(feature = "vision", not(target_arch = "wasm32"))))]
    {
        let _ = (s, p);
        Err(bad("library.model.load", "native vision feature is unavailable"))
    }
}
#[cfg(all(feature = "vision", not(target_arch = "wasm32")))]
fn finish(job: &crate::Job, result: std::result::Result<filmcraft_export::Report, String>) {
    if let Err(error) = &result {
        *job.progress.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
        *job.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = error.clone();
    }
    *job.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    job.progress.finished.store(true, Ordering::Relaxed);
}
/// Write the local package's asset sidecars and index after all artifacts have been verified.
/// Callers provide publisher/license metadata from their pinned catalogue, never guessed values.
pub fn write_attributions(
    dir: &std::path::Path,
    files: &[filmcraft_speech::models::ModelFile],
    author: &str,
    license: &str,
    license_file: &str,
    notes: &str,
) -> std::result::Result<Vec<String>, String> {
    let simple = |v: &str| !v.is_empty() && v.len() <= 4096 && !v.chars().any(char::is_control);
    let filename = |v: &str| simple(v) && !v.contains(['/', '\\']) && !matches!(v, "." | "..");
    if files.is_empty() || files.len() > 256 || !simple(author) || !simple(license) || !filename(license_file) || !simple(notes) {
        return Err("invalid model attribution metadata".into());
    }
    if !dir.join(license_file).is_file() {
        return Err("local model package license text is missing".into());
    }
    let secs = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs();
    let date = i64::try_from(secs)
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .ok_or("model attribution date is out of range")?
        .format("%Y-%m-%d")
        .to_string();
    let mut index = String::from("# FilmCraft local model artifacts\n\n| Asset | Author | License | Source | SHA-256 |\n|---|---|---|---|---|\n");
    let mut written = Vec::new();
    for file in files {
        if !filename(file.name) || !simple(file.url) || file.sha256.len() != 64 || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid model artifact attribution".into());
        }
        let path = dir.join(format!("{}.attribution", file.name));
        let text = format!(
            "# FilmCraft local model artifact\nasset:        {}\ntitle:        Local model package artifact {}\nauthor:       {author}\nsource:       {}\nlicense:      {license}\nlicense-file: {license_file}\nadded:        {date} by FilmCraft local model installer\nnotes:        {notes}; SHA-256 {}; {} bytes; downloaded only after explicit opt-in\n",
            file.name, file.name, file.url, file.sha256, file.size
        );
        if path.exists() {
            let meta = path.metadata().map_err(|e| e.to_string())?;
            if !meta.is_file() || meta.len() > 65_536 {
                return Err(format!("invalid existing attribution {}; file preserved", path.display()));
            }
            let existing = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            if !existing.starts_with("# FilmCraft local model artifact\n")
                || !existing.contains(file.sha256)
                || !existing.contains(&format!("license:      {license}\n"))
            {
                return Err(format!("attribution {} does not match this package; file preserved", path.display()));
            }
        } else {
            filmcraft_format::atomic_write_new(&path, text.as_bytes()).map_err(|e| e.to_string())?;
        }
        index.push_str(&format!("| {} | {author} | {license} | {} | {} |\n", file.name, file.url, file.sha256));
        written.push(path.to_string_lossy().into());
    }
    let path = dir.join("ATTRIBUTION.md");
    if path.exists() {
        if path.metadata().map_err(|e| e.to_string())?.len() > 65_536
            || !std::fs::read_to_string(&path).map_err(|e| e.to_string())?.starts_with("# FilmCraft local model artifacts\n")
        {
            return Err(format!("existing index {} was preserved; choose a dedicated model package directory", path.display()));
        }
        filmcraft_format::atomic_write(&path, index.as_bytes()).map_err(|e| e.to_string())?;
    } else {
        filmcraft_format::atomic_write_new(&path, index.as_bytes()).map_err(|e| e.to_string())?;
    }
    written.push(path.to_string_lossy().into());
    Ok(written)
}

fn download(s: &mut Session, p: &Value) -> Result<Value> {
    #[cfg(all(feature = "vision", not(target_arch = "wasm32")))]
    {
        const CMD: &str = "library.model.download";
        let p = request(p, CMD)?;
        if s.jobs.iter().any(|j| j.label == "Download verified OpenAI CLIP" && !j.progress.finished.load(Ordering::Relaxed)) {
            return Err(bad(CMD, "a CLIP package download is already running; inspect or cancel its stable job ID"));
        }
        if !p.accept_download {
            return Err(bad(CMD, "inspect library.model.status for the ~608 MB package, source and MIT license, then choose acceptDownload:true explicitly"));
        }
        if p.weights.is_some() || p.tokenizer.is_some() || p.format.is_some() || p.weights_bytes.is_some() || p.weights_sha256.is_some() {
            return Err(bad(CMD, "downloads use only the pinned first-party catalogue"));
        }
        let dir = directory(&p)?;
        let job = s.job_registry.new_job(s, "Download verified OpenAI CLIP".into())?;
        let id = job.id;
        let work = job.clone();
        let dest = dir.clone();
        let run = crate::durable_jobs::guard(s, &job, move || {
            let start = web_time::Instant::now();
            let files: Vec<filmcraft_speech::models::ModelFile> = filmcraft_vision::models::FILES
                .iter()
                .map(|f| filmcraft_speech::models::ModelFile { name: f.name, url: f.url, sha256: f.sha256, size: f.size })
                .collect();
            let result = filmcraft_speech::models::download_files(&dest, &files, &mut |done, total, file| {
                work.progress.total.store(total, Ordering::Relaxed);
                work.progress.done.store(done, Ordering::Relaxed);
                *work.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = format!("Downloading {file}; SHA-256 verified before installation");
                !work.progress.cancel.load(Ordering::Relaxed)
            })
            .map_err(|e| e.to_string())
            .and_then(|()| {
                write_attributions(
                    &dest,
                    &files,
                    "OpenAI",
                    "MIT",
                    "LICENSE.txt",
                    &format!("OpenAI CLIP ViT-B/32; pinned model revision {}", filmcraft_vision::models::REVISION),
                )
            })
            .map(|attributions| filmcraft_export::Report {
                path: dest.to_string_lossy().into(),
                frames: 0,
                seconds: start.elapsed().as_secs_f64(),
                bytes: files.iter().map(|f| f.size).sum(),
                render_fps: 0.0,
                extra_files: files.iter().map(|f| dest.join(f.name).to_string_lossy().into()).chain(attributions).collect(),
            });
            finish(&work, result);
        });
        s.jobs.push(job);
        crate::durable_jobs::spawn(s, id, "filmcraft-clip-download", run)?;
        Ok(
            json!({"job":id,"model":filmcraft_vision::models::ID,"directory":dir,"status":"downloading","inputRevision":s.revision,"autoload":false,"projectChanged":false}),
        )
    }
    #[cfg(not(all(feature = "vision", not(target_arch = "wasm32"))))]
    {
        let _ = (s, p);
        Err(bad("library.model.download", "native vision feature is unavailable"))
    }
}
fn unload(s: &mut Session, _: &Value) -> Result<Value> {
    s.library_index.cancel_index();
    if let Some(pending) = s.model_jobs.pending.take()
        && let Some(job) = s.jobs.iter().find(|j| j.id == pending.id)
    {
        job.progress.cancel.store(true, Ordering::Relaxed);
    }
    s.library_index.provider = None;
    s.model_jobs.last_error = None;
    Ok(json!({"loaded":false,"filesRetained":true,"projectChanged":false}))
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    type Run = fn(&mut Session, &Value) -> Result<Value>;
    [
        (
            "library.model.status",
            "Inspect Local Vision Model",
            r#"{"model":"clip-vit-base-patch32"?,"directory":path?}"#,
            always as fn(&Session) -> std::result::Result<(), String>,
            status as Run,
        ),
        (
            "library.model.download",
            "Download Local Vision Model",
            r#"{"model":"clip-vit-base-patch32"?,"directory":path?,"acceptDownload":true}"#,
            available,
            download,
        ),
        (
            "library.model.load",
            "Load Local Vision Model",
            r#"{"directory":path?,"weights":path?,"tokenizer":path?,"format":"safetensors"?,"weightsBytes":n?,"weightsSha256":str?}"#,
            available,
            load,
        ),
        ("library.model.unload", "Unload Local Vision Model", "{}", always, unload),
    ]
    .into_iter()
    .map(|(id, label, params, enabled, run)| CommandSpec { id, label, menu: &[], shortcut: None, params, enabled, journal: false, run })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalogue_does_not_download_or_change_project() {
        let mut s = Session::default();
        let before = s.project.to_json();
        let revision = s.revision;
        let status = s.execute("library.model.status", json!({})).unwrap();
        assert_eq!(status["catalogue"]["dimension"], 512);
        assert!(s.jobs.is_empty());
        assert!(s.execute("library.model.download", json!({"acceptDownload":false})).is_err());
        assert!(s.jobs.is_empty());
        assert_eq!(s.project.to_json(), before);
        assert_eq!(s.revision, revision);
    }
    #[test]
    fn attribution_sidecars_are_complete_repeatable_and_preserve_unowned_metadata() {
        let dir = std::env::temp_dir().join(format!("filmcraft-model-attribution-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("LICENSE.txt"), "synthetic test license").unwrap();
        let file = filmcraft_speech::models::ModelFile {
            name: "fixture.bin",
            url: "https://example.invalid/synthetic-fixture",
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            size: 3,
        };
        let written = write_attributions(&dir, &[file], "Synthetic test author", "MIT", "LICENSE.txt", "original synthetic fixture").unwrap();
        assert_eq!(written.len(), 2);
        let text = std::fs::read_to_string(dir.join("fixture.bin.attribution")).unwrap();
        for field in ["asset:", "title:", "author:", "source:", "license:", "license-file:", "added:", "notes:", file.sha256] {
            assert!(text.contains(field));
        }
        write_attributions(&dir, &[file], "Synthetic test author", "MIT", "LICENSE.txt", "original synthetic fixture").unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("fixture.bin.attribution")).unwrap(), text);
        std::fs::write(dir.join("ATTRIBUTION.md"), "user-authored notes").unwrap();
        assert!(write_attributions(&dir, &[file], "Synthetic test author", "MIT", "LICENSE.txt", "original synthetic fixture").is_err());
        assert_eq!(std::fs::read_to_string(dir.join("ATTRIBUTION.md")).unwrap(), "user-authored notes");
        assert!(write_attributions(&dir, &[file], "Synthetic test author", "MIT", "../LICENSE.txt", "original synthetic fixture").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(all(feature = "vision", not(target_arch = "wasm32")))]
    #[test]
    fn failed_background_load_is_durable_and_preserves_existing_provider() {
        struct Existing;
        impl EmbeddingProvider for Existing {
            fn model(&self) -> EmbeddingModel {
                EmbeddingModel { id: "test-existing".into(), dimension: 512, provenance: "test double, never installed by product".into() }
            }
            fn embed_image(&self, _: &filmcraft_render::Image) -> Result<Vec<f32>> {
                Err(bad("test", "unused"))
            }
            fn embed_text(&self, _: &str) -> Result<Vec<f32>> {
                Err(bad("test", "unused"))
            }
        }
        let mut s = Session::default();
        s.library_index.provider = Some(Arc::new(Existing));
        let before = s.project.to_json();
        let revision = s.revision;
        let history = s.history.undo.len();
        let dir = std::env::temp_dir().join(format!("filmcraft-missing-clip-model-{}", std::process::id()));
        let result = s.execute("library.model.load", json!({"directory":dir})).unwrap();
        let id = result["job"].as_u64().unwrap();
        let deadline = web_time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            poll(&mut s);
            if s.model_jobs.pending.is_none() {
                break;
            }
            assert!(web_time::Instant::now() < deadline, "model loader did not finish");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(s.library_index.provider.as_ref().unwrap().model().id, "test-existing");
        assert!(s.model_jobs.last_error.is_some());
        assert_eq!(s.project.to_json(), before);
        assert_eq!(s.revision, revision);
        assert_eq!(s.history.undo.len(), history);
        s.job_registry.sync(&s.jobs).unwrap();
        assert_eq!(s.job_registry.get(id).unwrap()["state"], "failed");
        s.execute("library.model.unload", json!({})).unwrap();
        assert!(s.library_index.provider.is_none());
    }
}

#[cfg(all(test, feature = "vision", not(target_arch = "wasm32")))]
mod real_tests {
    use super::*;
    fn wait(s: &mut Session, id: u64) {
        let deadline = web_time::Instant::now() + std::time::Duration::from_secs(1800);
        let mut last = web_time::Instant::now() - std::time::Duration::from_secs(5);
        loop {
            poll(s);
            let job = s.jobs.iter().find(|j| j.id == id).unwrap();
            if last.elapsed() >= std::time::Duration::from_secs(5) {
                eprintln!(
                    "job {id}: {} {} / {}",
                    job.progress.status.lock().unwrap_or_else(|e| e.into_inner()),
                    job.progress.done.load(Ordering::Relaxed),
                    job.progress.total.load(Ordering::Relaxed)
                );
                last = web_time::Instant::now();
            }
            if job.progress.finished.load(Ordering::Relaxed) {
                assert!(job.result.lock().unwrap_or_else(|e| e.into_inner()).as_ref().unwrap().is_ok(), "{}", job.to_json());
                poll(s);
                return;
            }
            assert!(web_time::Instant::now() < deadline, "CLIP job timed out");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    #[test]
    #[ignore = "requires explicit model-download opt-in and approved indoor laptop/desk/cups footage"]
    fn pinned_download_load_and_actual_footage_semantic_search() {
        assert_eq!(std::env::var("FILMCRAFT_CLIP_ALLOW_DOWNLOAD").as_deref(), Ok("1"), "explicitly opt in with FILMCRAFT_CLIP_ALLOW_DOWNLOAD=1");
        let footage = std::env::var("FILMCRAFT_CLIP_TEST_FOOTAGE").expect("set FILMCRAFT_CLIP_TEST_FOOTAGE to an approved local video");
        let hardware = filmcraft_platform::register();
        eprintln!("hardware: {hardware:?}");
        let mut s = Session::default();
        let package = directory(&request(&json!({}), "test").unwrap()).unwrap();
        eprintln!("CLIP package {}", package.display());
        let download = s.execute("library.model.download", json!({"acceptDownload":true})).unwrap();
        wait(&mut s, download["job"].as_u64().unwrap());
        let load = s.execute("library.model.load", json!({})).unwrap();
        wait(&mut s, load["job"].as_u64().unwrap());
        assert!(s.library_index.provider.is_some());
        let imported = s.execute("file.import", json!({"paths":[footage]})).unwrap();
        let item = imported["items"][0].as_u64().unwrap();
        let before = s.project.to_json();
        let revision = s.revision;
        let history = s.history.undo.len();
        let indexing =
            s.execute("library.indexAsync", json!({"items":[item],"end":8*filmcraft_time::TICKS_PER_SECOND,"maxFrames":3,"embeddings":true})).unwrap();
        wait(&mut s, indexing["job"].as_u64().unwrap());
        let indexed = s.execute("library.indexStatus", json!({})).unwrap()["lastResult"]["report"].clone();
        eprintln!("indexed {indexed}");
        let mut reports = Vec::new();
        for query in ["a laptop computer on a desk with red plastic cups inside a room", "a forest with trees outdoors", "a dense crowd at an outdoor concert"]
        {
            let found = s.execute("library.search", json!({"query":query,"limit":3,"thumbnails":false})).unwrap();
            assert_eq!(found["semantic"]["status"], "used_compatible_model_vectors");
            assert_eq!(found["semantic"]["candidateCount"], 3);
            assert!(!found["hits"].as_array().unwrap().is_empty());
            eprintln!("query {query}: {}", serde_json::to_string(&found).unwrap());
            reports.push(found);
        }
        let best = |report: &Value| report["hits"].as_array().unwrap().iter().filter_map(|hit| hit["ranking"]["cosine"].as_f64()).reduce(f64::max).unwrap();
        let positive = best(&reports[0]);
        for negative in reports.iter().skip(1) {
            assert!(
                positive > best(negative),
                "known indoor laptop/desk/cups description must outrank clear outdoor negatives: {positive} vs {}",
                best(negative)
            );
        }
        assert_eq!(s.project.to_json(), before);
        assert_eq!(s.revision, revision);
        assert_eq!(s.history.undo.len(), history);
        if let Some(path) = std::env::var_os("FILMCRAFT_CLIP_REPORT_DIR") {
            let dir = PathBuf::from(path);
            std::fs::create_dir_all(&dir).unwrap();
            let sheet = s.execute("render.frames", json!({"target":"source","item":item,"frames":[0,160,320],"layout":"contactSheet","maxSide":512})).unwrap();
            use base64::Engine as _;
            let encoded = sheet["contactSheet"]["base64"].as_str().unwrap();
            let png = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
            filmcraft_format::atomic_write(&dir.join("semantic-source-contact-sheet.png"), &png).unwrap();
            let report = json!({"model":s.library_index.provider.as_ref().unwrap().model(),"indexed":indexed,"queries":reports,"positiveDescriptionBestCosine":positive,"sourceContactSheet":"semantic-source-contact-sheet.png","frames":[0,160,320]});
            filmcraft_format::atomic_write(&dir.join("semantic-search.json"), serde_json::to_string_pretty(&report).unwrap().as_bytes()).unwrap();
        }
    }
}
