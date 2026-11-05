use super::*;
use crate::media_test_util::{make_movie, session_with, tmp_dir};
use filmcraft_color::{ColorSpace, WorkingSpace};
use filmcraft_media::{DemoScene, FrameRequest, MediaSource};
use filmcraft_project::{ItemKind, Label, Project};
use filmcraft_time::FrameRate;

fn request(item: ItemId, width: u32, height: u32, fit: &str) -> Request {
    serde_json::from_value(json!({"item":item.0,"width":width,"height":height,"fit":fit,"path":"unused.mov"})).unwrap()
}
fn source() -> (Session, ItemId) {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":2,"width":720,"height":576})).unwrap()["item"].as_u64().unwrap();
    (s, ItemId(item))
}
fn scale(project: &Project, sequence: ItemId) -> f64 {
    let clip = &project.sequence(sequence).unwrap().video_tracks[0].items[0];
    match clip.effects.iter().find(|e| e.effect == "motion").unwrap().params["scale"].value {
        ParamValue::Float(value) => value,
        ref other => panic!("unexpected scale {other:?}"),
    }
}
#[test]
fn fitting_uses_display_pixels_and_interpret_footage_override() {
    let (mut s, item) = source();
    let media = Arc::make_mut(&mut s.project).item_mut(item).unwrap().as_media_mut().unwrap();
    media.info.video.as_mut().unwrap().par = (64, 45);
    for fit in ["contain", "cover"] {
        let (snapshot, sequence, provenance) = prepare_snapshot(&s, &request(item, 1280, 720, fit)).unwrap();
        assert!((scale(&snapshot, sequence) - 125.0).abs() < 1e-10, "{fit}: {provenance}");
        assert_eq!(provenance["sourceDisplaySize"], json!([1024.0, 576.0]));
    }
    Arc::make_mut(&mut s.project).item_mut(item).unwrap().as_media_mut().unwrap().interpret.par = Some((1, 1));
    let (snapshot, sequence, _) = prepare_snapshot(&s, &request(item, 1280, 720, "cover")).unwrap();
    assert!((scale(&snapshot, sequence) - 100.0 * 1280.0 / 720.0).abs() < 1e-10);
}
#[test]
fn working_space_follows_effective_source_color_and_subclip_limits() {
    let (mut s, item) = source();
    for (detected, override_space, expected) in [
        (filmcraft_color::Transfer::Bt709, Some(ColorSpace::Rec2100Pq), WorkingSpace::Rec2100Pq),
        (filmcraft_color::Transfer::Pq, Some(ColorSpace::Rec2100Hlg), WorkingSpace::Rec2100Hlg),
        (filmcraft_color::Transfer::Hlg, Some(ColorSpace::Rec709), WorkingSpace::Rec709),
        (filmcraft_color::Transfer::Hlg, None, WorkingSpace::Rec2100Hlg),
    ] {
        let media = Arc::make_mut(&mut s.project).item_mut(item).unwrap().as_media_mut().unwrap();
        media.info.video.as_mut().unwrap().color.transfer = detected;
        media.interpret.color_space = override_space;
        let (snapshot, sequence, _) = prepare_snapshot(&s, &request(item, 32, 18, "contain")).unwrap();
        assert_eq!(snapshot.sequence(sequence).unwrap().settings.color.working, expected);
    }
    let rate = s.project.resolve_media(item).unwrap().1.frame_rate();
    let range = TimeRange::new(rate.tick_of(10), rate.tick_of(5));
    let subclip = Arc::make_mut(&mut s.project).add_item("Selected take", Label::Iris, ItemKind::Subclip { parent: item, range, restrict_trims: false }, None);
    let mut p = request(subclip, 32, 18, "contain");
    let (snapshot, sequence, provenance) = prepare_snapshot(&s, &p).unwrap();
    let clip = &snapshot.sequence(sequence).unwrap().video_tracks[0].items[0];
    assert_eq!(clip.source_in, range.start);
    assert_eq!(clip.duration, range.duration);
    assert_eq!(provenance["sourceStartTick"], range.start.0);
    assert_eq!(provenance["sourceEndTick"], range.end().0);
    for (start, end) in [(Some(0), None), (None, Some(rate.tick_of(16).0)), (Some(i64::MIN), Some(i64::MAX)), (Some(range.end().0), None)] {
        p.start = start;
        p.end = end;
        assert!(prepare_snapshot(&s, &p).is_err());
    }
}
#[test]
fn hostile_requests_and_occupied_artifacts_leave_source_and_history_unchanged() {
    let (mut s, item) = source();
    let dir = tmp_dir("upscale-hostile");
    let path = dir.join("result.mov");
    let preview = format!("{}.preview.png", path.display());
    std::fs::write(&preview, b"unrelated preview sentinel").unwrap();
    let before = s.project.clone();
    let history = (s.history.undo.len(), s.history.redo.len());
    let jobs = s.jobs.len();
    for bad in [
        json!({"item":item.0,"path":path,"width":0,"height":18}),
        json!({"item":item.0,"path":path,"width":u32::MAX,"height":u32::MAX}),
        json!({"item":item.0,"path":path,"width":32,"height":18,"start":-1}),
        json!({"item":item.0,"path":path,"width":32,"height":18,"fit":"invalid"}),
        json!({"item":item.0,"path":path,"width":32,"height":18,"format":"mxf-opatom"}),
        json!({"item":item.0,"path":path,"width":32,"height":18}),
    ] {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut s, &bad))).unwrap().is_err(), "{bad}");
        assert_eq!(*s.project, *before);
        assert_eq!((s.history.undo.len(), s.history.redo.len()), history);
        assert_eq!(s.jobs.len(), jobs);
        assert!(!path.exists());
        assert_eq!(std::fs::read(&preview).unwrap(), b"unrelated preview sentinel");
    }
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn native_subclip_upscale_roundtrips_without_touching_original_or_project() {
    let dir = tmp_dir("upscale-roundtrip");
    let path = dir.join("original.mov");
    make_movie(&path, DemoScene::CityNight, 32, 18, 4);
    let original = std::fs::read(&path).unwrap();
    let (mut s, items, _) = session_with(&[&path]);
    let rate = FrameRate::FPS_24;
    let subclip = Arc::make_mut(&mut s.project).add_item(
        "Two frames",
        Label::Iris,
        ItemKind::Subclip { parent: items[0], range: TimeRange::new(rate.tick_of(1), rate.tick_of(2)), restrict_trims: false },
        None,
    );
    let before = s.project.clone();
    let history = (s.history.undo.len(), s.history.redo.len());
    let output = dir.join("enlarged.mov");
    let result = run(&mut s, &json!({"item":subclip.0,"path":output,"width":64,"height":36,"audio":false,"wait":true})).unwrap();
    let job = s.jobs.iter().find(|j| j.id == result["job"].as_u64().unwrap()).unwrap();
    let report = job.result.lock().unwrap().clone().unwrap().unwrap();
    assert_eq!(report.frames, 2);
    let source = filmcraft_codecs::Mp4Source::open("enlarged.mov", std::fs::read(&output).unwrap().into()).unwrap();
    let video = source.info().video.as_ref().unwrap();
    assert_eq!((video.width, video.height), (64, 36));
    assert_eq!(source.info().duration, rate.tick_of(2));
    assert!(source.video_frame(FrameRequest::full(Tick::ZERO)).is_ok());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(*s.project, *before);
    assert_eq!((s.history.undo.len(), s.history.redo.len()), history);
    let preview = image::open(result["previewPath"].as_str().unwrap()).unwrap();
    assert_eq!((preview.width(), preview.height()), (64, 36));
    let provenance: Value = serde_json::from_slice(&std::fs::read(result["provenancePath"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(provenance["sourceStartTick"], rate.tick_of(1).0);
    assert_eq!(provenance["sourceEndTick"], rate.tick_of(3).0);
    assert!(run(&mut s, &json!({"item":subclip.0,"path":output,"width":64,"height":36,"wait":true})).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    std::fs::remove_dir_all(dir).unwrap();
}
#[cfg(unix)]
#[test]
fn reserved_primary_handle_is_used_even_if_its_path_is_replaced() {
    let dir = tmp_dir("upscale-retained-handle");
    let (s, item) = source();
    let (snapshot, sequence, _) = prepare_snapshot(&s, &request(item, 32, 18, "contain")).unwrap();
    let output = dir.join("reserved.mov");
    let retained = dir.join("retained.mov");
    let handle = reserve_output(output.to_str().unwrap()).unwrap();
    assert!(reserve_output(output.to_str().unwrap()).is_err());
    std::fs::rename(&output, &retained).unwrap();
    std::fs::write(&output, b"file created after reservation").unwrap();
    let snapshot = Arc::new(snapshot);
    let provider = s.media.full_res_provider(snapshot.clone(), s.services.clone()).own_sources();
    let report = filmcraft_export::export(
        &snapshot,
        sequence,
        &filmcraft_export::ExportSettings {
            format: filmcraft_export::Format::ProRes,
            path: output.to_string_lossy().into_owned(),
            output_file: Some(handle),
            include_audio: false,
            ..Default::default()
        },
        &provider,
        &Default::default(),
    )
    .unwrap();
    assert!(report.bytes > 0);
    assert_eq!(std::fs::read(&output).unwrap(), b"file created after reservation");
    assert!(filmcraft_codecs::Mp4Source::open("retained.mov", std::fs::read(retained).unwrap().into()).is_ok());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn hdr_to_sdr_upscale_needs_explicit_consent_and_reports_actual_output() {
    let dir = tmp_dir("upscale-explicit-sdr");
    let source_path = dir.join("original.mov");
    make_movie(&source_path, DemoScene::OceanSunset, 32, 18, 2);
    let original = std::fs::read(&source_path).unwrap();
    let (mut s, items, _) = session_with(&[&source_path]);
    Arc::make_mut(&mut s.project).item_mut(items[0]).unwrap().as_media_mut().unwrap().interpret.color_space = Some(ColorSpace::Rec2100Hlg);
    let before = s.project.clone();
    let history = (s.history.undo.len(), s.history.redo.len());
    let output = dir.join("explicit-sdr.mov");
    let mut params = json!({"item":items[0].0,"path":output,"width":64,"height":36,"format":"mjpeg","audio":false,"wait":true});
    let jobs = s.jobs.len();
    for consent in [None, Some(json!(false)), Some(json!("yes"))] {
        match consent {
            Some(value) => {
                params["allowSdr"] = value;
            }
            None => {
                params.as_object_mut().unwrap().remove("allowSdr");
            }
        }
        let error = run(&mut s, &params).unwrap_err().to_string();
        assert!(error.contains("allowSdr") || error.contains("expected a boolean"), "{error}");
        assert_eq!(s.jobs.len(), jobs);
        assert!(!output.exists());
        assert!(!dir.join("explicit-sdr.mov.preview.png").exists());
        assert!(!dir.join("explicit-sdr.mov.provenance.json").exists());
    }
    params["allowSdr"] = json!(true);
    let result = run(&mut s, &params).unwrap();
    let job = s.jobs.iter().find(|job| job.id == result["job"].as_u64().unwrap()).unwrap();
    assert!(job.result.lock().unwrap().as_ref().unwrap().is_ok());
    assert_eq!(result["outputColorSpace"], "Rec709");
    assert_eq!(result["toneMapApplied"], true);
    assert!(result["warnings"].as_array().unwrap().iter().any(|v| v.as_str().is_some_and(|s| s.contains("tone-mapped"))));
    let source = filmcraft_codecs::Mp4Source::open("explicit-sdr.mov", std::fs::read(&output).unwrap().into()).unwrap();
    assert_eq!(source.info().video.as_ref().unwrap().color.transfer, filmcraft_color::Transfer::Bt709);
    assert!(source.video_frame(FrameRequest::full(Tick::ZERO)).is_ok());
    let provenance: Value = serde_json::from_slice(&std::fs::read(result["provenancePath"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(provenance["sourceColorSpace"], "Rec2100Hlg");
    assert_eq!(provenance["workingSpace"], "Rec2100Hlg");
    assert_eq!(provenance["outputColorSpace"], "Rec709");
    assert_eq!(provenance["toneMapApplied"], true);
    assert_eq!(provenance["allowSdr"], true);
    assert_eq!(std::fs::read(source_path).unwrap(), original);
    assert_eq!(*s.project, *before);
    assert_eq!((s.history.undo.len(), s.history.redo.len()), history);
    std::fs::remove_dir_all(dir).unwrap();
}
