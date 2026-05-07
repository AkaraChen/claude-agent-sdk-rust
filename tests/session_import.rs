use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use claude_agent_sdk::{
    InMemorySessionStore, SessionKey, SessionListSubkeysKey, SessionStore, SessionStoreEntry,
    SessionStoreListEntry, SessionSummaryEntry, file_path_to_session_key, import_session_to_store,
    project_key_for_directory,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Mutex, MutexGuard};

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        // The test holds ENV_LOCK while mutating process-wide environment.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // The test holds ENV_LOCK until this guard is dropped.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

struct CwdGuard {
    previous: PathBuf,
}

impl CwdGuard {
    fn set(path: &Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self { previous }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous).unwrap();
    }
}

struct Fixture {
    _env_lock: MutexGuard<'static, ()>,
    _env: EnvVarGuard,
    _tmp: TempDir,
    cwd: PathBuf,
    project_key: String,
    claude_dir: PathBuf,
}

async fn fixture() -> Fixture {
    let env_lock = ENV_LOCK.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    let project_key = project_key_for_directory(Some(&cwd));
    let config = tmp.path().join("claude_config");
    let claude_dir = config.join("projects").join(&project_key);
    std::fs::create_dir_all(&claude_dir).unwrap();
    let env = EnvVarGuard::set_path("CLAUDE_CONFIG_DIR", &config);
    Fixture {
        _env_lock: env_lock,
        _env: env,
        _tmp: tmp,
        cwd,
        project_key,
        claude_dir,
    }
}

fn entry(i: i64) -> Value {
    json!({
        "type": "user",
        "uuid": format!("u{i}"),
        "timestamp": format!("2026-01-01T00:00:{i:02}Z"),
    })
}

fn write_jsonl(path: &Path, entries: &[Value]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let content = entries
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(path, content).unwrap();
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

#[tokio::test]
async fn imports_main_transcript() {
    let fixture = fixture().await;
    let entries = (0..7).map(entry).collect::<Vec<_>>();
    write_jsonl(
        &fixture.claude_dir.join(format!("{SESSION_ID}.jsonl")),
        &entries,
    );

    let store = InMemorySessionStore::new();
    import_session_to_store(SESSION_ID, &store, Some(&fixture.cwd), true, None)
        .await
        .unwrap();

    let key = SessionKey {
        project_key: fixture.project_key,
        session_id: SESSION_ID.to_string(),
        subpath: None,
    };
    assert_eq!(store.get_entries(key).await, entries);
}

#[tokio::test]
async fn batches_append_per_chunk_and_skips_blank_lines() {
    let fixture = fixture().await;
    let entries = (0..5).map(entry).collect::<Vec<_>>();
    let path = fixture.claude_dir.join(format!("{SESSION_ID}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n\n{}\n{}\n{}\n{}\n",
            entries[0], entries[1], entries[2], entries[3], entries[4]
        ),
    )
    .unwrap();

    let store = RecordingStore::default();
    import_session_to_store(SESSION_ID, &store, Some(&fixture.cwd), true, Some(2))
        .await
        .unwrap();

    let key = SessionKey {
        project_key: fixture.project_key,
        session_id: SESSION_ID.to_string(),
        subpath: None,
    };
    let calls = store.calls.lock().await.clone();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0], (key.clone(), entries[0..2].to_vec()));
    assert_eq!(calls[1], (key.clone(), entries[2..4].to_vec()));
    assert_eq!(calls[2], (key, entries[4..5].to_vec()));
}

#[tokio::test]
async fn imports_subagent_transcript_and_metadata() {
    let fixture = fixture().await;
    write_jsonl(
        &fixture.claude_dir.join(format!("{SESSION_ID}.jsonl")),
        &[entry(0)],
    );
    let sub_dir = fixture.claude_dir.join(SESSION_ID).join("subagents");
    write_jsonl(&sub_dir.join("agent-abc.jsonl"), &[entry(10)]);
    std::fs::write(
        sub_dir.join("agent-abc.meta.json"),
        json!({"agentType": "coder", "worktreePath": "/tmp/wt"}).to_string(),
    )
    .unwrap();

    let store = InMemorySessionStore::new();
    import_session_to_store(SESSION_ID, &store, Some(&fixture.cwd), true, None)
        .await
        .unwrap();

    let sub_key = SessionKey {
        project_key: fixture.project_key.clone(),
        session_id: SESSION_ID.to_string(),
        subpath: Some("subagents/agent-abc".to_string()),
    };
    assert_eq!(
        store.get_entries(sub_key.clone()).await,
        vec![
            entry(10),
            json!({"type": "agent_metadata", "agentType": "coder", "worktreePath": "/tmp/wt"})
        ]
    );
    assert_eq!(
        store
            .list_subkeys(SessionListSubkeysKey {
                project_key: fixture.project_key,
                session_id: SESSION_ID.to_string(),
            })
            .await
            .unwrap(),
        vec!["subagents/agent-abc".to_string()]
    );
}

