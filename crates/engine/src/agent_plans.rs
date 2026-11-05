//! Reviewable edits evaluated on an isolated project snapshot, then installed atomically.
//!
//! The allowlist is intentional: adding a command requires checking every indirect side effect.
//! Plans never run file writes, downloads, media registration, recording, or background jobs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use filmcraft_project::{Project, Sequence, Track, TrackId, TrackItem};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::{EditorState, EngineError, Result, Services, Session};

const MAX_STEPS: usize = 256;
const MAX_PLANS: usize = 32;
const MAX_CHANGES: usize = 10_000;
const MAX_REQUEST_BYTES: usize = 1_048_576;
const MAX_REPORT_BYTES: usize = 8_388_608;

#[derive(Default)]
pub struct PlanStore {
    next_id: u64,
    plans: BTreeMap<u64, EditPlan>,
}

struct Snapshots {
    before: Arc<Project>,
    after: Arc<Project>,
    before_state: EditorState,
    after_state: EditorState,
}

struct EditPlan {
    identity: Arc<()>,
    base_revision: u64,
    label: String,
    snapshots: Option<Snapshots>,
    report: Value,
    applied: Option<Value>,
    discarded: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    id: String,
    #[serde(default)]
    params: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    base_revision: Option<u64>,
    steps: Vec<Step>,
}

/// Whether a command may be evaluated as a staged in-memory edit. No prefix-based opt-in.
pub fn eligible_command(id: &str) -> bool {
    matches!(
        id,
        "file.newSequence"
            | "sequence.open"
            | "source.open"
            | "source.setPlayhead"
            | "timeline.place"
            | "timeline.select"
            | "timeline.selectGap"
            | "timeline.move"
            | "timeline.trim"
            | "timeline.delete"
            | "timeline.razor"
            | "timeline.roll"
            | "timeline.slip"
            | "timeline.slide"
            | "timeline.rateStretch"
            | "timeline.setTargeting"
            | "playhead.set"
            | "playhead.step"
            | "sequence.addEdit"
            | "sequence.addEditAllTracks"
            | "sequence.addTracks"
            | "sequence.deleteTrack"
            | "sequence.deleteTracks"
            | "sequence.lift"
            | "sequence.extract"
            | "sequence.closeGap"
            | "sequence.removeTransition"
            | "sequence.settings"
            | "sequence.colorSettings"
            | "sequence.joinThroughEdits"
            | "sequence.linkedSelection"
            | "sequence.snap"
            | "sequence.setWorkArea"
            | "edit.selectAll"
            | "edit.deselectAll"
            | "edit.copy"
            | "edit.cut"
            | "edit.paste"
            | "edit.pasteInsert"
            | "edit.duplicate"
            | "clip.rename"
            | "clip.enable"
            | "clip.link"
            | "clip.group"
            | "clip.ungroup"
            | "clip.speedDuration"
            | "clip.audioGain"
            | "clip.scaleToFrameSize"
            | "clip.frameHold"
            | "clip.nest"
            | "effects.apply"
            | "effects.remove"
            | "effects.setParam"
            | "effects.toggleAnimation"
            | "effects.toggleEnabled"
            | "effects.reset"
            | "effects.resetParam"
            | "effects.setKeyframe"
            | "effects.deleteKeyframe"
            | "effects.setInterpolation"
            | "grade.patch"
            | "grade.copy"
            | "grade.reset"
            | "grade.matchBatch"
            | "lumetri.applyMatch"
            | "lumetri.applyPreset"
            | "lumetri.setSection"
            | "captions.newTrack"
            | "captions.deleteTrack"
            | "captions.setTrack"
            | "captions.setStyle"
            | "captions.add"
            | "captions.split"
            | "captions.merge"
            | "captions.setText"
            | "captions.setTimes"
            | "captions.trim"
            | "captions.move"
            | "captions.duplicate"
            | "captions.delete"
            | "captions.select"
            | "captions.showAll"
            | "captions.hideAll"
            | "transcript.select"
            | "transcript.extract"
            | "transcript.lift"
            | "transcript.removePauses"
            | "transcript.removeFillers"
            | "transcript.createCaptions"
            | "markers.add"
            | "markers.addRange"
            | "markers.addChapter"
            | "markers.edit"
            | "markers.clearCurrent"
            | "markers.clearAll"
            | "markers.markIn"
            | "markers.markOut"
            | "markers.clearIn"
            | "markers.clearOut"
            | "markers.clearInOut"
            | "project.setNotes"
    )
}

