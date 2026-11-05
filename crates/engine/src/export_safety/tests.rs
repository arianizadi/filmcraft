use super::*;
use filmcraft_project::{CaptionFormat, CaptionTrack, MediaRef, TrackId};
use filmcraft_time::Tick;
use std::sync::Arc;
fn setup(name: &str) -> (Session, PathBuf, Vec<u8>, u64) {
    let dir = crate::temp_dir().join(format!("filmcraft-export-source-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("original.wav");
    let bytes = filmcraft_media::wav::write_wav16(&vec![0.1; 4800], 1, 48000);
    std::fs::write(&path, &bytes).unwrap();
    let mut s = Session::default();
    let item = s.execute("file.import", serde_json::json!({"paths":[path]})).unwrap()["items"][0].as_u64().unwrap();
    s.execute("file.newSequence", serde_json::json!({"width":16,"height":16,"frameRate":{"num":24,"den":1},"video":1,"audio":1})).unwrap();
    s.execute("timeline.place", serde_json::json!({"item":item,"time":0,"duration":Tick::from_units(1,10).0,"video":false})).unwrap();
    (s, dir, bytes, item)
}
#[test]
fn same_source_aliases_proxy_and_project_reject_before_any_job_or_sidecar() {
    let (mut s, dir, bytes, item) = setup("aliases");
    let source = dir.join("original.wav");
    let hard = dir.join("hard.wav");
    std::fs::hard_link(&source, &hard).unwrap();
    let mut outputs = vec![source.clone(), hard];
    #[cfg(unix)]
    {
        let symlink = dir.join("symlink.wav");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        outputs.push(symlink);
    }
    let proxy = dir.join("proxy.wav");
    std::fs::write(&proxy, &bytes).unwrap();
    Arc::make_mut(&mut s.project).item_mut(ItemId(item)).unwrap().as_media_mut().unwrap().proxy =
        Some(MediaRef::File { path: proxy.to_string_lossy().into_owned() });
    outputs.push(proxy.clone());
    let project = dir.join("project.wav");
    std::fs::write(&project, b"saved project sentinel").unwrap();
    s.path = Some(project.to_string_lossy().into_owned());
    outputs.push(project.clone());
    let id = s.state.active_sequence.unwrap();
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().caption_tracks.push(CaptionTrack::new(TrackId(500), "captions".into(), CaptionFormat::Subtitle));
    let snapshot = s.project.clone();
    let jobs = s.jobs.len();
    for output in outputs {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            s.execute("file.exportMedia", serde_json::json!({"path":output,"format":"wav","wait":true,"captionSidecar":"srt"}))
        }));
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
        assert_eq!(s.jobs.len(), jobs);
        assert!(Arc::ptr_eq(&s.project, &snapshot));
        assert!(!output.with_extension("srt").exists());
    }
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    assert_eq!(std::fs::read(proxy).unwrap(), bytes);
    assert_eq!(std::fs::read(project).unwrap(), b"saved project sentinel");
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn sidecar_numbered_image_and_opatom_audio_aliases_are_protected() {
    let (mut s, dir, bytes, _) = setup("families");
    let id = s.state.active_sequence.unwrap();
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().caption_tracks.push(CaptionTrack::new(TrackId(500), "captions".into(), CaptionFormat::Subtitle));
    let source = dir.join("original.wav");
    let sidecar = dir.join("delivery.srt");
    std::fs::hard_link(&source, &sidecar).unwrap();
    let mut settings =
        ExportSettings { path: dir.join("delivery.mp4").to_string_lossy().into_owned(), caption_sidecar: Some("srt".into()), ..Default::default() };
    assert!(preflight(&s, &s.project, id, &settings).is_err());
    assert_eq!(std::fs::read(&sidecar).unwrap(), bytes);
    settings.caption_sidecar = None;
    settings.format = Format::PngSequence;
    settings.path = dir.join("frame.png").to_string_lossy().into_owned();
    let numbered = PathBuf::from(filmcraft_export::image_sequence_path(&settings.path, 1, 3));
    std::fs::hard_link(&source, &numbered).unwrap();
    assert!(preflight(&s, &s.project, id, &settings).is_err());
    assert_eq!(std::fs::read(&numbered).unwrap(), bytes);
    std::fs::remove_file(numbered).unwrap();
    settings.format = Format::MxfOpAtom;
    settings.path = dir.join("atom.mxf").to_string_lossy().into_owned();
    let audio = dir.join("atom_A1.mxf");
    std::fs::hard_link(&source, &audio).unwrap();
    assert!(preflight(&s, &s.project, id, &settings).is_err());
    assert_eq!(std::fs::read(audio).unwrap(), bytes);
    assert_eq!(std::fs::read(source).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn normal_existing_delivery_overwrite_still_works_and_preopened_source_handles_refuse() {
    let (mut s, dir, bytes, _) = setup("delivery");
    let source = dir.join("original.wav");
    let delivery = dir.join("delivery.wav");
    std::fs::write(&delivery, b"old delivery").unwrap();
    let result = s.execute("file.exportMedia", serde_json::json!({"path":delivery,"format":"wav","wait":true})).unwrap();
    assert!(result["result"]["path"].as_str().is_some());
    assert_eq!(std::fs::read(source.clone()).unwrap(), bytes);
    assert_ne!(std::fs::read(delivery).unwrap(), b"old delivery");
    let settings = ExportSettings {
        path: dir.join("different.wav").to_string_lossy().into_owned(),
        output_file: Some(Arc::new(std::fs::File::open(source).unwrap())),
        ..Default::default()
    };
    assert!(preflight(&s, &s.project, s.state.active_sequence.unwrap(), &settings).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn numbered_pattern_is_bounded_and_uses_the_exporters_exact_naming() {
    let images = Images::new("Sequence01.png", 10_000_000_000, false).unwrap();
    assert_eq!(
        images.index(Path::new(&filmcraft_export::image_sequence_path("Sequence01.png", 1, 10_000_000_000)).file_name().unwrap().to_str().unwrap()),
        Some(1)
    );
    assert_eq!(
        images.index(Path::new(&filmcraft_export::image_sequence_path("Sequence01.png", 9_999_999_999, 10_000_000_000)).file_name().unwrap().to_str().unwrap()),
        Some(9_999_999_999)
    );
    assert!(images.index("Sequence01bad.png").is_none());
    assert!(images.index("Sequence0110000000000.png").is_none());
    assert!(images.index("Sequence01😀.png").is_none());
}

#[test]
fn virtual_host_preserves_input_and_can_deliver_without_native_filesystem() {
    use std::sync::Mutex;
    #[derive(Default)]
    struct Memory(Mutex<BTreeMap<String, Vec<u8>>>);
    impl crate::Services for Memory {
        fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
            self.0.lock().unwrap().get(path).cloned().ok_or(std::io::ErrorKind::NotFound.into())
        }
        fn write_file(&self, path: &str, bytes: &[u8]) -> std::io::Result<()> {
            self.0.lock().unwrap().insert(path.into(), bytes.to_vec());
            Ok(())
        }
        fn export_in_memory(&self) -> bool {
            true
        }
    }
    let memory = Arc::new(Memory::default());
    let original = filmcraft_media::wav::write_wav16(&vec![0.1; 480], 1, 48000);
    memory.0.lock().unwrap().insert("virtual-original.wav".into(), original.clone());
    let mut s = Session::new(memory.clone());
    let id = s.execute("file.import", serde_json::json!({"paths":["virtual-original.wav"]})).unwrap()["items"][0].as_u64().unwrap();
    s.execute("file.newSequence", serde_json::json!({"width":16,"height":16,"video":1,"audio":1})).unwrap();
    s.execute("timeline.place", serde_json::json!({"item":id,"video":false,"duration":Tick::from_units(1,100).0})).unwrap();
    assert!(s.execute("file.exportMedia", serde_json::json!({"path":"./virtual-original.wav","format":"wav","wait":true})).is_err());
    s.execute("file.exportMedia", serde_json::json!({"path":"virtual-delivery.wav","format":"wav","wait":true})).unwrap();
    let files = memory.0.lock().unwrap();
    assert_eq!(files["virtual-original.wav"], original);
    assert!(files.contains_key("virtual-delivery.wav"));
}

#[test]
fn later_source_image_sequence_frame_alias_is_protected() {
    let (mut s, dir, _, item) = setup("source-sequence");
    let first = dir.join("source000.png");
    let second = dir.join("source001.png");
    let png = crate::inspection::png_image(&filmcraft_render::Image::filled(2, 2, [0.2, 0.3, 0.4, 1.0])).unwrap();
    std::fs::write(&first, &png).unwrap();
    std::fs::write(&second, &png).unwrap();
    let media = Arc::make_mut(&mut s.project).item_mut(ItemId(item)).unwrap().as_media_mut().unwrap();
    media.media = MediaRef::File { path: first.to_string_lossy().into_owned() };
    media.info.kind = filmcraft_media::MediaKind::ImageSequence;
    let settings = ExportSettings { format: Format::PngSequence, path: dir.join("delivery.png").to_string_lossy().into_owned(), ..Default::default() };
    let target = PathBuf::from(filmcraft_export::image_sequence_path(&settings.path, 1, 3));
    std::fs::hard_link(&second, &target).unwrap();
    assert!(preflight(&s, &s.project, s.state.active_sequence.unwrap(), &settings).is_err());
    assert_eq!(std::fs::read(first).unwrap(), png);
    assert_eq!(std::fs::read(second).unwrap(), png);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn generic_services_guard_protects_sources_aliases_and_project_and_allows_saves() {
    let (mut s, dir, bytes, _) = setup("generic-services");
    let source = dir.join("original.wav");
    let alias = dir.join("hard.wav");
    std::fs::hard_link(&source, &alias).unwrap();
    let project = dir.join("current.fcproj");
    std::fs::write(&project, b"project sentinel").unwrap();
    s.path = Some(project.to_string_lossy().into_owned());
    let original_services = s.services.clone();
    let guard = SourceWriteServices::for_command(&s, "lut.export").unwrap();
    for path in [&source, &alias, &project] {
        assert!(guard.write_file(path.to_str().unwrap(), b"must not replace").is_err());
        assert!(guard.write_file_new(path.to_str().unwrap(), b"must not replace").is_err());
    }
    #[cfg(unix)]
    {
        let link = dir.join("symlink.wav");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(guard.write_file(link.to_str().unwrap(), b"must not replace").is_err());
    }
    assert_eq!(guard.read_range(source.to_str().unwrap(), 0, 4).unwrap(), b"RIFF");
    assert!(guard.reader(source.to_str().unwrap()).unwrap().is_ok());
    assert_eq!(guard.file_size(source.to_str().unwrap()).unwrap(), bytes.len() as u64);
    assert!(guard.list_dir(dir.to_str().unwrap()).unwrap().is_ok());
    assert!(guard.file_loader().is_some());
    s.services = guard;
    let saved = s.execute("file.save", serde_json::json!({})).unwrap();
    assert!(saved.is_object());
    let result = s.execute("file.saveCopy", serde_json::json!({"path":source}));
    assert!(result.is_err());
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    assert_ne!(std::fs::read(project).unwrap(), b"project sentinel");
    s.services = original_services;
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn generic_caption_export_and_panic_restore_services_without_replacing_media() {
    let (mut s, dir, bytes, _) = setup("caption-source");
    let original = s.services.clone();
    let id = s.state.active_sequence.unwrap();
    Arc::make_mut(&mut s.project).sequence_mut(id).unwrap().caption_tracks.push(CaptionTrack::new(TrackId(501), "caption".into(), CaptionFormat::Subtitle));
    let source = dir.join("original.wav");
    let error = s.execute("captions.export", serde_json::json!({"path":source,"format":"srt","track":501})).unwrap_err();
    assert!(error.to_string().contains("export source protection"), "{error}");
    assert!(Arc::ptr_eq(&s.services, &original));
    assert_eq!(std::fs::read(&source).unwrap(), bytes);
    struct PanicWrite(Arc<dyn crate::Services>);
    impl crate::Services for PanicWrite {
        fn read_file(&self, p: &str) -> std::io::Result<Vec<u8>> {
            self.0.read_file(p)
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            panic!("synthetic write failure")
        }
    }
    let panicking: Arc<dyn crate::Services> = Arc::new(PanicWrite(original));
    s.services = panicking.clone();
    assert!(s.execute("file.saveCopy", serde_json::json!({"path":dir.join("copy.fcproj")})).is_err());
    assert!(Arc::ptr_eq(&s.services, &panicking));
    assert_eq!(std::fs::read(source).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lazy_guard_uses_fixed_relative_source_resolution_after_project_switch() {
    let (mut s, dir, bytes, item) = setup("fixed-path-snapshot");
    let source = dir.join("original.wav");
    let project = dir.join("current.fcproj");
    s.path = Some(project.to_string_lossy().into_owned());
    Arc::make_mut(&mut s.project).item_mut(ItemId(item)).unwrap().as_media_mut().unwrap().media = MediaRef::File { path: "original.wav".into() };
    s.media.add_stored_paths(["original.wav".into()], s.path.as_deref());
    let guard = SourceWriteServices::for_command(&s, "captions.export").unwrap();
    s.media.clear_stored();
    assert!(guard.write_file(source.to_str().unwrap(), b"must not replace old source").is_err());
    assert_eq!(std::fs::read(source).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn nested_project_save_exception_keeps_the_underlying_folder_boundary() {
    struct DenyWrites;
    impl crate::Services for DenyWrites {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            Err(std::io::ErrorKind::NotFound.into())
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
    }
    let mut s = Session::new(Arc::new(DenyWrites));
    let inner = s.services.clone();
    s.services = SourceWriteServices::for_command(&s, "workflow.prepare").unwrap();
    let outer = s.services.clone();
    assert!(s.execute("file.saveCopy", serde_json::json!({"path":"denied-copy.fcproj"})).is_err());
    assert!(Arc::ptr_eq(&s.services, &outer));
    assert!(Arc::ptr_eq(&s.services.source_write_base().unwrap(), &inner));
    assert!(!std::path::Path::new("denied-copy.fcproj").exists());
}
