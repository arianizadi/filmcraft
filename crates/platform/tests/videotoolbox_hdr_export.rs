//! Native VideoToolbox Main 10 roundtrip; FFmpeg/ffprobe are external test oracles only.
#![cfg(target_os = "macos")]
use filmcraft_color::{ColorPipeline, WorkingSpace};
use filmcraft_export::{BitrateMode, ExportSettings, Format, Progress};
use filmcraft_media::{DemoScene, FrameRequest, Generator, MediaSource, generators::GeneratorSource};
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_render::SourceMap;
use filmcraft_time::{FrameRate, Tick, TimeRange};
use std::{process::Command, sync::Arc};

#[test]
fn pq_and_hlg_files_are_actually_ten_bit_hardware_exports() {
    filmcraft_platform::register();
    if !filmcraft_platform::hardware_encode::hevc_hdr_available() {
        eprintln!("SKIPPED: VideoToolbox Main 10 is unavailable");
        return;
    }
    let directory = std::env::temp_dir().join(format!("filmcraft-vt-hdr-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let ffprobe = filmcraft_testkit::ffprobe_or_skip("VideoToolbox HDR");
    let ffmpeg = filmcraft_testkit::ffmpeg_or_skip("VideoToolbox HDR");
    let rate = FrameRate::FPS_24;
    for (working, transfer, pq, extension) in [
        (WorkingSpace::Rec2100Pq, "smpte2084", true, "mp4"),
        (WorkingSpace::Rec2100Hlg, "arib-std-b67", false, "mp4"),
        (WorkingSpace::Rec2100Pq, "smpte2084", true, "mov"),
    ] {
        let (w, h, frames) = (1280, 720, 6);
        let source = Arc::new(GeneratorSource::new(Generator::Demo(DemoScene::OceanSunset), w, h, rate, rate.tick_of(frames)));
        let mut project = Project::new("HDR oracle");
        let item = project.add_item(
            "Ocean",
            Label::Iris,
            ItemKind::Media(Box::new(MediaClip {
                media: MediaRef::Generator(source.generator.clone()),
                info: source.info().clone(),
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
        let sequence = project.new_sequence(
            "HDR",
            SequenceSettings { width: w, height: h, frame_rate: rate, color: ColorPipeline { working, ..ColorPipeline::REC709 }, ..Default::default() },
            1,
            0,
            None,
        );
        let clip = project.make_track_item(item, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, rate.tick_of(frames)), rate).unwrap();
        project.sequence_mut(sequence).unwrap().video_tracks[0].items.push(clip);
        let mut sources = SourceMap::default();
        sources.0.insert(item, source);
        let path = directory.join(format!("{}.{extension}", working.id())).to_string_lossy().into_owned();
        let before = filmcraft_export::hw_encode_stats();
        let settings = ExportSettings {
            format: Format::Hevc,
            path: path.clone(),
            include_audio: false,
            bitrate_kbps: 10000,
            bitrate_mode: BitrateMode::Vbr1Pass,
            ..Default::default()
        };
        let project = Arc::new(project);
        let report = filmcraft_export::export(&project, sequence, &settings, &sources, &Progress::default()).unwrap();
        assert_eq!(report.frames, frames as u64);
        assert_eq!(filmcraft_export::hw_encode_stats().frames - before.frames, frames as u64);
        let bytes: Arc<[u8]> = std::fs::read(&path).unwrap().into();
        let reopened = filmcraft_codecs::open_bytes(&path, bytes.clone()).unwrap();
        let video = reopened.info().video.as_ref().unwrap();
        assert_eq!(video.color.transfer, if pq { filmcraft_color::Transfer::Pq } else { filmcraft_color::Transfer::Hlg });
        assert_eq!(video.color.primaries, filmcraft_color::Primaries::Bt2020);
        let file = filmcraft_isobmff::open(&bytes[..]).unwrap();
        let track = file.track_of_kind(filmcraft_isobmff::TrackKind::Video).unwrap();
        let entry = &file.tracks[track].entries[0];
        assert_eq!(entry.video.as_ref().unwrap().mastering_display.is_some(), pq);
        let reference_path = directory.join(format!("{}-reference.mov", working.id())).to_string_lossy().into_owned();
        filmcraft_export::export(
            &project,
            sequence,
            &ExportSettings { format: Format::ProRes, path: reference_path.clone(), include_audio: false, ..Default::default() },
            &sources,
            &Progress::default(),
        )
        .unwrap();
        let reference = filmcraft_codecs::open_bytes(&reference_path, std::fs::read(&reference_path).unwrap().into()).unwrap();
        let mut worst_psnr = 99.0f64;
        for index in 0..frames {
            let frame = reopened.video_frame(FrameRequest::full(rate.tick_of(index))).unwrap();
            let filmcraft_frame::PixelData::Yuv16 { planes, bits, .. } = &frame.data else { panic!("expected native 10-bit decode") };
            let reference_frame = reference.video_frame(FrameRequest::full(rate.tick_of(index))).unwrap();
            let filmcraft_frame::PixelData::Yuv16 { planes: ref_planes, bits: ref_bits, .. } = &reference_frame.data else {
                panic!("expected HDR ProRes reference");
            };
            assert!(*ref_bits >= 10);
            assert_eq!(planes[0].len(), ref_planes[0].len());
            let mse = planes[0].iter().zip(ref_planes[0].iter()).map(|(a, b)| (f64::from(*a) - f64::from(*b >> (*ref_bits - 10))).powi(2)).sum::<f64>()
                / planes[0].len() as f64;
            let psnr = if mse == 0.0 { 99.0 } else { 10.0 * (1023.0f64.powi(2) / mse).log10() };
            worst_psnr = worst_psnr.min(psnr);
            assert_eq!(*bits, 10);
            assert!(planes[0].iter().all(|c| *c < 1024));
            let mut levels = planes[0].to_vec();
            levels.sort_unstable();
            levels.dedup();
            assert!(levels.len() > 256, "actual picture needs more than 8 bits: {}", levels.len());
        }
        if let Some(ffprobe) = &ffprobe {
            let output = Command::new(ffprobe)
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=codec_name,profile,pix_fmt,color_transfer,color_primaries,color_space,color_range,nb_frames",
                    "-of",
                    "default=nw=1",
                    &path,
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            let text = String::from_utf8_lossy(&output.stdout);
            for expected in
                ["codec_name=hevc", "profile=Main 10", "pix_fmt=yuv420p10le", "color_primaries=bt2020", "color_space=bt2020nc", "color_range=tv", "nb_frames=6"]
            {
                assert!(text.contains(expected), "{expected}: {text}");
            }
            assert!(text.contains(&format!("color_transfer={transfer}")));
            if pq {
                let data = Command::new(ffprobe)
                    .args([
                        "-v",
                        "error",
                        "-select_streams",
                        "v:0",
                        "-read_intervals",
                        "%+#1",
                        "-show_frames",
                        "-show_entries",
                        "frame=side_data_list",
                        "-of",
                        "default=nw=1",
                        &path,
                    ])
                    .output()
                    .unwrap();
                assert!(String::from_utf8_lossy(&data.stdout).contains("Mastering display metadata"));
            }
        }
        if let Some(ffmpeg) = &ffmpeg {
            let output = Command::new(ffmpeg).args(["-v", "error", "-xerror", "-i", &path, "-f", "null", "-"]).output().unwrap();
            assert!(output.status.success() && output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        assert!(worst_psnr > 35.0, "{working:?}: {worst_psnr:.1}dB luma versus HDR ProRes reference");
        eprintln!("VERIFIED: {path}; worst luma PSNR versus HDR ProRes {worst_psnr:.1}dB");
    }
    std::fs::remove_dir_all(directory).unwrap();
}
