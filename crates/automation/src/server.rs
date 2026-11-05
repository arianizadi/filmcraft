//! The MCP server. Tool names are snake_case.

use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use filmcraft_engine::Session;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock as Content};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AutomationError, BridgeClient, png_rgba};

mod workflow;
use workflow::{
    ApplyParams, DescribeParams, Effects, FramesParams, GetOperationParams, GradeAddress, GradeCopyParams, GradeGetParams, GradePatchParams, GradeResetParams,
    InspectParams, MatchMode, MatchParams, OperationParams, PlanIdParams, PlanParams, RenderTarget, ScopesParams,
};

pub enum Backend {
    Headless(Arc<Mutex<Session>>),
    Bridge(Arc<BridgeClient>),
}

/// A panic payload as text.
fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

/// A failed blocking task: a panic is reported as an internal error and the session stays usable.
fn join_error(e: tokio::task::JoinError) -> AutomationError {
    match e.try_into_panic() {
        Ok(p) => AutomationError::Other(format!("internal error: {}", panic_message(p))),
        Err(e) => AutomationError::Other(e.to_string()),
    }
}

#[derive(Clone)]
pub struct FilmcraftMcp {
    backend: Arc<Backend>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Only commands whose id or label contains this text (case-insensitive).
    #[serde(default)]
    pub filter: Option<String>,
    /// Only commands that can run right now.
    #[serde(default)]
    pub enabled_only: Option<bool>,
    /// Command category (the prefix before the first dot).
    pub category: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunParams {
    /// Command id, e.g. `sequence.addEdit`, `timeline.trim`, `effects.apply`, `file.import`.
    pub id: String,
    /// Parameters object (see `params` in `command_list`).
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchStep {
    /// Command id.
    pub id: String,
    /// Parameters object.
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchParams {
    /// Commands to run in order.
    pub steps: Vec<BatchStep>,
    /// Stop at the first failing step (default true).
    #[serde(default)]
    pub stop_on_error: Option<bool>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct RenderParams {
    /// Timeline time in seconds (default: the playhead).
    #[serde(default)]
    pub seconds: Option<f64>,
    /// Longest side of the PNG (default 960).
    #[serde(default)]
    pub max_side: Option<u32>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ShotParams {
    /// Panel to crop to (e.g. `Timeline`, `Program`); omit for the whole window.
    #[serde(default)]
    pub panel: Option<String>,
    /// Longest side of the PNG (default 1600).
    #[serde(default)]
    pub max_side: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClickParams {
    /// Element id from `ui_elements` (e.g. `timeline.track.V1.lock`, `tools.Razor`, `project.item.12`).
    #[serde(default)]
    pub id: Option<String>,
    /// Or screen coordinates in points.
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    /// left | right | middle
    #[serde(default)]
    pub button: Option<String>,
    /// Number of clicks (0–10; defaults to 1). Use 2 for double-click. Larger values are rejected.
    #[serde(default)]
    pub count: Option<u32>,
    /// {"shift":bool,"alt":bool,"command":bool,"ctrl":bool}
    #[serde(default)]
    pub modifiers: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DragParams {
    /// Start: {"id":..} or {"x":..,"y":..} (optionally "fx"/"fy" fractions inside the element).
    pub from: Value,
    /// End point, same forms.
    pub to: Value,
    /// Movement steps (0–1024; defaults to 12; 0 or 1 uses 2). Larger values are rejected.
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub modifiers: Option<Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyParams {
    /// Key with optional modifiers, e.g. `Space`, `Cmd+K`, `Shift+Delete`, `I`, `L`.
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeParams {
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MediaImportParams {
    /// Absolute paths to media files (MP4/MOV, WAV/MP3/FLAC/AIFF/Ogg, PNG/JPEG/…).
    pub paths: Vec<String>,
    /// Bin ID to import into (optional).
    #[serde(default)]
    pub bin: Option<u64>,
    /// Import numbered stills as an image sequence (optional, default false).
    #[serde(default)]
    pub image_sequence: Option<bool>,
}

/// `file.import` params for a `media_import` call.
fn media_import_params(p: MediaImportParams) -> Value {
    let mut params = json!({"paths": p.paths});
    if let Some(bin) = p.bin {
        params["bin"] = json!(bin);
    }
    if let Some(seq) = p.image_sequence {
        params["imageSequence"] = json!(seq);
    }
    params
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ElementsParams {
    /// Only element ids starting with this prefix (e.g. `timeline.clip.`).
    #[serde(default)]
    pub prefix: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ControlParams {
    /// Any control-channel method (e.g. `ui.set`, `ui.scroll`, `ui.timeline.hit`, `ui.playback`).
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

fn frame_contents(mut v: Value) -> CallToolResult {
    fn extract(v: &mut Value, out: &mut Vec<Content>) {
        match v {
            Value::Object(map) => {
                for key in ["png", "pngBase64", "imageBase64", "base64"] {
                    if let Some(Value::String(encoded)) = map.remove(key) {
                        out.push(Content::image(encoded, "image/png"));
                        map.insert("imageContent".into(), json!(out.len()));
                    }
                }
                for value in map.values_mut() {
                    extract(value, out);
                }
            }
            Value::Array(values) => {
                for value in values {
                    extract(value, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    extract(&mut v, &mut out);
    match serde_json::to_string_pretty(&v) {
        Ok(note) => out.push(Content::text(note)),
        Err(e) => return fail(e),
    }
    CallToolResult::success(out)
}

fn ok_json(v: &Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(serde_json::to_string_pretty(v).unwrap_or_default())])
}
fn fail(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![Content::text(e.to_string())])
}
fn png_result(png: &[u8], note: &str) -> CallToolResult {
    CallToolResult::success(vec![Content::image(base64::engine::general_purpose::STANDARD.encode(png), "image/png"), Content::text(note.to_string())])
}
fn wrap(r: Result<Value, AutomationError>) -> Result<CallToolResult, McpError> {
    Ok(match r {
        Ok(v) => ok_json(&v),
        Err(e) => fail(e),
    })
}

/// Parse-error replies queued for the writer before the reader waits for it to catch up.
const MAX_QUEUED_PARSE_ERRORS: usize = 64;

impl FilmcraftMcp {
    pub fn headless(session: Session) -> Self {
        Self { backend: Arc::new(Backend::Headless(Arc::new(Mutex::new(session)))), tool_router: Self::tool_router() }
    }
    pub fn bridge(addr: &str) -> Result<Self, AutomationError> {
        Ok(Self { backend: Arc::new(Backend::Bridge(Arc::new(BridgeClient::new(addr)?))), tool_router: Self::tool_router() })
    }

    pub async fn serve_stdio(self) -> Result<(), AutomationError> {
        self.serve_io(tokio::io::stdin(), tokio::io::stdout()).await
    }

    /// Serve MCP over any line stream. A line that isn't JSON gets a JSON-RPC parse error
    /// (`-32700`, id null) instead of being dropped silently; the rest go to the rmcp service. One
    /// writer task owns `output`, so replies never interleave. Parse errors wait for room in a
    /// small queue before more input is read: a client that sends junk and never reads its output
    /// stalls its own input instead of growing the server's memory.
    pub async fn serve_io(
        self,
        input: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        output: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
    ) -> Result<(), AutomationError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (mut to_service, service_in) = tokio::io::duplex(1 << 20);
        let (service_out, from_service) = tokio::io::duplex(1 << 20);
        let (err_tx, mut err_rx) = tokio::sync::mpsc::channel::<String>(MAX_QUEUED_PARSE_ERRORS);
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(input).lines();
            // `err_tx.closed()`: the writer is gone (output failed), so stop forwarding requests whose replies cannot be delivered.
            loop {
                let line = tokio::select! {
                    l = lines.next_line() => match l { Ok(Some(l)) => l, _ => break },
                    () = err_tx.closed() => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(_) => {
                        if to_service.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let reply = json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}});
                        // the writer is gone (output failed): stop reading
                        if err_tx.send(reply.to_string()).await.is_err() {
                            break;
                        }
                    }
                }
            }
            // EOF: closing the pipe ends the service
            let _ = to_service.shutdown().await;
        });
        let writer = tokio::spawn(async move {
            let mut output = output;
            let mut replies = BufReader::new(from_service).lines();
            loop {
                let line = tokio::select! {
                    l = replies.next_line() => match l { Ok(Some(l)) => l, _ => break },
                    Some(e) = err_rx.recv() => e,
                };
                if output.write_all(format!("{line}\n").as_bytes()).await.is_err() || output.flush().await.is_err() {
                    break;
                }
            }
        });
        let running = self.serve((service_in, service_out)).await.map_err(|e| AutomationError::Other(format!("MCP init: {e}")))?;
        let r = running.waiting().await.map_err(|e| AutomationError::Other(e.to_string()));
        reader.abort();
        let _ = writer.await;
        r.map(|_| ())
    }

    /// Run an engine command on whichever backend.
    pub async fn run(&self, id: &str, mut params: Value) -> Result<Value, AutomationError> {
        if matches!(
            id,
            "agent.plan" | "render.frames" | "grade.get" | "grade.patch" | "grade.copy" | "grade.reset" | "grade.matchBatch" | "scopes.read" | "agent.inspect"
        ) && let Some(map) = params.as_object_mut()
        {
            map.retain(|_, v| !v.is_null());
        }
        match &*self.backend {
            Backend::Headless(s) => {
                let s = s.clone();
                let id = id.to_string();
                tokio::task::spawn_blocking(move || {
                    // A command that panicked earlier poisoned the lock; the session is still usable.
                    let mut g = s.lock().unwrap_or_else(PoisonError::into_inner);
                    filmcraft_engine::operations::authorize(&g, &id, &params)?;
                    g.execute(&id, params).map_err(AutomationError::from)
                })
                .await
                .map_err(join_error)?
            }
            Backend::Bridge(b) => b.execute(id, params).await,
        }
    }

    async fn operation(&self, p: OperationParams) -> Result<Value, AutomationError> {
        let params = p.params.unwrap_or_else(|| json!({}));
        match &*self.backend {
            Backend::Headless(_) => {
                self.run(
                    "agent.execute",
                    json!({"sessionId":p.session_id,"operationId":p.operation_id,
                "idempotencyKey":p.idempotency_key,"command":p.id,"params":params,"baseRevision":p.base_revision}),
                )
                .await
            }
            Backend::Bridge(b) => b.execute_operation(&p.id, params, &p.operation_id, &p.idempotency_key, p.base_revision, Some(&p.session_id)).await,
        }
    }

    /// The project tree plus the active sequence (null when there is none).
    ///
    /// `project.inspect` and `sequence.inspect` are two separate calls (two lock scopes in
    /// headless mode, two round trips to the app in bridge mode), so the active sequence can
    /// change between them if another client, or the app's own UI, switches sequence tabs in
    /// that window (#1477). Pinning `sequence.inspect` to the `activeSequence` id this
    /// `project.inspect` just reported keeps the pair consistent: the returned `sequence`, when
    /// not null, always describes the sequence the returned `project.activeSequence` names.
    async fn document(&self) -> Result<Value, AutomationError> {
        let project = self.run("project.inspect", json!({})).await?;
        // `ItemId` is a u64 (`item_p` parses the `item` param with `u64_p`): read it as one too,
        // not `as_i64`, so an id above `i64::MAX` is still looked up instead of silently going null.
        let active = project.get("activeSequence").and_then(Value::as_u64);
        let sequence = match active {
            Some(id) => self.run("sequence.inspect", json!({"item": id})).await.ok(),
            None => None,
        };
        Ok(json!({"project": project, "sequence": sequence}))
    }

    fn bridge_client(&self) -> Option<Arc<BridgeClient>> {
        match &*self.backend {
            Backend::Bridge(b) => Some(b.clone()),
            Backend::Headless(_) => None,
        }
    }

    async fn ui(&self, method: &str, params: Value) -> Result<CallToolResult, McpError> {
        match self.bridge_client() {
            Some(b) => wrap(b.call(method, params).await),
            None => Ok(fail(
                "this tool drives the live app: run `filmcraft-cli mcp --bridge 127.0.0.1:<port>` with the app started as `filmcraft --control <port>`",
            )),
        }
    }
}

#[tool_router]
impl FilmcraftMcp {
    #[tool(
        title = "List commands",
        description = "List every command (id, label, menu path, shortcut, parameter doc, enabled now). Menus, shortcuts, panels and timeline gestures all map to these ids.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn command_list(&self, Parameters(p): Parameters<ListParams>) -> Result<CallToolResult, McpError> {
        let r = self.run("command.list", json!({})).await.map(|v| {
            let f = p.filter.unwrap_or_default().to_ascii_lowercase();
            let only = p.enabled_only.unwrap_or(false);
            let category = p.category.unwrap_or_default();
            Value::Array(
                v.as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|c| {
                        let id = c["id"].as_str().unwrap_or("").to_ascii_lowercase();
                        let label = c["label"].as_str().unwrap_or("").to_ascii_lowercase();
                        (f.is_empty() || id.contains(&f) || label.contains(&f))
                            && (!only || c["enabled"].as_bool() == Some(true))
                            && (category.is_empty() || id.split('.').next() == Some(category.as_str()))
                    })
                    .collect(),
            )
        });
        wrap(r)
    }

    #[tool(
        title = "Inspect agent session",
        description = "Get session_id, revision and Inspect/Propose/Edit permissions before submitting operations. Retain session_id across retries.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn agent_session(&self) -> Result<CallToolResult, McpError> {
        wrap(self.run("agent.session", json!({})).await)
    }

    #[tool(
        title = "Run retry-safe operation",
        description = "Execute a command exactly once in this app session. Reuse operation_id, idempotency_key, session_id and identical arguments on retry. Returns a receipt with status, affected IDs and before/after revisions. A changed session refuses execution with unknown_commit. Start exports with wait:false and poll jobs.list using the receipt job ID.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn operation_run(&self, Parameters(p): Parameters<OperationParams>) -> Result<CallToolResult, McpError> {
        wrap(self.operation(p).await)
    }

    #[tool(
        title = "Get operation receipt",
        description = "Resolve a lost write reply. unknown_commit means inspect the project before creating another operation; it never proves the write did not land.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_operation(&self, Parameters(p): Parameters<GetOperationParams>) -> Result<CallToolResult, McpError> {
        wrap(self.run("agent.operation", json!({"operationId":p.operation_id,"sessionId":p.session_id})).await)
    }

    #[tool(
        title = "Stage an edit proposal",
        description = "Simulate eligible in-memory commands against one immutable revision. Returns a plan, typed before/after changes, stable created IDs, warnings and step results. Project, undo history and live selection remain unchanged. Params can reference a previous result with {\"$step\":0,\"path\":\"/clip\"}.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn plan_edit(&self, Parameters(p): Parameters<PlanParams>) -> Result<CallToolResult, McpError> {
        let steps: Vec<Value> = p.steps.into_iter().map(|s| json!({"id":s.id,"params":s.params.unwrap_or_else(||json!({}))})).collect();
        wrap(self.run("agent.plan", json!({"label":p.label,"baseRevision":p.base_revision,"steps":steps})).await)
    }

    #[tool(
        title = "Inspect staged proposal",
        description = "Inspect a plan's affected clips, tracks, grades and captions, warnings, results and stale state.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn preview_plan(&self, Parameters(p): Parameters<PlanIdParams>) -> Result<CallToolResult, McpError> {
        wrap(self.run("agent.preview", json!({"plan":p.plan})).await)
    }

    #[tool(
        title = "Apply approved proposal",
        description = "Apply an approved plan against its exact original project/revision as one undo step. Supply identities from agent_session and reuse them unchanged on retry. Stale plans refuse to apply.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn apply_plan(&self, Parameters(p): Parameters<ApplyParams>) -> Result<CallToolResult, McpError> {
        wrap(
            self.operation(OperationParams {
                session_id: p.session_id,
                operation_id: p.operation_id,
                idempotency_key: p.idempotency_key,
                id: "agent.apply".into(),
                params: Some(json!({"plan":p.plan})),
                base_revision: None,
            })
            .await,
        )
    }

    #[tool(
        title = "Discard staged proposal",
        description = "Release an unapplied plan without editing the timeline.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn discard_plan(&self, Parameters(p): Parameters<PlanIdParams>) -> Result<CallToolResult, McpError> {
        wrap(self.run("agent.discard", json!({"plan":p.plan})).await)
    }

