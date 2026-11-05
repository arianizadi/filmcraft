//! `scopes.read`: Lumetri Scopes as numbers (M8.9), so agents can check colour without looking.
//! It renders the active sequence at the playhead (or `time`), computes the scopes with
//! `filmcraft-scopes` and returns levels in percent (per channel and per column range), histogram
//! bins, and the densest vectorscope spots with their angle and saturation.

use filmcraft_scopes::{self as scopes, ColorSpace, ParadeType, Params, ScopeKind, Signal, WaveformType, summary};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::inspection::{Subject, subject};
use crate::{Result, Session};
use base64::Engine as _;

/// The scope signal of the active sequence at `t` (`scale` = render scale): R'G'B' code values
/// of the monitor picture, or PQ code values of the working-space render for HDR scopes.
pub fn scope_signal(s: &Session, t: Tick, scale: f32, hdr: bool) -> Option<Signal> {
    let seq = s.state.active_sequence?;
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    if hdr {
        let opts = filmcraft_render::RenderOptions { scale, working_output: true, ..Default::default() };
        let img = filmcraft_render::render_sequence(&s.project, seq, t, opts, &provider);
        return Some(Signal::from_rgba_f32_with(img.w, img.h, &img.px, scopes::MAX_W, scopes::MAX_H, scopes::linear_to_pq));
    }
    let opts = filmcraft_render::RenderOptions { scale, captions: true, ..Default::default() };
    let img = filmcraft_render::render_sequence(&s.project, seq, t, opts, &provider);
    Some(Signal::from_rgba8(img.w, img.h, &img.over_black_rgba8()))
}

fn levels_json(w: &scopes::Waveform, columns: usize) -> Value {
    let mut o = serde_json::Map::new();
    for g in &w.traces {
        o.insert(g.name.to_string(), json!(summary::trace_columns(w, g, columns)));
    }
    json!({"lo": (w.lo as f64 * 100.0).round(), "hi": (w.hi as f64 * 100.0).round(), "columns": columns, "traces": o})
}

fn vectorscope_json(v: &scopes::Vectorscope, peaks: usize) -> Value {
    json!({
        "samples": v.samples,
        "mean": [(v.mean[0] as f64 * 10_000.0).round() / 100.0, (v.mean[1] as f64 * 10_000.0).round() / 100.0],
        "meanAngleDeg": (scopes::angle_deg(v.mean[0], v.mean[1]) as f64 * 100.0).round() / 100.0,
        "peaks": summary::peaks(v, peaks, 0.0),
    })
}

