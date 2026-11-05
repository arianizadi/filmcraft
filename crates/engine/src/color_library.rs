//! Persistent still gallery, named grade versions, and shared scene/camera looks.

use std::sync::Arc;

use base64::Engine as _;
use filmcraft_project::color_workflow::{FrozenImage, GalleryStill, GradeVersion, SharedLook, is_color_effect, validate_effects};
use filmcraft_project::{ClipId, EffectInstance, ItemId, TrackItem, TrackKind};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};

pub(crate) struct ClipTarget {
    pub sequence: ItemId,
    pub clip: ClipId,
    pub item: TrackItem,
    pub pipeline: filmcraft_color::ColorPipeline,
}

pub(crate) fn flag(p: &Value, key: &str, default: bool, cmd: &str) -> Result<bool> {
    p.get(key).map(|v| v.as_bool().ok_or_else(|| bad(cmd, format!("{key} must be boolean")))).transpose().map(|v| v.unwrap_or(default))
}

pub(crate) fn guard(s: &Session, p: &Value, cmd: &str) -> Result<()> {
    crate::agent_plans::bounded_json(p, 4 * 1024 * 1024)?;
    let revision = p.get("expectedRevision").and_then(Value::as_u64).ok_or_else(|| bad(cmd, "need expectedRevision from inspection"))?;
    if revision != s.revision {
        return Err(bad(cmd, format!("stale_revision: expected {revision}, current {}", s.revision)));
    }
    Ok(())
}

pub(crate) fn target(s: &Session, p: &Value, write: bool, cmd: &str) -> Result<ClipTarget> {
    let clip = ClipId(p.get("clip").and_then(Value::as_u64).ok_or_else(|| bad(cmd, "need an explicit clip ID"))?);
    let sequence = match p.get("sequence") {
        None => s.state.active_sequence.ok_or(crate::EngineError::NoSequence)?,
        Some(v) => ItemId(v.as_u64().ok_or_else(|| bad(cmd, "sequence must be an unsigned ID"))?),
    };
    let q = s.project.sequence(sequence).ok_or(crate::EngineError::NoSequence)?;
    q.settings.validate().map_err(|e| bad(cmd, e))?;
    let (track, item) = q.find_item(clip).ok_or_else(|| bad(cmd, "clip is absent from the requested sequence"))?;
    let track = q.track(track).ok_or_else(|| bad(cmd, "missing clip track"))?;
    if track.kind != TrackKind::Video || write && track.locked {
        return Err(bad(cmd, "target must be a video clip on an unlocked track"));
    }
    Ok(ClipTarget { sequence, clip, item: item.clone(), pipeline: q.settings.color })
}

pub(crate) fn sample_time(s: &Session, t: &ClipTarget, p: &Value) -> Result<Tick> {
    let mut request = p.clone();
    request["target"] = json!("clip");
    request["sequence"] = json!(t.sequence.0);
    request["clip"] = json!(t.clip.0);
    let subject = crate::inspection::subject(s, &request, false)?;
    let times = subject.times(&request)?;
    if times.len() != 1 {
        return Err(bad("color", "exactly one frame is required"));
    }
    times.first().copied().ok_or_else(|| bad("color", "no frame"))
}

pub(crate) fn color_stack(s: &Session, target: &ClipTarget, shared: bool) -> Vec<EffectInstance> {
    let shared = if shared { s.project.color_workflow.shared_effects(&target.item) } else { &[] };
    shared
        .iter()
        .chain(target.item.effects.iter())
        .filter(|e| is_color_effect(&e.effect))
        .cloned()
        .map(|mut effect| {
            effect.instance_id = 0;
            effect
        })
        .collect()
}

fn name(p: &Value, cmd: &str) -> Result<String> {
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty() && n.len() <= 256 && !n.chars().any(char::is_control))
        .ok_or_else(|| bad(cmd, "name must contain 1..256 bytes without control characters"))?;
    Ok(name.to_string())
}

