//! Source-word based dialogue proposals and explicit, time-preserving caption translations.
use crate::{
    Result, Session,
    commands::{CommandSpec, always, bad},
};
use filmcraft_project::{ClipId, ItemId, TrackId};
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn required(p: &Value, key: &str, cmd: &str) -> Result<u64> {
    p[key].as_u64().ok_or_else(|| bad(cmd, format!("{key} must be an unsigned ID")))
}
fn source_words(s: &mut Session, p: &Value) -> Result<Value> {
    let item = ItemId(required(p, "item", "transcript.sourceWords")?);
    let transcript = s.project.transcripts.get(&item).ok_or_else(|| bad("transcript.sourceWords", "transcript is missing"))?;
    Ok(json!({"item":item.0,"transcript":transcript}))
}
fn patch(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 262_144)?;
    let item = ItemId(required(p, "item", "transcript.patchWords")?);
    let mut transcript = s.project.transcripts.get(&item).map(|t| (**t).clone()).ok_or_else(|| bad("transcript.patchWords", "transcript is missing"))?;
    let changes = p["words"].as_array().filter(|a| !a.is_empty() && a.len() <= 1000).ok_or_else(|| bad("transcript.patchWords", "give 1–1000 word patches"))?;
    let mut seen = BTreeSet::new();
    for change in changes {
        let id = required(change, "id", "transcript.patchWords")?;
        if !seen.insert(id) {
            return Err(bad("transcript.patchWords", "duplicate word patch"));
        }
        let word = transcript.words.iter_mut().find(|w| w.id == id && id != 0).ok_or_else(|| bad("transcript.patchWords", "source word is missing"))?;
        if let Some(text) = change.get("text") {
            word.text = text
                .as_str()
                .filter(|s| !s.trim().is_empty() && s.len() <= 4096)
                .ok_or_else(|| bad("transcript.patchWords", "text must contain 1–4096 bytes"))?
                .to_string()
        }
        if let Some(start) = change.get("start") {
            word.start = Tick(start.as_i64().filter(|t| *t >= 0).ok_or_else(|| bad("transcript.patchWords", "invalid start"))?)
        }
        if let Some(end) = change.get("end") {
            word.end = Tick(end.as_i64().filter(|t| *t >= 0).ok_or_else(|| bad("transcript.patchWords", "invalid end"))?)
        }
    }
    transcript.words.sort_by_key(|w| (w.start, w.end));
    transcript.check().map_err(|e| bad("transcript.patchWords", e))?;
    s.edit("Correct Transcript Words", move |pr, _| {
        pr.store_transcript(item, transcript);
        Ok(())
    })?;
    Ok(json!({"item":item.0,"corrected":changes.len()}))
}
fn resolve_range(s: &Session, p: &Value) -> Result<(TimeRange, Value)> {
    let clip = ClipId(required(p, "clip", "transcript.cutRange")?);
    let from = required(p, "fromWordId", "transcript.cutRange")?;
    let to = p["toWordId"].as_u64().unwrap_or(from);
    let words = crate::transcript::sequence_words(s);
    let locate = |id| {
        words
            .iter()
            .position(|w| w.clip == clip && s.project.transcripts.get(&w.item).and_then(|t| t.words.get(w.index)).is_some_and(|w| w.id == id && id != 0))
    };
    let a = locate(from).ok_or_else(|| bad("transcript.cutRange", "first source word is missing from this clip"))?;
    let b = locate(to).filter(|b| *b >= a).ok_or_else(|| bad("transcript.cutRange", "last source word is missing or precedes the first"))?;
    if words.get(a..=b).is_none_or(|w| w.iter().any(|w| w.clip != clip)) {
        return Err(bad("transcript.cutRange", "word range crosses clip occurrences"));
    }
    let mut range = filmcraft_edit::transcript::word_range(&words, a, b, s.sequence_rate())
        .ok_or_else(|| bad("transcript.cutRange", "word range cannot be represented"))?;
    let padding = p["paddingTicks"].as_i64().unwrap_or(0);
    if !(0..=filmcraft_time::TICKS_PER_SECOND * 10).contains(&padding) {
        return Err(bad("transcript.cutRange", "paddingTicks must be 0–10 seconds"));
    }
    let end = Tick(range.end().0.saturating_add(padding).min(Tick::MAX.0));
    range = TimeRange::from_bounds(Tick(range.start.0.saturating_sub(padding).max(0)), end);
    let q = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let (track, source) = q.find_item(clip).ok_or_else(|| bad("transcript.cutRange", "clip is missing"))?;
    let linked: Vec<Value> = q
        .all_tracks()
        .flat_map(|t| {
            t.items
                .iter()
                .filter(move |c| c.id == clip || source.link.is_some() && c.link == source.link)
                .map(move |c| json!({"clip":c.id.0,"track":t.id.0,"name":c.name,"locked":t.locked}))
        })
        .collect();
    if q.track(track).is_some_and(|t| t.locked) || linked.iter().any(|c| c["locked"] == true) {
        return Err(bad("transcript.cutRange", "a source or linked track is locked; unlock before proposing this cut"));
    }
    let affected: Vec<Value> = q.all_tracks().map(|t| json!({"track":t.id.0,"name":t.name,"locked":t.locked,"ripple":!t.locked})).collect();
    Ok((range, json!({"clip":clip.0,"fromWordId":from,"toWordId":to,"start":range.start.0,"end":range.end().0,"linked":linked,"tracks":affected})))
}
fn cut_range(s: &mut Session, p: &Value) -> Result<Value> {
    let (range, info) = resolve_range(s, p)?;
    let mode = p["mode"].as_str().unwrap_or("extract");
    if !["extract", "lift"].contains(&mode) {
        return Err(bad("transcript.cutRange", "mode is extract or lift"));
    }
    s.edit_sequence("Approved Transcript Range", |q, ctx, _| {
        if mode == "extract" {
            filmcraft_edit::transcript::ripple_delete_ranges(q, vec![range], ctx);
        } else {
            let tracks = q.all_tracks().filter(|t| !t.locked).map(|t| t.id).collect::<Vec<_>>();
            filmcraft_edit::lift(q, &tracks, range, ctx);
        }
        Ok(())
    })?;
    Ok(info)
}
fn propose(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 262_144)?;
    let ranges = p["ranges"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 128)
        .ok_or_else(|| bad("transcript.proposeCuts", "give 1–128 stable source-word ranges"))?;
    let mut resolved = Vec::new();
    for range in ranges {
        let (span, info) = resolve_range(s, range)?;
        resolved.push((span, range.clone(), info))
    }
    resolved.sort_by_key(|(span, _, _)| span.start);
    if resolved.windows(2).any(|w| w.first().zip(w.get(1)).is_some_and(|(a, b)| a.0.end() > b.0.start)) {
        return Err(bad("transcript.proposeCuts", "cut ranges overlap"));
    }
    resolved.reverse();
    let steps = resolved.iter().map(|(_, p, _)| json!({"id":"transcript.cutRange","params":p})).collect::<Vec<_>>();
    let info = resolved.iter().enumerate().map(|(index, (_, _, info))| json!({"step":index,"range":info})).collect::<Vec<_>>();
    let mut result = s.execute("agent.plan", json!({"label":p["label"].as_str().unwrap_or("Review dialogue cuts"),"baseRevision":s.revision,"steps":steps}))?;
    if let Some(id) = result["plan"].as_u64() {
        result = s.agent_annotate_plan(id, &json!({"ranges":info}))?;
    }
    Ok(result)
}
fn translate(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 1_048_576)?;
    let track = TrackId(required(p, "sourceTrack", "captions.translate")?);
    let language = p["language"]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= 64)
        .ok_or_else(|| bad("captions.translate", "target language is required"))?
        .to_string();
    let name = p["name"].as_str().unwrap_or(&language).to_string();
    if name.len() > 256 {
        return Err(bad("captions.translate", "name too long"));
    }
    let source = s
        .active_sequence()
        .and_then(|q| q.caption_tracks.iter().find(|t| t.id == track))
        .cloned()
        .ok_or_else(|| bad("captions.translate", "source caption track is missing"))?;
    let translations = p["translations"].as_array().filter(|a| a.len() == source.captions.len() && a.len() <= 10_000).ok_or_else(|| {
        bad("captions.translate", "provide exactly one explicit translation per source cue (up to 10000); no translation engine is configured")
    })?;
    let mut by_id = BTreeMap::new();
    for row in translations {
        let id = required(row, "caption", "captions.translate")?;
        let text = row["text"]
            .as_str()
            .filter(|s| !s.trim().is_empty() && s.len() <= 16_384)
            .ok_or_else(|| bad("captions.translate", "translated cue text must contain 1–16384 bytes"))?
            .to_string();
        if by_id.insert(id, text).is_some() {
            return Err(bad("captions.translate", "duplicate translated cue"));
        }
    }
    if source.captions.iter().any(|c| !by_id.contains_key(&c.id.0)) {
        return Err(bad("captions.translate", "translation cue IDs must match the source track exactly"));
    }
    let mut output = source.clone();
    output.name = name;
    output.language = language;
    output.locked = false;
    for cue in &mut output.captions {
        cue.text = by_id.remove(&cue.id.0).ok_or_else(|| bad("captions.translate", "translation is missing"))?
    }
    let count = output.captions.len();
    let id = s.edit_sequence("Translated Caption Track", move |q, ctx, _| {
        output.id = TrackId(ctx.alloc());
        for cue in &mut output.captions {
            cue.id = ClipId(ctx.alloc())
        }
        let id = output.id;
        q.caption_tracks.insert(0, output);
        Ok(id)
    })?;
    Ok(json!({"track":id.0,"sourceTrack":track.0,"captions":count,"timingPreserved":true,"translationSource":"explicit_supplied_text"}))
}
pub fn commands() -> Vec<CommandSpec> {
    let spec = |id, label, params, run, journal| CommandSpec { id, label, menu: &[], shortcut: None, params, enabled: always, run, journal };
    vec![
        spec("transcript.sourceWords", "Read Source Words", r#"{"item":id}"#, source_words, false),
        spec("transcript.patchWords", "Correct Stable Source Words", r#"{"item":id,"words":[{"id":id,"text":str?,"start":ticks?,"end":ticks?}]}"#, patch, true),
        spec(
            "transcript.cutRange",
            "Cut Stable Source Word Range",
            r#"{"clip":id,"fromWordId":id,"toWordId":id?,"paddingTicks":ticks?,"mode":"extract|lift"?}"#,
            cut_range,
            true,
        ),
        spec(
            "transcript.proposeCuts",
            "Propose Reviewable Dialogue Cuts",
            r#"{"label":str?,"ranges":[{"clip":id,"fromWordId":id,"toWordId":id?,"mode":"extract|lift"?}]}"#,
            propose,
            false,
        ),
        spec(
            "captions.translate",
            "Create Translated Caption Track",
            r#"{"sourceTrack":id,"language":str,"name":str?,"translations":[{"caption":id,"text":str}]}"#,
            translate,
            true,
        ),
    ]
}
