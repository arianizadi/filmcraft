//! ASC CDL interchange and an explicitly reported, sampled color-stack LUT bake.
use crate::color_library::{flag, guard, sample_time, target};
use crate::commands::{CommandSpec, always, bad};
use crate::{Result, Session};
use filmcraft_color::cdl::{AscCdl, MAX_CDL_BYTES};
use filmcraft_project::{EffectInstance, Param, ParamValue};
use serde_json::{Value, json};

fn input(s: &Session, p: &Value, cmd: &str) -> Result<String> {
    match (p.get("xml"), p.get("path")) {
        (Some(v), None) => v.as_str().filter(|v| v.len() <= MAX_CDL_BYTES).map(str::to_string).ok_or_else(|| bad(cmd, "xml must be a string of at most 1 MiB")),
        (None, Some(v)) => {
            let path = v.as_str().filter(|v| !v.is_empty() && v.len() <= 4096).ok_or_else(|| bad(cmd, "invalid path"))?;
            if s.services.file_size(path).map_err(|e| bad(cmd, e.to_string()))? > MAX_CDL_BYTES as u64 {
                return Err(bad(cmd, "CDL exceeds 1 MiB"));
            }
            let bytes = s.services.read_range(path, 0, MAX_CDL_BYTES + 1).map_err(|e| bad(cmd, e.to_string()))?;
            if bytes.len() > MAX_CDL_BYTES {
                return Err(bad(cmd, "CDL exceeds 1 MiB"));
            }
            String::from_utf8(bytes).map_err(|e| bad(cmd, e.to_string()))
        }
        _ => Err(bad(cmd, "provide exactly one of xml or path")),
    }
}
fn output(s: &Session, p: &Value, cmd: &str, text: String, mut report: Value) -> Result<Value> {
    if let Some(path) = p.get("path") {
        let path = path.as_str().filter(|v| !v.is_empty() && v.len() <= 4096).ok_or_else(|| bad(cmd, "invalid output path"))?;
        // Exclusive publication is atomic on the native host and never destroys an existing file.
        s.services.write_file_new(path, text.as_bytes()).map_err(|e| bad(cmd, format!("could not publish {path}: {e}")))?;
        report["path"] = json!(path);
    } else {
        report["text"] = json!(text);
    }
    Ok(report)
}
fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let values = filmcraft_color::cdl::parse_xml(&input(s, p, "grade.cdl.inspect")?).map_err(|e| bad("grade.cdl.inspect", e))?;
    Ok(json!({"corrections":values,"style":"ASC-CDL-v1.2-Fwd-clamped","encoding":"sRGB-encoded-rec709"}))
}
fn selected_cdl(t: &crate::color_library::ClipTarget, p: &Value, cmd: &str) -> Result<(Option<usize>, usize, EffectInstance)> {
    let id = p.get("effectInstance").and_then(Value::as_str).ok_or_else(|| bad(cmd, "need effectInstance asc_cdl:<ordinal> or new"))?;
    let mut instances = t.item.effects.iter().enumerate().filter(|(_, e)| e.effect == "asc_cdl");
    if id == "new" {
        let count = instances.count();
        return Ok((None, count, filmcraft_project::find_effect("asc_cdl").ok_or_else(|| bad(cmd, "CDL definition missing"))?.instance()));
    }
    let ordinal =
        id.strip_prefix("asc_cdl:").and_then(|v| v.parse::<usize>().ok()).filter(|v| *v < 256).ok_or_else(|| bad(cmd, "invalid ASC CDL effect instance"))?;
    let (index, e) = instances.nth(ordinal).ok_or_else(|| bad(cmd, "ASC CDL instance not found"))?;
    Ok((Some(index), ordinal, e.clone()))
}
fn import(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.cdl.import";
    guard(s, p, CMD)?;
    let t = target(s, p, true, CMD)?;
    if !t.pipeline.is_plain() {
        return Err(bad(CMD, "ASC CDL uses sRGB-encoded Rec.709 values; choose an SDR Rec.709 working sequence before importing"));
    }
    let values = filmcraft_color::cdl::parse_xml(&input(s, p, CMD)?).map_err(|e| bad(CMD, e))?;
    let c = if let Some(id) = p.get("correction").and_then(Value::as_str) {
        values.iter().find(|c| c.id == id).ok_or_else(|| bad(CMD, "correction ID not found"))?
    } else if values.len() == 1 {
        values.first().ok_or_else(|| bad(CMD, "no correction"))?
    } else {
        return Err(bad(CMD, "multiple corrections: select correction by ID"));
    };
    if c.slope.iter().chain(&c.power).any(|v| *v > 10.0) || c.offset.iter().any(|v| !(-1.0..=1.0).contains(v)) || c.saturation > 10.0 {
        return Err(bad(CMD, "CDL values exceed FilmCraft's editable ranges (slope/power/saturation 0..10, offset -1..1)"));
    }
    let (index, ordinal, mut effect) = selected_cdl(&t, p, CMD)?;
    if effect.is_animated() && !flag(p, "replaceKeyframes", false, CMD)? {
        return Err(bad(CMD, "replacing CDL animation requires replaceKeyframes:true"));
    }
    for (channel, n) in ["r", "g", "b"].into_iter().zip(0..3) {
        for (suffix, value) in [("slope", c.slope[n]), ("offset", c.offset[n]), ("power", c.power[n])] {
            effect.params.insert(format!("{channel}_{suffix}"), Param::new(ParamValue::Float(value)));
        }
    }
    effect.params.insert("saturation".into(), Param::new(ParamValue::Float(c.saturation)));
    filmcraft_project::color_workflow::validate_effects(std::slice::from_ref(&effect)).map_err(|e| bad(CMD, e))?;
    s.edit("Import ASC CDL", |project, _| {
        let (_, clip) = project.sequence_mut(t.sequence).and_then(|q| q.find_item_mut(t.clip)).ok_or_else(|| bad(CMD, "clip disappeared"))?;
        if let Some(index) = index {
            *clip.effects.get_mut(index).ok_or_else(|| bad(CMD, "effect disappeared"))? = effect.clone();
        } else {
            clip.effects.push(effect.clone());
        }
        Ok(())
    })?;
    Ok(json!({"clip":t.clip.0,"effectInstance":format!("asc_cdl:{ordinal}"),"correction":c,"revision":s.revision,"encoding":"sRGB-encoded-rec709"}))
}
fn export(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.cdl.export";
    let t = target(s, p, false, CMD)?;
    if !t.pipeline.is_plain() {
        return Err(bad(CMD, "CDL export requires the effect's SDR Rec.709 working pipeline"));
    }
    let (index, ordinal, mut effect) = selected_cdl(&t, p, CMD)?;
    if index.is_none() {
        return Err(bad(CMD, "export needs an existing ASC CDL effect"));
    }
    if effect.is_animated() {
        if !flag(p, "sampleAnimation", false, CMD)? {
            return Err(bad(CMD, "animated CDL export requires sampleAnimation:true and an exact frame"));
        }
        let time = t.item.effect_time_at(sample_time(s, &t, p)?);
        for param in effect.params.values_mut() {
            param.value = param.value_at(time);
            param.keyframes.clear();
        }
    }
    let values = |suffix: &str| {
        ["r", "g", "b"]
            .map(|ch| effect.params.get(&format!("{ch}_{suffix}")).and_then(|p| p.value.as_f64()).unwrap_or(if suffix == "offset" { 0.0 } else { 1.0 }))
    };
    let c = AscCdl {
        id: p.get("id").and_then(Value::as_str).unwrap_or("FilmCraft").into(),
        description: p.get("description").and_then(Value::as_str).unwrap_or("ASC CDL effect only; sRGB-encoded Rec.709, Fwd/clamped").into(),
        slope: values("slope"),
        offset: values("offset"),
        power: values("power"),
        saturation: effect.params.get("saturation").and_then(|p| p.value.as_f64()).unwrap_or(1.0),
    };
    let xml = c.to_xml().map_err(|e| bad(CMD, e))?;
    let omitted: Vec<_> = s
        .project
        .color_workflow
        .shared_effects(&t.item)
        .iter()
        .chain(t.item.effects.iter())
        .filter(|e| e.enabled && e.effect != "asc_cdl")
        .map(|e| e.effect.clone())
        .collect();
    output(
        s,
        p,
        CMD,
        xml,
        json!({"revision":s.revision,"clip":t.clip.0,"effectInstance":format!("asc_cdl:{ordinal}"),"correction":c,"enabled":effect.enabled,"masksOmitted":!effect.masks.is_empty(),"omittedEffects":omitted,"encoding":"sRGB-encoded-rec709","style":"ASC-CDL-v1.2-Fwd-clamped"}),
    )
}
fn switch(e: &EffectInstance, id: &str) -> bool {
    e.params.get(id).and_then(|p| p.value.as_bool()).unwrap_or(true)
}
fn unsupported(e: &EffectInstance, sample: bool) -> Option<String> {
    if !e.enabled {
        return None;
    }
    if e.is_animated() && !sample {
        return Some("animation requires sampleAnimation:true and an exact frame".into());
    }
    if !e.masks.is_empty() {
        return Some("geometric effect masks vary by image position".into());
    }
    if !filmcraft_project::color_workflow::is_color_effect(&e.effect) {
        return Some("spatial, temporal, compositing or intrinsic control cannot be represented by a color LUT".into());
    }
    if e.effect == "lumetri" {
        let value = |id| e.params.get(id).and_then(|p| p.value.as_f64()).unwrap_or(0.0).abs();
        if switch(e, "vignette_on") && value("vignette_amount") > 1e-6 || switch(e, "creative_on") && value("sharpen") > 1e-6 {
            return Some("Lumetri vignette and sharpening depend on image position/neighbors".into());
        }
        if e.params.get("hsl_on").and_then(|p| p.value.as_bool()).unwrap_or(false) && (value("hsl_denoise") > 1e-6 || value("hsl_blur") > 1e-6) {
            return Some("HSL mask denoise/blur depend on neighboring pixels".into());
        }
    }
    None
}
fn domain(p: &Value, key: &str, default: [f32; 3]) -> Result<[f32; 3]> {
    let Some(v) = p.get(key) else { return Ok(default) };
    let a = v.as_array().filter(|a| a.len() == 3).ok_or_else(|| bad("grade.bakeLut", format!("{key} needs 3 numbers")))?;
    let mut out = [0.0; 3];
    for (slot, v) in out.iter_mut().zip(a) {
        let n = v
            .as_f64()
            .filter(|n| n.is_finite() && n.abs() <= 65504.0)
            .ok_or_else(|| bad("grade.bakeLut", "domain values must be finite and within +/-65504"))?;
        *slot = n as f32;
    }
    Ok(out)
}
fn bake(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "grade.bakeLut";
    crate::agent_plans::bounded_json(p, 4 * 1024 * 1024)?;
    let t = target(s, p, false, CMD)?;
    let size = match p.get("size") {
        None => 33,
        Some(v) => v.as_u64().filter(|v| (2..=65).contains(v)).ok_or_else(|| bad(CMD, "size must be 2..65"))? as usize,
    };
    let min = domain(p, "domainMin", [0.0; 3])?;
    let max = domain(p, "domainMax", [1.0; 3])?;
    if min.iter().zip(max).any(|(a, b)| *a >= b) {
        return Err(bad(CMD, "domainMin must be strictly less than domainMax per channel"));
    }
    let sample = flag(p, "sampleAnimation", false, CMD)?;
    let time = sample_time(s, &t, p)?;
    let mt = t.item.effect_time_at(time);
    let encoding = p.get("encoding").and_then(Value::as_str).unwrap_or("working-linear");
    if !matches!(encoding, "working-linear" | "srgb") || encoding == "srgb" && !t.pipeline.is_plain() {
        return Err(bad(CMD, "encoding must be working-linear, or srgb in an SDR Rec.709 working sequence"));
    }
    let mut report = Vec::new();
    let mut effects = Vec::new();
    for (index, source) in s.project.color_workflow.shared_effects(&t.item).iter().chain(t.item.effects.iter()).enumerate() {
        let mut effect = source.clone();
        if sample {
            for param in effect.params.values_mut() {
                param.value = param.value_at(mt);
                param.keyframes.clear();
            }
        }
        let reason = unsupported(&effect, sample);
        report.push(json!({"index":index,"effect":effect.effect,"enabled":effect.enabled,"included":effect.enabled && reason.is_none(),"reason":reason}));
        if effect.enabled && reason.is_none() {
            effects.push(effect);
        }
    }
    let incomplete = report.iter().any(|r| {
        r["enabled"] == true && r["reason"].is_string() && !filmcraft_project::find_effect(r["effect"].as_str().unwrap_or("")).is_some_and(|d| d.intrinsic)
    });
    let allowed = flag(p, "allowPartial", false, CMD)?;
    let metadata = json!({"revision":s.revision,"clip":t.clip.0,"sequence":t.sequence.0,"frame":s.project.sequence(t.sequence).map(|q|q.settings.frame_rate.frame_at(time)),"time":time,"pipeline":t.pipeline,"encoding":encoding,"primaries":format!("{:?}",t.pipeline.working_gamut()),"domainMin":min,"domainMax":max,"size":size,"effects":report,"partial":incomplete,"gridSamples":size*size*size,"interpolation":"tetrahedral","hdrLinearUnits":"1.0 = 203 cd/m2 in HDR; relative linear SDR otherwise"});
    if incomplete && !allowed {
        return Ok(
            json!({"status":"needs_explicit_partial_consent","report":metadata,"message":"A .cube cannot represent these controls. Inspect effects, then choose allowPartial:true to bake only included effects; no file was written."}),
        );
    }
    let count = size * size * size;
    let mut image = filmcraft_render::Image::new(count, 1);
    if image.px.len() != count * 4 {
        return Err(bad(CMD, "could not allocate LUT grid"));
    }
    for (index, pixel) in image.px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let coord = [index % size, (index / size) % size, index / (size * size)];
        for ch in 0..3 {
            let v = min[ch] + (max[ch] - min[ch]) * (coord[ch] as f32 / (size - 1) as f32);
            pixel[ch] = if encoding == "srgb" { filmcraft_color::srgb_to_linear(v) } else { v };
        }
        pixel[3] = 1.0;
    }
    let cx = filmcraft_render::effects::FxCtx {
        t: mt,
        px_scale: 1.0,
        seconds: 0.0,
        timecode: "",
        clip_name: "",
        project: Some(&s.project),
        env: None,
        frame_rate: s.project.sequence(t.sequence).map_or(24.0, |q| q.settings.frame_rate.as_f64()),
        working: t.pipeline.working,
    };
    for effect in &effects {
        filmcraft_render::effects::apply(&mut image, effect, &cx);
    }
    let mut data = Vec::with_capacity(count);
    for pixel in image.px.as_chunks::<4>().0 {
        let rgb = [pixel[0], pixel[1], pixel[2]].map(|v| if encoding == "srgb" { filmcraft_color::linear_to_srgb(v) } else { v });
        if !rgb.iter().all(|v| v.is_finite()) {
            return Err(bad(CMD, "grade produced a nonfinite LUT value"));
        }
        data.push(rgb);
    }
    let title = p.get("title").and_then(Value::as_str).unwrap_or("FilmCraft baked color stack");
    if title.len() > 256 || title.chars().any(char::is_control) {
        return Err(bad(CMD, "invalid LUT title"));
    }
    let lut = filmcraft_color::Lut3d { title: title.into(), size, domain_min: min, domain_max: max, data };
    let text = format!("# FilmCraft metadata {}\n{}", serde_json::to_string(&metadata).map_err(|e| bad(CMD, e.to_string()))?, lut.to_cube());
    output(s, p, CMD, text, json!({"status":"baked","report":metadata}))
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    type Run = fn(&mut Session, &Value) -> Result<Value>;
    [("grade.cdl.inspect","Inspect ASC CDL",r#"{"xml":str|"path":str}"#,false,inspect as Run),
    ("grade.cdl.import","Import ASC CDL",r#"{"xml":str|"path":str,"correction":str?,"clip":id,"sequence":id?,"effectInstance":"asc_cdl:0"|"new","expectedRevision":n,"replaceKeyframes":bool=false}"#,true,import),
    ("grade.cdl.export","Export ASC CDL",r#"{"clip":id,"sequence":id?,"effectInstance":"asc_cdl:0","id":str?,"description":str?,"sampleAnimation":bool=false,"frame":n?,"path":str?}"#,false,export),
    ("grade.bakeLut","Bake Color Stack LUT",r#"{"clip":id,"sequence":id?,"size":n=33,"frame":n?,"encoding":"working-linear"|"srgb","domainMin":[r,g,b]?,"domainMax":[r,g,b]?,"sampleAnimation":bool=false,"allowPartial":bool=false,"path":str?}"#,false,bake)]
    .into_iter().map(|(id,label,params,journal,run)|CommandSpec{id,label,menu:&[],shortcut:None,params,enabled:always,journal,run}).collect()
}
