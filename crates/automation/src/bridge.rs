//! Client for the desktop app's JSON-lines control protocol. A connection per request in flight,
//! reused once its reply is in; reconnect on failure.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::AutomationError;

/// How often a blocking export's app job is polled.
const JOB_POLL: Duration = Duration::from_millis(250);
/// Idle connections kept for reuse; the app serves at most 16 at once.
const MAX_IDLE: usize = 4;

type Conn = (BufReader<tokio::net::tcp::OwnedReadHalf>, tokio::net::tcp::OwnedWriteHalf);

pub struct BridgeClient {
    addr: String,
    /// Inherited from the same owner-controlled environment as the desktop app.
    token: Option<String>,
    /// Connections with no request in flight. A request takes one (or opens one) for its whole
    /// round trip and puts it back only once the reply is read, so a slow or abandoned request
    /// never holds up the others or leaves its late reply for the next one to read (#1538).
    idle: Mutex<Vec<Conn>>,
    next_id: AtomicU64,
    client_id: String,
}

impl BridgeClient {
    /// `addr` such as `127.0.0.1:9876` (loopback only). Reads `FILMCRAFT_CONTROL_TOKEN`
    /// from the environment; the desktop app requires this credential on every request.
    pub fn new(addr: impl Into<String>) -> Result<Self, AutomationError> {
        let addr = addr.into();
        let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(&addr);
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
            return Err(AutomationError::BadRequest(format!("bridge address must be loopback, got `{addr}`")));
        }
        Ok(Self {
            addr,
            token: std::env::var("FILMCRAFT_CONTROL_TOKEN").ok(),
            idle: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            client_id: format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()),
        })
    }

    /// Run engine command `id` in the app. A blocking export (`file.exportMedia` with
    /// `wait: true`, see [`crate::long_job::LONG_COMMANDS`]) is started as an app job and polled
    /// here until it finishes, so the app keeps repainting (its status bar shows the progress) and
    /// answering other requests, and no request waits on the control server's 60 s reply limit
    /// (#91, #92). The reply is the same as a blocking export's: `{job, path, result}`, or the
    /// export's error.
    pub async fn execute(&self, id: &str, mut params: Value) -> Result<Value, AutomationError> {
        if !crate::long_job::is_long(id, &params) {
            return self.call("engine.execute", json!({"command": id, "params": params})).await;
        }
        if let Some(p) = params.as_object_mut() {
            p.insert("wait".into(), json!(false));
        }
        let mut start = self.call("engine.execute", json!({"command": id, "params": params})).await?;
        let Some(job) = start.get("job").and_then(Value::as_u64) else { return Ok(start) };
        let (result, offline) = loop {
            tokio::time::sleep(JOB_POLL).await;
            let jobs = self.call("engine.execute", json!({"command": "jobs.list", "params": {}})).await?;
            let Some(j) = jobs.as_array().and_then(|a| a.iter().find(|j| j["id"].as_u64() == Some(job))).cloned() else {
                break (Value::Null, None);
            };
            if j["finished"].as_bool() == Some(true) {
                break (j["result"].clone(), j.get("offlineMedia").cloned());
            }
        };
        if let Some(e) = result.get("error").and_then(Value::as_str) {
            return Err(AutomationError::App(format!("export failed: {e}")));
        }
        if let Some(o) = start.as_object_mut() {
            o.insert("result".into(), result);
            if let Some(off) = offline {
                o.insert("offlineMedia".into(), off);
            }
        }
        Ok(start)
    }

    /// Execute a write with a caller-owned operation identity and retain its complete receipt.
    pub async fn execute_operation(
        &self,
        command: &str,
        params: Value,
        operation: &str,
        key: &str,
        base: Option<u64>,
        session_id: Option<&str>,
    ) -> Result<Value, AutomationError> {
        let session = if let Some(id) = session_id {
            json!({"sessionId":id})
        } else {
            self.call_raw("engine.execute", json!({"command":"agent.session", "params":{}}), true).await?
        };
        let session_id = session
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| AutomationError::Bridge("app does not support operation receipts; update FilmCraft before sending writes".into()))?;
        self.call_raw(
            "engine.execute",
            json!({"command":"agent.execute", "params":{
                "operationId":operation, "idempotencyKey":key, "sessionId":session_id,
                "baseRevision":base, "command":command, "params":params
            }}),
            true,
        )
        .await
        .map_err(|error| {
            AutomationError::Bridge(format!("{error}; operationId={operation}, sessionId={session_id}; query get_operation before submitting another write"))
        })
    }

    /// Call a control method. Engine writes use operation receipts; UI gestures are never retried.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, AutomationError> {
        if method == "engine.execute"
            && let Some(command) = params.get("command").and_then(Value::as_str)
            && command != "agent.execute"
            && filmcraft_engine::find_command(command).is_some()
            && filmcraft_engine::operations::is_write(command, params.get("params").unwrap_or(&Value::Null))
        {
            let operation = format!("bridge-{}-{}", self.client_id, self.next_id.fetch_add(1, Ordering::Relaxed));
            let receipt =
                self.execute_operation(command, params.get("params").cloned().unwrap_or_else(|| json!({})), &operation, &operation, None, None).await?;
            return operation_result(receipt);
        }
        let retry = matches!(
            method,
            "engine.commands" | "ui.inspect" | "ui.elements" | "ui.menu.list" | "ui.docking" | "ui.timeline.hit" | "ui.timeline.locate" | "perf.stats"
        ) || (method == "engine.execute"
            && params
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|id| id == "agent.execute" || !filmcraft_engine::operations::is_write(id, params.get("params").unwrap_or(&Value::Null))));
        self.call_raw(method, params, retry).await
    }

    async fn call_raw(&self, method: &str, params: Value, retry: bool) -> Result<Value, AutomationError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = format!("{}\n", json!({"id": id, "token": self.token, "method": method, "params": params}));
        for attempt in 0..if retry { 2 } else { 1 } {
            let idle = if attempt == 0 { self.idle.lock().await.pop() } else { None };
            let mut conn = match idle {
                Some(c) => c,
                None => match self.connect().await {
                    Ok(c) => c,
                    Err(e) if attempt > 0 => return Err(unknown_commit(method, &params, e)),
                    Err(e) => return Err(e),
                },
            };
            let res: Result<Value, AutomationError> = async {
                conn.1.write_all(line.as_bytes()).await.map_err(|e| AutomationError::Bridge(e.to_string()))?;
                let mut buf = String::new();
                tokio::time::timeout(Duration::from_secs(90), conn.0.read_line(&mut buf))
                    .await
                    .map_err(|_| AutomationError::Bridge("timeout".into()))?
                    .map_err(|e| AutomationError::Bridge(e.to_string()))?;
                let reply: Value = serde_json::from_str(&buf).map_err(|e| AutomationError::Bridge(format!("bad reply: {e}")))?;
                if reply.get("id").and_then(Value::as_u64) != Some(id) {
                    return Err(AutomationError::Bridge("reply request ID mismatch".into()));
                }
                Ok(reply)
            }
            .await;
            match res {
                Ok(v) => {
                    let mut idle = self.idle.lock().await;
                    if idle.len() < MAX_IDLE {
                        idle.push(conn);
                    }
                    drop(idle);
                    return if v.get("ok").and_then(Value::as_bool) == Some(true) {
                        Ok(v.get("result").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(AutomationError::App(v.get("error").and_then(Value::as_str).unwrap_or("error").to_string()))
                    };
                }
                Err(_) if attempt == 0 && retry => {}
                Err(e) => return Err(unknown_commit(method, &params, e)),
            }
        }
        Err(AutomationError::Bridge("request did not complete".into()))
    }

    async fn connect(&self) -> Result<Conn, AutomationError> {
        let s = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&self.addr))
            .await
            .map_err(|_| AutomationError::Bridge(format!("timed out connecting to {}", self.addr)))?
            .map_err(|e| AutomationError::Bridge(format!(
                "cannot connect to {} ({e}); start the app with `filmcraft --control <port>` and set FILMCRAFT_CONTROL_TOKEN to the same 64-hex-character value on both sides (see docs/control-protocol.md)",
                self.addr
            )))?;
        let (r, w) = s.into_split();
        Ok((BufReader::new(r), w))
    }
}

