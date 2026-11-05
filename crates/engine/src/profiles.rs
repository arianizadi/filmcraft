//! Versioned reusable project preferences and ordinary editable layout proposals.
use crate::{
    Result, Session,
    commands::{CommandSpec, always, bad},
};
use filmcraft_project::{
    ItemId,
    review::{EditingProfile, MAX_PROFILES},
};
use serde_json::{Value, json};
fn value<T: serde::Serialize>(v: T) -> Result<Value> {
    serde_json::to_value(v).map_err(|e| bad("profile", e.to_string()))
}
fn resolve_profile(s: &Session, p: &Value) -> Result<EditingProfile> {
    crate::agent_plans::bounded_json(p, 65_536)?;
    let name = p["name"].as_str().or(s.project.review.active_profile.as_deref()).ok_or_else(|| bad("profile.resolve", "profile name is required"))?;
    let versions = s.project.review.profiles.get(name).ok_or_else(|| bad("profile.resolve", "profile does not exist"))?;
    let profile = if p.get("name").is_none() && p.get("version").is_none() {
        s.project.review.active_profile_settings.as_ref().or_else(|| versions.last())
    } else if let Some(version) = p["version"].as_u64() {
        versions.iter().find(|v| u64::from(v.version) == version)
    } else {
        versions.last()
    }
    .ok_or_else(|| bad("profile.resolve", "profile version does not exist"))?;
    let mut resolved = value(profile)?;
    if let Some(overrides) = p.get("overrides") {
        let overrides = overrides.as_object().ok_or_else(|| bad("profile.resolve", "overrides must be an object"))?;
        if overrides.contains_key("version") || overrides.contains_key("name") {
            return Err(bad("profile.resolve", "one-off overrides cannot change identity or version"));
        }
        let object = resolved.as_object_mut().ok_or_else(|| bad("profile.resolve", "profile is damaged"))?;
        for (key, value) in overrides {
            object.insert(key.clone(), value.clone());
        }
    }
    let profile: EditingProfile = serde_json::from_value(resolved).map_err(|e| bad("profile.resolve", e.to_string()))?;
    profile.check().map_err(|e| bad("profile.resolve", e))?;
    Ok(profile)
}
fn save(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 65_536)?;
    let mut profile: EditingProfile = serde_json::from_value(p["profile"].clone()).map_err(|e| bad("profile.save", e.to_string()))?;
    profile.check().map_err(|e| bad("profile.save", e))?;
    let versions = s.project.review.profiles.get(&profile.name);
    if versions.is_none() && s.project.review.profiles.len() >= MAX_PROFILES {
        return Err(bad("profile.save", "project profile limit reached"));
    }
    let len = versions.map_or(0, Vec::len);
    if len >= 32 {
        return Err(bad("profile.save", "profile version limit reached"));
    }
    profile.version = len as u32 + 1;
    let result = value(&profile)?;
    s.edit("Save Editing Profile", move |pr, _| {
        pr.review.profiles.entry(profile.name.clone()).or_default().push(profile);
        Ok(())
    })?;
    Ok(result)
}
fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    let profile = resolve_profile(s, p)?;
    let name = profile.name.clone();
    let style = profile.caption_style.clone();
    let resolved_profile = profile.clone();
    s.edit("Apply Editing Profile", move |pr, st| {
        pr.review.active_profile = Some(name);
        pr.review.active_profile_settings = Some(resolved_profile);
        if let Some(q) = st.active_sequence.and_then(|id| pr.sequence_mut(id)) {
            for track in &mut q.caption_tracks {
                if !track.locked {
                    track.style = style.clone()
                }
            }
        }
        Ok(())
    })?;
    Ok(
        json!({"profile":profile,"applied":["activeProfile","unlockedCaptionTrackStyle"],"preferences":["referenceLook","skinTonePreference","pacing","pausePaddingTicks","loudnessLufs","delivery"]}),
    )
}
fn plan(s: &mut Session, p: &Value) -> Result<Value> {
    let profile = resolve_profile(s, p)?;
    let mut steps = vec![json!({"id":"profile.apply","params":p})];
    if p["delivery"].as_bool().unwrap_or(true) {
        steps.push(json!({"id":"sequence.settings","params":{"width":profile.delivery.width,"height":profile.delivery.height,"frameRate":{"num":profile.delivery.fps_num,"den":profile.delivery.fps_den},"workingSpace":profile.delivery.color_space,"pixelAspect":[1,1]}}))
    }
    let warnings: Vec<String> = Vec::new();
    if let Some(q) = s.active_sequence() {
        if p["normalizeAudio"].as_bool().unwrap_or(true) {
            let audio = q.audio_tracks.iter().filter(|t| !t.locked).flat_map(|t| &t.items).collect::<Vec<_>>();
            let untyped = audio.iter().filter(|clip| clip.essential.is_none()).map(|clip| clip.id.0).collect::<Vec<_>>();
            for clips in untyped.chunks(192) {
                steps.push(json!({"id":"essentialSound.setType","params":{"clips":clips,"type":"dialogue"}}));
            }
            let ids = audio.iter().map(|clip| clip.id.0).collect::<Vec<_>>();
            for clips in ids.chunks(192) {
                steps.push(json!({"id":"workflow.normalizeAudio","params":{"clips":clips,"target":profile.loudness_lufs}}));
            }
        }
        if let Some(look) = &profile.reference_look {
            for clip in q.video_tracks.iter().filter(|t| !t.locked).flat_map(|t| &t.items) {
                steps.push(json!({"id":"grade.look.link","params":{"look":look,"clip":clip.id.0}}));
            }
        }
    }
    let mut report =
        s.execute("agent.plan", json!({"label":format!("Apply profile {} v{}",profile.name,profile.version),"baseRevision":s.revision,"steps":steps}))?;
    if let Some(id) = report["plan"].as_u64() {
        report = s.agent_annotate_plan(id, &json!({"resolvedProfile":profile,"profileWarnings":warnings}))?;
    }
    Ok(report)
}

