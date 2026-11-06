use filmcraft_engine::{
    Session,
    project::{Transcript, Word},
    time::Tick,
};
use serde_json::json;
fn dialogue() -> (Session, u64) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let clip = s.active_sequence().unwrap().audio_tracks[0].items[0].clone();
    let mut transcript = Transcript::default();
    for (text, a, b) in [("Hello", 0.2, 0.5), ("um", 0.7, 0.9), ("world", 1.1, 1.4), ("again", 3.0, 3.3)] {
        transcript.words.push(Word::new(text, clip.source_in + Tick::from_seconds_f64(a), clip.source_in + Tick::from_seconds_f64(b)));
    }
    s.execute("transcript.set", json!({"item":clip.item.0,"transcript":transcript})).unwrap();
    (s, clip.item.0)
}
#[test]
fn automatic_dialogue_proposes_real_cut_captions_profile_and_one_undo() {
    let (mut s, item) = dialogue();
    let before = s.project.clone();
    let revision = s.revision;
    let original = s.state.active_sequence;
    let proposal = s.execute("workflow.autoEdit", json!({"items":[item],"analyze":false,"normalizeAudio":false})).unwrap();
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    let id = proposal["plan"].as_u64().unwrap();
    let (after, state, _) = s.agent_plan_snapshot(id, false).unwrap();
    assert_ne!(state.active_sequence, original);
    let q = after.sequence(state.active_sequence.unwrap()).unwrap();
    assert!(!q.caption_tracks.is_empty());
    assert!(q.duration() < s.project.item(filmcraft_engine::project::ItemId(item)).unwrap().duration());
    assert_eq!(q.settings.width, 1920);
    assert!(after.review.active_profile.is_some());
    assert_eq!(s.execute("agent.preview", json!({"plan":id})).unwrap()["workflow"]["sourcesPreserved"], true);
    assert!(s.execute("agent.subset", json!({"plan":id,"steps":[1]})).is_err(), "range depends on setup context");
    let undo = s.history.undo.len();
    s.execute("agent.apply", json!({"plan":id})).unwrap();
    assert_eq!(s.project, after);
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}
#[test]
fn footage_preparation_stages_source_metadata_and_does_not_touch_files_or_live_project() {
    let mut s = Session::default();
    let dir = filmcraft_engine::temp_dir().join(format!("filmcraft-auto-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("source.png");
    image::RgbaImage::from_pixel(8, 8, image::Rgba([120, 80, 40, 255])).save(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let before = s.project.clone();
    let p = s.execute("workflow.prepare", json!({"paths":[path],"mode":"selects","analyze":false,"transcribe":false,"normalizeAudio":false})).unwrap();
    assert_eq!(s.project, before);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(p["commands"][0]["id"], "workflow.importSources");
    let id = p["plan"].as_u64().unwrap();
    let (after, _, _) = s.agent_plan_snapshot(id, false).unwrap();
    assert!(after.items.len() > before.items.len());
    s.execute("agent.apply", json!({"plan":id})).unwrap();
    assert_eq!(s.project, after);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn prepared_background_artifact_refuses_stale_baseline_and_never_mutates_live_session() {
    let (mut s, item) = dialogue();
    let before = s.project.clone();
    let receiver =
        filmcraft_engine::workflow::prepare_background(&mut s, json!({"items":[item],"analyze":false,"normalizeAudio":false,"transcribe":false,"qc":false}))
            .unwrap();
    let artifact = receiver.receiver.recv_timeout(std::time::Duration::from_secs(30)).unwrap().unwrap();
    assert_eq!(s.project, before);
    s.execute("project.setNotes", json!({"text":"Owner kept editing"})).unwrap();
    assert!(filmcraft_engine::workflow::adopt_background_plan(&mut s, &receiver, Ok(artifact)).is_err());
    assert_eq!(s.project.notes, "Owner kept editing");
}
#[test]
fn auto_apply_requires_explicit_edit_and_unapproved_export_is_refused() {
    let (mut s, item) = dialogue();
    s.operations.permission = filmcraft_engine::operations::Permission::Propose;
    let before = s.project.clone();
    assert!(s.execute("workflow.autoEdit", json!({"items":[item],"analyze":false,"apply":true})).is_err());
    assert_eq!(s.project, before);
    let p = s.execute("workflow.autoEdit", json!({"items":[item],"analyze":false,"normalizeAudio":false})).unwrap();
    assert!(s.execute("workflow.deliver", json!({"plan":p["plan"],"path":"/tmp/must-not-export.mp4","wait":false})).is_err());
    assert_eq!(s.project, before);
}
#[test]
fn approved_generated_footage_exports_and_reopens_with_native_delivery_qc() {
    let mut s = Session::default();
    let dir = filmcraft_engine::temp_dir().join(format!("filmcraft-auto-deliver-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("original.png");
    let output = dir.join("reviewed.mp4");
    image::RgbaImage::from_pixel(16, 16, image::Rgba([120, 80, 40, 255])).save(&input).unwrap();
    let bytes = std::fs::read(&input).unwrap();
    let p=s.execute("workflow.prepare",json!({"paths":[input],"mode":"selects","analyze":true,"transcribe":false,"normalizeAudio":false,"overrides":{"delivery":{"width":64,"height":64,"fpsNum":24,"fpsDen":1,"captions":"none"}}})).unwrap();
    let id = p["plan"].as_u64().unwrap();
    s.execute("agent.apply", json!({"plan":id})).unwrap();
    let result = s.execute("workflow.deliver", json!({"plan":id,"path":output,"wait":true})).unwrap();
    assert!(output.is_file(), "{result}");
    assert_eq!(result["fileQc"]["verified"], true, "{result}");
    assert_eq!(std::fs::read(&input).unwrap(), bytes);
    assert_eq!(result["fileQc"]["metadata"]["video"]["width"], 64);
    let delivered = std::fs::read(&output).unwrap();
    let occupied = s.execute("workflow.deliver", json!({"plan":id,"path":output,"wait":false})).unwrap_err();
    assert!(occupied.to_string().contains("already exists"), "{occupied}");
    assert_eq!(std::fs::read(&output).unwrap(), delivered, "a repeated delivery cannot truncate its previous output");
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn authorized_automatic_apply_installs_the_exact_approved_snapshot_without_extra_activity() {
    let (mut s, item) = dialogue();
    let before = s.history.undo.len();
    let p = s.execute("workflow.autoEdit", json!({"items":[item],"analyze":false,"normalizeAudio":false,"apply":true})).unwrap();
    let id = p["plan"].as_u64().unwrap();
    let (exact, _, _) = s.agent_plan_snapshot(id, false).unwrap();
    assert_eq!(s.project, exact);
    assert_eq!(s.history.undo.len(), before + 1);
    assert_eq!(s.project.review.activity.last().unwrap().command, "agent.apply");
    assert!(s.execute("workflow.deliver", json!({"plan":id,"path":""})).unwrap_err().to_string().contains("output path"));
}
#[test]
fn preparation_is_durable_running_until_adoption_and_cancellation_never_changes_project() {
    let (mut s, item) = dialogue();
    let before = s.project.clone();
    let task =
        filmcraft_engine::workflow::prepare_background(&mut s, json!({"items":[item],"analyze":false,"normalizeAudio":false,"transcribe":false,"qc":false}))
            .unwrap();
    let id = task.job.id;
    assert_eq!(s.job_registry.get(id).unwrap()["inputRevision"], s.revision);
    let artifact = task.receiver.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
    s.job_registry.sync(&s.jobs).unwrap();
    assert_eq!(s.job_registry.get(id).unwrap()["state"], "running", "waiting for native adoption is not a completed plan");
    task.cancel();
    assert!(filmcraft_engine::workflow::adopt_background_plan(&mut s, &task, artifact).is_err());
    assert_eq!(s.project, before);
    assert_eq!(s.job_registry.get(id).unwrap()["state"], "cancelled");
    let task =
        filmcraft_engine::workflow::prepare_background(&mut s, json!({"items":[item],"analyze":false,"normalizeAudio":false,"transcribe":false,"qc":false}))
            .unwrap();
    let artifact = task.receiver.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
    let report = filmcraft_engine::workflow::adopt_background_plan(&mut s, &task, artifact).unwrap();
    assert!(report["plan"].as_u64().is_some());
    assert_eq!(s.project, before);
    assert_eq!(s.job_registry.get(task.job.id).unwrap()["state"], "completed");
}
#[test]
fn pending_native_preparation_is_marked_interrupted_after_reopening_job_history() {
    let (mut s, item) = dialogue();
    let dir = filmcraft_engine::temp_dir().join(format!("filmcraft-workflow-ledger-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    s.job_registry.configure(&dir).unwrap();
    let task =
        filmcraft_engine::workflow::prepare_background(&mut s, json!({"items":[item],"analyze":false,"normalizeAudio":false,"transcribe":false,"qc":false}))
            .unwrap();
    let id = task.job.id;
    let _artifact = task.receiver.recv_timeout(std::time::Duration::from_secs(30)).unwrap().unwrap();
    drop(task);
    drop(s);
    let registry = filmcraft_engine::durable_jobs::JobRegistry::default();
    registry.configure(&dir).unwrap();
    assert_eq!(registry.get(id).unwrap()["state"], "interrupted");
    drop(registry);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn preparation_rejects_directory_inputs_before_folder_import_expands_them() {
    let mut s = Session::default();
    let dir = filmcraft_engine::temp_dir().join(format!("filmcraft-auto-directory-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let before = s.project.clone();
    let error = s.execute("workflow.prepare", json!({"paths":[dir],"analyze":false,"transcribe":false})).unwrap_err();
    assert!(error.to_string().contains("individual footage files"), "{error}");
    assert_eq!(s.project, before);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn delivery_override_cannot_change_the_reviewed_geometry_or_rate() {
    let (mut s, item) = dialogue();
    let mut profile = filmcraft_engine::project::review::EditingProfile { name: "Other target".into(), ..Default::default() };
    profile.delivery.width = 720;
    profile.delivery.height = 1280;
    profile.delivery.fps_num = 60;
    s.execute("profile.save", json!({"profile":profile})).unwrap();
    let p = s.execute("workflow.autoEdit", json!({"items":[item],"analyze":false,"normalizeAudio":false,"qc":false})).unwrap();
    s.execute("agent.apply", json!({"plan":p["plan"]})).unwrap();
    let path = filmcraft_engine::temp_dir().join(format!("filmcraft-mismatched-target-{}.mp4", std::process::id()));
    let error = s.execute("workflow.deliver", json!({"plan":p["plan"],"path":path,"profile":"Other target","wait":false})).unwrap_err();
    assert!(error.to_string().contains("differs from the approved sequence"), "{error}");
    assert!(!path.exists());
}

#[test]
fn pending_delivery_caption_none_request_preserves_its_explicit_verification_limit() {
    let (mut s, item) = dialogue();
    let dir = filmcraft_engine::temp_dir().join(format!("filmcraft-caption-none-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p=s.execute("workflow.autoEdit",json!({"items":[item],"analyze":false,"normalizeAudio":false,"qc":false,"overrides":{"delivery":{"width":64,"height":64,"fpsNum":24,"fpsDen":1,"captions":"none"}}})).unwrap();
    s.execute("agent.apply", json!({"plan":p["plan"]})).unwrap();
    assert!(s.active_sequence().unwrap().caption_tracks.iter().any(|t| !t.captions.is_empty()));
    let result = s.execute("workflow.deliver", json!({"plan":p["plan"],"path":dir.join("output.mp4"),"wait":false})).unwrap();
    assert_eq!(result["fileQc"]["params"]["expected"]["captions"], json!({"mode":"none","count":0}));
    let id = result["export"]["job"].as_u64().unwrap();
    let job = s.jobs.iter().find(|j| j.id == id).unwrap();
    for _ in 0..10_000 {
        if job.result.lock().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(job.result.lock().unwrap().as_ref().unwrap().is_ok());
    let qc = s.execute("qc.export", result["fileQc"]["params"].clone()).unwrap();
    assert_eq!(qc["verified"], false, "absence of embedded CC is honestly unknown");
    let check = qc["checks"].as_array().unwrap().iter().find(|c| c["check"] == "captions").unwrap();
    assert_eq!(check["expected"], "none");
    assert_eq!(check["status"], "unknown");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn asynchronous_delivery_qc_uses_approved_captions_and_revision_after_project_switch() {
    let mut s = Session::default();
    let dir = filmcraft_engine::temp_dir().join(format!("filmcraft-frozen-delivery-qc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("original.png");
    let output = dir.join("reviewed.mp4");
    image::RgbaImage::from_pixel(8, 8, image::Rgba([110, 70, 40, 255])).save(&input).unwrap();
    let p=s.execute("workflow.prepare",json!({"paths":[input],"mode":"selects","analyze":false,"transcribe":false,"normalizeAudio":false,"qc":false,"overrides":{"delivery":{"width":64,"height":64,"fpsNum":24,"fpsDen":1,"captions":"srt"}}})).unwrap();
    s.execute("agent.apply", json!({"plan":p["plan"]})).unwrap();
    let caption_plan =
        s.execute("agent.plan", json!({"steps":[{"id":"captions.add","params":{"text":"Approved caption","time":0,"durationSeconds":1.0}}]})).unwrap();
    s.execute("agent.apply", json!({"plan":caption_plan["plan"]})).unwrap();
    let revision = s.revision;
    s.path = Some(dir.join("approved.filmcraft").to_string_lossy().into_owned());
    let approved_path = s.path.clone();
    let frozen = filmcraft_engine::workflow::QcSnapshot::capture(&s);
    let delivery = s.execute("workflow.deliver", json!({"plan":caption_plan["plan"],"path":output,"wait":false})).unwrap();
    let job = s.jobs.iter().find(|j| j.id == delivery["export"]["job"].as_u64().unwrap()).unwrap().clone();
    s.execute("file.newProject", json!({"name":"Owner switched projects"})).unwrap();
    for _ in 0..10_000 {
        if job.result.lock().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(job.result.lock().unwrap().as_ref().unwrap().is_ok());
    let qc = filmcraft_engine::workflow::qc_background_from(&mut s, frozen, delivery["fileQc"]["params"].clone()).unwrap();
    let result = qc.receiver.recv_timeout(std::time::Duration::from_secs(30)).unwrap().unwrap();
    assert_eq!(result["revision"], revision);
    let check = result["checks"].as_array().unwrap().iter().find(|c| c["check"] == "caption_sequence_content_and_timing").unwrap();
    assert_eq!(check["status"], "pass", "{result}");
    assert_eq!(s.job_registry.get(qc.job.id).unwrap()["inputRevision"], revision);
    assert_eq!(s.job_registry.get(qc.job.id).unwrap()["projectPath"], json!(approved_path));
    assert_eq!(s.project.name, "Owner switched projects");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn newly_recognized_existing_sources_stage_the_transcript_with_the_proposal() {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":1})).unwrap()["item"].as_u64().unwrap();
    let transcript = Transcript { words: vec![Word::new("Recognized", Tick::from_seconds_f64(0.1), Tick::from_seconds_f64(0.4))], ..Default::default() };
    s.transcriber = Some(std::sync::Arc::new(filmcraft_speech::FixedTranscriber { transcript, id: "deterministic-workflow-test".into() }));
    let before = s.project.clone();
    let task =
        filmcraft_engine::workflow::prepare_background(&mut s, json!({"items":[item],"analyze":false,"normalizeAudio":false,"transcribe":true,"qc":false}))
            .unwrap();
    let artifact = task.receiver.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
    let proposal = filmcraft_engine::workflow::adopt_background_plan(&mut s, &task, artifact).unwrap();
    assert_eq!(s.project, before, "recognition and transcript installation are proposed together");
    assert_eq!(proposal["commands"][0]["id"], "transcript.set");
    let (after, _, _) = s.agent_plan_snapshot(proposal["plan"].as_u64().unwrap(), false).unwrap();
    assert_eq!(after.transcripts[&filmcraft_engine::project::ItemId(item)].words[0].text, "Recognized");
    assert!(after.transcripts[&filmcraft_engine::project::ItemId(item)].words[0].id > 0);
    s.execute("agent.apply", json!({"plan":proposal["plan"]})).unwrap();
    assert_eq!(s.project, after);
}

struct WorkflowAudioStreams {
    picture: filmcraft_media::SharedSource,
    info: filmcraft_media::MediaInfo,
    reads: std::sync::Arc<[std::sync::atomic::AtomicUsize; 2]>,
}
impl filmcraft_media::MediaSource for WorkflowAudioStreams {
    fn info(&self) -> &filmcraft_media::MediaInfo {
        &self.info
    }
    fn video_frame(&self, request: filmcraft_media::FrameRequest) -> filmcraft_media::Result<std::sync::Arc<filmcraft_frame::VideoFrame>> {
        self.picture.video_frame(request)
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        self.audio_stream(0, start, frames, rate)
    }
    fn audio_stream(&self, stream: usize, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        assert!(stream < self.info.audio_streams.len(), "selected nonexistent stream {stream}");
        self.reads[stream].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let amplitude = if stream == 0 { 0.02 } else { 0.12 };
        let signal: Vec<_> = (0..frames)
            .map(|i| {
                let sample = start + i as i64;
                if sample < 0 || sample >= i64::from(rate) {
                    0.0
                } else {
                    (std::f64::consts::TAU * 550.0 * sample as f64 / f64::from(rate)).sin() as f32 * amplitude
                }
            })
            .collect();
        Ok(filmcraft_frame::AudioBuffer { sample_rate: rate, channels: vec![signal.clone(), signal] })
    }
}

fn workflow_stream_source(streams: usize, split_primary: bool) -> (Session, u64, std::sync::Arc<[std::sync::atomic::AtomicUsize; 2]>) {
    use filmcraft_engine::project::{AudioChannelMap, AudioChannels, ItemId};
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":1,"width":64,"height":64})).unwrap()["item"].as_u64().unwrap();
    let picture = s.source(ItemId(item)).unwrap();
    let mut info = picture.info().clone();
    info.audio_streams = vec![info.audio_streams[0].clone(); streams];
    let reads = std::sync::Arc::new(std::array::from_fn(|_| std::sync::atomic::AtomicUsize::new(0)));
    let source = std::sync::Arc::new(WorkflowAudioStreams { picture, info: info.clone(), reads: reads.clone() });
    let media = std::sync::Arc::make_mut(&mut s.project).item_mut(ItemId(item)).unwrap().as_media_mut().unwrap();
    media.info = info;
    if split_primary {
        media.interpret.audio_channels = Some(AudioChannelMap { format: AudioChannels::Mono, clips: vec![vec![0], vec![1]] });
    }
    s.media.insert(ItemId(item), source);
    (s, item, reads)
}

fn workflow_volume(clip: &filmcraft_engine::project::TrackItem) -> f64 {
    clip.effect("volume").unwrap().param("level").unwrap().value.as_f64().unwrap()
}

#[test]
fn automatic_main_stream_binding_follows_primary_channel_map_and_normalizes_only_that_stream() {
    use filmcraft_engine::project::AudioType;
    let (mut s, item, reads) = workflow_stream_source(2, true);
    let before = s.project.clone();
    let revision = s.revision;
    let undo = s.history.undo.len();
    let proposal = s
        .execute(
            "workflow.autoEdit",
            json!({"items":[item],"mainAudioStream":1,"secondaryAudio":"mute",
        "analyze":false,"repairChannels":false,"polishVoice":false,"normalizeAudio":true,"captions":false,"qc":false,
        "overrides":{"loudnessLufs":-42}}),
        )
        .unwrap();
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    assert_eq!(s.history.undo.len(), undo);
    let commands = proposal["commands"].as_array().unwrap();
    let placement = commands.iter().position(|step| step["id"] == "timeline.place").unwrap();
    let normalization = commands.iter().find(|step| step["id"] == "workflow.normalizeAudio").unwrap();
    assert_eq!(
        normalization["params"]["clips"],
        json!([{"$step":placement,"path":"/clips/3"}]),
        "video is first, two mapped stream-0 clips precede the native stream-1 clip"
    );
    let (plan, state, _) = s.agent_plan_snapshot(proposal["plan"].as_u64().unwrap(), false).unwrap();
    let sequence = plan.sequence(state.active_sequence.unwrap()).unwrap();
    assert_eq!(sequence.video_tracks.iter().map(|t| t.items.len()).sum::<usize>(), 1);
    assert!(sequence.video_tracks.iter().flat_map(|t| &t.items).all(|clip| clip.enabled));
    let audio: Vec<_> = sequence.audio_tracks.iter().flat_map(|t| &t.items).collect();
    assert_eq!(audio.len(), 3);
    assert_eq!(audio[0].audio_stream, 0);
    assert_eq!(audio[0].source_channels, vec![0]);
    assert_eq!(audio[1].audio_stream, 0);
    assert_eq!(audio[1].source_channels, vec![1]);
    for backup in audio.iter().filter(|clip| clip.audio_stream == 0) {
        assert!(!backup.enabled, "backup source streams are retained and muted");
        assert!(backup.essential.is_none(), "backup never receives main-stream dialogue processing");
        assert_eq!(workflow_volume(backup), 0.0);
    }
    let main = audio.iter().find(|clip| clip.audio_stream == 1).unwrap();
    assert!(main.enabled && main.source_channels.is_empty());
    assert_eq!(main.essential.as_ref().unwrap().kind, AudioType::Dialogue);
    assert!(workflow_volume(main) < -1.0, "the -42 LUFS request must reach actual Volume normalization");
    assert!(reads[1].load(std::sync::atomic::Ordering::Relaxed) > 0, "main stream was measured");
    assert_eq!(reads[0].load(std::sync::atomic::Ordering::Relaxed), 0, "mapped backups were never measured or normalized");
    s.execute("agent.apply", json!({"plan":proposal["plan"]})).unwrap();
    assert_eq!(s.project, plan);
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}

#[test]
fn automatic_missing_main_stream_falls_back_explicitly_and_still_retains_one_undo() {
    let (mut s, item, reads) = workflow_stream_source(1, false);
    let before = s.project.clone();
    let revision = s.revision;
    let undo = s.history.undo.len();
    let proposal = s
        .execute(
            "workflow.autoEdit",
            json!({"items":[item],"mainAudioStream":1,"analyze":false,
        "repairChannels":false,"polishVoice":false,"normalizeAudio":true,"captions":false,"qc":false,"overrides":{"loudnessLufs":-42}}),
        )
        .unwrap();
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    assert!(
        proposal["workflow"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|w| w.as_str())
            .any(|warning| warning.contains("no audio stream 1") && warning.contains("stream0")),
        "{proposal}"
    );
    let (plan, state, _) = s.agent_plan_snapshot(proposal["plan"].as_u64().unwrap(), false).unwrap();
    let audio: Vec<_> = plan.sequence(state.active_sequence.unwrap()).unwrap().audio_tracks.iter().flat_map(|track| &track.items).collect();
    assert_eq!(audio.len(), 1);
    assert_eq!(audio[0].audio_stream, 0);
    assert!(audio[0].enabled);
    assert!(workflow_volume(audio[0]) < -1.0);
    assert!(reads[0].load(std::sync::atomic::Ordering::Relaxed) > 0);
    assert_eq!(reads[1].load(std::sync::atomic::Ordering::Relaxed), 0);
    s.execute("agent.apply", json!({"plan":proposal["plan"]})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}

#[test]
fn automatic_long_dialogue_skips_unbounded_channel_repair_and_loudness_without_losing_the_proposal() {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":301,"width":64,"height":64})).unwrap()["item"].as_u64().unwrap();
    let before = s.project.clone();
    let undo = s.history.undo.len();
    let proposal = s.execute("workflow.autoEdit", json!({"items":[item],"analyze":false,"polishVoice":false,"captions":false,"qc":false})).unwrap();
    assert_eq!(s.project, before);
    assert!(!proposal["commands"].as_array().unwrap().iter().any(|c| c["id"] == "audio.channels.set"));
    assert!(proposal["workflow"]["warnings"].as_array().unwrap().iter().any(|w| w.as_str().is_some_and(|s| s.contains("300s") && s.contains("skipped"))));
    let (after, state, _) = s.agent_plan_snapshot(proposal["plan"].as_u64().unwrap(), false).unwrap();
    let clip = &after.sequence(state.active_sequence.unwrap()).unwrap().audio_tracks[0].items[0];
    assert!(clip.duration.0 > 300 * filmcraft_engine::time::TICKS_PER_SECOND);
    assert!(clip.effect("channel_router").is_none());
    assert_eq!(workflow_volume(clip), 0.0);
    s.execute("agent.apply", json!({"plan":proposal["plan"]})).unwrap();
    assert_eq!(s.history.undo.len(), undo + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}
