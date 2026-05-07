use chrono::DateTime;
use serde_json::Value;

use crate::types::{JsonMap, SdkSessionInfo, SessionKey, SessionStoreEntry, SessionSummaryEntry};

use super::sessions::{command_name_regex, skip_first_prompt_regex};

const LAST_WINS_FIELDS: &[(&str, &str)] = &[
    ("customTitle", "custom_title"),
    ("aiTitle", "ai_title"),
    ("lastPrompt", "last_prompt"),
    ("summary", "summary_hint"),
    ("gitBranch", "git_branch"),
];

pub fn fold_session_summary(
    prev: Option<&SessionSummaryEntry>,
    key: &SessionKey,
    entries: &[SessionStoreEntry],
) -> SessionSummaryEntry {
    let mut summary = match prev {
        Some(prev) => SessionSummaryEntry {
            session_id: prev.session_id.clone(),
            mtime: prev.mtime,
            data: prev.data.clone(),
        },
        None => SessionSummaryEntry {
            session_id: key.session_id.clone(),
            mtime: 0,
            data: JsonMap::new(),
        },
    };

    for raw in entries {
        let Value::Object(entry) = raw else {
            continue;
        };
        let ms = entry.get("timestamp").and_then(iso_to_epoch_ms);

        if !summary.data.contains_key("is_sidechain") {
            summary.data.insert(
                "is_sidechain".to_string(),
                Value::Bool(entry.get("isSidechain").and_then(Value::as_bool) == Some(true)),
            );
        }
        if !summary.data.contains_key("created_at") {
            if let Some(ms) = ms {
                summary
                    .data
                    .insert("created_at".to_string(), Value::Number(ms.into()));
            }
        }
        if !summary.data.contains_key("cwd") {
            if let Some(cwd) = entry
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                summary
                    .data
                    .insert("cwd".to_string(), Value::String(cwd.to_string()));
            }
        }

        fold_first_prompt(&mut summary.data, raw);

        for (src, dst) in LAST_WINS_FIELDS {
            if let Some(value) = entry.get(*src).and_then(Value::as_str) {
                summary
                    .data
                    .insert((*dst).to_string(), Value::String(value.to_string()));
            }
        }

        if entry.get("type").and_then(Value::as_str) == Some("tag") {
            if let Some(tag) = entry
                .get("tag")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                summary
                    .data
                    .insert("tag".to_string(), Value::String(tag.to_string()));
            } else {
                summary.data.remove("tag");
            }
        }
    }

    summary
}

