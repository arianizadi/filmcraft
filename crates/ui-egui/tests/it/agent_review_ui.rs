use std::sync::mpsc::{Sender, channel};

use egui::{Event, Modifiers, PointerButton, pos2, vec2};
use egui_kittest::Harness;
use filmcraft_engine::{Session, operations::Permission};
use filmcraft_ui_egui::{FilmcraftApp, control::ControlRequest};
use serde_json::{Value, json};

struct Driver {
    h: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    plan: u64,
}
impl Driver {
    fn new() -> Self {
        let mut session = Session::default();
        // Keep native workflow tests independent of a developer's installed Whisper weights.
        // The preparation still exercises bounded decoding and the transcription integration.
        session.transcriber = Some(std::sync::Arc::new(filmcraft_speech::FixedTranscriber {
            transcript: filmcraft_engine::project::Transcript::default(),
            id: "native-review-test".into(),
        }));
        session.execute("file.newSequence", json!({})).unwrap();
        let plan = session
            .execute(
                "agent.plan",
                json!({"label":"Review dialogue","steps":[
                    {"id":"project.setNotes","params":{"text":"Reviewed dialogue cuts"}}
                ]}),
            )
            .unwrap()["plan"]
            .as_u64()
            .unwrap();
        session.operations.permission = Permission::Propose;
        let (tx, rx) = channel();
        let mut app = FilmcraftApp::new(session).with_control(rx);
        app.agent_review.open = true;
        let h = Harness::builder().with_size(vec2(1600.0, 1000.0)).with_max_steps(10_000).build_eframe(move |_| app);
        let mut d = Self { h, tx, plan };
        d.frames(5);
        d
    }
    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.h.ctx.clone();
            let mut input = std::mem::take(self.h.input_mut());
            input.focused = true;
            if let Some(viewport) = input.viewports.get_mut(&egui::ViewportId::ROOT) {
                viewport.focused = Some(true);
            }
            eframe::App::raw_input_hook(self.h.state_mut(), &ctx, &mut input);
            *self.h.input_mut() = input;
            self.h.step();
            assert!(self.h.state().ui_error.is_none(), "{:?}", self.h.state().ui_error);
        }
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        let (request, reply) = ControlRequest::new(method, params);
        self.tx.send(request).unwrap();
        for _ in 0..100 {
            self.frames(1);
            if let Ok(result) = reply.try_recv() {
                return result;
            }
        }
        panic!("No reply to {method}");
    }
    fn native_click(&mut self, id: &str) {
        // Expanded proposal details can exceed the window. Exercise real scrolling before clicks.
        if !matches!(id, "agentReview.close" | "agentReview.collapse") {
            for _ in 0..12 {
                let Some(target) = self.h.state().auto.find(id).map(|e| e.rect) else { break };
                let Some(scroll) = self.h.state().auto.find("agentReview.scroll").map(|e| e.rect) else { break };
                let y = target[1] + target[3] / 2.0;
                if y >= scroll[1] + 8.0 && y <= scroll[1] + scroll[3] - 8.0 {
                    break;
                }
                let delta = (scroll[1] + scroll[3] / 2.0 - y).clamp(-400.0, 400.0);
                self.h.input_mut().events.push(Event::PointerMoved(pos2(scroll[0] + scroll[2] / 2.0, scroll[1] + scroll[3] / 2.0)));
                self.h.input_mut().events.push(Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    source: egui::MouseWheelSource::Wheel,
                    phase: egui::TouchPhase::Move,
                    delta: vec2(0.0, delta),
                    modifiers: Modifiers::NONE,
                });
                self.frames(4);
            }
        }
        let rect = self.h.state().auto.find(id).unwrap_or_else(|| panic!("Missing {id}")).rect;
        let position = pos2(rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0);
        for pressed in [true, false] {
            self.h.input_mut().events.push(Event::PointerMoved(position));
            self.h.input_mut().events.push(Event::PointerButton { pos: position, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
            self.frames(1);
        }
        self.frames(3);
    }
}

