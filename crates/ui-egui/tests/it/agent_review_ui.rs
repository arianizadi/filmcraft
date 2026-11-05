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
