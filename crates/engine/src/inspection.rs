//! Snapshot-based visual inspection, shared by MCP, the desktop control channel and the CLI.
//! Requests never seek the transport or capture a window. Source renders always use originals;
//! metadata names the snapshot and the exact display transform represented by the PNG.

use std::sync::Arc;

use base64::Engine as _;
use filmcraft_project::{ClipId, ItemId, ItemKind, Project, SequenceSettings};
use filmcraft_render::{Image, SourceProvider};
use filmcraft_time::{FrameRate, Tick, parse_timecode};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad};
use crate::{EditorState, Result, Session};

pub(crate) const MAX_FRAMES: usize = 24;
const MAX_SIDE: u32 = 8192;
const MAX_TOTAL_PIXELS: u64 = 33_554_432;
const MAX_PNG_BYTES: usize = 48 * 1024 * 1024;

/// A prepared immutable subject. Isolated clips retain their intrinsic attributes, opacity,
/// masks and standard effect stack. Other tracks remain available to effect dependencies but
/// are not composited into the requested picture.
pub struct Subject {
    pub project: Arc<Project>,
    pub revision: u64,
    pub sequence: Option<ItemId>,
    pub clip: Option<ClipId>,
    pub item: Option<ItemId>,
    pub rate: FrameRate,
    pub settings: SequenceSettings,
    pub default_time: Tick,
    pub first_time: Tick,
    pub end_time: Option<Tick>,
    pub effects: bool,
    pub captions: bool,
    pub plan: Option<u64>,
    pub before: bool,
    pub source_color: Option<Value>,
}

fn optional_id(p: &Value, key: &str) -> Result<Option<u64>> {
    p.get(key).map(|v| v.as_u64().ok_or_else(|| bad("render.frames", format!("`{key}` must be an unsigned integer ID")))).transpose()
}

fn optional_bool(p: &Value, key: &str, default: bool) -> Result<bool> {
    p.get(key).map(|v| v.as_bool().ok_or_else(|| bad("render.frames", format!("`{key}` must be a boolean")))).transpose().map(|v| v.unwrap_or(default))
}

/// Resolve a single frozen project before any rendering begins. The renderer below never
/// accesses the live project again.
fn snapshot(s: &Session, p: &Value, before: bool) -> Result<(Arc<Project>, EditorState, u64)> {
    if let Some(plan) = optional_id(p, "plan")? {
        return s.agent_plan_snapshot(plan, before);
    }
    Ok((s.project.clone(), s.state.clone(), s.revision))
}