pub fn summary_entry_to_sdk_info(
    entry: &SessionSummaryEntry,
    project_path: Option<&str>,
) -> Option<SdkSessionInfo> {
    if entry
        .data
        .get("is_sidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let first_prompt = if entry
        .data
        .get("first_prompt_locked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        string_field(&entry.data, "first_prompt")
    } else {
        string_field(&entry.data, "command_fallback")
    };
    let custom_title =
        string_field(&entry.data, "custom_title").or_else(|| string_field(&entry.data, "ai_title"));
    let summary = custom_title
        .clone()
        .or_else(|| string_field(&entry.data, "last_prompt"))
        .or_else(|| string_field(&entry.data, "summary_hint"))
        .or_else(|| first_prompt.clone())?;

    Some(SdkSessionInfo {
        session_id: entry.session_id.clone(),
        summary,
        last_modified: entry.mtime,
        file_size: None,
        custom_title,
        first_prompt,
        git_branch: string_field(&entry.data, "git_branch"),
        cwd: string_field(&entry.data, "cwd").or_else(|| project_path.map(ToString::to_string)),
        tag: string_field(&entry.data, "tag"),
        created_at: entry.data.get("created_at").and_then(Value::as_i64),
    })
}

fn iso_to_epoch_ms(value: &Value) -> Option<i64> {
    let s = value.as_str()?;
    DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00"))
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn entry_text_blocks(entry: &Value) -> Vec<String> {
    let Some(message) = entry.get("message").and_then(Value::as_object) else {
        return Vec::new();
    };
    match message.get("content") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| {
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .map(ToString::to_string)
                    })
                    .flatten()
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn fold_first_prompt(data: &mut JsonMap, entry: &Value) {
    if data
        .get("first_prompt_locked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return;
    }
    if entry.get("type").and_then(Value::as_str) != Some("user") {
        return;
    }
    if entry.get("isMeta").and_then(Value::as_bool) == Some(true)
        || entry.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
    {
        return;
    }
    if entry
        .pointer("/message/content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
    {
        return;
    }

    for raw in entry_text_blocks(entry) {
        let mut result = raw.replace('\n', " ").trim().to_string();
        if result.is_empty() {
            continue;
        }
        if let Some(caps) = command_name_regex().captures(&result) {
            if !data.contains_key("command_fallback") {
                if let Some(m) = caps.get(1) {
                    data.insert(
                        "command_fallback".to_string(),
                        Value::String(m.as_str().to_string()),
                    );
                }
            }
            continue;
        }
        if skip_first_prompt_regex().is_match(&result) {
            continue;
        }
        if result.chars().count() > 200 {
            result = result
                .chars()
                .take(200)
                .collect::<String>()
                .trim_end()
                .to_string();
            result.push('…');
        }
        data.insert("first_prompt".to_string(), Value::String(result));
        data.insert("first_prompt_locked".to_string(), Value::Bool(true));
        return;
    }
}

fn string_field(map: &JsonMap, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn key() -> SessionKey {
        SessionKey {
            project_key: "proj".to_string(),
            session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            subpath: None,
        }
    }

    fn user(text: Value) -> Value {
        json!({
            "type": "user",
            "timestamp": "2024-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": text},
        })
    }

    #[test]
    fn fold_initializes_from_none() {
        let summary = fold_session_summary(None, &key(), &[]);
        assert_eq!(summary.session_id, key().session_id);
        assert_eq!(summary.mtime, 0);
        assert!(summary.data.is_empty());
    }

    #[test]
    fn fold_set_once_fields_freeze() {
        let summary = fold_session_summary(
            None,
            &key(),
            &[
                json!({"type": "x", "timestamp": "2024-01-01T00:00:00.000Z", "cwd": "/a", "isSidechain": false}),
                json!({"type": "x", "timestamp": "2024-01-01T00:00:05.000Z", "cwd": "/b"}),
            ],
        );
        assert_eq!(summary.data["created_at"], 1_704_067_200_000_i64);
        assert_eq!(summary.data["cwd"], "/a");
        assert_eq!(summary.data["is_sidechain"], false);

        let summary2 = fold_session_summary(
            Some(&summary),
            &key(),
            &[
                json!({"type": "x", "timestamp": "2024-01-02T00:00:00.000Z", "cwd": "/c", "isSidechain": true}),
            ],
        );
        assert_eq!(summary2.data["created_at"], 1_704_067_200_000_i64);
        assert_eq!(summary2.data["cwd"], "/a");
        assert_eq!(summary2.data["is_sidechain"], false);
    }

    #[test]
    fn fold_last_wins_fields_overwrite() {
        let summary = fold_session_summary(
            None,
            &key(),
            &[
                json!({"type": "x", "timestamp": "2024-01-01T00:00:00Z", "customTitle": "t1", "gitBranch": "main"}),
                json!({"type": "x", "timestamp": "2024-01-01T00:00:01Z", "customTitle": "t2"}),
            ],
        );
        assert_eq!(summary.data["custom_title"], "t2");
        assert_eq!(summary.data["git_branch"], "main");

        let summary2 = fold_session_summary(
            Some(&summary),
            &key(),
            &[
                json!({"type": "x", "aiTitle": "ai", "lastPrompt": "lp", "summary": "sm", "gitBranch": "dev"}),
            ],
        );
        assert_eq!(summary2.data["custom_title"], "t2");
        assert_eq!(summary2.data["ai_title"], "ai");
        assert_eq!(summary2.data["last_prompt"], "lp");
        assert_eq!(summary2.data["summary_hint"], "sm");
        assert_eq!(summary2.data["git_branch"], "dev");
    }

    #[test]
    fn fold_preserves_mtime() {
        let summary = fold_session_summary(
            None,
            &key(),
            &[json!({"type": "x", "timestamp": "2024-01-01T00:00:05.000Z"})],
        );
        assert_eq!(summary.mtime, 0);

        let prev = SessionSummaryEntry {
            session_id: key().session_id,
            mtime: 42,
            data: JsonMap::new(),
        };
        let summary2 = fold_session_summary(
            Some(&prev),
            &key(),
            &[json!({"type": "x", "timestamp": "2024-01-01T00:00:10.000Z"})],
        );
        assert_eq!(summary2.mtime, 42);
    }

    #[test]
    fn fold_tag_set_clear_and_ignore_non_tag() {
        let summary = fold_session_summary(None, &key(), &[json!({"type": "tag", "tag": "wip"})]);
        assert_eq!(summary.data["tag"], "wip");
        let cleared =
            fold_session_summary(Some(&summary), &key(), &[json!({"type": "tag", "tag": ""})]);
        assert!(!cleared.data.contains_key("tag"));
        let ignored = fold_session_summary(
            Some(&summary),
            &key(),
            &[json!({"type": "user", "tag": "ignored"})],
        );
        assert_eq!(ignored.data["tag"], "wip");
    }

    #[test]
    fn fold_first_prompt_skip_and_command_fallback() {
        let summary = fold_session_summary(
            None,
            &key(),
            &[
                json!({"type": "user", "isMeta": true, "message": {"content": "ignored meta"}}),
                json!({"type": "user", "isCompactSummary": true, "message": {"content": "ignored compact"}}),
                user(json!([{"type": "tool_result", "tool_use_id": "x", "content": "res"}])),
                user(json!("<command-name>/init</command-name> stuff")),
                user(json!("real first")),
            ],
        );
        assert_eq!(summary.data["command_fallback"], "/init");
        assert_eq!(summary.data["first_prompt"], "real first");
        assert_eq!(summary.data["first_prompt_locked"], true);
    }

    #[test]
    fn fold_first_prompt_truncates() {
        let summary = fold_session_summary(None, &key(), &[user(json!("x".repeat(300)))]);
        let prompt = summary.data["first_prompt"].as_str().unwrap();
        assert!(prompt.chars().count() <= 201);
        assert!(prompt.ends_with('…'));
    }

    #[test]
    fn summary_entry_to_info_matches_precedence() {
        let mut data = JsonMap::new();
        data.insert(
            "first_prompt".to_string(),
            Value::String("first".to_string()),
        );
        data.insert("first_prompt_locked".to_string(), Value::Bool(true));
        data.insert("ai_title".to_string(), Value::String("AI".to_string()));
        data.insert(
            "custom_title".to_string(),
            Value::String("Custom".to_string()),
        );
        data.insert("git_branch".to_string(), Value::String("main".to_string()));
        data.insert("created_at".to_string(), Value::Number(123_i64.into()));
        let entry = SessionSummaryEntry {
            session_id: "sess".to_string(),
            mtime: 99,
            data,
        };
        let info = summary_entry_to_sdk_info(&entry, Some("/project")).unwrap();
        assert_eq!(info.summary, "Custom");
        assert_eq!(info.custom_title.as_deref(), Some("Custom"));
        assert_eq!(info.first_prompt.as_deref(), Some("first"));
        assert_eq!(info.git_branch.as_deref(), Some("main"));
        assert_eq!(info.cwd.as_deref(), Some("/project"));
        assert_eq!(info.created_at, Some(123));
    }

    #[test]
    fn summary_entry_to_info_filters_sidechains_and_empty() {
        let mut sidechain_data = JsonMap::new();
        sidechain_data.insert("is_sidechain".to_string(), Value::Bool(true));
        sidechain_data.insert(
            "custom_title".to_string(),
            Value::String("Hidden".to_string()),
        );
        let sidechain = SessionSummaryEntry {
            session_id: "sess".to_string(),
            mtime: 1,
            data: sidechain_data,
        };
        assert!(summary_entry_to_sdk_info(&sidechain, None).is_none());

        let empty = SessionSummaryEntry {
            session_id: "sess".to_string(),
            mtime: 1,
            data: JsonMap::new(),
        };
        assert!(summary_entry_to_sdk_info(&empty, None).is_none());
    }
}
