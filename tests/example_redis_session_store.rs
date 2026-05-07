use std::collections::HashSet;
use std::sync::Arc;

use claude_agent_sdk::testing::{RedisLikeSessionStore, run_session_store_conformance};
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
async fn redis_like_store_passes_session_store_conformance() {
    run_session_store_conformance(
        || Arc::new(RedisLikeSessionStore::new("transcripts")) as Arc<dyn SessionStore>,
        HashSet::from(["list_session_summaries"]),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn redis_like_append_updates_lists_session_index_and_subkeys_like_python_example() {
    let store = RedisLikeSessionStore::new("transcripts:");
    let main = key("proj", "sess", None);
    store
        .append(main.clone(), vec![json!({"type": "user", "n": 1})])
        .await
        .unwrap();

    let raw = store.raw_list(&store.entry_key(&main)).await;
    assert_eq!(raw.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw[0]).unwrap(),
        json!({"type": "user", "n": 1})
    );
    let zset = store.raw_zset(&store.sessions_key("proj")).await;
    assert!(
        zset.get("sess")
            .is_some_and(|mtime| *mtime > 1_000_000_000_000)
    );

    let sub = key("proj", "sess", Some("subagents/agent-1"));
    store
        .append(sub.clone(), vec![json!({"type": "assistant", "n": 2})])
        .await
        .unwrap();
    assert_eq!(
        store
            .raw_set(&store.subkeys_key(&list_key("proj", "sess")))
            .await,
        HashSet::from(["subagents/agent-1".to_string()])
    );
    assert_eq!(store.raw_zset(&store.sessions_key("proj")).await.len(), 1);
    assert_eq!(
        store.load(sub).await.unwrap(),
        Some(vec![json!({"type": "assistant", "n": 2})])
    );
}

#[tokio::test]
async fn redis_like_load_skips_malformed_json_and_empty_append_is_noop() {
    let store = RedisLikeSessionStore::new("");
    let main = key("proj", "sess", None);
    store.append(main.clone(), vec![]).await.unwrap();
    assert!(store.raw_list(&store.entry_key(&main)).await.is_empty());
    assert!(store.raw_zset(&store.sessions_key("proj")).await.is_empty());

    store
        .push_raw_list(
            store.entry_key(&main),
            vec![
                "{not json".to_string(),
                json!({"type": "user", "ok": true}).to_string(),
            ],
        )
        .await;
    assert_eq!(
        store.load(main).await.unwrap(),
        Some(vec![json!({"type": "user", "ok": true})])
    );
}

#[tokio::test]
async fn redis_like_delete_targeted_subpath_and_main_cascade_match_python_example() {
    let store = RedisLikeSessionStore::new("pfx");
    let main = key("proj", "sess", None);
    let sub1 = key("proj", "sess", Some("subagents/agent-1"));
    let sub2 = key("proj", "sess", Some("subagents/agent-2"));
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

    store.delete(sub1.clone()).await.unwrap();
    assert!(store.load(sub1).await.unwrap().is_none());
    assert!(store.load(sub2.clone()).await.unwrap().is_some());
    assert_eq!(
        store.list_subkeys(list_key("proj", "sess")).await.unwrap(),
        vec!["subagents/agent-2".to_string()]
    );

    store.delete(main.clone()).await.unwrap();
    assert!(store.load(main).await.unwrap().is_none());
    assert!(store.load(sub2).await.unwrap().is_none());
    assert!(
        store
            .list_subkeys(list_key("proj", "sess"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .list_sessions("proj".to_string())
            .await
            .unwrap()
            .is_empty()
    );
}

#[test]
fn redis_like_prefix_normalization_avoids_double_colon_artifacts() {
    let prefixed = RedisLikeSessionStore::new("transcripts::");
    let unprefixed = RedisLikeSessionStore::new("");
    let main = key("proj", "sess", None);
    assert_eq!(prefixed.entry_key(&main), "transcripts:proj:sess");
    assert_eq!(unprefixed.entry_key(&main), "proj:sess");
    assert!(!prefixed.entry_key(&main).contains("::"));
    assert!(!unprefixed.entry_key(&main).starts_with(':'));
}