/// Keep legacy command result shapes while the explicit operation tool returns the full receipt.
pub fn operation_result(receipt: Value) -> Result<Value, AutomationError> {
    if receipt.get("status").and_then(Value::as_str) == Some("committed") {
        Ok(receipt.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(AutomationError::App(format!(
            "{}: {} (operation {})",
            receipt["status"],
            receipt.get("error").and_then(Value::as_str).unwrap_or("inspect get_operation before retrying"),
            receipt["operationId"]
        )))
    }
}

fn unknown_commit(method: &str, params: &Value, error: AutomationError) -> AutomationError {
    let command = params.get("command").and_then(Value::as_str);
    if method == "engine.execute" && command.is_some_and(|id| !filmcraft_engine::operations::is_write(id, params.get("params").unwrap_or(&Value::Null))) {
        return error;
    }
    AutomationError::Bridge(format!(
        "unknown_commit: {error}; operationId={}; check get_operation and inspect before submitting another write",
        params.pointer("/params/operationId").and_then(Value::as_str).unwrap_or("untracked UI gesture")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    /// A stand-in for the app's control server: the export starts job 7, which `jobs.list` reports
    /// running twice and then finished with `result`. Every request is recorded.
    async fn fake_app(result: Value) -> (String, Arc<Mutex<Vec<Value>>>) {
        fake_app_with(result, None).await
    }

    /// [`fake_app`] whose finished job also lists `offline` as its `offlineMedia`.
    async fn fake_app_with(result: Value, offline: Option<Value>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            let mut polls = 0;
            while let Ok(Some(line)) = lines.next_line().await {
                let mut req: Value = serde_json::from_str(&line).unwrap();
                if req["params"]["command"] == "agent.session" {
                    let line = format!("{}\n", json!({"id":req["id"],"ok":true,"result":{"sessionId":"test-app"}}));
                    w.write_all(line.as_bytes()).await.unwrap();
                    continue;
                }
                let envelope = req["params"]["command"] == "agent.execute";
                if envelope {
                    req["params"] = req["params"]["params"].clone();
                }
                log.lock().await.push(req.clone());
                let reply = match req["params"]["command"].as_str() {
                    Some("file.exportMedia") => json!({"job": 7, "path": "/tmp/out.mov"}),
                    Some("jobs.list") => {
                        polls += 1;
                        let finished = polls >= 3;
                        let mut j = json!({"id": 7, "finished": finished, "result": if finished { result.clone() } else { Value::Null }});
                        if let Some(o) = offline.clone().filter(|_| finished) {
                            j["offlineMedia"] = o;
                        }
                        json!([j])
                    }
                    _ => json!(null),
                };
                let reply = if envelope { json!({"status":"committed","result":reply}) } else { reply };
                let line = format!("{}\n", json!({"id": req["id"], "ok": true, "result": reply}));
                w.write_all(line.as_bytes()).await.unwrap();
            }
        });
        (addr, seen)
    }

    async fn lossy_app(restart: bool) -> (String, Arc<Mutex<filmcraft_engine::Session>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let session = Arc::new(Mutex::new(filmcraft_engine::Session::default()));
        let app = session.clone();
        tokio::spawn(async move {
            let mut lost = false;
            while let Ok((socket, _)) = listener.accept().await {
                let (r, mut w) = socket.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let command = req["params"]["command"].as_str().unwrap();
                    let result = app.lock().await.execute(command, req["params"]["params"].clone());
                    if command == "agent.execute" && !lost {
                        lost = true;
                        if restart {
                            *app.lock().await = filmcraft_engine::Session::default();
                        }
                        break; // The write landed; the reply and connection are lost.
                    }
                    let reply = match result {
                        Ok(v) => json!({"id":req["id"],"ok":true,"result":v}),
                        Err(e) => json!({"id":req["id"],"ok":false,"error":e.to_string()}),
                    };
                    w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
                }
            }
        });
        (addr, session)
    }

    #[tokio::test]
    async fn lost_write_reply_replays_receipt_instead_of_the_edit() {
        let (addr, session) = lossy_app(false).await;
        let b = BridgeClient::new(addr).unwrap();
        let result = b.execute("file.newSequence", json!({"name":"Once"})).await.unwrap();
        assert!(result.get("item").is_some() || result.get("sequence").is_some(), "{result}");
        let s = session.lock().await;
        assert_eq!(s.project.sequences().count(), 1);
        assert_eq!(s.history.undo.len(), 1);
    }

    #[tokio::test]
    async fn restarting_app_after_lost_reply_refuses_to_reexecute() {
        let (addr, session) = lossy_app(true).await;
        let b = BridgeClient::new(addr).unwrap();
        let error = b.execute("file.newSequence", json!({"name":"Once"})).await.unwrap_err();
        assert!(error.to_string().contains("unknown_commit"), "{error}");
        assert_eq!(session.lock().await.project.sequences().count(), 0);
    }

    /// A refused connection names the launch command and the token requirement (#3436): the
    /// reporter spent two ports and a debug-level console capture on a bare "connection refused".
    #[tokio::test]
    async fn refused_connection_error_names_the_token_requirement() {
        // Bind and drop: connecting to the freed address is deterministically refused.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        let b = BridgeClient::new(addr.as_str()).unwrap();
        let err = b.call("ui.inspect", json!({})).await.unwrap_err().to_string();
        assert!(err.contains("--control"), "{err}");
        assert!(err.contains("FILMCRAFT_CONTROL_TOKEN"), "{err}");
    }

    /// A blocking bridge export runs as an app job (#91, #92): the app never sees `wait: true` (it
    /// would encode on its UI thread and the reply would time out after 60 s), and the caller still
    /// gets the finished result.
    #[tokio::test]
    async fn a_blocking_export_is_polled_as_a_job() {
        let (addr, seen) = fake_app(json!({"frames": 642})).await;
        let b = BridgeClient::new(addr).unwrap();
        let r = b.execute("file.exportMedia", json!({"path": "/tmp/out.mov", "wait": true})).await.unwrap();
        assert_eq!(r, json!({"job": 7, "path": "/tmp/out.mov", "result": {"frames": 642}}));
        let seen = seen.lock().await;
        assert_eq!(seen[0]["params"]["params"]["wait"], json!(false));
        assert!(seen.iter().all(|q| q["params"]["params"]["wait"] != json!(true)));
        assert_eq!(seen.iter().filter(|q| q["params"]["command"] == "file.exportMedia").count(), 1, "started once");
        assert_eq!(seen.iter().filter(|q| q["params"]["command"] == "jobs.list").count(), 3);
    }

    /// The offline media a polled export drew comes back as in a blocking export's reply (#1054).
    #[tokio::test]
    async fn a_polled_export_reports_its_offline_media() {
        let off = json!([{"item": 8, "status": "missing"}]);
        let (addr, _) = fake_app_with(json!({"frames": 3}), Some(off.clone())).await;
        let b = BridgeClient::new(addr).unwrap();
        let r = b.execute("file.exportMedia", json!({"path": "/tmp/out.mov", "wait": true})).await.unwrap();
        assert_eq!(r["offlineMedia"], off, "{r}");
    }

    #[tokio::test]
    async fn a_failed_blocking_export_is_an_error() {
        let (addr, _) = fake_app(json!({"error": "encode: disk full"})).await;
        let b = BridgeClient::new(addr).unwrap();
        let e = b.execute("file.exportMedia", json!({"path": "/tmp/out.mov", "wait": true})).await.unwrap_err();
        assert!(e.to_string().contains("export failed: encode: disk full"), "{e}");
    }

    /// A stand-in for the app that serves any number of connections and answers each request on
    /// its own, as the app does: `slow` after `slow_reply` (never when `None`), anything else at
    /// once with its command id. The ids of the requests it receives are sent to the returned channel.
    async fn slow_app(slow_reply: Option<Duration>) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (seen, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let (r, w) = s.into_split();
                    let w = Arc::new(Mutex::new(w));
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let cmd = req["params"]["command"].as_str().unwrap_or_default().to_string();
                        let _ = seen.send(cmd.clone());
                        let delay = if cmd == "slow" { slow_reply } else { Some(Duration::ZERO) };
                        let w = w.clone();
                        tokio::spawn(async move {
                            let Some(d) = delay else { return };
                            tokio::time::sleep(d).await;
                            let line = format!("{}\n", json!({"id": req["id"], "ok": true, "result": cmd}));
                            let _ = w.lock().await.write_all(line.as_bytes()).await;
                        });
                    }
                });
            }
        });
        (addr, rx)
    }

    /// A command the app is slow to answer holds up only itself: a later call is answered at once
    /// instead of queueing behind it, and behind every call queued after it (#1538).
    #[tokio::test]
    async fn a_slow_command_does_not_block_later_calls() {
        let (addr, mut seen) = slow_app(None).await;
        let b = Arc::new(BridgeClient::new(addr).unwrap());
        let slow = tokio::spawn({
            let b = b.clone();
            async move { b.execute("slow", json!({})).await }
        });
        assert_eq!(seen.recv().await.as_deref(), Some("slow"));
        let fast = tokio::time::timeout(Duration::from_secs(5), b.execute("fast", json!({}))).await;
        assert_eq!(fast.expect("the later call waited on the slow one").unwrap(), json!("fast"));
        slow.abort();
    }

    /// A call given up while it waits leaves no late reply on a connection for the next call to read
    /// as its own (#1538).
    #[tokio::test]
    async fn an_abandoned_call_leaves_no_late_reply_behind() {
        let (addr, mut seen) = slow_app(Some(Duration::from_millis(100))).await;
        let b = Arc::new(BridgeClient::new(addr).unwrap());
        let slow = tokio::spawn({
            let b = b.clone();
            async move { b.execute("slow", json!({})).await }
        });
        assert_eq!(seen.recv().await.as_deref(), Some("slow"));
        slow.abort();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(b.execute("fast", json!({})).await.unwrap(), json!("fast"));
    }

    /// Everything else is forwarded unchanged.
    #[tokio::test]
    async fn other_commands_are_forwarded() {
        let (addr, seen) = fake_app(Value::Null).await;
        let b = BridgeClient::new(addr).unwrap();
        b.execute("file.exportMedia", json!({"path": "/tmp/out.mov"})).await.unwrap();
        b.execute("sequence.inspect", json!({})).await.unwrap();
        let seen = seen.lock().await;
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1]["params"]["command"], "sequence.inspect");
    }

    #[tokio::test]
    async fn credential_is_sent_on_every_request() {
        let (addr, seen) = fake_app(Value::Null).await;
        let mut b = BridgeClient::new(addr).unwrap();
        // Inject directly to avoid mutating the process environment in parallel tests.
        let token = "0123456789abcdef".repeat(4);
        b.token = Some(token.clone());
        b.call("ui.inspect", json!({})).await.unwrap();
        b.execute("file.saveAs", json!({"path": "/owner/project.fcproj"})).await.unwrap();
        let seen = seen.lock().await;
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|request| request["token"] == token));
        assert_eq!(seen[1]["params"]["params"]["path"], "/owner/project.fcproj");
    }
}