    #[tool(
        title = "Inspect timeline range",
        description = "Inspect one sequence, clip, half-open time range and/or selected fields. Returns revision and typed effect data. Positions are exact ticks.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inspect(&self, Parameters(p): Parameters<InspectParams>) -> Result<CallToolResult, McpError> {
        wrap(self.run("agent.inspect", json!({"sequence":p.sequence,"clip":p.clip,"start":p.start,"end":p.end,"fields":p.fields})).await)
    }

    #[tool(
        title = "Render exact snapshot frames",
        description = "Render source media, an isolated clip, a sequence or a staged plan without seeking the UI. Returns PNGs with actual target, frame, timecode, revision and color transform; optionally before/after pairs or a contact sheet.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn render_frames(&self, Parameters(p): Parameters<FramesParams>) -> Result<CallToolResult, McpError> {
        let target = p.target.map(|v| match v {
            RenderTarget::Sequence => "sequence",
            RenderTarget::Clip => "clip",
            RenderTarget::Source => "source",
        });
        let effects = p.effects.map(|v| match v {
            Effects::Pre => "pre",
            Effects::Post => "post",
        });
        let r = self.run("render.frames", json!({"target":target,"sequence":p.sequence,"clip":p.clip,"item":p.item,"frames":p.frames,
            "times":p.times,"maxSide":p.max_side,"effects":effects,"captions":p.captions,"plan":p.plan,"comparison":p.comparison,"layout":if p.contact_sheet==Some(true){"contactSheet"}else{"stills"}})).await;
        Ok(match r {
            Ok(v) => frame_contents(v),
            Err(e) => fail(e),
        })
    }

    #[tool(
        title = "Read typed grades",
        description = "Read all Lumetri instances on an explicit clip, including values, wheel vectors, curve/LUT references, masks and full keyframes. Retain the revision and effectInstance for patches.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn grade_get(&self, Parameters(p): Parameters<GradeGetParams>) -> Result<CallToolResult, McpError> {
        wrap(self.run("grade.get", json!({"clip":p.clip,"sequence":p.sequence})).await)
    }

    #[tool(
        title = "Patch a grade",
        description = "Change only supplied controls on an explicitly addressed Lumetri instance. Other controls, masks and animation are preserved. expected_revision guards ordinal addresses; replacing animation requires replace_keyframes:true. Prefer this as a plan step.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn grade_patch(&self, Parameters(p): Parameters<GradePatchParams>) -> Result<CallToolResult, McpError> {
        wrap(
            self.run(
                "grade.patch",
                json!({"clip":p.clip,"sequence":p.sequence,"effectInstance":p.effect_instance,
            "expectedRevision":p.expected_revision,"params":p.params,"masks":p.masks,"enabled":p.enabled,"replaceKeyframes":p.replace_keyframes}),
            )
            .await,
        )
    }

    #[tool(
        title = "Copy a grade",
        description = "Copy a typed Lumetri grade to explicit target instances atomically with revision and animation guards.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn grade_copy(&self, Parameters(p): Parameters<GradeCopyParams>) -> Result<CallToolResult, McpError> {
        let address = |a: GradeAddress| {
            let mut v = json!({"clip":a.clip,"effectInstance":a.effect_instance});
            if let Some(seq) = a.sequence {
                v["sequence"] = json!(seq);
            }
            v
        };
        let targets: Vec<Value> = p.targets.into_iter().map(address).collect();
        wrap(
            self.run(
                "grade.copy",
                json!({"source":address(p.source),"targets":targets,"expectedRevision":p.expected_revision,"replaceKeyframes":p.replace_keyframes}),
            )
            .await,
        )
    }

    #[tool(
        title = "Reset grade controls",
        description = "Reset named controls or the complete addressed grade. Animation replacement requires explicit consent and expected_revision.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn grade_reset(&self, Parameters(p): Parameters<GradeResetParams>) -> Result<CallToolResult, McpError> {
        wrap(
            self.run(
                "grade.reset",
                json!({"clip":p.clip,"sequence":p.sequence,"effectInstance":p.effect_instance,
            "expectedRevision":p.expected_revision,"params":p.params,"replaceKeyframes":p.replace_keyframes}),
            )
            .await,
        )
    }

    #[tool(
        title = "Describe one command",
        description = "Get one command's category, enablement/disabled reason, units, staging eligibility and its typed MCP parameter schema when available. Legacy commands retain their parameter documentation.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn command_describe(&self, Parameters(p): Parameters<DescribeParams>) -> Result<CallToolResult, McpError> {
        let catalog = match self.run("command.list", json!({})).await {
            Ok(v) => v,
            Err(e) => return Ok(fail(e)),
        };
        let Some(mut command) = catalog.as_array().and_then(|a| a.iter().find(|v| v["id"] == p.id)).cloned() else { return Ok(fail("unknown command")) };
        command["eligibleForPlan"] = json!(filmcraft_engine::agent_plans::eligible_command(&p.id));
        let tool = match p.id.as_str() {
            "agent.plan" => Some("plan_edit"),
            "agent.apply" => Some("apply_plan"),
            "agent.preview" => Some("preview_plan"),
            "agent.inspect" => Some("inspect"),
            "render.frames" => Some("render_frames"),
            "grade.get" => Some("grade_get"),
            "grade.patch" => Some("grade_patch"),
            "grade.copy" => Some("grade_copy"),
            "grade.reset" => Some("grade_reset"),
            "grade.matchBatch" => Some("grade_match_batch"),
            "scopes.read" => Some("scopes_read"),
            _ => None,
        };
        if let Some(tool) = tool
            && let Some(def) = self.tool_router.get(tool)
        {
            command["schemaTool"] = json!(tool);
            command["inputSchema"] = Value::Object((*def.input_schema).clone());
            command["schemaStatus"] = json!("typed_mcp_tool");
        } else {
            command["schemaStatus"] = json!("legacy_parameter_documentation");
        }
        Ok(ok_json(&command))
    }

    #[tool(
        title = "Match shots to hero",
        description = "Match explicit clips to a hero clip or imported reference item with multiple frame samples. Supports full, exposure and white_balance passes, confidence skipping, animation protection and per-clip verified deltas. Use this command in a plan for review before approval.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn grade_match_batch(&self, Parameters(p): Parameters<MatchParams>) -> Result<CallToolResult, McpError> {
        let clean = |mut v: Value| {
            if let Some(o) = v.as_object_mut() {
                o.retain(|_, v| !v.is_null());
            }
            v
        };
        let reference = clean(json!({"clip":p.reference.clip,"sequence":p.reference.sequence,"item":p.reference.item,"frames":p.reference.frames}));
        let targets: Vec<Value> =
            p.targets.into_iter().map(|a| clean(json!({"clip":a.clip,"sequence":a.sequence,"effectInstance":a.effect_instance,"frames":a.frames}))).collect();
        let mode = p.mode.map(|m| match m {
            MatchMode::Full => "full",
            MatchMode::Exposure => "exposure",
            MatchMode::WhiteBalance => "white_balance",
        });
        wrap(
            self.run(
                "grade.matchBatch",
                json!({"reference":reference,"targets":targets,"expectedRevision":p.expected_revision,"mode":mode,
            "samplesPerClip":p.samples_per_clip,"minConfidence":p.min_confidence,"skinProtection":p.skin_protection,"replaceKeyframes":p.replace_keyframes}),
            )
            .await,
        )
    }

    #[tool(
        title = "Measure subject scopes",
        description = "Measure source media, an isolated clip or composite before/after effects at exact frames, optionally within a normalized ROI and against a reference. Returns declared color-space values, clipping metrics and reference deltas. Can measure staged plans without applying them.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn scopes_read(&self, Parameters(p): Parameters<ScopesParams>) -> Result<CallToolResult, McpError> {
        let target = p.target.map(|v| match v {
            RenderTarget::Sequence => "sequence",
            RenderTarget::Clip => "clip",
            RenderTarget::Source => "source",
        });
        let effects = p.effects.map(|v| match v {
            Effects::Pre => "pre",
            Effects::Post => "post",
        });
        let roi = p.roi.map(|r| json!({"x":r.x,"y":r.y,"width":r.width,"height":r.height}));
        let r = self
            .run(
                "scopes.read",
                json!({"target":target,"sequence":p.sequence,"clip":p.clip,"item":p.item,"frames":p.frames,"times":p.times,
            "effects":effects,"roi":roi,"reference":p.reference,"plan":p.plan,"colorSpace":p.color_space,"images":p.images,"bins":p.bins,"scopes":p.scopes}),
            )
            .await;
        Ok(match r {
            Ok(v) => frame_contents(v),
            Err(e) => fail(e),
        })
    }

    #[tool(
        title = "Run a command",
        description = "Run a command by id with JSON params, e.g. {\"id\":\"timeline.razor\",\"params\":{\"seconds\":3.5}} or {\"id\":\"effects.apply\",\"params\":{\"effect\":\"Gaussian Blur\"}}. Undoable edits land in History.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn command_run(&self, Parameters(p): Parameters<RunParams>) -> Result<CallToolResult, McpError> {
        wrap(self.run(&p.id, p.params.unwrap_or(json!({}))).await)
    }

    #[tool(
        title = "Run several commands",
        description = "Run several commands in order: {steps: [{id, params?}], stop_on_error?: true}. Returns {completed, failed, results: [{ok, result | error}]}; each edit is its own undo step.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn command_batch(&self, Parameters(p): Parameters<BatchParams>) -> Result<CallToolResult, McpError> {
        if p.steps.len() > 256 {
            return Ok(fail("command_batch accepts at most 256 steps; use separate reviewed plans"));
        }
        let stop = p.stop_on_error.unwrap_or(true);
        let (mut completed, mut failed, mut results) = (0u32, 0u32, Vec::new());
        for st in p.steps {
            match self.run(&st.id, st.params.unwrap_or(json!({}))).await {
                Ok(v) => {
                    completed += 1;
                    results.push(json!({"ok": true, "result": v}));
                }
                Err(e) => {
                    failed += 1;
                    results.push(json!({"ok": false, "error": e.to_string()}));
                    if stop {
                        break;
                    }
                }
            }
        }
        let v = json!({"completed": completed, "failed": failed, "results": results});
        Ok(if failed > 0 { CallToolResult::error(vec![Content::text(v.to_string())]) } else { ok_json(&v) })
    }

    #[tool(
        title = "Inspect the document",
        description = "The project as JSON: the tree (as project_inspect) plus the active sequence (as sequence_inspect; null when there is none).",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn doc_inspect(&self) -> Result<CallToolResult, McpError> {
        wrap(self.document().await)
    }

    #[tool(
        title = "Render a preview",
        description = "The active sequence's frame at `seconds` (default: the playhead) as PNG, same deterministic snapshot path as render_frame.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn render_preview(&self, p: Parameters<RenderParams>) -> Result<CallToolResult, McpError> {
        self.render_frame(p).await
    }

    #[tool(
        title = "Inspect the project tree",
        description = "Project tree (bins and items with ids, types, durations), active sequence, source clip.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn project_inspect(&self) -> Result<CallToolResult, McpError> {
        wrap(self.run("project.inspect", json!({})).await)
    }

    #[tool(
        title = "Inspect the active sequence",
        description = "The active sequence as JSON: settings, tracks, clips (ids, start/duration in ticks and frames, effects), transitions, markers, in/out, playhead, selection. 254016000000 ticks = 1 second.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn sequence_inspect(&self) -> Result<CallToolResult, McpError> {
        wrap(self.run("sequence.inspect", json!({})).await)
    }

    #[tool(
        title = "Import media",
        description = "Import media files by absolute path (MP4/MOV, WAV/MP3/FLAC/AIFF/Ogg, PNG/JPEG/…).",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn media_import(&self, Parameters(p): Parameters<MediaImportParams>) -> Result<CallToolResult, McpError> {
        wrap(self.run("file.import", media_import_params(p)).await)
    }

    #[tool(
        title = "Render a frame",
        description = "Render the active sequence's frame (at `seconds` or the playhead) and return it as PNG.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn render_frame(&self, Parameters(p): Parameters<RenderParams>) -> Result<CallToolResult, McpError> {
        let mut params = json!({"maxSide":p.max_side.unwrap_or(960)});
        if let Some(seconds) = p.seconds {
            if !seconds.is_finite() || seconds < 0.0 || seconds > filmcraft_time::Tick::MAX.seconds() {
                return Ok(fail("seconds must be finite, nonnegative and within the timeline range"));
            }
            params["times"] = json!([filmcraft_time::Tick::from_seconds_f64(seconds).0]);
        }
        Ok(match self.run("render.frames", params).await {
            Ok(v) => frame_contents(v),
            Err(e) => fail(e),
        })
    }

    #[tool(
        title = "Live app: UI state",
        description = "Live app: UI state (tool, workspace, panels, timeline zoom, playback, selection, fps).",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ui_inspect(&self) -> Result<CallToolResult, McpError> {
        self.ui("ui.inspect", json!({})).await
    }

    #[tool(
        title = "Live app: on-screen elements",
        description = "Live app: every on-screen interactive element with id, label and rect (points). Use ids with ui_click/ui_drag.",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ui_elements(&self, Parameters(p): Parameters<ElementsParams>) -> Result<CallToolResult, McpError> {
        self.ui("ui.elements", json!({"prefix": p.prefix.unwrap_or_default()})).await
    }

    /// Forward a click to the live editor; an omitted count is sent as null and uses its default.
    #[tool(
        title = "Live app: click",
        description = "Live app: click an element by id or at x,y (points). Supports right/middle button, double-click (count=2) and modifiers.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn ui_click(&self, Parameters(p): Parameters<ClickParams>) -> Result<CallToolResult, McpError> {
        let mut v = json!({"button": p.button, "count": p.count, "modifiers": p.modifiers});
        if let Some(id) = p.id {
            v["id"] = json!(id);
        } else {
            v["x"] = json!(p.x);
            v["y"] = json!(p.y);
        }
        self.ui("ui.click", v).await
    }

    /// Forward a drag to the live editor, which applies the step default and validates its bounds.
    #[tool(
        title = "Live app: drag",
        description = "Live app: press-drag-release from one point/element to another (move clips, trim edges, scrub, resize panels, drop project items onto tracks).",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn ui_drag(&self, Parameters(p): Parameters<DragParams>) -> Result<CallToolResult, McpError> {
        self.ui("ui.drag", json!({"from": p.from, "to": p.to, "steps": p.steps, "modifiers": p.modifiers})).await
    }

    #[tool(
        title = "Live app: press a key",
        description = "Live app: press a key or shortcut (e.g. `Space`, `L`, `Cmd+K`, `Shift+Delete`, `Cmd+Z`).",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn ui_key(&self, Parameters(p): Parameters<KeyParams>) -> Result<CallToolResult, McpError> {
        self.ui("ui.key", json!({"key": p.key})).await
    }

    #[tool(
        title = "Live app: type text",
        description = "Live app: type text into the focused field.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn ui_type(&self, Parameters(p): Parameters<TypeParams>) -> Result<CallToolResult, McpError> {
        self.ui("ui.type", json!({"text": p.text})).await
    }

    #[tool(
        title = "Live app: screenshot",
        description = "Live app: screenshot of the window or one panel (PNG).",
        annotations(read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ui_screenshot(&self, Parameters(p): Parameters<ShotParams>) -> Result<CallToolResult, McpError> {
        match self.bridge_client() {
            Some(b) => self.screenshot(&b, p.panel, p.max_side.unwrap_or(1600)).await,
            None => Ok(fail("ui_screenshot needs bridge mode")),
        }
    }

    #[tool(
        title = "Live app: control-channel call",
        description = "Live app: call any control-channel method directly (ui.set, ui.scroll, ui.timeline.hit, ui.timeline.locate, ui.playback, ui.panel.show, ui.menu.list, …).",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn ui_control(&self, Parameters(p): Parameters<ControlParams>) -> Result<CallToolResult, McpError> {
        self.ui(&p.method, p.params.unwrap_or(json!({}))).await
    }
}

