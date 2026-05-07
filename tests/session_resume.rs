use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
#[cfg(unix)]
use claude_agent_sdk::internal::session_resume::MaterializedResume;
use claude_agent_sdk::internal::session_resume::materialize_resume_session;
use claude_agent_sdk::{
    ClaudeAgentOptions, ClaudeSdkClient, InMemorySessionStore, Prompt, SessionKey,
    SessionListSubkeysKey, SessionStore, SessionStoreEntry, SessionStoreListEntry, Transport,
    project_key_for_directory, query,
};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc};

const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const SESSION_ID_2: &str = "660e8400-e29b-41d4-a716-446655440000";

fn project_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    String,
) {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    let source_config = tmp.path().join("source_config");
    std::fs::create_dir(&source_config).unwrap();
    let project_key = project_key_for_directory(Some(&cwd));
    (tmp, cwd, source_config, project_key)
}

fn session_key(project_key: &str, session_id: &str) -> SessionKey {
    SessionKey {
        project_key: project_key.to_string(),
        session_id: session_id.to_string(),
        subpath: None,
    }
}

fn options_with_store(
    cwd: std::path::PathBuf,
    source_config: std::path::PathBuf,
    store: InMemorySessionStore,
    resume: Option<&str>,
    continue_conversation: bool,
) -> ClaudeAgentOptions {
    let mut options = ClaudeAgentOptions {
        cwd: Some(cwd),
        session_store: Some(Arc::new(store) as Arc<dyn SessionStore>),
        resume: resume.map(ToString::to_string),
        continue_conversation,
        ..ClaudeAgentOptions::default()
    };
    options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );
    options
}

fn read_jsonl(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn current_resume_dirs() -> HashSet<PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("claude-resume-"))
        })
        .collect()
}

async fn wait_for_paths_removed(paths: &[PathBuf]) -> bool {
    for _ in 0..20 {
        if paths.iter().all(|path| !path.exists()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    paths.iter().all(|path| !path.exists())
}

#[derive(Clone)]
struct AutoInitTransport {
    tx: mpsc::UnboundedSender<claude_agent_sdk::Result<Value>>,
    rx: Arc<std::sync::Mutex<Option<mpsc::UnboundedReceiver<claude_agent_sdk::Result<Value>>>>>,
}

impl AutoInitTransport {
    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(std::sync::Mutex::new(Some(rx))),
        }
    }
}

#[async_trait]
impl Transport for AutoInitTransport {
    async fn connect(&self) -> claude_agent_sdk::Result<()> {
        Ok(())
    }

    async fn write(&self, data: &str) -> claude_agent_sdk::Result<()> {
        let value = serde_json::from_str::<Value>(data.trim()).ok();
        if value
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(Value::as_str)
            == Some("control_request")
        {
            let value = value.unwrap();
            let request_id = value
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.tx
                .send(Ok(json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": {},
                    }
                })))
                .unwrap();
        }
        Ok(())
    }

    fn read_messages(&self) -> BoxStream<'static, claude_agent_sdk::Result<Value>> {
        let rx = self.rx.lock().unwrap().take();
        match rx {
            Some(mut rx) => Box::pin(async_stream::try_stream! {
                while let Some(item) = rx.recv().await {
                    yield item?;
                }
            }),
            None => Box::pin(stream::empty()),
        }
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

#[derive(Clone)]
struct FailingConnectTransport;

#[async_trait]
impl Transport for FailingConnectTransport {
    async fn connect(&self) -> claude_agent_sdk::Result<()> {
        Err(claude_agent_sdk::ClaudeSdkError::Runtime(
            "spawn failed".to_string(),
        ))
    }

    async fn write(&self, _data: &str) -> claude_agent_sdk::Result<()> {
        Ok(())
    }

    fn read_messages(&self) -> BoxStream<'static, claude_agent_sdk::Result<Value>> {
        Box::pin(stream::empty())
    }

    async fn close(&self) -> claude_agent_sdk::Result<()> {
        Ok(())
    }

    fn is_ready(&self) -> bool {
        false
    }

    async fn end_input(&self) -> claude_agent_sdk::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct CountingStore {
    load_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SessionStore for CountingStore {
    async fn append(
        &self,
        _key: SessionKey,
        _entries: Vec<SessionStoreEntry>,
    ) -> claude_agent_sdk::Result<()> {
        Ok(())
    }

    async fn load(
        &self,
        _key: SessionKey,
    ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
        self.load_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(vec![json!({"type": "user", "uuid": "u1"})]))
    }
}

