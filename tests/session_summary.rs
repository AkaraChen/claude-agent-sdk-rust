use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use claude_agent_sdk::internal::sessions::STORE_LIST_LOAD_CONCURRENCY;
use claude_agent_sdk::{
    ClaudeSdkError, InMemorySessionStore, JsonMap, SessionKey, SessionStore, SessionStoreEntry,
    SessionStoreListEntry, SessionSummaryEntry, fold_session_summary, list_sessions_from_store,
    project_key_for_directory, summary_entry_to_sdk_info,
};
use serde_json::{Value, json};
use tokio::sync::Notify;
use uuid::Uuid;

const DIR: &str = "/workspace/project";

fn project_key() -> String {
    project_key_for_directory(Some(Path::new(DIR)))
}

fn key(session_id: &str) -> SessionKey {
    SessionKey {
        project_key: project_key(),
        session_id: session_id.to_string(),
        subpath: None,
    }
}

fn user(text: impl Into<Value>, ts: &str) -> Value {
    json!({
        "type": "user",
        "timestamp": ts,
        "message": {"role": "user", "content": text.into()},
    })
}

fn object_map(value: Value) -> JsonMap {
    let Value::Object(map) = value else {
        panic!("expected object");
    };
    map
}

#[test]
fn fold_sidechain_latches_from_first_entry_even_without_timestamp() {
    let summary = fold_session_summary(
        None,
        &key("11111111-1111-4111-8111-111111111111"),
        &[
            json!({"type": "user", "isSidechain": true}),
            json!({"type": "x", "timestamp": "2024-01-01T00:00:00Z"}),
        ],
    );
    assert_eq!(summary.data["is_sidechain"], true);
    assert_eq!(summary.data["created_at"], 1_704_067_200_000_i64);
}

#[test]
fn fold_first_prompt_command_fallback_skip_patterns_and_prev_immutability() {
    let k = key("11111111-1111-4111-8111-111111111111");
    let summary = fold_session_summary(
        None,
        &k,
        &[
            user(
                "<command-name>/init</command-name> stuff",
                "2024-01-01T00:00:00Z",
            ),
            user(
                "<command-name>/second</command-name>",
                "2024-01-01T00:00:01Z",
            ),
        ],
    );
    assert_ne!(summary.data.get("first_prompt_locked"), Some(&json!(true)));
    assert_eq!(summary.data["command_fallback"], "/init");

    let locked = fold_session_summary(
        Some(&summary),
        &k,
        &[user("now real", "2024-01-01T00:00:02Z")],
    );
    assert_eq!(locked.data["first_prompt"], "now real");
    assert_eq!(locked.data["first_prompt_locked"], true);

    let skipped = fold_session_summary(
        None,
        &k,
        &[
            user("<local-command-stdout> some output", "2024-01-01T00:00:00Z"),
            user("hello", "2024-01-01T00:00:01Z"),
        ],
    );
    assert_eq!(skipped.data["first_prompt"], "hello");

    let prev = SessionSummaryEntry {
        session_id: "a".to_string(),
        mtime: 5,
        data: JsonMap::new(),
    };
    let _ = fold_session_summary(Some(&prev), &k, &[json!({"type": "x", "customTitle": "t"})]);
    assert_eq!(prev.session_id, "a");
    assert_eq!(prev.mtime, 5);
    assert!(prev.data.is_empty());
}