fn next_id(project: &mut filmcraft_project::Project, cmd: &str) -> Result<u64> {
    if project.next_id >= filmcraft_project::Project::MAX_NEXT_ID {
        return Err(bad(cmd, "project identity space exhausted"));
    }
    Ok(project.alloc_id())
}

pub(crate) fn gallery_id(s: &Session, p: &Value, key: &str, cmd: &str) -> Result<u64> {
    let value = p.get(key).ok_or_else(|| bad(cmd, format!("need {key} id or name")))?;
    if let Some(id) = value.as_u64()
        && s.project.color_workflow.gallery.contains_key(&id)
    {
        return Ok(id);
    }
    if let Some(name) = value.as_str()
        && let Some(still) = s.project.color_workflow.gallery.values().find(|v| v.name.eq_ignore_ascii_case(name))
    {
        return Ok(still.id);
    }
    Err(bad(cmd, "unknown gallery still"))
}

fn look_id(s: &Session, p: &Value, cmd: &str) -> Result<u64> {
    let value = p.get("look").ok_or_else(|| bad(cmd, "need look id or name"))?;
    if let Some(id) = value.as_u64()
        && s.project.color_workflow.looks.contains_key(&id)
    {
        return Ok(id);
    }
    if let Some(name) = value.as_str()
        && let Some(look) = s.project.color_workflow.looks.values().find(|v| v.name.eq_ignore_ascii_case(name))
    {
        return Ok(look.id);
    }
    Err(bad(cmd, "unknown shared look"))
}

pub(crate) fn replace_color_stack(project: &mut filmcraft_project::Project, target: &ClipTarget, effects: &[EffectInstance], unlink: bool) -> Result<()> {
    let (_, clip) = project.sequence_mut(target.sequence).and_then(|q| q.find_item_mut(target.clip)).ok_or_else(|| bad("grade", "clip disappeared"))?;
    // Replace corresponding color slots in place, retaining the order/values of non-color effects.
    let mut pending = effects.iter();
    let mut replaced = Vec::new();
    let mut last_slot = None;
    for effect in &clip.effects {
        if is_color_effect(&effect.effect) {
            if let Some(next) = pending.next() {
                let mut copied = next.clone();
                copied.instance_id = if copied.effect == effect.effect { effect.instance_id } else { 0 };
                replaced.push(copied);
                last_slot = Some(replaced.len());
            }
        } else {
            replaced.push(effect.clone());
        }
    }
    let extras: Vec<EffectInstance> = pending
        .cloned()
        .map(|mut effect| {
            effect.instance_id = 0;
            effect
        })
        .collect();
    let insert = last_slot.unwrap_or(replaced.len());
    replaced.splice(insert..insert, extras);
    clip.effects = replaced;
    if unlink {
        clip.shared_look = None;
    }
    Ok(())
}

fn animation_consent(target: &ClipTarget, p: &Value, cmd: &str) -> Result<()> {
    if target.item.effects.iter().filter(|e| is_color_effect(&e.effect)).any(EffectInstance::is_animated) && !flag(p, "replaceKeyframes", false, cmd)? {
        return Err(bad(cmd, "existing color animation requires replaceKeyframes:true before replacement"));
    }
    Ok(())
}

fn pipeline_consent(target: &ClipTarget, pipeline: filmcraft_color::ColorPipeline, p: &Value, cmd: &str) -> Result<()> {
    if pipeline != target.pipeline && !flag(p, "allowColorSpaceChange", false, cmd)? {
        return Err(bad(cmd, "authoring and target color pipelines differ; inspect previews and choose allowColorSpaceChange:true explicitly"));
    }
    Ok(())
}