// Defence in depth if a future allowlisted command gains an unexpected file-write path.
struct PlanningServices(Arc<dyn Services>);
impl Services for PlanningServices {
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        self.0.read_file(path)
    }
    fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "file writes are not allowed in edit plans"))
    }
    fn file_size(&self, path: &str) -> std::io::Result<u64> {
        self.0.file_size(path)
    }
    fn read_range(&self, path: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.0.read_range(path, offset, len)
    }
    fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        self.0.reader(path)
    }
    fn file_loader(&self) -> Option<filmcraft_media::sequence::FrameLoader> {
        self.0.file_loader()
    }
    fn list_dir(&self, path: &str) -> Option<std::io::Result<Vec<String>>> {
        self.0.list_dir(path)
    }
}

/// Check tree size without serializing or recursively walking attacker-controlled nesting.
pub(crate) fn bounded_json(value: &Value, bytes_limit: usize) -> Result<()> {
    let mut stack = vec![(value, 0usize)];
    let (mut nodes, mut bytes) = (0usize, 0usize);
    while let Some((v, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if depth > 32 || nodes > 100_000 {
            return Err(bad("agent.plan", "JSON nesting or node limit exceeded"));
        }
        bytes = bytes.saturating_add(8);
        match v {
            Value::String(s) => bytes = bytes.saturating_add(s.len().saturating_mul(6)),
            Value::Array(a) => {
                if a.len().saturating_add(nodes).saturating_add(stack.len()) > 100_000 {
                    return Err(bad("agent.plan", "JSON node limit exceeded"));
                }
                stack.extend(a.iter().map(|x| (x, depth.saturating_add(1))));
            }
            Value::Object(o) => {
                if o.len().saturating_add(nodes).saturating_add(stack.len()) > 100_000 {
                    return Err(bad("agent.plan", "JSON node limit exceeded"));
                }
                for (k, x) in o {
                    bytes = bytes.saturating_add(k.len().saturating_mul(6));
                    stack.push((x, depth.saturating_add(1)));
                }
            }
            _ => {}
        }
        if bytes > bytes_limit {
            return Err(bad("agent.plan", "JSON size limit exceeded; split the proposal into smaller plans"));
        }
    }
    Ok(())
}

fn resolve(value: &Value, results: &[Value], depth: usize) -> Result<Value> {
    if depth > 32 {
        return Err(bad("agent.plan", "step reference nesting limit exceeded"));
    }
    match value {
        Value::Object(o) if o.contains_key("$step") => {
            if o.keys().any(|k| k != "$step" && k != "path") {
                return Err(bad("agent.plan", "a step reference contains only `$step` and optional `path`"));
            }
            let index = o
                .get("$step")
                .and_then(Value::as_u64)
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| bad("agent.plan", "`$step` must be a previous step index"))?;
            let source = results.get(index).ok_or_else(|| bad("agent.plan", "step references may only name earlier steps"))?;
            let path = match o.get("path") {
                None => "",
                Some(v) => v.as_str().ok_or_else(|| bad("agent.plan", "step reference `path` must be a JSON pointer"))?,
            };
            source.pointer(path).cloned().ok_or_else(|| bad("agent.plan", format!("no result at step {index} pointer `{path}`")))
        }
        Value::Object(o) => {
            o.iter().map(|(k, v)| Ok((k.clone(), resolve(v, results, depth.saturating_add(1))?))).collect::<Result<serde_json::Map<_, _>>>().map(Value::Object)
        }
        Value::Array(a) => a.iter().map(|v| resolve(v, results, depth.saturating_add(1))).collect::<Result<Vec<_>>>().map(Value::Array),
        _ => Ok(value.clone()),
    }
}

fn serialize<T: serde::Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| EngineError::Other(format!("cannot describe edit plan: {e}")))
}

fn kind(before: bool, after: bool) -> &'static str {
    match (before, after) {
        (false, true) => "added",
        (true, false) => "removed",
        _ => "changed",
    }
}