#[tokio::test]
async fn materialize_resume_none_cases_match_python() {
    let (_tmp, cwd, source_config, project_key) = project_fixture();
    let store = InMemorySessionStore::new();

    let no_store = ClaudeAgentOptions {
        cwd: Some(cwd.clone()),
        resume: Some(SESSION_ID.to_string()),
        ..ClaudeAgentOptions::default()
    };
    assert!(
        materialize_resume_session(&no_store)
            .await
            .unwrap()
            .is_none()
    );

    let no_resume = options_with_store(
        cwd.clone(),
        source_config.clone(),
        store.clone(),
        None,
        false,
    );
    assert!(
        materialize_resume_session(&no_resume)
            .await
            .unwrap()
            .is_none()
    );

    let invalid_resume = options_with_store(
        cwd.clone(),
        source_config.clone(),
        store.clone(),
        Some("../../etc/passwd"),
        false,
    );
    assert!(
        materialize_resume_session(&invalid_resume)
            .await
            .unwrap()
            .is_none()
    );

    let missing = options_with_store(
        cwd.clone(),
        source_config.clone(),
        store.clone(),
        Some(SESSION_ID),
        false,
    );
    assert!(
        materialize_resume_session(&missing)
            .await
            .unwrap()
            .is_none()
    );

    store
        .append(session_key(&project_key, SESSION_ID), vec![])
        .await
        .unwrap();
    let empty = options_with_store(cwd, source_config, store, Some(SESSION_ID), false);
    assert!(materialize_resume_session(&empty).await.unwrap().is_none());
}