/// Normalized ROI, interpreted in the rendered subject canvas before scope decimation.
#[derive(Clone, Copy)]
struct Roi {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Roi {
    fn parse(p: &Value) -> Result<Self> {
        let Some(roi) = p.get("roi") else { return Ok(Self { x: 0.0, y: 0.0, width: 1.0, height: 1.0 }) };
        if !roi.is_object() {
            return Err(bad("scopes.read", "roi must be {x,y,width,height} in normalized 0..1 canvas coordinates"));
        }
        let number =
            |key: &str| roi.get(key).and_then(Value::as_f64).filter(|v| v.is_finite()).ok_or_else(|| bad("scopes.read", format!("roi.{key} must be finite")));
        let r = Self { x: number("x")?, y: number("y")?, width: number("width")?, height: number("height")? };
        if r.x < 0.0 || r.y < 0.0 || r.width <= 0.0 || r.height <= 0.0 || r.x + r.width > 1.0 || r.y + r.height > 1.0 {
            return Err(bad("scopes.read", "roi must have positive area and fit inside the normalized 0..1 canvas"));
        }
        Ok(r)
    }
    fn json(self) -> Value {
        json!({"x":self.x,"y":self.y,"width":self.width,"height":self.height})
    }
}

struct ReadOptions {
    space: ColorSpace,
    params: Params,
    wt: WaveformType,
    pt: ParadeType,
    kinds: Vec<ScopeKind>,
    columns: usize,
    peaks: usize,
    bins: bool,
    images: bool,
}

fn bool_option(p: &Value, key: &str, default: bool) -> Result<bool> {
    p.get(key).map(|v| v.as_bool().ok_or_else(|| bad("scopes.read", format!("{key} must be a boolean")))).transpose().map(|v| v.unwrap_or(default))
}

impl ReadOptions {
    fn parse(p: &Value, hdr: bool) -> Result<Self> {
        let name = |key: &str| -> Result<Option<&str>> {
            p.get(key).map(|v| v.as_str().ok_or_else(|| bad("scopes.read", format!("{key} must be a string")))).transpose()
        };
        let space = name("colorSpace")?
            .map(|c| ColorSpace::from_name(c).ok_or_else(|| bad("scopes.read", format!("unknown colorSpace `{c}` (auto, 601, 709, 2100)"))))
            .transpose()?
            .unwrap_or(ColorSpace::Auto)
            .resolve(hdr);
        let wt = name("waveformType")?
            .map(|n| WaveformType::from_name(n).ok_or_else(|| bad("scopes.read", format!("unknown waveformType `{n}`"))))
            .transpose()?
            .unwrap_or(WaveformType::Rgb);
        let pt = name("paradeType")?
            .map(|n| ParadeType::from_name(n).ok_or_else(|| bad("scopes.read", format!("unknown paradeType `{n}`"))))
            .transpose()?
            .unwrap_or(ParadeType::Rgb);
        let kinds = match p.get("scopes") {
            Some(v) => {
                let a = v.as_array().ok_or_else(|| bad("scopes.read", "scopes must be an array"))?;
                if a.is_empty() || a.len() > ScopeKind::ALL.len() {
                    return Err(bad("scopes.read", "request between one and five distinct scopes"));
                }
                let mut kinds = Vec::new();
                for value in a {
                    let k = value.as_str().and_then(ScopeKind::from_name).ok_or_else(|| bad("scopes.read", format!("unknown scope {value}")))?;
                    if kinds.contains(&k) {
                        return Err(bad("scopes.read", "scope list contains duplicates"));
                    }
                    kinds.push(k);
                }
                kinds
            }
            None => ScopeKind::ALL.to_vec(),
        };
        let count = |key: &str, default: usize, max: usize| -> Result<usize> {
            let Some(v) = p.get(key) else { return Ok(default) };
            v.as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (1..=max).contains(n))
                .ok_or_else(|| bad("scopes.read", format!("{key} must be an integer in 1..={max}")))
        };
        Ok(Self {
            space,
            params: Params { matrix: space.matrix(), clamp: bool_option(p, "clamp", true)?, ..Default::default() },
            wt,
            pt,
            kinds,
            columns: count("columns", 8, 512)?,
            peaks: count("peaks", 8, 64)?,
            bins: bool_option(p, "bins", true)?,
            images: bool_option(p, "images", false)?,
        })
    }
}