fn track_metadata(t: &Track) -> Result<Value> {
    let mut copy = t.clone();
    copy.items.clear();
    copy.transitions.clear();
    serialize(&copy)
}

fn track_map(q: Option<&Sequence>) -> BTreeMap<TrackId, &Track> {
    q.into_iter().flat_map(Sequence::all_tracks).map(|t| (t.id, t)).collect()
}

fn clip_map<'a>(tracks: &BTreeMap<TrackId, &'a Track>) -> BTreeMap<u64, (TrackId, &'a TrackItem)> {
    tracks.iter().flat_map(|(id, t)| t.items.iter().map(move |c| (c.id.0, (*id, c)))).collect()
}

fn locked_track_warnings(before: &Project) -> Vec<Value> {
    let mut warnings = Vec::new();
    for item in before.sequences() {
        if let Some(q) = item.as_sequence() {
            for track in q.all_tracks().filter(|t| t.locked) {
                warnings.push(json!({"code":"locked_track","sequence":item.id.0,"track":track.id.0,"message":"Track is locked; commands may skip its clips."}));
            }
            for track in q.caption_tracks.iter().filter(|t| t.locked) {
                warnings.push(
                    json!({"code":"locked_track","sequence":item.id.0,"track":track.id.0,"message":"Caption track is locked; commands may skip its cues."}),
                );
            }
        }
    }
    warnings
}

fn item_metadata(item: Option<&filmcraft_project::ProjectItem>) -> Result<Value> {
    let Some(item) = item else { return Ok(Value::Null) };
    let mut meta = item.clone();
    if let Some(q) = item.as_sequence() {
        meta.kind = filmcraft_project::ItemKind::Sequence(Arc::new(Sequence {
            settings: q.settings.clone(),
            video_tracks: Vec::new(),
            audio_tracks: Vec::new(),
            submix_tracks: Vec::new(),
            caption_tracks: Vec::new(),
            markers: q.markers.clone(),
            mark_in: q.mark_in,
            mark_out: q.mark_out,
            work_area: q.work_area,
            start_timecode: q.start_timecode,
            master_volume_db: q.master_volume_db,
            master_effects: q.master_effects.clone(),
            master_mixer: q.master_mixer.clone(),
            multicam: q.multicam.clone(),
            merged: q.merged.clone(),
            split: q.split,
        }));
    }
    serialize(&meta)
}

fn count_change(changes: &mut usize) -> Result<()> {
    *changes = changes.saturating_add(1);
    if *changes > MAX_CHANGES {
        return Err(bad("agent.plan", "too many changes for one plan"));
    }
    Ok(())
}

struct CaptionChanges {
    tracks: Vec<Value>,
    captions: Vec<Value>,
    track_ids: BTreeSet<u64>,
    caption_ids: BTreeSet<u64>,
    count: usize,
}

fn caption_changes(old_seq: Option<&Sequence>, new_seq: Option<&Sequence>) -> Result<CaptionChanges> {
    let (mut tracks, mut captions, mut affected_tracks, mut affected_captions, mut changes) =
        (Vec::new(), Vec::new(), BTreeSet::new(), BTreeSet::new(), 0usize);
    let (old_ct, new_ct): (BTreeMap<_, _>, BTreeMap<_, _>) = (
        old_seq.into_iter().flat_map(|q| &q.caption_tracks).map(|t| (t.id, t)).collect(),
        new_seq.into_iter().flat_map(|q| &q.caption_tracks).map(|t| (t.id, t)).collect(),
    );
    for track in old_ct.keys().chain(new_ct.keys()).copied().collect::<BTreeSet<_>>() {
        let (a, b) = (old_ct.get(&track).copied(), new_ct.get(&track).copied());
        let metadata = |t: Option<&filmcraft_project::CaptionTrack>| -> Result<Value> {
            let mut t = t.cloned();
            if let Some(t) = &mut t {
                t.captions.clear();
            }
            serialize(&t)
        };
        let (am, bm) = (metadata(a)?, metadata(b)?);
        if am != bm {
            affected_tracks.insert(track.0);
            count_change(&mut changes)?;
            tracks.push(json!({"track":track.0,"kind":"captions","change":kind(a.is_some(),b.is_some()),"before":am,"after":bm}));
        }
        let (ac, bc): (BTreeMap<_, _>, BTreeMap<_, _>) =
            (a.into_iter().flat_map(|t| &t.captions).map(|c| (c.id.0, c)).collect(), b.into_iter().flat_map(|t| &t.captions).map(|c| (c.id.0, c)).collect());
        for caption in ac.keys().chain(bc.keys()).copied().collect::<BTreeSet<_>>() {
            let (x, y) = (ac.get(&caption), bc.get(&caption));
            if x == y {
                continue;
            }
            affected_captions.insert(caption);
            affected_tracks.insert(track.0);
            count_change(&mut changes)?;
            captions.push(json!({"caption":caption,"track":track.0,"change":kind(x.is_some(),y.is_some()),"before":serialize(&x)?,"after":serialize(&y)?}));
        }
    }
    Ok(CaptionChanges { tracks, captions, track_ids: affected_tracks, caption_ids: affected_captions, count: changes })
}

