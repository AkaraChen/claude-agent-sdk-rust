use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use claude_agent_sdk::internal::query::Query;
use claude_agent_sdk::internal::sessions::get_projects_dir;
use claude_agent_sdk::internal::transcript_mirror_batcher::{
    MAX_PENDING_BYTES, MAX_PENDING_ENTRIES, MirrorErrorCallback, TranscriptMirrorBatcher,
};
use claude_agent_sdk::{
    InMemorySessionStore, SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry,
    SessionStoreListEntry, SessionSummaryEntry, Transport, file_path_to_session_key,
};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify};

const PROJECTS_DIR: &str = "/home/user/.claude/projects";
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvGuard {
    previous: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", value) },
            None => unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") },
        }
    }
}

fn set_config_dir(value: &str) -> EnvGuard {
    let lock = ENV_LOCK.lock().unwrap();
    let previous = std::env::var("CLAUDE_CONFIG_DIR").ok();
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", value) };
    EnvGuard {
        previous,
        _lock: lock,
    }
}

fn main_path(project: &str, session: &str) -> String {
    format!("{PROJECTS_DIR}/{project}/{session}.jsonl")
}

fn noop_error() -> MirrorErrorCallback {
    Arc::new(|_, _| Box::pin(async {}))
}

async fn wait_for_append_calls(store: &RecordingStore, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if store.calls.lock().await.len() >= expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[derive(Default)]
struct RecordingStore {
    inner: InMemorySessionStore,
    calls: Arc<Mutex<Vec<(SessionKey, Vec<SessionStoreEntry>)>>>,
}

#[async_trait]
impl SessionStore for RecordingStore {
    async fn append(
        &self,
        key: SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> claude_agent_sdk::Result<()> {
        self.calls.lock().await.push((key.clone(), entries.clone()));
        self.inner.append(key, entries).await
    }

    async fn load(
        &self,
        key: SessionKey,
    ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
        self.inner.load(key).await
    }

    async fn list_sessions(
        &self,
        project_key: String,
    ) -> claude_agent_sdk::Result<Vec<SessionStoreListEntry>> {
        self.inner.list_sessions(project_key).await
    }

    async fn list_session_summaries(
        &self,
        project_key: String,
    ) -> claude_agent_sdk::Result<Vec<SessionSummaryEntry>> {
        self.inner.list_session_summaries(project_key).await
    }

    async fn delete(&self, key: SessionKey) -> claude_agent_sdk::Result<()> {
        self.inner.delete(key).await
    }

    async fn list_subkeys(
        &self,
        key: SessionListSubkeysKey,
    ) -> claude_agent_sdk::Result<Vec<String>> {
        self.inner.list_subkeys(key).await
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_list_session_summaries(&self) -> bool {
        true
    }

    fn supports_delete(&self) -> bool {
        true
    }

    fn supports_list_subkeys(&self) -> bool {
        true
    }
}

struct FailingStore;

#[async_trait]
impl SessionStore for FailingStore {
    async fn append(
        &self,
        _key: SessionKey,
        _entries: Vec<SessionStoreEntry>,
    ) -> claude_agent_sdk::Result<()> {
        Err(claude_agent_sdk::ClaudeSdkError::Runtime(
            "boom".to_string(),
        ))
    }

    async fn load(
        &self,
        _key: SessionKey,
    ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
        Ok(None)
    }
}

#[derive(Clone)]
struct StaticTransport {
    messages: Arc<std::sync::Mutex<Option<Vec<Value>>>>,
}

impl StaticTransport {
    fn new(messages: Vec<Value>) -> Self {
        Self {
            messages: Arc::new(std::sync::Mutex::new(Some(messages))),
        }
    }
}

#[async_trait]
impl Transport for StaticTransport {
    async fn connect(&self) -> claude_agent_sdk::Result<()> {
        Ok(())
    }

    async fn write(&self, _data: &str) -> claude_agent_sdk::Result<()> {
        Ok(())
    }

    fn read_messages(&self) -> BoxStream<'static, claude_agent_sdk::Result<Value>> {
        let messages = self.messages.lock().unwrap().take().unwrap_or_default();
        Box::pin(stream::iter(messages.into_iter().map(Ok)))
    }

    async fn close(&self) -> claude_agent_sdk::Result<()> {
        Ok(())
    }

    fn is_ready(&self) -> bool {
        true
    }

    async fn end_input(&self) -> claude_agent_sdk::Result<()> {
        Ok(())
    }
}

#[test]
fn file_path_to_session_key_matches_python_shapes() {
    assert_eq!(
        file_path_to_session_key(
            "/home/user/.claude/projects/-home-user-repo/abc-123.jsonl",
            PROJECTS_DIR
        ),
        Some(SessionKey {
            project_key: "-home-user-repo".to_string(),
            session_id: "abc-123".to_string(),
            subpath: None,
        })
    );
    assert_eq!(
        file_path_to_session_key(
            "/home/user/.claude/projects/proj/sess/subagents/nested/agent-1.jsonl",
            PROJECTS_DIR
        ),
        Some(SessionKey {
            project_key: "proj".to_string(),
            session_id: "sess".to_string(),
            subpath: Some("subagents/nested/agent-1".to_string()),
        })
    );
    assert!(file_path_to_session_key("/elsewhere/proj/sess.jsonl", PROJECTS_DIR).is_none());
    assert!(
        file_path_to_session_key(
            "/home/user/.claude/projects/proj/sess/weird.jsonl",
            PROJECTS_DIR
        )
        .is_none()
    );
}

#[test]
fn get_projects_dir_ignores_empty_override_like_python() {
    let _guard = set_config_dir("/ambient/config");
    let custom = PathBuf::from("/custom/config");
    let mut env_override = HashMap::new();
    env_override.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        custom.to_string_lossy().to_string(),
    );
    assert_eq!(
        get_projects_dir(Some(&env_override)),
        custom.join("projects")
    );

    env_override.insert("CLAUDE_CONFIG_DIR".to_string(), String::new());
    assert_eq!(
        get_projects_dir(Some(&env_override)),
        PathBuf::from("/ambient/config").join("projects")
    );
}

#[tokio::test]
async fn flush_coalesces_per_file_path_preserving_first_seen_order() {
    let store = Arc::new(RecordingStore::default());
    let batcher = Arc::new(TranscriptMirrorBatcher::new(
        store.clone(),
        PROJECTS_DIR.to_string(),
        noop_error(),
        MAX_PENDING_ENTRIES,
        MAX_PENDING_BYTES,
    ));
    batcher
        .clone()
        .enqueue(main_path("p", "a"), vec![json!({"type": "x", "n": 1})])
        .await;
    batcher
        .clone()
        .enqueue(main_path("p", "b"), vec![json!({"type": "x", "n": 2})])
        .await;
    batcher
        .clone()
        .enqueue(main_path("p", "a"), vec![json!({"type": "x", "n": 3})])
        .await;
    batcher.flush().await;

    let calls = store.calls.lock().await.clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0.session_id, "a");
    assert_eq!(
        calls[0]
            .1
            .iter()
            .map(|e| e["n"].clone())
            .collect::<Vec<_>>(),
        vec![json!(1), json!(3)]
    );
    assert_eq!(calls[1].0.session_id, "b");
    assert_eq!(calls[1].1[0]["n"], json!(2));
}

