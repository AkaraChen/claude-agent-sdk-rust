use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use claude_agent_sdk::testing::run_session_store_conformance;
use claude_agent_sdk::{RedisSessionStore, SessionKey, SessionListSubkeysKey, SessionStore};

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

#[test]
fn redis_session_store_key_normalization_matches_python_example() {
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let prefixed = RedisSessionStore::from_client(client.clone(), "transcripts::");
    let unprefixed = RedisSessionStore::from_client(client, "");
    let main = key("proj", "sess", None);
    let sub = key("proj", "sess", Some("subagents/agent-1"));

    assert_eq!(prefixed.entry_key(&main), "transcripts:proj:sess");
    assert_eq!(
        prefixed.entry_key(&sub),
        "transcripts:proj:sess:subagents/agent-1"
    );
    assert_eq!(
        prefixed.subkeys_key(&list_key("proj", "sess")),
        "transcripts:proj:sess:__subkeys"
    );
    assert_eq!(prefixed.sessions_key("proj"), "transcripts:proj:__sessions");
    assert_eq!(unprefixed.entry_key(&main), "proj:sess");
    assert!(!prefixed.entry_key(&main).contains("::"));
    assert!(!unprefixed.entry_key(&main).starts_with(':'));
}

#[tokio::test]
async fn live_redis_store_passes_conformance_when_env_is_set() {
    let Ok(url) = std::env::var("SESSION_STORE_REDIS_URL") else {
        return;
    };
    let prefix = format!("cas-rust-test-{}", uuid::Uuid::new_v4().simple());
    let counter = Arc::new(AtomicUsize::new(0));
    let result = run_session_store_conformance(
        {
            let url = url.clone();
            let prefix = prefix.clone();
            let counter = counter.clone();
            move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                Arc::new(
                    RedisSessionStore::new(&url, format!("{prefix}:{n}"))
                        .expect("create RedisSessionStore"),
                ) as Arc<dyn SessionStore>
            }
        },
        HashSet::from(["list_session_summaries"]),
    )
    .await;
    cleanup_redis_prefix(&url, &prefix).await;
    result.unwrap();
}

async fn cleanup_redis_prefix(url: &str, prefix: &str) {
    let Ok(client) = redis::Client::open(url) else {
        return;
    };
    let Ok(mut conn) = client.get_multiplexed_async_connection().await else {
        return;
    };
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{prefix}*"))
        .query_async(&mut conn)
        .await
        .unwrap_or_default();
    if !keys.is_empty() {
        let _: redis::RedisResult<()> = redis::cmd("DEL").arg(keys).query_async(&mut conn).await;
    }
}