/// Both readings and clipping metrics use this same sampled ROI. The float path keeps values
/// outside [0,1] for clipping; SDR scope bins retain the monitor's 8-bit signal for compatibility.
fn signal(image: &filmcraft_render::Image, subject: &Subject, roi: Roi, space: ColorSpace) -> Result<(Signal, Value)> {
    let x0 = ((roi.x * image.w as f64).floor() as usize).min(image.w);
    let y0 = ((roi.y * image.h as f64).floor() as usize).min(image.h);
    let x1 = (((roi.x + roi.width) * image.w as f64).ceil() as usize).min(image.w);
    let y1 = (((roi.y + roi.height) * image.h as f64).ceil() as usize).min(image.h);
    let (width, height) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if width == 0 || height == 0 {
        return Err(bad("scopes.read", "ROI contains no pixels at the requested render scale"));
    }
    let (sx, sy) = (width.div_ceil(scopes::MAX_W).max(1), height.div_ceil(scopes::MAX_H).max(1));
    let (w, h) = (width.div_ceil(sx), height.div_ceil(sy));
    let mut rgb = Vec::with_capacity(w * h);
    let (mut low, mut high, mut below, mut above, mut nonfinite) = ([0u64; 3], [0u64; 3], [0u64; 3], [0u64; 3], 0u64);
    let mut above_reference = 0u64;
    let hdr = space == ColorSpace::Rec2100;
    let transform = filmcraft_color::OutputTransform::new(&subject.settings.color, filmcraft_color::ColorSpace::Rec2100Pq);
    for y in (y0..y1).step_by(sy) {
        for x in (x0..x1).step_by(sx) {
            let index = (y * image.w + x) * 4;
            let px = image.px.get(index..index + 4).ok_or_else(|| bad("scopes.read", "rendered image has invalid storage"))?;
            let raw = if hdr {
                // Convert the working gamut to BT.2020 before PQ encoding. OutputTransform::encode
                // clamps legal code values, so overflow is counted from its linear target instead.
                let linear = transform.apply([px[0], px[1], px[2]]);
                if linear.iter().any(|v| *v > 1.0) {
                    above_reference = above_reference.saturating_add(1);
                }
                let max = 10_000.0 / filmcraft_color::REFERENCE_WHITE_NITS as f32;
                for (channel, value) in linear.iter().enumerate() {
                    if *value < 0.0 {
                        below[channel] = below[channel].saturating_add(1);
                    }
                    if *value > max {
                        above[channel] = above[channel].saturating_add(1);
                    }
                }
                scopes::linear_to_pq([linear[0], linear[1], linear[2], 1.0])
            } else {
                [filmcraft_color::linear_to_srgb(px[0]), filmcraft_color::linear_to_srgb(px[1]), filmcraft_color::linear_to_srgb(px[2])]
            };
            if raw.iter().any(|v| !v.is_finite()) || px.iter().any(|v| !v.is_finite()) {
                nonfinite = nonfinite.saturating_add(1);
            }
            let code = if hdr { raw } else { raw.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() / 255.0) };
            for (channel, value) in raw.iter().enumerate() {
                if code.get(channel).is_some_and(|v| *v <= 0.0) {
                    low[channel] = low[channel].saturating_add(1);
                }
                if code.get(channel).is_some_and(|v| *v >= 1.0) {
                    high[channel] = high[channel].saturating_add(1);
                }
                if !hdr && *value < 0.0 {
                    below[channel] = below[channel].saturating_add(1);
                }
                if !hdr && *value > 1.0 {
                    above[channel] = above[channel].saturating_add(1);
                }
            }
            rgb.push(code);
        }
    }
    let n = rgb.len() as f64;
    Ok((
        Signal { w, h, rgb },
        json!({"samples":w*h,"blackCode":low,"whiteCode":high,"belowRange":below,"aboveRange":above,
        "whiteCodePercent": high.map(|v| v as f64 / n * 100.0),"blackCodePercent":low.map(|v|v as f64/n*100.0),
        "aboveReferenceWhite":above_reference,"nonFinite":nonfinite,"channelOrder":["r","g","b"],
        "range":if hdr {"PQ 0..10000 cd/m2; reference white 203 cd/m2"} else {"sRGB monitor full scale 0..1 before quantization"},
        "sampledRoiPixels":[x0,y0,x1-x0,y1-y0],"sampling":"nearest samples, at most 480x270 after ROI crop"}),
    ))
}

fn tint(name: &str) -> [f32; 3] {
    match name {
        "R" | "Cr" => [1.0, 0.2, 0.2],
        "G" => [0.2, 1.0, 0.2],
        "B" | "Cb" | "C" => [0.2, 0.4, 1.0],
        _ => [1.0; 3],
    }
}

