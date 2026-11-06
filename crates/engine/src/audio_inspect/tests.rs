use super::*;
use filmcraft_media::{FrameRequest, MediaInfo, MediaSource};
use filmcraft_project::{ClipId, ItemKind, Label};
use filmcraft_time::TimeRange;
type Calls = Arc<Mutex<Vec<(usize, i64, usize)>>>;
struct Synthetic {
    info: MediaInfo,
    mode: &'static str,
    calls: Calls,
}
impl MediaSource for Synthetic {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, _: FrameRequest) -> filmcraft_media::Result<Arc<filmcraft_frame::VideoFrame>> {
        Err(filmcraft_media::MediaError::NoStream("video"))
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        self.audio_stream(0, start, frames, rate)
    }
    fn audio_stream(&self, stream: usize, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        self.calls.lock().unwrap().push((stream, start, frames));
        if self.mode == "fail" {
            return Err(filmcraft_media::MediaError::Io("synthetic decoder failure".into()));
        }
        let count = self.info.audio_streams.get(stream).ok_or(filmcraft_media::MediaError::NoStream("audio"))?.channels as usize;
        let mut channels = vec![Vec::with_capacity(frames); count];
        for i in 0..frames {
            let t = (start + i as i64) as f32 / rate as f32;
            let a = (t * std::f32::consts::TAU * 440.0).sin() * 0.25;
            let b = (t * std::f32::consts::TAU * 613.0).sin() * 0.25;
            let (l, r) = match self.mode {
                "left" => (a, 0.0),
                "right" => (0.0, a),
                "inverted" => (a, -a),
                "unrelated" => (a, b),
                "silence" => (0.0, 0.0),
                "over" => (a * 8.0, a * 8.0),
                "ramp" => (t / 10.0, -t / 10.0),
                "nan" => (f32::NAN, a),
                _ => (a, a),
            };
            for (j, c) in channels.iter_mut().enumerate() {
                c.push(if (j + stream).is_multiple_of(2) { l } else { r });
            }
        }
        Ok(AudioBuffer { sample_rate: rate, channels })
    }
}
fn fixture(mode: &'static str, channels: u32, seconds: i64) -> (Session, u64, Calls) {
    let mut s = Session::default();
    let id = s.execute("file.newBarsAndTone", json!({"seconds":seconds,"width":16,"height":16})).unwrap()["item"].as_u64().unwrap();
    let info = {
        let p = Arc::make_mut(&mut s.project);
        let media = p.item_mut(ItemId(id)).unwrap().as_media_mut().unwrap();
        media.info.audio_streams[0].channels = channels;
        media.info.audio_streams.push(media.info.audio_streams[0].clone());
        media.info.clone()
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut info = info;
    if mode == "missing" {
        info.audio_streams.clear();
    }
    s.media.insert(ItemId(id), Arc::new(Synthetic { info, mode, calls: calls.clone() }));
    (s, id, calls)
}
fn source(id: u64) -> Value {
    json!({"subject":{"target":"source","item":id}})
}
fn clip_fixture(mode: &'static str) -> (Session, u64, u64) {
    let (mut s, item, _) = fixture(mode, 2, 20);
    s.execute("file.newSequence", json!({"fps":24,"width":16,"height":16})).unwrap();
    let placed = s
        .execute(
            "timeline.place",
            json!({"item":item,"track":"A1","video":false,"audio":true,"frame":120,"duration":{"frame":24},"sourceIn":Tick::from_units(2,1).0}),
        )
        .unwrap();
    (s, item, placed["clips"][0].as_u64().unwrap())
}
#[test]
fn one_sided_microphones_and_stereo_evidence_never_mutate() {
    for (mode, expected, finding) in [
        ("left", Some("left"), "left_only"),
        ("right", Some("right"), "right_only"),
        ("dual", None, "dual_mono"),
        ("inverted", None, "inverted_polarity"),
        ("unrelated", None, "uncorrelated_stereo"),
        ("silence", None, "silent"),
    ] {
        let (mut s, id, _) = fixture(mode, 2, 1);
        let before = s.project.clone();
        let state = serde_json::to_value(&s.state).unwrap();
        let rev = s.revision;
        let r = s.execute("audio.inspect", source(id)).unwrap();
        assert_eq!(r["recommendation"]["mode"].as_str(), expected, "{mode}: {r}");
        assert!(r["findings"].as_array().unwrap().iter().any(|f| f["kind"] == finding), "{mode}: {r}");
        assert!(Arc::ptr_eq(&s.project, &before));
        assert_eq!(s.revision, rev);
        assert_eq!(serde_json::to_value(&s.state).unwrap(), state);
        if mode == "inverted" {
            assert!(r["stereo"]["correlation"].as_f64().unwrap() < -0.99);
            assert!(r["stereo"]["completeMonoCancellation"].as_bool().unwrap());
        }
    }
    let (mut s, id, _) = fixture("dual", 1, 1);
    let r = s.execute("audio.inspect", source(id)).unwrap();
    assert!(r["stereo"].is_null());
    assert!(r["recommendation"]["mode"].is_null());
    assert!(r["loudness"]["integratedLufs"].is_number());
    assert!(r["loudness"]["shortTermLufs"].is_null());
    assert!(r["loudness"]["loudnessRangeLu"].is_null());
}
#[test]
fn preview_float_wav_retains_headroom_and_native_decoder_reads_it() {
    let (mut s, id, _) = fixture("over", 2, 1);
    let before = s.project.clone();
    let r = s.execute("audio.preview", source(id)).unwrap();
    assert_eq!(r["audio"]["format"], "ieee_float32");
    assert!(r["headroom"]["peak"].as_f64().unwrap() > 1.9);
    let bytes = base64::engine::general_purpose::STANDARD.decode(r["audio"]["base64"].as_str().unwrap()).unwrap();
    assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 3);
    let decoded = filmcraft_media::open_bytes("preview.wav", bytes.into(), &[]).unwrap().audio(0, 4800, 48000).unwrap();
    assert_eq!(decoded.channels.len(), 2);
    assert!(decoded.channels[0].iter().any(|x| x.abs() > 1.9));
    assert!(Arc::ptr_eq(&s.project, &before));
}
#[test]
fn source_streams_subclip_bounds_and_clip_reverse_mapping_are_respected() {
    let (mut s, id, calls) = fixture("left", 2, 20);
    let r = s.execute("audio.inspect", json!({"subject":{"item":id},"stream":1,"start":Tick::from_units(2,1).0,"end":Tick::from_units(3,1).0})).unwrap();
    assert_eq!(r["recommendation"]["mode"], "right");
    assert!(calls.lock().unwrap().iter().all(|c| c.0 == 1 && c.1 >= 96000));
    let sub = Arc::make_mut(&mut s.project).add_item(
        "sub",
        Label::Iris,
        ItemKind::Subclip { restrict_trims: true, parent: ItemId(id), range: TimeRange::from_bounds(Tick::from_units(4, 1), Tick::from_units(5, 1)) },
        None,
    );
    let r = s.execute("audio.inspect", source(sub.0)).unwrap();
    assert_eq!(r["start"], Tick::from_units(4, 1).0);
    assert_eq!(r["end"], Tick::from_units(5, 1).0);
    let (mut s, _, clip) = clip_fixture("ramp");
    let seq = s.state.active_sequence.unwrap();
    s.edit("map one reversed channel", |p, _| {
        let it = p.sequence_mut(seq).unwrap().find_item_mut(ClipId(clip)).unwrap().1;
        it.reverse = true;
        it.source_channels = vec![0];
        Ok(())
    })
    .unwrap();
    let r: Request = bounded_decode(&json!({"subject":{"target":"clip","clip":clip,"effects":"pre"}}), "test").unwrap();
    let reader = Reader::new(&s, &r, false, false).unwrap();
    let b = reader.block(0, 4800).unwrap();
    assert_eq!(reader.start, Tick::from_units(5, 1));
    assert!(b.channels[0][0] > b.channels[0][4799]);
    assert_eq!(b.channels[0], b.channels[1]);
    let r = s.execute("audio.inspect", json!({"subject":{"clip":clip,"effects":"pre"}})).unwrap();
    assert!(r["recommendation"]["mode"].is_null());
    assert_eq!(r["routing"]["sourceChannels"], json!([0]));
}
#[test]
fn post_clip_controls_and_frozen_plan_ab_preview_use_exact_snapshots() {
    let (mut s, _, clip) = clip_fixture("dual");
    let plan = s
        .execute("agent.plan", json!({"steps":[{"id":"effects.setParam","params":{"clip":clip,"effect":"volume","param":"level","value":-12.0}}]}))
        .unwrap()["plan"]
        .as_u64()
        .unwrap();
    let revision = s.revision;
    let before = s.project.clone();
    let pair = s.execute("audio.inspect", json!({"subject":{"clip":clip,"plan":plan},"compare":true})).unwrap();
    let a = pair["before"]["channels"][0]["rmsDbfs"].as_f64().unwrap();
    let b = pair["after"]["channels"][0]["rmsDbfs"].as_f64().unwrap();
    assert!((a - b - 12.0).abs() < 0.05, "{pair}");
    let pre = s.execute("audio.inspect", json!({"subject":{"clip":clip,"plan":plan,"effects":"pre"}})).unwrap();
    assert!((pre["channels"][0]["rmsDbfs"].as_f64().unwrap() - a).abs() < 0.05);
    s.execute("clip.rename", json!({"clip":clip,"name":"Owner continues"})).unwrap();
    let again = s.execute("audio.inspect", json!({"subject":{"clip":clip,"plan":plan},"compare":true})).unwrap();
    assert_eq!(pair, again);
    assert_eq!(pair["after"]["revision"], revision);
    assert!(!Arc::ptr_eq(&s.project, &before));
    let p = s.execute("audio.preview", json!({"subject":{"clip":clip,"plan":plan},"compare":true})).unwrap();
    assert_eq!(p["before"]["audio"]["frames"], p["after"]["audio"]["frames"]);
    assert_ne!(p["before"]["audio"]["base64"], p["after"]["audio"]["base64"]);
}
#[test]
fn bounded_long_defaults_decode_errors_and_hostile_parameters_are_explicit() {
    let (mut s, id, calls) = fixture("silence", 2, 1000);
    let r = s.execute("audio.inspect", source(id)).unwrap();
    assert_eq!(r["frames"], 30 * 48000);
    assert_eq!(r["coverage"]["entireSubject"], false);
    assert!(calls.lock().unwrap().iter().all(|c| c.2 <= 4096));
    for p in [
        json!({"subject":{"item":id},"start":-1}),
        json!({"subject":{"item":id},"end":i64::MAX}),
        json!({"subject":{"item":id},"end":Tick::from_units(301,1).0}),
        json!({"subject":{"item":id},"snapshot":"bad"}),
        json!({"subject":{"item":id},"compare":true}),
        json!({"subject":{"item":id},"stream":u32::MAX}),
        json!({"subject":{"item":id},"unknown":1}),
    ] {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.execute("audio.inspect", p.clone())));
        assert!(r.is_ok());
        assert!(r.unwrap().is_err(), "{p}");
    }
    assert!(s.execute("audio.preview", json!({"subject":{"item":id},"end":Tick::from_units(11,1).0})).is_err());
    for mode in ["fail", "nan"] {
        let (mut s, id, _) = fixture(mode, 2, 1);
        assert!(s.execute("audio.inspect", source(id)).is_err());
        assert!(s.execute("audio.preview", source(id)).is_err());
    }
}
#[test]
fn async_jobs_are_frozen_cancellable_and_document_scoped() {
    let (mut s, id, _) = fixture("left", 2, 1);
    let rev = s.revision;
    let project = s.project.clone();
    let job = s.execute("audio.inspectAsync", source(id)).unwrap()["job"].as_u64().unwrap();
    let deadline = web_time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let r = s.execute("audio.inspectStatus", json!({"job":job})).unwrap();
        if r["status"] != "running" {
            assert_eq!(r["status"], "completed", "{r}");
            assert_eq!(r["result"]["recommendation"]["mode"], "left");
            assert_eq!(r["result"]["revision"], rev);
            break;
        }
        assert!(web_time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(Arc::ptr_eq(&project, &s.project));
    let (mut s, id, _) = fixture("left", 2, 1000);
    let job = s.execute("audio.inspectAsync", source(id)).unwrap()["job"].as_u64().unwrap();
    s.execute("jobs.cancel", json!({"job":job})).unwrap();
    loop {
        let r = s.execute("audio.inspectStatus", json!({"job":job})).unwrap();
        if r["status"] != "running" {
            assert_eq!(r["status"], "cancelled", "{r}");
            break;
        }
        assert!(web_time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    s.execute("file.newProject", json!({"name":"Other"})).unwrap();
    assert!(s.execute("audio.inspectStatus", json!({"job":job})).is_err());
}
#[test]
fn native_pcm_left_mic_is_measured_without_modifying_source_bytes() {
    let dir = crate::media_test_util::tmp_dir("audio-native-left-mic");
    let file = dir.join("mic.wav");
    let mut samples = Vec::with_capacity(96000);
    for n in 0..48000 {
        samples.push((n as f32 * std::f32::consts::TAU * 440.0 / 48000.0).sin() * 0.3);
        samples.push(0.0);
    }
    let bytes = filmcraft_media::wav::write_wav16(&samples, 2, 48000);
    std::fs::write(&file, &bytes).unwrap();
    let mut s = Session::default();
    let item = s.execute("file.import", json!({"paths":[file]})).unwrap()["items"][0].as_u64().unwrap();
    let before = s.project.clone();
    let revision = s.revision;
    let r = s.execute("audio.inspect", source(item)).unwrap();
    assert_eq!(r["recommendation"]["mode"], "left");
    assert!(r["channels"][1]["rmsDbfs"].is_null());
    let p = s.execute("audio.preview", source(item)).unwrap();
    assert_eq!(p["audio"]["channels"], 2);
    assert!(p["headroom"]["peak"].as_f64().unwrap() > 0.29);
    assert!(Arc::ptr_eq(&before, &s.project));
    assert_eq!(revision, s.revision);
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn generated_pcm24_and_hot_float32_native_wavs_keep_format_and_recoverable_headroom() {
    let dir = crate::media_test_util::tmp_dir("audio-pcm24-float32");
    let frames = 48000usize;
    let mut pcm = Vec::with_capacity(44 + frames * 6);
    pcm.extend_from_slice(b"RIFF");
    pcm.extend_from_slice(&(36u32 + frames as u32 * 6).to_le_bytes());
    pcm.extend_from_slice(b"WAVEfmt ");
    pcm.extend_from_slice(&16u32.to_le_bytes());
    pcm.extend_from_slice(&1u16.to_le_bytes());
    pcm.extend_from_slice(&2u16.to_le_bytes());
    pcm.extend_from_slice(&RATE.to_le_bytes());
    pcm.extend_from_slice(&(RATE * 6).to_le_bytes());
    pcm.extend_from_slice(&6u16.to_le_bytes());
    pcm.extend_from_slice(&24u16.to_le_bytes());
    pcm.extend_from_slice(b"data");
    pcm.extend_from_slice(&(frames as u32 * 6).to_le_bytes());
    let mut hot = vec![vec![0.0; frames], vec![0.0; frames]];
    for i in 0..frames {
        let x = (i as f32 * std::f32::consts::TAU * 440.0 / 48000.0).sin();
        let sample = (x * 0.3 * 8388608.0).round() as i32;
        pcm.extend_from_slice(&sample.to_le_bytes()[..3]);
        pcm.extend_from_slice(&[0, 0, 0]);
        hot[0][i] = x * 2.0;
    }
    let float = float_wav(&hot).unwrap();
    for (name, bytes, bits, format, over) in [("pcm24.wav", pcm, 24, "integer_pcm", false), ("float32.wav", float, 32, "floating_point", true)] {
        let path = dir.join(name);
        std::fs::write(&path, &bytes).unwrap();
        let mut s = Session::default();
        let item = s.execute("file.import", json!({"paths":[path]})).unwrap()["items"][0].as_u64().unwrap();
        let r = s.execute("audio.inspect", source(item)).unwrap();
        assert_eq!(r["sourceFormat"]["bitDepth"], bits);
        assert_eq!(r["sourceFormat"]["sampleFormat"], format);
        assert_eq!(r["recommendation"]["mode"], "left");
        assert_eq!(r["channels"][0]["overFullScaleSamples"].as_u64().unwrap() > 0, over);
        let p = s.execute("audio.preview", source(item)).unwrap();
        let wav = base64::engine::general_purpose::STANDARD.decode(p["audio"]["base64"].as_str().unwrap()).unwrap();
        let decoded = filmcraft_media::open_bytes("audition.wav", wav.into(), &[]).unwrap().audio(0, 4800, RATE).unwrap();
        assert_eq!(decoded.channels[0].iter().any(|x| x.abs() > 1.9), over);
        assert!(decoded.channels[1].iter().all(|x| x.abs() < 1e-8));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sequence_missing_declared_audio_stream_is_not_reported_as_silence() {
    let (mut s, _, clip) = clip_fixture("missing");
    assert!(s.execute("audio.inspect", json!({"subject":{"clip":clip}})).is_err());
    let error = s.execute("audio.inspect", json!({"subject":{"target":"sequence"}})).unwrap_err().to_string();
    assert!(error.contains("declared audio stream"), "{error}");
    assert!(s.execute("audio.preview", json!({"subject":{"target":"sequence"},"start":Tick::from_units(5,1).0,"end":Tick::from_units(6,1).0})).is_err());
}