#[tokio::test]
async fn materialize_resume_writes_jsonl_credentials_and_cleanup_removes_dir() {
    let (_tmp, cwd, source_config, project_key) = project_fixture();
    std::fs::write(
        source_config.join(".credentials.json"),
        json!({"claudeAiOauth": {"accessToken": "at", "refreshToken": "SECRET"}}).to_string(),
    )
    .unwrap();
    std::fs::write(source_config.join(".claude.json"), r#"{"theme":"dark"}"#).unwrap();

    let store = InMemorySessionStore::new();
    let entries = vec![
        json!({"type": "user", "uuid": "u1", "message": {"role": "user", "content": "hi"}}),
        json!({"type": "assistant", "uuid": "a1"}),
    ];
    store
        .append(session_key(&project_key, SESSION_ID), entries.clone())
        .await
        .unwrap();
    let options = options_with_store(cwd, source_config, store, Some(SESSION_ID), false);

    let materialized = materialize_resume_session(&options)
        .await
        .unwrap()
        .expect("session should materialize");
    assert_eq!(materialized.resume_session_id, SESSION_ID);
    let jsonl = materialized
        .config_dir
        .join("projects")
        .join(&project_key)
        .join(format!("{SESSION_ID}.jsonl"));
    assert_eq!(read_jsonl(&jsonl), entries);

    let creds = serde_json::from_str::<Value>(
        &std::fs::read_to_string(materialized.config_dir.join(".credentials.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "at");
    assert!(creds["claudeAiOauth"].get("refreshToken").is_none());
    assert_eq!(
        std::fs::read_to_string(materialized.config_dir.join(".claude.json")).unwrap(),
        r#"{"theme":"dark"}"#
    );

    let config_dir = materialized.config_dir.clone();
    materialized.cleanup().await;
    assert!(!config_dir.exists());
}

#[tokio::test]
async fn custom_transport_skips_resume_materialization_for_client_and_query() {
    let (_tmp, cwd, _source_config, _project_key) = project_fixture();
    let load_calls = Arc::new(AtomicUsize::new(0));
    let options = ClaudeAgentOptions {
        cwd: Some(cwd),
        session_store: Some(Arc::new(CountingStore {
            load_calls: load_calls.clone(),
        }) as Arc<dyn SessionStore>),
        resume: Some(SESSION_ID.to_string()),
        ..ClaudeAgentOptions::default()
    };

    let transport = AutoInitTransport::new();
    let mut client = ClaudeSdkClient::new(Some(options.clone()), Some(Arc::new(transport)));
    client.connect(None).await.unwrap();
    assert_eq!(load_calls.load(Ordering::SeqCst), 0);
    client.disconnect().await.unwrap();

    let mut stream = query(
        Prompt::Text("hi".to_string()),
        Some(options),
        Some(Arc::new(FailingConnectTransport)),
    );
    let err = stream.next().await.unwrap().unwrap_err();
    assert!(err.to_string().contains("spawn failed"));
    assert_eq!(load_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn continue_materialization_picks_recent_main_session_and_is_deterministic_on_ties() {
    let (_tmp, cwd, source_config, project_key) = project_fixture();
    let store = InMemorySessionStore::new();
    store
        .append(
            session_key(&project_key, SESSION_ID),
            vec![json!({"type": "user", "uuid": "old"})],
        )
        .await
        .unwrap();
    store
        .append(
            session_key(&project_key, SESSION_ID_2),
            vec![json!({"type": "user", "uuid": "new"})],
        )
        .await
        .unwrap();

    let options = options_with_store(cwd.clone(), source_config.clone(), store, None, true);
    let materialized = materialize_resume_session(&options)
        .await
        .unwrap()
        .expect("latest session should materialize");
    assert_eq!(materialized.resume_session_id, SESSION_ID_2);
    materialized.cleanup().await;

    #[derive(Default)]
    struct TieStore;

    #[async_trait]
    impl SessionStore for TieStore {
        async fn append(
            &self,
            _key: SessionKey,
            _entries: Vec<SessionStoreEntry>,
        ) -> claude_agent_sdk::Result<()> {
            Ok(())
        }

        async fn load(
            &self,
            key: SessionKey,
        ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
            Ok(Some(vec![
                json!({"type": "user", "uuid": format!("u-{}", key.session_id)}),
            ]))
        }

        async fn list_sessions(
            &self,
            _project_key: String,
        ) -> claude_agent_sdk::Result<Vec<SessionStoreListEntry>> {
            Ok(vec![
                SessionStoreListEntry {
                    session_id: SESSION_ID.to_string(),
                    mtime: 5000,
                },
                SessionStoreListEntry {
                    session_id: SESSION_ID_2.to_string(),
                    mtime: 5000,
                },
            ])
        }
    }

    let mut tie_options = ClaudeAgentOptions {
        cwd: Some(cwd),
        session_store: Some(Arc::new(TieStore) as Arc<dyn SessionStore>),
        continue_conversation: true,
        ..ClaudeAgentOptions::default()
    };
    tie_options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );
    let first = materialize_resume_session(&tie_options)
        .await
        .unwrap()
        .unwrap();
    let first_id = first.resume_session_id.clone();
    first.cleanup().await;
    let second = materialize_resume_session(&tie_options)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.resume_session_id, first_id);
    second.cleanup().await;
}

#[tokio::test]
async fn continue_materialization_skips_sidechain_sessions() {
    let (_tmp, cwd, source_config, project_key) = project_fixture();
    let store = InMemorySessionStore::new();
    let sidechain_sid = uuid::Uuid::new_v4().to_string();
    store
        .append(
            session_key(&project_key, SESSION_ID),
            vec![json!({"type": "user", "uuid": "main"})],
        )
        .await
        .unwrap();
    store
        .append(
            session_key(&project_key, &sidechain_sid),
            vec![json!({"type": "user", "uuid": "sc", "isSidechain": true})],
        )
        .await
        .unwrap();
    let options = options_with_store(cwd, source_config, store, None, true);
    let materialized = materialize_resume_session(&options)
        .await
        .unwrap()
        .expect("main session should materialize");
    assert_eq!(materialized.resume_session_id, SESSION_ID);
    materialized.cleanup().await;
}

#[tokio::test]
async fn materializes_subagent_jsonl_and_meta_by_appending_jsonl_suffix() {
    let (_tmp, cwd, source_config, project_key) = project_fixture();

    let store = InMemorySessionStore::new();
    store
        .append(
            SessionKey {
                project_key: project_key.clone(),
                session_id: SESSION_ID.to_string(),
                subpath: None,
            },
            vec![json!({"type": "user", "uuid": "u1"})],
        )
        .await
        .unwrap();
    store
        .append(
            SessionKey {
                project_key: project_key.clone(),
                session_id: SESSION_ID.to_string(),
                subpath: Some("subagents/agent.v1".to_string()),
            },
            vec![
                json!({"type": "user", "uuid": "su1"}),
                json!({"type": "assistant", "uuid": "sa1"}),
                json!({"type": "agent_metadata", "agentType": "general", "ver": 1}),
            ],
        )
        .await
        .unwrap();

    let mut options = ClaudeAgentOptions {
        cwd: Some(cwd),
        session_store: Some(Arc::new(store) as Arc<dyn SessionStore>),
        resume: Some(SESSION_ID.to_string()),
        ..ClaudeAgentOptions::default()
    };
    options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );

    let materialized = materialize_resume_session(&options)
        .await
        .unwrap()
        .expect("session should materialize");
    let session_dir = materialized
        .config_dir
        .join("projects")
        .join(project_key)
        .join(SESSION_ID);
    let jsonl = session_dir.join("subagents").join("agent.v1.jsonl");
    let legacy_wrong_jsonl = session_dir.join("subagents").join("agent.jsonl");
    let meta = session_dir.join("subagents").join("agent.v1.meta.json");

    assert!(jsonl.is_file());
    assert!(!legacy_wrong_jsonl.exists());
    let lines = std::fs::read_to_string(jsonl)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        lines,
        vec![
            json!({"type": "user", "uuid": "su1"}),
            json!({"type": "assistant", "uuid": "sa1"})
        ]
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(meta).unwrap()).unwrap(),
        json!({"agentType": "general", "ver": 1})
    );

    materialized.cleanup().await;
}

#[tokio::test]
async fn subkey_materialization_rejects_unsafe_paths_and_skips_without_list_subkeys() {
    let (_tmp, cwd, source_config, project_key) = project_fixture();

    struct EvilStore;

    #[async_trait]
    impl SessionStore for EvilStore {
        async fn append(
            &self,
            _key: SessionKey,
            _entries: Vec<SessionStoreEntry>,
        ) -> claude_agent_sdk::Result<()> {
            Ok(())
        }

        async fn load(
            &self,
            key: SessionKey,
        ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
            match key.subpath.as_deref() {
                None => Ok(Some(vec![json!({"type": "user", "uuid": "main"})])),
                Some("subagents/agent-ok") => Ok(Some(vec![json!({"type": "user", "uuid": "ok"})])),
                Some(other) => panic!("loaded unsafe subpath {other}"),
            }
        }

        async fn list_subkeys(
            &self,
            _key: SessionListSubkeysKey,
        ) -> claude_agent_sdk::Result<Vec<String>> {
            Ok(vec![
                "".to_string(),
                ".".to_string(),
                "./".to_string(),
                "a/.".to_string(),
                "subagents/.".to_string(),
                "/etc/passwd".to_string(),
                "../escape".to_string(),
                "a/../b".to_string(),
                "C:escape".to_string(),
                "C:\\abs".to_string(),
                "subagents/agent\0x".to_string(),
                "subagents/agent-ok".to_string(),
            ])
        }

        fn supports_list_subkeys(&self) -> bool {
            true
        }
    }

    let mut options = ClaudeAgentOptions {
        cwd: Some(cwd.clone()),
        session_store: Some(Arc::new(EvilStore) as Arc<dyn SessionStore>),
        resume: Some(SESSION_ID.to_string()),
        ..ClaudeAgentOptions::default()
    };
    options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );
    let materialized = materialize_resume_session(&options).await.unwrap().unwrap();
    let session_dir = materialized
        .config_dir
        .join("projects")
        .join(&project_key)
        .join(SESSION_ID);
    assert!(
        session_dir
            .join("subagents")
            .join("agent-ok.jsonl")
            .is_file()
    );
    let main_jsonl = materialized
        .config_dir
        .join("projects")
        .join(&project_key)
        .join(format!("{SESSION_ID}.jsonl"));
    assert_eq!(
        read_jsonl(&main_jsonl),
        vec![json!({"type": "user", "uuid": "main"})]
    );
    materialized.cleanup().await;

    #[derive(Default)]
    struct MinimalStore;

    #[async_trait]
    impl SessionStore for MinimalStore {
        async fn append(
            &self,
            _key: SessionKey,
            _entries: Vec<SessionStoreEntry>,
        ) -> claude_agent_sdk::Result<()> {
            Ok(())
        }

        async fn load(
            &self,
            _key: SessionKey,
        ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
            Ok(Some(vec![json!({"type": "user", "uuid": "u1"})]))
        }
    }

    let mut options = ClaudeAgentOptions {
        cwd: Some(cwd),
        session_store: Some(Arc::new(MinimalStore) as Arc<dyn SessionStore>),
        resume: Some(SESSION_ID.to_string()),
        ..ClaudeAgentOptions::default()
    };
    options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );
    let materialized = materialize_resume_session(&options).await.unwrap().unwrap();
    assert!(
        materialized
            .config_dir
            .join("projects")
            .join(&project_key)
            .join(format!("{SESSION_ID}.jsonl"))
            .is_file()
    );
    assert!(
        !materialized
            .config_dir
            .join("projects")
            .join(&project_key)
            .join(SESSION_ID)
            .exists()
    );
    materialized.cleanup().await;
}

