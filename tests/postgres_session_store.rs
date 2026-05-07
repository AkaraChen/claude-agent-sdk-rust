use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use claude_agent_sdk::testing::run_session_store_conformance;
use claude_agent_sdk::{PostgresSessionStore, SessionStore};

#[test]
fn postgres_store_rejects_unsafe_table_names_like_python_example() {
    assert!(PostgresSessionStore::new("postgres://example.invalid/db", "claude_sessions").is_ok());
    assert!(PostgresSessionStore::new("postgres://example.invalid/db", "_valid123").is_ok());
    for table in [
        "",
        "1bad",
        "bad-name",
        "bad;drop table sessions",
        "bad name",
    ] {
        let err = PostgresSessionStore::new("postgres://example.invalid/db", table).unwrap_err();
        assert!(
            err.to_string().contains("[A-Za-z_][A-Za-z0-9_]*"),
            "unexpected error: {err}"
        );
    }
}

#[tokio::test]
async fn live_postgres_store_passes_conformance_when_env_is_set() {
    let Ok(url) = std::env::var("SESSION_STORE_POSTGRES_URL") else {
        return;
    };
    let prefix = format!("cas_rust_{}", uuid::Uuid::new_v4().simple());
    let mut tables = Vec::new();
    for n in 0..64 {
        let table = format!("{prefix}_{n}");
        let store = PostgresSessionStore::new(url.clone(), table.clone())
            .expect("create PostgresSessionStore");
        store.create_schema().await.expect("create schema");
        tables.push(table);
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let result = run_session_store_conformance(
        {
            let url = url.clone();
            let tables = Arc::new(tables.clone());
            let counter = counter.clone();
            move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                Arc::new(
                    PostgresSessionStore::new(url.clone(), tables[n].clone())
                        .expect("create PostgresSessionStore"),
                ) as Arc<dyn SessionStore>
            }
        },
        HashSet::from(["list_session_summaries"]),
    )
    .await;
    cleanup_postgres_tables(&url, &tables).await;
    result.unwrap();
}

async fn cleanup_postgres_tables(url: &str, tables: &[String]) {
    let Ok((client, connection)) = tokio_postgres::connect(url, tokio_postgres::NoTls).await else {
        return;
    };
    tokio::spawn(async move {
        let _ = connection.await;
    });
    for table in tables {
        let _ = client
            .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
            .await;
    }
}