#[test]
fn summary_entry_to_sdk_info_matches_python_precedence_chain() {
    let mut data = object_map(json!({
        "first_prompt": "fp",
        "first_prompt_locked": true,
        "command_fallback": "/cmd",
        "summary_hint": "sh",
        "last_prompt": "lp",
        "ai_title": "ai",
        "custom_title": "ct",
    }));
    let mut entry = SessionSummaryEntry {
        session_id: "s".to_string(),
        mtime: 1,
        data: data.clone(),
    };
    let info = summary_entry_to_sdk_info(&entry, None).unwrap();
    assert_eq!(info.summary, "ct");
    assert_eq!(info.custom_title.as_deref(), Some("ct"));

    data.remove("custom_title");
    entry.data = data.clone();
    let info = summary_entry_to_sdk_info(&entry, None).unwrap();
    assert_eq!(info.summary, "ai");
    assert_eq!(info.custom_title.as_deref(), Some("ai"));

    data.remove("ai_title");
    entry.data = data.clone();
    let info = summary_entry_to_sdk_info(&entry, None).unwrap();
    assert_eq!(info.summary, "lp");
    assert!(info.custom_title.is_none());

    data.remove("last_prompt");
    entry.data = data.clone();
    assert_eq!(
        summary_entry_to_sdk_info(&entry, None).unwrap().summary,
        "sh"
    );

    data.remove("summary_hint");
    entry.data = data.clone();
    let info = summary_entry_to_sdk_info(&entry, None).unwrap();
    assert_eq!(info.summary, "fp");
    assert_eq!(info.first_prompt.as_deref(), Some("fp"));

    data.insert("first_prompt_locked".to_string(), Value::Bool(false));
    entry.data = data;
    let info = summary_entry_to_sdk_info(&entry, None).unwrap();
    assert_eq!(info.summary, "/cmd");
    assert_eq!(info.first_prompt.as_deref(), Some("/cmd"));
}

#[test]
fn summary_entry_to_sdk_info_cwd_and_field_passthrough() {
    let info = summary_entry_to_sdk_info(
        &SessionSummaryEntry {
            session_id: "s".to_string(),
            mtime: 99,
            data: object_map(json!({
                "custom_title": "t",
                "git_branch": "main",
                "tag": "wip",
                "created_at": 50,
            })),
        },
        Some("/proj"),
    )
    .unwrap();
    assert_eq!(info.session_id, "s");
    assert_eq!(info.last_modified, 99);
    assert_eq!(info.summary, "t");
    assert_eq!(info.cwd.as_deref(), Some("/proj"));
    assert_eq!(info.git_branch.as_deref(), Some("main"));
    assert_eq!(info.tag.as_deref(), Some("wip"));
    assert_eq!(info.created_at, Some(50));
    assert!(info.file_size.is_none());

    let own_cwd = summary_entry_to_sdk_info(
        &SessionSummaryEntry {
            session_id: "s".to_string(),
            mtime: 1,
            data: object_map(json!({"custom_title": "t", "cwd": "/own"})),
        },
        Some("/proj"),
    )
    .unwrap();
    assert_eq!(own_cwd.cwd.as_deref(), Some("/own"));
}

#[tokio::test]
async fn in_memory_list_session_summaries_track_appends_delete_and_project_scope() {
    let store = InMemorySessionStore::new();
    let pk = project_key();
    let a = SessionKey {
        project_key: pk.clone(),
        session_id: "a".to_string(),
        subpath: None,
    };
    let b = SessionKey {
        project_key: pk.clone(),
        session_id: "b".to_string(),
        subpath: None,
    };
    store
        .append(a.clone(), vec![user("hello a", "2024-01-01T00:00:00Z")])
        .await
        .unwrap();
    store
        .append(
            a.clone(),
            vec![json!({"type": "x", "customTitle": "Title A"})],
        )
        .await
        .unwrap();
    store
        .append(b.clone(), vec![user("hello b", "2024-01-02T00:00:00Z")])
        .await
        .unwrap();
    store
        .append(
            SessionKey {
                subpath: Some("subagents/agent-1".to_string()),
                ..a.clone()
            },
            vec![
                user("sub prompt", "2024-01-03T00:00:00Z"),
                json!({"type": "x", "customTitle": "sub"}),
            ],
        )
        .await
        .unwrap();

    let summaries = store.list_session_summaries(pk.clone()).await.unwrap();
    let by_id = summaries
        .iter()
        .map(|summary| (summary.session_id.as_str(), summary))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(by_id["a"].data["custom_title"], "Title A");
    assert_eq!(by_id["a"].data["first_prompt"], "hello a");
    assert_eq!(by_id["b"].data["first_prompt"], "hello b");
    assert_eq!(
        store
            .list_session_summaries("missing".to_string())
            .await
            .unwrap(),
        vec![]
    );

    store.delete(a).await.unwrap();
    let remaining = store.list_session_summaries(pk).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].session_id, "b");
}

