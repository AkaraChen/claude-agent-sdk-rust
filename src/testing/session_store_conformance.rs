use std::collections::HashSet;
use std::sync::Arc;

use serde_json::json;

use crate::Result;
use crate::internal::session_summary::fold_session_summary;
use crate::types::{SessionKey, SessionListSubkeysKey, SessionStore};

pub async fn run_session_store_conformance<F>(
    make_store: F,
    skip_optional: HashSet<&'static str>,
) -> Result<()>
where
    F: Fn() -> Arc<dyn SessionStore>,
{
    let optional: HashSet<&'static str> = [
        "list_sessions",
        "list_session_summaries",
        "delete",
        "list_subkeys",
    ]
    .into_iter()
    .collect();
    assert!(
        skip_optional.is_subset(&optional),
        "unknown optional methods in skip_optional: {:?}",
        skip_optional.difference(&optional).collect::<Vec<_>>()
    );

    let key = make_key("proj", "sess", None);
    let probe = make_store();
    let has_list_sessions =
        probe.supports_list_sessions() && !skip_optional.contains("list_sessions");
    let has_list_summaries = probe.supports_list_session_summaries()
        && !skip_optional.contains("list_session_summaries");
    let has_delete = probe.supports_delete() && !skip_optional.contains("delete");
    let has_list_subkeys = probe.supports_list_subkeys() && !skip_optional.contains("list_subkeys");

    // 1. append then load returns same entries in same order.
    let store = make_store();
    store
        .append(
            key.clone(),
            vec![
                entry(json!({"uuid": "b", "n": 1})),
                entry(json!({"uuid": "a", "n": 2})),
            ],
        )
        .await?;
    assert_eq!(
        store.load(key.clone()).await?,
        Some(vec![
            entry(json!({"uuid": "b", "n": 1})),
            entry(json!({"uuid": "a", "n": 2}))
        ])
    );

    // 2. load unknown key returns None, including unknown subpaths.
    let store = make_store();
    assert!(store.load(make_key("proj", "nope", None)).await?.is_none());
    store
        .append(key.clone(), vec![entry(json!({"uuid": "x", "n": 1}))])
        .await?;
    assert!(
        store
            .load(make_key("proj", "sess", Some("nope")))
            .await?
            .is_none()
    );

    // 3. multiple append calls preserve call order.
    let store = make_store();
    store
        .append(key.clone(), vec![entry(json!({"uuid": "z", "n": 1}))])
        .await?;
    store
        .append(
            key.clone(),
            vec![
                entry(json!({"uuid": "a", "n": 2})),
                entry(json!({"uuid": "m", "n": 3})),
            ],
        )
        .await?;
    store
        .append(key.clone(), vec![entry(json!({"uuid": "b", "n": 4}))])
        .await?;
    assert_eq!(
        store.load(key.clone()).await?,
        Some(vec![
            entry(json!({"uuid": "z", "n": 1})),
            entry(json!({"uuid": "a", "n": 2})),
            entry(json!({"uuid": "m", "n": 3})),
            entry(json!({"uuid": "b", "n": 4})),
        ])
    );

    // 4. append([]) is a no-op.
    let store = make_store();
    store
        .append(key.clone(), vec![entry(json!({"uuid": "a", "n": 1}))])
        .await?;
    store.append(key.clone(), vec![]).await?;
    assert_eq!(
        store.load(key.clone()).await?,
        Some(vec![entry(json!({"uuid": "a", "n": 1}))])
    );

    // 5. subpath keys are stored independently of main.
    let store = make_store();
    let sub = make_key("proj", "sess", Some("subagents/agent-1"));
    store
        .append(key.clone(), vec![entry(json!({"uuid": "m", "n": 1}))])
        .await?;
    store
        .append(sub.clone(), vec![entry(json!({"uuid": "s", "n": 1}))])
        .await?;
    assert_eq!(
        store.load(key.clone()).await?,
        Some(vec![entry(json!({"uuid": "m", "n": 1}))])
    );
    assert_eq!(
        store.load(sub.clone()).await?,
        Some(vec![entry(json!({"uuid": "s", "n": 1}))])
    );

    // 6. project_key isolation.
    let store = make_store();
    store
        .append(make_key("A", "s1", None), vec![entry(json!({"from": "A"}))])
        .await?;
    store
        .append(make_key("B", "s1", None), vec![entry(json!({"from": "B"}))])
        .await?;
    assert_eq!(
        store.load(make_key("A", "s1", None)).await?,
        Some(vec![entry(json!({"from": "A"}))])
    );
    assert_eq!(
        store.load(make_key("B", "s1", None)).await?,
        Some(vec![entry(json!({"from": "B"}))])
    );
    if has_list_sessions {
        assert_eq!(store.list_sessions("A".to_string()).await?.len(), 1);
        assert_eq!(store.list_sessions("B".to_string()).await?.len(), 1);
    }

    if has_list_sessions {
        // 7. list_sessions returns project-scoped session IDs with epoch-ms mtimes.
        let store = make_store();
        store
            .append(make_key("proj", "a", None), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(make_key("proj", "b", None), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(make_key("other", "c", None), vec![entry(json!({"n": 1}))])
            .await?;
        let sessions = store.list_sessions("proj".to_string()).await?;
        let mut ids: Vec<String> = sessions.iter().map(|s| s.session_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
        assert!(sessions.iter().all(|s| s.mtime > 1_000_000_000_000));
        assert!(
            store
                .list_sessions("never-appended-project".to_string())
                .await?
                .is_empty()
        );

        // 8. list_sessions excludes subagent subpaths.
        let store = make_store();
        store
            .append(make_key("proj", "main", None), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(
                make_key("proj", "main", Some("subagents/agent-1")),
                vec![entry(json!({"n": 1}))],
            )
            .await?;
        let ids: Vec<String> = store
            .list_sessions("proj".to_string())
            .await?
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids, vec!["main".to_string()]);
    }

    if has_list_summaries {
        // 14. list_session_summaries returns persisted fold output with store-time mtime.
        let store = make_store();
        let summ_key = make_key("proj", "summ-sess", None);
        store
            .append(
                summ_key.clone(),
                vec![
                    entry(json!({"timestamp": "2024-01-01T00:00:00.000Z", "customTitle": "first"})),
                    entry(json!({"timestamp": "2024-01-01T00:00:01.000Z"})),
                ],
            )
            .await?;
        store
            .append(
                summ_key.clone(),
                vec![entry(
                    json!({"timestamp": "2024-01-01T00:00:02.000Z", "customTitle": "second"}),
                )],
            )
            .await?;
        store
            .append(
                make_key("other", "elsewhere", None),
                vec![entry(json!({"timestamp": "2024-01-01T00:00:00.000Z"}))],
            )
            .await?;

        let summaries = store.list_session_summaries("proj".to_string()).await?;
        let mut summaries: Vec<_> = summaries
            .into_iter()
            .filter(|summary| summary.session_id == "summ-sess")
            .collect();
        assert_eq!(summaries.len(), 1);
        let summary = summaries.remove(0);
        assert!(summary.mtime > 1_000_000_000_000);
        if has_list_sessions {
            let list_mtime = store
                .list_sessions("proj".to_string())
                .await?
                .into_iter()
                .find(|entry| entry.session_id == "summ-sess")
                .map(|entry| entry.mtime)
                .expect("summarized session must be listed");
            assert!(summary.mtime >= list_mtime);
        }
        let refolded = fold_session_summary(
            Some(&summary),
            &summ_key,
            &[entry(json!({"timestamp": "2024-01-01T00:00:03.000Z"}))],
        );
        assert_eq!(refolded.session_id, "summ-sess");
        assert_eq!(refolded.mtime, summary.mtime);

        let summary_data_before_subagent = summary.data.clone();
        store
            .append(
                make_key("proj", "summ-sess", Some("subagents/agent-1")),
                vec![entry(
                    json!({"timestamp": "2024-01-01T00:00:09.000Z", "customTitle": "subagent"}),
                )],
            )
            .await?;
        let after_subagent = store
            .list_session_summaries("proj".to_string())
            .await?
            .into_iter()
            .find(|summary| summary.session_id == "summ-sess")
            .expect("summary must still exist");
        assert_eq!(after_subagent.data, summary_data_before_subagent);
        assert!(
            store
                .list_session_summaries("never-appended-project".to_string())
                .await?
                .is_empty()
        );
        if has_delete {
            store.delete(summ_key).await?;
            assert!(
                store
                    .list_session_summaries("proj".to_string())
                    .await?
                    .is_empty()
            );
        }
    }

    if has_delete {
        // 9. delete main then load returns None.
        let store = make_store();
        store
            .delete(make_key("proj", "never-written", None))
            .await?;
        store
            .append(key.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store.delete(key.clone()).await?;
        assert!(store.load(key.clone()).await?.is_none());

        // 10. delete main cascades to subkeys without touching other sessions/projects.
        let store = make_store();
        let sub1 = make_key("proj", "sess", Some("subagents/agent-1"));
        let sub2 = make_key("proj", "sess", Some("subagents/agent-2"));
        let other = make_key("proj", "sess2", None);
        let other_proj = make_key("other-proj", "sess", None);
        store
            .append(key.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(sub1.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(sub2.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(other.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(other_proj.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store.delete(key.clone()).await?;
        assert!(store.load(key.clone()).await?.is_none());
        assert!(store.load(sub1.clone()).await?.is_none());
        assert!(store.load(sub2.clone()).await?.is_none());
        assert_eq!(
            store.load(other.clone()).await?,
            Some(vec![entry(json!({"n": 1}))])
        );
        assert_eq!(
            store.load(other_proj.clone()).await?,
            Some(vec![entry(json!({"n": 1}))])
        );
        if has_list_subkeys {
            assert!(
                store
                    .list_subkeys(list_key("proj", "sess"))
                    .await?
                    .is_empty()
            );
        }
        if has_list_sessions {
            let listed = store.list_sessions("proj".to_string()).await?;
            assert!(!listed.iter().any(|s| s.session_id == "sess"));
        }

        // 11. delete with subpath removes only that subkey.
        let store = make_store();
        store
            .append(key.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(sub1.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(sub2.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store.delete(sub1.clone()).await?;
        assert!(store.load(sub1).await?.is_none());
        assert_eq!(store.load(sub2).await?, Some(vec![entry(json!({"n": 1}))]));
        assert_eq!(
            store.load(key.clone()).await?,
            Some(vec![entry(json!({"n": 1}))])
        );
        if has_list_subkeys {
            assert_eq!(
                store.list_subkeys(list_key("proj", "sess")).await?,
                vec!["subagents/agent-2".to_string()]
            );
        }
    }

    if has_list_subkeys {
        // 12. list_subkeys returns only subpaths for the requested session.
        let store = make_store();
        store
            .append(key.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(sub.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        store
            .append(
                make_key("proj", "sess", Some("subagents/agent-2")),
                vec![entry(json!({"n": 1}))],
            )
            .await?;
        store
            .append(
                make_key("proj", "other-sess", Some("subagents/agent-x")),
                vec![entry(json!({"n": 1}))],
            )
            .await?;
        assert_eq!(
            sorted(store.list_subkeys(list_key("proj", "sess")).await?),
            vec![
                "subagents/agent-1".to_string(),
                "subagents/agent-2".to_string()
            ]
        );

        // 13. list_subkeys excludes the main transcript and unknown sessions are empty.
        let store = make_store();
        store
            .append(key.clone(), vec![entry(json!({"n": 1}))])
            .await?;
        assert!(
            store
                .list_subkeys(list_key("proj", "sess"))
                .await?
                .is_empty()
        );
        assert!(
            store
                .list_subkeys(list_key("proj", "never-appended"))
                .await?
                .is_empty()
        );
    }

    Ok(())
}

fn make_key(project_key: &str, session_id: &str, subpath: Option<&str>) -> SessionKey {
    SessionKey {
        project_key: project_key.to_string(),
        session_id: session_id.to_string(),
        subpath: subpath.map(ToString::to_string),
    }
}

fn list_key(project_key: &str, session_id: &str) -> SessionListSubkeysKey {
    SessionListSubkeysKey {
        project_key: project_key.to_string(),
        session_id: session_id.to_string(),
    }
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

fn entry(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "type".to_string(),
            serde_json::Value::String("x".to_string()),
        );
    }
    value
}
