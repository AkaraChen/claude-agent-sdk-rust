use async_trait::async_trait;
use claude_agent_sdk::{
    ClaudeSdkError, InMemorySessionStore, SessionKey, SessionStore, SessionStoreEntry,
    SessionStoreListEntry, delete_session_via_store, fork_session_via_store,
    get_session_info_from_store, get_session_messages_from_store, get_subagent_messages_from_store,
    list_sessions_from_store, list_subagents_from_store, project_key_for_directory,
    rename_session_via_store, tag_session_via_store,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use tokio::sync::Mutex;
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

fn subkey(session_id: &str, subpath: &str) -> SessionKey {
    SessionKey {
        project_key: project_key(),
        session_id: session_id.to_string(),
        subpath: Some(subpath.to_string()),
    }
}

fn user(text: &str, uuid: &str, parent_uuid: Option<&str>, session_id: &str) -> Value {
    json!({
        "type": "user",
        "uuid": uuid,
        "parentUuid": parent_uuid,
        "sessionId": session_id,
        "timestamp": "2024-01-01T00:00:00.000Z",
        "message": {"role": "user", "content": text},
    })
}

fn assistant(text: &str, uuid: &str, parent_uuid: &str, session_id: &str) -> Value {
    json!({
        "type": "assistant",
        "uuid": uuid,
        "parentUuid": parent_uuid,
        "sessionId": session_id,
        "timestamp": "2024-01-01T00:00:01.000Z",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
    })
}

async fn seed_chain(store: &InMemorySessionStore, session_id: &str, n: usize) -> Vec<String> {
    let mut uuids = Vec::new();
    let mut entries = Vec::new();
    let mut parent: Option<String> = None;
    for i in 0..n {
        let user_uuid = Uuid::new_v4().to_string();
        let assistant_uuid = Uuid::new_v4().to_string();
        entries.push(user(
            &format!("prompt {i}"),
            &user_uuid,
            parent.as_deref(),
            session_id,
        ));
        entries.push(assistant(
            &format!("reply {i}"),
            &assistant_uuid,
            &user_uuid,
            session_id,
        ));
        uuids.push(user_uuid);
        uuids.push(assistant_uuid.clone());
        parent = Some(assistant_uuid);
    }
    store.append(key(session_id), entries).await.unwrap();
    uuids
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
async fn store_read_helpers_list_info_messages_and_degrade_bad_rows() {
    #[derive(Default)]
    struct FlakeyStore {
        inner: InMemorySessionStore,
        bad_sid: Mutex<Option<String>>,
    }

    #[async_trait]
    impl SessionStore for FlakeyStore {
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
            if self.bad_sid.lock().await.as_deref() == Some(key.session_id.as_str()) {
                return Err(ClaudeSdkError::Runtime("backend down".to_string()));
            }
            self.inner.load(key).await
        }

        async fn list_sessions(
            &self,
            project_key: String,
        ) -> claude_agent_sdk::Result<Vec<SessionStoreListEntry>> {
            self.inner.list_sessions(project_key).await
        }

        fn supports_list_sessions(&self) -> bool {
            true
        }
    }

    let store = InMemorySessionStore::new();
    let sid_a = Uuid::new_v4().to_string();
    let sid_b = Uuid::new_v4().to_string();
    let uuids = seed_chain(&store, &sid_a, 2).await;
    seed_chain(&store, &sid_b, 1).await;

    let sessions = list_sessions_from_store(&store, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap();
    let ids = sessions
        .iter()
        .map(|session| session.session_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids, [sid_a.as_str(), sid_b.as_str()].into_iter().collect());
    assert!(sessions.iter().all(|s| s.summary == "prompt 0"));
    let mtimes = sessions.iter().map(|s| s.last_modified).collect::<Vec<_>>();
    assert!(mtimes.windows(2).all(|pair| pair[0] >= pair[1]));

    let page = list_sessions_from_store(&store, Some(Path::new(DIR)), Some(1), 1)
        .await
        .unwrap();
    assert_eq!(page.len(), 1);

    let info = get_session_info_from_store(&store, &sid_a, Some(Path::new(DIR)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.session_id, sid_a);
    assert_eq!(info.summary, "prompt 0");
    assert_eq!(info.cwd.as_deref(), Some(DIR));
    assert!(info.created_at.is_some());
    assert!(
        get_session_info_from_store(&store, &Uuid::new_v4().to_string(), Some(Path::new(DIR)))
            .await
            .unwrap()
            .is_none()
    );

    let messages = get_session_messages_from_store(&store, &sid_a, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages
            .iter()
            .map(|message| message.uuid.as_str())
            .collect::<Vec<_>>(),
        uuids.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let page = get_session_messages_from_store(&store, &sid_a, Some(Path::new(DIR)), Some(2), 2)
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert!(
        get_session_messages_from_store(
            &store,
            &Uuid::new_v4().to_string(),
            Some(Path::new(DIR)),
            None,
            0,
        )
        .await
        .unwrap()
        .is_empty()
    );

    let minimal = MinimalStore::default();
    let err = list_sessions_from_store(&minimal, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("list_sessions"));

    let flakey = FlakeyStore::default();
    let good_sid = Uuid::new_v4().to_string();
    let bad_sid = Uuid::new_v4().to_string();
    seed_chain(&flakey.inner, &good_sid, 1).await;
    seed_chain(&flakey.inner, &bad_sid, 1).await;
    *flakey.bad_sid.lock().await = Some(bad_sid.clone());
    let sessions = list_sessions_from_store(&flakey, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap();
    let by_id = sessions
        .iter()
        .map(|session| (session.session_id.as_str(), session))
        .collect::<HashMap<_, _>>();
    assert_eq!(by_id[good_sid.as_str()].summary, "prompt 0");
    assert_eq!(by_id[bad_sid.as_str()].summary, "");
}

#[tokio::test]
async fn list_sessions_from_store_preserves_listing_order_for_equal_mtimes() {
    struct TieOrderStore {
        first_sid: String,
        second_sid: String,
    }

    #[async_trait]
    impl SessionStore for TieOrderStore {
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
            if key.session_id == self.first_sid {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok(Some(vec![user(
                if key.session_id == self.first_sid {
                    "first"
                } else {
                    "second"
                },
                &Uuid::new_v4().to_string(),
                None,
                &key.session_id,
            )]))
        }

        async fn list_sessions(
            &self,
            _project_key: String,
        ) -> claude_agent_sdk::Result<Vec<SessionStoreListEntry>> {
            Ok(vec![
                SessionStoreListEntry {
                    session_id: self.first_sid.clone(),
                    mtime: 100,
                },
                SessionStoreListEntry {
                    session_id: self.second_sid.clone(),
                    mtime: 100,
                },
            ])
        }

        fn supports_list_sessions(&self) -> bool {
            true
        }
    }

    let first_sid = Uuid::new_v4().to_string();
    let second_sid = Uuid::new_v4().to_string();
    let store = TieOrderStore {
        first_sid: first_sid.clone(),
        second_sid: second_sid.clone(),
    };

    let sessions = list_sessions_from_store(&store, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![first_sid.as_str(), second_sid.as_str()]
    );
}

#[tokio::test]
async fn store_read_helpers_filter_sidechains_and_subagents() {
    let store = InMemorySessionStore::new();
    let normal_sid = Uuid::new_v4().to_string();
    let sidechain_sid = Uuid::new_v4().to_string();
    store
        .append(
            key(&normal_sid),
            vec![user(
                "hello world",
                &Uuid::new_v4().to_string(),
                None,
                &normal_sid,
            )],
        )
        .await
        .unwrap();
    let mut side_entry = user(
        "internal",
        &Uuid::new_v4().to_string(),
        None,
        &sidechain_sid,
    );
    side_entry["isSidechain"] = true.into();
    store
        .append(key(&sidechain_sid), vec![side_entry])
        .await
        .unwrap();

    let sessions = list_sessions_from_store(&store, Some(Path::new(DIR)), None, 0)
        .await
        .unwrap();
    assert!(sessions.iter().any(|s| s.session_id == normal_sid));
    assert!(sessions.iter().all(|s| s.session_id != sidechain_sid));

    let sid = Uuid::new_v4().to_string();
    seed_chain(&store, &sid, 1).await;
    let u = Uuid::new_v4().to_string();
    let a = Uuid::new_v4().to_string();
    store
        .append(
            subkey(&sid, "subagents/agent-abc123"),
            vec![
                json!({"type": "agent_metadata", "name": "abc123"}),
                user("sub prompt", &u, None, &sid),
                assistant("sub reply", &a, &u, &sid),
            ],
        )
        .await
        .unwrap();
    store
        .append(
            subkey(&sid, "subagents/workflows/run-1/agent-nested"),
            vec![user(
                "nested prompt",
                &Uuid::new_v4().to_string(),
                None,
                &sid,
            )],
        )
        .await
        .unwrap();

    let mut ids = list_subagents_from_store(&store, &sid, Some(Path::new(DIR)))
        .await
        .unwrap();
    ids.sort();
    assert_eq!(ids, vec!["abc123", "nested"]);
    let messages =
        get_subagent_messages_from_store(&store, &sid, "abc123", Some(Path::new(DIR)), None, 0)
            .await
            .unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].message_type, "user");
    assert_eq!(messages[1].message_type, "assistant");

    assert!(
        list_subagents_from_store(&store, "not-a-uuid", Some(Path::new(DIR)))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        get_subagent_messages_from_store(&store, "not-a-uuid", "x", Some(Path::new(DIR)), None, 0,)
            .await
            .unwrap()
            .is_empty()
    );

    let minimal = MinimalStore::default();
    let err = list_subagents_from_store(&minimal, &sid, Some(Path::new(DIR)))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("list_subkeys"));
    minimal
        .append(
            subkey(&sid, "subagents/agent-direct"),
            vec![user("hi", &Uuid::new_v4().to_string(), None, &sid)],
        )
        .await
        .unwrap();
    let direct =
        get_subagent_messages_from_store(&minimal, &sid, "direct", Some(Path::new(DIR)), None, 0)
            .await
            .unwrap();
    assert_eq!(direct.len(), 1);
}

#[tokio::test]
async fn store_mutation_helpers_append_metadata_reflect_info_and_delete() {
    let store = InMemorySessionStore::new();
    let sid = Uuid::new_v4().to_string();
    seed_chain(&store, &sid, 1).await;

    rename_session_via_store(&store, &sid, "  New Title  ", Some(Path::new(DIR)))
        .await
        .unwrap();
    let entries = store.get_entries(key(&sid)).await;
    let title = entries.last().unwrap();
    assert_eq!(title["type"], "custom-title");
    assert_eq!(title["customTitle"], "New Title");
    assert_eq!(title["sessionId"], sid);
    assert!(title["uuid"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(title["timestamp"].as_str().is_some_and(|s| !s.is_empty()));

    tag_session_via_store(&store, &sid, Some("experiment"), Some(Path::new(DIR)))
        .await
        .unwrap();
    tag_session_via_store(&store, &sid, None, Some(Path::new(DIR)))
        .await
        .unwrap();
    let entries = store.get_entries(key(&sid)).await;
    assert_eq!(entries.last().unwrap()["type"], "tag");
    assert_eq!(entries.last().unwrap()["tag"], "");

    tag_session_via_store(&store, &sid, Some("exp"), Some(Path::new(DIR)))
        .await
        .unwrap();
    let info = get_session_info_from_store(&store, &sid, Some(Path::new(DIR)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.custom_title.as_deref(), Some("New Title"));
    assert_eq!(info.summary, "New Title");
    assert_eq!(info.tag.as_deref(), Some("exp"));

    assert!(
        rename_session_via_store(&store, "not-a-uuid", "title", Some(Path::new(DIR)))
            .await
            .unwrap_err()
            .to_string()
            .contains("Invalid session_id")
    );
    assert!(
        rename_session_via_store(&store, &sid, "   ", Some(Path::new(DIR)))
            .await
            .unwrap_err()
            .to_string()
            .contains("title must be non-empty")
    );
    assert!(
        tag_session_via_store(&store, "not-a-uuid", Some("tag"), Some(Path::new(DIR)))
            .await
            .unwrap_err()
            .to_string()
            .contains("Invalid session_id")
    );

    let noop_store = MinimalStore::default();
    delete_session_via_store(&noop_store, &sid, Some(Path::new(DIR)))
        .await
        .unwrap();

    assert_eq!(store.size().await, 1);
    delete_session_via_store(&store, &sid, Some(Path::new(DIR)))
        .await
        .unwrap();
    assert_eq!(store.size().await, 0);
    assert!(store.load(key(&sid)).await.unwrap().is_none());
}

#[tokio::test]
async fn fork_session_via_store_remaps_chain_slices_and_stamps_synthetic_entries() {
    let store = InMemorySessionStore::new();
    let sid = Uuid::new_v4().to_string();
    let u1 = user("one", &Uuid::new_v4().to_string(), None, &sid);
    let a1 = assistant(
        "two",
        &Uuid::new_v4().to_string(),
        u1["uuid"].as_str().unwrap(),
        &sid,
    );
    let u2 = user(
        "three",
        &Uuid::new_v4().to_string(),
        a1["uuid"].as_str(),
        &sid,
    );
    let cr = json!({
        "type": "content-replacement",
        "sessionId": sid,
        "replacements": [{"toolUseId": "tu_1", "newContent": "x"}],
    });
    store
        .append(key(&sid), vec![u1.clone(), a1.clone(), u2, cr])
        .await
        .unwrap();

    let result = fork_session_via_store(
        &store,
        &sid,
        Some(Path::new(DIR)),
        Some(a1["uuid"].as_str().unwrap()),
        Some("My Fork"),
    )
    .await
    .unwrap();
    assert_ne!(result.session_id, sid);
    let forked = store.get_entries(key(&result.session_id)).await;
    assert_eq!(forked.len(), 4);
    let f0 = &forked[0];
    let f1 = &forked[1];
    let cr_out = &forked[2];
    let title = &forked[3];

    assert_ne!(f0["uuid"], u1["uuid"]);
    assert!(f0["parentUuid"].is_null());
    assert_eq!(f1["parentUuid"], f0["uuid"]);
    assert_eq!(f0["sessionId"], result.session_id);
    assert_eq!(f0["forkedFrom"]["messageUuid"], u1["uuid"]);
    assert_eq!(title["type"], "custom-title");
    assert_eq!(title["customTitle"], "My Fork");
    assert!(title["uuid"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(title["timestamp"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(cr_out["type"], "content-replacement");
    assert_eq!(cr_out["sessionId"], result.session_id);
    assert!(cr_out["uuid"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(cr_out["timestamp"].as_str().is_some_and(|s| !s.is_empty()));

    let messages =
        get_session_messages_from_store(&store, &result.session_id, Some(Path::new(DIR)), None, 0)
            .await
            .unwrap();
    assert_eq!(messages.len(), 2);

    let titled_sid = Uuid::new_v4().to_string();
    seed_chain(&store, &titled_sid, 1).await;
    rename_session_via_store(&store, &titled_sid, "My Title", Some(Path::new(DIR)))
        .await
        .unwrap();
    let result = fork_session_via_store(&store, &titled_sid, Some(Path::new(DIR)), None, None)
        .await
        .unwrap();
    let forked = store.get_entries(key(&result.session_id)).await;
    assert_eq!(forked.last().unwrap()["customTitle"], "My Title (fork)");

    assert!(
        fork_session_via_store(&store, "not-a-uuid", Some(Path::new(DIR)), None, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("Invalid session_id")
    );
    assert!(
        fork_session_via_store(&store, &sid, Some(Path::new(DIR)), Some("not-a-uuid"), None,)
            .await
            .unwrap_err()
            .to_string()
            .contains("Invalid up_to_message_id")
    );
    let missing = fork_session_via_store(
        &store,
        &Uuid::new_v4().to_string(),
        Some(Path::new(DIR)),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(missing.to_string().contains("not found"));
}