impl FilmcraftMcp {
    async fn screenshot(&self, b: &BridgeClient, panel: Option<String>, max: u32) -> Result<CallToolResult, McpError> {
        let result = match b.call("ui.screenshot", json!({"panel":panel})).await {
            Ok(v) => v,
            Err(e) => return Ok(fail(format!("screenshot: {e}"))),
        };
        let bytes = match result
            .get("pngBase64")
            .and_then(Value::as_str)
            .ok_or_else(|| AutomationError::Bridge("app did not return inline screenshot bytes".into()))
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).map_err(|e| AutomationError::Bridge(e.to_string())))
        {
            Ok(v) => v,
            Err(e) => return Ok(fail(format!("screenshot: {e}"))),
        };
        let bytes = match image::load_from_memory(&bytes) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                png_rgba(w, h, rgba.into_raw(), max).unwrap_or(bytes)
            }
            Err(_) => bytes,
        };
        Ok(png_result(&bytes, "screenshot of the live FilmCraft window"))
    }
}

impl FilmcraftMcp {
    /// The first argument `tool` doesn't take, as a message naming the ones it does.
    fn unknown_arg(&self, tool: &str, args: Option<&rmcp::model::JsonObject>) -> Option<String> {
        let def = self.tool_router.get(tool)?;
        let props = def.input_schema.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        let bad = args?.keys().find(|k| !props.contains_key(*k))?;
        let accepted: Vec<&str> = props.keys().map(String::as_str).collect();
        let accepted = if accepted.is_empty() { "no arguments".to_string() } else { accepted.join(", ") };
        Some(format!("unknown argument \"{bad}\" for {tool}; expected: {accepted}"))
    }
}

