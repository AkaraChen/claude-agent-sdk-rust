use std::collections::HashSet;
use std::sync::Arc;

use claude_agent_sdk::testing::{PostgresLikeSessionStore, run_session_store_conformance};
use claude_agent_sdk::{SessionKey, SessionListSubkeysKey, SessionStore};
use serde_json::json;

fn key(project: &str, session: &str, subpath: Option<&str>) -> SessionKey {
    SessionKey {
        project_key: project.to_string(),
        session_id: session.to_string(),
        subpath: subpath.map(ToString::to_string),
    }
}

fn list_key(project: &str, session: &str) -> SessionListSubkeysKey {
    SessionListSubkeysKey {
        project_key: project.to_string(),
        session_id: session.to_string(),
    }
}

#[tokio::test]
async fn postgres_like_store_passes_session_store_conformance() {
    run_session_store_conformance(
        || Arc::new(PostgresLikeSessionStore::new()) as Arc<dyn SessionStore>,
        HashSet::from(["list_session_summaries"]),
    )
    .await
    .unwrap();
}

#[test]
fn postgres_like_store_rejects_unsafe_table_names_like_python_example() {
    assert!(PostgresLikeSessionStore::try_new("claude_session_store").is_ok());
    assert!(PostgresLikeSessionStore::try_new("_valid123").is_ok());
    for table in [
        "",
        "1bad",
        "bad-name",
        "bad;drop table sessions",
        "bad name",
    ] {
        let err = PostgresLikeSessionStore::try_new(table).unwrap_err();
        assert!(err.to_string().contains("[A-Za-z_][A-Za-z0-9_]*"));
    }
}

#[tokio::test]
async fn postgres_like_store_preserves_append_order_and_deep_json_values() {
    let store = PostgresLikeSessionStore::try_new("custom_table").unwrap();
    assert_eq!(store.table(), "custom_table");
    let main = key("proj", "sess", None);
    store
        .append(
            main.clone(),
            vec![
                json!({"type": "user", "entry": {"long_key": 1, "a": true}}),
                json!({"type": "assistant", "entry": {"nested": ["x", "y"]}}),
            ],
        )
        .await
        .unwrap();
    store
        .append(main.clone(), vec![json!({"type": "result", "n": 3})])
        .await
        .unwrap();
    assert_eq!(
        store.load(main).await.unwrap(),
        Some(vec![
            json!({"type": "user", "entry": {"long_key": 1, "a": true}}),
            json!({"type": "assistant", "entry": {"nested": ["x", "y"]}}),
            json!({"type": "result", "n": 3}),
        ])
    );
}

#[tokio::test]
async fn postgres_like_list_delete_and_subkeys_match_python_example() {
    let store = PostgresLikeSessionStore::new();
    let main = key("proj", "sess", None);
    let sub1 = key("proj", "sess", Some("subagents/agent-1"));
    let sub2 = key("proj", "sess", Some("subagents/agent-2"));
    let other = key("proj", "other", None);
    store
        .append(main.clone(), vec![json!({"type": "user", "n": 1})])
        .await
        .unwrap();
    store
        .append(sub1.clone(), vec![json!({"type": "user", "n": 2})])
        .await
        .unwrap();
    store
        .append(sub2.clone(), vec![json!({"type": "user", "n": 3})])
        .await
        .unwrap();
    store
        .append(other.clone(), vec![json!({"type": "user", "n": 4})])
        .await
        .unwrap();

    let mut listed = store
        .list_sessions("proj".to_string())
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.session_id)
        .collect::<Vec<_>>();
    listed.sort();
    assert_eq!(listed, vec!["other".to_string(), "sess".to_string()]);
    assert_eq!(
        store.list_subkeys(list_key("proj", "sess")).await.unwrap(),
        vec![
            "subagents/agent-1".to_string(),
            "subagents/agent-2".to_string()
        ]
    );

    store.delete(sub1.clone()).await.unwrap();
    assert!(store.load(sub1).await.unwrap().is_none());
    assert!(store.load(sub2.clone()).await.unwrap().is_some());
    store.delete(main.clone()).await.unwrap();
    assert!(store.load(main).await.unwrap().is_none());
    assert!(store.load(sub2).await.unwrap().is_none());
    assert!(store.load(other).await.unwrap().is_some());
}
