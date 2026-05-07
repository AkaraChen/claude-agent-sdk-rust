use std::collections::HashSet;
use std::sync::Arc;

use claude_agent_sdk::testing::{S3LikeSessionStore, run_session_store_conformance};
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
async fn s3_like_store_passes_session_store_conformance() {
    run_session_store_conformance(
        || Arc::new(S3LikeSessionStore::new("bucket", "transcripts")) as Arc<dyn SessionStore>,
        HashSet::from(["list_session_summaries"]),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn s3_like_append_uses_sortable_part_names_and_jsonl_body() {
    let store = S3LikeSessionStore::new("bucket", "transcripts/");
    assert_eq!(store.bucket(), "bucket");
    let main = key("proj", "sess", None);
    store
        .append(
            main.clone(),
            vec![
                json!({"type": "user", "n": 1}),
                json!({"type": "assistant", "n": 2}),
            ],
        )
        .await
        .unwrap();
    store
        .append(main.clone(), vec![json!({"type": "result", "n": 3})])
        .await
        .unwrap();

    let keys = store.object_keys().await;
    assert_eq!(keys.len(), 2);
    assert!(keys[0].starts_with("transcripts/proj/sess/part-"));
    assert!(keys[0].ends_with(".jsonl"));
    assert_ne!(keys[0], keys[1]);
    assert!(keys[0] < keys[1]);

    let body = String::from_utf8(store.object_body(&keys[0]).await.unwrap()).unwrap();
    assert!(body.ends_with('\n'));
    let lines = body.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(lines[0]).unwrap(),
        json!({"type": "user", "n": 1})
    );
}

#[tokio::test]
async fn s3_like_load_sorts_concatenates_skips_malformed_and_excludes_subpaths() {
    let store = S3LikeSessionStore::new("bucket", "");
    let main = key("proj", "sess", None);
    store
        .append(main.clone(), vec![json!({"type": "user", "n": 1})])
        .await
        .unwrap();
    store
        .append(
            key("proj", "sess", Some("subagents/agent-1")),
            vec![json!({"type": "user", "n": 999})],
        )
        .await
        .unwrap();
    store
        .put_raw_object(
            "proj/sess/part-9999999999999-abcdef.jsonl",
            b"{not json}\n{\"type\":\"assistant\",\"n\":2}\n".to_vec(),
        )
        .await;

    assert_eq!(
        store.load(main).await.unwrap(),
        Some(vec![
            json!({"type": "user", "n": 1}),
            json!({"type": "assistant", "n": 2}),
        ])
    );
}

#[tokio::test]
async fn s3_like_list_sessions_subkeys_and_deletes_match_python_example() {
    let store = S3LikeSessionStore::new("bucket", "pfx");
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
    store
        .put_raw_object(
            "pfx/proj/sess/../part-9999999999999-abcdef.jsonl",
            b"{\"type\":\"bad\"}\n".to_vec(),
        )
        .await;

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

#[test]
fn s3_like_prefix_normalization_matches_python_example() {
    let prefixed = S3LikeSessionStore::new("bucket", "transcripts///");
    let unprefixed = S3LikeSessionStore::new("bucket", "");
    let main = key("proj", "sess", None);
    assert_eq!(prefixed.key_prefix(&main), "transcripts/proj/sess/");
    assert_eq!(unprefixed.key_prefix(&main), "proj/sess/");
    assert!(!prefixed.key_prefix(&main).contains("//"));
    assert!(!unprefixed.key_prefix(&main).starts_with('/'));
}