fn scope_image(computed: &scopes::Computed) -> Result<Value> {
    let (w, h, mut rgba) = match computed {
        scopes::Computed::Waveform(w) => {
            let layers: Vec<_> = w.traces.iter().map(|g| (g, tint(g.name))).collect();
            scopes::paint::grids(&layers, scopes::paint::waveform_reference(w.per_column), 1.0)
        }
        scopes::Computed::Parade(w) => {
            let cols = w.traces.first().map_or(0, |g| g.cols);
            let rows = w.traces.first().map_or(0, |g| g.rows);
            let width = cols.saturating_mul(w.traces.len());
            let mut rgba = vec![0u8; width * rows * 4];
            for (index, g) in w.traces.iter().enumerate() {
                let (_, _, pixels) = scopes::paint::grids(&[(g, tint(g.name))], scopes::paint::waveform_reference(w.per_column), 1.0);
                for row in 0..rows {
                    let source = pixels.get(row * cols * 4..(row + 1) * cols * 4).ok_or_else(|| bad("scopes.read", "invalid parade source"))?;
                    let offset = (row * width + index * cols) * 4;
                    let target = rgba.get_mut(offset..offset + cols * 4).ok_or_else(|| bad("scopes.read", "invalid parade destination"))?;
                    target.copy_from_slice(source);
                }
            }
            (width, rows, rgba)
        }
        scopes::Computed::Vectorscope(v) => scopes::paint::vectorscope(v, [1.0; 3], 1.0, true),
        scopes::Computed::Histogram(h) => {
            let (width, height) = (256usize, 128usize);
            let peak = h.r.iter().chain(&h.g).chain(&h.b).copied().max().unwrap_or(1).max(1) as f32;
            let mut rgba = vec![0u8; width * height * 4];
            for (channel, bins) in [&h.r, &h.g, &h.b].iter().enumerate() {
                for (x, value) in bins.iter().take(width).enumerate() {
                    let rows = ((*value as f32 / peak).sqrt() * height as f32).round() as usize;
                    for y in height.saturating_sub(rows)..height {
                        if let Some(byte) = rgba.get_mut((y * width + x) * 4 + channel) {
                            *byte = 220;
                        }
                    }
                }
            }
            (width, height, rgba)
        }
    };
    for pixel in rgba.as_chunks_mut::<4>().0 {
        if let Some(a) = pixel.get_mut(3) {
            *a = 255;
        }
    }
    let (w, h) =
        (u32::try_from(w).map_err(|_| bad("scopes.read", "scope width overflow"))?, u32::try_from(h).map_err(|_| bad("scopes.read", "scope height overflow"))?);
    let image = image::RgbaImage::from_raw(w, h, rgba).ok_or_else(|| bad("scopes.read", "invalid scope pixels"))?;
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).map_err(|e| bad("scopes.read", format!("scope PNG failed: {e}")))?;
    Ok(json!({"width":w,"height":h,"mimeType":"image/png","base64":base64::engine::general_purpose::STANDARD.encode(png.into_inner()),"plotOnly":true}))
}

fn reading(s: &Session, subject: &Subject, t: Tick, scale: f32, roi: Roi, options: &ReadOptions) -> Result<Value> {
    let hdr = options.space == ColorSpace::Rec2100;
    let (image, offline) = subject.render(s, t, scale, hdr)?;
    let (sig, clipping) = signal(&image, subject, roi, options.space)?;
    let mut out = subject.metadata(t, hdr);
    out["offlineMedia"] = offline;
    out["samples"] = json!([sig.w, sig.h]);
    out["roi"] = roi.json();
    out["colorSpace"] = json!(options.space.label());
    out["matrix"] = json!(format!("{:?}", options.space.matrix()));
    out["signal"] = json!(if hdr { "BT.2020 primaries, PQ code values" } else { "sRGB/Rec.709 monitor RGB; selected YCbCr analysis matrix" });
    out["clamp"] = json!(options.params.clamp);
    out["unit"] = json!(if hdr { "pq%" } else { "%" });
    out["stats"] = json!(summary::channel_stats(&sig, options.params.matrix));
    out["clipping"] = clipping;
    for k in &options.kinds {
        let computed = scopes::compute(*k, &sig, &options.params, options.wt, options.pt);
        let mut value = match &computed {
            scopes::Computed::Waveform(w) => {
                let mut v = levels_json(w, options.columns);
                v["type"] = json!(options.wt);
                v
            }
            scopes::Computed::Parade(w) => {
                let mut v = levels_json(w, options.columns);
                v["type"] = json!(options.pt);
                v
            }
            scopes::Computed::Histogram(h) => {
                let peak = |b: &[u32]| b.iter().enumerate().max_by_key(|(i, c)| (**c, usize::MAX - i)).map(|(i, _)| i).unwrap_or(0);
                let mut v =
                    json!({"samples":h.samples,"below":h.below,"above":h.above,"peakBin":{"r":peak(&h.r),"g":peak(&h.g),"b":peak(&h.b),"y":peak(&h.y)}});
                if options.bins {
                    v["r"] = json!(h.r);
                    v["g"] = json!(h.g);
                    v["b"] = json!(h.b);
                    v["y"] = json!(h.y);
                }
                v
            }
            scopes::Computed::Vectorscope(v) => vectorscope_json(v, options.peaks),
        };
        if options.images {
            value["image"] = scope_image(&computed)?;
        }
        out[k.name()] = value;
    }
    Ok(out)
}