#[tokio::test]
async fn materialize_resume_timeouts_and_store_errors_are_contextual() {
    let (_tmp, cwd, source_config, _project_key) = project_fixture();

    struct SlowLoadStore;

    #[async_trait]
    impl SessionStore for SlowLoadStore {
        async fn append(
            &self,
            _key: SessionKey,
            _entries: Vec<SessionStoreEntry>,
        ) -> claude_agent_sdk::Result<()> {
            Ok(())
        }

        async fn load(
            &self,
            _key: SessionKey,
        ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(None)
        }
    }

    let mut options = ClaudeAgentOptions {
        cwd: Some(cwd.clone()),
        session_store: Some(Arc::new(SlowLoadStore) as Arc<dyn SessionStore>),
        resume: Some(SESSION_ID.to_string()),
        load_timeout_ms: 50,
        ..ClaudeAgentOptions::default()
    };
    options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );
    let err = materialize_resume_session(&options).await.unwrap_err();
    assert!(err.to_string().contains("timed out"));
    assert!(err.to_string().contains("SessionStore.load()"));

    struct HungListStore;

    #[async_trait]
    impl SessionStore for HungListStore {
        async fn append(
            &self,
            _key: SessionKey,
            _entries: Vec<SessionStoreEntry>,
        ) -> claude_agent_sdk::Result<()> {
            Ok(())
        }

        async fn load(
            &self,
            _key: SessionKey,
        ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
            Ok(None)
        }

        async fn list_sessions(
            &self,
            _project_key: String,
        ) -> claude_agent_sdk::Result<Vec<SessionStoreListEntry>> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Vec::new())
        }
    }

    let mut options = ClaudeAgentOptions {
        cwd: Some(cwd),
        session_store: Some(Arc::new(HungListStore) as Arc<dyn SessionStore>),
        continue_conversation: true,
        load_timeout_ms: 50,
        ..ClaudeAgentOptions::default()
    };
    options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );
    let err = materialize_resume_session(&options).await.unwrap_err();
    assert!(err.to_string().contains("list_sessions()"));
    assert!(err.to_string().contains("timed out"));
}

