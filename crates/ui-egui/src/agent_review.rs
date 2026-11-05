//! Desktop review of immutable agent proposals. Permission state is deliberately outside UiState:
//! generic `ui.set` cannot grant a remote agent additional capabilities.

use std::sync::mpsc::{Receiver, TryRecvError};

use filmcraft_engine::{Session, inspection, operations::Permission};
use filmcraft_render::Image;
use serde_json::{Value, json};

use crate::FilmcraftApp;

#[derive(Default)]
pub struct AgentReview {
    pub open: bool,
    compact: bool,
    selected: Option<u64>,
    frame: i64,
    pending: Option<(PreviewKey, Receiver<Result<RenderedPair, String>>)>,
    preview: Option<PreviewTextures>,
    message: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PreviewKey {
    plan: u64,
    sequence: u64,
    frame: i64,
}
struct RenderedPair {
    before: Image,
    after: Image,
    before_metadata: Value,
    after_metadata: Value,
    offline: bool,
}
struct PreviewTextures {
    key: PreviewKey,
    before: egui::TextureHandle,
    after: egui::TextureHandle,
    before_metadata: Value,
    after_metadata: Value,
    offline: bool,
}

fn button(app: &mut FilmcraftApp, ui: &mut egui::Ui, id: &str, label: &str, enabled: bool) -> egui::Response {
    let response = ui.add_enabled(enabled, egui::Button::new(label));
    app.auto.add_response(id, &response, label);
    response
}

fn permission_picker(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    ui.label("Agent permissions");
    ui.horizontal(|ui| {
        for (permission, label, description) in [
            (Permission::Inspect, "Inspect", "Read the project and render frames."),
            (Permission::Propose, "Propose", "Inspect and prepare proposals. You approve timeline changes."),
            (Permission::Edit, "Edit", "Allow agents to change the project and run jobs directly."),
        ] {
            let response = ui.selectable_label(app.session.operations.permission == permission, label).on_hover_text(description);
            app.auto.add_response(&format!("agentReview.permission.{}", label.to_ascii_lowercase()), &response, label);
            if response.clicked() {
                app.session.operations.permission = permission;
            }
        }
    });
    let description = match app.session.operations.permission {
        Permission::Inspect => "Agents can inspect this session. Proposals and edits are blocked.",
        Permission::Propose => "Agents prepare edits for review. Only your approval applies a proposal.",
        Permission::Edit => "Agents can apply edits directly. Undo remains available.",
    };
    ui.label(description);
}

fn concise_json(value: &Value) -> String {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("Cannot display details: {e}"));
    if text.len() > 24_000 { format!("{}\n… Details continue in agent.preview.", text.chars().take(20_000).collect::<String>()) } else { text }
}

fn description(change: &Value, entity: &str) -> String {
    let action = if change.get("moved").and_then(Value::as_bool) == Some(true) { "moved" } else { change["change"].as_str().unwrap_or("changed") };
    let id = change[entity].as_u64().unwrap_or(0);
    let name = change["after"]["name"].as_str().or_else(|| change["before"]["name"].as_str()).unwrap_or("");
    let grade = if change["gradeChanged"].as_bool() == Some(true) { " · grade changed" } else { "" };
    if name.is_empty() { format!("{action} {entity} #{id}{grade}") } else { format!("{action} {entity} #{id} — {name}{grade}") }
}