#[tokio::test]
async fn eager_flush_triggers_on_entry_count_and_byte_thresholds() {
    let store = Arc::new(RecordingStore::default());
    let batcher = Arc::new(TranscriptMirrorBatcher::new(
        store.clone(),
        PROJECTS_DIR.to_string(),
        noop_error(),
        5,
        MAX_PENDING_BYTES,
    ));
    batcher
        .clone()
        .enqueue(main_path("p", "s"), vec![json!({"type": "x"}); 6])
        .await;
    wait_for_append_calls(&store, 1).await;
    assert_eq!(store.calls.lock().await[0].1.len(), 6);

    let store = Arc::new(RecordingStore::default());
    let batcher = Arc::new(TranscriptMirrorBatcher::new(
        store.clone(),
        PROJECTS_DIR.to_string(),
        noop_error(),
        MAX_PENDING_ENTRIES,
        100,
    ));
    batcher
        .clone()
        .enqueue(
            main_path("p", "s"),
            vec![json!({"type": "x", "blob": "a".repeat(200)})],
        )
        .await;
    wait_for_append_calls(&store, 1).await;
}

#[tokio::test]
async fn close_flushes_pending_and_flush_waits_for_inflight_eager_flush() {
    let store = Arc::new(RecordingStore::default());
    let batcher = Arc::new(TranscriptMirrorBatcher::new(
        store.clone(),
        PROJECTS_DIR.to_string(),
        noop_error(),
        MAX_PENDING_ENTRIES,
        MAX_PENDING_BYTES,
    ));
    batcher
        .clone()
        .enqueue(main_path("p", "s"), vec![json!({"type": "x", "n": 1})])
        .await;
    batcher.close().await;
    assert_eq!(store.calls.lock().await.len(), 1);

    struct SlowStore {
        started: Arc<Notify>,
        release: Arc<Notify>,
        appended: Arc<Mutex<Vec<i64>>>,
    }

    #[async_trait]
    impl SessionStore for SlowStore {
        async fn append(
            &self,
            _key: SessionKey,
            entries: Vec<SessionStoreEntry>,
        ) -> claude_agent_sdk::Result<()> {
            self.started.notify_one();
            self.release.notified().await;
            self.appended
                .lock()
                .await
                .extend(entries.iter().filter_map(|entry| entry["n"].as_i64()));
            Ok(())
        }

        async fn load(
            &self,
            _key: SessionKey,
        ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
            Ok(None)
        }
    }

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let appended = Arc::new(Mutex::new(Vec::new()));
    let batcher = Arc::new(TranscriptMirrorBatcher::new(
        Arc::new(SlowStore {
            started: started.clone(),
            release: release.clone(),
            appended: appended.clone(),
        }),
        PROJECTS_DIR.to_string(),
        noop_error(),
        0,
        MAX_PENDING_BYTES,
    ));
    batcher
        .clone()
        .enqueue(main_path("p", "s"), vec![json!({"type": "x", "n": 1})])
        .await;
    started.notified().await;

    let flush = {
        let batcher = batcher.clone();
        tokio::spawn(async move {
            batcher.flush().await;
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(!flush.is_finished());
    release.notify_waiters();
    tokio::time::timeout(std::time::Duration::from_secs(1), flush)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*appended.lock().await, vec![1]);
}

#[tokio::test]
async fn empty_and_unmapped_batches_do_not_append_or_report_error() {
    let store = Arc::new(RecordingStore::default());
    let errors = Arc::new(Mutex::new(Vec::<(Option<SessionKey>, String)>::new()));
    let on_error: MirrorErrorCallback = {
        let errors = errors.clone();
        Arc::new(move |key, error| {
            let errors = errors.clone();
            Box::pin(async move {
                errors.lock().await.push((key, error));
            })
        })
    };
    let batcher = Arc::new(TranscriptMirrorBatcher::new(
        store.clone(),
        PROJECTS_DIR.to_string(),
        on_error,
        MAX_PENDING_ENTRIES,
        MAX_PENDING_BYTES,
    ));
    batcher
        .clone()
        .enqueue(main_path("p", "s"), Vec::<Value>::new())
        .await;
    batcher
        .clone()
        .enqueue("/elsewhere/x.jsonl".to_string(), vec![json!({"type": "x"})])
        .await;
    batcher.flush().await;

    assert!(store.calls.lock().await.is_empty());
    assert!(errors.lock().await.is_empty());
}

#[tokio::test]
async fn append_failures_are_retried_then_reported_once() {
    let errors = Arc::new(Mutex::new(Vec::<(Option<SessionKey>, String)>::new()));
    let on_error: MirrorErrorCallback = {
        let errors = errors.clone();
        Arc::new(move |key, error| {
            let errors = errors.clone();
            Box::pin(async move {
                errors.lock().await.push((key, error));
            })
        })
    };
    let batcher = Arc::new(TranscriptMirrorBatcher::new(
        Arc::new(FailingStore),
        PROJECTS_DIR.to_string(),
        on_error,
        MAX_PENDING_ENTRIES,
        MAX_PENDING_BYTES,
    ));
    batcher
        .clone()
        .enqueue(main_path("proj", "sess"), vec![json!({"type": "x"})])
        .await;
    batcher.flush().await;

    let errors = errors.lock().await.clone();
    assert_eq!(errors.len(), 1);
    assert_eq!(
        errors[0].0,
        Some(SessionKey {
            project_key: "proj".to_string(),
            session_id: "sess".to_string(),
            subpath: None,
        })
    );
    assert!(errors[0].1.contains("boom"));
}

#[tokio::test]
async fn receive_loop_peels_mirror_frames_and_flushes_before_result() {
    let store = Arc::new(RecordingStore::default());
    let mirror_frame = json!({
        "type": "transcript_mirror",
        "filePath": main_path("myproj", "mysess"),
        "entries": [{"type": "user", "uuid": "u1"}],
    });
    let transport = StaticTransport::new(vec![
        mirror_frame.clone(),
        json!({"type": "assistant", "message": {"role": "assistant", "content": []}}),
        mirror_frame,
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "s",
        }),
    ]);
    let query = Arc::new(Query::new(
        Arc::new(transport),
        true,
        None,
        None,
        Default::default(),
        std::time::Duration::from_secs(1),
        None,
        None,
        None,
    ));
    query
        .set_transcript_mirror_batcher(TranscriptMirrorBatcher::new(
            store.clone(),
            PROJECTS_DIR.to_string(),
            noop_error(),
            MAX_PENDING_ENTRIES,
            MAX_PENDING_BYTES,
        ))
        .await;
    query.start().await;

    let mut stream = query.receive_messages();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first["type"], "assistant");
    let second = stream.next().await.unwrap().unwrap();
    assert_eq!(second["type"], "result");
    assert_eq!(store.calls.lock().await.len(), 1);
    assert!(stream.next().await.is_none());

    let calls = store.calls.lock().await.clone();
    assert_eq!(calls[0].0.project_key, "myproj");
    assert_eq!(calls[0].0.session_id, "mysess");
    assert_eq!(
        calls[0].1,
        vec![
            json!({"type": "user", "uuid": "u1"}),
            json!({"type": "user", "uuid": "u1"}),
        ]
    );
    query.close().await.unwrap();
}