/// Complete typed project changes for review and operation receipts. Oversized reports fail closed.
pub fn project_diff(before: &Project, after: &Project) -> Result<Value> {
    let (mut items, mut sequences, mut warnings) = (Vec::new(), Vec::new(), Vec::new());
    let (mut affected_items, mut affected_sequences, mut affected_clips, mut affected_tracks, mut affected_captions, mut affected_transitions) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let mut changes = 0usize;
    warnings.extend(locked_track_warnings(before));
    for id in before.items.keys().chain(after.items.keys()).copied().collect::<BTreeSet<_>>() {
        let (old, new) = (before.item(id), after.item(id));
        if old == new {
            continue;
        }
        count_change(&mut changes)?;
        affected_items.insert(id.0);
        let (old_seq, new_seq) = (old.and_then(|i| i.as_sequence()), new.and_then(|i| i.as_sequence()));
        items.push(json!({"item":id.0,"change":kind(old.is_some(),new.is_some()),"before":item_metadata(old)?,"after":item_metadata(new)?}));
        if old_seq.is_none() && new_seq.is_none() {
            continue;
        }
        affected_sequences.insert(id.0);
        let (old_tracks, new_tracks) = (track_map(old_seq), track_map(new_seq));
        let (old_clips, new_clips) = (clip_map(&old_tracks), clip_map(&new_tracks));
        let (mut clips, mut tracks, mut captions, mut transitions) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for clip in old_clips.keys().chain(new_clips.keys()).copied().collect::<BTreeSet<_>>() {
            let (a, b) = (old_clips.get(&clip), new_clips.get(&clip));
            if a == b {
                continue;
            }
            count_change(&mut changes)?;
            affected_clips.insert(clip);
            if let Some((t, _)) = a {
                affected_tracks.insert(t.0);
            }
            if let Some((t, _)) = b {
                affected_tracks.insert(t.0);
            }
            let moved = matches!((a,b),(Some((ta,ca)),Some((tb,cb))) if ta != tb || ca.start != cb.start);
            let grade_changed = matches!((a,b),(Some((_,ca)),Some((_,cb))) if ca.effects != cb.effects);
            clips.push(json!({"clip":clip,"change":kind(a.is_some(),b.is_some()),"moved":moved,"gradeChanged":grade_changed,
                "beforeTrack":a.map(|x|x.0.0),"afterTrack":b.map(|x|x.0.0),"before":serialize(&a.map(|x|x.1))?,"after":serialize(&b.map(|x|x.1))?}));
            if let Some(link) = a.and_then(|(_, c)| c.link).or_else(|| b.and_then(|(_, c)| c.link)) {
                let partners: Vec<u64> = old_clips.values().filter(|(_, c)| c.link == Some(link) && c.id.0 != clip).map(|(_, c)| c.id.0).collect();
                warnings.push(json!({"code":"linked_media","sequence":id.0,"clip":clip,"partners":partners,"message":"Review linked video/audio and any ripple movement."}));
            }
        }
        for track in old_tracks.keys().chain(new_tracks.keys()).copied().collect::<BTreeSet<_>>() {
            let (a, b) = (old_tracks.get(&track).copied(), new_tracks.get(&track).copied());
            let (am, bm) = (a.map(track_metadata).transpose()?, b.map(track_metadata).transpose()?);
            if am != bm {
                affected_tracks.insert(track.0);
                count_change(&mut changes)?;
                tracks.push(json!({"track":track.0,"change":kind(a.is_some(),b.is_some()),"before":am,"after":bm}));
            }
            let (at, bt): (BTreeMap<_, _>, BTreeMap<_, _>) = (
                a.into_iter().flat_map(|t| &t.transitions).map(|t| (t.id.0, t)).collect(),
                b.into_iter().flat_map(|t| &t.transitions).map(|t| (t.id.0, t)).collect(),
            );
            for transition in at.keys().chain(bt.keys()).copied().collect::<BTreeSet<_>>() {
                let (x, y) = (at.get(&transition), bt.get(&transition));
                if x == y {
                    continue;
                }
                affected_transitions.insert(transition);
                affected_tracks.insert(track.0);
                count_change(&mut changes)?;
                transitions.push(
                    json!({"transition":transition,"track":track.0,"change":kind(x.is_some(),y.is_some()),"before":serialize(&x)?,"after":serialize(&y)?}),
                );
                warnings.push(json!({"code":"transition_changed","sequence":id.0,"track":track.0,"transition":transition,"message":"Review transition handles and the rendered cut."}));
            }
        }
        let caption_changes = caption_changes(old_seq, new_seq)?;
        tracks.extend(caption_changes.tracks);
        captions.extend(caption_changes.captions);
        affected_tracks.extend(caption_changes.track_ids);
        affected_captions.extend(caption_changes.caption_ids);
        for _ in 0..caption_changes.count {
            count_change(&mut changes)?;
        }
        sequences.push(json!({"sequence":id.0,"clips":clips,"tracks":tracks,"captions":captions,"transitions":transitions,
            "settingsChanged":old_seq.map(|q|&q.settings)!=new_seq.map(|q|&q.settings),"markersChanged":old_seq.map(|q|&q.markers)!=new_seq.map(|q|&q.markers)}));
    }
    let mut project_fields = Vec::new();
    macro_rules! field { ($($f:ident),*)=>{$(if before.$f != after.$f {project_fields.push(stringify!($f));})*}; }
    field!(name, custom_colors, custom_color_names, settings, root, luts, transcripts, search_bins, source_graphics, narrations, notes);
    let report = json!({"items":items,"sequences":sequences,"projectFields":project_fields,"warnings":warnings,"affected":{
        "items":affected_items,"sequences":affected_sequences,"clips":affected_clips,"tracks":affected_tracks,"captions":affected_captions,"transitions":affected_transitions}});
    bounded_json(&report, MAX_REPORT_BYTES)?;
    Ok(report)
}