fn changes(app: &mut FilmcraftApp, ui: &mut egui::Ui, report: &Value) {
    ui.heading("Proposed changes");
    if report["noChange"].as_bool() == Some(true) {
        ui.label("This proposal makes no project changes.");
    }
    if let Some(sequences) = report["diff"]["sequences"].as_array() {
        for sequence in sequences {
            ui.label(egui::RichText::new(format!("Sequence #{}", sequence["sequence"])).strong());
            for (key, entity) in [("clips", "clip"), ("tracks", "track"), ("captions", "caption"), ("transitions", "transition")] {
                if let Some(entries) = sequence[key].as_array() {
                    for entry in entries {
                        let id = format!(
                            "agentReview.diff.{}.{}.{entity}.{}.{}",
                            report["plan"],
                            sequence["sequence"],
                            entry[entity],
                            entry.get("track").and_then(Value::as_u64).map_or_else(|| "clip".into(), |id| id.to_string())
                        );
                        let label = description(entry, entity);
                        let collapse = egui::CollapsingHeader::new(&label).id_salt(&id).show(ui, |ui| {
                            let details = concise_json(entry);
                            let response = ui.monospace(&details);
                            app.auto.add_response(&format!("{id}.details"), &response, &details);
                        });
                        app.auto.add_response(&id, &collapse.header_response, &label);
                    }
                }
            }
            if sequence["settingsChanged"].as_bool() == Some(true) {
                ui.label("Sequence settings change.");
            }
            if sequence["markersChanged"].as_bool() == Some(true) {
                ui.label("Sequence markers change.");
            }
        }
    }
    if let Some(fields) = report["diff"]["projectFields"].as_array().filter(|v| !v.is_empty()) {
        ui.label(format!("Project changes: {}", fields.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
    }
    if let Some(warnings) = report["diff"]["warnings"].as_array().filter(|v| !v.is_empty()) {
        ui.separator();
        ui.label(egui::RichText::new("Review warnings").strong());
        for warning in warnings {
            ui.label(format!("{}  {}", warning["code"].as_str().unwrap_or("warning"), warning["message"].as_str().unwrap_or("")));
            if let Some(clip) = warning.get("clip") {
                ui.small(format!("Clip #{clip}"));
            }
        }
    }
    let label = "Analysis and command results";
    let collapse = egui::CollapsingHeader::new(label).id_salt(("agentReview.commands", report["plan"].as_u64())).show(ui, |ui| {
        let details = concise_json(&json!({"commands":report["commands"],"results":report["steps"]}));
        let response = ui.monospace(&details);
        app.auto.add_response("agentReview.commands.details", &response, &details);
    });
    app.auto.add_response("agentReview.commands", &collapse.header_response, label);
}

fn texture(ctx: &egui::Context, name: &str, image: Image) -> Result<egui::TextureHandle, String> {
    let size = [image.w, image.h];
    let rgba = image.over_black_rgba8();
    if size[0] == 0 || size[1] == 0 || size[0].checked_mul(size[1]).and_then(|n| n.checked_mul(4)) != Some(rgba.len()) {
        return Err("The preview renderer returned invalid image dimensions.".into());
    }
    let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
    Ok(ctx.load_texture(name, image, egui::TextureOptions::LINEAR))
}

fn render_pair(reader: &Session, before: inspection::Subject, after: inspection::Subject, frame: i64) -> Result<RenderedPair, String> {
    let positions = json!({"frame":frame,"maxSide":640});
    let t_before = before.times(&positions).map_err(|e| e.to_string())?.first().copied().ok_or("No before frame was requested")?;
    let t_after = after.times(&positions).map_err(|e| e.to_string())?.first().copied().ok_or("No after frame was requested")?;
    let scale_before = before.scale(&positions, 2).map_err(|e| e.to_string())?;
    let scale_after = after.scale(&positions, 2).map_err(|e| e.to_string())?;
    let (a, a_offline) = before.render(reader, t_before, scale_before, false).map_err(|e| e.to_string())?;
    let (b, b_offline) = after.render(reader, t_after, scale_after, false).map_err(|e| e.to_string())?;
    let offline = a_offline.as_array().is_some_and(|v| !v.is_empty()) || b_offline.as_array().is_some_and(|v| !v.is_empty());
    Ok(RenderedPair { before: a, after: b, before_metadata: before.metadata(t_before, false), after_metadata: after.metadata(t_after, false), offline })
}

fn request_preview(app: &mut FilmcraftApp, review: &mut AgentReview, ctx: &egui::Context, key: PreviewKey) {
    let p = json!({"plan":key.plan,"sequence":key.sequence});
    let subjects = inspection::subject(&app.session, &p, true).and_then(|before| inspection::subject(&app.session, &p, false).map(|after| (before, after)));
    let (before, after) = match subjects {
        Ok(subjects) => subjects,
        Err(e) => {
            review.message = Some(e.to_string());
            return;
        }
    };
    let mut reader = Session::new(app.session.services.clone());
    reader.media = app.session.media.clone();
    review.message = None;
    #[cfg(not(target_arch = "wasm32"))]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        let spawn = std::thread::Builder::new().name("agent-review-preview".into()).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| render_pair(&reader, before, after, key.frame)))
                .unwrap_or_else(|_| Err("Preview failed internally. The project is unchanged; see the crash log.".into()));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        match spawn {
            Ok(_) => review.pending = Some((key, rx)),
            Err(e) => review.message = Some(format!("Cannot start preview: {e}")),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        match render_pair(&reader, before, after, key.frame) {
            Ok(pair) => install_preview(review, ctx, key, pair),
            Err(e) => review.message = Some(e),
        }
    }
}

fn install_preview(review: &mut AgentReview, ctx: &egui::Context, key: PreviewKey, pair: RenderedPair) {
    let textures =
        texture(ctx, "agent-review-before", pair.before).and_then(|before| texture(ctx, "agent-review-after", pair.after).map(|after| (before, after)));
    match textures {
        Ok((before, after)) => {
            review.preview =
                Some(PreviewTextures { key, before, after, before_metadata: pair.before_metadata, after_metadata: pair.after_metadata, offline: pair.offline })
        }
        Err(e) => review.message = Some(e),
    }
}

fn poll_preview(review: &mut AgentReview, ctx: &egui::Context) {
    let Some((key, rx)) = &review.pending else { return };
    let key = *key;
    let result = match rx.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Err("Preview worker stopped before returning a frame.".into())),
    };
    if let Some(result) = result {
        review.pending = None;
        if review.selected != Some(key.plan) {
            return;
        }
        match result {
            Ok(pair) => install_preview(review, ctx, key, pair),
            Err(e) => review.message = Some(e),
        }
    }
}