#[tokio::test]
async fn materialize_resume_subkey_failure_cleans_temp_dir() {
    let (_tmp, cwd, source_config, _project_key) = project_fixture();
    let before = current_resume_dirs();
    let seen_dirs = Arc::new(Mutex::new(Vec::<PathBuf>::new()));

    struct FailLateStore {
        before: HashSet<PathBuf>,
        seen_dirs: Arc<Mutex<Vec<PathBuf>>>,
    }

    #[async_trait]
    impl SessionStore for FailLateStore {
        async fn append(
            &self,
            _key: SessionKey,
            _entries: Vec<SessionStoreEntry>,
        ) -> claude_agent_sdk::Result<()> {
            Ok(())
        }

        async fn load(
            &self,
            _key: SessionKey,
        ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
            Ok(Some(vec![json!({"type": "user", "uuid": "u1"})]))
        }

        async fn list_subkeys(
            &self,
            _key: SessionListSubkeysKey,
        ) -> claude_agent_sdk::Result<Vec<String>> {
            let created = current_resume_dirs()
                .difference(&self.before)
                .cloned()
                .collect::<Vec<_>>();
            *self.seen_dirs.lock().await = created;
            Err(claude_agent_sdk::ClaudeSdkError::Runtime(
                "boom".to_string(),
            ))
        }

        fn supports_list_subkeys(&self) -> bool {
            true
        }
    }

    let mut options = ClaudeAgentOptions {
        cwd: Some(cwd),
        session_store: Some(Arc::new(FailLateStore {
            before,
            seen_dirs: seen_dirs.clone(),
        }) as Arc<dyn SessionStore>),
        resume: Some(SESSION_ID.to_string()),
        ..ClaudeAgentOptions::default()
    };
    options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );

    let err = materialize_resume_session(&options).await.unwrap_err();
    assert!(err.to_string().contains("list_subkeys()"));
    assert!(err.to_string().contains("boom"));

    let seen = seen_dirs.lock().await.clone();
    assert!(
        !seen.is_empty(),
        "materialization did not create a temp dir"
    );
    assert!(
        wait_for_paths_removed(&seen).await,
        "leaked resume temp dirs: {seen:?}"
    );
}

