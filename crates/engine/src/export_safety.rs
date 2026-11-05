//! Source-preservation preflight for every native export output, before sidecars, job reservation
//! or writers are opened. Existing non-source delivery files retain normal overwrite behavior.
use crate::{EngineError, Result, Services, Session};
use filmcraft_export::{ExportSettings, Format};
use filmcraft_project::{ItemId, MediaRef, Project};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(not(target_arch = "wasm32"))]
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
const MAX_FILES: usize = 100_000;
fn error(message: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("export source protection / I/O: {message}"))
}
fn lexical(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.as_os_str().as_encoded_bytes().len() > 32768 {
        return Err(error("invalid virtual file path"));
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}
fn normalized(path: &Path, virtual_files: bool) -> Result<PathBuf> {
    if virtual_files {
        return lexical(path);
    }
    native_normalized(path)
}
fn native_normalized(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || path.as_os_str().as_encoded_bytes().len() > 32768 {
        return Err(error("invalid or excessive output/source path"));
    }
    let absolute = std::path::absolute(path).map_err(error)?;
    if absolute.try_exists().map_err(error)? {
        return std::fs::canonicalize(absolute).map_err(error);
    }
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    while !ancestor.try_exists().map_err(error)? {
        if missing.len() >= 128 {
            return Err(error("path nesting exceeds 128 components"));
        }
        missing.push(ancestor.file_name().ok_or_else(|| error("path has no existing ancestor"))?.to_os_string());
        ancestor = ancestor.parent().ok_or_else(|| error("path has no parent"))?;
    }
    let mut result = std::fs::canonicalize(ancestor).map_err(error)?;
    for part in missing.into_iter().rev() {
        result.push(part);
    }
    Ok(result)
}
#[cfg(not(target_arch = "wasm32"))]
fn identity(path: &Path) -> Result<Option<u64>> {
    let handle = match same_file::Handle::from_path(path) {
        Ok(h) => h,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(error(format!("cannot inspect {}: {e}", path.display()))),
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    handle.hash(&mut hasher);
    Ok(Some(hasher.finish()))
}
#[cfg(target_arch = "wasm32")]
fn identity(_path: &Path) -> Result<Option<u64>> {
    Ok(None)
}
#[cfg(not(target_arch = "wasm32"))]
fn same_file(a: &Path, b: &Path) -> Result<bool> {
    same_file::is_same_file(a, b).map_err(error)
}
#[cfg(target_arch = "wasm32")]
fn same_file(_a: &Path, _b: &Path) -> Result<bool> {
    Ok(false)
}
struct Sources {
    paths: BTreeSet<PathBuf>,
    identities: BTreeMap<u64, Vec<PathBuf>>,
    count: usize,
    virtual_files: bool,
}
impl Sources {
    fn add(&mut self, path: &Path) -> Result<()> {
        if self.count >= MAX_FILES {
            return Err(error("source scan exceeds 100000 files; split the project or output range"));
        }
        let path = normalized(path, self.virtual_files)?;
        if !self.paths.insert(path.clone()) {
            return Ok(());
        }
        self.count = self.count.saturating_add(1);
        if !self.virtual_files
            && let Some(id) = identity(&path)?
        {
            self.identities.entry(id).or_default().push(path);
        }
        Ok(())
    }
    fn protect(&self, path: &Path) -> Result<()> {
        let normalized = normalized(path, self.virtual_files)?;
        if self.paths.contains(&normalized) {
            return Err(error(format!("refusing to overwrite source/project {}", path.display())));
        }
        if !self.virtual_files
            && let Some(id) = identity(path)?
            && let Some(sources) = self.identities.get(&id)
        {
            for source in sources {
                if same_file(path, source)? {
                    return Err(error(format!("output {} aliases protected source/project {}", path.display(), source.display())));
                }
            }
        }
        Ok(())
    }
}
fn directory_files(directory: &Path, mut f: impl FnMut(PathBuf) -> Result<()>) -> Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(error(e)),
    };
    for (index, entry) in entries.enumerate() {
        if index >= MAX_FILES {
            return Err(error("directory scan exceeds 100000 entries; choose a smaller output directory"));
        }
        let entry = entry.map_err(error)?;
        f(entry.path())?;
    }
    Ok(())
}
fn collect(s: &Session, project: &Project) -> Result<Sources> {
    collect_snapshot(project, s.path.as_deref(), &s.media.path_snapshot(), cfg!(target_arch = "wasm32") || s.services.export_in_memory())
}
fn collect_snapshot(project: &Project, project_path: Option<&str>, paths: &crate::media_pool::SourcePaths, virtual_files: bool) -> Result<Sources> {
    let mut out = Sources { paths: BTreeSet::new(), identities: BTreeMap::new(), count: 0, virtual_files };
    let mut scanned = BTreeSet::new();
    if project.items.len() > MAX_FILES {
        return Err(error("project exceeds 100000 items"));
    }
    if let Some(path) = project_path {
        out.add(Path::new(path))?;
    }
    for item in project.items.values() {
        let Some(media) = item.as_media() else {
            continue;
        };
        for reference in std::iter::once(&media.media).chain(media.proxy.iter()) {
            let MediaRef::File { path } = reference else {
                continue;
            };
            // Use the same stored-document/relative-path resolution as actual decoding.
            let path = match paths.follow(path) {
                Ok(path) => PathBuf::from(path),
                Err(_) => continue,
            };
            out.add(&path)?;
            if !virtual_files
                && media.info.kind == filmcraft_media::MediaKind::ImageSequence
                && let Some(sequence) = path.to_str().and_then(filmcraft_media::sequence::Numbered::parse)
            {
                let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
                let key = (normalized(dir, false)?, sequence.prefix.clone(), sequence.suffix.clone(), sequence.digits, sequence.number);
                if scanned.insert(key) {
                    directory_files(dir, |path| {
                        if path.file_name().and_then(|n| n.to_str()).and_then(|n| sequence.number_of(n)).is_some_and(|n| n >= sequence.number) {
                            out.add(&path)?;
                        }
                        Ok(())
                    })?;
                }
            }
        }
    }
    Ok(out)
}
/// Exactly the sidecar name written by export_tools::write_sidecar.
pub(crate) fn sidecar_path(settings: &ExportSettings) -> Result<Option<String>> {
    let Some(kind) = settings.caption_sidecar.as_deref() else {
        return Ok(None);
    };
    let format = filmcraft_captions::Format::from_name(kind).ok_or_else(|| error("unrecognized caption sidecar format"))?;
    let ext = if format == filmcraft_captions::Format::WebVtt { "vtt" } else { kind };
    Ok(Some(format!("{}.{ext}", settings.path.rsplit_once('.').map_or(settings.path.as_str(), |p| p.0))))
}
struct Images {
    base: String,
    count: u64,
    prefix: String,
    suffix: String,
    digits: usize,
    directory: PathBuf,
}
impl Images {
    fn new(base: &str, count: u64, virtual_files: bool) -> Result<Self> {
        let path = Path::new(base);
        let directory = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        Ok(Self {
            base: base.into(),
            count,
            prefix: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            suffix: format!(".{}", path.extension().map(|e| e.to_string_lossy()).unwrap_or_else(|| "png".into())),
            digits: count.saturating_sub(1).max(1).to_string().len().max(3),
            directory: normalized(directory, virtual_files)?,
        })
    }
    fn index(&self, name: &str) -> Option<u64> {
        let start = self.prefix.len();
        let end = name.len().checked_sub(self.suffix.len())?;
        if start >= end {
            return None;
        }
        let prefix = name.get(..start)?;
        let suffix = name.get(end..)?;
        if !prefix.eq_ignore_ascii_case(&self.prefix) || !suffix.eq_ignore_ascii_case(&self.suffix) {
            return None;
        }
        let middle = name.get(start..end)?;
        if middle.len() != self.digits || !middle.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        middle.parse::<u64>().ok().filter(|n| *n < self.count)
    }
    fn path(&self, index: u64) -> PathBuf {
        PathBuf::from(filmcraft_export::image_sequence_path(&self.base, index, self.count))
    }
}
/// Validate all known output families without iterating a potentially millions-frame duration.
pub(crate) fn preflight(s: &Session, project: &Project, sequence: ItemId, settings: &ExportSettings) -> Result<()> {
    let sources = collect(s, project)?;
    let q = project.sequence(sequence).ok_or(crate::EngineError::NoSequence)?;
    if settings.format.is_image_sequence() {
        let range = filmcraft_export::export_range(project, sequence, settings).map_err(error)?;
        let resolved = settings.resolve(q.settings.width, q.settings.height, q.settings.frame_rate, q.settings.sample_rate);
        let (first, last) = filmcraft_export::frame_span(resolved.rate, range);
        let count = u64::try_from(last.saturating_sub(first)).map_err(error)?;
        if count > 0 {
            let images = Images::new(&settings.path, count, sources.virtual_files)?;
            for source in &sources.paths {
                if normalized(source.parent().unwrap_or_else(|| Path::new(".")), sources.virtual_files)? == images.directory
                    && let Some(index) = source.file_name().and_then(|n| n.to_str()).and_then(|n| images.index(n))
                {
                    sources.protect(&images.path(index))?;
                }
            }
            // Existing numbered targets may be hardlinks or symlinks to any protected media.
            if !sources.virtual_files {
                directory_files(&images.directory, |entry| {
                    if let Some(index) = entry.file_name().and_then(|n| n.to_str()).and_then(|n| images.index(n)) {
                        sources.protect(&images.path(index))?;
                    }
                    Ok(())
                })?;
            }
        }
    } else {
        sources.protect(Path::new(&settings.path))?;
    }
    if !q.caption_tracks.is_empty()
        && let Some(path) = sidecar_path(settings)?
    {
        sources.protect(Path::new(&path))?;
    }
    if settings.format == Format::MxfOpAtom && settings.include_audio {
        let resolved = settings.resolve(q.settings.width, q.settings.height, q.settings.frame_rate, q.settings.sample_rate);
        let channels = resolved.channels;
        if channels > 64 {
            return Err(error("OP-Atom output exceeds 64 audio channels"));
        }
        for path in filmcraft_export::opatom_audio_paths(&settings.path, channels as usize) {
            sources.protect(Path::new(&path))?;
        }
    }
    if let Some(file) = &settings.output_file {
        if sources.virtual_files {
            return Err(error("pre-opened filesystem handles are unavailable for virtual exports"));
        }
        protect_handle(&sources, file)?;
    }
    Ok(())
}
#[cfg(not(target_arch = "wasm32"))]
fn protect_handle(sources: &Sources, file: &std::fs::File) -> Result<()> {
    let handle = same_file::Handle::from_file(file.try_clone().map_err(error)?).map_err(error)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    handle.hash(&mut hasher);
    if let Some(paths) = sources.identities.get(&hasher.finish()) {
        for path in paths {
            let source = same_file::Handle::from_path(path).map_err(error)?;
            if handle == source {
                return Err(error("pre-opened export handle aliases protected media/project"));
            }
        }
    }
    Ok(())
}
#[cfg(target_arch = "wasm32")]
fn protect_handle(_sources: &Sources, _file: &std::fs::File) -> Result<()> {
    Err(error("filesystem output handles are unavailable"))
}