fn preview_controls(app: &mut FilmcraftApp, review: &mut AgentReview, ui: &mut egui::Ui, report: &Value) {
    let Some(plan) = review.selected else { return };
    let (_, state, _) = match app.session.agent_plan_snapshot(plan, false) {
        Ok(s) => s,
        Err(_) => return,
    };
    let sequence = report["diff"]["sequences"].as_array().and_then(|q| q.first()).and_then(|q| q["sequence"].as_u64()).or(state.active_sequence.map(|i| i.0));
    let Some(sequence) = sequence else { return };
    ui.separator();
    ui.label(egui::RichText::new("Exact before / after frames").strong());
    ui.horizontal(|ui| {
        ui.label("Sequence frame");
        let response = ui.add(egui::DragValue::new(&mut review.frame).range(0..=1_000_000_000i64));
        app.auto.add_response("agentReview.frame", &response, "Sequence frame");
        if button(app, ui, "agentReview.preview", "Render before / after", review.pending.is_none()).clicked() {
            request_preview(app, review, ui.ctx(), PreviewKey { plan, sequence, frame: review.frame });
        }
    });
    if review.pending.is_some() {
        ui.spinner();
        ui.label("Rendering the fixed project snapshots…");
    }
    if let Some(preview) = review.preview.as_ref().filter(|p| p.key.plan == plan) {
        ui.small(format!("Sequence #{} · frame {} · revision {}", preview.key.sequence, preview.key.frame, preview.after_metadata["revision"]));
        if preview.key.frame != review.frame {
            ui.label("The displayed pair is from a different frame. Render again to refresh it.");
        }
        ui.columns(2, |columns| {
            for (column, texture, title, metadata) in
                [(0, &preview.before, "Before", &preview.before_metadata), (1, &preview.after, "Proposed", &preview.after_metadata)]
            {
                if let Some(ui) = columns.get_mut(column) {
                    ui.label(title);
                    let response =
                        ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(ui.available_width(), ui.available_width() / texture.aspect_ratio())));
                    app.auto.add_response(if column == 0 { "agentReview.before" } else { "agentReview.after" }, &response, title);
                    ui.small(format!(
                        "{} · {}",
                        metadata["timecode"].as_str().unwrap_or(""),
                        metadata["colorTransform"]["workingSpace"].as_str().unwrap_or("")
                    ));
                }
            }
        });
        if preview.offline {
            ui.label("Missing media appears as a slate in this preview. Relink before approving.");
        }
    }
}

