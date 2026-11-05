//! Shared VideoToolbox export fixtures used by the general integration suite and the isolated
//! rate-conversion/cancellation test. These helpers have no process-wide test state of their own.
#![cfg(target_os = "macos")]

use std::sync::Arc;

use filmcraft_export::{ExportSettings, Format, HardwareEncoding, VideoEncoder};
use filmcraft_isobmff::CodecConfig;
use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::{DemoScene, MediaSource};
use filmcraft_platform::hardware_encode::videotoolbox_encoder_factory;
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_render::SourceMap;
use filmcraft_time::{FrameRate, Tick, TimeRange};

pub const W: u32 = 640;
pub const H: u32 = 360;
pub const FPS: FrameRate = FrameRate::FPS_30;

pub fn hardware_settings() -> ExportSettings {
    ExportSettings { format: Format::H264, hardware_encoding: HardwareEncoding::Auto, bitrate_kbps: 8_000, keyframe_distance: Some(30), ..Default::default() }
}

/// The hardware encoder for the settings, or `None` (skip) when this machine has none.
pub fn hardware(s: &ExportSettings, w: u32, h: u32) -> Option<Box<dyn VideoEncoder>> {
    match videotoolbox_encoder_factory(s.format, w, h, FPS, s) {
        Some(Ok(e)) => Some(e),
        Some(Err(e)) => panic!("factory error: {e}"),
        None => {
            eprintln!("SKIPPED: no VideoToolbox hardware {} encoder for these settings", s.format.label());
            None
        }
    }
}

/// A project with an animated demo clip as the whole sequence.
pub fn project(frames: i64) -> (Arc<Project>, filmcraft_project::ItemId, SourceMap) {
    let mut p = Project::new("hw");
    let g = GeneratorSource::demo(DemoScene::Plasma);
    let info = g.info().clone();
    let item = p.add_item(
        &info.name.clone(),
        Label::Iris,
        ItemKind::Media(Box::new(MediaClip {
            media: MediaRef::Generator(g.generator.clone()),
            info,
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: None,
        })),
        None,
    );
    let seq = p.new_sequence("s", SequenceSettings { width: W, height: H, frame_rate: FPS, ..Default::default() }, 1, 1, None);
    let mut clip = p.make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, FPS.tick_of(frames)), FPS).unwrap();
    clip.scale_to_frame = true;
    p.sequence_mut(seq).unwrap().video_tracks[0].items.push(clip);
    let mut m = SourceMap::default();
    m.0.insert(item, Arc::new(g));
    (Arc::new(p), seq, m)
}

pub fn tmp(name: &str) -> String {
    let d = std::env::temp_dir().join(format!("fc-hw-encode-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name).to_string_lossy().to_string()
}

/// (sample count, decoded picture count, sync sample count) of an MP4's video track, decoded by our decoder.
pub fn inspect(path: &str) -> (usize, usize, usize) {
    let bytes = std::fs::read(path).unwrap();
    let file = filmcraft_isobmff::open(bytes.as_slice()).unwrap();
    let vt = file.track_of_kind(filmcraft_isobmff::TrackKind::Video).unwrap();
    let track = &file.tracks[vt];
    let entry = track.entries.first().expect("a sample entry");
    assert!(matches!(&entry.codec, CodecConfig::Avc(_) | CodecConfig::Hevc(_)), "neither AVC nor HEVC: {}", entry.codec.name());
    let mut dec = filmcraft_codecs::software_video_decoder(entry).expect("software decoder");
    let mut pictures = 0;
    for (i, s) in track.samples.iter().enumerate() {
        let data = file.read_sample(bytes.as_slice(), vt, i).unwrap();
        pictures += dec.decode(&data, s.pts).expect("decode").len();
    }
    pictures += dec.flush().len();
    (track.samples.len(), pictures, track.samples.iter().filter(|s| s.is_sync).count())
}
