//! User-approved, project-scoped connections. Credentials never enter project files or UI state.
use crate::{EngineError, Result, Services, Session, operations::Permission};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

const MAX_PAIRINGS: usize = 32;
const MAX_FILE: u64 = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub project_path: String,
    pub permission: Permission,
    pub media_folders: Vec<String>,
    pub output_folders: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Connection {
    id: String,
    label: String,
    credential: String,
    grant: Grant,
    approved: bool,
}
#[derive(Default)]
struct Store {
    blocked: Option<String>,
    path: Option<PathBuf>,
    entries: BTreeMap<String, Connection>,
}
#[derive(Clone, Default)]
pub struct Pairings(Arc<Mutex<Store>>);
fn error(message: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("pairing: {message}"))
}
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn canonical(path: &str) -> Result<PathBuf> {
    if path.len() > 8192 || path.contains('\0') {
        return Err(error("invalid path"));
    }
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(error("folder grants require absolute paths"));
    }
    if path.exists() {
        return std::fs::canonicalize(path).map_err(error);
    }
    let mut parent = path;
    let mut suffix = Vec::new();
    while !parent.exists() {
        if suffix.len() >= 128 {
            return Err(error("path nesting exceeds limit"));
        }
        let name = parent.file_name().ok_or_else(|| error("path has no existing ancestor"))?;
        if name == ".." || name == "." {
            return Err(error("relative components are forbidden"));
        }
        suffix.push(name.to_os_string());
        parent = parent.parent().ok_or_else(|| error("path has no existing ancestor"))?;
    }
    let mut result = std::fs::canonicalize(parent).map_err(error)?;
    for name in suffix.into_iter().rev() {
        result.push(name);
    }
    Ok(result)
}
impl Grant {
    pub fn validate(&mut self) -> Result<()> {
        self.project_path = canonical(&self.project_path)?.to_string_lossy().into_owned();
        if self.media_folders.len() > 32 || self.output_folders.len() > 32 {
            return Err(error("at most 32 media and output folders are allowed"));
        }
        for folder in self.media_folders.iter_mut().chain(self.output_folders.iter_mut()) {
            let path = canonical(folder)?;
            if !path.is_dir() {
                return Err(error(format!("folder must exist: {}", path.display())));
            }
            *folder = path.to_string_lossy().into_owned();
        }
        Ok(())
    }
    pub fn allows(&self, path: &str, write: bool) -> bool {
        let Ok(path) = canonical(path) else { return false };
        if !write && Path::new(&self.project_path) == path {
            return true;
        }
        self.output_folders.iter().chain(if write { [].iter() } else { self.media_folders.iter() }).any(|root| path.starts_with(root))
    }
}
impl Pairings {
    pub fn configure(&self, dir: &Path) -> Result<()> {
        let result = self.configure_inner(dir);
        if let Err(e) = &result {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).blocked = Some(e.to_string());
        }
        result
    }
    fn configure_inner(&self, dir: &Path) -> Result<()> {
        let mut store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let path = dir.join("agent-pairings.json");
        let entries: Vec<Connection> = match std::fs::metadata(&path) {
            Ok(m) if m.len() <= MAX_FILE => serde_json::from_slice(&std::fs::read(&path).map_err(error)?).map_err(error)?,
            Ok(_) => return Err(error("credential file exceeds limit; preserved")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(error(e)),
        };
        if entries.len() > MAX_PAIRINGS {
            return Err(error("credential count exceeds limit"));
        }
        let mut result = BTreeMap::new();
        for entry in entries {
            if !valid_id(&entry.id) || entry.credential.len() != 64 || !entry.credential.bytes().all(|b| b.is_ascii_hexdigit()) || entry.label.len() > 128 {
                return Err(error("invalid credential file; preserved"));
            }
            if result.insert(entry.id.clone(), entry).is_some() {
                return Err(error("duplicate connection identity"));
            }
        }
        store.entries = result;
        store.path = Some(path);
        Ok(())
    }
    pub fn authenticate(&self, credential: &str) -> Option<String> {
        if credential.len() != 64 || self.0.lock().unwrap_or_else(|e| e.into_inner()).blocked.is_some() {
            return None;
        }
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .values()
            .find(|entry| entry.approved && bool::from(credential.as_bytes().ct_eq(entry.credential.as_bytes())))
            .map(|e| e.id.clone())
    }
    pub fn list(&self) -> Vec<Value> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .values()
            .map(|e| json!({"id":e.id,"label":e.label,"approved":e.approved,"grant":e.grant}))
            .collect()
    }
    pub fn grant(&self, id: &str, project_path: Option<&str>) -> Result<Grant> {
        let store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let connection = store.entries.get(id).filter(|e| e.approved).ok_or_else(|| error("connection is not approved"))?;
        let project = project_path.ok_or_else(|| error("open the approved saved project first"))?;
        if canonical(project)? != canonical(&connection.grant.project_path)? {
            return Err(error("connection belongs to another project"));
        }
        Ok(connection.grant.clone())
    }
    /// Native user interaction only. This is deliberately not registered as an engine command.
    pub fn approve(&self, id: &str, mut grant: Grant) -> Result<()> {
        grant.validate()?;
        let mut store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = &store.blocked {
            return Err(error(e));
        }
        let before = store.entries.clone();
        let connection = store.entries.get_mut(id).ok_or_else(|| error("no such request"))?;
        connection.grant = grant;
        connection.approved = true;
        if let Err(e) = persist(&store) {
            store.entries = before;
            return Err(e);
        }
        Ok(())
    }
    /// Native user interaction only; revocation invalidates the token immediately.
    pub fn revoke(&self, id: &str) -> Result<()> {
        let mut store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let before = store.entries.clone();
        store.entries.remove(id).ok_or_else(|| error("no such connection"))?;
        if let Err(e) = persist(&store) {
            store.entries = before;
            return Err(e);
        }
        Ok(())
    }
    fn request(&self, p: &Value) -> Result<Value> {
        let id = p
            .get("clientId")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .ok_or_else(|| error("clientId must contain 1..128 ASCII letters/digits/-/_"))?;
        let label = p
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control))
            .ok_or_else(|| error("label must contain 1..128 bytes without controls"))?;
        let credential = p
            .get("credential")
            .and_then(Value::as_str)
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| error("credential must be 64 random hex characters"))?;
        let mut grant: Grant = serde_json::from_value(p.get("grant").cloned().ok_or_else(|| error("grant is required"))?).map_err(error)?;
        grant.validate()?;
        let mut store = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = &store.blocked {
            return Err(error(e));
        }
        if let Some(entry) = store.entries.get(id) {
            if !bool::from(entry.credential.as_bytes().ct_eq(credential.as_bytes())) {
                return Err(error("clientId is already in use"));
            }
            return Ok(json!({"clientId":id,"approved":entry.approved}));
        }
        if store.entries.values().any(|entry| bool::from(entry.credential.as_bytes().ct_eq(credential.as_bytes()))) {
            return Err(error("credential is already in use"));
        }
        if store.entries.len() >= MAX_PAIRINGS {
            return Err(error("connection limit reached; revoke an unused connection"));
        }
        store.entries.insert(id.into(), Connection { id: id.into(), label: label.into(), credential: credential.into(), grant, approved: false });
        Ok(json!({"clientId":id,"approved":false,"next":"Approve this connection and folders in Agent Review"}))
    }
}
fn persist(store: &Store) -> Result<()> {
    let Some(path) = &store.path else { return Ok(()) };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(error)?;
    }
    let values: Vec<_> = store.entries.values().filter(|e| e.approved).collect();
    let bytes = serde_json::to_vec(&values).map_err(error)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(error("credential file size exceeds limit"));
    }
    // Credentials are owner-only even during initial creation; publication remains atomic.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(path).map_err(error)?;
        use std::io::Write;
        file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600)).map_err(error)?;
        file.flush().map_err(error)?;
    }
    filmcraft_format::atomic_write(path, &bytes).map_err(error)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).map_err(error)?;
    Ok(())
}

