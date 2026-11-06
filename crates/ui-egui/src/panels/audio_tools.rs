//! Channel repair, measured diagnostics, source-preserving proposals and native PCM audition.
use crate::FilmcraftApp;
use base64::Engine;
use egui::Rect;
use filmcraft_engine::{Session, agent_plans::PreparedEditPlan};
use filmcraft_media::MediaSource;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, atomic::Ordering};

mod mix;
const STATE: &str = "audioTools.state";
#[derive(Clone, Default)]
struct State {
    identity: Option<Arc<()>>,
    selection: Vec<u64>,
    requested_selection: Vec<u64>,
    revision: u64,
    channels: Value,
    left: f64,
    right: f64,
    invert_left: bool,
    invert_right: bool,
    replace_animation: bool,
    preset: usize,
    routing: usize,
    normalize: bool,
    target_lufs: f64,
    report: Option<Value>,
    inspect_job: Option<u64>,
    inspect_identity: Option<Arc<()>>,
    pending: Option<Pending>,
    plan: Option<u64>,
    plan_clips: Vec<u64>,
    previews: Option<(Arc<filmcraft_frame::AudioBuffer>, Arc<filmcraft_frame::AudioBuffer>)>,
    preview_note: String,
    message: String,
    reference: u64,
    target: u64,
    manual: bool,
    reference_time: i64,
    target_time: i64,
    mute_reference: bool,
    source_stream: usize,
    source_left: u16,
    source_right: u16,
    source_mono: bool,
}
#[derive(Clone)]
struct Seed {
    project: Arc<filmcraft_project::Project>,
    identity: Arc<()>,
    state: filmcraft_engine::EditorState,
    revision: u64,
    effect_floor: u64,
    permission: filmcraft_engine::operations::Permission,
    path: Option<String>,
    services: Arc<dyn filmcraft_engine::Services>,
    media: Arc<filmcraft_engine::MediaPool>,
}
impl Seed {
    fn capture(s: &Session) -> Self {
        Self {
            project: s.project.clone(),
            identity: s.project_instance.clone(),
            state: s.state.clone(),
            revision: s.revision,
            effect_floor: s.effect_identity_floor(),
            permission: s.operations.permission,
            path: s.path.clone(),
            services: s.services.clone(),
            media: s.media.clone(),
        }
    }
    fn session(self) -> Session {
        let mut s = Session::new(self.services);
        s.project = self.project;
        s.project_instance = self.identity;
        s.state = self.state;
        s.revision = self.revision;
        s.inherit_effect_identity_floor(self.effect_floor);
        s.operations.permission = self.permission;
        s.path = self.path;
        s.media = self.media;
        s
    }
}
enum Output {
    Data(Value),
    Plan(Box<PreparedEditPlan>, Value),
}
#[derive(Clone)]
struct Pending {
    job: filmcraft_engine::Job,
    registry: filmcraft_engine::durable_jobs::JobRegistry,
    result: Arc<Mutex<Option<Result<Output, String>>>>,
    identity: Arc<()>,
    clips: Vec<u64>,
    started: web_time::Instant,
}
fn finish(p: &Pending, error: Option<&str>) {
    let result = if let Some(error) = error {
        Err(error.to_string())
    } else {
        Ok(filmcraft_export::Report {
            path: String::new(),
            frames: 0,
            seconds: p.started.elapsed().as_secs_f64(),
            bytes: 0,
            render_fps: 0.0,
            extra_files: vec![],
        })
    };
    *p.job.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = error.unwrap_or("Completed").to_string();
    *p.job.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    p.job.progress.finished.store(true, Ordering::Relaxed);
    if let Err(e) = p.registry.sync(std::slice::from_ref(&p.job)) {
        log::error!("{e}");
    }
}
fn start(app: &mut FilmcraftApp, state: &mut State, command: &'static str, params: Value, plan: bool, paired: Option<(Seed, Seed, bool)>) {
    if state.pending.is_some() {
        return;
    }
    let job = match app.session.job_registry.new_job(&app.session, if plan { "Prepare audio proposal" } else { "Measure / preview audio" }.into()) {
        Ok(j) => j,
        Err(e) => {
            state.message = e.to_string();
            return;
        }
    };
    job.progress.total.store(if paired.is_some() { 2 } else { 1 }, Ordering::Relaxed);
    let p = Pending {
        job: job.clone(),
        registry: app.session.job_registry.clone(),
        result: Arc::new(Mutex::new(None)),
        identity: app.session.project_instance.clone(),
        clips: state.selection.clone(),
        started: web_time::Instant::now(),
    };
    app.session.jobs.push(job);
    let work = p.clone();
    let seed = Seed::capture(&app.session);
    let run = move || {
        let result = filmcraft_media::cancel::with_cancel(&work.job.progress.cancel, || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Output, String> {
                let check = || if work.job.progress.cancel.load(Ordering::Relaxed) { Err("Audio job cancelled".to_string()) } else { Ok(()) };
                check()?;
                let output = if let Some((before, after, pre)) = paired {
                    let mut first = params.clone();
                    // Both readers already own the exact frozen project. Their session-local
                    // plan stores are intentionally empty, so query each reader's current data.
                    first["snapshot"] = json!("after");
                    if pre {
                        first["subject"]["effects"] = json!("pre");
                    }
                    let a = before.session().execute(command, first).map_err(|e| e.to_string())?;
                    work.job.progress.done.fetch_add(1, Ordering::Relaxed);
                    check()?;
                    let b = after.session().execute(command, params).map_err(|e| e.to_string())?;
                    work.job.progress.done.fetch_add(1, Ordering::Relaxed);
                    Output::Data(json!({"before":a,"after":b}))
                } else {
                    let mut s = seed.session();
                    let value = s.execute(command, params).map_err(|e| e.to_string())?;
                    work.job.progress.done.fetch_add(1, Ordering::Relaxed);
                    if plan {
                        let id = value["plan"].as_u64().ok_or("Audio proposal returned no plan")?;
                        Output::Plan(Box::new(s.agent_export_prepared_plan(id).map_err(|e| e.to_string())?), value)
                    } else {
                        Output::Data(value)
                    }
                };
                check()?;
                Ok(output)
            }))
            .unwrap_or_else(|_| Err("Audio worker failed; the project is unchanged".into()))
        });
        if !matches!(&result, Ok(Output::Plan(..))) {
            finish(&work, result.as_ref().err().map(String::as_str));
        } else {
            *work.job.progress.status.lock().unwrap_or_else(|e| e.into_inner()) = "Prepared; open Channels & Tools to stage for review".into();
            if let Err(e) = work.registry.sync(std::slice::from_ref(&work.job)) {
                log::error!("{e}");
            }
        }
        *work.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    };
    #[cfg(not(target_arch = "wasm32"))]
    if let Err(e) = std::thread::Builder::new().name("audio-workbench".into()).spawn(run) {
        finish(&p, Some(&e.to_string()));
        state.message = e.to_string();
        return;
    }
    #[cfg(target_arch = "wasm32")]
    run();
    state.pending = Some(p);
    state.message = "Working from a fixed snapshot. You can keep editing.".into();
}
fn decode_preview(value: &Value) -> Result<filmcraft_frame::AudioBuffer, String> {
    let encoded = value["audio"]["base64"].as_str().ok_or("Preview has no inline WAV")?;
    if encoded.len() > 16 * 1024 * 1024 {
        return Err("Audio preview exceeds the sixteen MiB transport bound".into());
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|e| format!("Invalid preview base64: {e}"))?;
    let source = filmcraft_media::wav::WavSource::parse("audio-preview.wav", bytes.into()).map_err(|e| e.to_string())?;
    let info = source.info().audio().ok_or("Preview contains no audio")?;
    let rate = info.sample_rate;
    if !(8000..=192000).contains(&rate) {
        return Err("Invalid preview sample rate".into());
    }
    let frames = usize::try_from(i128::from(source.info().duration.0) * i128::from(rate) / i128::from(filmcraft_time::TICKS_PER_SECOND))
        .map_err(|_| "Invalid preview duration")?;
    if frames == 0 || frames > rate as usize * 10 || !(1..=2).contains(&info.channels) {
        return Err("Preview must contain at most ten seconds of mono/stereo audio".into());
    }
    source.audio(0, frames, rate).map_err(|e| e.to_string())
}
fn poll(app: &mut FilmcraftApp, state: &mut State) {
    if let Some(p) = &state.pending {
        let result = p.result.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(result) = result {
            let Some(p) = state.pending.take() else { return };
            match result {
                Ok(Output::Plan(artifact, _evidence)) => {
                    let adopted = if p.job.progress.cancel.load(Ordering::Relaxed) {
                        Err("Audio job cancelled before adoption".into())
                    } else {
                        app.session.agent_adopt_prepared_plan(*artifact).map_err(|e| e.to_string())
                    };
                    match adopted {
                        Ok(report) => {
                            state.plan = report["plan"].as_u64();
                            state.plan_clips = p.clips.clone();
                            state.report = Some(report.clone());
                            app.agent_review.open = true;
                            state.message = format!("Audio plan #{} is ready in Agent Review. Preview it, then approve.", report["plan"]);
                            finish(&p, None)
                        }
                        Err(e) => {
                            finish(&p, Some(&e));
                            state.message = e
                        }
                    }
                }
                Ok(Output::Data(value)) => {
                    if !Arc::ptr_eq(&p.identity, &app.session.project_instance) {
                        state.message = "Audio result belongs to a different project; analyze this project again.".into();
                        return;
                    }
                    if value.get("before").is_some() {
                        match decode_preview(&value["before"]).and_then(|a| decode_preview(&value["after"]).map(|b| (a, b))) {
                            Ok(pair) => {
                                state.previews = Some((Arc::new(pair.0), Arc::new(pair.1)));
                                let a = &value["before"]["headroom"];
                                let b = &value["after"]["headroom"];
                                state.preview_note = format!(
                                    "Subject {} · snapshot revisions {} / {}. Original headroom {} dB; processed headroom {} dB. Float samples are preserved; the listening device may clip above full scale.",
                                    value["before"]["subject"], value["before"]["revision"], value["after"]["revision"], a["headroomDb"], b["headroomDb"]
                                );
                                state.message = "A/B preview ready. Both versions use fixed project snapshots.".into()
                            }
                            Err(e) => state.message = e,
                        }
                    } else {
                        state.report = Some(value);
                        state.message = "Audio measurements ready.".into();
                    }
                }
                Err(e) => state.message = e,
            }
        }
    }
    if let Some(job) = state.inspect_job {
        match app.session.execute("audio.inspectStatus", json!({"job":job})) {
            Ok(value) if value.get("result").is_some_and(|v| !v.is_null()) => {
                if state.inspect_identity.as_ref().is_some_and(|id| Arc::ptr_eq(id, &app.session.project_instance)) {
                    state.report = Some(value["result"].clone());
                    state.message = "Channel measurements ready.".into();
                } else {
                    state.message = "Inspection finished for another project; analyze this project again.".into();
                }
                state.inspect_job = None;
                state.inspect_identity = None;
            }
            Ok(value) if value["error"].is_string() => {
                state.message = value["error"].as_str().unwrap_or("Audio inspection failed").into();
                state.inspect_job = None;
            }
            Err(e) => {
                state.message = e.to_string();
                state.inspect_job = None;
            }
            _ => {}
        }
    }
}
fn button(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, label: &str, enabled: bool) -> bool {
    let r = ui.add_enabled(enabled, egui::Button::new(label).wrap());
    app.auto.add_response(&format!("audioTools.{id}"), &r, label);
    r.clicked()
}
fn command(app: &mut FilmcraftApp, state: &mut State, id: &str, mut params: Value) {
    params["clips"] = json!(state.selection);
    params["replaceAnimation"] = json!(state.replace_animation);
    match app.session.execute(id, params) {
        Ok(_) => {
            state.revision = u64::MAX;
            state.previews = None;
            state.message = "Audio change applied. Undo restores the previous setting.".into()
        }
        Err(e) => state.message = e.to_string(),
    }
}
fn checkbox(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, label: &str, value: &mut bool) -> bool {
    let r = ui.checkbox(value, label);
    app.auto.add_response(&format!("audioTools.{id}"), &r, label);
    r.changed()
}
fn choices(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, label: &str, selected: &mut usize, values: &[(&str, &str)]) {
    ui.label(label);
    let text = values.get(*selected).map_or("Choose", |v| v.1);
    let response = craft_ui::ComboBox::from_id_salt(id).selected_text(text).width(ui.available_width().min(250.0)).show_ui(ui, |ui| {
        for (index, (key, label)) in values.iter().enumerate() {
            let r = ui.selectable_value(selected, index, *label);
            app.auto.add_response(&format!("audioTools.{id}.{key}"), &r, label);
        }
    });
    app.auto.add_response(&format!("audioTools.{id}"), &response.response, label);
}
fn clip_label(clips: &[Value], id: u64) -> String {
    clips
        .iter()
        .find(|v| v["clip"].as_u64() == Some(id))
        .map_or_else(|| format!("Clip #{id}"), |v| format!("#{} {} · stream {}", id, v["name"].as_str().unwrap_or("Clip"), v["stream"]))
}
fn clip_choice(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, label: &str, selected: &mut u64, clips: &[Value]) {
    ui.label(label);
    let response = craft_ui::ComboBox::from_id_salt(id).selected_text(clip_label(clips, *selected)).width(ui.available_width().min(250.0)).show_ui(ui, |ui| {
        for clip in clips {
            if let Some(value) = clip["clip"].as_u64() {
                let label = clip_label(clips, value);
                let r = ui.selectable_value(selected, value, &label);
                app.auto.add_response(&format!("audioTools.{id}.{value}"), &r, &label);
            }
        }
    });
    app.auto.add_response(&format!("audioTools.{id}"), &response.response, label);
}
fn inputs(app: &mut FilmcraftApp, state: &mut State, ui: &mut egui::Ui, enabled: bool) {
    let opened=egui::CollapsingHeader::new("Input streams / roles").id_salt("audioTools.input").show(ui,|ui|{
        let first=state.channels["clips"].as_array().and_then(|v|v.first()).cloned().unwrap_or(Value::Null);
        let streams=first["sourceStreams"].as_array().cloned().unwrap_or_default();
        ui.small("Input selection changes the first selected clip. Stream numbers and input channels are zero-based.");
        let name=streams.iter().find(|v|v["stream"].as_u64()==Some(state.source_stream as u64)).map_or_else(||format!("Stream {}",state.source_stream),|v|format!("Stream {} · {} channels · {}",v["stream"],v["channels"],v["codec"].as_str().unwrap_or("audio")));
        let response=craft_ui::ComboBox::from_id_salt("audioTools.source.stream").selected_text(name).width(ui.available_width().min(250.0)).show_ui(ui,|ui|{
            for stream in &streams {if let Some(index)=stream["stream"].as_u64().and_then(|n|usize::try_from(n).ok()) {let label=format!("Stream {index} · {} ch · {} Hz · {}",stream["channels"],stream["sampleRate"],stream["codec"].as_str().unwrap_or("audio"));let r=ui.selectable_value(&mut state.source_stream,index,&label);app.auto.add_response(&format!("audioTools.source.stream.{index}"),&r,&label);}}
        });app.auto.add_response("audioTools.source.stream",&response.response,"Container audio stream");
        let count=streams.iter().find(|v|v["stream"].as_u64()==Some(state.source_stream as u64)).and_then(|v|v["channels"].as_u64()).unwrap_or(2).clamp(1,64) as u16;
        checkbox(app,ui,"source.mono","Use one input channel (mono)",&mut state.source_mono);
        for (id,label,value) in [("source.left","First input channel",&mut state.source_left),("source.right","Second input channel",&mut state.source_right)] {
            ui.label(label);let r=ui.add(egui::DragValue::new(value).range(0..=count-1));app.auto.add_response(&format!("audioTools.{id}"),&r,label);
        }
        if button(app,ui,"source.apply","Use input selection",enabled&&first["clip"].is_u64()) {
            let channels=if state.source_mono {vec![state.source_left]}else{vec![state.source_left,state.source_right]};
            match app.session.execute("audio.source.set",json!({"clip":first["clip"],"stream":state.source_stream,"channels":channels})){Ok(_)=>{state.revision=u64::MAX;state.message="Input stream/channel selection applied; original media is unchanged.".into()},Err(e)=>state.message=e.to_string()}
        }
        if button(app,ui,"source.default","Default input channels",enabled&&first["clip"].is_u64()) {
            match app.session.execute("audio.source.set",json!({"clip":first["clip"],"channels":[]})){Ok(_)=>{state.revision=u64::MAX;state.message="Default source-channel mapping restored.".into()},Err(e)=>state.message=e.to_string()}
        }
        if button(app,ui,"source.mainPlan","Propose this stream as main; mute scratch…",enabled&&streams.len()>1&&state.pending.is_none()) {
            let p=json!({"label":"Choose main microphone and mute linked scratch","steps":[{"id":"audio.streams.configure","params":{"clips":state.selection,"mainStream":state.source_stream,"muteSecondary":true}}]});start(app,state,"agent.plan",p,true,None);
        }
        ui.small("Main and scratch roles are your choices. Other linked streams are retained and muted only after approval; no streams are merged or dropped.");
    });
    app.auto.add_response("audioTools.input", &opened.header_response, "Input streams and main / scratch");
}
fn routing(app: &mut FilmcraftApp, state: &mut State, ui: &mut egui::Ui, enabled: bool) {
    ui.label(egui::RichText::new("Channel repair").strong());
    ui.label("A microphone recorded on one side can feed both output channels.");
    ui.horizontal_wrapped(|ui| {
        for (id, label, mode) in
            [("left", "Left mic → both", "left"), ("right", "Right mic → both", "right"), ("mono", "Mono", "mono"), ("swap", "Swap L/R", "swap")]
        {
            if button(app, ui, id, label, enabled) {
                command(app, state, "audio.channels.set", json!({"mode":mode}));
            }
        }
        if button(app, ui, "original", "Original channels", enabled) {
            command(app, state, "audio.channels.reset", json!({}));
        }
    });
    let more = egui::CollapsingHeader::new("Gain, polarity and M/S").id_salt("audioTools.advanced").show(ui, |ui| {
        for (id, label, value, key) in
            [("leftGain", "Left output gain", &mut state.left, "leftGainDb"), ("rightGain", "Right output gain", &mut state.right, "rightGainDb")]
        {
            ui.label(label);
            let r = ui.add_enabled(enabled, egui::DragValue::new(value).range(-60.0..=24.0).speed(0.2).suffix(" dB"));
            app.auto.add_response(&format!("audioTools.{id}"), &r, label);
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                let mut p = json!({});
                p[key] = json!(*value);
                p["clips"] = json!(state.selection);
                p["replaceAnimation"] = json!(state.replace_animation);
                if let Err(e) = app.session.execute("audio.channels.set", p) {
                    state.message = e.to_string();
                } else {
                    state.revision = u64::MAX;
                }
            }
        }
        if checkbox(app, ui, "invertLeft", "Invert left polarity", &mut state.invert_left) {
            let value = state.invert_left;
            command(app, state, "audio.channels.set", json!({"invertLeft":value}));
        }
        if checkbox(app, ui, "invertRight", "Invert right polarity", &mut state.invert_right) {
            let value = state.invert_right;
            command(app, state, "audio.channels.set", json!({"invertRight":value}));
        }
        ui.horizontal_wrapped(|ui| {
            for (mode, label) in [("midSideEncode", "Encode M/S"), ("midSideDecode", "Decode M/S"), ("side", "Side only")] {
                if button(app, ui, mode, label, enabled) {
                    command(app, state, "audio.channels.set", json!({"mode":mode}));
                }
            }
        });
        checkbox(app, ui, "replaceAnimation", "Replace affected routing keyframes", &mut state.replace_animation);
        ui.label("Default: preserve routing animation. Explicit replacement is required for animated controls.");
    });
    app.auto.add_response("audioTools.advanced", &more.header_response, "Gain, polarity and M/S");
}
const PRESETS: &[(&str, &str)] = &[
    ("dialogueClean", "Clean dialogue"),
    ("dialogueWarm", "Warm dialogue"),
    ("podcast", "Podcast"),
    ("musicGentle", "Music (preserve stereo)"),
    ("reset", "Reset sound settings"),
];
const ROUTES: &[(&str, &str)] = &[
    ("auto", "Auto (confident only)"),
    ("left", "Left microphone to both"),
    ("right", "Right microphone to both"),
    ("mono", "Mono"),
    ("stereo", "Keep stereo"),
];
fn preparation(app: &mut FilmcraftApp, state: &mut State, ui: &mut egui::Ui, enabled: bool) {
    ui.separator();
    ui.label(egui::RichText::new("Prepare repair for review").strong());
    choices(app, ui, "routing", "Channels", &mut state.routing, ROUTES);
    choices(app, ui, "preset", "Existing Essential Sound preset", &mut state.preset, PRESETS);
    checkbox(app, ui, "normalize", "Normalize measured full-clip loudness", &mut state.normalize);
    if state.normalize {
        ui.label("Loudness target");
        let r = ui.add(egui::DragValue::new(&mut state.target_lufs).range(-60.0..=0.0).speed(0.2).suffix(" LUFS"));
        app.auto.add_response("audioTools.targetLufs", &r, "Loudness target");
        ui.small("Full clips up to five minutes; gain is limited by peak headroom.");
    }
    if button(app, ui, "prepare", "Prepare audio repair…", enabled && state.pending.is_none()) {
        let p = json!({"clips":state.selection,"routing":ROUTES.get(state.routing).map(|v|v.0).unwrap_or("auto"),"preset":PRESETS.get(state.preset).map(|v|v.0).unwrap_or("dialogueClean"),"normalize":state.normalize,"targetLufs":state.target_lufs,"replaceAnimation":state.replace_animation});
        start(app, state, "audio.prepare", p, true, None);
    }
    if button(app, ui, "applyPreset", "Apply preset now", enabled) {
        let p = json!({"preset":PRESETS.get(state.preset).map(|v|v.0).unwrap_or("dialogueClean")});
        command(app, state, "audio.preset", p);
    }
    ui.small("EQ, compression, noise reduction, dehum, de-essing and ducking remain in the Edit tab.");
}
fn report(app: &mut FilmcraftApp, state: &State, ui: &mut egui::Ui) {
    let Some(value) = &state.report else { return };
    let shown = serde_json::to_string_pretty(value).unwrap_or_else(|e| e.to_string());
    let summary = value["recommendation"]["reason"].as_str().unwrap_or("Measured audio report");
    ui.label(summary);
    if value["status"] == "needsManual" {
        ui.label("No reliable audio match. Use manual source-time clap anchors before preparing alignment.");
    }
    let number = |v: &Value| v.as_f64().filter(|n| n.is_finite()).map_or_else(|| "n/a".into(), |v| format!("{v:.1}"));
    if let Some(channels) = value["channels"].as_array() {
        for (index, channel) in channels.iter().take(8).enumerate() {
            let name = match index {
                0 => "Left".into(),
                1 => "Right".into(),
                _ => format!("Channel {}", index + 1),
            };
            ui.label(format!(
                "{name}: peak {} dBFS · RMS {} dBFS · samples ≥ full scale {}",
                number(&channel["peakDbfs"]),
                number(&channel["rmsDbfs"]),
                channel["overFullScaleSamples"]
            ));
        }
        ui.label(format!("Stereo correlation: {} · mono loss: {} dB", number(&value["stereo"]["correlation"]), number(&value["stereo"]["monoLossDb"])));
        ui.small(format!("Continuous analysis: {} seconds · revision {}", number(&value["coverage"]["durationSeconds"]), value["revision"]));
    }
    if let Some(offset) = value["offsetTicks"].as_i64() {
        ui.label(format!(
            "Alignment offset: {:.4} s · {} samples · confidence {}",
            offset as f64 / filmcraft_time::TICKS_PER_SECOND as f64,
            value["samples"],
            number(&value["confidence"])
        ));
    }
    if let Some(warnings) = value["warnings"].as_array() {
        for warning in warnings.iter().take(20) {
            ui.label(warning.as_str().or_else(|| warning["message"].as_str()).unwrap_or("Review the exact warning evidence."));
        }
    }
    let detail = egui::CollapsingHeader::new("Channel evidence").id_salt("audioTools.evidence").show(ui, |ui| {
        let r = ui.monospace(shown.chars().take(24_000).collect::<String>());
        app.auto.add_response("audioTools.evidence.details", &r, "Measured channel evidence");
    });
    app.auto.add_response("audioTools.evidence", &detail.header_response, "Channel evidence and warnings");
}
fn diagnostics(app: &mut FilmcraftApp, state: &mut State, ui: &mut egui::Ui, enabled: bool) {
    ui.separator();
    ui.label(egui::RichText::new("Inspect and listen").strong());
    ui.small("Diagnostics and A/B use the first selected clip. Select one clip to focus listening.");
    if button(app, ui, "inspect", "Analyze selected clip channels", enabled && state.inspect_job.is_none() && state.pending.is_none())
        && let Some(clip) = state.selection.first()
    {
        match app.session.execute("audio.inspectAsync", json!({"subject":{"target":"clip","clip":clip,"effects":"pre"}})) {
            Ok(v) => {
                state.inspect_job = v["job"].as_u64();
                state.inspect_identity = Some(app.session.project_instance.clone());
                state.message = "Analyzing the first selected clip; no changes applied.".into()
            }
            Err(e) => state.message = e.to_string(),
        }
    }
    if button(app, ui, "preview", "Build original / processed A/B", enabled && state.pending.is_none())
        && let Some(clip) = state.selection.first().copied()
    {
        let mut before = Seed::capture(&app.session);
        let mut after = before.clone();
        let staged = state
            .plan
            .filter(|_| state.plan_clips.contains(&clip))
            .and_then(|plan| app.session.agent_plan_snapshot(plan, true).ok().zip(app.session.agent_plan_snapshot(plan, false).ok()))
            .filter(|((a, _, revision), (b, _, _))| (*revision == app.session.revision && **a == *app.session.project) || **b == *app.session.project);
        let pre = staged.is_none();
        if let Some(((a, sa, ra), (b, sb, rb))) = staged {
            before.project = a;
            before.state = sa;
            before.revision = ra;
            after.project = b;
            after.state = sb;
            after.revision = rb;
        }
        start(app, state, "audio.preview", json!({"subject":{"target":"clip","clip":clip,"effects":"post"}}), false, Some((before, after, pre)));
    }
    if let Some((before, after)) = &state.previews {
        ui.horizontal_wrapped(|ui| {
            if button(app, ui, "listenBefore", "Listen original", true) {
                match app.play_audio_preview((**before).clone()) {
                    Ok(_) => {}
                    Err(e) => state.message = e,
                }
            }
            if button(app, ui, "listenAfter", "Listen processed / proposed", true) {
                match app.play_audio_preview((**after).clone()) {
                    Ok(_) => {}
                    Err(e) => state.message = e,
                }
            }
            if button(app, ui, "stop", "Stop audition", true) {
                app.stop();
            }
        });
        ui.small(&state.preview_note);
    }
    report(app, state, ui);
}
fn sync(app: &mut FilmcraftApp, state: &mut State, ui: &mut egui::Ui, enabled: bool) {
    let open = egui::CollapsingHeader::new("Camera / microphone sync").id_salt("audioTools.sync").show(ui, |ui| {
        if state.selection.len() < 2 {
            ui.label("Select a camera audio clip and an external microphone clip on the timeline.");
            return;
        }
        let clips = state.channels["clips"].as_array().cloned().unwrap_or_default();
        clip_choice(app, ui, "sync.reference", "Camera/reference (stays fixed)", &mut state.reference, &clips);
        clip_choice(app, ui, "sync.target", "External/main microphone (moves)", &mut state.target, &clips);
        checkbox(app, ui, "sync.manual", "Use manual source-time anchors", &mut state.manual);
        if state.manual {
            for (id, label, value) in [
                ("sync.referenceTime", "Reference clap source time (ticks)", &mut state.reference_time),
                ("sync.clipTime", "Microphone clap source time (ticks)", &mut state.target_time),
            ] {
                ui.label(label);
                let r = ui.add(egui::DragValue::new(value).range(0..=filmcraft_time::Tick::MAX.0));
                app.auto.add_response(&format!("audioTools.{id}"), &r, label);
            }
            for (clip, id, label) in
                [(state.reference, "sync.referenceHere", "Mark reference clap at playhead"), (state.target, "sync.targetHere", "Mark mic clap at playhead")]
            {
                let time = app.session.playhead();
                let anchor = app
                    .session
                    .active_sequence()
                    .and_then(|q| q.find_item(filmcraft_project::ClipId(clip)))
                    .filter(|(_, c)| time >= c.start && time < c.end())
                    .map(|(_, c)| c.source_time_at(time));
                if button(app, ui, id, label, anchor.is_some())
                    && let Some(time) = anchor
                {
                    if clip == state.reference { state.reference_time = time.0 } else { state.target_time = time.0 }
                }
            }
        }
        checkbox(app, ui, "sync.muteReference", "Mute linked camera audio after alignment", &mut state.mute_reference);
        let mut p =
            json!({"reference":state.reference,"clip":state.target,"method":if state.manual{"manual"}else{"waveform"},"muteReference":state.mute_reference});
        if state.manual {
            p["referenceTime"] = json!(state.reference_time);
            p["clipTime"] = json!(state.target_time);
        }
        let available = state.reference != state.target && state.pending.is_none();
        if button(app, ui, "sync.inspect", "Analyze sync", available) {
            start(app, state, "audio.sync.inspect", p.clone(), false, None);
        }
        if button(app, ui, "sync.prepare", "Prepare alignment…", available && enabled) {
            start(app, state, "audio.sync.prepare", p, true, None);
        }
        ui.small(
            "Waveform matching measures a clap or shared dialogue. Ambiguous audio needs manual anchors. Review movement and linked tracks before approval.",
        );
    });
    app.auto.add_response("audioTools.sync", &open.header_response, "Sync camera and external microphone");
}
fn refresh_channels(app: &mut FilmcraftApp, state: &mut State) {
    let clips = app.session.state.selection.iter().map(|c| c.0).collect::<Vec<_>>();
    if state.requested_selection != clips || state.revision != app.session.revision {
        state.requested_selection = clips;
        state.selection.clear();
        state.revision = app.session.revision;
        match app.session.execute("audio.channels.get", json!({"clips":state.requested_selection})) {
            Ok(value) => {
                state.selection = value["clips"].as_array().into_iter().flatten().filter_map(|v| v["clip"].as_u64()).collect();
                let route = &value["clips"][0]["routing"];
                state.left = route["leftGainDb"].as_f64().unwrap_or(0.0);
                state.right = route["rightGainDb"].as_f64().unwrap_or(0.0);
                state.invert_left = route["invertLeft"].as_bool().unwrap_or(false);
                state.invert_right = route["invertRight"].as_bool().unwrap_or(false);
                let first = &value["clips"][0];
                state.source_stream = first["stream"].as_u64().and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
                let mapping = first["sourceChannels"].as_array();
                state.source_left = mapping.and_then(|v| v.first()).and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok()).unwrap_or(0);
                state.source_right = mapping.and_then(|v| v.get(1)).and_then(Value::as_u64).and_then(|n| u16::try_from(n).ok()).unwrap_or(1);
                state.source_mono = mapping.is_some_and(|v| v.len() == 1) || first["sourceChannelCount"].as_u64() == Some(1);
                state.channels = value;
            }
            Err(e) => {
                state.channels = Value::Null;
                if !state.requested_selection.is_empty() {
                    state.message = e.to_string();
                }
            }
        }
        if !state.selection.contains(&state.reference) {
            state.reference = state.selection.first().copied().unwrap_or(0);
        }
        if !state.selection.contains(&state.target) || state.reference == state.target {
            state.target = state.selection.get(1).copied().unwrap_or(0);
        }
    }
}
pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let key = egui::Id::unique(STATE);
    let mut state = ui.data_mut(|d| d.remove_temp::<State>(key)).unwrap_or_else(|| State { target_lufs: -16.0, ..Default::default() });
    poll(app, &mut state);
    if state.identity.as_ref().is_none_or(|id| !Arc::ptr_eq(id, &app.session.project_instance)) {
        state.identity = Some(app.session.project_instance.clone());
        state.revision = u64::MAX;
        state.report = None;
        state.previews = None;
        state.plan = None;
    }
    refresh_channels(app, &mut state);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(10.0, 4.0))).id_salt("audio-tools"));
    let scroll = egui::ScrollArea::vertical().id_salt("audioTools.scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
        ui.set_max_width(ui.available_width().max(1.0));
        if state.selection.is_empty() {
            ui.label("Select audio clips on the timeline. Channel fixes do not alter your source file.");
        } else {
            ui.label(format!("{} selected audio clip(s)", state.selection.len()));
            if state.selection.len() > 1 {
                ui.small("Edits apply to every selected audio clip; gain fields show the first clip.");
            }
        }
        let locked = state.channels["clips"].as_array().is_some_and(|c| c.iter().any(|v| v["locked"] == true));
        if locked {
            ui.colored_label(app.tokens.text_dim, "A selected track is locked. Unlock it before editing.");
        }
        if state.channels["clips"].as_array().is_some_and(|c| c.iter().any(|v| v["routing"]["animated"] == true)) {
            ui.label("Routing animation is present. Preserve it by default, or explicitly replace affected keys in Gain, polarity and M/S.");
        }
        if let Some(values) = state.channels["clips"].as_array() {
            for value in values.iter().take(8) {
                ui.label(format!(
                    "#{} {} · {} source channels · {}",
                    value["clip"],
                    value["name"].as_str().unwrap_or("Clip"),
                    value["sourceChannelCount"],
                    value["routing"]["mode"].as_str().unwrap_or("stereo")
                ));
            }
        }
        let enabled = !state.selection.is_empty() && !locked;
        let selected = !state.selection.is_empty();
        ui.add_enabled_ui(enabled, |ui| routing(app, &mut state, ui, enabled));
        inputs(app, &mut state, ui, enabled);
        preparation(app, &mut state, ui, enabled);
        mix::show(app, &mut state, ui);
        diagnostics(app, &mut state, ui, selected);
        sync(app, &mut state, ui, enabled);
        if let Some(job) = state.pending.as_ref().map(|p| p.job.id).or(state.inspect_job) {
            if let Some(active) = app.session.jobs.iter().find(|j| j.id == job) {
                let text = format!("Job #{job}: {}", active.progress.status.lock().unwrap_or_else(|e| e.into_inner()));
                let r = ui.add(egui::ProgressBar::new(active.progress.fraction()).text(&text));
                app.auto.add_response("audioTools.progress", &r, &text);
            } else {
                ui.label(format!("Audio job #{job}"));
            }
            if button(app, ui, "cancel", "Cancel audio job", true)
                && let Err(e) = app.session.execute("jobs.cancel", json!({"job":job}))
            {
                state.message = e.to_string();
            }
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
        if !state.message.is_empty() {
            let r = ui.label(&state.message);
            app.auto.add_response("audioTools.message", &r, &state.message);
        }
    });
    app.auto.add("audioTools.scroll", scroll.inner_rect, "Scroll audio tools");
    ui.data_mut(|d| d.insert_temp(key, state));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> FilmcraftApp {
        let mut s = Session::default();
        s.execute("file.newSequence", json!({})).unwrap();
        FilmcraftApp::new(s)
    }
    fn wait(app: &mut FilmcraftApp, state: &mut State) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while state.pending.is_some() {
            poll(app, state);
            assert!(std::time::Instant::now() < deadline, "{}", state.message);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    #[test]
    fn audio_proposal_workers_stage_once_and_refuse_a_changed_baseline() {
        let mut app = app();
        let mut state = State::default();
        let before = app.session.project.clone();
        let params = json!({"steps":[{"id":"project.setNotes","params":{"text":"Audio proposal"}}]});
        start(&mut app, &mut state, "agent.plan", params.clone(), true, None);
        let id = state.pending.as_ref().unwrap().job.id;
        wait(&mut app, &mut state);
        assert_eq!(app.session.project, before);
        assert!(state.plan.is_some());
        assert_eq!(app.session.job_registry.get(id).unwrap()["state"], "completed");
        let plan = state.plan.unwrap();
        let (after, _, _) = app.session.agent_plan_snapshot(plan, false).unwrap();
        assert_eq!(after.notes, "Audio proposal");
        assert_eq!(app.session.project.notes, "");
        start(&mut app, &mut state, "agent.plan", params, true, None);
        app.session.execute("project.setNotes", json!({"text":"Owner continued editing"})).unwrap();
        wait(&mut app, &mut state);
        assert!(state.message.contains("stale_plan"), "{}", state.message);
        assert_eq!(app.session.project.notes, "Owner continued editing");
    }
    #[test]
    fn adopted_audio_evidence_always_names_the_actual_live_plan_identity() {
        let mut app = app();
        let existing = app.session.execute("agent.plan", json!({"steps":[{"id":"project.setNotes","params":{"text":"Unrelated existing proposal"}}]})).unwrap();
        let mut state = State::default();
        start(&mut app, &mut state, "agent.plan", json!({"steps":[{"id":"project.setNotes","params":{"text":"New audio proposal"}}]}), true, None);
        wait(&mut app, &mut state);
        let live = state.plan.unwrap();
        assert_ne!(Some(live), existing["plan"].as_u64());
        assert_eq!(state.report.as_ref().unwrap()["plan"], live);
        let (after, _, _) = app.session.agent_plan_snapshot(live, false).unwrap();
        assert_eq!(after.notes, "New audio proposal");
        assert_eq!(app.session.project.notes, "");
    }
    #[test]
    fn audio_target_cache_resolves_raw_selection_without_expanding_selected_microphones() {
        let mut app = app();
        app.session.execute("file.openDemoProject", json!({})).unwrap();
        let seq = app.session.state.active_sequence.unwrap();
        let a = app.session.active_sequence().unwrap().audio_tracks[0].items[0].id;
        let b = app.session.active_sequence().unwrap().audio_tracks[1].items[0].id;
        let video = app.session.active_sequence().unwrap().video_tracks[0].items[0].id;
        app.session
            .edit("AV group fixture", |p, _| {
                let q = p.sequence_mut(seq).unwrap();
                for clip in [a, b, video] {
                    q.find_item_mut(clip).unwrap().1.link = Some(777);
                }
                Ok(())
            })
            .unwrap();
        let mut state = State::default();
        app.session.state.selection = vec![a];
        refresh_channels(&mut app, &mut state);
        assert_eq!(state.selection, vec![a.0]);
        let untouched = app.session.active_sequence().unwrap().find_item(b).unwrap().1.effects.clone();
        command(&mut app, &mut state, "audio.channels.set", json!({"mode":"left"}));
        assert_eq!(
            app.session.active_sequence().unwrap().find_item(b).unwrap().1.effects,
            untouched,
            "camera/scratch audio is not an implicit explicit target"
        );
        app.session.state.selection = vec![video];
        refresh_channels(&mut app, &mut state);
        assert!(state.selection.contains(&a.0) && state.selection.contains(&b.0));
        assert!(!state.selection.contains(&video.0));
        let revision = state.revision;
        refresh_channels(&mut app, &mut state);
        assert_eq!(state.revision, revision);
        assert_eq!(state.requested_selection, vec![video.0]);
    }
    #[test]
    fn paired_native_audition_reads_actual_frozen_projects_without_a_plan_store_or_timeline_writes() {
        let mut app = app();
        let item = app.session.execute("file.newBarsAndTone", json!({"seconds":1,"width":64,"height":64})).unwrap()["item"].as_u64().unwrap();
        let clip = app.session.execute("timeline.place", json!({"item":item,"audio":true,"video":false,"audioTrack":"A1","time":0})).unwrap()["clips"][0]
            .as_u64()
            .unwrap();
        let before = Seed::capture(&app.session);
        app.session.execute("audio.channels.set", json!({"clip":clip,"leftGainDb":6})).unwrap();
        let after = Seed::capture(&app.session);
        let project = app.session.project.clone();
        let revision = app.session.revision;
        let mut state = State::default();
        start(&mut app, &mut state, "audio.preview", json!({"subject":{"target":"clip","clip":clip,"effects":"post"}}), false, Some((before, after, false)));
        wait(&mut app, &mut state);
        let (a, b) = state.previews.as_ref().unwrap_or_else(|| panic!("{}", state.message));
        let peak = |c: &[f32]| c.iter().copied().map(f32::abs).fold(0.0, f32::max);
        assert!(peak(&b.channels[0]) > 1.9 * peak(&a.channels[0]));
        assert!((peak(&b.channels[1]) - peak(&a.channels[1])).abs() < 1e-5);
        assert_eq!(app.session.project, project);
        assert_eq!(app.session.revision, revision);
    }
    #[test]
    fn audition_decode_preserves_stereo_and_rejects_malformed_transport() {
        let wav = filmcraft_media::wav::write_wav16(&[0.5, 0.0, 0.5, 0.0, 0.5, 0.0, 0.5, 0.0], 2, 48000);
        let value = json!({"audio":{"base64":base64::engine::general_purpose::STANDARD.encode(&wav)}});
        let decoded = decode_preview(&value).unwrap();
        assert_eq!(decoded.sample_rate, 48000);
        assert_eq!(decoded.channels.len(), 2);
        assert!(decoded.channels[0].iter().all(|v| *v > 0.49));
        assert!(decoded.channels[1].iter().all(|v| *v == 0.0));
        // Public RIFF/WAVE IEEE-float fixture: audition must retain headroom above unity.
        let samples = [1.25f32, 0.0, -1.25, 0.0];
        let bytes = (samples.len() * 4) as u32;
        let mut float = b"RIFF".to_vec();
        float.extend_from_slice(&(36 + bytes).to_le_bytes());
        float.extend_from_slice(b"WAVEfmt ");
        float.extend_from_slice(&16u32.to_le_bytes());
        float.extend_from_slice(&3u16.to_le_bytes());
        float.extend_from_slice(&2u16.to_le_bytes());
        float.extend_from_slice(&48000u32.to_le_bytes());
        float.extend_from_slice(&384000u32.to_le_bytes());
        float.extend_from_slice(&8u16.to_le_bytes());
        float.extend_from_slice(&32u16.to_le_bytes());
        float.extend_from_slice(b"data");
        float.extend_from_slice(&bytes.to_le_bytes());
        for value in samples {
            float.extend_from_slice(&value.to_le_bytes());
        }
        let decoded = decode_preview(&json!({"audio":{"base64":base64::engine::general_purpose::STANDARD.encode(float)}})).unwrap();
        assert_eq!(decoded.channels[0], vec![1.25, -1.25]);
        assert_eq!(decoded.channels[1], vec![0.0, 0.0]);
        assert!(decode_preview(&json!({"audio":{"base64":"?not-base64"}})).is_err());
        for cut in 0..wav.len() {
            let malformed = json!({"audio":{"base64":base64::engine::general_purpose::STANDARD.encode(&wav[..cut])}});
            let result = std::panic::catch_unwind(|| decode_preview(&malformed));
            assert!(result.is_ok(), "truncation {cut}");
        }
    }
    #[test]
    fn narrow_panel_tabs_and_channel_controls_are_real_registered_widgets() {
        let mut app = app();
        let ctx = egui::Context::default();
        let frame = |app: &mut FilmcraftApp, events: Vec<egui::Event>| {
            app.auto.begin_frame();
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(220.0, 700.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |root| {
                egui::CentralPanel::default().show(root, |ui| {
                    let rect = ui.available_rect_before_wrap();
                    super::super::essential_sound::show(app, ui, rect);
                });
            });
            output.textures_delta.clear();
        };
        frame(&mut app, vec![]);
        let r = app.auto.find("essentialSound.tab.Tools").unwrap().rect;
        let pos = egui::pos2(r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
        frame(
            &mut app,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() },
            ],
        );
        frame(&mut app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() }]);
        frame(&mut app, vec![]);
        assert_eq!(app.ui.essential_sound_tab, "Tools");
        for id in [
            "audioTools.left",
            "audioTools.right",
            "audioTools.mono",
            "audioTools.swap",
            "audioTools.original",
            "audioTools.prepare",
            "audioTools.inspect",
            "audioTools.preview",
            "audioTools.sync",
        ] {
            let element = app.auto.find(id).unwrap_or_else(|| panic!("missing {id}"));
            assert!(element.widget_id.is_some(), "{id}");
            assert!(element.rect[0] >= 0.0 && element.rect[0] + element.rect[2] <= 220.5, "{id}: {:?}", element.rect);
        }
        assert!(app.auto.find("essentialSound.tab.Edit").is_some());
        assert!(app.auto.find("essentialSound.tab.Browse").is_some());
    }
}