fn deltas(current: &Value, reference: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for channel in ["r", "g", "b", "y"] {
        let mut values = serde_json::Map::new();
        for key in ["min", "max", "mean"] {
            if let Some((a, b)) = current
                .get("stats")
                .and_then(|v| v.get(channel))
                .and_then(|v| v.get(key))
                .and_then(Value::as_f64)
                .zip(reference.get("stats").and_then(|v| v.get(channel)).and_then(|v| v.get(key)).and_then(Value::as_f64))
            {
                values.insert(key.into(), json!(a - b));
            }
        }
        out.insert(channel.into(), Value::Object(values));
    }
    json!({"direction":"subject minus reference","unit":"percentage points","channels":out})
}

fn scope_scale(subject: &Subject, p: &Value, count: usize) -> Result<f32> {
    if let Some(v) = p.get("scale") {
        if p.get("maxSide").is_some() {
            return Err(bad("scopes.read", "choose scale or maxSide"));
        }
        let value = v.as_f64().filter(|v| v.is_finite() && *v > 0.0 && *v <= 1.0).ok_or_else(|| bad("scopes.read", "scale must be finite in (0,1]"))? as f32;
        subject.validate_scale(value, count)?;
        Ok(value)
    } else {
        subject.scale(p, count)
    }
}

fn scopes_read(s: &mut Session, p: &Value) -> Result<Value> {
    let subject = subject(s, p, false)?;
    let times = subject.times(p)?;
    let options = ReadOptions::parse(p, subject.hdr())?;
    let scale = scope_scale(&subject, p, times.len())?;
    let roi = Roi::parse(p)?;
    let reference = if let Some(reference) = p.get("reference") {
        if !reference.is_object() {
            return Err(bad("scopes.read", "reference must be a subject object"));
        }
        let mut reference = reference.clone();
        if reference.get("plan").is_none()
            && let Some(plan) = p.get("plan")
        {
            reference["plan"] = plan.clone();
        }
        let reference_subject = crate::inspection::subject(s, &reference, false)?;
        let positions = reference_subject.times(&reference)?;
        if positions.len() != 1 {
            return Err(bad("scopes.read", "reference must request exactly one frame"));
        }
        let t = positions.first().copied().ok_or_else(|| bad("scopes.read", "missing reference frame"))?;
        let reference_scale = scope_scale(&reference_subject, &reference, 1)?;
        Some(reading(s, &reference_subject, t, reference_scale, Roi::parse(&reference)?, &options)?)
    } else {
        None
    };
    let mut readings = Vec::new();
    for time in times {
        let mut value = reading(s, &subject, time, scale, roi, &options)?;
        if let Some(reference) = &reference {
            value["referenceDelta"] = deltas(&value, reference);
        }
        readings.push(value);
    }
    if p.get("frames").is_some() || p.get("times").is_some() {
        Ok(json!({"revision":subject.revision,"plan":subject.plan,"frames":readings,"reference":reference}))
    } else {
        let mut value = readings.into_iter().next().ok_or_else(|| bad("scopes.read", "no frame requested"))?;
        if let Some(reference) = reference {
            value["reference"] = reference;
        }
        Ok(value)
    }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "scopes.read",
        label: "Read Lumetri Scopes",
        menu: &[],
        shortcut: None,
        params: r#"{"target":"sequence|clip|source"?,"sequence":id?,"clip":id?,"item":id?,"frames":[frame]?,"times":[ticks]?,"effects":"pre|post"?,"roi":{"x":0..1,"y":0..1,"width":0..1,"height":0..1}?,"reference":subject?,"images":bool=false,"plan":id?,"scopes":["waveform","parade","histogram","vectorscopeYuv","vectorscopeHls"]?,"waveformType":"rgb|luma|yc|ycNoChroma"?,"paradeType":"rgb|yuv|rgbWhite"?,"colorSpace":"auto|601|709|2100"?,"clamp":bool=true,"columns":n=8,"peaks":n=8,"bins":bool=true,"scale":0.5,"time":ticks?|"frame"|"seconds"|"timecode"}"#,
        enabled: always,
        run: scopes_read,
        journal: false,
    }]
}