fn layout(s: &mut Session, p: &Value) -> Result<Value> {
    let kind = p["kind"].as_str().ok_or_else(|| bad("profile.layout", "kind is pip, split or vertical"))?;
    let sequence = p["sequence"].as_u64().map(ItemId).or(s.state.active_sequence).ok_or(crate::EngineError::NoSequence)?;
    let q = s.project.sequence(sequence).ok_or_else(|| bad("profile.layout", "sequence is missing"))?;
    let clips = p["clips"].as_array().filter(|a| !a.is_empty() && a.len() <= 32).ok_or_else(|| bad("profile.layout", "supply 1–32 video clip IDs"))?;
    if !["pip", "split", "vertical"].contains(&kind) || (kind == "split" && clips.len() != 2) || (kind == "pip" && clips.len() != 2) {
        return Err(bad("profile.layout", "pip and split require two video clips; vertical accepts 1–32"));
    }
    if kind == "pip" || kind == "split" {
        let pair = clips
            .iter()
            .map(|v| v.as_u64().and_then(|id| q.find_item(filmcraft_project::ClipId(id))))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| bad("profile.layout", "clip is missing"))?;
        if let (Some((ta, a)), Some((tb, b))) = (pair.first(), pair.get(1)) {
            if ta == tb || a.start >= b.end() || b.start >= a.end() {
                return Err(bad("profile.layout", "PiP and split targets must overlap on different video tracks"));
            }
            if kind == "pip" && q.video_tracks.iter().position(|t| t.id == *tb) <= q.video_tracks.iter().position(|t| t.id == *ta) {
                return Err(bad("profile.layout", "the second PiP clip must be on a higher video track than the background"));
            }
        }
    }
    let duplicate = p["variant"].as_bool().unwrap_or(true);
    let mut steps = if duplicate {
        vec![
            json!({"id":"project.select","params":{"items":[sequence.0]}}),
            json!({"id":"edit.duplicate","params":{}}),
            json!({"id":"sequence.open","params":{"item":{"$step":1,"path":"/items/0"}}}),
        ]
    } else {
        vec![json!({"id":"sequence.open","params":{"item":sequence.0}})]
    };
    let (width, height) = if kind == "vertical" { (1080.0, 1920.0) } else { (f64::from(q.settings.width), f64::from(q.settings.height)) };
    if kind == "vertical" {
        steps.push(json!({"id":"sequence.settings","params":{"width":1080,"height":1920,"pixelAspect":[1,1]}}))
    }
    let mut seen = std::collections::BTreeSet::new();
    for (i, id) in clips.iter().enumerate() {
        let id = id.as_u64().ok_or_else(|| bad("profile.layout", "invalid clip ID"))?;
        if !seen.insert(id) {
            return Err(bad("profile.layout", "duplicate clip ID"));
        }
        let (_, clip) = q
            .find_item(filmcraft_project::ClipId(id))
            .filter(|(t, _)| q.video_tracks.iter().any(|v| v.id == *t))
            .ok_or_else(|| bad("profile.layout", "clip must belong to a video track"))?;
        if q.video_tracks.iter().any(|t| t.locked && t.items.iter().any(|c| c.id.0 == id)) {
            return Err(bad("profile.layout", "cannot lay out a clip on a locked track"));
        }
        if clip
            .effects
            .iter()
            .find(|e| e.effect == "motion")
            .is_some_and(|e| ["position", "scale"].iter().any(|id| e.param(id).is_some_and(|p| p.is_animated())))
        {
            return Err(bad("profile.layout", "layout target has animated motion; preserve the animation and prepare explicit keyframe edits instead"));
        }
        let size = s.project.source_size(clip.item).unwrap_or((q.settings.width, q.settings.height));
        let source_par = s.project.source_par(clip.item).unwrap_or((1, 1));
        let frame_par = if kind == "vertical" { (1, 1) } else { q.settings.par };
        let (sw, sh) = filmcraft_project::conformed_size(size, source_par, frame_par);
        if sw <= 0.0 || sh <= 0.0 {
            return Err(bad("profile.layout", "source dimensions are invalid"));
        }
        let (x, y, tw, th, fill) = match kind {
            "pip" if i == 1 => (width * 0.79, height * 0.78, width * 0.35, height * 0.35, false),
            "split" => (width * (0.25 + 0.5 * i as f64), height * 0.5, width * 0.5, height, false),
            "vertical" => (width * 0.5, height * 0.5, width, height, true),
            _ => (width * 0.5, height * 0.5, width, height, false),
        };
        let scale = if fill { (tw / sw).max(th / sh) } else { (tw / sw).min(th / sh) } * 100.0;
        steps.push(json!({"id":"effects.setParam","params":{"clip":id,"effect":"motion","param":"position","value":[x,y]}}));
        steps.push(json!({"id":"effects.setParam","params":{"clip":id,"effect":"motion","param":"scale","value":scale}}));
    }
    s.execute("agent.plan", json!({"label":format!("{} editable variant",kind),"baseRevision":s.revision,"steps":steps}))
}
pub fn commands() -> Vec<CommandSpec> {
    let spec = |id, label, params, run, journal| CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal };
    vec![
        spec("profile.save", "Save Editing Profile Version", r#"{"profile":EditingProfile}"#, save, true),
        spec(
            "profile.list",
            "List Editing Profiles",
            "{}",
            |s, _| Ok(json!({"active":s.project.review.active_profile,"profiles":s.project.review.profiles})),
            false,
        ),
        spec("profile.get", "Read Editing Profile", r#"{"name":str?,"version":u32?}"#, |s, p| value(resolve_profile(s, p)?), false),
        spec(
            "profile.resolve",
            "Resolve Profile Overrides",
            r#"{"name":str?,"version":u32?,"overrides":object?}"#,
            |s, p| value(resolve_profile(s, p)?),
            false,
        ),
        spec("profile.apply", "Apply Profile Preferences", r#"{"name":str?,"version":u32?,"overrides":object?}"#, apply, true),
        spec(
            "profile.plan",
            "Propose Profile",
            r#"{"name":str?,"version":u32?,"overrides":object?,"delivery":bool=true,"normalizeAudio":bool=true}"#,
            plan,
            false,
        ),
        spec("profile.layout", "Propose Editable Layout", r#"{"kind":"pip|split|vertical","sequence":id?,"clips":[id],"variant":bool=true}"#, layout, false),
    ]
}