#[tokio::test]
async fn imports_nested_subagents_and_keys_match_file_path_to_session_key() {
    let fixture = fixture().await;
    let main_file = fixture.claude_dir.join(format!("{SESSION_ID}.jsonl"));
    write_jsonl(&main_file, &[entry(0)]);
    let nested_file = fixture
        .claude_dir
        .join(SESSION_ID)
        .join("subagents")
        .join("workflows")
        .join("run-1")
        .join("agent-def.jsonl");
    write_jsonl(&nested_file, &[entry(11)]);

    let store = InMemorySessionStore::new();
    import_session_to_store(SESSION_ID, &store, Some(&fixture.cwd), true, None)
        .await
        .unwrap();

    let projects_dir = fixture.claude_dir.parent().unwrap();
    let expected_main = file_path_to_session_key(&main_file, projects_dir).unwrap();
    let expected_sub = file_path_to_session_key(&nested_file, projects_dir).unwrap();
    assert_eq!(store.get_entries(expected_main).await, vec![entry(0)]);
    assert_eq!(store.get_entries(expected_sub).await, vec![entry(11)]);

    let mut subkeys = store
        .list_subkeys(SessionListSubkeysKey {
            project_key: fixture.project_key,
            session_id: SESSION_ID.to_string(),
        })
        .await
        .unwrap();
    subkeys.sort();
    assert_eq!(
        subkeys,
        vec!["subagents/workflows/run-1/agent-def".to_string()]
    );
}

#[tokio::test]
async fn directory_none_keys_from_resolved_path_not_current_working_directory() {
    let fixture = fixture().await;
    write_jsonl(
        &fixture.claude_dir.join(format!("{SESSION_ID}.jsonl")),
        &[entry(0)],
    );
    let elsewhere = fixture._tmp.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let _cwd = CwdGuard::set(&elsewhere);
    assert_ne!(project_key_for_directory(None), fixture.project_key);

    let store = InMemorySessionStore::new();
    import_session_to_store(SESSION_ID, &store, None, true, None)
        .await
        .unwrap();

    let key = SessionKey {
        project_key: fixture.project_key,
        session_id: SESSION_ID.to_string(),
        subpath: None,
    };
    assert_eq!(store.get_entries(key).await, vec![entry(0)]);
}

#[tokio::test]
async fn invalid_subagent_metadata_sidecar_errors_like_python() {
    let fixture = fixture().await;
    write_jsonl(
        &fixture.claude_dir.join(format!("{SESSION_ID}.jsonl")),
        &[entry(0)],
    );
    let sub_dir = fixture.claude_dir.join(SESSION_ID).join("subagents");
    write_jsonl(&sub_dir.join("agent-bad.jsonl"), &[entry(10)]);
    std::fs::write(sub_dir.join("agent-bad.meta.json"), "{not json").unwrap();

    let store = InMemorySessionStore::new();
    let err = import_session_to_store(SESSION_ID, &store, Some(&fixture.cwd), true, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("key must be a string"));
}

#[tokio::test]
async fn include_subagents_false_skips_subagents() {
    let fixture = fixture().await;
    write_jsonl(
        &fixture.claude_dir.join(format!("{SESSION_ID}.jsonl")),
        &[entry(0)],
    );
    write_jsonl(
        &fixture
            .claude_dir
            .join(SESSION_ID)
            .join("subagents")
            .join("agent-abc.jsonl"),
        &[entry(10)],
    );

    let store = InMemorySessionStore::new();
    import_session_to_store(SESSION_ID, &store, Some(&fixture.cwd), false, None)
        .await
        .unwrap();

    assert!(
        store
            .list_subkeys(SessionListSubkeysKey {
                project_key: fixture.project_key,
                session_id: SESSION_ID.to_string(),
            })
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_uuid_and_missing_session_return_errors() {
    let fixture = fixture().await;
    let store = InMemorySessionStore::new();

    let invalid =
        import_session_to_store("../../etc/passwd", &store, Some(&fixture.cwd), true, None)
            .await
            .unwrap_err();
    assert!(invalid.to_string().contains("Invalid session_id"));

    let missing = import_session_to_store(SESSION_ID, &store, Some(&fixture.cwd), true, None)
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("not found"));
}
