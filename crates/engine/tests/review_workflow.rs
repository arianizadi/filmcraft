use filmcraft_engine::{
    Session,
    project::{Transcript, Word, review::EditingProfile},
    time::Tick,
};
use serde_json::{Value, json};
fn new() -> Session {
    let mut s = Session::default();
    s.execute("file.newSequence", json!({})).unwrap();
    s
}
fn staged(s: &mut Session, steps: Value) -> u64 {
    s.execute("agent.plan", json!({"steps":steps})).unwrap()["plan"].as_u64().unwrap()
}
#[test]
fn checkpoints_profiles_annotations_and_activity_round_trip_and_compare() {
    let mut s = new();
    let seq = s.state.active_sequence.unwrap();
    let cp = s.execute("review.checkpoint", json!({"name":"Before polish"})).unwrap()["checkpoint"].as_u64().unwrap();
    s.execute("markers.add", json!({"name":"Polish"})).unwrap();
    s.execute("review.annotate", json!({"text":"Check this cut","kind":"suggestion","start":0,"end":100})).unwrap();
    let profile = EditingProfile { name: "Talking head".into(), ..Default::default() };
    assert_eq!(s.execute("profile.save", json!({"profile":profile})).unwrap()["version"], 1);
    assert_eq!(s.execute("profile.save", json!({"profile":profile})).unwrap()["version"], 2);
    let bytes = filmcraft_format::encode(&s.project, false);
    let loaded = filmcraft_format::decode(&bytes).unwrap();
    assert_eq!(*s.project, loaded.project);
    let mut reopened = Session::default();
    reopened.project = std::sync::Arc::new(loaded.project);
    reopened.state.active_sequence = Some(seq);
    let diff = reopened.execute("review.diff", json!({"checkpoint":cp})).unwrap();
    assert_eq!(diff["diff"]["sequences"][0]["markersChanged"], true);
    assert_eq!(reopened.execute("review.annotations", json!({})).unwrap()["annotations"][0]["text"], "Check this cut");
    assert!(!reopened.project.review.activity.is_empty());
    let resolved = reopened.execute("profile.resolve", json!({"name":"Talking head","version":1,"overrides":{"loudnessLufs":-14.0}})).unwrap();
    assert_eq!(resolved["loudnessLufs"], -14.0);
    assert_eq!(reopened.project.review.profiles["Talking head"][0].loudness_lufs, -16.0);
}
#[test]
fn partial_approval_regenerates_exact_snapshot_and_requires_omitted_dependencies() {
    let mut s = new();
    let before = s.project.clone();
    let id = staged(
        &mut s,
        json!([
            {"id":"markers.add","params":{"name":"Keep"}},
            {"id":"markers.edit","params":{"marker":{"$step":0,"path":"/marker"},"name":"Kept correction"}},
            {"id":"project.setNotes","params":{"text":"Omit"}}
        ]),
    );
    assert!(s.execute("agent.subset", json!({"plan":id,"steps":[1]})).is_err());
    assert!(s.execute("agent.subset", json!({"plan":id,"steps":[0,0]})).is_err());
    let subset = s.execute("agent.subset", json!({"plan":id,"steps":[1,0]})).unwrap();
    let subset_id = subset["plan"].as_u64().unwrap();
    assert_eq!(s.project, before);
    let (exact, _, _) = s.agent_plan_snapshot(subset_id, false).unwrap();
    assert!(exact.notes.is_empty());
    assert_eq!(exact.sequence(s.state.active_sequence.unwrap()).unwrap().markers[0].name, "Kept correction");
    let undo = s.history.undo.len();
    s.execute("agent.apply", json!({"plan":subset_id})).unwrap();
    assert_eq!(s.project, exact);
    assert_eq!(s.history.undo.len(), undo + 1);
    assert!(s.execute("agent.subset", json!({"plan":id,"steps":[2]})).is_err());
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project, before);
}
fn speech() -> (Session, u64, u64) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let c = s.active_sequence().unwrap().audio_tracks[0].items[0].clone();
    let mut t = Transcript::default();
    for (text, a, b) in [("hello", 0.2, 0.5), ("um", 0.6, 0.9), ("world", 1.1, 1.4), ("again", 2.0, 2.3)] {
        t.words.push(Word::new(text, c.source_in + Tick::from_seconds_f64(a), c.source_in + Tick::from_seconds_f64(b)))
    }
    s.execute("transcript.set", json!({"item":c.item.0,"transcript":t})).unwrap();
    (s, c.id.0, c.item.0)
}
#[test]
fn source_ids_survive_correction_reordering_reopen_and_transcript_range_review() {
    let (mut s, clip, item) = speech();
    let original = s.execute("transcript.sourceWords", json!({"item":item})).unwrap();
    let ids = original["transcript"]["words"].as_array().unwrap().iter().map(|w| w["id"].as_u64().unwrap()).collect::<Vec<_>>();
    assert!(ids.iter().all(|id| *id > 0));
    s.execute("transcript.patchWords", json!({"item":item,"words":[{"id":ids[1],"text":"erm"}]})).unwrap();
    assert_eq!(s.project.transcripts[&filmcraft_engine::project::ItemId(item)].words[1].id, ids[1]);
    let before = s.project.clone();
    let proposal = s.execute("transcript.proposeCuts", json!({"ranges":[{"clip":clip,"fromWordId":ids[1]},{"clip":clip,"fromWordId":ids[3]}]})).unwrap();
    assert_eq!(s.project, before);
    assert_eq!(proposal["ranges"].as_array().unwrap().len(), 2);
    assert!(proposal["steps"][0]["tracks"].as_array().unwrap().len() >= 2);
    let subset = s.execute("agent.subset", json!({"plan":proposal["plan"],"steps":[1]})).unwrap();
    s.execute("agent.apply", json!({"plan":subset["plan"]})).unwrap();
    let words = s.execute("transcript.inspect", json!({})).unwrap();
    let texts = words["words"].as_array().unwrap().iter().map(|w| w["text"].as_str().unwrap()).collect::<Vec<_>>();
    assert!(!texts.contains(&"erm"));
    assert!(texts.contains(&"again"));
    let loaded = filmcraft_format::decode(&filmcraft_format::encode(&s.project, false)).unwrap();
    assert_eq!(loaded.project.transcripts[&filmcraft_engine::project::ItemId(item)].words.iter().map(|w| w.id).collect::<Vec<_>>(), ids);
}
#[test]
fn translations_keep_exact_timing_and_reject_missing_or_duplicate_cues() {
    let (mut s, _, _) = speech();
    let result = s.execute("transcript.createCaptions", json!({"maxChars":10})).unwrap();
    let tid = result["track"].as_u64().unwrap();
    let original = s.active_sequence().unwrap().caption_tracks.iter().find(|t| t.id.0 == tid).unwrap().clone();
    let translations = original.captions.iter().enumerate().map(|(i, c)| json!({"caption":c.id.0,"text":format!("Traduction {i}")})).collect::<Vec<_>>();
    let before = s.project.clone();
    assert!(s.execute("captions.translate", json!({"sourceTrack":tid,"language":"fr","translations":[]})).is_err());
    assert_eq!(s.project, before);
    let id = s.execute("captions.translate", json!({"sourceTrack":tid,"language":"fr","translations":translations})).unwrap()["track"].as_u64().unwrap();
    let translated = s.active_sequence().unwrap().caption_tracks.iter().find(|t| t.id.0 == id).unwrap();
    assert_eq!(translated.language, "fr");
    for (a, b) in original.captions.iter().zip(&translated.captions) {
        assert_eq!((a.start, a.duration), (b.start, b.duration));
        assert_ne!(a.id, b.id);
        assert_ne!(a.text, b.text);
    }
}
#[test]
fn layouts_are_editable_variants_and_failed_hostile_edits_are_atomic() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let sequence = s.state.active_sequence.unwrap();
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
    let before = s.project.clone();
    let result = s.execute("profile.layout", json!({"kind":"vertical","clips":[clip.0]})).unwrap();
    assert_eq!(s.project, before);
    let id = result["plan"].as_u64().unwrap();
    let (after, st, _) = s.agent_plan_snapshot(id, false).unwrap();
    assert_ne!(st.active_sequence, Some(sequence));
    let q = after.sequence(st.active_sequence.unwrap()).unwrap();
    assert_eq!((q.settings.width, q.settings.height), (1080, 1920));
    assert_eq!(after.sequence(sequence).unwrap().settings, before.sequence(sequence).unwrap().settings);
    s.execute("agent.apply", json!({"plan":id})).unwrap();
    assert_eq!(s.project, after);
    for (cmd, p) in [
        ("review.checkpoint", json!({"name":""})),
        ("review.annotate", json!({"text":"x","start":-1})),
        ("profile.save", json!({"profile":{"name":"bad","delivery":{"fpsNum":0,"fpsDen":0}}})),
        ("agent.subset", json!({"plan":id,"steps":[u64::MAX]})),
    ] {
        let (before, rev, undo) = (s.project.clone(), s.revision, s.history.undo.len());
        assert!(s.execute(cmd, p).is_err(), "{cmd}");
        assert_eq!(s.project, before);
        assert_eq!(s.revision, rev);
        assert_eq!(s.history.undo.len(), undo)
    }
}
#[test]
fn duplicate_source_ids_and_oversized_review_data_refuse_without_mutation() {
    let (mut s, _, item) = speech();
    let mut transcript = (**s.project.transcripts.get(&filmcraft_engine::project::ItemId(item)).unwrap()).clone();
    transcript.words[1].id = transcript.words[0].id;
    let before = s.project.clone();
    assert!(s.execute("transcript.set", json!({"item":item,"transcript":transcript})).is_err());
    assert_eq!(s.project, before);
    let mut document = serde_json::to_value(&*s.project).unwrap();
    document["review"]["checkpoints"] = json!([{"id":1,"name":"bad","revision":0,"created_utc":"","document":"x"}]);
    document["review"]["next_id"] = json!(0);
    let envelope = json!({"format":filmcraft_format::FORMAT_ID,"schema_version":filmcraft_format::SCHEMA_VERSION,"generator":"test","project":document});
    assert!(filmcraft_format::decode(&serde_json::to_vec(&envelope).unwrap()).is_err());
}
#[test]
fn grouped_edits_log_once_and_failed_group_rolls_back_activity() {
    let mut s = new();
    let events = s.project.review.activity.len();
    s.edit_group("Two notes", |s| {
        s.execute("project.setNotes", json!({"text":"a"}))?;
        s.execute("project.setNotes", json!({"text":"b"}))?;
        Ok(())
    })
    .unwrap();
    assert_eq!(s.project.review.activity.len(), events + 1);
    assert_eq!(s.project.review.activity.last().unwrap().label, "Two notes");
    let before = s.project.clone();
    let failed: filmcraft_engine::Result<()> = s.edit_group("Must roll back", |s| {
        s.execute("project.setNotes", json!({"text":"c"}))?;
        Err(filmcraft_engine::EngineError::Other("synthetic failure".into()))
    });
    assert!(failed.is_err());
    assert_eq!(s.project, before);
}
#[test]
fn active_profile_keeps_approved_version_and_one_off_overrides_for_delivery() {
    let mut s = new();
    let first = EditingProfile { name: "Versions".into(), loudness_lufs: -18.0, ..Default::default() };
    s.execute("profile.save", json!({"profile":first})).unwrap();
    let second = EditingProfile { name: "Versions".into(), loudness_lufs: -12.0, ..Default::default() };
    s.execute("profile.save", json!({"profile":second})).unwrap();
    s.execute("profile.apply", json!({"name":"Versions","version":1,"overrides":{"loudnessLufs":-17.0}})).unwrap();
    let resolved = s.execute("profile.resolve", json!({})).unwrap();
    assert_eq!(resolved["version"], 1);
    assert_eq!(resolved["loudnessLufs"], -17.0);
    assert_eq!(s.execute("profile.get", json!({"name":"Versions"})).unwrap()["loudnessLufs"], -12.0);
    let loaded = filmcraft_format::decode(&filmcraft_format::encode(&s.project, false)).unwrap();
    assert_eq!(loaded.project.review.active_profile_settings.unwrap().loudness_lufs, -17.0);
}
#[test]
fn source_word_ids_are_not_recycled_by_set_recognition_or_transcript_deletion() {
    use std::sync::Arc;
    let (mut s, clip, item) = speech();
    let item = filmcraft_engine::project::ItemId(item);
    let original = (**s.project.transcripts.get(&item).unwrap()).clone();
    let old_max = original.next_word_id;
    let mut replacement = original.clone();
    for word in &mut replacement.words {
        word.id = 0;
    }
    replacement.next_word_id = 0;
    replacement.words[0].text = "Different".into();
    s.execute("transcript.set", json!({"item":item.0,"transcript":replacement})).unwrap();
    let current = s.project.transcripts.get(&item).unwrap();
    assert!(current.words[0].id > old_max);
    assert_eq!(current.words[1].id, original.words[1].id, "verified source-time/text matches keep identity");
    let set_max = current.next_word_id;
    let mut generated = (**current).clone();
    for word in &mut generated.words {
        word.text = format!("New {}", word.text);
        word.id = 1;
    }
    generated.next_word_id = 0;
    s.transcriber = Some(Arc::new(filmcraft_speech::FixedTranscriber { transcript: generated, id: "fixed".into() }));
    s.execute("transcript.generate", json!({"items":[item.0]})).unwrap();
    assert!(s.project.transcripts[&item].words.iter().all(|w| w.id > set_max));
    let generated_max = s.project.transcripts[&item].next_word_id;
    s.execute("transcript.delete", json!({"item":item.0})).unwrap();
    assert_eq!(s.project.transcript_word_counters[&item], generated_max);
    let loaded = filmcraft_format::decode(&filmcraft_format::encode(&s.project, false)).unwrap();
    assert_eq!(loaded.project.transcript_word_counters[&item], generated_max);
    let mut fresh = original.clone();
    for word in &mut fresh.words {
        word.id = 0;
    }
    fresh.next_word_id = 0;
    s.execute("transcript.set", json!({"item":item.0,"transcript":fresh})).unwrap();
    assert!(s.project.transcripts[&item].words.iter().all(|w| w.id > generated_max));
    assert!(s.execute("transcript.proposeCuts", json!({"ranges":[{"clip":clip,"fromWordId":original.words[0].id}]})).is_err());
}
#[test]
fn long_recordings_are_recognized_in_bounded_windows_with_exact_source_offsets() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct BoundedRecognizer(Arc<AtomicUsize>);
    impl filmcraft_speech::Transcriber for BoundedRecognizer {
        fn id(&self) -> String {
            "bounded-test".into()
        }
        fn transcribe(
            &self,
            audio: &[f32],
            _: &filmcraft_speech::Options,
            _: filmcraft_speech::ProgressFn,
        ) -> Result<Transcript, filmcraft_speech::SpeechError> {
            assert!(audio.len() <= 4_800_000);
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Transcript {
                language: "en".into(),
                words: vec![Word::new("window", Tick::from_seconds_f64(0.1), Tick::from_seconds_f64(0.2))],
                ..Default::default()
            })
        }
    }
    let mut s = new();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":601.0})).unwrap()["item"].as_u64().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    s.transcriber = Some(Arc::new(BoundedRecognizer(count.clone())));
    s.execute("transcript.generate", json!({"items":[item]})).unwrap();
    assert!(count.load(Ordering::SeqCst) >= 3);
    let transcript = &s.project.transcripts[&filmcraft_engine::project::ItemId(item)];
    assert_eq!(transcript.words.len(), count.load(Ordering::SeqCst));
    assert_eq!(transcript.words[0].start, Tick::from_seconds_f64(0.1));
    assert!(transcript.words[1].start >= Tick::from_seconds_f64(200.0));
    transcript.check().unwrap();
}
