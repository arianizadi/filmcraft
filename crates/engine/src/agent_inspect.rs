//! Targeted, bounded inspection for proposal authors.

use filmcraft_project::ItemId;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::{EngineError, Result, Session};

fn optional_u64(p: &Value, key: &str) -> Result<Option<u64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| bad("agent.inspect", format!("{key} must be an unsigned integer"))),
    }
}

fn optional_tick(p: &Value, key: &str) -> Result<Option<i64>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_i64().filter(|v| *v >= 0).map(Some).ok_or_else(|| bad("agent.inspect", format!("{key} must be nonnegative ticks"))),
    }
}

fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let id = optional_u64(p, "sequence")?.map(ItemId).or(s.state.active_sequence).ok_or(EngineError::NoSequence)?;
    let clip = optional_u64(p, "clip")?;
    let start = optional_tick(p, "start")?;
    let end = optional_tick(p, "end")?;
    if start.zip(end).is_some_and(|(a, b)| a >= b) {
        return Err(bad("agent.inspect", "end must be after start"));
    }
    let seq = s.project.sequence(id).ok_or(EngineError::NoSequence)?;
    if clip.is_some_and(|c| !seq.all_tracks().any(|t| t.items.iter().any(|i| i.id.0 == c))) {
        return Err(bad("agent.inspect", "clip does not belong to the requested sequence"));
    }
    let mut result = crate::commands::inspect_sequence(s, id, seq);
    for kind in ["video", "audio"] {
        if let Some(tracks) = result.get_mut(kind).and_then(Value::as_array_mut) {
            for track in tracks.iter_mut() {
                if let Some(items) = track.get_mut("items").and_then(Value::as_array_mut) {
                    items.retain(|i| {
                        clip.is_none_or(|c| i["clip"].as_u64() == Some(c))
                            && start.is_none_or(|a| i["end"].as_i64().is_some_and(|b| b > a))
                            && end.is_none_or(|b| i["start"].as_i64().is_some_and(|a| a < b))
                    });
                }
            }
            if clip.is_some() {
                tracks.retain(|t| t["items"].as_array().is_some_and(|a| !a.is_empty()));
            }
        }
    }
    // Keep legacy inspection fields and add exact serializable effects with all keyframes/masks.
    for kind in ["video", "audio"] {
        if let Some(tracks) = result.get_mut(kind).and_then(Value::as_array_mut) {
            for track in tracks {
                if let Some(items) = track.get_mut("items").and_then(Value::as_array_mut) {
                    for item in items {
                        if let Some(id) = item["clip"].as_u64()
                            && let Some(original) = seq.all_tracks().flat_map(|t| t.items.iter()).find(|i| i.id.0 == id)
                        {
                            item["typedEffects"] = serde_json::to_value(&original.effects).map_err(|e| bad("agent.inspect", e.to_string()))?;
                        }
                    }
                }
            }
        }
    }
    if let Some(fields) = p.get("fields").filter(|v| !v.is_null()) {
        let fields = fields.as_array().filter(|a| a.len() <= 32).ok_or_else(|| bad("agent.inspect", "fields must contain at most 32 names"))?;
        let names: Vec<&str> = fields.iter().map(|f| f.as_str().ok_or_else(|| bad("agent.inspect", "field names must be strings"))).collect::<Result<_>>()?;
        if let Some(map) = result.as_object_mut() {
            for name in &names {
                if !map.contains_key(*name) {
                    return Err(bad("agent.inspect", format!("unknown sequence field {name}")));
                }
            }
            map.retain(|k, _| k == "id" || names.contains(&k.as_str()));
        }
    }
    result["revision"] = json!(s.revision);
    result["sessionId"] = json!(s.operations.session_id);
    result["documentEpoch"] = json!(s.document_epoch);
    Ok(result)
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "agent.inspect",
        label: "Inspect Timeline Range",
        menu: &[],
        shortcut: None,
        params: r#"{"sequence":u64?,"clip":u64?,"start":ticks?,"end":ticks?,"fields":[string]?}"#,
        enabled: always,
        run: inspect,
        journal: false,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn targeted_inspection_does_not_edit_and_rejects_hostile_ranges() {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"name":"Test"})).unwrap();
        let revision = s.revision;
        let v = inspect(&mut s, &json!({"fields":["settings"]})).unwrap();
        assert!(v.get("video").is_none());
        assert_eq!(v["revision"], revision);
        assert_eq!(s.revision, revision);
        assert!(inspect(&mut s, &json!({"start":2,"end":1})).is_err());
        assert!(inspect(&mut s, &json!({"clip":u64::MAX})).is_err());
        assert!(inspect(&mut s, &json!({"fields":["missing"]})).is_err());
    }
}