#[tokio::test]
async fn materialize_resume_cancelled_after_temp_creation_cleans_temp_dir() {
    let (_tmp, cwd, source_config, _project_key) = project_fixture();
    let before = current_resume_dirs();
    let entered_subkeys = Arc::new(Notify::new());
    let release_subkeys = Arc::new(Notify::new());

    struct HungSubkeysStore {
        entered_subkeys: Arc<Notify>,
        release_subkeys: Arc<Notify>,
    }

    #[async_trait]
    impl SessionStore for HungSubkeysStore {
        async fn append(
            &self,
            _key: SessionKey,
            _entries: Vec<SessionStoreEntry>,
        ) -> claude_agent_sdk::Result<()> {
            Ok(())
        }

        async fn load(
            &self,
            _key: SessionKey,
        ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
            Ok(Some(vec![json!({"type": "user", "uuid": "u1"})]))
        }

        async fn list_subkeys(
            &self,
            _key: SessionListSubkeysKey,
        ) -> claude_agent_sdk::Result<Vec<String>> {
            self.entered_subkeys.notify_one();
            self.release_subkeys.notified().await;
            Ok(Vec::new())
        }

        fn supports_list_subkeys(&self) -> bool {
            true
        }
    }

    let mut options = ClaudeAgentOptions {
        cwd: Some(cwd),
        session_store: Some(Arc::new(HungSubkeysStore {
            entered_subkeys: entered_subkeys.clone(),
            release_subkeys,
        }) as Arc<dyn SessionStore>),
        resume: Some(SESSION_ID.to_string()),
        ..ClaudeAgentOptions::default()
    };
    options.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        source_config.to_string_lossy().to_string(),
    );

    let task = tokio::spawn(async move { materialize_resume_session(&options).await });
    tokio::time::timeout(Duration::from_secs(1), entered_subkeys.notified())
        .await
        .expect("list_subkeys was not reached");

    let created = current_resume_dirs()
        .difference(&before)
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        !created.is_empty(),
        "materialization did not create a temp dir"
    );

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        wait_for_paths_removed(&created).await,
        "leaked resume temp dirs after cancellation: {created:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn materialized_resume_cleanup_retries_transient_permission_errors() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join("claude-resume-held");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(config_dir.join(".credentials.json"), "secret").unwrap();
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let chmod_dir = config_dir.clone();
    let chmod_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = std::fs::set_permissions(&chmod_dir, std::fs::Permissions::from_mode(0o700));
    });

    let materialized = MaterializedResume {
        config_dir: config_dir.clone(),
        resume_session_id: SESSION_ID.to_string(),
    };
    materialized.cleanup().await;
    chmod_task.await.unwrap();
    if config_dir.exists() {
        let _ = std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700));
    }
    assert!(!config_dir.exists());
}