/// Protects every Services-backed file write, including commands added later. Source directory
/// discovery and inode inspection happen on the first write, not on ordinary timeline commands.
/// Readers delegate unchanged, preserving native random-access/streamed 8K media behavior.
pub(crate) struct SourceWriteServices {
    inner: Arc<dyn Services>,
    project: Arc<Project>,
    project_path: Option<String>,
    paths: crate::media_pool::SourcePaths,
    virtual_files: bool,
    sources: OnceLock<std::result::Result<Sources, String>>,
}
impl SourceWriteServices {
    pub(crate) fn for_command(s: &Session, id: &str) -> Result<Arc<dyn Services>> {
        let mut inner = s.services.clone();
        let mut settled = false;
        for _ in 0..64 {
            if let Some(base) = inner.source_write_base() {
                inner = base;
            } else {
                settled = true;
                break;
            }
        }
        if !settled {
            return Err(error("source-write service nesting exceeds64"));
        }
        let project_path = if matches!(id, "file.save" | "file.saveAs" | "file.saveCopy") { None } else { s.path.clone() };
        Ok(Arc::new(Self {
            virtual_files: cfg!(target_arch = "wasm32") || inner.export_in_memory(),
            inner,
            project: s.project.clone(),
            project_path,
            paths: s.media.path_snapshot(),
            sources: OnceLock::new(),
        }))
    }
    fn check(&self, path: &str) -> std::io::Result<()> {
        let sources = self
            .sources
            .get_or_init(|| collect_snapshot(&self.project, self.project_path.as_deref(), &self.paths, self.virtual_files).map_err(|e| e.to_string()));
        let sources = sources.as_ref().map_err(|message| std::io::Error::new(std::io::ErrorKind::PermissionDenied, message.clone()))?;
        sources.protect(Path::new(path)).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e.to_string()))
    }
}
impl Services for SourceWriteServices {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.inner.read_file(path)
    }
    fn write_file(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        self.check(path)?;
        self.inner.write_file(path, data)
    }
    fn write_file_new(&self, path: &str, data: &[u8]) -> std::io::Result<()> {
        if let Err(error) = self.check(path) {
            // Preserve exclusive-creation collision semantics: importers can safely choose a
            // different generated name when an existing original already occupies this one.
            if self.inner.file_size(path).is_ok() {
                return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, error.to_string()));
            }
            return Err(error);
        }
        self.inner.write_file_new(path, data)
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        self.inner.file_size(path)
    }
    fn read_range(&self, path: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, len)
    }
    fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        self.inner.reader(path)
    }
    fn list_dir(&self, path: &str) -> Option<std::io::Result<Vec<String>>> {
        self.inner.list_dir(path)
    }
    fn list_entries(&self, path: &str) -> Option<std::io::Result<Vec<crate::media_browser::DirEntry>>> {
        self.inner.list_entries(path)
    }
    fn file_loader(&self) -> Option<filmcraft_media::sequence::FrameLoader> {
        self.inner.file_loader()
    }
    fn export_in_memory(&self) -> bool {
        self.inner.export_in_memory()
    }
    fn volumes(&self) -> Vec<crate::media_browser::Volume> {
        self.inner.volumes()
    }
    fn home_dir(&self) -> Option<String> {
        self.inner.home_dir()
    }
    fn source_write_base(&self) -> Option<Arc<dyn Services>> {
        Some(self.inner.clone())
    }
}

#[cfg(test)]
mod tests;