pub fn subject(s: &Session, p: &Value, before: bool) -> Result<Subject> {
    if !p.is_object() {
        return Err(bad("render.frames", "parameters must be an object"));
    }
    let (project, state, revision) = snapshot(s, p, before)?;
    if let Some(requested) = optional_id(p, "revision")?
        && requested != revision
    {
        return Err(bad("render.frames", format!("snapshot revision is {revision}, requested {requested}")));
    }
    let effects = match p.get("effects").and_then(Value::as_str).unwrap_or("post") {
        "post" => true,
        "pre" => false,
        _ => return Err(bad("render.frames", "`effects` must be pre or post (standard effects; intrinsic placement and opacity are retained)")),
    };
    if p.get("effects").is_some_and(|v| !v.is_string()) {
        return Err(bad("render.frames", "`effects` must be pre or post"));
    }
    let target = match p.get("target") {
        Some(v) => v.as_str().ok_or_else(|| bad("render.frames", "`target` must be sequence, clip or source"))?,
        None if p.get("clip").is_some() => "clip",
        None if p.get("item").is_some() => "source",
        None => "sequence",
    };
    let plan = optional_id(p, "plan")?;
    let source_id = optional_id(p, "item")?.map(ItemId);
    let clip = optional_id(p, "clip")?.map(ClipId);
    let explicit_sequence = optional_id(p, "sequence")?.map(ItemId);
    if target == "source" {
        if clip.is_some() || explicit_sequence.is_some() {
            return Err(bad("render.frames", "source target takes item, without clip or sequence"));
        }
        let id = source_id.ok_or_else(|| bad("render.frames", "source target requires `item`"))?;
        let (_, media, _) = project.resolve_media(id).ok_or_else(|| bad("render.frames", "source item must resolve to media"))?;
        let video = media.info.video.as_ref().ok_or_else(|| bad("render.frames", "source item has no video"))?;
        let mut settings = crate::commands::default_seq_settings_for(media);
        let input_space = media.interpret.color_space.unwrap_or_else(|| filmcraft_color::ColorSpace::from_info(&video.color));
        if input_space.is_hdr() {
            settings.color = filmcraft_color::ColorPipeline { working: filmcraft_color::WorkingSpace::Rec2100Pq, wide_gamut: false, auto_tone_map: false };
        }
        settings.validate().map_err(|e| bad("render.frames", e))?;
        let first_time = match project.item(id).map(|i| &i.kind) {
            Some(ItemKind::Subclip { range, .. }) => range.start,
            _ => Tick::ZERO,
        };
        let end_time = if media.info.kind == filmcraft_media::MediaKind::Still {
            None
        } else {
            Some(match project.item(id).map(|i| &i.kind) {
                Some(ItemKind::Subclip { range, .. }) => range.end(),
                _ => media.duration(),
            })
        };
        let source_color = Some(
            json!({"detected": video.color, "interpretation": media.interpret.color_space, "input": "media metadata with project interpretation override"}),
        );
        return Ok(Subject {
            project,
            revision,
            sequence: None,
            clip: None,
            item: Some(id),
            rate: settings.frame_rate,
            settings,
            default_time: first_time,
            first_time,
            end_time,
            effects,
            captions: false,
            plan,
            before,
            source_color,
        });
    }
    if !matches!(target, "sequence" | "composite" | "clip") || source_id.is_some() {
        return Err(bad("render.frames", "target must be sequence/composite, clip or source; item is only valid for source"));
    }
    let sequence = explicit_sequence.or(state.active_sequence).ok_or(crate::EngineError::NoSequence)?;
    let seq = project.sequence(sequence).ok_or_else(|| bad("render.frames", "sequence ID does not name a sequence"))?;
    seq.settings.validate().map_err(|e| bad("render.frames", e))?;
    let settings = seq.settings.clone();
    let mut default_time = state.playheads.get(&sequence).copied().unwrap_or(Tick::ZERO);
    let mut first_time = Tick::ZERO;
    let mut end_time = None;
    if target == "clip" {
        let id = clip.ok_or_else(|| bad("render.frames", "clip target requires `clip`"))?;
        let (track, item) = seq.find_item(id).ok_or_else(|| bad("render.frames", "clip is absent from the requested sequence"))?;
        if !seq.video_tracks.iter().any(|t| t.id == track) {
            return Err(bad("render.frames", "isolated target must be a video clip"));
        }
        first_time = item.start;
        default_time = item.start;
        end_time = Some(item.end());
        if project.item(item.item).is_some_and(|i| matches!(i.kind, ItemKind::AdjustmentLayer { .. })) {
            return Err(bad("render.frames", "an adjustment layer has no isolated picture; request its sequence composite"));
        }
    } else if clip.is_some() {
        return Err(bad("render.frames", "clip ID requires target clip"));
    }
    Ok(Subject {
        project,
        revision,
        sequence: Some(sequence),
        clip,
        item: None,
        rate: settings.frame_rate,
        settings,
        default_time,
        first_time,
        end_time,
        effects,
        captions: target != "clip" && optional_bool(p, "captions", true)?,
        plan,
        before,
        source_color: None,
    })
}

impl Subject {
    pub fn hdr(&self) -> bool {
        self.settings.color.working.is_hdr()
    }

