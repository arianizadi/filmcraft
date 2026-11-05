//! Sandboxed, parameterized motion templates: declarative native graphic layers and keyframes.
//! No scripting, network access, external assets or hidden render callbacks.
use crate::{
    Result, Session,
    commands::{CommandSpec, always, bad},
};
use filmcraft_time::Tick;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    template: Template,
    #[serde(default)]
    controls: BTreeMap<String, Value>,
    at: Option<i64>,
    track: Option<usize>,
    base_revision: Option<u64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Template {
    name: String,
    duration_frames: u32,
    #[serde(default)]
    controls: BTreeMap<String, Control>,
    layers: Vec<Layer>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Control {
    kind: String,
    default: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Layer {
    kind: String,
    #[serde(default)]
    props: BTreeMap<String, Value>,
    #[serde(default)]
    keys: Vec<Key>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Key {
    at_frame: u32,
    props: BTreeMap<String, Value>,
}
fn invalid(message: impl Into<String>) -> crate::EngineError {
    bad("motion.plan", message)
}
fn kind_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "text" => value.as_str().is_some_and(|s| s.len() <= 4096),
        "number" => value.as_f64().is_some_and(|n| n.is_finite() && n.abs() <= 100000.0),
        "point" => value.as_array().is_some_and(|a| a.len() == 2 && a.iter().all(|v| kind_matches("number", v))),
        "color" => value.as_array().is_some_and(|a| a.len() == 4 && a.iter().all(|v| v.as_f64().is_some_and(|n| n.is_finite() && (0.0..=1.0).contains(&n)))),
        _ => false,
    }
}
fn substitute(value: &Value, controls: &BTreeMap<String, Value>, depth: usize) -> Result<Value> {
    if depth > 4 {
        return Err(invalid("template value nesting exceeds four levels"));
    }
    Ok(match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("control") => {
            let name = map.get("control").and_then(Value::as_str).ok_or_else(|| invalid("control reference needs a string name"))?;
            controls.get(name).cloned().ok_or_else(|| invalid(format!("unknown control {name}")))?
        }
        Value::Object(_) => return Err(invalid("property objects must be a single named control reference")),
        Value::Array(values) => {
            if values.len() > 64 {
                return Err(invalid("property array exceeds 64 values"));
            }
            Value::Array(values.iter().map(|v| substitute(v, controls, depth + 1)).collect::<Result<_>>()?)
        }
        Value::String(s) if s.len() > 4096 => return Err(invalid("template text exceeds 4096 bytes")),
        Value::Number(n) if n.as_f64().is_none_or(|n| !n.is_finite() || n.abs() > 100000.0) => {
            return Err(invalid("template numeric property exceeds finite bounds"));
        }
        Value::Null => return Err(invalid("null is not a template property value")),
        other => other.clone(),
    })
}
fn properties(props: &BTreeMap<String, Value>, controls: &BTreeMap<String, Value>) -> Result<Value> {
    if props.len() > 32 || props.keys().any(|k| k.len() > 64) {
        return Err(invalid("at most 32 named properties per layer/keyframe"));
    }
    props.iter().map(|(key, value)| Ok((key.clone(), substitute(value, controls, 0)?))).collect::<Result<serde_json::Map<_, _>>>().map(Value::Object)
}
fn plan(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 256 * 1024)?;
    let request: Request = serde_json::from_value(p.clone()).map_err(|e| invalid(e.to_string()))?;
    let template = request.template;
    if template.name.is_empty()
        || template.name.len() > 128
        || !(1..=16).contains(&template.layers.len())
        || template.controls.len() > 32
        || !(1..=1_000_000).contains(&template.duration_frames)
    {
        return Err(invalid("name, layer count, controls or duration is outside template limits"));
    }
    if request.controls.keys().any(|k| !template.controls.contains_key(k)) {
        return Err(invalid("an override names a control absent from the template"));
    }
    let mut controls = BTreeMap::new();
    for (name, control) in &template.controls {
        let value = request.controls.get(name).unwrap_or(&control.default);
        if name.is_empty() || name.len() > 64 || !kind_matches(&control.kind, value) {
            return Err(invalid(format!("invalid {name} control value or kind")));
        }
        controls.insert(name.clone(), value.clone());
    }
    let sequence = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let rate = sequence.settings.frame_rate;
    let start = rate.snap(Tick(request.at.unwrap_or(s.playhead().0)));
    let duration = rate.tick_of(i64::from(template.duration_frames));
    if start < Tick::ZERO
        || duration <= Tick::ZERO
        || duration.0 > filmcraft_time::TICKS_PER_SECOND * 86400
        || start.0.checked_add(duration.0).is_none_or(|end| end > Tick::MAX.0)
    {
        return Err(invalid("template placement is outside representable timeline bounds"));
    }
    let mut steps = Vec::new();
    let mut first = None;
    for layer in &template.layers {
        if layer.keys.len() > 32 {
            return Err(invalid("at most 32 keyframes per layer"));
        }
        let text = layer.kind == "text";
        if !text && !matches!(layer.kind.as_str(), "rectangle" | "ellipse" | "polygon" | "path") {
            return Err(invalid("layer must be text, rectangle, ellipse, polygon or path"));
        }
        let mut params = json!({"duration":duration.0,"time":start.0});
        if let Some(track) = request.track {
            if track > 128 {
                return Err(invalid("track index exceeds 128"));
            }
            params["track"] = json!(track);
        }
        if let Some(first) = first {
            params["clip"] = json!({"$step":first,"path":"/clip"});
        } else if text {
            params["newClip"] = json!(true);
        }
        if text {
            params["text"] = json!("Motion template");
        } else {
            params["shape"] = json!(layer.kind);
            if layer.kind == "path" {
                let props = properties(&layer.props, &controls)?;
                params["points"] = props.get("points").cloned().ok_or_else(|| invalid("path layer needs points"))?;
            }
        }
        let creator = steps.len();
        first.get_or_insert(creator);
        steps.push(json!({"id":if text{"graphics.newText"}else{"graphics.newShape"},"params":params}));
        let clip = json!({"$step":creator,"path":"/clip"});
        let index = json!({"$step":creator,"path":"/layer"});
        let mut props = properties(&layer.props, &controls)?;
        if let Some(props) = props.as_object_mut() {
            props.remove("points");
        }
        if props.as_object().is_some_and(|p| !p.is_empty()) {
            steps.push(json!({"id":"graphics.set","params":{"clip":clip,"layer":index,"props":props,"time":start.0}}));
        }
        let mut previous = None;
        for key in &layer.keys {
            if key.at_frame >= template.duration_frames || previous.is_some_and(|p| p >= key.at_frame) {
                return Err(invalid("layer keyframes must be in increasing frame order within the template"));
            }
            previous = Some(key.at_frame);
            let time = start.0.checked_add(rate.tick_of(i64::from(key.at_frame)).0).ok_or_else(|| invalid("keyframe time overflows"))?;
            steps.push(json!({"id":"graphics.set","params":{"clip":clip,"layer":index,"props":properties(&key.props,&controls)?,"time":time,"keyframe":true}}));
        }
    }
    if steps.len() > 256 {
        return Err(invalid("template exceeds the 256-step proposal budget"));
    }
    let mut result =
        s.execute("agent.plan", json!({"label":format!("Motion: {}",template.name),"baseRevision":request.base_revision.unwrap_or(s.revision),"steps":steps}))?;
    result["template"] = json!({"name":template.name,"controls":controls,"durationFrames":template.duration_frames,"frameRate":rate,"sandbox":"Native declarative graphics and keyframes only; no scripts, external files or network"});
    Ok(result)
}
fn catalogue(s: &mut Session, _: &Value) -> Result<Value> {
    let settings = s.active_sequence().map(|q| q.settings.clone()).unwrap_or_default();
    let (x, y) = (f64::from(settings.width) / 1920.0, f64::from(settings.height) / 1080.0);
    let frames = settings.frame_rate.frame_at(Tick(filmcraft_time::TICKS_PER_SECOND * 5)).clamp(4, 1_000_000) as u32;
    let fade = (frames / 10).max(1);
    let keys = json!([{"atFrame":0,"props":{"opacity":0}},{"atFrame":fade,"props":{"opacity":100}},{"atFrame":frames-fade-1,"props":{"opacity":100}},{"atFrame":frames-1,"props":{"opacity":0}}]);
    Ok(
        json!({"templates":[{"name":"Lower third","durationFrames":frames,"controls":{"title":{"kind":"text","default":"Your title"},"accent":{"kind":"color","default":[0.15,0.65,0.65,1.0]}},"layers":[{"kind":"rectangle","props":{"position":[400.0*x,900.0*y],"size":[650.0*x,140.0*y],"fill_color":{"control":"accent"}},"keys":keys},{"kind":"text","props":{"text":{"control":"title"},"position":[120.0*x,920.0*y],"size":64.0*y,"fill_color":[1.0,1.0,1.0,1.0]},"keys":keys}]}],"license":"MIT OR Apache-2.0","author":"FilmCraft contributors","placementUnits":"sequence pixels; keyframes in sequence frames","designSequence":{"width":settings.width,"height":settings.height,"frameRate":settings.frame_rate}}),
    )
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "motion.templates",
            label: "List Sandboxed Motion Templates",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: always,
            journal: false,
            run: catalogue,
        },
        CommandSpec {
            id: "motion.plan",
            label: "Propose a Sandboxed Motion Template",
            menu: &[],
            shortcut: None,
            params: r#"{"template":{"name":str,"durationFrames":n,"controls":object?,"layers":[{kind:enum,props:object,keys:[{atFrame:n,props:object}]}]},"controls":object?,"at":ticks?,"track":index?,"baseRevision":u64?}"#,
            enabled: always,
            journal: false,
            run: plan,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn template_is_staged_rendered_and_remains_editable_in_one_undo() {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"width":32,"height":18,"frameRate":{"num":24,"den":1}})).unwrap();
        let revision = s.revision;
        let undo = s.history.undo.len();
        let template = json!({"name":"Fade","durationFrames":12,"controls":{"fill":{"kind":"color","default":[1.0,0.0,0.0,1.0]}},"layers":[{"kind":"rectangle","props":{"position":[16,9],"size":[20,10],"fill_color":{"control":"fill"}},"keys":[{"atFrame":0,"props":{"opacity":0}},{"atFrame":6,"props":{"opacity":100}}]}]});
        let result = s.execute("motion.plan", json!({"template":template})).unwrap();
        assert_eq!(s.revision, revision);
        assert_eq!(s.history.undo.len(), undo);
        let id = result["plan"].as_u64().unwrap();
        let before = s.execute("render.frames", json!({"target":"sequence","plan":id,"frames":[0,6],"maxSide":32})).unwrap();
        assert!(before["frames"].as_array().is_some());
        s.execute("agent.apply", json!({"plan":id})).unwrap();
        assert_eq!(s.history.undo.len(), undo + 1);
        let clip = &s.active_sequence().unwrap().video_tracks.iter().flat_map(|t| &t.items).next().unwrap();
        let layer = clip.effects.iter().find(|e| e.effect == filmcraft_project::graphic::SHAPE_LAYER).unwrap();
        assert_eq!(layer.params["opacity"].keyframes.len(), 2);
        assert_eq!(layer.params["opacity"].keyframes[1].time, s.active_sequence().unwrap().settings.frame_rate.tick_of(6));
        assert_eq!(clip.duration, s.active_sequence().unwrap().settings.frame_rate.tick_of(12));
        assert!(s.undo().is_some());
        assert!(s.active_sequence().unwrap().video_tracks.iter().all(|t| t.items.is_empty()));
        let mut hostile = template.clone();
        hostile["script"] = json!("do something");
        assert!(s.execute("motion.plan", json!({"template":hostile})).is_err());
        assert!(s.execute("motion.plan", json!({"template":template,"controls":{"fill":[1,2,3,4]}})).is_err());
        assert!(s.execute("motion.plan", json!({"template":template,"at":Tick::MAX.0})).is_err());
    }
    #[test]
    fn builtin_layout_and_duration_follow_actual_8k_and_hfr_sequence() {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"width":7680,"height":4320,"frameRate":{"num":960,"den":1}})).unwrap();
        let catalogue = s.execute("motion.templates", json!({})).unwrap();
        let template = &catalogue["templates"][0];
        assert_eq!(template["durationFrames"], 4800);
        assert_eq!(template["layers"][0]["props"]["position"], json!([1600.0, 3600.0]));
        assert!(s.execute("motion.plan", json!({"template":template,"controls":{"title":"8K960"}})).is_ok());
    }
}