#[test]
fn propose_permission_requires_user_approval_and_native_approval_is_one_undo() {
    let mut d = Driver::new();
    let revision = d.h.state().session.revision;
    let undo = d.h.state().session.history.undo.len();
    assert_eq!(d.h.state().session.project.notes, "");
    assert_eq!(d.h.state().auto.find("agentReview.approve").unwrap().enabled, Some(true));
    for (method, params) in [
        ("engine.execute", json!({"command":"agent.apply","params":{"plan":d.plan}})),
        ("ui.click", json!({"id":"agentReview.approve"})),
        ("ui.click", json!({"id":"agentReview.permission.edit"})),
        ("ui.set", json!({"agentReview":{"permission":"edit"},"permission":"edit"})),
        ("engine.execute", json!({"command":"agent.setPermission","params":{"permission":"edit"}})),
    ] {
        let result = d.call(method, params);
        assert_eq!(result["ok"], false, "{method}: {result}");
        assert_eq!(d.h.state().session.operations.permission, Permission::Propose);
    }
    assert_eq!(d.h.state().session.revision, revision);
    d.native_click("agentReview.approve");
    assert_eq!(d.h.state().session.project.notes, "Reviewed dialogue cuts");
    assert_eq!(d.h.state().session.history.undo.len(), undo + 1);
    d.h.state_mut().session.execute("edit.undo", json!({})).unwrap();
    assert_eq!(d.h.state().session.project.notes, "");
}

#[test]
fn stale_proposal_cannot_be_approved_and_discard_preserves_manual_edit() {
    let mut d = Driver::new();
    d.h.state_mut().session.execute("project.setNotes", json!({"text":"My manual edit"})).unwrap();
    d.frames(3);
    assert_eq!(d.h.state().auto.find("agentReview.approve").unwrap().enabled, Some(false));
    let revision = d.h.state().session.revision;
    d.native_click("agentReview.discard");
    assert_eq!(d.h.state().session.revision, revision);
    assert_eq!(d.h.state().session.project.notes, "My manual edit");
    assert_eq!(d.h.state_mut().session.execute("agent.preview", json!({"plan":d.plan})).unwrap()["status"], "discarded");
}

