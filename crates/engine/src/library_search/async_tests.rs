use super::*;
fn session() -> (Session, u64) {
    let mut s = Session::default();
    let item = s.execute("file.newBarsAndTone", json!({"seconds":2,"width":160,"height":90})).unwrap();
    let id = item["item"].as_u64().unwrap();
    (s, id)
}

struct BlockingProvider {
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}
impl EmbeddingProvider for BlockingProvider {
    fn model(&self) -> EmbeddingModel {
        EmbeddingModel { id: "test-double".into(), dimension: 2, provenance: "synthetic test provider, no production inference".into() }
    }
    fn embed_image(&self, _: &Image) -> Result<Vec<f32>> {
        self.started.store(true, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !self.release.load(Ordering::Relaxed) {
            if filmcraft_media::cancel::cancelled() {
                return Err(bad("test", "cancelled"));
            }
            if std::time::Instant::now() >= deadline {
                return Err(bad("test", "provider timed out"));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(vec![1.0, 0.0])
    }
    fn embed_text(&self, _: &str) -> Result<Vec<f32>> {
        Ok(vec![1.0, 0.0])
    }
}
fn async_session(release: bool) -> (Session, u64, Arc<AtomicBool>, Arc<AtomicBool>) {
    let (mut s, id) = session();
    let started = Arc::new(AtomicBool::new(false));
    let flag = Arc::new(AtomicBool::new(release));
    s.library_index.provider = Some(Arc::new(BlockingProvider { started: started.clone(), release: flag.clone() }));
    (s, id, started, flag)
}
fn wait_started(flag: &AtomicBool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !flag.load(Ordering::Relaxed) {
        assert!(std::time::Instant::now() < deadline, "index never reached provider");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
fn wait_async(s: &mut Session) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        poll(s);
        if s.library_index.pending.is_none() {
            return s.library_index.last_async.clone().unwrap();
        }
        assert!(std::time::Instant::now() < deadline, "index did not finish");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}
#[test]
fn async_index_installs_only_on_main_thread_and_preserves_project_history() {
    let (mut s, item, _, _) = async_session(true);
    let before = s.project.to_json();
    let revision = s.revision;
    let history = s.history.undo.len();
    let request = s.execute("library.indexAsync", json!({"items":[item],"maxFrames":2,"embeddings":true})).unwrap();
    let job = request["job"].as_u64().unwrap();
    let result = wait_async(&mut s);
    assert_eq!(result["status"], "installed");
    assert_eq!(result["entries"], 2);
    assert_eq!(result["inputRevision"], revision);
    assert_eq!(s.project.to_json(), before);
    assert_eq!(s.revision, revision);
    assert_eq!(s.history.undo.len(), history);
    assert_eq!(s.job_registry.get(job).unwrap()["state"], "completed");
    let result = s.execute("library.search", json!({"query":"test"})).unwrap();
    assert_eq!(result["semantic"]["candidateCount"], 2);
}
#[test]
fn async_index_refuses_stale_revision_or_cache_without_replacing_it() {
    for change_cache in [false, true] {
        let (mut s, item, started, release) = async_session(false);
        s.execute("library.index", json!({"items":[item],"maxFrames":2})).unwrap();
        let mut before = serde_json::to_value(&s.library_index.entries).unwrap();
        s.execute("library.indexAsync", json!({"items":[item],"maxFrames":2,"embeddings":true,"force":true})).unwrap();
        wait_started(&started);
        if change_cache {
            let range = s.library_index.entries.keys().next().cloned().unwrap();
            s.execute(
                "library.embeddings",
                json!({"model":{"id":"new-test-vectors","dimension":2,"provenance":"test cache update"},"entries":[{"id":range,"vector":[0.0,1.0]}]}),
            )
            .unwrap();
            before = serde_json::to_value(&s.library_index.entries).unwrap();
        } else {
            s.edit("Human edit", |p, _| {
                p.name.push_str(" changed");
                Ok(())
            })
            .unwrap();
        }
        release.store(true, Ordering::Relaxed);
        let result = wait_async(&mut s);
        assert_eq!(result["status"], "stale");
        assert_eq!(serde_json::to_value(&s.library_index.entries).unwrap(), before);
    }
}
#[test]
fn async_cancel_is_cooperative_and_worker_overlap_is_rejected() {
    let (mut s, item, started, _) = async_session(false);
    let request = s.execute("library.indexAsync", json!({"items":[item],"maxFrames":2,"embeddings":true})).unwrap();
    let id = request["job"].as_u64().unwrap();
    wait_started(&started);
    assert!(s.execute("library.indexAsync", json!({"items":[item],"maxFrames":2})).is_err());
    assert!(s.execute("library.index", json!({"items":[item],"maxFrames":2})).is_err());
    #[cfg(all(feature = "vision", not(target_arch = "wasm32")))]
    assert!(s.execute("library.model.load", json!({})).is_err());
    s.execute("jobs.cancel", json!({"job":id})).unwrap();
    let result = wait_async(&mut s);
    assert_eq!(result["status"], "cancelled");
    assert!(s.library_index.entries.is_empty());
    assert_eq!(s.job_registry.get(id).unwrap()["state"], "cancelled");
}

#[test]
fn provider_panic_becomes_failed_job_without_hanging_or_installing_partial_entries() {
    struct Broken;
    impl EmbeddingProvider for Broken {
        fn model(&self) -> EmbeddingModel {
            EmbeddingModel { id: "broken-test-provider".into(), dimension: 2, provenance: "synthetic panic regression".into() }
        }
        fn embed_image(&self, _: &Image) -> Result<Vec<f32>> {
            panic!("synthetic provider panic")
        }
        fn embed_text(&self, _: &str) -> Result<Vec<f32>> {
            Err(bad("test", "unused"))
        }
    }
    let (mut s, item) = session();
    s.library_index.provider = Some(Arc::new(Broken));
    let before = s.project.to_json();
    let request = s.execute("library.indexAsync", json!({"items":[item],"maxFrames":2,"embeddings":true})).unwrap();
    let id = request["job"].as_u64().unwrap();
    let result = wait_async(&mut s);
    assert_eq!(result["status"], "failed");
    assert_eq!(s.job_registry.get(id).unwrap()["state"], "failed");
    assert!(s.library_index.entries.is_empty());
    assert_eq!(s.project.to_json(), before);
}
