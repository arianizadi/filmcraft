//! Native-only approval of individual agent connections and their project folder grants.
use crate::FilmcraftApp;
use filmcraft_engine::{operations::Permission, pairing::Grant};
#[derive(Clone)]
struct Draft {
    project: String,
    media: String,
    output: String,
    permission: Permission,
}

pub(crate) fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui) {
    let collapse = egui::CollapsingHeader::new("Connections and folders").id_salt("agentPairing.connections").show(ui, |ui| {
        ui.label("Pair an agent with the launcher’s pair command. Approve its saved project, permission and media/output folders here.");
        let entries = app.session.pairings.list();
        if entries.is_empty() {
            ui.label("No individual connections yet. The owner connection uses the session permission above.");
        }
        for entry in entries {
            let Some(id) = entry["id"].as_str() else { continue };
            let Ok(grant) = serde_json::from_value::<Grant>(entry["grant"].clone()) else { continue };
            let key = egui::Id::unique(("agentPairing.draft", id));
            let mut draft = ui.ctx().data_mut(|d| d.get_temp::<Draft>(key)).unwrap_or_else(|| Draft {
                project: grant.project_path,
                media: grant.media_folders.join("\n"),
                output: grant.output_folders.join("\n"),
                permission: grant.permission,
            });
            let label =
                format!("{} · {}", entry["label"].as_str().unwrap_or("Agent"), if entry["approved"] == true { "approved" } else { "awaiting approval" });
            ui.group(|ui| {
                ui.strong(&label);
                ui.label("Saved project");
                let r = ui.text_edit_singleline(&mut draft.project);
                app.auto.add_response(&format!("agentPairing.{id}.project"), &r, "Saved project path");
                ui.horizontal(|ui| {
                    for (permission, label) in [(Permission::Inspect, "Inspect"), (Permission::Propose, "Propose"), (Permission::Edit, "Edit")] {
                        let r = ui.selectable_label(draft.permission == permission, label);
                        app.auto.add_response(&format!("agentPairing.{id}.{}", label.to_lowercase()), &r, label);
                        if r.clicked() {
                            draft.permission = permission;
                        }
                    }
                });
                ui.label("Media folders (one absolute folder per line)");
                let r = ui.add(egui::TextEdit::multiline(&mut draft.media).desired_rows(2));
                app.auto.add_response(&format!("agentPairing.{id}.media"), &r, "Approved media folders");
                ui.label("Output folders (one absolute folder per line)");
                let r = ui.add(egui::TextEdit::multiline(&mut draft.output).desired_rows(2));
                app.auto.add_response(&format!("agentPairing.{id}.output"), &r, "Approved output folders");
                ui.horizontal(|ui| {
                    let r = ui.button(if entry["approved"] == true { "Save grant" } else { "Approve connection" });
                    app.auto.add_response(&format!("agentPairing.{id}.approve"), &r, "Approve connection and folders");
                    if r.clicked() {
                        let grant = Grant {
                            project_path: draft.project.clone(),
                            permission: draft.permission,
                            media_folders: draft.media.lines().filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_owned()).collect(),
                            output_folders: draft.output.lines().filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_owned()).collect(),
                        };
                        match app.session.pairings.approve(id, grant) {
                            Ok(()) => app.session.toast("Connection grant saved"),
                            Err(e) => app.session.toast(e.to_string()),
                        }
                    }
                    let r = ui.button("Revoke / reject");
                    app.auto.add_response(&format!("agentPairing.{id}.revoke"), &r, "Revoke connection");
                    if r.clicked() {
                        match app.session.pairings.revoke(id) {
                            Ok(()) => app.session.toast("Connection revoked"),
                            Err(e) => app.session.toast(e.to_string()),
                        }
                    }
                });
                ui.small("Folder-scoped connections use engine commands. Synthetic UI input requires the owner connection.");
            });
            ui.ctx().data_mut(|d| d.insert_temp(key, draft));
        }
    });
    app.auto.add_response("agentPairing.connections", &collapse.header_response, "Connections and folders");
}
