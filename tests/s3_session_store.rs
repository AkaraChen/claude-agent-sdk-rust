use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aws_credential_types::Credentials;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::config::Region;
use claude_agent_sdk::testing::run_session_store_conformance;
use claude_agent_sdk::{S3SessionStore, SessionKey, SessionStore};

fn key(project: &str, session: &str, subpath: Option<&str>) -> SessionKey {
    SessionKey {
        project_key: project.to_string(),
        session_id: session.to_string(),
        subpath: subpath.map(ToString::to_string),
    }
}

fn dummy_s3_client() -> S3Client {
    let conf = aws_sdk_s3::config::Builder::new()
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("test", "test", None, None, "test"))
        .build();
    S3Client::from_conf(conf)
}

#[test]
fn s3_session_store_prefixes_match_python_example() {
    let store = S3SessionStore::new("bucket", "transcripts///", dummy_s3_client());
    let unprefixed = S3SessionStore::new("bucket", "", dummy_s3_client());
    let main = key("proj", "sess", None);
    let sub = key("proj", "sess", Some("subagents/agent-1"));

    assert_eq!(store.bucket(), "bucket");
    assert_eq!(store.key_prefix(&main), "transcripts/proj/sess/");
    assert_eq!(
        store.key_prefix(&sub),
        "transcripts/proj/sess/subagents/agent-1/"
    );
    assert_eq!(store.project_prefix("proj"), "transcripts/proj/");
    assert_eq!(unprefixed.key_prefix(&main), "proj/sess/");
    assert!(!store.key_prefix(&main).contains("//"));
    assert!(!unprefixed.key_prefix(&main).starts_with('/'));
}

#[tokio::test]
async fn live_s3_store_passes_conformance_when_env_is_set() {
    let Some((client, bucket)) = live_s3_client().await else {
        return;
    };
    let prefix = format!("cas-rust-test-{}/", uuid::Uuid::new_v4().simple());
    let counter = Arc::new(AtomicUsize::new(0));
    let result = run_session_store_conformance(
        {
            let client = client.clone();
            let bucket = bucket.clone();
            let prefix = prefix.clone();
            let counter = counter.clone();
            move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                Arc::new(S3SessionStore::new(
                    bucket.clone(),
                    format!("{prefix}iso{n}"),
                    client.clone(),
                )) as Arc<dyn SessionStore>
            }
        },
        HashSet::from(["list_session_summaries"]),
    )
    .await;
    cleanup_s3_prefix(&client, &bucket, &prefix).await;
    result.unwrap();
}

async fn live_s3_client() -> Option<(S3Client, String)> {
    let endpoint = std::env::var("SESSION_STORE_S3_ENDPOINT").ok()?;
    let bucket = std::env::var("SESSION_STORE_S3_BUCKET").ok()?;
    let access_key = std::env::var("SESSION_STORE_S3_ACCESS_KEY").ok()?;
    let secret_key = std::env::var("SESSION_STORE_S3_SECRET_KEY").ok()?;
    let region = std::env::var("SESSION_STORE_S3_REGION").unwrap_or_else(|_| "us-east-1".into());

    let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(Region::new(region))
        .endpoint_url(endpoint)
        .credentials_provider(Credentials::new(
            access_key,
            secret_key,
            None,
            None,
            "session-store-test",
        ))
        .load()
        .await;
    let conf = aws_sdk_s3::config::Builder::from(&shared)
        .force_path_style(true)
        .build();
    Some((S3Client::from_conf(conf), bucket))
}

async fn cleanup_s3_prefix(client: &S3Client, bucket: &str, prefix: &str) {
    let mut token = None::<String>;
    loop {
        let mut req = client.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(token_value) = token {
            req = req.continuation_token(token_value);
        }
        let Ok(response) = req.send().await else {
            return;
        };
        for object in response.contents() {
            if let Some(key) = object.key() {
                let _ = client.delete_object().bucket(bucket).key(key).send().await;
            }
        }
        token = response.next_continuation_token().map(ToString::to_string);
        if token.is_none() {
            break;
        }
    }
}