pub struct ScopedServices {
    pub inner: Arc<dyn Services>,
    pub grant: Grant,
}
impl ScopedServices {
    fn check(&self, path: &str, write: bool) -> std::io::Result<()> {
        if self.grant.allows(path, write) {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("path is outside approved {} folders: {path}", if write { "output" } else { "media" }),
            ))
        }
    }
}
impl Services for ScopedServices {
    fn read_file(&self, p: &str) -> std::io::Result<Vec<u8>> {
        self.check(p, false)?;
        self.inner.read_file(p)
    }
    fn write_file(&self, p: &str, b: &[u8]) -> std::io::Result<()> {
        self.check(p, true)?;
        self.inner.write_file(p, b)
    }
    fn write_file_new(&self, p: &str, b: &[u8]) -> std::io::Result<()> {
        self.check(p, true)?;
        self.inner.write_file_new(p, b)
    }
    fn file_size(&self, p: &str) -> std::io::Result<u64> {
        self.check(p, false)?;
        self.inner.file_size(p)
    }
    fn read_range(&self, p: &str, o: u64, n: usize) -> std::io::Result<Vec<u8>> {
        self.check(p, false)?;
        self.inner.read_range(p, o, n)
    }
    fn reader(&self, p: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        if let Err(e) = self.check(p, false) {
            return Some(Err(e));
        }
        self.inner.reader(p)
    }
    fn list_dir(&self, p: &str) -> Option<std::io::Result<Vec<String>>> {
        if let Err(e) = self.check(p, false) {
            return Some(Err(e));
        }
        self.inner.list_dir(p)
    }
    fn list_entries(&self, p: &str) -> Option<std::io::Result<Vec<crate::media_browser::DirEntry>>> {
        if let Err(e) = self.check(p, false) {
            return Some(Err(e));
        }
        self.inner.list_entries(p)
    }
    fn file_loader(&self) -> Option<filmcraft_media::sequence::FrameLoader> {
        let inner = self.inner.clone();
        let grant = self.grant.clone();
        Some(Arc::new(move |p| if grant.allows(p, false) { inner.read_file(p) } else { Err(std::io::ErrorKind::PermissionDenied.into()) }))
    }
    fn export_in_memory(&self) -> bool {
        self.inner.export_in_memory()
    }
}