    pub fn metadata(&self, t: Tick, working: bool) -> Value {
        let frame = self.rate.frame_at(t);
        let timecode_frame = frame.saturating_add(self.sequence.and_then(|id| self.project.sequence(id)).map_or(0, |q| q.start_timecode));
        let source = self
            .sequence
            .and_then(|sequence| self.clip.and_then(|clip| self.project.sequence(sequence)?.find_item(clip).map(|(_, i)| (i.item, i.source_time_at(t)))))
            .or_else(|| self.item.map(|item| (item, t)));
        let source_metadata = source.map(|(item, time)| {
            let rate = self
                .project
                .resolve_media(item)
                .map(|(_, m, _)| m.frame_rate())
                .or_else(|| self.project.sequence(item).map(|q| q.settings.frame_rate))
                .unwrap_or(self.rate);
            json!({"item":item.0,"time":time.0,"frame":rate.frame_at(time),"frameRate":rate,"timeSemantics":"requested media position; decoder PTS is not reported"})
        });
        json!({
            "revision": self.revision,
            "plan": self.plan,
            "snapshot": if self.plan.is_some() { if self.before { "before" } else { "staged" } } else { "live" },
            "target": if self.item.is_some() { "source" } else if self.clip.is_some() { "clip" } else { "sequence" },
            "sequence": self.sequence.map(|v| v.0),
            "clip": self.clip.map(|v| v.0),
            "item": self.item.map(|v| v.0),
            "source": source_metadata,
            "time": t.0,
            "frame": frame,
            "timecode": filmcraft_time::format_timecode_frames(timecode_frame, self.rate, self.settings.drop_frame),
            "frameRate": self.rate,
            "pixelAspect": self.settings.par,
            "effects": if self.effects { "post" } else { "pre" },
            "effectsBoundary": "standard effects; intrinsic placement, opacity and masks retained",
            "captions": self.captions,
            "sourceColor": self.source_color,
            "colorTransform": {
                "pipeline": self.settings.color,
                "workingSpace": self.settings.color.working.label(),
                "output": if working { "linear working-space RGBA" } else { "sRGB RGBA8 over black" },
                "displayTransform": if working { "none" } else { "FilmCraft CPU working-to-SDR monitor transform, sRGB transfer" },
                "originals": true,
            },
        })
    }

    pub fn validate_time(&self, t: Tick) -> Result<Tick> {
        if t < Tick::ZERO || t > Tick::MAX || t < self.first_time || self.end_time.is_some_and(|end| t >= end) {
            return Err(bad("render.frames", "requested frame is outside the subject range"));
        }
        let snapped = self.rate.snap(t);
        if snapped < self.first_time {
            return Err(bad("render.frames", "requested time snaps before the subject range"));
        }
        Ok(snapped)
    }