fn plan(s: &mut Session, p: &Value) -> Result<Value> {
    bounded_json(p, MAX_REQUEST_BYTES)?;
    let request: Request = serde_json::from_value(p.clone()).map_err(|e| bad("agent.plan", e.to_string()))?;
    if request.steps.is_empty() || request.steps.len() > MAX_STEPS {
        return Err(bad("agent.plan", format!("give 1 to {MAX_STEPS} steps")));
    }
    if request.base_revision.is_some_and(|r| r != s.revision) {
        return Err(bad("agent.plan", "stale_revision: inspect the current project before proposing edits"));
    }
    let label = request.label.unwrap_or_else(|| "Agent edit".into());
    if label.trim().is_empty() || label.len() > 256 {
        return Err(bad("agent.plan", "label must contain 1 to 256 bytes"));
    }
    for step in &request.steps {
        if !step.params.is_null() && !step.params.is_object() {
            return Err(bad("agent.plan", "step params must be an object"));
        }
        if !eligible_command(&step.id) {
            return Err(bad(
                "agent.plan",
                format!("`{}` is not eligible for staging; file writes, jobs, downloads and document replacement are outside transactions", step.id),
            ));
        }
    }
    if s.agent_plans.plans.len() >= MAX_PLANS {
        let completed = s.agent_plans.plans.iter().find(|(_, p)| p.discarded || p.applied.is_some()).map(|(id, _)| *id);
        if let Some(id) = completed {
            s.agent_plans.plans.remove(&id);
        } else {
            return Err(bad("agent.plan", "too many staged plans; discard or apply one first"));
        }
    }
    let id = s.agent_plans.next_id.checked_add(1).ok_or_else(|| bad("agent.plan", "plan IDs exhausted; reopen the application"))?;
    let mut sandbox = Session::new(Arc::new(PlanningServices(s.services.clone())));
    sandbox.project = s.project.clone();
    sandbox.project_instance = s.project_instance.clone();
    sandbox.state = s.state.clone();
    sandbox.revision = s.revision;
    sandbox.saved_revision = s.saved_revision;
    sandbox.document_epoch = s.document_epoch;
    sandbox.media = s.media.clone();
    sandbox.prefs = s.prefs.clone();
    sandbox.prefs.media_analysis.auto_transcribe = false;
    let mut results = Vec::with_capacity(request.steps.len());
    for (index, step) in request.steps.iter().enumerate() {
        let mut params = resolve(&step.params, &results, 0)?;
        if params.is_null() {
            params = json!({});
        }
        if !params.is_object() {
            return Err(bad("agent.plan", format!("step {index} params must resolve to an object")));
        }
        if matches!(step.id.as_str(), "grade.patch" | "grade.copy" | "grade.reset" | "grade.matchBatch")
            && let Some(o) = params.as_object_mut()
        {
            o.entry("expectedRevision").or_insert(json!(sandbox.revision));
        }
        let value =
            sandbox.execute(&step.id, params).map_err(|e| bad("agent.plan", format!("step {index} `{}` failed: {e}; live project unchanged", step.id)))?;
        if !sandbox.jobs.is_empty() || !sandbox.mask_jobs.is_empty() || !sandbox.media_jobs.is_empty() || !sandbox.scene_jobs.is_empty() {
            return Err(bad("agent.plan", format!("step {index} started an unsupported background job; live project unchanged")));
        }
        results.push(value);
    }
    let diff = project_diff(&s.project, &sandbox.project)?;
    let commands: Vec<Value> = request.steps.iter().map(|step| json!({"id":step.id,"params":step.params})).collect();
    let report = json!({"plan":id,"label":label,"baseRevision":s.revision,"status":"staged","steps":results,"commands":commands,"diff":diff,"noChange":*s.project==*sandbox.project});
    bounded_json(&report, MAX_REPORT_BYTES)?;
    let entry = EditPlan {
        identity: s.project_instance.clone(),
        base_revision: s.revision,
        label,
        snapshots: Some(Snapshots { before: s.project.clone(), after: sandbox.project, before_state: s.state.clone(), after_state: sandbox.state }),
        report: report.clone(),
        applied: None,
        discarded: false,
    };
    s.agent_plans.next_id = id;
    s.agent_plans.plans.insert(id, entry);
    Ok(report)
}