fn gallery_save(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "gallery.save";
    guard(s, p, CMD)?;
    let name = name(p, CMD)?;
    if s.project.color_workflow.gallery.values().any(|v| v.name.eq_ignore_ascii_case(&name)) {
        return Err(bad(CMD, "gallery name is already in use"));
    }
    let subject = crate::inspection::subject(s, p, false)?;
    let times = subject.times(p)?;
    if times.len() != 1 {
        return Err(bad(CMD, "a still captures exactly one frame"));
    }
    let time = times.first().copied().ok_or_else(|| bad(CMD, "no frame"))?;
    let scale = subject.scale(p, 1)?;
    let (display, offline) = subject.render(s, time, scale, false)?;
    if offline.as_array().is_some_and(|v| !v.is_empty()) {
        return Err(bad(CMD, "relink offline media before saving a reference still"));
    }
    let max = subject.settings.width.max(subject.settings.height).max(1);
    let (reference, offline) = subject.render(s, time, (128.0 / max as f32).min(scale), true)?;
    if offline.as_array().is_some_and(|v| !v.is_empty()) {
        return Err(bad(CMD, "reference source is offline"));
    }
    let reference = FrozenImage { width: reference.w, height: reference.h, pixels: reference.px.into() };
    reference.validate().map_err(|e| bad(CMD, e))?;
    let png = filmcraft_export::encode_png(display.over_black_rgba8(), display.w as u32, display.h as u32).map_err(|e| bad(CMD, e.to_string()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let grade = if let Some(clip) = subject.clip {
        let t = target(s, &json!({"clip":clip.0,"sequence":subject.sequence.map(|v|v.0)}), false, CMD)?;
        color_stack(s, &t, true)
    } else {
        Vec::new()
    };
    validate_effects(&grade).map_err(|e| bad(CMD, e))?;
    let metadata = subject.metadata(time, false);
    let transform = serde_json::to_string(&metadata["colorTransform"]).map_err(|e| bad(CMD, e.to_string()))?;
    let mut still = GalleryStill {
        id: 0,
        name,
        sequence: subject.sequence,
        clip: subject.clip,
        item: subject.item,
        time,
        frame: subject.rate.frame_at(time),
        rate: subject.rate,
        revision: s.revision,
        pipeline: subject.settings.color,
        width: display.w as u32,
        height: display.h as u32,
        png_base64: encoded.into(),
        reference,
        grade: grade.into(),
        transform,
    };
    let id = s.edit("Save Reference Still", |project, _| {
        still.id = next_id(project, CMD)?;
        project.color_workflow.gallery.insert(still.id, still.clone());
        project.color_workflow.validate().map_err(|e| bad(CMD, e))?;
        Ok(still.id)
    })?;
    Ok(
        json!({"still":id,"name":still.name,"revision":s.revision,"capturedRevision":still.revision,"width":still.width,"height":still.height,"metadata":metadata}),
    )
}

fn gallery_list(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(
        json!({"revision":s.revision,"stills":s.project.color_workflow.gallery.values().map(|v|json!({"still":v.id,"name":v.name,"sequence":v.sequence,"clip":v.clip,"item":v.item,"frame":v.frame,"time":v.time,"capturedRevision":v.revision,"width":v.width,"height":v.height,"pipeline":v.pipeline,"gradeEffects":v.grade.len()})).collect::<Vec<_>>()}),
    )
}

fn gallery_get(s: &mut Session, p: &Value) -> Result<Value> {
    let id = gallery_id(s, p, "still", "gallery.get")?;
    let still = s.project.color_workflow.gallery.get(&id).ok_or_else(|| bad("gallery.get", "still missing"))?;
    Ok(
        json!({"still":id,"name":still.name,"revision":s.revision,"capturedRevision":still.revision,"sequence":still.sequence,"clip":still.clip,"item":still.item,"time":still.time,"frame":still.frame,"rate":still.rate,"pipeline":still.pipeline,"width":still.width,"height":still.height,"pngBase64":still.png_base64,"grade":still.grade,"transform":still.transform,"referenceSample":{"width":still.reference.width,"height":still.reference.height}}),
    )
}

fn gallery_delete(s: &mut Session, p: &Value) -> Result<Value> {
    guard(s, p, "gallery.delete")?;
    let id = gallery_id(s, p, "still", "gallery.delete")?;
    s.edit("Delete Reference Still", |project, _| {
        project.color_workflow.gallery.remove(&id);
        Ok(())
    })?;
    Ok(json!({"deleted":id,"revision":s.revision}))
}

fn gallery_apply(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "gallery.applyGrade";
    guard(s, p, CMD)?;
    let id = gallery_id(s, p, "still", CMD)?;
    let t = target(s, p, true, CMD)?;
    animation_consent(&t, p, CMD)?;
    let still = s.project.color_workflow.gallery.get(&id).cloned().ok_or_else(|| bad(CMD, "still missing"))?;
    if still.clip.is_none() {
        return Err(bad(CMD, "source/composite stills are visual references; save an isolated timeline clip to capture an applicable grade"));
    }
    pipeline_consent(&t, still.pipeline, p, CMD)?;
    s.edit("Apply Reference Grade", |project, _| replace_color_stack(project, &t, &still.grade, true))?;
    Ok(json!({"still":id,"clip":t.clip.0,"revision":s.revision,"effects":still.grade,"sharedLookDetached":true}))
}

fn version_save(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.version.save";
    guard(s, p, CMD)?;
    let t = target(s, p, false, CMD)?;
    let name = name(p, CMD)?;
    let versions = s.project.color_workflow.versions.get(&t.clip);
    if versions.is_some_and(|v| v.len() >= 64 || v.iter().any(|v| v.name.eq_ignore_ascii_case(&name))) {
        return Err(bad(CMD, "version name exists or 64-version clip limit reached"));
    }
    let effects = color_stack(s, &t, true);
    validate_effects(&effects).map_err(|e| bad(CMD, e))?;
    let full = s.project.color_workflow.shared_effects(&t.item).iter().chain(t.item.effects.iter());
    let color_positions = full.clone().enumerate().filter(|(_, e)| is_color_effect(&e.effect)).map(|(index, _)| index).collect();
    let non_color_effects = full.filter(|e| !is_color_effect(&e.effect)).map(|e| e.effect.clone()).collect();
    let mut version = GradeVersion {
        id: 0,
        name,
        effects: effects.into(),
        revision: s.revision,
        pipeline: t.pipeline,
        shared_look: t.item.shared_look,
        color_positions,
        non_color_effects,
    };
    let id = s.edit("Save Grade Version", |project, _| {
        version.id = next_id(project, CMD)?;
        project.color_workflow.versions.entry(t.clip).or_default().push(version.clone());
        project.color_workflow.validate().map_err(|e| bad(CMD, e))?;
        Ok(version.id)
    })?;
    Ok(json!({"version":id,"clip":t.clip.0,"name":version.name,"revision":s.revision,"savedRevision":version.revision}))
}

fn version_list(s: &mut Session, p: &Value) -> Result<Value> {
    let t = target(s, p, false, "grade.version.list")?;
    Ok(json!({"clip":t.clip.0,"revision":s.revision,"versions":s.project.color_workflow.versions.get(&t.clip)}))
}

fn version_find(s: &Session, p: &Value, clip: ClipId, cmd: &str) -> Result<GradeVersion> {
    let v = p.get("version").ok_or_else(|| bad(cmd, "need version id or name"))?;
    s.project
        .color_workflow
        .versions
        .get(&clip)
        .into_iter()
        .flatten()
        .find(|r| v.as_u64() == Some(r.id) || v.as_str().is_some_and(|v| r.name.eq_ignore_ascii_case(v)))
        .cloned()
        .ok_or_else(|| bad(cmd, "unknown grade version for this clip"))
}

fn version_apply(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.version.apply";
    guard(s, p, CMD)?;
    let t = target(s, p, true, CMD)?;
    let version = version_find(s, p, t.clip, CMD)?;
    animation_consent(&t, p, CMD)?;
    pipeline_consent(&t, version.pipeline, p, CMD)?;
    let signature: Vec<String> = t.item.effects.iter().filter(|e| !is_color_effect(&e.effect)).map(|e| e.effect.clone()).collect();
    let same_structure = signature == version.non_color_effects;
    if !same_structure && !flag(p, "allowStructureChange", false, CMD)? {
        return Err(bad(CMD, "non-color effect structure changed; choose allowStructureChange:true to merge color controls into the current stack"));
    }
    s.edit("Switch Grade Version", |project, _| {
        if !same_structure {
            return replace_color_stack(project, &t, &version.effects, true);
        }
        let (_, clip) = project.sequence_mut(t.sequence).and_then(|q| q.find_item_mut(t.clip)).ok_or_else(|| bad(CMD, "clip disappeared"))?;
        let mut others = clip.effects.iter().filter(|e| !is_color_effect(&e.effect));
        let prior_colors: Vec<_> = clip.effects.iter().filter(|e| is_color_effect(&e.effect)).map(|e| (e.effect.clone(), e.instance_id)).collect();
        let mut color_index = 0usize;
        let mut colors = version.effects.iter();
        let count = version.effects.len().saturating_add(version.non_color_effects.len());
        let mut restored = Vec::with_capacity(count);
        for i in 0..count {
            let effect =
                if version.color_positions.contains(&i) { colors.next() } else { others.next() }.ok_or_else(|| bad(CMD, "invalid saved color positions"))?;
            let mut copied = effect.clone();
            if is_color_effect(&copied.effect) {
                copied.instance_id = prior_colors.get(color_index).filter(|(name, _)| name == &copied.effect).map_or(0, |(_, id)| *id);
                color_index = color_index.saturating_add(1);
            }
            restored.push(copied);
        }
        clip.effects = restored;
        clip.shared_look = None;
        Ok(())
    })?;
    Ok(json!({"version":version.id,"clip":t.clip.0,"revision":s.revision,"effects":version.effects,"sharedLookDetached":true}))
}

fn version_delete(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.version.delete";
    guard(s, p, CMD)?;
    let t = target(s, p, false, CMD)?;
    let version = version_find(s, p, t.clip, CMD)?;
    s.edit("Delete Grade Version", |project, _| {
        if let Some(v) = project.color_workflow.versions.get_mut(&t.clip) {
            v.retain(|v| v.id != version.id);
        }
        Ok(())
    })?;
    Ok(json!({"deleted":version.id,"revision":s.revision}))
}

fn prepare_look(s: &Session, p: &Value, cmd: &str) -> Result<(ClipTarget, Arc<[EffectInstance]>)> {
    let target = target(s, p, false, cmd)?;
    let mut effects = color_stack(s, &target, false);
    if effects.is_empty() {
        return Err(bad(cmd, "the source clip has no color effects"));
    }
    let freeze = flag(p, "freezeAnimation", false, cmd)?;
    if effects.iter().any(EffectInstance::is_animated) && !freeze {
        return Err(bad(cmd, "shared looks are static; choose freezeAnimation:true and an exact frame, or keep animation in per-shot trims"));
    }
    if effects.iter().any(|e| !e.masks.is_empty()) {
        return Err(bad(cmd, "keep geometric masks in per-shot trims; a shared camera/scene look is unmasked"));
    }
    if freeze {
        let time = sample_time(s, &target, p)?;
        let mt = target.item.effect_time_at(time);
        for effect in &mut effects {
            for param in effect.params.values_mut() {
                param.value = param.value_at(mt);
                param.keyframes.clear();
            }
        }
    }
    validate_effects(&effects).map_err(|e| bad(cmd, e))?;
    Ok((target, effects.into()))
}

fn look_save(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.look.save";
    guard(s, p, CMD)?;
    let name = name(p, CMD)?;
    let (t, effects) = prepare_look(s, p, CMD)?;
    if s.project.color_workflow.looks.values().any(|l| l.name.eq_ignore_ascii_case(&name)) {
        return Err(bad(CMD, "look name is already used"));
    }
    let group = p
        .get("group")
        .map(|v| v.as_str().filter(|v| v.len() <= 256).ok_or_else(|| bad(CMD, "group must be at most 256 bytes")))
        .transpose()?
        .unwrap_or("")
        .to_string();
    let mut look = SharedLook { id: 0, name, group, effects, pipeline: t.pipeline, revision: s.revision };
    let id = s.edit("Save Shared Look", |project, _| {
        look.id = next_id(project, CMD)?;
        project.color_workflow.looks.insert(look.id, look.clone());
        project.color_workflow.validate().map_err(|e| bad(CMD, e))?;
        Ok(look.id)
    })?;
    Ok(json!({"look":id,"name":look.name,"group":look.group,"revision":s.revision,"effects":look.effects}))
}

fn look_update(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.look.update";
    guard(s, p, CMD)?;
    let id = look_id(s, p, CMD)?;
    let (t, effects) = prepare_look(s, p, CMD)?;
    let before = s.project.color_workflow.looks.get(&id).ok_or_else(|| bad(CMD, "look missing"))?;
    if before.pipeline != t.pipeline && !flag(p, "allowColorSpaceChange", false, CMD)? {
        return Err(bad(CMD, "shared look pipeline changes require allowColorSpaceChange:true"));
    }
    let linked = filmcraft_project::color_workflow::ColorWorkflow::linked_clips(&s.project, id);
    let affected: Vec<u64> = linked.iter().map(|(_, clip)| clip.0).collect();
    for (sequence, clip) in linked {
        if let Some(q) = s.project.sequence(sequence)
            && let Some((track, _)) = q.find_item(clip)
            && q.track(track).is_some_and(|t| t.locked)
        {
            return Err(bad(CMD, "a linked clip is on a locked track; unlock it before updating its shared look"));
        }
    }
    s.edit("Update Shared Look", |project, _| {
        let look = project.color_workflow.looks.get_mut(&id).ok_or_else(|| bad(CMD, "look missing"))?;
        look.effects = effects.clone();
        look.pipeline = t.pipeline;
        look.revision = look.revision.saturating_add(1);
        Ok(())
    })?;
    Ok(json!({"look":id,"revision":s.revision,"affectedClips":affected,"effects":effects}))
}

fn look_list(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(
        json!({"revision":s.revision,"looks":s.project.color_workflow.looks.values().map(|look|json!({"look":look.id,"name":look.name,"group":look.group,"pipeline":look.pipeline,"revision":look.revision,"effects":look.effects,"clips":filmcraft_project::color_workflow::ColorWorkflow::linked_clips(&s.project,look.id).iter().map(|(_,clip)|clip.0).collect::<Vec<_>>()})).collect::<Vec<_>>(),"ordering":"shared grade before local color trims"}),
    )
}

fn look_link(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.look.link";
    guard(s, p, CMD)?;
    let id = look_id(s, p, CMD)?;
    let t = target(s, p, true, CMD)?;
    let look = s.project.color_workflow.looks.get(&id).ok_or_else(|| bad(CMD, "look missing"))?;
    pipeline_consent(&t, look.pipeline, p, CMD)?;
    if t.item.shared_look == Some(id) {
        return Ok(
            json!({"look":id,"clip":t.clip.0,"revision":s.revision,"unchanged":true,"trimEffects":t.item.effects.iter().filter(|e|is_color_effect(&e.effect)).count(),"ordering":"shared grade before local color trims"}),
        );
    }
    if t.item.shared_look.is_some() && !flag(p, "replaceLink", false, CMD)? {
        return Err(bad(CMD, "clip already has a shared look; choose replaceLink:true explicitly"));
    }
    s.edit("Link Shared Look", |project, _| {
        let (_, clip) = project.sequence_mut(t.sequence).and_then(|q| q.find_item_mut(t.clip)).ok_or_else(|| bad(CMD, "clip disappeared"))?;
        clip.shared_look = Some(id);
        project.color_workflow.validate().map_err(|e| bad(CMD, e))?;
        Ok(())
    })?;
    Ok(
        json!({"look":id,"clip":t.clip.0,"revision":s.revision,"trimEffects":t.item.effects.iter().filter(|e|is_color_effect(&e.effect)).count(),"ordering":"shared grade before local color trims"}),
    )
}

fn look_unlink(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.look.unlink";
    guard(s, p, CMD)?;
    let t = target(s, p, true, CMD)?;
    let binding = t.item.shared_look.ok_or_else(|| bad(CMD, "clip has no shared look"))?;
    let bake = flag(p, "keepAppearance", true, CMD)?;
    let effects = s.project.color_workflow.shared_effects(&t.item).to_vec();
    s.edit("Unlink Shared Look", |project, _| {
        let (_, clip) = project.sequence_mut(t.sequence).and_then(|q| q.find_item_mut(t.clip)).ok_or_else(|| bad(CMD, "clip disappeared"))?;
        if bake {
            clip.effects.splice(0..0, effects.clone());
        }
        clip.shared_look = None;
        Ok(())
    })?;
    Ok(json!({"look":binding,"clip":t.clip.0,"revision":s.revision,"keptAppearance":bake}))
}

fn look_delete(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.look.delete";
    guard(s, p, CMD)?;
    let id = look_id(s, p, CMD)?;
    if !filmcraft_project::color_workflow::ColorWorkflow::linked_clips(&s.project, id).is_empty() {
        return Err(bad(CMD, "unlink the look's clips before deleting it; keepAppearance preserves their grade"));
    }
    s.edit("Delete Shared Look", |project, _| {
        project.color_workflow.looks.remove(&id);
        Ok(())
    })?;
    Ok(json!({"deleted":id,"revision":s.revision}))
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    type Run = fn(&mut Session, &Value) -> Result<Value>;
    [
        ("gallery.save","Save Reference Still",r#"{"name":str,"expectedRevision":n,"target":"clip|source|sequence"?,"clip":id?,"sequence":id?,"item":id?,"frame":n?,"maxSide":n=960}"#,true,gallery_save as Run),
        ("gallery.list","List Reference Stills","{}",false,gallery_list),
        ("gallery.get","Get Reference Still",r#"{"still":id|name}"#,false,gallery_get),
        ("gallery.delete","Delete Reference Still",r#"{"still":id|name,"expectedRevision":n}"#,true,gallery_delete),
        ("gallery.applyGrade","Apply Reference Grade",r#"{"still":id|name,"clip":id,"sequence":id?,"expectedRevision":n,"replaceKeyframes":bool=false,"allowColorSpaceChange":bool=false}"#,true,gallery_apply),
        ("grade.version.save","Save Grade Version",r#"{"name":str,"clip":id,"sequence":id?,"expectedRevision":n}"#,true,version_save),
        ("grade.version.list","List Grade Versions",r#"{"clip":id,"sequence":id?}"#,false,version_list),
        ("grade.version.apply","Switch Grade Version",r#"{"version":id|name,"clip":id,"sequence":id?,"expectedRevision":n,"replaceKeyframes":bool=false,"allowColorSpaceChange":bool=false,"allowStructureChange":bool=false}"#,true,version_apply),
        ("grade.version.delete","Delete Grade Version",r#"{"version":id|name,"clip":id,"sequence":id?,"expectedRevision":n}"#,true,version_delete),
        ("grade.look.save","Save Shared Look",r#"{"name":str,"group":str?,"clip":id,"sequence":id?,"expectedRevision":n,"freezeAnimation":bool=false,"frame":n?}"#,true,look_save),
        ("grade.look.update","Update Shared Look",r#"{"look":id|name,"clip":id,"sequence":id?,"expectedRevision":n,"freezeAnimation":bool=false,"frame":n?,"allowColorSpaceChange":bool=false}"#,true,look_update),
        ("grade.look.list","List Shared Looks","{}",false,look_list),
        ("grade.look.link","Link Shared Look",r#"{"look":id|name,"clip":id,"sequence":id?,"expectedRevision":n,"replaceLink":bool=false,"allowColorSpaceChange":bool=false}"#,true,look_link),
        ("grade.look.unlink","Unlink Shared Look",r#"{"clip":id,"sequence":id?,"expectedRevision":n,"keepAppearance":bool=true}"#,true,look_unlink),
        ("grade.look.delete","Delete Shared Look",r#"{"look":id|name,"expectedRevision":n}"#,true,look_delete),
    ].into_iter().map(|(id,label,params,journal,run)|CommandSpec{id,label,menu:&[],shortcut:None,params,enabled:always,journal,run}).collect()
}