    pub fn times(&self, p: &Value) -> Result<Vec<Tick>> {
        let list_frames = p.get("frames");
        let list_times = p.get("times");
        let single = ["frame", "time", "seconds", "timecode"].iter().filter(|k| p.get(**k).is_some()).count();
        if usize::from(list_frames.is_some()) + usize::from(list_times.is_some()) + single > 1 {
            return Err(bad("render.frames", "choose one of frames, times, frame, time, seconds or timecode"));
        }
        let raw = if let Some(values) = list_frames.or(list_times) {
            let values = values.as_array().ok_or_else(|| bad("render.frames", "frames/times must be arrays"))?;
            if values.is_empty() || values.len() > MAX_FRAMES {
                return Err(bad("render.frames", format!("request 1 to {MAX_FRAMES} frames")));
            }
            values.iter().map(|v| self.parse_position(v, list_frames.is_some())).collect::<Result<Vec<_>>>()?
        } else if let Some(v) = p.get("frame") {
            vec![self.parse_position(v, true)?]
        } else if let Some(v) = p.get("time") {
            vec![self.parse_position(v, false)?]
        } else if let Some(v) = p.get("seconds") {
            let seconds = v
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= Tick::MAX.seconds())
                .ok_or_else(|| bad("render.frames", "seconds must be finite, nonnegative and representable"))?;
            vec![Tick::from_seconds_f64(seconds)]
        } else if let Some(v) = p.get("timecode") {
            let text = v.as_str().ok_or_else(|| bad("render.frames", "timecode must be a string"))?;
            let f = parse_timecode(text, self.rate, self.settings.drop_frame, self.rate.frame_at(self.default_time))
                .map_err(|e| bad("render.frames", e.to_string()))?;
            vec![self.parse_position(&json!(f), true)?]
        } else {
            vec![self.default_time]
        };
        raw.into_iter().map(|t| self.validate_time(t)).collect()
    }

    fn parse_position(&self, v: &Value, frame: bool) -> Result<Tick> {
        let n = v.as_i64().filter(|v| *v >= 0).ok_or_else(|| bad("render.frames", "frame/time must be a nonnegative integer"))?;
        if !frame {
            return Ok(Tick(n));
        }
        let t = self.rate.tick_of(n);
        if t > Tick::MAX || self.rate.frame_at(t) != n {
            return Err(bad("render.frames", "frame is outside the representable tick range"));
        }
        Ok(t)
    }

    pub fn scale(&self, p: &Value, count: usize) -> Result<f32> {
        let max = p
            .get("maxSide")
            .map(|v| v.as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(|| bad("render.frames", "maxSide must be an unsigned integer")))
            .transpose()?
            .unwrap_or(960);
        if max > MAX_SIDE {
            return Err(bad("render.frames", format!("maxSide must not exceed {MAX_SIDE}; 0 requests full resolution")));
        }
        let side = self.settings.width.max(self.settings.height);
        let scale = if max == 0 || max >= side { 1.0 } else { max as f32 / side as f32 };
        self.validate_scale(scale, count)?;
        Ok(scale)
    }

    pub fn validate_scale(&self, scale: f32, count: usize) -> Result<()> {
        if !scale.is_finite() || scale <= 0.0 || scale > 1.0 {
            return Err(bad("render.frames", "scale must be finite and in (0,1]"));
        }
        let w = (self.settings.width as f32 * scale).round().max(1.0) as u64;
        let h = (self.settings.height as f32 * scale).round().max(1.0) as u64;
        let count = u64::try_from(count).map_err(|_| bad("render.frames", "too many frames"))?;
        if w > u64::from(MAX_SIDE) || h > u64::from(MAX_SIDE) || w.checked_mul(h).and_then(|v| v.checked_mul(count)).is_none_or(|v| v > MAX_TOTAL_PIXELS) {
            return Err(bad("render.frames", "requested frames exceed the 8192 side / 32-megapixel total budget; reduce maxSide or frame count"));
        }
        Ok(())
    }

    /// Render a validated, frame-snapped position from this snapshot. `working` retains HDR
    /// working light for scopes; false applies the same SDR display transform as the monitor.
    pub fn render(&self, s: &Session, t: Tick, scale: f32, working: bool) -> Result<(Image, Value)> {
        let t = self.validate_time(t)?;
        self.validate_scale(scale, 1)?;
        let provider = s.media.full_res_provider(self.project.clone(), s.services.clone()).own_sources().recording();
        let image = if let Some(sequence) = self.sequence {
            let options =
                filmcraft_render::RenderOptions { scale, effects: self.effects, captions: self.captions, working_output: working, ..Default::default() };
            if let Some(clip) = self.clip {
                filmcraft_render::render_isolated_clip(&self.project, sequence, clip, t, options, &provider)
                    .ok_or_else(|| bad("render.frames", "isolated clip frame could not be rendered"))?
            } else {
                filmcraft_render::render_sequence(&self.project, sequence, t, options, &provider)
            }
        } else if let Some(item) = self.item {
            let source = provider.source(item).ok_or_else(|| bad("render.frames", "source is unavailable"))?;
            let frame = source.video_frame(filmcraft_media::FrameRequest { time: t, scale })?;
            let target_w = (self.settings.width as f32 * scale).round().max(1.0) as usize;
            let target_h = (self.settings.height as f32 * scale).round().max(1.0) as usize;
            let mut decimation = 1usize;
            while decimation < 32768 && frame.width as usize / decimation / 2 >= target_w && frame.height as usize / decimation / 2 >= target_h {
                decimation = decimation.saturating_mul(2);
            }
            let mut img = filmcraft_render::colorman::decode(&self.project, item, &frame, decimation, &self.settings.color);
            if !working {
                filmcraft_render::colorman::to_display(&mut img, &self.settings.color);
            }
            if (img.w, img.h) != (target_w, target_h) && img.w > 0 && img.h > 0 {
                img = img.transformed(target_w, target_h, &filmcraft_geom::Affine::scale(target_w as f64 / img.w as f64, target_h as f64 / img.h as f64));
            }
            img
        } else {
            return Err(bad("render.frames", "render subject is missing"));
        };
        let pixels = image.w.checked_mul(image.h).and_then(|v| v.checked_mul(4));
        if image.w == 0 || image.h == 0 || pixels != Some(image.px.len()) {
            return Err(bad("render.frames", "renderer could not allocate a valid frame"));
        }
        let offline = serde_json::to_value(provider.offline_used()).map_err(|e| bad("render.frames", e.to_string()))?;
        Ok((image, offline))
    }
}

pub(crate) fn png_image(image: &Image) -> Result<Vec<u8>> {
    let w = u32::try_from(image.w).map_err(|_| bad("render.frames", "image width overflow"))?;
    let h = u32::try_from(image.h).map_err(|_| bad("render.frames", "image height overflow"))?;
    let rgba = image::RgbaImage::from_raw(w, h, image.over_black_rgba8()).ok_or_else(|| bad("render.frames", "invalid RGBA image"))?;
    let mut output = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut output, image::ImageFormat::Png).map_err(|e| bad("render.frames", format!("PNG encoding failed: {e}")))?;
    Ok(output.into_inner())
}