#[derive(Default)]
struct NoLoadFastPathStore {
    inner: InMemorySessionStore,
}

#[async_trait]
impl SessionStore for NoLoadFastPathStore {
    async fn append(
        &self,
        key: SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> claude_agent_sdk::Result<()> {
        self.inner.append(key, entries).await
    }

    async fn load(
        &self,
        _key: SessionKey,
    ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
        Err(ClaudeSdkError::Runtime(
            "load() must not be called on the fast path".to_string(),
        ))
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

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_list_session_summaries(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn list_sessions_from_store_fast_path_skips_load_when_sidecars_complete() {
    let store = NoLoadFastPathStore::default();
    let sid_a = Uuid::new_v4().to_string();
    let sid_b = Uuid::new_v4().to_string();
    store
        .append(key(&sid_a), vec![user("first a", "2024-01-01T00:00:00Z")])
        .await
        .unwrap();
    store
        .append(key(&sid_b), vec![user("first b", "2024-01-02T00:00:00Z")])
        .await
        .unwrap();

    let sessions = list_sessions_from_store(&store, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<std::collections::HashSet<_>>(),
        [sid_a.as_str(), sid_b.as_str()].into_iter().collect()
    );
    assert_eq!(sessions[0].session_id, sid_b);
    assert_eq!(sessions[0].summary, "first b");
    assert_eq!(sessions[1].first_prompt.as_deref(), Some("first a"));
}

struct SummaryErrorStore {
    inner: InMemorySessionStore,
    load_calls: AtomicUsize,
    summary_error: String,
}

#[async_trait]
impl SessionStore for SummaryErrorStore {
    async fn append(
        &self,
        key: SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> claude_agent_sdk::Result<()> {
        self.inner.append(key, entries).await
    }

    async fn load(
        &self,
        key: SessionKey,
    ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
        self.load_calls.fetch_add(1, Ordering::SeqCst);
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
        _project_key: String,
    ) -> claude_agent_sdk::Result<Vec<SessionSummaryEntry>> {
        Err(ClaudeSdkError::Runtime(self.summary_error.clone()))
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_list_session_summaries(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn list_sessions_from_store_only_falls_back_for_unimplemented_summaries() {
    let fallback = SummaryErrorStore {
        inner: InMemorySessionStore::new(),
        load_calls: AtomicUsize::new(0),
        summary_error: "list_session_summaries is not implemented".to_string(),
    };
    fallback
        .append(
            key(&Uuid::new_v4().to_string()),
            vec![user("fallback prompt", "2024-01-01T00:00:00Z")],
        )
        .await
        .unwrap();
    let sessions = list_sessions_from_store(&fallback, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].summary, "fallback prompt");
    assert_eq!(fallback.load_calls.load(Ordering::SeqCst), 1);

    let failure = SummaryErrorStore {
        inner: InMemorySessionStore::new(),
        load_calls: AtomicUsize::new(0),
        summary_error: "summary index unavailable".to_string(),
    };
    failure
        .append(
            key(&Uuid::new_v4().to_string()),
            vec![user("should not fallback", "2024-01-01T00:00:00Z")],
        )
        .await
        .unwrap();
    let err = list_sessions_from_store(&failure, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("summary index unavailable"));
    assert_eq!(failure.load_calls.load(Ordering::SeqCst), 0);
}

struct StaleSidecarStore {
    inner: InMemorySessionStore,
    sid: String,
    load_calls: AtomicUsize,
}

#[async_trait]
impl SessionStore for StaleSidecarStore {
    async fn append(
        &self,
        key: SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> claude_agent_sdk::Result<()> {
        self.inner.append(key, entries).await
    }

    async fn load(
        &self,
        key: SessionKey,
    ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
        self.load_calls.fetch_add(1, Ordering::SeqCst);
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
        project_key_arg: String,
    ) -> claude_agent_sdk::Result<Vec<SessionSummaryEntry>> {
        if project_key_arg != project_key() {
            return Ok(Vec::new());
        }
        Ok(vec![SessionSummaryEntry {
            session_id: self.sid.clone(),
            mtime: 1,
            data: object_map(json!({
                "custom_title": "old",
                "first_prompt": "old prompt",
                "first_prompt_locked": true,
                "created_at": 1,
            })),
        }])
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_list_session_summaries(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn stale_sidecar_triggers_gap_fill_and_fresh_transcript_wins() {
    let sid = Uuid::new_v4().to_string();
    let store = StaleSidecarStore {
        inner: InMemorySessionStore::new(),
        sid: sid.clone(),
        load_calls: AtomicUsize::new(0),
    };
    store
        .append(
            key(&sid),
            vec![
                user("fresh prompt", "2024-01-02T00:00:00Z"),
                json!({"type": "x", "timestamp": "2024-01-02T00:01:00Z", "customTitle": "fresh"}),
            ],
        )
        .await
        .unwrap();

    let sessions = list_sessions_from_store(&store, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, sid);
    assert_eq!(sessions[0].custom_title.as_deref(), Some("fresh"));
    assert_eq!(sessions[0].summary, "fresh");
    assert_eq!(store.load_calls.load(Ordering::SeqCst), 1);
}

struct PartialSlowStore {
    inner: InMemorySessionStore,
    in_flight: AtomicUsize,
    peak: AtomicUsize,
    released: AtomicBool,
    gate: Notify,
}

impl PartialSlowStore {
    fn new() -> Self {
        Self {
            inner: InMemorySessionStore::new(),
            in_flight: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            released: AtomicBool::new(false),
            gate: Notify::new(),
        }
    }

    fn update_peak(&self, current: usize) {
        let mut peak = self.peak.load(Ordering::SeqCst);
        while current > peak {
            match self
                .peak
                .compare_exchange(peak, current, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break,
                Err(next) => peak = next,
            }
        }
    }
}

#[async_trait]
impl SessionStore for PartialSlowStore {
    async fn append(
        &self,
        key: SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> claude_agent_sdk::Result<()> {
        self.inner.append(key, entries).await
    }

    async fn load(
        &self,
        key: SessionKey,
    ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.update_peak(current);
        while !self.released.load(Ordering::SeqCst) {
            self.gate.notified().await;
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
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
        _project_key: String,
    ) -> claude_agent_sdk::Result<Vec<SessionSummaryEntry>> {
        Ok(Vec::new())
    }

    fn supports_list_sessions(&self) -> bool {
        true
    }

    fn supports_list_session_summaries(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn gap_fill_loads_are_bounded_concurrently_on_fast_path() {
    let store = Arc::new(PartialSlowStore::new());
    for i in 0..(STORE_LIST_LOAD_CONCURRENCY * 2) {
        store
            .append(
                key(&Uuid::new_v4().to_string()),
                vec![user(format!("p{i}"), "2024-01-01T00:00:00Z")],
            )
            .await
            .unwrap();
    }

    let task_store = store.clone();
    let handle = tokio::spawn(async move {
        list_sessions_from_store(task_store.as_ref(), Some(Path::new(DIR)), None, 0)
            .await
            .unwrap()
    });

    for _ in 0..100 {
        if store.peak.load(Ordering::SeqCst) >= STORE_LIST_LOAD_CONCURRENCY {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store.peak.load(Ordering::SeqCst),
        STORE_LIST_LOAD_CONCURRENCY
    );
    store.released.store(true, Ordering::SeqCst);
    store.gate.notify_waiters();

    let sessions = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("list_sessions_from_store timed out")
        .expect("task panicked");
    assert_eq!(sessions.len(), STORE_LIST_LOAD_CONCURRENCY * 2);
}
