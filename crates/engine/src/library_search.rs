//! Incremental local source-range index. Transcript/measurement retrieval is always available;
//! genuine semantic retrieval requires matching image and text vectors from a declared model.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use base64::Engine as _;
use filmcraft_project::{ItemId, MediaRef};
use filmcraft_render::Image;
use filmcraft_time::Tick;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::analysis_support::{SubjectArgs, Target, bounded_decode};
use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};

const MAX_ENTRIES: usize = 4096;
const MAX_DIM: usize = 4096;
const MAX_VECTOR_VALUES: usize = 2_097_152;
const CACHE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddingModel {
    pub id: String,
    pub dimension: usize,
    pub provenance: String,
}
impl EmbeddingModel {
    fn validate(&self) -> Result<()> {
        if self.id.is_empty() || self.id.len() > 256 || self.provenance.is_empty() || self.provenance.len() > 2048 || !(1..=MAX_DIM).contains(&self.dimension) {
            return Err(bad("library.embeddings", "model needs an ID, provenance, and dimension 1..4096"));
        }
        Ok(())
    }
}
/// Local image/text inference. The built-in CLIP provider is installed explicitly through
/// library.model.*; indexing/search never download models or fabricate semantic vectors.
pub trait EmbeddingProvider: Send + Sync {
    fn model(&self) -> EmbeddingModel;
    fn embed_image(&self, image: &Image) -> Result<Vec<f32>>;
    fn embed_text(&self, text: &str) -> Result<Vec<f32>>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Vector {
    model: EmbeddingModel,
    values: Vec<f32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Entry {
    id: String,
    item: u64,
    start: i64,
    end: i64,
    frame: i64,
    fingerprint: String,
    descriptors: Vec<String>,
    metrics: Option<crate::footage_analysis::FrameMetrics>,
    words: Vec<crate::footage_analysis::SpeechWord>,
    embedding: Option<Vector>,
}
#[derive(Default)]
pub struct IndexStore {
    identity: Option<Arc<()>>,
    entries: BTreeMap<String, Entry>,
    /// Install a local image/text embedding provider before `library.index` or `library.search`.
    pub provider: Option<Arc<dyn EmbeddingProvider>>,
    generation: Arc<()>,
    pending: Option<PendingIndex>,
    last_async: Option<Value>,
}
impl IndexStore {
    pub(crate) fn cancel_index(&self) {
        if let Some(pending) = &self.pending {
            pending.job.progress.cancel.store(true, Ordering::Relaxed);
        }
    }
    pub(crate) fn index_pending(&self) -> bool {
        self.pending.is_some()
    }
    fn replace_entries(&mut self, entries: BTreeMap<String, Entry>) {
        self.entries = entries;
        self.generation = Arc::new(());
    }
    fn ensure_document(&mut self, id: &Arc<()>) {
        if self.identity.as_ref().is_none_or(|old| !Arc::ptr_eq(old, id)) {
            self.replace_entries(BTreeMap::new());
            self.identity = Some(id.clone());
        }
    }
}
fn normalized(values: Vec<f32>, dimension: usize) -> Result<Vec<f32>> {
    if values.len() != dimension || values.iter().any(|n| !n.is_finite()) {
        return Err(bad("library.embeddings", "vector dimension mismatch or non-finite value"));
    }
    let norm = values.iter().map(|n| f64::from(*n).powi(2)).sum::<f64>().sqrt();
    if !norm.is_finite() || norm < 1e-20 {
        return Err(bad("library.embeddings", "vector norm must be finite and positive"));
    }
    Ok(values.into_iter().map(|n| (f64::from(n) / norm) as f32).collect())
}
fn hash(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash = (*hash ^ u64::from(*byte)).wrapping_mul(0x1000_0000_01b3);
    }
}
fn fingerprint(s: &Session, item: ItemId) -> Result<String> {
    let (root, media, _) = s.project.resolve_media(item).ok_or_else(|| bad("library.index", "item does not resolve to media"))?;
    if media.offline {
        return Err(bad("library.index", "media is explicitly offline"));
    }
    let mut stamp = 0xcbf2_9ce4_8422_2325u64;
    let meta = serde_json::to_vec(&(root, media)).map_err(|e| bad("library.index", e.to_string()))?;
    if meta.len() > 1024 * 1024 {
        return Err(bad("library.index", "source metadata exceeds 1 MiB"));
    }
    hash(&mut stamp, &meta);
    if let MediaRef::File { path } = &media.media {
        let identity = crate::relink::identity_of(&*s.services, path).map_err(|e| bad("library.index", format!("cannot fingerprint source: {e}")))?;
        hash(&mut stamp, &identity.size.to_le_bytes());
        hash(&mut stamp, &identity.fingerprint.to_le_bytes());
    }
    if let Some(transcript) = s.project.transcripts.get(&item).or_else(|| s.project.transcripts.get(&root)) {
        if transcript.words.len() > 100_000 {
            return Err(bad("library.index", "transcript exceeds 100000 words"));
        }
        let mut bytes = 0usize;
        for word in &transcript.words {
            bytes = bytes.saturating_add(word.text.len());
            if bytes > 8 * 1024 * 1024 {
                return Err(bad("library.index", "transcript text exceeds 8 MiB"));
            }
            hash(&mut stamp, word.text.as_bytes());
            hash(&mut stamp, &word.start.0.to_le_bytes());
            hash(&mut stamp, &word.end.0.to_le_bytes());
        }
    }
    if let Some(it) = s.project.item(item) {
        hash(&mut stamp, it.name.as_bytes());
    }
    Ok(format!("{stamp:016x}"))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IndexRequest {
    items: Vec<u64>,
    start: Option<i64>,
    end: Option<i64>,
    max_frames: Option<usize>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    embeddings: bool,
}
fn expected_samples(target: &Target, a: Tick, b: Tick, max: usize) -> usize {
    let count = target.rate().frame_at(b - Tick(1)).saturating_sub(target.rate().frame_at(a)).saturating_add(1).max(1);
    let stride = count.saturating_add(max as i64 - 1) / (max as i64);
    usize::try_from(count.saturating_add(stride - 1) / stride).unwrap_or(max)
}
fn index(s: &mut Session, p: &Value) -> Result<Value> {
    index_with_progress(s, p, None)
}
fn index_with_progress(s: &mut Session, p: &Value, progress: Option<&filmcraft_export::Progress>) -> Result<Value> {
    if s.library_index.index_pending() {
        return Err(bad("library.index", "an asynchronous index is running; inspect or cancel its job before changing the index"));
    }
    let p: IndexRequest = bounded_decode(p, "library.index")?;
    if p.items.is_empty() || p.items.len() > 32 {
        return Err(bad("library.index", "index 1..32 source items per bounded call"));
    }
    let max = p.max_frames.unwrap_or(24);
    if !(1..=256).contains(&max) || max.saturating_mul(p.items.len()) > 512 {
        return Err(bad("library.index", "use 1..256 frames per item, at most 512 frames in one call"));
    }
    let ids: BTreeSet<_> = p.items.iter().copied().collect();
    if ids.len() != p.items.len() {
        return Err(bad("library.index", "duplicate source item"));
    }
    s.library_index.ensure_document(&s.project_instance);
    let provider = if p.embeddings {
        Some(s.library_index.provider.clone().ok_or_else(|| {
            bad("library.index", "load a verified local model with library.model.load, or supply compatible model vectors with library.embeddings")
        })?)
    } else {
        None
    };
    let model = provider.as_ref().map(|p| p.model());
    if let Some(model) = &model {
        model.validate()?;
    }
    let mut staged = Vec::new();
    let (mut staged_words, mut staged_text_bytes) = (0usize, 0usize);
    let mut summaries = Vec::new();
    let mut replaced = BTreeSet::new();
    let mut completed = 0u64;
    for id in p.items {
        index_progress(progress, completed, format!("Analyzing source item {id}"))?;
        let target = Target::resolve(s, &SubjectArgs { target: Some("source".into()), item: Some(id), ..Default::default() })?;
        let (a, b) = target.window(p.start, p.end)?;
        let stamp = fingerprint(s, ItemId(id))?;
        let old: Vec<_> = s.library_index.entries.values().filter(|e| e.item == id && e.start >= a.0 && e.end <= b.0).collect();
        if !p.force
            && old.len() == if target.video { expected_samples(&target, a, b, max) } else { max.min(32) }
            && !old.is_empty()
            && old.iter().all(|e| e.fingerprint == stamp)
            && (!p.embeddings || old.iter().all(|e| e.embedding.as_ref().is_some_and(|v| Some(&v.model) == model.as_ref())))
        {
            completed = completed.saturating_add(max as u64);
            index_progress(progress, completed, format!("Source item {id} is current"))?;
            summaries.push(json!({"item":id,"status":"unchanged","entries":old.len(),"fingerprint":stamp}));
            continue;
        }
        let request: crate::footage_analysis::Request = serde_json::from_value(
            json!({"subject":{"target":"source","item":id},"start":a.0,"end":b.0,"maxFrames":max,"audio":false,"music":false,"waveform":false}),
        )
        .map_err(|e| bad("library.index", e.to_string()))?;
        let analysis = crate::footage_analysis::analyze(s, &request)?;
        let mut ranges = Vec::new();
        if target.video {
            for (index, frame) in analysis.frames.iter().enumerate() {
                let end = analysis.frames.get(index + 1).map_or(b.0, |f| f.position.time);
                ranges.push((frame.position.time, end, frame.position.frame, Some(frame.metrics.clone()), frame.descriptors.clone()));
            }
        } else {
            let count = max.min(32);
            let length = b.0 - a.0;
            let step = (length.saturating_add(count as i64 - 1) / count as i64).max(1);
            let mut at = a.0;
            while at < b.0 && ranges.len() < count {
                let end = at.saturating_add(step).min(b.0);
                ranges.push((at, end, target.rate().frame_at(Tick(at)), None, vec!["audio".into()]));
                at = end;
            }
        }
        let visual = if provider.is_some() && target.video { Some(target.video_subject(s)?) } else { None };
        for (start, end, frame, metrics, descriptors) in ranges {
            index_progress(progress, completed, format!("Indexing source item {id}, frame {frame}"))?;
            let mut words = Vec::new();
            for word in analysis.speech_words.iter().filter(|w| w.start.time < end && w.end.time > start).take(4096) {
                staged_words = staged_words.saturating_add(1);
                staged_text_bytes = staged_text_bytes.saturating_add(word.text.len());
                if staged_words > 100_000 || staged_text_bytes > 8 * 1024 * 1024 {
                    return Err(bad("library.index", "index staging exceeds evidence word/text budget"));
                }
                words.push(word.clone());
            }
            let embedding = if let (Some(provider), Some(model)) = (&provider, &model) {
                let values = if let Some(subject) = &visual {
                    let (image, offline) = subject.render(s, Tick(start), subject.scale(&json!({"maxSide":512}), 1)?, false)?;
                    if offline.as_array().is_some_and(|a| !a.is_empty()) {
                        return Err(bad("library.index", "cannot embed offline media"));
                    }
                    provider.embed_image(&image)?
                } else {
                    let text = words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
                    if text.is_empty() { Vec::new() } else { provider.embed_text(&text)? }
                };
                if values.is_empty() { None } else { Some(Vector { model: model.clone(), values: normalized(values, model.dimension)? }) }
            } else {
                None
            };
            if staged.len() >= MAX_ENTRIES {
                return Err(bad("library.index", "index staging exceeds 4096 ranges"));
            }
            completed = completed.saturating_add(1);
            index_progress(progress, completed, format!("Indexed source item {id}, frame {frame}"))?;
            staged.push(Entry {
                id: format!("{id}:{start}:{end}:{stamp}"),
                item: id,
                start,
                end,
                frame,
                fingerprint: stamp.clone(),
                descriptors,
                metrics,
                words,
                embedding,
            });
        }
        replaced.insert((id, a.0, b.0));
        summaries.push(
            json!({"item":id,"status":"indexed","fingerprint":stamp,"window":{"start":a.0,"end":b.0},"sampling":analysis.video,"speech":analysis.speech}),
        );
    }
    let retained = s.library_index.entries.values().filter(|e| !replaced.iter().any(|(id, a, b)| e.item == *id && e.start < *b && e.end > *a));
    let mut next: BTreeMap<_, _> = retained.map(|e| (e.id.clone(), e.clone())).collect();
    for e in staged {
        next.insert(e.id.clone(), e);
    }
    validate_store(&next)?;
    s.library_index.replace_entries(next);
    Ok(
        json!({"revision":s.revision,"entries":s.library_index.entries.len(),"items":summaries,"semanticModel":model,"methods":{"fallback":"literal transcript and measured visual descriptors","semantic":if provider.is_some(){"local_provider"}else{"requires a loaded local model or supplied compatible image/text vectors"}},"projectChanged":false}),
    )
}
fn validate_store(entries: &BTreeMap<String, Entry>) -> Result<()> {
    if entries.len() > MAX_ENTRIES {
        return Err(bad("library.index", "index exceeds 4096 ranges; use a project-specific subset or clear it"));
    }
    let word_count = entries.values().map(|e| e.words.len()).fold(0usize, usize::saturating_add);
    let text_bytes = entries.values().flat_map(|e| &e.words).map(|w| w.text.len()).fold(0usize, usize::saturating_add);
    if word_count > 100_000 || text_bytes > 8 * 1024 * 1024 {
        return Err(bad("library.index", "index transcript evidence exceeds 100000 words / 8 MiB text"));
    }
    let values = entries.values().map(|e| e.embedding.as_ref().map_or(0, |v| v.values.len())).fold(0usize, usize::saturating_add);
    if values > MAX_VECTOR_VALUES {
        return Err(bad("library.embeddings", "index exceeds 2097152 embedding values"));
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Supplied {
    model: EmbeddingModel,
    entries: Vec<SuppliedVector>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuppliedVector {
    id: String,
    vector: Vec<f32>,
}
fn embeddings(s: &mut Session, p: &Value) -> Result<Value> {
    let p: Supplied = bounded_decode(p, "library.embeddings")?;
    p.model.validate()?;
    if p.entries.is_empty() || p.entries.len() > 512 {
        return Err(bad("library.embeddings", "supply 1..512 indexed ranges per call"));
    }
    s.library_index.ensure_document(&s.project_instance);
    let mut next = s.library_index.entries.clone();
    let mut seen = BTreeSet::new();
    let mut stamps = BTreeMap::new();
    for supplied in p.entries {
        if !seen.insert(supplied.id.clone()) {
            return Err(bad("library.embeddings", "duplicate range ID"));
        }
        let entry = next.get_mut(&supplied.id).ok_or_else(|| bad("library.embeddings", "unknown indexed range ID"))?;
        if stamps.entry(entry.item).or_insert_with(|| fingerprint(s, ItemId(entry.item)).ok()).as_deref() != Some(&entry.fingerprint) {
            return Err(bad("library.embeddings", "source changed; rebuild its index before attaching vectors"));
        }
        entry.embedding = Some(Vector { model: p.model.clone(), values: normalized(supplied.vector, p.model.dimension)? });
    }
    validate_store(&next)?;
    s.library_index.replace_entries(next);
    Ok(json!({"model":p.model,"updated":seen.len(),"normalization":"unit_l2","projectChanged":false}))
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Query {
    query: String,
    limit: Option<usize>,
    #[serde(default)]
    thumbnails: bool,
    embedding: Option<QueryVector>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryVector {
    model: EmbeddingModel,
    vector: Vec<f32>,
}
fn tokens(s: &str) -> BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).take(256).map(str::to_lowercase).collect()
}
fn lexical(query: &BTreeSet<String>, entry: &Entry, name: &str) -> (f32, Vec<String>) {
    let words = entry.words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
    let spoken = tokens(&words);
    let measured = tokens(&entry.descriptors.join(" "));
    let named = tokens(name);
    let mut score = 0f32;
    let mut evidence = Vec::new();
    for term in query {
        if spoken.contains(term) {
            score += 1.0;
            evidence.push(format!("transcript:{term}"));
        }
        if measured.contains(term) {
            score += 0.6;
            evidence.push(format!("measured:{term}"));
        }
        if named.contains(term) {
            score += 0.25;
            evidence.push(format!("source_name:{term}"));
        }
    }
    (score / query.len().max(1) as f32, evidence)
}
fn thumbnail(s: &Session, entry: &Entry) -> Result<Value> {
    let target = Target::resolve(s, &SubjectArgs { target: Some("source".into()), item: Some(entry.item), ..Default::default() })?;
    if !target.video {
        return Ok(json!({"status":"audio_only"}));
    }
    let subject = target.video_subject(s)?;
    let time = subject.validate_time(Tick(entry.start))?;
    let (image, offline) = subject.render(s, time, subject.scale(&json!({"maxSide":160}), 1)?, false)?;
    if offline.as_array().is_some_and(|a| !a.is_empty()) {
        return Err(bad("library.search", "thumbnail source is offline"));
    }
    Ok(
        json!({"mimeType":"image/png","base64":base64::engine::general_purpose::STANDARD.encode(crate::inspection::png_image(&image)?),"metadata":subject.metadata(time,false)}),
    )
}
fn search(s: &mut Session, p: &Value) -> Result<Value> {
    let p: Query = bounded_decode(p, "library.search")?;
    if p.query.trim().is_empty() || p.query.len() > 8192 {
        return Err(bad("library.search", "query must contain 1..8192 bytes"));
    }
    let limit = p.limit.unwrap_or(10);
    if !(1..=50).contains(&limit) || p.thumbnails && limit > 10 {
        return Err(bad("library.search", "limit 1..50; at most 10 thumbnails"));
    }
    s.library_index.ensure_document(&s.project_instance);
    let query_vector = if let Some(v) = p.embedding {
        v.model.validate()?;
        Some(Vector { values: normalized(v.vector, v.model.dimension)?, model: v.model })
    } else if let Some(provider) = &s.library_index.provider {
        let model = provider.model();
        model.validate()?;
        Some(Vector { values: normalized(provider.embed_text(&p.query)?, model.dimension)?, model })
    } else {
        None
    };
    let terms = tokens(&p.query);
    let mut candidates = Vec::new();
    let mut stamps = BTreeMap::new();
    let mut stale = BTreeSet::new();
    let mut semantic_candidates = 0usize;
    for entry in s.library_index.entries.values() {
        let stamp = stamps.entry(entry.item).or_insert_with(|| fingerprint(s, ItemId(entry.item)).ok());
        if stamp.as_deref() != Some(&entry.fingerprint) {
            stale.insert(entry.item);
            continue;
        }
        let name = s.project.item(ItemId(entry.item)).map_or("", |i| i.name.as_str());
        let (text_score, evidence) = lexical(&terms, entry, name);
        let semantic = query_vector.as_ref().and_then(|q| {
            entry.embedding.as_ref().filter(|v| v.model == q.model).map(|v| v.values.iter().zip(&q.values).map(|(a, b)| a * b).sum::<f32>().clamp(-1.0, 1.0))
        });
        if semantic.is_some() {
            semantic_candidates += 1;
        }
        let score = semantic.map_or(text_score, |cosine| 0.8 * cosine.max(0.0) + 0.2 * text_score.min(1.0));
        if score > 0.0 {
            candidates.push((score, entry, semantic, text_score, evidence));
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
    let mut hits = Vec::new();
    for (score, entry, cosine, text_score, evidence) in candidates.into_iter().take(limit) {
        let target = Target::resolve(s, &SubjectArgs { target: Some("source".into()), item: Some(entry.item), ..Default::default() })?;
        hits.push(json!({"id":entry.id,"item":entry.item,"name":s.project.item(ItemId(entry.item)).map(|i|&i.name),"start":target.position(Tick(entry.start)),"end":target.position(Tick(entry.end)),"score":score,
            "ranking":{"cosine":cosine,"lexical":text_score,"matches":evidence,"semanticModel":entry.embedding.as_ref().filter(|_|cosine.is_some()).map(|v|&v.model)},
            "transcriptEvidence":entry.words,"measuredDescriptors":entry.descriptors,"metrics":entry.metrics,"thumbnail":if p.thumbnails{Some(thumbnail(s,entry)?)}else{None}}));
    }
    Ok(
        json!({"revision":s.revision,"query":p.query,"hits":hits,"staleItems":stale,"semantic":{"status":if semantic_candidates>0{"used_compatible_model_vectors"}else{"unavailable_for_indexed_ranges"},"model":query_vector.map(|v|v.model),"candidateCount":semantic_candidates},
        "fallback":"Literal transcript words, source names and measured color/motion/detail descriptors. Object, person and wide-shot semantics require compatible image/text embeddings; no such inference is claimed by the fallback."}),
    )
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanRequest {
    hits: Vec<String>,
    name: Option<String>,
    sequence: Option<u64>,
    at: Option<i64>,
}
fn propose(s: &mut Session, p: &Value, broll: bool) -> Result<Value> {
    let p: PlanRequest = bounded_decode(p, if broll { "library.brollPlan" } else { "library.selectsPlan" })?;
    if p.hits.is_empty() || p.hits.len() > 128 {
        return Err(bad("library.selectsPlan", "choose 1..128 indexed range IDs"));
    }
    let name = p.name.unwrap_or_else(|| if broll { "Proposed B-roll".into() } else { "Library selects".into() });
    if name.len() > 512 || name.trim().is_empty() {
        return Err(bad("library.selectsPlan", "name must contain 1..512 bytes"));
    }
    s.library_index.ensure_document(&s.project_instance);
    let mut chosen = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stamps = BTreeMap::new();
    for id in p.hits {
        if !seen.insert(id.clone()) {
            return Err(bad("library.selectsPlan", "duplicate range ID"));
        }
        let e = s.library_index.entries.get(&id).ok_or_else(|| bad("library.selectsPlan", "unknown indexed range ID"))?;
        if stamps.entry(e.item).or_insert_with(|| fingerprint(s, ItemId(e.item)).ok()).as_deref() != Some(&e.fingerprint) {
            return Err(bad("library.selectsPlan", "source changed; refresh the index"));
        }
        chosen.push(e.clone());
    }
    let first = chosen.first().ok_or_else(|| bad("library.selectsPlan", "no source ranges"))?;
    let mut steps = Vec::new();
    let mut at = p.at.unwrap_or(0);
    if at < 0 {
        return Err(bad("library.brollPlan", "at must be nonnegative ticks"));
    }
    if broll {
        let id = p.sequence.or(s.state.active_sequence.map(|i| i.0)).ok_or(crate::EngineError::NoSequence)?;
        if s.project.sequence(ItemId(id)).is_none() {
            return Err(crate::EngineError::NoSequence);
        }
        if chosen.iter().any(|e| !s.project.source_streams(ItemId(e.item)).0) {
            return Err(bad("library.brollPlan", "B-roll placements require video source ranges"));
        }
        steps.push(json!({"id":"sequence.open","params":{"item":id}}));
        steps.push(json!({"id":"sequence.addTracks","params":{"video":1,"audio":0}}));
    } else {
        if p.sequence.is_some() || at != 0 {
            return Err(bad("library.selectsPlan", "selects creates a new timeline beginning at zero; use brollPlan to place into a sequence"));
        }
        let target = Target::resolve(s, &SubjectArgs { target: Some("source".into()), item: Some(first.item), ..Default::default() })?;
        steps.push(json!({"id":"file.newSequence","params":{"name":name,"width":target.settings.width,"height":target.settings.height,"frameRate":target.rate(),"sampleRate":target.settings.sample_rate,"video":1,"audio":1}}));
    }
    for entry in chosen {
        let duration = entry.end.checked_sub(entry.start).filter(|d| *d > 0).ok_or_else(|| bad("library.selectsPlan", "invalid indexed range"))?;
        let mut params = json!({"item":entry.item,"sourceIn":entry.start,"duration":duration,"time":at,"insert":false});
        if broll {
            params["audio"] = json!(false);
            params["track"] = json!({"$step":1,"path":"/video/0"});
        }
        steps.push(json!({"id":"timeline.place","params":params}));
        at = at.checked_add(duration).filter(|n| *n <= Tick::MAX.0).ok_or_else(|| bad("library.selectsPlan", "selects duration overflow"))?;
    }
    s.execute("agent.plan", json!({"label":name,"baseRevision":s.revision,"steps":steps}))
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cache {
    version: u32,
    entries: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathRequest {
    path: String,
}
fn cache(s: &mut Session, p: &Value, save: bool) -> Result<Value> {
    let p: PathRequest = bounded_decode(p, "library.cache")?;
    if p.path.is_empty() || p.path.len() > 4096 {
        return Err(bad("library.cache", "path must contain 1..4096 bytes"));
    }
    s.library_index.ensure_document(&s.project_instance);
    if save {
        let bytes = serde_json::to_vec(&Cache { version: 1, entries: s.library_index.entries.values().cloned().collect() })
            .map_err(|e| bad("library.save", e.to_string()))?;
        if bytes.len() > CACHE_BYTES {
            return Err(bad("library.save", "cache exceeds 16 MiB"));
        }
        s.services.write_file(&p.path, &bytes).map_err(|e| bad("library.save", e.to_string()))?;
        Ok(json!({"path":p.path,"entries":s.library_index.entries.len(),"bytes":bytes.len(),"version":1}))
    } else {
        let size = s.services.file_size(&p.path).map_err(|e| bad("library.load", e.to_string()))?;
        if size > CACHE_BYTES as u64 {
            return Err(bad("library.load", "cache exceeds 16 MiB"));
        }
        let bytes = s.services.read_range(&p.path, 0, CACHE_BYTES).map_err(|e| bad("library.load", e.to_string()))?;
        if bytes.len() as u64 != size {
            return Err(bad("library.load", "cache changed or could not be read fully"));
        }
        let c: Cache = serde_json::from_slice(&bytes).map_err(|e| bad("library.load", e.to_string()))?;
        if c.entries.len() > MAX_ENTRIES {
            return Err(bad("library.load", "cache exceeds 4096 ranges"));
        }
        if c.version != 1 {
            return Err(bad("library.load", "unsupported index version"));
        }
        let mut entries = BTreeMap::new();
        let mut stale = BTreeSet::new();
        let mut stamps = BTreeMap::new();
        for mut entry in c.entries {
            if entry.id.len() > 512 || entry.start < 0 || entry.end <= entry.start || entry.words.len() > 4096 || entry.descriptors.len() > 32 {
                return Err(bad("library.load", "invalid cache range"));
            }
            if stamps.entry(entry.item).or_insert_with(|| fingerprint(s, ItemId(entry.item)).ok()).as_deref() != Some(&entry.fingerprint) {
                stale.insert(entry.item);
                continue;
            }
            if let Some(v) = entry.embedding.as_mut() {
                v.model.validate()?;
                v.values = normalized(std::mem::take(&mut v.values), v.model.dimension)?;
            }
            if entries.insert(entry.id.clone(), entry).is_some() {
                return Err(bad("library.load", "duplicate range ID in cache"));
            }
        }
        validate_store(&entries)?;
        s.library_index.replace_entries(entries);
        Ok(json!({"path":p.path,"entries":s.library_index.entries.len(),"staleItems":stale,"version":1}))
    }
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "library.indexAsync",
            label: "Index Library in Background",
            menu: &[],
            shortcut: None,
            params: r#"{"items":[id],"start":ticks?,"end":ticks?,"maxFrames":1..256=24,"force":bool=false,"embeddings":bool=false}"#,
            enabled: always,
            run: index_async,
            journal: false,
        },
        CommandSpec {
            id: "library.indexStatus",
            label: "Inspect Background Library Index",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: always,
            run: index_status,
            journal: false,
        },
        CommandSpec {
            id: "library.index",
            label: "Index local footage evidence",
            menu: &[],
            shortcut: None,
            params: r#"{"items":[id],"start":ticks?,"end":ticks?,"maxFrames":1..256=24,"force":bool=false,"embeddings":bool=false}"#,
            enabled: always,
            run: index,
            journal: false,
        },
        CommandSpec {
            id: "library.embeddings",
            label: "Attach provenance-tagged semantic vectors",
            menu: &[],
            shortcut: None,
            params: r#"{"model":{"id":str,"dimension":1..4096,"provenance":str},"entries":[{"id":rangeId,"vector":[finiteFloat]}]}"#,
            enabled: always,
            run: embeddings,
            journal: false,
        },
        CommandSpec {
            id: "library.search",
            label: "Search source ranges",
            menu: &[],
            shortcut: None,
            params: r#"{"query":str,"limit":1..50=10,"thumbnails":bool=false,"embedding":{"model":{"id":str,"dimension":n,"provenance":str},"vector":[float]}?}"#,
            enabled: always,
            run: search,
            journal: false,
        },
        CommandSpec {
            id: "library.selectsPlan",
            label: "Propose a selects timeline",
            menu: &[],
            shortcut: None,
            params: r#"{"hits":[rangeId],"name":str?}"#,
            enabled: always,
            run: |s, p| propose(s, p, false),
            journal: false,
        },
        CommandSpec {
            id: "library.brollPlan",
            label: "Propose B-roll placements",
            menu: &[],
            shortcut: None,
            params: r#"{"hits":[rangeId],"name":str?,"sequence":id?,"at":ticks=0}"#,
            enabled: always,
            run: |s, p| propose(s, p, true),
            journal: false,
        },
        CommandSpec {
            id: "library.save",
            label: "Save the local library index",
            menu: &[],
            shortcut: None,
            params: r#"{"path":str}"#,
            enabled: always,
            run: |s, p| cache(s, p, true),
            journal: false,
        },
        CommandSpec {
            id: "library.load",
            label: "Load a validated local library index",
            menu: &[],
            shortcut: None,
            params: r#"{"path":str}"#,
            enabled: always,
            run: |s, p| cache(s, p, false),
            journal: false,
        },
        CommandSpec {
            id: "library.clear",
            label: "Clear the local library index",
            menu: &[],
            shortcut: None,
            params: r#"{}"#,
            enabled: always,
            run: |s, p| {
                if p.as_object().is_none_or(|o| !o.is_empty()) {
                    return Err(bad("library.clear", "expected an empty object"));
                }
                s.library_index.cancel_index();
                s.library_index.replace_entries(BTreeMap::new());
                Ok(json!({"entries":0}))
            },
            journal: false,
        },
    ]
}
#[cfg(test)]
mod async_tests;
#[cfg(test)]
mod tests;

struct PendingIndex {
    job: crate::Job,
    project: Arc<()>,
    revision: u64,
    generation: Arc<()>,
    items: BTreeSet<u64>,
    provider: Option<EmbeddingModel>,
    result: Arc<Mutex<Option<std::result::Result<IndexOutput, String>>>>,
}
struct IndexOutput {
    entries: BTreeMap<String, Entry>,
    report: Value,
    seconds: f64,
}
fn index_progress(progress: Option<&filmcraft_export::Progress>, done: u64, status: String) -> Result<()> {
    if filmcraft_media::cancel::cancelled() || progress.is_some_and(|p| p.cancel.load(Ordering::Relaxed)) {
        return Err(bad("library.indexAsync", "indexing cancelled"));
    }
    if let Some(progress) = progress {
        progress.done.store(done, Ordering::Relaxed);
        *progress.status.lock().unwrap_or_else(|e| e.into_inner()) = status;
    }
    Ok(())
}
fn finish_index(job: &crate::Job, result: std::result::Result<filmcraft_export::Report, String>) {
    if let Err(error) = &result {
        *job.progress.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
        *job.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = error.clone();
    }
    *job.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    job.progress.finished.store(true, Ordering::Relaxed);
}
/// Main-thread cache installation; a worker never replaces the live index or project.
pub fn poll(s: &mut Session) {
    let Some(pending) = &s.library_index.pending else { return };
    let output = pending.result.lock().unwrap_or_else(|e| e.into_inner()).take();
    let error =
        if output.is_none() { pending.job.result.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|r| r.as_ref().err().cloned()) } else { None };
    if output.is_none() && error.is_none() {
        return;
    }
    let Some(pending) = s.library_index.pending.take() else { return };
    let result = output.unwrap_or_else(|| Err(error.unwrap_or_else(|| "index worker ended without a result".into())));
    let result = result.and_then(|output| {
        if pending.job.progress.cancel.load(Ordering::Relaxed) {
            return Err("indexing cancelled before cache installation".into());
        }
        if !Arc::ptr_eq(&pending.project, &s.project_instance)
            || pending.revision != s.revision
            || !Arc::ptr_eq(&pending.generation, &s.library_index.generation)
        {
            return Err("stale_index: project/revision/index changed; completed measurements were not installed".into());
        }
        if s.library_index.provider.as_ref().map(|p| p.model()) != pending.provider {
            return Err("stale_index: embedding provider changed; result was not installed".into());
        }
        validate_store(&output.entries).map_err(|e| e.to_string())?;
        let mut fingerprints = BTreeMap::new();
        for entry in output.entries.values().filter(|entry| pending.items.contains(&entry.item)) {
            let expected = fingerprints.entry(entry.item).or_insert_with(|| fingerprint(s, ItemId(entry.item)).ok());
            if expected.as_deref() != Some(&entry.fingerprint) {
                return Err("stale_index: source file or transcript changed during indexing; result was not installed".into());
            }
        }
        Ok(output)
    });
    let status = match result {
        Ok(output) => {
            let bytes = serde_json::to_vec(&output.entries).map(|v| v.len() as u64);
            match bytes {
                Ok(bytes) if bytes <= CACHE_BYTES as u64 => {
                    let entries = output.entries.len();
                    s.library_index.replace_entries(output.entries);
                    *pending.job.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = "Library index ready".into();
                    pending.job.progress.done.store(pending.job.progress.total.load(Ordering::Relaxed), Ordering::Relaxed);
                    finish_index(
                        &pending.job,
                        Ok(filmcraft_export::Report {
                            path: String::new(),
                            frames: entries as u64,
                            seconds: output.seconds,
                            bytes,
                            render_fps: if output.seconds > 0.0 { entries as f64 / output.seconds } else { 0.0 },
                            extra_files: Vec::new(),
                        }),
                    );
                    json!({"job":pending.job.id,"status":"installed","inputRevision":pending.revision,"revision":s.revision,"entries":entries,"report":output.report,"projectChanged":false})
                }
                _ => {
                    let error = "index output exceeds its16MiB serialized budget".to_string();
                    finish_index(&pending.job, Err(error.clone()));
                    json!({"job":pending.job.id,"status":"failed","error":error})
                }
            }
        }
        Err(error) => {
            finish_index(&pending.job, Err(error.clone()));
            json!({"job":pending.job.id,"inputRevision":pending.revision,"revision":s.revision,"status":if error.starts_with("stale_index"){"stale"}else if pending.job.progress.cancel.load(Ordering::Relaxed){"cancelled"}else{"failed"},"error":error,"projectChanged":false})
        }
    };
    s.library_index.last_async = Some(status);
    if let Err(error) = s.job_registry.sync(&s.jobs) {
        s.log.push(crate::panels::Level::Error, "library.indexAsync", error.to_string());
    }
}
fn index_status(s: &mut Session, p: &Value) -> Result<Value> {
    if p.as_object().is_none_or(|o| !o.is_empty()) {
        return Err(bad("library.indexStatus", "expected an empty object"));
    }
    poll(s);
    Ok(
        json!({"revision":s.revision,"entries":s.library_index.entries.len(),"pending":s.library_index.pending.as_ref().map(|p|json!({"job":p.job.id,"inputRevision":p.revision,"status":p.job.to_json()})),"lastResult":s.library_index.last_async}),
    )
}
fn index_async(s: &mut Session, p: &Value) -> Result<Value> {
    poll(s);
    let request: IndexRequest = bounded_decode(p, "library.indexAsync")?;
    if s.library_index.pending.is_some()
        || s.model_jobs.loading()
        || s.jobs.iter().any(|j| j.label == "Load local CLIP model" && !j.progress.finished.load(Ordering::Relaxed))
    {
        return Err(bad("library.indexAsync", "a library index or provider load is already running; inspect/cancel its job first"));
    }
    let max = request.max_frames.unwrap_or(24);
    if request.items.is_empty()
        || request.items.len() > 32
        || !(1..=256).contains(&max)
        || max.saturating_mul(request.items.len()) > 512
        || request.items.iter().collect::<BTreeSet<_>>().len() != request.items.len()
    {
        return Err(bad("library.indexAsync", "provide unique1..32 items,1..256 frames per item, at most512 frames"));
    }
    if request.embeddings && s.library_index.provider.is_none() {
        return Err(bad("library.indexAsync", "load the local vision model before requesting embeddings"));
    }
    for item in &request.items {
        let target = Target::resolve(s, &SubjectArgs { target: Some("source".into()), item: Some(*item), ..Default::default() })?;
        target.window(request.start, request.end)?;
    }
    s.library_index.ensure_document(&s.project_instance);
    let mut sandbox = Session::new(s.services.clone());
    sandbox.project = s.project.clone();
    sandbox.project_instance = s.project_instance.clone();
    sandbox.revision = s.revision;
    sandbox.media = s.media.clone();
    sandbox.state = s.state.clone();
    sandbox.prefs = s.prefs.clone();
    sandbox.library_index.identity = Some(s.project_instance.clone());
    sandbox.library_index.entries = s.library_index.entries.clone();
    sandbox.library_index.provider = s.library_index.provider.clone();
    let job = s.job_registry.new_job(s, "Index local media library".into())?;
    let id = job.id;
    let work = job.clone();
    job.progress.total.store((max * request.items.len()) as u64, Ordering::Relaxed);
    let output = Arc::new(Mutex::new(None));
    let result = output.clone();
    let params = p.clone();
    let run = crate::durable_jobs::guard(s, &job, move || {
        let started = web_time::Instant::now();
        let cancel = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        struct Stop(Arc<AtomicBool>);
        impl Drop for Stop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let response = std::thread::scope(|scope| -> Result<Value> {
            let stop = Stop(finished.clone());
            let done = finished.clone();
            let flag = cancel.clone();
            let progress = work.progress.clone();
            let watcher = std::thread::Builder::new()
                .name("filmcraft-index-cancel".into())
                .spawn_scoped(scope, move || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        while !done.load(Ordering::Relaxed) {
                            if progress.cancel.load(Ordering::Relaxed) {
                                flag.store(true, Ordering::Relaxed);
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }))
                    .map_err(|_| {
                        flag.store(true, Ordering::Relaxed);
                        "cancellation monitor failed"
                    })
                })
                .map_err(|e| bad("library.indexAsync", format!("could not start cancellation monitor: {e}")))?;
            let response = filmcraft_media::cancel::with_cancel(&cancel, || {
                filmcraft_media::cancel::with_background(true, || index_with_progress(&mut sandbox, &params, Some(&work.progress)))
            });
            drop(stop);
            watcher.join().map_err(|_| bad("library.indexAsync", "cancellation monitor failed"))?.map_err(|e| bad("library.indexAsync", e))?;
            response
        })
        .map(|report| IndexOutput { entries: sandbox.library_index.entries, report, seconds: started.elapsed().as_secs_f64() })
        .map_err(|e| e.to_string());
        if let Err(error) = &response {
            finish_index(&work, Err(error.clone()));
        } else {
            *work.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = "Measurements ready; checking live project before cache installation".into();
        }
        *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(response);
    });
    s.jobs.push(job.clone());
    s.library_index.pending = Some(PendingIndex {
        job,
        project: s.project_instance.clone(),
        revision: s.revision,
        generation: s.library_index.generation.clone(),
        items: request.items.into_iter().collect(),
        provider: s.library_index.provider.as_ref().map(|p| p.model()),
        result: output,
    });
    crate::durable_jobs::spawn(s, id, "filmcraft-library-index", run)?;
    Ok(
        json!({"job":id,"inputRevision":s.revision,"status":"indexing","projectChanged":false,"installation":"requires identical project identity, revision, index generation and source fingerprints"}),
    )
}