const INSTRUCTIONS: &str = "FilmCraft: inspect -> propose -> preview -> apply -> verify. Start with agent_session and inspect (doc_inspect also returns the project tree). Build eligible in-memory edits with plan_edit, inspect preview_plan, then render_frames with plan/comparison and scopes.read. Apply only the user-approved plan using apply_plan with stable operation_id/idempotency_key/session_id; verify the receipt, affected IDs, scopes and frames. A stale plan must be rebuilt. Resolve lost replies with get_operation; unknown_commit never means safe to repeat a write under another ID. command_run and UI tools remain advanced direct editing tools and require Edit permission. File writes and jobs stay outside edit plans. Time is exact ticks (254016000000/s) or frame indexes.";

/// Resources: the project (as `doc_inspect`) and the command catalog (as `command_list`).
const DOCUMENT_URI: &str = "filmcraft://document";
const COMMANDS_URI: &str = "filmcraft://commands";

/// MCP 2026-07-28 clients (current Claude Code) require `ttlMs` and `cacheScope` on list and read
/// results; rmcp fills `resultType` and leaves the cache hints to the server, as the generated
/// `tools/list` does. The resource list never changes; the document does, so reads aren't cached.
fn cache_hints(context: &rmcp::service::RequestContext<rmcp::RoleServer>, ttl_ms: u64) -> Option<(u64, rmcp::model::CacheScope)> {
    context.protocol_version().is_some_and(|v| v >= rmcp::model::ProtocolVersion::V_2026_07_28).then_some((ttl_ms, rmcp::model::CacheScope::Private))
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FilmcraftMcp {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(rmcp::model::ServerCapabilities::builder().enable_tools().enable_resources().build())
            .with_server_info(rmcp::model::Implementation::new("filmcraft", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS.to_string())
    }

    /// Strict arguments and long exports in front of the tool router.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        if let Some(m) = self.unknown_arg(&request.name, request.arguments.as_ref()) {
            return Err(McpError::invalid_params(m, None));
        }
        // A blocking export reports progress and can be cancelled (docs/agents.md § Long exports).
        // Bridge mode too: the app runs it as a job, so it stays responsive (#91, #92).
        if request.name == "command_run"
            && let Some(a) = &request.arguments
            && let Some(id) = a.get("id").and_then(Value::as_str)
        {
            let params = a.get("params").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
            if crate::long_job::is_long(id, &params) {
                return Ok(self.run_long(id, params, &context).await.into());
            }
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, McpError> {
        use rmcp::model::Resource;
        let mut r = rmcp::model::ListResourcesResult::with_all_items(vec![
            Resource::new(DOCUMENT_URI, "document")
                .with_title("Project")
                .with_description("The project tree and the active sequence (same as doc_inspect).")
                .with_mime_type("application/json"),
            Resource::new(COMMANDS_URI, "commands")
                .with_title("Command catalog")
                .with_description("Every command with id, label, menu path, shortcut, params and enabled state (same as command_list).")
                .with_mime_type("application/json"),
        ]);
        if let Some((ttl, scope)) = cache_hints(&context, 600_000) {
            r = r.with_ttl_ms(ttl).with_cache_scope(scope);
        }
        Ok(r)
    }

    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, McpError> {
        let v = match request.uri.as_str() {
            DOCUMENT_URI => self.document().await,
            COMMANDS_URI => self.run("command.list", json!({})).await,
            other => return Err(McpError::resource_not_found(format!("resource not found: {other}"), None)),
        }
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let text = serde_json::to_string_pretty(&v).map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let mut r = rmcp::model::ReadResourceResult::new(vec![rmcp::model::ResourceContents::text(text, request.uri).with_mime_type("application/json")]);
        if let Some((ttl, scope)) = cache_hints(&context, 0) {
            r = r.with_ttl_ms(ttl).with_cache_scope(scope);
        }
        Ok(r.into())
    }
}

#[cfg(test)]
mod tests;