/// Restrict direct-filesystem commands as well as Services. Unknown writes fail closed.
pub fn authorize(s: &Session, grant: &Grant, id: &str, p: &Value) -> Result<()> {
    if matches!(id, "agent.execute" | "agent.operation")
        && let Some(path) = p.get("operationId").and_then(Value::as_str).and_then(|id| crate::operations::operation_project(s, id))
        && path.as_deref().and_then(|p| canonical(p).ok()) != canonical(&grant.project_path).ok()
    {
        return Err(error("no accessible operation in this project"));
    }
    if id == "agent.execute" {
        let inner = p.get("command").and_then(Value::as_str).ok_or_else(|| error("operation needs command"))?;
        if inner == id {
            return Err(error("nested operation"));
        }
        return authorize(s, grant, inner, p.get("params").unwrap_or(&Value::Null));
    }
    if matches!(id, "file.open" | "file.saveAs" | "agent.connections") {
        return Err(error("this action belongs to the owner connection; paired connections stay on their approved project"));
    }
    if matches!(id, "jobs.get" | "jobs.cancel" | "jobs.forget") || id == "workflow.status" && p.get("job").is_some_and(|v| !v.is_null()) {
        let job = p.get("job").and_then(Value::as_u64).and_then(|id| s.job_registry.get(id)).ok_or_else(|| error("no accessible job in this project"))?;
        if !belongs_to_project(&job, grant) {
            return Err(error("no accessible job in this project"));
        }
    }
    check_plan_sources(s, grant, p, 0)?;
    if id == "library.model.load"
        && ["directory", "weights", "tokenizer", "format", "weightsBytes", "weightsSha256"].iter().any(|key| p.get(*key).is_some_and(|v| !v.is_null()))
    {
        return Err(error("paired clients may load only the verified managed CLIP package"));
    }
    let read = crate::command_access::is_read_only(id, p);
    let proposal = crate::command_access::is_proposal(id);
    if !read && !(grant.permission == Permission::Edit || grant.permission == Permission::Propose && proposal) {
        return Err(error("permission_denied: this connection cannot perform that command"));
    }
    let file_read =
        matches!(id, "file.open" | "file.import" | "lut.import" | "presets.import" | "export.presets.import" | "transcript.import" | "library.load");
    let file_write = matches!(
        id,
        "file.save"
            | "file.saveAs"
            | "file.saveCopy"
            | "file.exportMedia"
            | "lut.export"
            | "presets.export"
            | "export.presets.export"
            | "captions.export"
            | "transcript.export"
            | "grade.cdl.export"
            | "grade.bakeLut"
            | "library.save"
            | "audio.dialogueEnhance"
            | "audio.separateStems"
            | "media.upscale"
            | "workflow.deliver"
    );
    if !read
        && !proposal
        && !crate::agent_plans::eligible_command(id)
        && !file_read
        && !file_write
        && !matches!(id, "agent.apply" | "jobs.cancel" | "jobs.forget")
    {
        return Err(error("command is unavailable to a folder-scoped connection; use a reviewed plan or an audited file command"));
    }
    if file_write && matches!(id, "file.save" | "file.saveCopy") && p.get("path").is_none() {
        let path = s.path.as_deref().ok_or_else(|| error("save requires a path"))?;
        if !grant.allows(path, true) {
            return Err(error("project is outside approved output folders"));
        }
    }
    // A decoder can already be cached by the desktop. Folder checks must happen before
    // consulting that shared cache, not only when opening a file through Services.
    if !matches!(
        id,
        "agent.session"
            | "agent.operation"
            | "agent.connections"
            | "agent.activity"
            | "command.list"
            | "project.inspect"
            | "agent.inspect"
            | "sequence.inspect"
            | "state.inspect"
            | "jobs.list"
            | "jobs.get"
            | "file.import"
            | "file.open"
    ) {
        check_project_sources(s, grant, &s.project)?;
    }
    check_paths(grant, p, file_write, 0, id == "agent.plan")?;
    Ok(())
}
fn check_project_sources(s: &Session, grant: &Grant, project: &filmcraft_project::Project) -> Result<()> {
    for item in project.items.values() {
        if let Some(media) = item.as_media() {
            for reference in std::iter::once(&media.media).chain(media.proxy.iter()) {
                if let filmcraft_project::MediaRef::File { path } = reference {
                    let resolved = s.media.follow(path).map_err(|e| error(format!("source path cannot be resolved: {e}")))?;
                    if !grant.allows(&resolved, false) {
                        return Err(error(format!(
                            "media item {} is outside this connection's approved folders; approve its containing folder first",
                            item.id.0
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}
fn check_plan_sources(s: &Session, grant: &Grant, value: &Value, depth: usize) -> Result<()> {
    if depth > 32 {
        return Err(error("parameter nesting exceeds limit"));
    }
    match value {
        Value::Object(values) => {
            for (key, value) in values {
                if key == "plan"
                    && let Some(id) = value.as_u64()
                {
                    if !crate::agent_plans::belongs_to_document(s, id) {
                        return Err(error("plan belongs to another project document"));
                    }
                    for before in [true, false] {
                        let (project, _, _) = s.agent_plan_snapshot(id, before)?;
                        check_project_sources(s, grant, &project)?;
                    }
                } else {
                    check_plan_sources(s, grant, value, depth + 1)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                check_plan_sources(s, grant, value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn belongs_to_project(value: &Value, grant: &Grant) -> bool {
    value.get("projectPath").and_then(Value::as_str).and_then(|p| canonical(p).ok()) == canonical(&grant.project_path).ok()
}
/// Filter cross-document metadata at the common desktop response boundary.
pub fn filter_response(s: &Session, grant: &Grant, id: &str, response: &mut Value) {
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return;
    }
    let Some(result) = response.get_mut("result") else { return };
    fn redact(s: &Session, grant: &Grant, receipt: &mut Value) {
        if check_plan_sources(s, grant, receipt, 0).is_err() {
            receipt["result"] = json!({"reportOmitted":true,"reason":"approve this proposal's media folders before inspecting its report"});
        }
    }
    if matches!(id, "agent.operation" | "agent.execute") {
        if !belongs_to_project(result, grant) {
            *response = json!({"ok":false,"error":"no accessible operation in this project"});
            return;
        }
        redact(s, grant, result);
    }
    if matches!(id, "jobs.list" | "agent.activity")
        && let Some(entries) = result.as_array_mut()
    {
        entries.retain(|entry| belongs_to_project(entry, grant));
        if id == "agent.activity" {
            for entry in entries {
                redact(s, grant, entry);
            }
        }
    }
    if id == "workflow.status"
        && let Some(entries) = result.get_mut("jobs").and_then(Value::as_array_mut)
    {
        entries.retain(|entry| belongs_to_project(entry, grant));
    }
    if id == "agent.plans"
        && let Some(plans) = result.get_mut("plans").and_then(Value::as_array_mut)
    {
        plans.retain(|plan| check_plan_sources(s, grant, plan, 0).is_ok());
    }
}

fn check_paths(grant: &Grant, v: &Value, write: bool, depth: usize, plan_refs: bool) -> Result<()> {
    if depth > 32 {
        return Err(error("parameter nesting exceeds limit"));
    }
    match v {
        Value::Object(map) if plan_refs && map.contains_key("$step") && map.len() <= 2 && map.keys().all(|key| matches!(key.as_str(), "$step" | "path")) => {}
        Value::Object(map) => {
            for (k, v) in map {
                if ["path", "paths", "dir", "directory", "folder", "destination", "dest", "outputPath", "projectPath"].contains(&k.as_str()) {
                    match v {
                        Value::String(path) if !grant.allows(path, write) => return Err(error(format!("path is outside approved folders: {path}"))),
                        Value::Array(paths) => {
                            for path in paths.iter().filter_map(Value::as_str) {
                                if !grant.allows(path, write) {
                                    return Err(error(format!("path is outside approved folders: {path}")));
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    check_paths(grant, v, write, depth + 1, plan_refs)?;
                }
            }
        }
        Value::Array(values) => {
            for v in values {
                check_paths(grant, v, write, depth + 1, plan_refs)?;
            }
        }
        _ => {}
    }
    Ok(())
}
pub(crate) fn commands() -> Vec<crate::CommandSpec> {
    vec![
        crate::CommandSpec {
            id: "agent.pairRequest",
            label: "Request Agent Pairing",
            menu: &[],
            shortcut: None,
            params: r#"{"clientId":string,"label":string,"credential":string,"grant":{projectPath:string,permission:Inspect|Propose|Edit,mediaFolders:[string],outputFolders:[string]}}"#,
            enabled: crate::commands::always,
            journal: false,
            run: |s, p| s.pairings.request(p),
        },
        crate::CommandSpec {
            id: "agent.connections",
            label: "Inspect Paired Connections",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: crate::commands::always,
            journal: false,
            run: |s, _| Ok(json!({"connections":s.pairings.list()})),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    fn setup() -> (PathBuf, Session, Grant) {
        let dir = crate::temp_dir().join(format!(
            "filmcraft-pairing-{}-{}",
            web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).unwrap().as_nanos(),
            TEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("media")).unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let project = dir.join("out/test.fcproj");
        std::fs::write(&project, b"test").unwrap();
        let s = Session { path: Some(project.to_string_lossy().into_owned()), ..Session::default() };
        let mut grant = Grant {
            project_path: s.path.clone().unwrap(),
            permission: Permission::Propose,
            media_folders: vec![dir.join("media").to_string_lossy().into_owned()],
            output_folders: vec![dir.join("out").to_string_lossy().into_owned()],
        };
        grant.validate().unwrap();
        (dir, s, grant)
    }
    #[test]
    fn pairing_requires_native_approval_and_survives_reopen_without_exposing_secret() {
        let (dir, mut s, grant) = setup();
        s.pairings.configure(&dir).unwrap();
        let secret = "f".repeat(64);
        s.execute("agent.pairRequest", json!({"clientId":"test","label":"Test","credential":secret,"grant":grant})).unwrap();
        assert!(s.pairings.authenticate(&secret).is_none());
        assert!(!serde_json::to_string(&s.pairings.list()).unwrap().contains(&secret));
        s.pairings.approve("test", grant.clone()).unwrap();
        assert_eq!(s.pairings.authenticate(&secret).as_deref(), Some("test"));
        assert!(s.pairings.grant("test", Some("/some/other/project.fcproj")).is_err());
        let restored = Pairings::default();
        restored.configure(&dir).unwrap();
        assert!(restored.authenticate(&secret).is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("agent-pairings.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        restored.revoke("test").unwrap();
        let restored = Pairings::default();
        restored.configure(&dir).unwrap();
        assert!(restored.authenticate(&secret).is_none());
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn job_and_plan_queries_cannot_cross_the_project_grant() {
        let (dir, mut s, mut grant) = setup();
        grant.permission = Permission::Edit;
        let own = s.job_registry.new_job(&s, "own".into()).unwrap();
        s.path = Some(dir.join("other.fcproj").to_string_lossy().into_owned());
        let foreign = s.job_registry.new_job(&s, "foreign".into()).unwrap();
        for command in ["jobs.get", "jobs.cancel", "jobs.forget"] {
            assert!(authorize(&s, &grant, command, &json!({"job":foreign.id})).is_err());
            assert!(authorize(&s, &grant, command, &json!({"job":own.id})).is_ok());
        }
        let mut response = json!({"ok":true,"result":s.job_registry.list()});
        filter_response(&s, &grant, "jobs.list", &mut response);
        assert_eq!(response["result"].as_array().unwrap().len(), 1);
        assert_eq!(response["result"][0]["id"], own.id);
        assert!(authorize(&s, &grant, "file.open", &json!({"path":s.path})).is_err());
        assert!(authorize(&s, &grant, "file.saveAs", &json!({"path":s.path})).is_err());
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn staged_new_media_cannot_bypass_a_folder_grant_through_the_cache() {
        let (dir, mut s, mut grant) = setup();
        grant.permission = Permission::Edit;
        let mut image = filmcraft_render::Image::new(8, 8);
        for pixel in image.px.as_chunks_mut::<4>().0 {
            *pixel = [0.1, 0.3, 0.5, 1.0];
        }
        let path = dir.join("private.png");
        std::fs::write(&path, crate::inspection::png_image(&image).unwrap()).unwrap();
        let receipt=s.execute("agent.execute",json!({"operationId":"owner-private-proposal","idempotencyKey":"owner-private-key","sessionId":s.operations.session_id,"command":"workflow.prepare","params":{"paths":[path],"transcribe":false,"analyze":false,"normalizeAudio":false,"mode":"selects"}})).unwrap();
        let prepared = receipt["result"].clone();
        let id = prepared["plan"].as_u64().unwrap();
        for command in ["agent.preview", "agent.apply", "agent.subset", "agent.discard", "render.frames"] {
            assert!(authorize(&s, &grant, command, &json!({"plan":id})).is_err(), "{command}");
        }
        assert!(authorize(&s, &grant, "analysis.footage", &json!({"subject":{"plan":id}})).is_err());
        let mut response = json!({"ok":true,"result":s.execute("agent.plans",json!({})).unwrap()});
        filter_response(&s, &grant, "agent.plans", &mut response);
        assert!(response["result"]["plans"].as_array().unwrap().is_empty());
        let mut operation = json!({"ok":true,"result":receipt.clone()});
        filter_response(&s, &grant, "agent.operation", &mut operation);
        assert_eq!(operation["result"]["result"]["reportOmitted"], true);
        assert_eq!(operation["result"]["status"], "committed");
        let mut activity = json!({"ok":true,"result":[receipt.clone()]});
        filter_response(&s, &grant, "agent.activity", &mut activity);
        assert_eq!(activity["result"][0]["result"]["reportOmitted"], true);
        grant.media_folders.push(dir.to_string_lossy().into_owned());
        grant.validate().unwrap();
        assert!(authorize(&s, &grant, "agent.preview", &json!({"plan":id})).is_ok());
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn document_open_receipt_cannot_reveal_the_destination_project() {
        let (dir, mut s, grant) = setup();
        let target = dir.join("private.fcproj");
        std::fs::write(&target, filmcraft_format::encode(&filmcraft_project::Project::new("Private"), false)).unwrap();
        let receipt=s.execute("agent.execute",json!({"sessionId":s.operations.session_id,"operationId":"open-private","idempotencyKey":"open-private-key","command":"file.open","params":{"path":target}})).unwrap();
        s.path = Some(grant.project_path.clone());
        assert!(authorize(&s, &grant, "agent.operation", &json!({"operationId":"open-private"})).is_ok());
        let mut response = json!({"ok":true,"result":receipt});
        filter_response(&s, &grant, "agent.operation", &mut response);
        assert_eq!(response["ok"], false);
        assert!(!response.to_string().contains("private.fcproj"));
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn operation_replay_cannot_reveal_another_project_receipt() {
        let (dir, mut s, grant) = setup();
        let session = s.operations.session_id.clone();
        let request = json!({"sessionId":session,"operationId":"private-op","idempotencyKey":"private-key","command":"file.newSequence","params":{}});
        s.path = Some(dir.join("private.fcproj").to_string_lossy().into_owned());
        s.execute("agent.execute", request.clone()).unwrap();
        s.path = Some(grant.project_path.clone());
        assert!(authorize(&s, &grant, "agent.execute", &request).is_err());
        assert!(authorize(&s, &grant, "agent.operation", &json!({"operationId":"private-op"})).is_err());
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scoped_files_and_generic_commands_enforce_grants() {
        let (dir, s, mut grant) = setup();
        grant.permission = Permission::Edit;
        let outside = dir.join("private.txt");
        std::fs::write(&outside, b"private").unwrap();
        let media = dir.join("media/a.txt");
        std::fs::write(&media, b"media").unwrap();
        let scoped = ScopedServices { inner: s.services.clone(), grant: grant.clone() };
        assert!(scoped.read_file(media.to_str().unwrap()).is_ok());
        assert!(scoped.read_file(outside.to_str().unwrap()).is_err());
        assert!(scoped.write_file(media.to_str().unwrap(), b"bad").is_err());
        assert!(authorize(&s, &grant, "file.exportMedia", &json!({"settings":{"path":outside}})).is_err());
        assert!(authorize(&s, &grant, "file.exportMedia", &json!({"settings":{"$step":0,"path":outside}})).is_err());
        assert!(authorize(&s, &grant, "file.exportMedia", &json!({"path":dir.join("out/test.mp4")})).is_ok());
        assert!(authorize(&s, &grant, "unreviewed.writer", &json!({})).is_err());
        grant.permission = Permission::Inspect;
        assert!(authorize(&s, &grant, "agent.plan", &json!({})).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&dir, dir.join("media/escape")).unwrap();
            assert!(!grant.allows(dir.join("media/escape/private.txt").to_str().unwrap(), false));
        }
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