#[test]
fn before_after_preview_is_async_and_never_seeks_or_changes_the_project() {
    let mut d = Driver::new();
    let (before, revision, playhead) = (d.h.state().session.project.clone(), d.h.state().session.revision, d.h.state().session.playhead());
    d.native_click("agentReview.preview");
    let start = std::time::Instant::now();
    while d.h.state().auto.find("agentReview.before").is_none() {
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "Preview did not arrive");
        d.frames(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(d.h.state().auto.find("agentReview.after").is_some());
    assert_eq!(d.h.state().session.project, before);
    assert_eq!(d.h.state().session.revision, revision);
    assert_eq!(d.h.state().session.playhead(), playhead);
    assert_eq!(d.h.state().session.operations.permission, Permission::Propose);
}

#[test]
fn permission_changes_are_desktop_controls() {
    let mut d = Driver::new();
    d.native_click("agentReview.permission.inspect");
    assert_eq!(d.h.state().session.operations.permission, Permission::Inspect);
    d.native_click("agentReview.permission.propose");
    assert_eq!(d.h.state().session.operations.permission, Permission::Propose);
}

#[test]
fn registered_disclosures_expand_real_diff_and_command_details() {
    let mut d = Driver::new();
    d.native_click("agentReview.permission.edit");
    let app = d.h.state_mut();
    app.session.execute("file.openDemoProject", json!({})).unwrap();
    let clip = app.session.active_sequence().unwrap().video_tracks[0].items[0].id.0;
    let plan = app
        .session
        .execute("agent.plan", json!({"label":"Inspect renamed shot","steps":[{"id":"clip.rename","params":{"clip":clip,"name":"Review rename"}}]}))
        .unwrap()["plan"]
        .as_u64()
        .unwrap();
    let (before, revision) = (app.session.project.clone(), app.session.revision);
    d.frames(4);
    d.native_click(&format!("agentReview.plan.{plan}"));
    let listed = d.call("ui.elements", json!({"prefix":"agentReview.diff."}));
    assert_eq!(listed["ok"], true, "{listed}");
    let header = listed["result"].as_array().unwrap().iter().find(|e| e["label"].as_str().is_some_and(|s| s.contains("changed clip"))).unwrap();
    let id = header["id"].as_str().unwrap().to_string();
    assert!(header["widgetId"].is_string(), "the disclosure is a registered interactive widget");
    let details_id = format!("{id}.details");
    assert!(d.h.state().auto.find(&details_id).is_none());
    d.native_click(&id);
    let details = d.call("ui.elements", json!({"prefix":details_id}));
    let body = details["result"].as_array().unwrap().first().expect("expanded body is inspectable");
    let text = body["label"].as_str().unwrap();
    assert!(text.contains("\"before\"") && text.contains("\"after\"") && text.contains("Review rename"), "{text}");
    d.native_click(&id);
    d.frames(20);
    for id in ["agentReview.commands", "agentReview.activity"] {
        assert!(d.h.state().auto.find(id).unwrap().widget_id.is_some());
    }
    d.native_click("agentReview.commands");
    let details = d.h.state().auto.find("agentReview.commands.details").unwrap();
    assert!(details.label.contains("clip.rename") && details.label.contains("Review rename"));
    d.native_click("agentReview.commands");
    d.frames(20);
    d.native_click("agentReview.activity");
    assert!(d.h.state().auto.find("agentReview.activity.session").is_some());
    assert_eq!(d.h.state().session.project, before);
    assert_eq!(d.h.state().session.revision, revision);
    d.native_click("agentReview.permission.propose");
    assert_eq!(d.h.state().session.operations.permission, Permission::Propose);
}

#[test]
fn registered_collapse_expand_and_close_controls_keep_the_project_unchanged() {
    let mut d = Driver::new();
    let (before, revision) = (d.h.state().session.project.clone(), d.h.state().session.revision);
    for id in ["agentReview.collapse", "agentReview.close", "agentReview.scroll"] {
        assert!(d.h.state().auto.find(id).is_some(), "{id}");
    }
    d.native_click("agentReview.collapse");
    assert!(d.h.state().auto.find("agentReview.permission.inspect").is_none());
    assert_eq!(d.h.state().auto.find("agentReview.collapse").unwrap().label, "Expand review");
    d.native_click("agentReview.collapse");
    assert!(d.h.state().auto.find("agentReview.permission.inspect").is_some());
    d.native_click("agentReview.permission.inspect");
    assert_eq!(d.h.state().session.operations.permission, Permission::Inspect, "expanded content remains usable");
    d.native_click("agentReview.permission.propose");
    d.native_click("agentReview.close");
    assert!(!d.h.state().agent_review.open);
    assert!(d.h.state().auto.find("agentReview.window").is_none());
    assert_eq!(d.h.state().session.project, before);
    assert_eq!(d.h.state().session.revision, revision);
}

#[test]
fn native_step_choice_builds_a_new_exact_preview_before_partial_approval() {
    let mut d = Driver::new();
    let id=d.h.state_mut().session.execute("agent.plan",json!({"label":"Partial native review","steps":[{"id":"project.setNotes","params":{"text":"Keep this"}},{"id":"markers.add","params":{"name":"Omit this"}}]})).unwrap()["plan"].as_u64().unwrap();
    d.frames(4);
    d.native_click(&format!("agentReview.plan.{id}"));
    d.native_click("agentReview.steps");
    d.native_click(&format!("agentReview.step.{id}.1"));
    assert_eq!(d.h.state().auto.find("agentReview.approve").unwrap().enabled, Some(false), "deselecting a step cannot approve the original full diff");
    let before = d.h.state().session.project.clone();
    d.native_click("agentReview.subset");
    assert_eq!(d.h.state().session.project, before);
    let list = d.h.state_mut().session.execute("agent.plans", json!({})).unwrap();
    let subset = list["plans"].as_array().unwrap().last().unwrap()["plan"].as_u64().unwrap();
    assert_ne!(subset, id);
    let (exact, _, _) = d.h.state().session.agent_plan_snapshot(subset, false).unwrap();
    assert_eq!(exact.notes, "Keep this");
    assert!(exact.sequence(d.h.state().session.state.active_sequence.unwrap()).unwrap().markers.is_empty());
    assert_eq!(d.h.state().auto.find("agentReview.approve").unwrap().enabled, Some(true));
    d.native_click("agentReview.approve");
    assert_eq!(d.h.state().session.project, exact);
}

#[test]
fn scoped_client_permissions_restore_owner_cap_and_block_synthetic_elevation() {
    use filmcraft_engine::pairing::Grant;
    let mut d = Driver::new();
    let path = filmcraft_engine::temp_dir().join("filmcraft-scoped-ui-review.fcproj").to_string_lossy().into_owned();
    let folder = filmcraft_engine::temp_dir().to_string_lossy().into_owned();
    let mut grant = Grant { project_path: path.clone(), permission: Permission::Propose, media_folders: vec![folder.clone()], output_folders: vec![folder] };
    d.h.state_mut().session.path = Some(path);
    d.h.state_mut()
        .session
        .execute("agent.pairRequest", json!({"clientId":"scoped-test","label":"Synthetic test client","credential":"9".repeat(64),"grant":grant}))
        .unwrap();
    d.h.state().session.pairings.approve("scoped-test", grant.clone()).unwrap();
    d.h.state_mut().session.operations.permission = Permission::Inspect;
    assert_eq!(d.call("ui.inspect", json!({}))["ok"], true, "owner inspection remains read-only");
    let call = |d: &mut Driver, method: &str, params: Value| {
        let (mut request, reply) = ControlRequest::new(method, params);
        request.client = Some("scoped-test".into());
        d.tx.send(request).unwrap();
        for _ in 0..100 {
            d.frames(1);
            if let Ok(result) = reply.try_recv() {
                return result;
            }
        }
        panic!("No scoped reply to {method}")
    };
    let inspect = call(&mut d, "engine.execute", json!({"command":"agent.inspect","params":{}}));
    assert_eq!(inspect["ok"], true, "{inspect}");
    let plan =
        call(&mut d, "engine.execute", json!({"command":"agent.plan","params":{"steps":[{"id":"project.setNotes","params":{"text":"Scoped proposal"}}]}}));
    assert_eq!(plan["ok"], true, "{plan}");
    assert_eq!(d.h.state().session.operations.permission, Permission::Inspect, "a scoped grant never changes the owner cap");
    assert!(d.h.state().session.project.notes.is_empty());
    for permission in [Permission::Propose, Permission::Edit] {
        grant.permission = permission;
        d.h.state().session.pairings.approve("scoped-test", grant.clone()).unwrap();
        for (method, params) in [
            ("ui.screenshot", json!({})),
            ("ui.inspect", json!({})),
            ("ui.elements", json!({})),
            ("ui.click", json!({"id":"agentReview.permission.edit"})),
            ("ui.set", json!({"permission":"edit"})),
            ("ui.key", json!({"key":"Enter"})),
            ("engine.execute", json!({"command":"agent.setPermission","params":{"permission":"edit"}})),
            ("engine.execute", json!({"command":"agent.pairRequest","params":{"clientId":"other"}})),
            ("engine.execute", json!({"command":"agent.revoke","params":{"clientId":"scoped-test"}})),
        ] {
            let result = call(&mut d, method, params);
            assert_eq!(result["ok"], false, "{method}: {result}");
            assert_eq!(d.h.state().session.operations.permission, Permission::Inspect);
        }
    }
    assert!(d.h.state().session.pairings.grant("scoped-test", d.h.state().session.path.as_deref()).is_ok());
}

#[test]
fn native_automatic_footage_flow_prepares_without_live_edits_and_routes_to_review() {
    let mut d = Driver::new();
    // Keep rendering work bounded even when all native UI suites run concurrently.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("original.png");
    image::RgbaImage::from_pixel(8, 8, image::Rgba([100, 80, 60, 255])).save(&path).unwrap();
    let item =
        d.h.state_mut().session.execute("file.import", json!({"paths":[path]})).unwrap()["items"][0].as_u64().map(filmcraft_engine::project::ItemId).unwrap();
    d.h.state_mut().session.state.project_selection = vec![item];
    d.frames(3);
    d.native_click("agentWorkflow");
    d.native_click("agentWorkflow.selection");
    d.native_click("agentWorkflow.mode.2");
    let before = d.h.state().session.project.clone();
    d.native_click("agentWorkflow.prepare");
    let mut prepared = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        d.frames(1);
        let list = d.h.state_mut().session.execute("agent.plans", json!({})).unwrap();
        if let Some(plan) = list["plans"].as_array().unwrap().iter().find(|p| p["label"] == "Automatic selects edit") {
            prepared = plan["plan"].as_u64();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let plan = prepared.unwrap_or_else(|| {
        panic!(
            "background native preparation must finish; message {:?}; jobs {:?}",
            d.h.state().auto.find("agentWorkflow.message"),
            d.h.state().session.job_registry.list()
        )
    });
    assert_eq!(d.h.state().session.project, before);
    let preview = d.h.state_mut().session.execute("agent.preview", json!({"plan":plan})).unwrap();
    assert_eq!(preview["workflow"]["sourcesPreserved"], true);
    assert!(preview["workflow"]["timelineQc"].is_object());
    d.native_click("agentWorkflow");
    assert_eq!(d.h.state().auto.find("agentReview.approve").unwrap().enabled, Some(true));
    let (after, _, _) = d.h.state().session.agent_plan_snapshot(plan, false).unwrap();
    d.native_click("agentReview.approve");
    assert_eq!(d.h.state().session.project, after);
}

#[test]
fn native_preparation_exposes_durable_progress_and_cancel_without_applying_source_imports() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct SlowReads(Arc<AtomicBool>);
    impl filmcraft_engine::Services for SlowReads {
        fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
            for _ in 0..10_000 {
                if filmcraft_media::cancel::cancelled() {
                    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cooperatively cancelled source read"));
                }
                if self.0.load(Ordering::Acquire) {
                    return std::fs::read(path);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "test file read gate was not released"))
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "test rejects file writes"))
        }
    }
    struct ReleaseGate(Arc<AtomicBool>);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let gate = ReleaseGate(Arc::new(AtomicBool::new(false)));
    let mut d = Driver::new();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("original.png");
    image::RgbaImage::from_pixel(8, 8, image::Rgba([100, 80, 60, 255])).save(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let selected = path.to_string_lossy().into_owned();
    d.h.state_mut().hooks.pick_files = Some(Box::new(move |_| vec![selected.clone()]));
    d.h.state_mut().session.services = std::sync::Arc::new(SlowReads(gate.0.clone()));
    d.frames(3);
    d.native_click("agentWorkflow");
    d.native_click("agentWorkflow.choose");
    let before = d.h.state().session.project.clone();
    let revision = d.h.state().session.revision;
    d.native_click("agentWorkflow.prepare");
    assert!(
        d.h.state().auto.find("agentWorkflow.preparation.progress").is_some(),
        "status: {:?}, jobs: {}",
        d.h.state().auto.find("agentWorkflow.message"),
        serde_json::to_value(d.h.state().session.jobs.iter().map(|j| j.to_json()).collect::<Vec<_>>()).unwrap()
    );
    let job = d.h.state().session.jobs.last().unwrap().id;
    assert_eq!(d.h.state().session.job_registry.get(job).unwrap()["inputRevision"], revision);
    d.native_click("agentWorkflow.preparation.cancel");
    for _ in 0..1000 {
        d.frames(1);
        if d.h.state().session.job_registry.get(job).unwrap()["state"] == "cancelled" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(d.h.state().session.job_registry.get(job).unwrap()["state"], "cancelled");
    assert!(!gate.0.load(Ordering::Acquire), "the source read stopped through the job's decoder cancellation context");
    assert_eq!(d.h.state().session.project, before);
    assert_eq!(d.h.state().session.revision, revision);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    drop(d);
}
