use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use claude_agent_sdk::internal::session_store_validation::validate_session_store_options;
use claude_agent_sdk::internal::sessions::{
    MAX_SANITIZED_LENGTH, canonicalize_path, sanitize_path, simple_hash,
};
use claude_agent_sdk::testing::run_session_store_conformance;
use claude_agent_sdk::{
    ClaudeAgentOptions, InMemorySessionStore, SessionKey, SessionStore, SessionStoreEntry,
    SessionStoreFlushMode, project_key_for_directory,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

fn key() -> SessionKey {
    SessionKey {
        project_key: "proj".to_string(),
        session_id: "sess".to_string(),
        subpath: None,
    }
}

#[derive(Default)]
struct MinimalStore {
    data: Mutex<HashMap<String, Vec<Value>>>,
}

impl MinimalStore {
    fn key_to_string(key: &SessionKey) -> String {
        format!(
            "{}/{}/{}",
            key.project_key,
            key.session_id,
            key.subpath.as_deref().unwrap_or("")
        )
    }
}

#[async_trait]
impl SessionStore for MinimalStore {
    async fn append(
        &self,
        key: SessionKey,
        entries: Vec<SessionStoreEntry>,
    ) -> claude_agent_sdk::Result<()> {
        self.data
            .lock()
            .await
            .entry(Self::key_to_string(&key))
            .or_default()
            .extend(entries);
        Ok(())
    }

    async fn load(
        &self,
        key: SessionKey,
    ) -> claude_agent_sdk::Result<Option<Vec<SessionStoreEntry>>> {
        Ok(self
            .data
            .lock()
            .await
            .get(&Self::key_to_string(&key))
            .cloned())
    }
}

#[tokio::test]
async fn in_memory_store_passes_conformance_and_minimal_store_auto_skips_optionals() {
    run_session_store_conformance(
        || Arc::new(InMemorySessionStore::new()) as Arc<dyn SessionStore>,
        HashSet::new(),
    )
    .await
    .unwrap();

    run_session_store_conformance(
        || Arc::new(MinimalStore::default()) as Arc<dyn SessionStore>,
        HashSet::new(),
    )
    .await
    .unwrap();

    run_session_store_conformance(
        || Arc::new(MinimalStore::default()) as Arc<dyn SessionStore>,
        ["list_sessions", "delete", "list_subkeys"]
            .into_iter()
            .collect(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn in_memory_helpers_return_copies_count_main_sessions_and_clear() {
    let store = InMemorySessionStore::new();
    assert!(store.get_entries(key()).await.is_empty());
    store
        .append(key(), vec![json!({"n": 1}), json!({"n": 2})])
        .await
        .unwrap();
    let mut entries = store.get_entries(key()).await;
    entries.push(json!({"n": 999}));
    assert_eq!(
        store.get_entries(key()).await,
        vec![json!({"n": 1}), json!({"n": 2})]
    );

    let mut loaded = store.load(key()).await.unwrap().unwrap();
    loaded.push(json!({"n": 999}));
    assert_eq!(
        store.load(key()).await.unwrap(),
        Some(vec![json!({"n": 1}), json!({"n": 2})])
    );

    let size_store = InMemorySessionStore::new();
    size_store
        .append(
            SessionKey {
                project_key: "p".to_string(),
                session_id: "a".to_string(),
                subpath: None,
            },
            vec![json!({"n": 1})],
        )
        .await
        .unwrap();
    size_store
        .append(
            SessionKey {
                project_key: "p".to_string(),
                session_id: "b".to_string(),
                subpath: None,
            },
            vec![json!({"n": 1})],
        )
        .await
        .unwrap();
    size_store
        .append(
            SessionKey {
                project_key: "p".to_string(),
                session_id: "a".to_string(),
                subpath: Some("sub/x".to_string()),
            },
            vec![json!({"n": 1})],
        )
        .await
        .unwrap();
    assert_eq!(size_store.size().await, 2);

    store.clear().await;
    assert_eq!(store.size().await, 0);
    assert!(store.load(key()).await.unwrap().is_none());
    assert!(
        store
            .list_sessions("proj".to_string())
            .await
            .unwrap()
            .is_empty()
    );
}

#[test]
fn session_store_options_validation_matches_python_contracts() {
    validate_session_store_options(&ClaudeAgentOptions {
        continue_conversation: true,
        enable_file_checkpointing: true,
        ..ClaudeAgentOptions::default()
    })
    .unwrap();

    validate_session_store_options(&ClaudeAgentOptions {
        session_store: Some(Arc::new(InMemorySessionStore::new())),
        ..ClaudeAgentOptions::default()
    })
    .unwrap();

    let err = validate_session_store_options(&ClaudeAgentOptions {
        session_store: Some(Arc::new(MinimalStore::default())),
        continue_conversation: true,
        ..ClaudeAgentOptions::default()
    })
    .unwrap_err();
    assert!(err.to_string().contains("list_sessions"));

    validate_session_store_options(&ClaudeAgentOptions {
        session_store: Some(Arc::new(MinimalStore::default())),
        continue_conversation: true,
        resume: Some("00000000-0000-4000-8000-000000000000".to_string()),
        ..ClaudeAgentOptions::default()
    })
    .unwrap();

    let err = validate_session_store_options(&ClaudeAgentOptions {
        session_store: Some(Arc::new(InMemorySessionStore::new())),
        enable_file_checkpointing: true,
        session_store_flush: SessionStoreFlushMode::Batched,
        ..ClaudeAgentOptions::default()
    })
    .unwrap_err();
    assert!(err.to_string().contains("enable_file_checkpointing"));
}

#[test]
fn project_key_for_directory_matches_python_normalization_contracts() {
    assert_eq!(
        project_key_for_directory(None),
        project_key_for_directory(Some(Path::new(".")))
    );

    let sanitized = project_key_for_directory(Some(Path::new("/tmp/my project!")));
    assert!(!sanitized.contains('/'));
    assert!(!sanitized.contains(' '));
    assert!(!sanitized.contains('!'));
    assert_eq!(
        project_key_for_directory(Some(Path::new("/a/b/c"))),
        project_key_for_directory(Some(Path::new("/a/b/c")))
    );

    let relative_key = project_key_for_directory(Some(Path::new(".")));
    assert_eq!(relative_key, sanitize_path(&canonicalize_path(".")));
    assert_ne!(relative_key, sanitize_path("."));

    let tmp = tempfile::tempdir().unwrap();
    let nfc = tmp.path().join("café");
    let nfd = tmp.path().join("cafe\u{301}");
    std::fs::create_dir_all(&nfc).unwrap();
    assert_eq!(
        project_key_for_directory(Some(&nfc)),
        project_key_for_directory(Some(&nfd))
    );

    let long_dir = format!("/{}", "a".repeat(MAX_SANITIZED_LENGTH + 50));
    let long_key = project_key_for_directory(Some(Path::new(&long_dir)));
    assert!(long_key.ends_with(&format!("-{}", simple_hash(&long_dir))));
    assert!(long_key.len() > MAX_SANITIZED_LENGTH);
}
