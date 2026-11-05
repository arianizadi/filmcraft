//! Simple native footage-to-proposal entry. All controls are registered, and preparation runs
//! against an immutable background snapshot so the owner can keep editing.
use crate::FilmcraftApp;
use filmcraft_engine::{agent_plans::PreparedEditPlan, workflow};
use serde_json::{Value, json};
use std::sync::mpsc::TryRecvError;
#[derive(Default)]
pub struct AgentWorkflow {
    paths: String,
    items: String,
    profile: String,
    mode: usize,
    pending: Option<workflow::BackgroundJob<PreparedEditPlan>>,
    message: String,
    output: String,
    delivery: Option<Delivery>,
    qc: Option<workflow::BackgroundJob<Value>>,
    model_download: Option<workflow::BackgroundJob<Value>>,
    model_catalog: Option<Result<Value, String>>,
    result: Option<Value>,
}
struct Delivery {
    job: u64,
    qc: Value,
    snapshot: workflow::QcSnapshot,
}
fn button(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, label: &str, enabled: bool) -> egui::Response {
    let r = ui.add_enabled(enabled, egui::Button::new(label));
    app.auto.add_response(id, &r, label);
    r
}
fn suggested_output(app: &FilmcraftApp, state: &AgentWorkflow, report: &Value) -> String {
    let source = state
        .paths
        .lines()
        .find(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            state.items.split([',', ' ', '\n']).filter_map(|s| s.parse::<u64>().ok()).find_map(|id| {
                app.session
                    .project
                    .item(filmcraft_engine::project::ItemId(id))
                    .and_then(|i| i.as_media())
                    .and_then(|m| if let filmcraft_engine::project::MediaRef::File { path } = &m.media { Some(path.clone()) } else { None })
            })
        })
        .or_else(|| app.session.path.clone());
    let Some(parent) = source.as_deref().and_then(|p| std::path::Path::new(p).parent()) else { return String::new() };
    let ext = match report["workflow"]["profile"]["delivery"]["format"].as_str().unwrap_or("h264") {
        "prores" | "dnxhr" => "mov",
        "wav" => "wav",
        "png" => "png",
        _ => "mp4",
    };
    for index in 0..1000 {
        let name = format!("FilmCraft-edit-{}-{index}.{ext}", report["plan"]);
        let path = parent.join(name);
        if !path.exists() && !path.with_extension("srt").exists() && !path.with_extension("vtt").exists() {
            return path.to_string_lossy().into_owned();
        }
    }
    String::new()
}
fn poll(app: &mut FilmcraftApp, state: &mut AgentWorkflow, ctx: &egui::Context) -> Option<u64> {
    let mut selected = None;
    if let Some(receiver) = &state.model_download {
        match receiver.receiver.try_recv() {
            Ok(result) => {
                state.model_download = None;
                state.model_catalog = None;
                state.message = match result {
                    Ok(_) => "Local speech model installed. Prepare the footage again to propose dialogue cuts and captions.".into(),
                    Err(e) => e.to_string(),
                };
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(200)),
            Err(TryRecvError::Disconnected) => {
                state.model_download = None;
                state.message = "Speech-model download interrupted. Prepare will preserve unrecognized dialogue.".into();
            }
        }
    }

    if let Some(receiver) = &state.pending {
        match receiver.receiver.try_recv() {
            Ok(result) => {
                let Some(task) = state.pending.take() else { return selected };
                match workflow::adopt_background_plan(&mut app.session, &task, result) {
                    Ok(report) => {
                        selected = report["plan"].as_u64();
                        if state.output.trim().is_empty() {
                            state.output = suggested_output(app, state, &report);
                        }
                        state.message = "Ready to review. Inspect the changes and exact frames, then approve.".into();
                    }
                    Err(e) => state.message = e.to_string(),
                }
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            Err(TryRecvError::Disconnected) => {
                if let Some(task) = state.pending.take() {
                    let error = filmcraft_engine::EngineError::Other("preparation worker interrupted".into());
                    let _ = workflow::adopt_background_plan(&mut app.session, &task, Err(error));
                }
                state.message = "Preparation worker interrupted; the project is unchanged.".into();
            }
        }
    }
    if let Some(delivery) = &state.delivery {
        let status = app.session.jobs.iter().find(|j| j.id == delivery.job).map(|j| j.to_json());
        if let Some(job) = status {
            if let Some(result) = job.get("result").filter(|v| !v.is_null()) {
                if let Some(error) = result["error"].as_str() {
                    state.message = format!("Export failed: {error}");
                    state.delivery = None;
                } else {
                    match workflow::qc_background_from(&mut app.session, delivery.snapshot.clone(), delivery.qc.clone()) {
                        Ok(receiver) => {
                            state.qc = Some(receiver);
                            state.message = "Export finished. Reopening and verifying the delivered file…".into();
                        }
                        Err(e) => state.message = e.to_string(),
                    }
                    state.delivery = None;
                }
            } else {
                state.message = format!("Export job {} is running. You can keep editing.", delivery.job);
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
    }
    if let Some(receiver) = &state.qc {
        match receiver.receiver.try_recv() {
            Ok(result) => {
                state.qc = None;
                match result {
                    Ok(result) => {
                        state.message = if result["verified"] == true {
                            "Export verified against the delivery request.".into()
                        } else {
                            "Export finished with failed or unknown checks. Review the verification details.".into()
                        };
                        state.result = Some(result);
                    }
                    Err(e) => state.message = e.to_string(),
                }
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
            Err(TryRecvError::Disconnected) => {
                state.qc = None;
                state.message = "Verification worker interrupted. The export has not been certified.".into();
            }
        }
    }
    selected
}
fn task_progress<T>(app: &mut FilmcraftApp, ui: &mut egui::Ui, task: &workflow::BackgroundJob<T>, id: &str) {
    let job = &task.job;
    let status = job.progress.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let record = app.session.job_registry.get(job.id).unwrap_or(Value::Null);
    let text = format!("Job #{} · input revision {} · {}", job.id, record["inputRevision"], status);
    let r = ui.add(egui::ProgressBar::new(job.progress.fraction()).text(&text));
    app.auto.add_response(&format!("agentWorkflow.{id}.progress"), &r, &text);
    if button(app, ui, &format!("agentWorkflow.{id}.cancel"), "Cancel job", !job.progress.cancel.load(std::sync::atomic::Ordering::Relaxed)).clicked() {
        task.cancel();
    }
}

fn speech_model_controls(app: &mut FilmcraftApp, state: &mut AgentWorkflow, ui: &mut egui::Ui) {
    // Installed status verifies model hashes. Keep that filesystem work out of the frame loop;
    // an explicit refresh or a completed download invalidates this catalogue.
    if state.model_catalog.is_none() {
        state.model_catalog = Some(app.session.execute("transcript.models", json!({})).map_err(|e| e.to_string()));
    }
    if button(app, ui, "agentWorkflow.refreshModels", "Refresh local speech models", state.model_download.is_none()).clicked() {
        state.model_catalog = None;
        ui.ctx().request_repaint();
        return;
    }
    let Some(Ok(models)) = &state.model_catalog else {
        if let Some(Err(error)) = &state.model_catalog {
            ui.label(error);
        }
        return;
    };
    let default = models["default"].as_str().unwrap_or("whisper-base");
    let Some(model) = models["models"].as_array().and_then(|m| m.iter().find(|m| m["id"] == default)) else { return };
    if model["installed"] == true {
        ui.label("Local dialogue recognition is installed.");
        return;
    }
    let label = "Optional local dialogue recognition";
    let collapse = egui::CollapsingHeader::new(label).id_salt("agentWorkflow.speechModel").show(ui, |ui| {
        ui.label(format!(
            "{} · {} bytes · {}",
            model["name"].as_str().unwrap_or(default),
            model["size"],
            model["license"].as_str().unwrap_or("See model catalogue")
        ));
        ui.label(model["source"].as_str().unwrap_or("Source in transcript.models"));
        ui.label("Download only if you want local recognition. Original footage stays on this computer; without a model, dialogue is preserved.");
        let enabled = filmcraft_engine::find_command("transcript.downloadModel").is_some_and(|command| (command.enabled)(&app.session).is_ok())
            && state.model_download.is_none();
        if button(app, ui, "agentWorkflow.downloadModel", "Download this local model", enabled).clicked() {
            match workflow::download_model_background(&mut app.session, default.to_string()) {
                Ok(receiver) => {
                    state.model_download = Some(receiver);
                    state.message = "Downloading the chosen model. You can keep editing.".into();
                }
                Err(e) => state.message = e.to_string(),
            }
        }
    });
    app.auto.add_response("agentWorkflow.speechModel", &collapse.header_response, label);
}

pub(crate) fn show(app: &mut FilmcraftApp, state: &mut AgentWorkflow, ui: &mut egui::Ui, selected: Option<u64>) -> Option<u64> {
    let mut proposed = poll(app, state, ui.ctx());
    let collapse = egui::CollapsingHeader::new("Automatic edit from footage").id_salt("agentWorkflow").show(ui, |ui| {
        ui.label("Choose original footage, pick a profile and prepare a proposal. Your source files stay unchanged. Review and approve before export.");
        let picked_key = egui::Id::unique("agentWorkflow.chosenPaths");
        if let Some(paths) = ui.ctx().data_mut(|d| d.remove_temp::<Vec<String>>(picked_key)) {
            state.paths = paths.join("\n");
        }
        let dialog = crate::FileDialog::open_files(&["mp4", "mov", "mkv", "avi", "mxf", "webm", "wav", "aiff", "flac", "mp3", "png", "jpg", "tiff", "exr"]);
        if button(app, ui, "agentWorkflow.choose", "Choose footage…", state.pending.is_none() && dialog.available(&app.hooks)).clicked() {
            let ctx = ui.ctx().clone();
            app.pick_ui(dialog, move |_, paths| {
                ctx.data_mut(|d| d.insert_temp(picked_key, paths));
            });
        }
        ui.label("Source paths (one absolute path per line)");
        let response = ui.add(egui::TextEdit::multiline(&mut state.paths).desired_rows(2));
        app.auto.add_response("agentWorkflow.paths", &response, "Source footage paths");
        ui.horizontal(|ui| {
            ui.label("Or imported source IDs");
            let r = ui.text_edit_singleline(&mut state.items);
            app.auto.add_response("agentWorkflow.items", &r, "Imported source IDs");
            if button(app, ui, "agentWorkflow.selection", "Use selected sources", true).clicked() {
                state.items = app
                    .session
                    .state
                    .project_selection
                    .iter()
                    .filter(|id| app.session.project.item(**id).is_some_and(|i| i.as_media().is_some()))
                    .map(|i| i.0.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                state.paths.clear();
            }
        });
        ui.horizontal(|ui| {
            for (index, label) in ["Talking head", "DJ / music", "Selects"].into_iter().enumerate() {
                let r = ui.selectable_label(state.mode == index, label);
                app.auto.add_response(&format!("agentWorkflow.mode.{index}"), &r, label);
                if r.clicked() {
                    state.mode = index;
                }
            }
        });
        speech_model_controls(app, state, ui);
        ui.label("Saved profile name (empty uses an automatic profile)");
        let r = ui.text_edit_singleline(&mut state.profile);
        app.auto.add_response("agentWorkflow.profile", &r, "Editing profile name");
        if button(
            app,
            ui,
            "agentWorkflow.prepare",
            "Prepare automatic proposal",
            state.pending.is_none() && (!state.paths.trim().is_empty() || !state.items.trim().is_empty()),
        )
        .clicked()
        {
            let mode = match state.mode {
                1 => "dj",
                2 => "selects",
                _ => "talkingHead",
            };
            let mut p = json!({"mode":mode,"transcribe":true,"analyze":true,"apply":false});
            let parsed = if state.paths.trim().is_empty() {
                state.items.split([',', ' ', '\n']).filter(|s| !s.is_empty()).map(str::parse::<u64>).collect::<Result<Vec<_>, _>>().map(|ids| {
                    p["items"] = json!(ids);
                })
            } else {
                p["paths"] = json!(state.paths.lines().map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>());
                Ok(())
            };
            if !state.profile.trim().is_empty() {
                p["profile"] = json!(state.profile.trim());
            }
            match parsed {
                Ok(()) => match workflow::prepare_background(&mut app.session, p) {
                    Ok(receiver) => {
                        state.pending = Some(receiver);
                        state.result = None;
                        state.message =
                            "Analyzing and preparing a fixed snapshot. You can keep editing; a changed baseline will require a fresh proposal.".into();
                    }
                    Err(e) => state.message = e.to_string(),
                },
                Err(e) => state.message = format!("Invalid source ID: {e}"),
            }
        }
        if let Some(task) = &state.pending {
            task_progress(app, ui, task, "preparation");
        }
        if let Some(task) = &state.qc {
            task_progress(app, ui, task, "verification");
        }
        if let Some(task) = &state.model_download {
            task_progress(app, ui, task, "modelDownload");
        }
        ui.separator();
        let output_key = egui::Id::unique("agentWorkflow.chosenOutput");
        if let Some(path) = ui.ctx().data_mut(|d| d.remove_temp::<String>(output_key)) {
            state.output = path;
        }
        ui.label("Output file");
        let dialog = crate::FileDialog::save_as(
            "Video",
            &["mp4", "mov", "wav"],
            std::path::Path::new(&state.output).file_name().and_then(|n| n.to_str()).unwrap_or("FilmCraft-edit.mp4"),
        );
        if button(app, ui, "agentWorkflow.chooseOutput", "Choose output…", dialog.available(&app.hooks)).clicked() {
            let ctx = ui.ctx().clone();
            app.pick_ui(dialog, move |_, paths| {
                if let Some(path) = paths.into_iter().next() {
                    ctx.data_mut(|d| d.insert_temp(output_key, path));
                }
            });
        }

        let r = ui.text_edit_singleline(&mut state.output);
        app.auto.add_response("agentWorkflow.output", &r, "Delivery output path");
        let approved = selected.and_then(|id| app.session.execute("agent.preview", json!({"plan":id})).ok()).is_some_and(|p| p["status"] == "applied");
        if button(
            app,
            ui,
            "agentWorkflow.export",
            "Export approved edit and verify",
            approved && !state.output.trim().is_empty() && state.delivery.is_none() && state.qc.is_none(),
        )
        .clicked()
            && let Some(plan) = selected
        {
            let snapshot = workflow::QcSnapshot::capture(&app.session);
            match app.session.execute("workflow.deliver", json!({"plan":plan,"path":state.output.trim(),"wait":false})) {
                Ok(result) => {
                    if let Some(job) = result["export"]["job"].as_u64() {
                        state.delivery = Some(Delivery { job, qc: result["fileQc"]["params"].clone(), snapshot });
                        state.message = "Export started. Verification follows when the job completes.".into();
                    } else {
                        state.message = "Export returned no trackable job ID. Inspect jobs.list.".into();
                    }
                    state.result = Some(result);
                }
                Err(e) => state.message = e.to_string(),
            }
        }
        if !state.message.is_empty() {
            let response = ui.label(&state.message);
            app.auto.add_response("agentWorkflow.message", &response, &state.message);
        }
        if let Some(result) = &state.result {
            let collapse = egui::CollapsingHeader::new("Delivery checks").id_salt("agentWorkflow.results").show(ui, |ui| {
                let text = serde_json::to_string_pretty(result).unwrap_or_else(|e| e.to_string());
                let r = ui.monospace(text.chars().take(24_000).collect::<String>());
                app.auto.add_response("agentWorkflow.results.details", &r, "Delivery verification details");
            });
            app.auto.add_response("agentWorkflow.results", &collapse.header_response, "Delivery checks");
        }
    });
    app.auto.add_response("agentWorkflow", &collapse.header_response, "Automatic edit from footage");
    // A second frame can finish a short preparation while controls were drawn.
    if proposed.is_none() {
        proposed = poll(app, state, ui.ctx());
    }
    proposed
}