fn plan_id(p: &Value, cmd: &str) -> Result<u64> {
    p.get("plan").and_then(Value::as_u64).ok_or_else(|| bad(cmd, "`plan` must be an unsigned plan ID"))
}

fn preview(s: &mut Session, p: &Value) -> Result<Value> {
    let id = plan_id(p, "agent.preview")?;
    let plan = s.agent_plans.plans.get(&id).ok_or_else(|| bad("agent.preview", "unknown plan"))?;
    let mut report = plan.report.clone();
    report["status"] = json!(if plan.discarded {
        "discarded"
    } else if plan.applied.is_some() {
        "applied"
    } else {
        "staged"
    });
    report["stale"] = json!(plan.base_revision != s.revision || !Arc::ptr_eq(&plan.identity, &s.project_instance));
    report["currentRevision"] = json!(s.revision);
    Ok(report)
}

fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    let id = plan_id(p, "agent.apply")?;
    let plan = s.agent_plans.plans.get(&id).ok_or_else(|| bad("agent.apply", "unknown plan"))?;
    if let Some(receipt) = &plan.applied {
        return Ok(receipt.clone());
    }
    if plan.discarded {
        return Err(bad("agent.apply", "plan was discarded"));
    }
    if plan.base_revision != s.revision || !Arc::ptr_eq(&plan.identity, &s.project_instance) {
        return Err(bad("agent.apply", "stale_plan: the project changed; inspect and build a new plan"));
    }
    let snapshots = plan.snapshots.as_ref().ok_or_else(|| bad("agent.apply", "plan snapshots are unavailable"))?;
    if *snapshots.before != *s.project {
        return Err(bad("agent.apply", "stale_plan: the project snapshot changed; build a new plan"));
    }
    let (after, label, before_revision, affected) = (snapshots.after.clone(), plan.label.clone(), s.revision, plan.report["diff"]["affected"].clone());
    let (before_state, after_state) = (snapshots.before_state.clone(), snapshots.after_state.clone());
    let changed = *s.project != *after;
    if changed {
        // One edit, rather than replaying commands: approved pixels/IDs are exactly what lands.
        // Preserve live navigation when the user changed it during review. Staged seeks only
        // drive the proposal; a newly created/opened sequence can become visible on approval.
        s.edit(&label, move |p, state| {
            *p = (*after).clone();
            if state.active_sequence == before_state.active_sequence && before_state.active_sequence != after_state.active_sequence {
                state.active_sequence = after_state.active_sequence;
                if let Some(id) = after_state.active_sequence
                    && !state.open_sequences.contains(&id)
                {
                    state.open_sequences.push(id);
                }
            }
            if state.selection == before_state.selection {
                state.selection = after_state.selection;
            }
            if state.caption_selection == before_state.caption_selection {
                state.caption_selection = after_state.caption_selection;
            }
            Ok(())
        })?;
        s.state.selection.retain(|c| s.state.active_sequence.and_then(|q| s.project.sequence(q)).is_some_and(|q| q.find_item(*c).is_some()));
    }
    let receipt = json!({"plan":id,"status":"applied","beforeRevision":before_revision,"afterRevision":s.revision,"affected":affected,"changed":changed,"undoSteps":u8::from(changed)});
    if let Some(plan) = s.agent_plans.plans.get_mut(&id) {
        plan.applied = Some(receipt.clone());
    }
    Ok(receipt)
}

