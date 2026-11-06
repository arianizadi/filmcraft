use super::*;
use filmcraft_export::SampleFormat;
use filmcraft_project::{Transition, TransitionAlign, TransitionId};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn noise(n: usize, mut state: u64) -> Vec<f32> {
    (0..n)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let x = ((state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0;
            // A clap-like transient plus nonperiodic low-level room activity, all original test data.
            x * if (24_000..24_960).contains(&i) { 0.8 } else { 0.08 }
        })
        .collect()
}
fn stereo(l: &[f32], r: &[f32]) -> Vec<f32> {
    l.iter().zip(r).flat_map(|(l, r)| [*l, *r]).collect()
}
struct Fixture {
    s: Session,
    reference: u64,
    target: u64,
    dir: PathBuf,
    originals: Vec<Vec<u8>>,
}
impl Fixture {
    fn with(reference: &[f32], target: &[f32], reference_format: SampleFormat, target_format: SampleFormat) -> Self {
        let dir = crate::temp_dir().join(format!("filmcraft-audio-sync-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Session::default();
        s.execute("file.newSequence", json!({"video":1,"audio":4,"fps":24,"width":160,"height":90})).unwrap();
        let mut clips = Vec::new();
        let mut originals = Vec::new();
        for (index, (samples, format)) in [(reference, reference_format), (target, target_format)].into_iter().enumerate() {
            let bytes = filmcraft_export::write_wav(samples, 2, RATE, format).unwrap();
            let path = dir.join(format!("recording-{index}.wav"));
            std::fs::write(&path, &bytes).unwrap();
            let imported = s.execute("file.import", json!({"paths":[path]})).unwrap()["items"][0].as_u64().unwrap();
            let placed = s.execute("timeline.place", json!({"item":imported,"audioTrack":format!("A{}",index+1),"seconds":2})).unwrap();
            clips.push(placed["clips"][0].as_u64().unwrap());
            originals.push(bytes);
        }
        Self { s, reference: clips[0], target: clips[1], dir, originals }
    }
    fn shifted(inverted: bool) -> Self {
        let base = noise(5 * RATE as usize, 0x78ab41);
        let n = 3 * RATE as usize;
        let zero = vec![0.0; n];
        let shifted: Vec<_> = base[12_000..12_000 + n].iter().map(|x| x * if inverted { -6.0 } else { 6.0 }).collect();
        Self::with(&stereo(&zero, &base[..n]), &stereo(&shifted, &zero), SampleFormat::Int24, SampleFormat::Float32)
    }
    fn params(&self) -> Value {
        json!({"reference":self.reference,"clip":self.target,"windowSeconds":2.5,"maxLagSeconds":1})
    }
    fn manual(&self, reference_time: Tick, target_time: Tick) -> Value {
        json!({"reference":self.reference,"clip":self.target,"method":"manual","referenceTime":reference_time.0,"clipTime":target_time.0})
    }
    fn item(&self, id: u64) -> TrackItem {
        self.s.active_sequence().unwrap().find_item(ClipId(id)).unwrap().1.clone()
    }
    fn change(&mut self, id: u64, change: impl FnOnce(&mut TrackItem)) {
        let seq = self.s.state.active_sequence.unwrap();
        change(Arc::make_mut(&mut self.s.project).sequence_mut(seq).unwrap().find_item_mut(ClipId(id)).unwrap().1);
    }
    fn copy_to(&mut self, id: u64, track: usize, start: Tick, link: Option<u64>) -> u64 {
        let mut item = self.item(id);
        let p = Arc::make_mut(&mut self.s.project);
        let id = p.alloc_id();
        item.id = ClipId(id);
        item.start = start;
        item.link = link;
        p.fresh_effect_instance_ids(&mut item.effects).unwrap();
        p.sequence_mut(self.s.state.active_sequence.unwrap()).unwrap().audio_tracks[track].items.push(item);
        id
    }
    fn sources_unchanged(&self) {
        for (index, bytes) in self.originals.iter().enumerate() {
            assert_eq!(&std::fs::read(self.dir.join(format!("recording-{index}.wav"))).unwrap(), bytes);
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rejected_without_edits(s: &mut Session, command: &str, params: Value) {
    let before = s.project.clone();
    let revision = s.revision;
    let history = s.execute("history.list", json!({})).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute(command, params.clone())));
    assert!(result.is_ok(), "panicked on {params}");
    assert!(result.unwrap().is_err(), "accepted {params}");
    assert_eq!(s.project, before);
    assert_eq!(s.revision, revision);
    assert_eq!(s.execute("history.list", json!({})).unwrap(), history);
}

#[test]
fn native_pcm24_and_float32_clap_noise_sync_selects_strong_channel_and_preserves_sources() {
    for inverted in [false, true] {
        let mut f = Fixture::shifted(inverted);
        let before = f.s.project.clone();
        let revision = f.s.revision;
        let params = f.params();
        let report = f.s.execute("audio.sync.inspect", params).unwrap();
        assert_eq!(report["status"], "ready", "{report}");
        assert_eq!(report["referenceChannel"], 1);
        assert_eq!(report["targetChannel"], 0);
        assert_eq!(report["lagSamples"], 12_000);
        assert_eq!(report["offsetTicks"], Tick::from_units(12_000, i64::from(RATE)).0);
        assert_eq!(report["inverted"], inverted);
        assert!(report["confidence"].as_f64().unwrap() >= 8.0);
        assert!(report["distinct"].as_f64().unwrap() >= 1.5);
        assert_eq!(report["verification"]["status"], "verified");
        assert!(report["verification"]["correlation"].as_f64().unwrap().abs() > 0.99);
        assert!(report["coverage"]["target"]["peak"].as_f64().unwrap() > 1.0);
        assert!(Arc::ptr_eq(&f.s.project, &before));
        assert_eq!(f.s.revision, revision);
        f.sources_unchanged();
    }
}

#[test]
fn opposite_polarity_stereo_is_not_averaged_into_silence() {
    let base = noise(4 * RATE as usize, 0x492481);
    let n = 2 * RATE as usize;
    let negative: Vec<_> = base[..n].iter().map(|x| -x).collect();
    let shifted = &base[9600..9600 + n];
    let negative_shifted: Vec<_> = shifted.iter().map(|x| -x).collect();
    let mut f = Fixture::with(&stereo(&base[..n], &negative), &stereo(shifted, &negative_shifted), SampleFormat::Float32, SampleFormat::Float32);
    let params = f.params();
    let report = f.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert_eq!(report["lagSamples"], 9600);
    assert_eq!(report["referenceChannel"], 0);
    assert_eq!(report["targetChannel"], 0);
}

#[test]
fn nonzero_source_ins_starts_and_manual_anchors_agree_exactly() {
    let mut f = Fixture::shifted(false);
    f.change(f.reference, |it| {
        it.start = Tick::from_units(3, 1);
        it.source_in = Tick::from_units(1, 2);
        it.duration = Tick::from_units(2, 1);
    });
    f.change(f.target, |it| {
        it.start = Tick::from_units(5, 1);
        it.source_in = Tick::from_units(3, 4);
        it.duration = Tick::from_units(2, 1);
    });
    let params = f.params();
    let waveform = f.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(waveform["status"], "ready", "{waveform}");
    assert_eq!(waveform["lagSamples"], 24_000);
    assert_eq!(waveform["offsetTicks"], Tick::from_units(-3, 2).0);
    let manual = f.manual(Tick::from_units(3, 2), Tick::from_units(5, 4));
    let manual = f.s.execute("audio.sync.inspect", manual).unwrap();
    assert_eq!(manual["offsetTicks"], waveform["offsetTicks"]);
    assert!(manual["confidence"].is_null());
    assert!(manual["distinct"].is_null());
    assert_eq!(manual["alignment"]["movedClips"][0]["after"]["start"], Tick::from_units(7, 2).0);
}

#[test]
fn unrelated_periodic_or_silent_waveforms_do_not_create_confident_proposals() {
    let n = 2 * RATE as usize;
    let a = noise(n, 0x789ab);
    let b = noise(n, 0x9abcde);
    let mut f = Fixture::with(&stereo(&a, &a), &stereo(&b, &b), SampleFormat::Float32, SampleFormat::Int24);
    let params = f.params();
    let report = f.s.execute("audio.sync.inspect", params.clone()).unwrap();
    assert_eq!(report["status"], "needsManual", "{report}");
    assert_eq!(report["verification"]["status"], "poorAlignedCorrelation");
    rejected_without_edits(&mut f.s, "audio.sync.prepare", params);
    let params = f.manual(Tick::from_units(1, 2), Tick::from_units(1, 4));
    assert!(f.s.execute("audio.sync.prepare", params).unwrap()["plan"].as_u64().is_some());
    let tone: Vec<_> = (0..n).map(|i| (std::f64::consts::TAU * 400.0 * i as f64 / f64::from(RATE)).sin() as f32 * 0.25).collect();
    let mut periodic = Fixture::with(&stereo(&tone, &tone), &stereo(&tone, &tone), SampleFormat::Float32, SampleFormat::Float32);
    let params = periodic.params();
    let report = periodic.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(report["status"], "needsManual", "{report}");
    assert_eq!(report["verification"]["status"], "periodicAmbiguity");
    assert_eq!(report["verification"]["periodicity"]["bothPeriodic"], true);
    let zero = vec![0.0; n * 2];
    let mut silent = Fixture::with(&zero, &zero, SampleFormat::Float32, SampleFormat::Float32);
    let params = silent.params();
    rejected_without_edits(&mut silent.s, "audio.sync.inspect", params);
    let params = silent.manual(Tick::ZERO, Tick::ZERO);
    assert_eq!(silent.s.execute("audio.sync.inspect", params).unwrap()["status"], "ready");
}

#[test]
fn leading_trim_moves_linked_partners_preserves_effects_and_is_one_undo() {
    let mut f = Fixture::shifted(false);
    f.change(f.reference, |it| it.start = Tick::ZERO);
    f.change(f.target, |it| {
        it.start = Tick::from_units(1, 4);
        it.link = Some(9001);
        let parameter = it
            .effects
            .iter_mut()
            .flat_map(|effect| effect.params.values_mut())
            .find(|parameter| matches!(parameter.value, filmcraft_project::ParamValue::Float(_)))
            .unwrap();
        parameter.keyframes = vec![
            filmcraft_project::Keyframe::new(Tick::from_units(1, 2), filmcraft_project::ParamValue::Float(-6.0)),
            filmcraft_project::Keyframe::new(Tick::from_units(2, 1), filmcraft_project::ParamValue::Float(-12.0)),
        ];
    });
    let partner = f.copy_to(f.target, 2, Tick::from_units(1, 2), Some(9001));
    let effects = f.item(f.target).effects;
    let before = f.s.project.clone();
    let undo = f.s.history.undo.len();
    let mut params = f.manual(Tick::ZERO, Tick::from_units(1, 1));
    params["muteReference"] = json!(true);
    let proposal = f.s.execute("audio.sync.prepare", params).unwrap();
    assert_eq!(f.s.project, before);
    assert_eq!(f.s.history.undo.len(), undo);
    let evidence = &proposal["preparation"]["evidence"];
    assert!(evidence["warnings"].as_array().unwrap().iter().any(|w| w["code"] == "leading_trim"));
    let plan = proposal["plan"].as_u64().unwrap();
    f.s.execute("agent.apply", json!({"plan":plan})).unwrap();
    assert_eq!(f.s.history.undo.len(), undo + 1);
    assert_eq!(f.item(f.reference).start, Tick::ZERO);
    assert!(!f.item(f.reference).enabled);
    let target = f.item(f.target);
    assert_eq!(target.start, Tick::ZERO);
    assert_eq!(target.source_in, Tick::from_units(1, 1));
    assert_eq!(target.duration, Tick::from_units(2, 1));
    assert_eq!(target.effects, effects);
    let partner = f.item(partner);
    assert_eq!(partner.start, Tick::ZERO);
    assert_eq!(partner.source_in, Tick::from_units(3, 4));
    assert_eq!(partner.duration, Tick::from_units(9, 4));
    f.s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(f.s.project, before);
    f.sources_unchanged();
}

#[test]
fn locks_transitions_collisions_linked_reference_and_full_removal_fail_atomically() {
    for failure in ["locked", "transition", "collision", "reference_link", "speed", "reverse", "hold", "source_range", "remove_all"] {
        let mut f = Fixture::shifted(false);
        let sequence = f.s.state.active_sequence.unwrap();
        let offset = Tick::from_units(1, 2).0;
        match failure {
            "locked" => Arc::make_mut(&mut f.s.project).sequence_mut(sequence).unwrap().audio_tracks[1].locked = true,
            "transition" => {
                let id = TransitionId(Arc::make_mut(&mut f.s.project).alloc_id());
                let fx = filmcraft_project::effect::find_effect("constant_power")
                    .or_else(|| filmcraft_project::effect::find_effect("cross_dissolve"))
                    .unwrap()
                    .instance();
                Arc::make_mut(&mut f.s.project).sequence_mut(sequence).unwrap().audio_tracks[1].transitions.push(Transition {
                    id,
                    effect: fx,
                    start: Tick::from_units(2, 1),
                    duration: Tick::from_units(1, 10),
                    from: None,
                    to: Some(ClipId(f.target)),
                    align: TransitionAlign::StartAtCut,
                    reverse: false,
                });
            }
            "collision" => {
                f.copy_to(f.target, 1, Tick::from_units(5, 1), None);
            }
            "reference_link" => {
                f.change(f.reference, |it| it.link = Some(9002));
                f.change(f.target, |it| it.link = Some(9002));
            }
            "speed" => f.change(f.target, |it| it.speed = 1.1),
            "reverse" => f.change(f.target, |it| it.reverse = true),
            "hold" => f.change(f.target, |it| it.frame_hold = Some(Tick::ZERO)),
            "source_range" => f.change(f.target, |it| it.source_in = Tick::from_units(3, 1)),
            "remove_all" => {}
            _ => unreachable!(),
        }
        let params = json!({"reference":f.reference,"clip":f.target,"offsetTicks":if failure=="remove_all"{-Tick::from_units(6,1).0}else{offset}});
        rejected_without_edits(&mut f.s, "audio.sync.align", params);
    }
}

#[test]
fn mute_reference_audio_is_selective_and_never_disables_camera_video_or_main_dji_stream() {
    let mut f = Fixture::shifted(false);
    let camera_item = f.item(f.reference).item;
    {
        let project = Arc::make_mut(&mut f.s.project);
        let media = project.item_mut(camera_item).unwrap().as_media_mut().unwrap();
        media.info.audio_streams.push(media.info.audio_streams[0].clone());
    }
    f.change(f.reference, |it| {
        it.audio_stream = 1;
        it.link = Some(222);
    });
    let main = f.copy_to(f.reference, 2, Tick::from_units(2, 1), Some(222));
    f.change(main, |it| it.audio_stream = 0);
    let mut camera = f.item(main);
    let project = Arc::make_mut(&mut f.s.project);
    camera.id = ClipId(project.alloc_id());
    camera.effects.clear();
    let camera_id = camera.id.0;
    project.sequence_mut(f.s.state.active_sequence.unwrap()).unwrap().video_tracks[0].items.push(camera);
    let params = json!({"reference":f.reference,"clip":f.target,"offsetTicks":0,"muteReference":true});
    let report = f.s.execute("audio.sync.align", params).unwrap();
    assert_eq!(report["mutedReferenceClips"], json!([f.reference]));
    assert!(!f.item(f.reference).enabled);
    assert!(f.item(main).enabled && f.item(camera_id).enabled);
    // Selecting video instead prefers only its linked stream-1 scratch, even though stream 0
    // is also linked. Explicit audio clip selection above was never inferred from stream order.
    f.change(f.reference, |it| it.enabled = true);
    let report = f.s.execute("audio.sync.align", json!({"reference":camera_id,"clip":f.target,"offsetTicks":0,"muteReference":true})).unwrap();
    assert_eq!(report["mutedReferenceClips"], json!([f.reference]));
    assert!(!f.item(f.reference).enabled);
    assert!(f.item(main).enabled && f.item(camera_id).enabled);
    let track = f.s.active_sequence().unwrap().find_item(ClipId(f.reference)).unwrap().0;
    Arc::make_mut(&mut f.s.project).sequence_mut(f.s.state.active_sequence.unwrap()).unwrap().track_mut(track).unwrap().locked = true;
    rejected_without_edits(&mut f.s, "audio.sync.align", json!({"reference":camera_id,"clip":f.target,"offsetTicks":0,"muteReference":true}));
}

#[test]
fn stale_plan_refuses_and_manual_anchor_bounds_and_parameters_are_strict() {
    let mut f = Fixture::shifted(false);
    let manual = f.manual(Tick::from_units(1, 1), Tick::from_units(3, 4));
    let plan = f.s.execute("audio.sync.prepare", manual.clone()).unwrap()["plan"].as_u64().unwrap();
    f.s.edit("Intervening user edit", |project, _| {
        project.name = "user edit".into();
        Ok(())
    })
    .unwrap();
    rejected_without_edits(&mut f.s, "agent.apply", json!({"plan":plan}));
    let mut invalid = vec![
        json!({"reference":f.reference,"clip":f.target,"method":"wrong"}),
        json!({"reference":0,"clip":f.target,"method":"manual","referenceTime":0,"clipTime":0}),
        json!({"reference":f.reference,"clip":f.reference}),
        json!({"reference":f.reference,"clip":f.target,"windowSeconds":1e300}),
        json!({"reference":f.reference,"clip":f.target,"windowSeconds":0}),
        json!({"reference":f.reference,"clip":f.target,"maxLagSeconds":-1}),
        json!({"reference":f.reference,"clip":f.target,"offsetSamples":i64::MIN}),
        json!({"reference":f.reference,"clip":f.target,"offsetSamples":i64::MAX}),
        json!({"reference":f.reference,"clip":f.target,"referenceTime":0,"clipTime":0}),
        json!({"reference":f.reference,"clip":f.target,"method":"manual","referenceTime":0}),
        json!({"reference":f.reference,"clip":f.target,"muteReference":"yes"}),
        json!({"reference":f.reference,"clip":f.target,"unknown":true}),
        f.manual(Tick(-1), Tick::ZERO),
        f.manual(Tick::from_units(3, 1), Tick::ZERO),
        f.manual(Tick::ZERO, Tick::from_units(3, 1)),
    ];
    invalid.push(json!({"reference":u64::MAX,"clip":f.target}));
    for p in invalid {
        rejected_without_edits(&mut f.s, "audio.sync.inspect", p);
    }
    for offset in [i64::MIN, i64::MAX] {
        rejected_without_edits(&mut f.s, "audio.sync.align", json!({"reference":f.reference,"clip":f.target,"offsetTicks":offset}));
    }
    let params = f.params();
    f.change(f.target, |it| it.source_channels = vec![u16::MAX]);
    rejected_without_edits(&mut f.s, "audio.sync.inspect", params);
}

#[test]
fn source_channel_selection_is_authoritative_and_subsample_source_ins_are_accounted_for() {
    let mut f = Fixture::shifted(false);
    f.change(f.reference, |it| it.source_channels = vec![0]);
    let params = f.params();
    rejected_without_edits(&mut f.s, "audio.sync.inspect", params);
    f.change(f.reference, |it| {
        it.source_channels = vec![1];
        it.source_in = Tick(1);
        it.duration = Tick::from_units(2, 1);
    });
    f.change(f.target, |it| {
        it.source_in = Tick(3);
        it.duration = Tick::from_units(2, 1);
    });
    let params = f.params();
    let report = f.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert_eq!(report["offsetTicks"], Tick::from_units(12_000, i64::from(RATE)).0 + 2);
    assert_eq!(report["coverage"]["reference"]["sourceStart"], Tick::from_units(1, i64::from(RATE)).0);
}

#[test]
fn oversized_linked_groups_and_nested_subclip_escape_are_refused_before_edit() {
    let mut f = Fixture::shifted(false);
    f.change(f.target, |it| it.link = Some(9222));
    for _ in 0..MAX_LINKED {
        f.copy_to(f.target, 2, Tick::from_units(2, 1), Some(9222));
    }
    rejected_without_edits(&mut f.s, "audio.sync.align", json!({"reference":f.reference,"clip":f.target,"offsetTicks":1}));
    let mut f = Fixture::shifted(false);
    let source = f.item(f.target).item;
    let p = Arc::make_mut(&mut f.s.project);
    let inner = p.add_item(
        "parent subclip",
        filmcraft_project::Label::Forest,
        ItemKind::Subclip { parent: source, range: TimeRange::new(Tick::from_units(1, 1), Tick::from_units(1, 1)), restrict_trims: true },
        None,
    );
    let outer = p.add_item(
        "escaped subclip",
        filmcraft_project::Label::Forest,
        ItemKind::Subclip { parent: inner, range: TimeRange::new(Tick::ZERO, Tick::from_units(3, 1)), restrict_trims: true },
        None,
    );
    f.change(f.target, |it| it.item = outer);
    let params = f.manual(Tick::from_units(1, 1), Tick::from_units(1, 1));
    rejected_without_edits(&mut f.s, "audio.sync.prepare", params);
}

struct MultipleStreams {
    info: filmcraft_media::MediaInfo,
    streams: [filmcraft_media::SharedSource; 2],
}
impl filmcraft_media::MediaSource for MultipleStreams {
    fn info(&self) -> &filmcraft_media::MediaInfo {
        &self.info
    }
    fn video_frame(&self, request: filmcraft_media::FrameRequest) -> filmcraft_media::Result<Arc<filmcraft_frame::VideoFrame>> {
        self.streams[0].video_frame(request)
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        self.streams[0].audio(start, frames, rate)
    }
    fn audio_stream(&self, stream: usize, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
        self.streams.get(stream).ok_or(filmcraft_media::MediaError::NoStream("audio"))?.audio(start, frames, rate)
    }
}

#[test]
fn explicit_audio_stream_is_read_and_video_reference_reports_default_secondary_stream() {
    let mut f = Fixture::shifted(false);
    let correct = filmcraft_codecs::open_bytes("scratch.wav", f.originals[0].clone().into()).unwrap();
    let zero = filmcraft_export::write_wav(&vec![0.0; 3 * RATE as usize * 2], 2, RATE, SampleFormat::Float32).unwrap();
    let wrong = filmcraft_codecs::open_bytes("main.wav", zero.into()).unwrap();
    let mut info = correct.info().clone();
    info.audio_streams.insert(0, wrong.info().audio_streams[0].clone());
    let source: filmcraft_media::SharedSource = Arc::new(MultipleStreams { info: info.clone(), streams: [wrong, correct] });
    let item = f.item(f.reference).item;
    {
        let project = Arc::make_mut(&mut f.s.project);
        let media = project.item_mut(item).unwrap().as_media_mut().unwrap();
        // Keep this test provider isolated from disk re-openers. Its two streams both decode
        // actual native WAV samples; the wrapper only represents their container stream slots.
        media.media = filmcraft_project::MediaRef::Generator(filmcraft_media::generators::Generator::BarsAndTone);
        media.info = info;
    }
    f.s.media.insert(item, source);
    f.change(f.reference, |it| it.audio_stream = 1);
    let params = f.params();
    let report = f.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert_eq!(report["reference"]["audioStream"], 1);
    assert_eq!(report["coverage"]["reference"]["stream"], 1);
    assert_eq!(report["lagSamples"], 12_000);
    f.change(f.reference, |it| it.audio_stream = 0);
    let params = f.params();
    rejected_without_edits(&mut f.s, "audio.sync.inspect", params);
    {
        let project = Arc::make_mut(&mut f.s.project);
        let seq = project.sequence_mut(f.s.state.active_sequence.unwrap()).unwrap();
        let clip = seq.audio_tracks[0].items.remove(0);
        seq.video_tracks[0].items.push(clip);
    }
    let params = f.params();
    let report = f.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert_eq!(report["reference"]["audioStream"], 1);
    assert!(report["warnings"].as_array().unwrap().iter().any(|w| w["code"] == "reference_stream"));
}

#[test]
fn unrestricted_subclip_trims_remain_valid_and_tick_range_overflow_is_refused() {
    let mut f = Fixture::shifted(false);
    let source = f.item(f.target).item;
    let subclip = Arc::make_mut(&mut f.s.project).add_item(
        "unrestricted",
        filmcraft_project::Label::Forest,
        ItemKind::Subclip { parent: source, range: TimeRange::new(Tick::from_units(1, 1), Tick::from_units(1, 1)), restrict_trims: false },
        None,
    );
    f.change(f.target, |it| it.item = subclip);
    let params = f.manual(Tick::from_units(1, 2), Tick::from_units(1, 4));
    let report = f.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(report["status"], "ready");
    assert_eq!(report["offsetTicks"], Tick::from_units(1, 4).0);
    f.change(f.target, |it| it.source_in = Tick(i64::MAX));
    let params = f.manual(Tick::ZERO, Tick::ZERO);
    rejected_without_edits(&mut f.s, "audio.sync.prepare", params);
    for (start, duration) in
        [(Tick::MAX, Tick(1)), (Tick(i64::MAX), Tick(i64::MAX)), (Tick::ZERO, Tick(i64::MAX)), (Tick(-1), Tick(1)), (Tick::ZERO, Tick(i64::MIN))]
    {
        assert!(std::panic::catch_unwind(|| range(start, duration)).unwrap().is_err());
    }
}

#[test]
fn aligned_verification_requires_finite_meaningful_overlap_but_accepts_inversion_and_dc() {
    let x = noise(2 * RATE as usize, 0x184afa);
    let y: Vec<_> = x.iter().map(|x| -x * 0.7 + 0.2).collect();
    let (ready, report) = verify_waveform(&x, &y, 0);
    assert!(ready, "{report}");
    assert!(report["correlation"].as_f64().unwrap() < -0.999);
    assert_eq!(report["overlapSamples"], x.len());
    for (a, b, lag) in [
        (vec![1.0; 5000], vec![2.0; 5000], 0),
        (vec![0.1; 10], vec![0.1; 10], 0),
        (vec![0.1; 5000], vec![0.1; 5000], i64::MIN),
        (vec![0.1; 5000], vec![0.1; 5000], i64::MAX),
        (vec![f32::NAN; 5000], vec![0.1; 5000], 0),
    ] {
        assert!(!std::panic::catch_unwind(|| verify_waveform(&a, &b, lag)).unwrap().0);
    }
    let unrelated = noise(x.len(), 0x27199f);
    assert!(!verify_waveform(&x, &unrelated, 164).0);
}

#[test]
fn differently_filtered_room_recording_with_reflections_passes_aligned_verification() {
    let n = 3 * RATE as usize;
    let base = noise(n + 12_000, 0x72abfe);
    let room_noise = noise(n, 0x994111);
    let mut memory = 0.0;
    let filtered: Vec<_> = base
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            memory += 0.25 * (x - memory);
            memory + if i >= 240 { base[i - 240] * 0.15 } else { 0.0 }
        })
        .collect();
    let target: Vec<_> = filtered[12_000..12_000 + n].iter().zip(room_noise).map(|(x, noise)| x * 2.5 + noise * 0.1).collect();
    let zero = vec![0.0; n];
    let mut f = Fixture::with(&stereo(&base[..n], &zero), &stereo(&target, &zero), SampleFormat::Int24, SampleFormat::Float32);
    let params = f.params();
    let report = f.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert!((report["lagSamples"].as_i64().unwrap() - 12_000).abs() <= 4, "{report}");
    assert!(report["verification"]["correlation"].as_f64().unwrap().abs() >= 0.25, "{report}");
}

#[test]
fn short_shared_clap_can_verify_when_long_unrelated_room_activity_dilutes_global_correlation() {
    let n = 12 * RATE as usize;
    let base = noise(n + 12_000, 0x914fff);
    let background_a = noise(n, 0x24aada);
    let background_b = noise(n, 0x52abcb);
    let event = 72_000..73_440;
    let reference: Vec<_> = (0..n).map(|i| if event.contains(&i) { base[i] * 10.0 } else { background_a[i] }).collect();
    let target: Vec<_> = (0..n).map(|i| if event.contains(&(i + 12_000)) { base[i + 12_000] * 7.0 } else { background_b[i] * 0.7 }).collect();
    let zero = vec![0.0; n];
    let mut f = Fixture::with(&stereo(&reference, &zero), &stereo(&target, &zero), SampleFormat::Float32, SampleFormat::Float32);
    let mut params = f.params();
    params["windowSeconds"] = json!(12);
    let report = f.s.execute("audio.sync.inspect", params).unwrap();
    assert_eq!(report["status"], "ready", "{report}");
    assert_eq!(report["lagSamples"], 12_000);
    assert!(report["verification"]["wholeOverlap"]["correlation"].as_f64().unwrap().abs() < 0.25, "{report}");
    assert_eq!(report["verification"]["acceptedBy"], "sharedEvent");
    assert!(report["verification"]["sharedEvent"]["correlation"].as_f64().unwrap().abs() >= 0.25, "{report}");
}

#[test]
fn periodicity_guard_rejects_sustained_tones_and_loops_without_aliasing_away_evidence() {
    for frequency in [400.0, 997.0, 8000.0] {
        let tone: Vec<_> = (0..RATE as usize).map(|i| (std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE)).sin() as f32 * 0.5).collect();
        let rms = PairStats::read(&tone, &tone).unwrap().left_rms;
        let (repeated, report) = periodicity(&tone, rms);
        assert!(repeated, "{frequency} {report}");
        assert!(report["samples"].as_u64().unwrap() <= 8192);
        if frequency == 8000.0 {
            assert_eq!(report["status"], "repeatedSubsampleCycle");
        }
        let (accepted, verification) = verify_waveform(&tone, &tone, 0);
        assert!(!accepted, "{frequency} {verification}");
        assert_eq!(verification["status"], "periodicAmbiguity");
    }
    let loop_segment = noise(4800, 0x791441);
    let repeated: Vec<_> = loop_segment.iter().copied().cycle().take(RATE as usize).collect();
    let rms = PairStats::read(&repeated, &repeated).unwrap().left_rms;
    assert!(periodicity(&repeated, rms).0);
    let clap = noise(RATE as usize, 0x746321);
    let rms = PairStats::read(&clap, &clap).unwrap().left_rms;
    assert!(!periodicity(&clap, rms).0);
    assert!(!periodicity(&[f32::NAN; 4800], 0.5).0);
    assert!(!periodicity(&[], 0.5).0);
    let long = noise(RATE as usize * 4, 0x888641);
    let (_, report) = periodicity(&long, 0.5);
    assert!(report["samples"].as_u64().unwrap() <= 8192);
}