fn encoded_image(image: &Image, used: &mut usize) -> Result<Value> {
    let png = png_image(image)?;
    *used = used
        .checked_add(png.len())
        .filter(|v| *v <= MAX_PNG_BYTES)
        .ok_or_else(|| bad("render.frames", "PNG response exceeds 48 MiB; request fewer or smaller frames"))?;
    Ok(json!({"mimeType":"image/png", "width":image.w,"height":image.h,"base64":base64::engine::general_purpose::STANDARD.encode(png)}))
}

fn contact_sheet(images: &[(Image, String)]) -> Result<(Image, Vec<Value>)> {
    let w = images.iter().map(|(i, _)| i.w).max().ok_or_else(|| bad("render.frames", "contact sheet has no frames"))?;
    let h = images.iter().map(|(i, _)| i.h).max().ok_or_else(|| bad("render.frames", "contact sheet has no frames"))?;
    let columns = images.len().min(3);
    let rows = images.len().div_ceil(columns);
    let cell_h = h.checked_add(30).ok_or_else(|| bad("render.frames", "contact sheet height overflow"))?;
    let width = w.checked_mul(columns).ok_or_else(|| bad("render.frames", "contact sheet width overflow"))?;
    let height = cell_h.checked_mul(rows).ok_or_else(|| bad("render.frames", "contact sheet height overflow"))?;
    if width.checked_mul(height).is_none_or(|n| n > MAX_TOTAL_PIXELS as usize) || width > 16384 || height > 16384 {
        return Err(bad("render.frames", "contact sheet exceeds pixel budget; reduce maxSide"));
    }
    let mut sheet = Image::filled(width, height, [0.02, 0.02, 0.02, 1.0]);
    if sheet.px.is_empty() {
        return Err(bad("render.frames", "could not allocate contact sheet"));
    }
    let mut cells = Vec::new();
    for (index, (img, label)) in images.iter().enumerate() {
        let x = (index % columns) * w;
        let y = (index / columns) * cell_h;
        for row in 0..img.h {
            let from = row * img.w * 4;
            let to = ((y + row) * width + x) * 4;
            let len = img.w * 4;
            let source = img.px.get(from..from + len).ok_or_else(|| bad("render.frames", "invalid contact sheet source"))?;
            let destination = sheet.px.get_mut(to..to + len).ok_or_else(|| bad("render.frames", "invalid contact sheet destination"))?;
            destination.copy_from_slice(source);
        }
        filmcraft_render::graphics::burn_text(
            &mut sheet,
            label,
            "JetBrains Mono",
            filmcraft_geom::Vec2::new((x + w / 2) as f64, (y + h + 15) as f64),
            14.0,
            0.0,
        );
        cells.push(json!({"index":index,"x":x,"y":y,"width":img.w,"height":img.h,"label":label}));
    }
    Ok((sheet, cells))
}

