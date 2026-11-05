//! Optional native resampling/export jobs with a fixed source snapshot and review artifacts.
//! This is FilmCraft's spatial resampler, not a generative detail-reconstruction model.
use crate::{
    EngineError, Result, Session,
    commands::{CommandSpec, always, bad},
};
use filmcraft_project::{ItemId, ParamValue, SequenceSettings, TrackKind};
use filmcraft_time::{Tick, TimeRange};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Cursor, path::Path, sync::Arc};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    item: u64,
    path: String,
    width: u32,
    height: u32,
    start: Option<i64>,
    end: Option<i64>,
    format: Option<String>,
    fit: Option<String>,
    #[serde(default)]
    wait: bool,
    #[serde(default = "yes")]
    audio: bool,
    #[serde(default)]
    allow_sdr: bool,
}
fn yes() -> bool {
    true
}
fn run(s: &mut Session, p: &Value) -> Result<Value> {
    crate::agent_plans::bounded_json(p, 65536)?;
    let p: Request = serde_json::from_value(p.clone()).map_err(|e| bad("media.upscale", e.to_string()))?;
    if p.path.is_empty() || p.path.len() > 8192 || p.path.contains('\0') {
        return Err(bad("media.upscale", "a valid output path is required"));
    }
    filmcraft_project::validate_frame_size(p.width, p.height).map_err(|e| bad("media.upscale", e))?;
    if p.width > 8192 || p.height > 8192 || u64::from(p.width) * u64::from(p.height) > 33_554_432 {
        return Err(bad("media.upscale", "output is limited to 8192 per side / 32 Mi pixels"));
    }
    let format = filmcraft_export::Format::from_name(p.format.as_deref().unwrap_or("prores"))
        .filter(|f| !f.is_image_sequence() && !f.is_audio_only())
        .ok_or_else(|| bad("media.upscale", "choose a supported video export format"))?;
    let (snapshot, sequence, mut provenance) = prepare_snapshot(s, &p)?;
    let preview_path = format!("{}.preview.png", p.path);
    let provenance_path = format!("{}.provenance.json", p.path);
    let mut settings = filmcraft_export::ExportSettings {
        format,
        path: p.path.clone(),
        include_audio: p.audio,
        hardware_encoding: filmcraft_export::HardwareEncoding::Auto,
        ..Default::default()
    };
    // Native managed outputs retain their exclusive reservation through finalization.
    if cfg!(target_arch = "wasm32") || s.services.export_in_memory() {
        return Err(bad("media.upscale", "native filesystem output is required"));
    }
    if format == filmcraft_export::Format::MxfOpAtom {
        return Err(bad("media.upscale", "choose MXF OP1a or another single-file video format; OP-Atom has separate audio outputs"));
    }
    if !filmcraft_export::available(format) {
        return Err(bad("media.upscale", "the chosen encoder is unavailable on this machine"));
    }
    let working = snapshot.sequence(sequence).ok_or(EngineError::NoSequence)?.settings.color.working;
    let signal = filmcraft_export::output_color_signal(working, &settings);
    let tone_map = working.is_hdr() && !signal.is_hdr();
    if tone_map && !p.allow_sdr {
        return Err(bad(
            "media.upscale",
            "the selected output encoder would tone-map HDR to Rec.709 SDR; choose ProRes/Main 10 HEVC or explicitly set allowSdr:true",
        ));
    }
    let output_space = if signal.is_hdr() { working.output_space() } else { filmcraft_color::ColorSpace::Rec709 };
    provenance["outputColorSpace"] = json!(output_space);
    provenance["toneMapApplied"] = json!(tone_map);
    provenance["allowSdr"] = json!(p.allow_sdr);
    for path in [&p.path, &preview_path, &provenance_path] {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Err(bad("media.upscale", format!("output already exists: {path}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(bad("media.upscale", format!("cannot inspect output {path}: {e}"))),
        }
        let mut artifact = settings.clone();
        artifact.path.clone_from(path);
        crate::export_safety::preflight(s, &snapshot, sequence, &artifact)?;
    }
    settings.output_file = Some(reserve_output(&p.path)?);
    let snapshot = Arc::new(snapshot);
    let job = crate::export_tools::spawn_export(s, snapshot.clone(), sequence, settings, "Native upscale".into(), p.wait)?;
    publish_artifacts(s, &p, snapshot, sequence, job, provenance, preview_path, provenance_path)
}

fn reserve_output(path: &str) -> Result<Arc<std::fs::File>> {
    if let Some(parent) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| bad("media.upscale", format!("cannot create output directory: {e}")))?;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map(Arc::new)
        .map_err(|e| bad("media.upscale", format!("cannot exclusively create output {path}: {e}")))
}

/// All geometry, source range and interpretation decisions use one immutable project snapshot.
fn prepare_snapshot(s: &Session, p: &Request) -> Result<(filmcraft_project::Project, ItemId, Value)> {
    let item = ItemId(p.item);
    let (_, media, subclip) = s.project.resolve_media(item).ok_or_else(|| bad("media.upscale", "item must be source media"))?;
    if media.offline {
        return Err(bad("media.upscale", "source is explicitly offline"));
    }
    if let filmcraft_project::MediaRef::File { path } = &media.media {
        let path = s.media.follow(path).map_err(|e| bad("media.upscale", format!("source path is unavailable: {e}")))?;
        s.services.file_size(&path).map_err(|e| EngineError::Other(e.to_string()))?;
    }
    let video = media.info.video.as_ref().ok_or_else(|| bad("media.upscale", "source has no video"))?;
    let allowed = subclip.unwrap_or_else(|| TimeRange::new(Tick::ZERO, media.duration()));
    let allowed_end = allowed
        .start
        .0
        .checked_add(allowed.duration.0)
        .map(Tick)
        .filter(|end| allowed.start >= Tick::ZERO && allowed.duration > Tick::ZERO && *end <= media.duration() && *end <= Tick::MAX)
        .ok_or_else(|| bad("media.upscale", "invalid source or subclip bounds"))?;
    let (start, end) = (p.start.map(Tick).unwrap_or(allowed.start), p.end.map(Tick).unwrap_or(allowed_end));
    if start < allowed.start || end <= start || end > allowed_end {
        return Err(bad("media.upscale", "source range must stay inside the selected media or subclip bounds"));
    }
    let duration = end.0.checked_sub(start.0).map(Tick).ok_or_else(|| bad("media.upscale", "range overflows"))?;
    let (source_w, source_h) = s.project.source_size(item).ok_or_else(|| bad("media.upscale", "cannot resolve source size"))?;
    if source_w == 0 || source_h == 0 {
        return Err(bad("media.upscale", "source dimensions are invalid"));
    }
    let fit = p.fit.as_deref().unwrap_or("contain");
    let (display_w, display_h) = s
        .project
        .conformed_source_size(item, (1, 1))
        .filter(|(w, h)| w.is_finite() && h.is_finite() && *w > 0.0 && *h > 0.0)
        .ok_or_else(|| bad("media.upscale", "source display dimensions are invalid"))?;
    let ratios = (f64::from(p.width) / display_w, f64::from(p.height) / display_h);
    let scale = match fit {
        "contain" => ratios.0.min(ratios.1),
        "cover" => ratios.0.max(ratios.1),
        _ => return Err(bad("media.upscale", "fit must be contain or cover")),
    } * 100.0;
    if !scale.is_finite() || scale > 10000.0 {
        return Err(bad("media.upscale", "requested scale exceeds Motion's 10000% limit"));
    }
    let source_space = media.interpret.color_space.unwrap_or_else(|| filmcraft_color::ColorSpace::from_info(&video.color));
    let working = match source_space {
        filmcraft_color::ColorSpace::Rec2100Pq => filmcraft_color::WorkingSpace::Rec2100Pq,
        filmcraft_color::ColorSpace::Rec2100Hlg => filmcraft_color::WorkingSpace::Rec2100Hlg,
        _ => filmcraft_color::WorkingSpace::Rec709,
    };
    let settings = SequenceSettings {
        width: p.width,
        height: p.height,
        frame_rate: media.frame_rate(),
        color: filmcraft_color::ColorPipeline { working, ..filmcraft_color::ColorPipeline::REC709 },
        ..Default::default()
    };
    settings.validate().map_err(|e| bad("media.upscale", e))?;
    let rate = settings.frame_rate;
    let mut snapshot = (*s.project).clone();
    let sequence = snapshot.new_sequence("Upscale job snapshot", settings, 1, usize::from(p.audio && media.info.has_audio()), None);
    let mut video_clip = snapshot
        .make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(start, duration), rate)
        .ok_or_else(|| bad("media.upscale", "cannot create source snapshot"))?;
    for effect in &mut video_clip.effects {
        filmcraft_project::resolve_auto_points(effect, (p.width, p.height), (source_w, source_h));
        if effect.effect == "motion"
            && let Some(param) = effect.params.get_mut("scale")
        {
            param.value = ParamValue::Float(scale);
        }
    }
    snapshot.sequence_mut(sequence).ok_or(EngineError::NoSequence)?.video_tracks.first_mut().ok_or(EngineError::NoSequence)?.items.push(video_clip);
    if p.audio && media.info.has_audio() {
        let audio = snapshot
            .make_track_item(item, TrackKind::Audio, Tick::ZERO, TimeRange::new(start, duration), rate)
            .ok_or_else(|| bad("media.upscale", "cannot create audio snapshot"))?;
        snapshot.sequence_mut(sequence).ok_or(EngineError::NoSequence)?.audio_tracks.first_mut().ok_or(EngineError::NoSequence)?.items.push(audio);
    }
    let provenance = json!({"schemaVersion":1,"sourceItem":item.0,"inputRevision":s.revision,"sourceIdentity":media.identity,"sourceStartTick":start.0,"sourceEndTick":end.0,"sourceSize":[source_w,source_h],"sourcePixelAspect":s.project.source_par(item),"sourceDisplaySize":[display_w,display_h],"sourceColorSpace":source_space,"outputSize":[p.width,p.height],"frameRate":rate,"workingSpace":working,"fit":fit,"motionScalePercent":scale,"method":"FilmCraft native spatial resampling and encoder","model":null,"originalPreserved":true,"limitations":"Resampling changes pixel dimensions; it does not recover missing scene detail.","output":p.path});
    Ok((snapshot, sequence, provenance))
}
#[allow(clippy::too_many_arguments)]
fn publish_artifacts(
    s: &Session,
    p: &Request,
    snapshot: Arc<filmcraft_project::Project>,
    sequence: ItemId,
    job: u64,
    provenance: Value,
    preview_path: String,
    provenance_path: String,
) -> Result<Value> {
    let mut warnings = Vec::new();
    if provenance["toneMapApplied"].as_bool() == Some(true) {
        warnings.push("HDR was tone-mapped to Rec.709 SDR with explicit allowSdr consent; the selected encoder does not preserve HDR".into());
    }
    let provider = s.media.full_res_provider(snapshot.clone(), s.services.clone()).own_sources();
    let (w, h, rgba) = filmcraft_render::render_sequence_rgba8(
        &snapshot,
        sequence,
        Tick::ZERO,
        filmcraft_render::RenderOptions { scale: (960.0 / p.width as f32).min(540.0 / p.height as f32).min(1.0), ..Default::default() },
        &provider,
    );
    let mut png = Cursor::new(Vec::new());
    let preview = image::write_buffer_with_format(&mut png, &rgba, w as u32, h as u32, image::ExtendedColorType::Rgba8, image::ImageFormat::Png)
        .map_err(|e| e.to_string())
        .and_then(|()| s.services.write_file_new(&preview_path, png.get_ref()).map_err(|e| e.to_string()));
    if let Err(e) = preview {
        warnings.push(format!("Preview unavailable: {e}"));
    }
    match serde_json::to_vec_pretty(&provenance) {
        Ok(bytes) => {
            if let Err(e) = s.services.write_file_new(&provenance_path, &bytes) {
                warnings.push(format!("Provenance write: {e}"));
            }
        }
        Err(e) => warnings.push(format!("Provenance encoding: {e}")),
    }
    Ok(
        json!({"job":job,"path":p.path,"previewPath":preview_path,"provenancePath":provenance_path,"inputRevision":s.revision,"originalPreserved":true,"outputColorSpace":provenance["outputColorSpace"],"toneMapApplied":provenance["toneMapApplied"],"warnings":warnings,"verify":"qc.export","poll":"jobs.get"}),
    )
}
pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "media.upscale",
        label: "Upscale a Source Snapshot",
        menu: &[],
        shortcut: None,
        params: r#"{"item":id,"path":str,"width":u32,"height":u32,"start":ticks?,"end":ticks?,"fit":"contain|cover"="contain","format":str="prores","audio":bool=true,"allowSdr":bool=false,"wait":bool=false}"#,
        enabled: always,
        journal: true,
        run,
    }]
}

#[cfg(test)]
mod tests;