#[tokio::test]
async fn mirror_frames_are_dropped_without_store_and_failures_inject_mirror_error() {
    let transport = StaticTransport::new(vec![
        json!({
            "type": "transcript_mirror",
            "filePath": main_path("p", "s"),
            "entries": [{"type": "user"}],
        }),
        json!({"type": "assistant", "message": {"role": "assistant", "content": []}}),
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "s",
        }),
    ]);
    let query = Arc::new(Query::new(
        Arc::new(transport),
        true,
        None,
        None,
        Default::default(),
        std::time::Duration::from_secs(1),
        None,
        None,
        None,
    ));
    query.start().await;
    let messages = query.receive_messages().collect::<Vec<_>>().await;
    assert_eq!(
        messages
            .into_iter()
            .map(|msg| msg.unwrap()["type"].as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        vec!["assistant".to_string(), "result".to_string()]
    );
    query.close().await.unwrap();

    let transport = StaticTransport::new(vec![
        json!({
            "type": "transcript_mirror",
            "filePath": main_path("proj", "sess"),
            "entries": [{"type": "user"}],
        }),
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "sess",
        }),
    ]);
    let query = Arc::new(Query::new(
        Arc::new(transport),
        true,
        None,
        None,
        Default::default(),
        std::time::Duration::from_secs(1),
        None,
        None,
        None,
    ));
    let query_for_error = query.clone();
    let on_error: MirrorErrorCallback = Arc::new(move |key, error| {
        let query_for_error = query_for_error.clone();
        Box::pin(async move {
            query_for_error.report_mirror_error(key, error).await;
        })
    });
    query
        .set_transcript_mirror_batcher(TranscriptMirrorBatcher::new(
            Arc::new(FailingStore),
            PROJECTS_DIR.to_string(),
            on_error,
            MAX_PENDING_ENTRIES,
            MAX_PENDING_BYTES,
        ))
        .await;
    query.start().await;
    let messages = query
        .receive_messages()
        .map(|msg| msg.unwrap())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(messages[0]["type"], "system");
    assert_eq!(messages[0]["subtype"], "mirror_error");
    assert!(messages[0]["error"].as_str().unwrap().contains("boom"));
    assert_eq!(messages[0]["key"]["project_key"], "proj");
    assert_eq!(messages[1]["type"], "result");
    query.close().await.unwrap();
}

#[tokio::test]
async fn late_mirror_frames_after_result_are_flushed_on_stream_end() {
    let store = Arc::new(RecordingStore::default());
    let transport = StaticTransport::new(vec![
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "sess",
        }),
        json!({
            "type": "transcript_mirror",
            "filePath": main_path("late", "sess"),
            "entries": [{"type": "user", "uuid": "late-u1"}],
        }),
    ]);
    let query = Arc::new(Query::new(
        Arc::new(transport),
        true,
        None,
        None,
        Default::default(),
        std::time::Duration::from_secs(1),
        None,
        None,
        None,
    ));
    query
        .set_transcript_mirror_batcher(TranscriptMirrorBatcher::new(
            store.clone(),
            PROJECTS_DIR.to_string(),
            noop_error(),
            MAX_PENDING_ENTRIES,
            MAX_PENDING_BYTES,
        ))
        .await;
    query.start().await;

    let messages = query
        .receive_messages()
        .map(|msg| msg.unwrap())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["type"], "result");

    let calls = store.calls.lock().await.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.project_key, "late");
    assert_eq!(calls[0].0.session_id, "sess");
    assert_eq!(calls[0].1, vec![json!({"type": "user", "uuid": "late-u1"})]);
    query.close().await.unwrap();
}