fn render_frames(s: &mut Session, p: &Value) -> Result<Value> {
    let after = subject(s, p, false)?;
    let comparison = optional_bool(p, "comparison", false)?;
    if comparison && after.plan.is_none() {
        return Err(bad("render.frames", "comparison requires a staged plan"));
    }
    let before = comparison.then(|| subject(s, p, true)).transpose()?;
    let times = after.times(p)?;
    let count = times.len().checked_mul(if comparison { 2 } else { 1 }).ok_or_else(|| bad("render.frames", "frame count overflow"))?;
    if count > MAX_FRAMES {
        return Err(bad("render.frames", format!("including comparisons, request at most {MAX_FRAMES} frames")));
    }
    let scale = after.scale(p, count)?;
    if let Some(before) = &before {
        before.validate_scale(scale, count)?;
    }
    let layout =
        p.get("layout").map(|v| v.as_str().ok_or_else(|| bad("render.frames", "layout must be stills or contactSheet"))).transpose()?.unwrap_or("stills");
    let sheet = match layout {
        "stills" => false,
        "contactSheet" => true,
        _ => return Err(bad("render.frames", "layout must be stills or contactSheet")),
    };
    let mut output = Vec::new();
    let mut images = Vec::new();
    let mut used = 0;
    for time in times {
        for subject in before.iter().chain(std::iter::once(&after)) {
            let actual_time = subject.validate_time(time)?;
            let (img, offline) = subject.render(s, actual_time, scale, false)?;
            let mut metadata = subject.metadata(actual_time, false);
            metadata["offlineMedia"] = offline;
            metadata["width"] = json!(img.w);
            metadata["height"] = json!(img.h);
            if sheet {
                let timecode = metadata.get("timecode").and_then(Value::as_str).unwrap_or("");
                let prefix = if comparison { if subject.before { "Before " } else { "After " } } else { "" };
                images.push((img, format!("{prefix}{timecode}")));
            } else {
                metadata["image"] = encoded_image(&img, &mut used)?;
            }
            output.push(metadata);
        }
    }
    let mut result = json!({"revision":after.revision,"plan":after.plan,"frames":output});
    if sheet {
        let (image, cells) = contact_sheet(&images)?;
        result["contactSheet"] = encoded_image(&image, &mut used)?;
        result["contactSheet"]["cells"] = json!(cells);
    }
    Ok(result)
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "render.frames",
        label: "Render exact snapshot frames",
        menu: &[],
        shortcut: None,
        params: r#"{"target":"sequence|clip|source"?,"sequence":id?,"clip":id?,"item":id?,"frames":[frame]?,"times":[ticks]?,"frame":n?,"time":ticks?,"maxSide":960,"effects":"pre|post"?,"captions":bool?,"plan":id?,"comparison":bool?,"layout":"stills|contactSheet"?,"revision":n?}"#,
        enabled: always,
        run: render_frames,
        journal: false,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Session {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s
    }

    #[test]
    fn exact_sequence_and_isolated_frames_are_read_only() {
        let mut s = demo();
        let sequence = s.state.active_sequence.unwrap();
        let item = s.active_sequence().unwrap().video_tracks[0].items[0].clone();
        let state = serde_json::to_value(&s.state).unwrap();
        let revision = s.revision;
        let project = s.project.clone();
        let rendered = s.execute("render.frames", json!({"sequence":sequence.0,"frames":[0,2],"maxSide":160})).unwrap();
        assert_eq!(rendered["revision"], revision);
        assert_eq!(rendered["frames"][1]["frame"], 2);
        assert_eq!(rendered["frames"][0]["sequence"], sequence.0);
        assert_eq!(rendered["frames"][0]["image"]["mimeType"], "image/png");
        let png = base64::engine::general_purpose::STANDARD.decode(rendered["frames"][0]["image"]["base64"].as_str().unwrap()).unwrap();
        assert_eq!(image::load_from_memory(&png).unwrap().width(), 160);
        let isolated = s.execute("render.frames", json!({"target":"clip","clip":item.id.0,"frame":0,"maxSide":160})).unwrap();
        assert_eq!(isolated["frames"][0]["clip"], item.id.0);
        assert_eq!(serde_json::to_value(&s.state).unwrap(), state);
        assert!(Arc::ptr_eq(&project, &s.project));
        assert_eq!(s.revision, revision);
    }

    #[test]
    fn source_and_timecoded_contact_sheet_render_originals() {
        let mut s = demo();
        let item = s.active_sequence().unwrap().video_tracks[0].items[0].item;
        let rendered = s.execute("render.frames", json!({"target":"source","item":item.0,"frames":[0,1,2],"maxSide":160,"layout":"contactSheet"})).unwrap();
        assert_eq!(rendered["frames"][0]["colorTransform"]["originals"], true);
        assert_eq!(rendered["contactSheet"]["cells"].as_array().unwrap().len(), 3);
        assert_eq!(rendered["contactSheet"]["cells"][1]["label"], "00:00:00:01");
        assert_eq!(rendered["contactSheet"]["width"], 480);
    }

    #[test]
    fn hostile_render_parameters_are_errors_without_mutation() {
        let mut s = demo();
        let revision = s.revision;
        for p in [
            json!({"frames":[]}),
            json!({"frames":vec![0;MAX_FRAMES+1]}),
            json!({"frame":i64::MAX}),
            json!({"frame":-1}),
            json!({"frame":0.1}),
            json!({"maxSide":u64::MAX}),
            json!({"maxSide":8193}),
            json!({"seconds":"oops"}),
            json!({"frames":[0],"time":0}),
            json!({"effects":true}),
            json!({"sequence":"1"}),
            json!({"revision":u64::MAX}),
            json!({"target":"unknown"}),
            json!({"target":"source","item":u64::MAX}),
            json!({"target":"clip","clip":u64::MAX}),
            json!({"layout":"bad"}),
            json!({"comparison":true}),
        ] {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("render.frames", p.clone())));
            assert!(result.is_ok(), "panicked: {p}");
            assert!(result.unwrap().is_err(), "accepted: {p}");
            assert_eq!(s.revision, revision);
        }
    }
    #[test]
    fn large_and_high_rate_sequences_keep_exact_frame_identity() {
        for (width, height, fps, frame) in [(7680, 4320, 60.0, 12345i64), (1920, 1080, 960.0, 12345i64)] {
            let mut s = Session::default();
            s.execute("file.newSequence", json!({"width":width,"height":height,"fps":fps})).unwrap();
            let subject = subject(&s, &json!({}), false).unwrap();
            assert!(subject.scale(&json!({"maxSide":0}), 1).is_ok(), "single full frame fits budget");
            let result = s.execute("render.frames", json!({"frame":frame,"maxSide":160})).unwrap();
            assert_eq!(result["frames"][0]["frame"], frame);
            assert_eq!(result["frames"][0]["frameRate"]["num"], fps as i64);
            assert_eq!(result["frames"][0]["frameRate"]["den"], 1);
            assert_eq!(result["frames"][0]["width"], 160);
            assert_eq!(result["frames"][0]["height"], 90);
            assert_eq!(s.playhead(), Tick::ZERO);
        }
    }
    #[test]
    fn staged_comparison_renders_frozen_before_and_after_without_applying() {
        let mut s = Session::default();
        let item = s.execute("file.newColorMatte", json!({"color":"#406080","seconds":2})).unwrap()["item"].as_u64().unwrap();
        s.execute("file.newSequenceFromClip", json!({"items":[item]})).unwrap();
        let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
        let revision = s.revision;
        let plan = s
            .execute(
                "agent.plan",
                json!({"steps":[
                    {"id":"effects.apply","params":{"clips":[clip.0],"effect":"lumetri"}},
                    {"id":"effects.setParam","params":{"clip":clip.0,"effect":"lumetri","param":"exposure","value":2.0}}
                ]}),
            )
            .unwrap()["plan"]
            .as_u64()
            .unwrap();
        let result = s.execute("render.frames", json!({"plan":plan,"comparison":true,"frame":0,"maxSide":160})).unwrap();
        assert_eq!(result["frames"].as_array().unwrap().len(), 2);
        assert_eq!(result["frames"][0]["snapshot"], "before");
        assert_eq!(result["frames"][1]["snapshot"], "staged");
        assert_eq!(result["frames"][0]["revision"], revision);
        assert_ne!(result["frames"][0]["image"]["base64"], result["frames"][1]["image"]["base64"]);
        let staged = s.execute("scopes.read", json!({"plan":plan,"target":"clip","clip":clip.0,"frame":0,"bins":false})).unwrap();
        let live = s.execute("scopes.read", json!({"target":"clip","clip":clip.0,"frame":0,"bins":false})).unwrap();
        assert!(staged["stats"]["y"]["mean"].as_f64().unwrap() > live["stats"]["y"]["mean"].as_f64().unwrap());
        assert_eq!(s.revision, revision);
        s.execute("clip.rename", json!({"clip":clip.0,"name":"Human edit"})).unwrap();
        let frozen = s.execute("render.frames", json!({"plan":plan,"comparison":true,"frame":0,"maxSide":160})).unwrap();
        assert_eq!(frozen, result, "a stale plan still previews its immutable snapshots");
        assert!(s.execute("agent.apply", json!({"plan":plan})).is_err());
    }
    #[test]
    fn comparison_metadata_uses_each_snapshots_actual_frame_grid() {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"width":160,"height":90,"fps":24})).unwrap();
        let plan = s.execute("agent.plan", json!({"steps":[{"id":"sequence.settings","params":{"fps":30}}]})).unwrap()["plan"].as_u64().unwrap();
        let result = s.execute("render.frames", json!({"plan":plan,"comparison":true,"frames":[1],"maxSide":160})).unwrap();
        assert_eq!(result["frames"][0]["frame"], 0);
        assert_eq!(result["frames"][0]["time"], 0);
        assert_eq!(result["frames"][1]["frame"], 1);
        assert_eq!(result["frames"][1]["time"], FrameRate::FPS_30.tick_of(1).0);
    }
}