fn discard(s: &mut Session, p: &Value) -> Result<Value> {
    let id = plan_id(p, "agent.discard")?;
    let plan = s.agent_plans.plans.get_mut(&id).ok_or_else(|| bad("agent.discard", "unknown plan"))?;
    if plan.applied.is_some() {
        return Err(bad("agent.discard", "applied plan cannot be discarded; use undo"));
    }
    plan.discarded = true;
    plan.snapshots = None;
    Ok(json!({"plan":id,"status":"discarded"}))
}

impl Session {
    /// Frozen before/after snapshots for exact staged renders, even if the live plan is now stale.
    pub fn agent_plan_snapshot(&self, plan_id: u64, before: bool) -> Result<(Arc<Project>, EditorState, u64)> {
        let plan = self.agent_plans.plans.get(&plan_id).ok_or_else(|| bad("agent.preview", "unknown plan"))?;
        if !Arc::ptr_eq(&plan.identity, &self.project_instance) {
            return Err(bad("agent.preview", "plan belongs to another project document"));
        }
        let snapshots = plan.snapshots.as_ref().ok_or_else(|| bad("agent.preview", "plan was discarded"))?;
        Ok(if before {
            (snapshots.before.clone(), snapshots.before_state.clone(), plan.base_revision)
        } else {
            (snapshots.after.clone(), snapshots.after_state.clone(), plan.base_revision)
        })
    }
}

pub fn commands() -> Vec<CommandSpec> {
    let spec = |id, label, params, run, journal| CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal };
    vec![
        spec("agent.plan", "Propose Edit Plan", r#"{"label":str?,"baseRevision":u64?,"steps":[{"id":str,"params":object?}]}"#, plan, false),
        spec("agent.preview", "Preview Edit Plan", r#"{"plan":id}"#, preview, false),
        spec("agent.apply", "Apply Edit Plan", r#"{"plan":id}"#, apply, true),
        spec("agent.discard", "Discard Edit Plan", r#"{"plan":id}"#, discard, false),
        spec(
            "agent.plans",
            "List Edit Plans",
            "{}",
            |s, _| {
                Ok(
                    json!({"revision":s.revision,"plans":s.agent_plans.plans.iter().map(|(id,p)|json!({"plan":id,"label":p.label,"baseRevision":p.base_revision,"status":if p.discarded{"discarded"}else if p.applied.is_some(){"applied"}else{"staged"},"stale":p.base_revision!=s.revision || !Arc::ptr_eq(&p.identity,&s.project_instance)})).collect::<Vec<_>>() }),
                )
            },
            false,
        ),
    ]
}