fn proposal(app: &mut FilmcraftApp, review: &mut AgentReview, ui: &mut egui::Ui, report: Value) {
    let status = report["status"].as_str().unwrap_or("unknown");
    let stale = report["stale"].as_bool().unwrap_or(true);
    ui.heading(report["label"].as_str().unwrap_or("Agent proposal"));
    ui.label(format!("{status} · built at revision {} · current revision {}", report["baseRevision"], app.session.revision));
    if stale && status == "staged" {
        ui.label("The project changed. Ask the agent to inspect again and prepare a fresh proposal.");
    }
    changes(app, ui, &report);
    preview_controls(app, review, ui, &report);
    ui.separator();
    ui.horizontal(|ui| {
        let eligible = status == "staged" && !stale;
        if button(app, ui, "agentReview.approve", "Approve as one undo step", eligible).clicked()
            && let Some(plan) = review.selected
        {
            match app.session.execute("agent.apply", json!({"plan":plan})) {
                Ok(_) => {
                    review.message = Some("Applied. Undo returns the whole proposal in one step.".into());
                    app.ui.status = "Agent proposal applied".into();
                }
                Err(e) => review.message = Some(e.to_string()),
            }
        }
        if button(app, ui, "agentReview.discard", "Discard", status == "staged").clicked()
            && let Some(plan) = review.selected
        {
            match app.session.execute("agent.discard", json!({"plan":plan})) {
                Ok(_) => {
                    review.preview = None;
                    review.message = Some("Proposal discarded.".into());
                }
                Err(e) => review.message = Some(e.to_string()),
            }
        }
    });
}

pub(crate) fn show(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let mut review = std::mem::take(&mut app.agent_review);
    poll_preview(&mut review, ctx);
    if !review.open {
        app.agent_review = review;
        return;
    }
    let mut close = false;
    let window = egui::Window::new("Agent Review").id(egui::Id::unique("agent-review")).collapsible(false).default_size(egui::vec2(860.0, 650.0));
    let window = if review.compact { window.auto_sized() } else { window };
    let shown = window.show(ctx, |ui| {
        ui.horizontal(|ui| {
            let label = if review.compact { "Expand review" } else { "Collapse review" };
            if button(app, ui, "agentReview.collapse", label, true).clicked() {
                review.compact = !review.compact;
            }
            close = button(app, ui, "agentReview.close", "Close", true).clicked();
        });
        if review.compact {
            return;
        }
        permission_picker(app, ui);
        ui.separator();
        let scroll = egui::ScrollArea::vertical().id_salt("agentReview.scroll").max_height(ui.available_height()).show(ui, |ui| {
            let list = app.session.execute("agent.plans", json!({}));
            match list {
                Ok(list) => {
                    let entries = list["plans"].as_array().cloned().unwrap_or_default();
                    if entries.is_empty() {
                        ui.label("No proposals yet. Connect an agent with Propose permission and ask it to prepare an edit.");
                    }
                    for entry in &entries {
                        let Some(id) = entry["plan"].as_u64() else { continue };
                        let label = format!("{} · {}", entry["label"].as_str().unwrap_or("Agent edit"), entry["status"].as_str().unwrap_or("unknown"));
                        let response = ui.selectable_label(review.selected == Some(id), &label);
                        app.auto.add_response(&format!("agentReview.plan.{id}"), &response, &label);
                        if response.clicked() {
                            review.selected = Some(id);
                            review.preview = None;
                            review.message = None;
                            review.frame = 0;
                        }
                    }
                    if review.selected.is_none() {
                        review.selected = entries.iter().rev().find(|p| p["status"] == "staged").and_then(|p| p["plan"].as_u64());
                    }
                    if let Some(plan) = review.selected {
                        ui.separator();
                        match app.session.execute("agent.preview", json!({"plan":plan})) {
                            Ok(report) => proposal(app, &mut review, ui, report),
                            Err(e) => {
                                ui.label(e.to_string());
                            }
                        }
                    }
                }
                Err(e) => {
                    ui.label(e.to_string());
                }
            }
            if let Some(message) = &review.message {
                ui.separator();
                ui.label(message);
            }
            let label = "Agent activity";
            let collapse = egui::CollapsingHeader::new(label).id_salt("agentReview.activity").show(ui, |ui| {
                for receipt in app.session.operations.activity().as_array().into_iter().flatten() {
                    ui.label(format!(
                        "{} · {} · revision {} → {}",
                        receipt["command"].as_str().unwrap_or("operation"),
                        receipt["status"].as_str().unwrap_or("unknown"),
                        receipt["beforeRevision"],
                        receipt["afterRevision"]
                    ));
                }
                let text = format!("Session {}", app.session.operations.session_id);
                let response = ui.small(&text);
                app.auto.add_response("agentReview.activity.session", &response, &text);
            });
            app.auto.add_response("agentReview.activity", &collapse.header_response, label);
        });
        app.auto.add("agentReview.scroll", scroll.inner_rect, "Agent review scrolling area");
    });
    if let Some(shown) = shown {
        app.auto.add("agentReview.window", shown.response.rect, "Agent Review");
    }
    review.open &= !close;
    app.agent_review = review;
}
