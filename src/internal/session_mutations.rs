use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde_json::{Value, json};
use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::errors::{ClaudeSdkError, Result};
use crate::types::{SessionKey, SessionStore};

use super::sessions::{
    LITE_READ_BUF_SIZE, canonicalize_path, extract_first_prompt_from_head,
    extract_last_json_string_field, find_project_dir, get_projects_dir, get_worktree_paths,
    project_key_for_directory, resolve_session_file_path, validate_uuid,
};

#[derive(Debug, Clone)]
pub struct ForkSessionResult {
    pub session_id: String,
}

pub fn rename_session(session_id: &str, title: &str, directory: Option<&Path>) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(ClaudeSdkError::InvalidOptions(
            "title must be non-empty".to_string(),
        ));
    }
    let data = format!(
        "{{\"type\":\"custom-title\",\"customTitle\":{},\"sessionId\":{}}}\n",
        serde_json::to_string(stripped)?,
        serde_json::to_string(session_id)?,
    );
    append_to_session(session_id, &data, directory)
}

pub fn tag_session(session_id: &str, tag: Option<&str>, directory: Option<&Path>) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let tag = match tag {
        Some(tag) => {
            let sanitized = sanitize_unicode(tag).trim().to_string();
            if sanitized.is_empty() {
                return Err(ClaudeSdkError::InvalidOptions(
                    "tag must be non-empty (use None to clear)".to_string(),
                ));
            }
            sanitized
        }
        None => String::new(),
    };
    let data = format!(
        "{{\"type\":\"tag\",\"tag\":{},\"sessionId\":{}}}\n",
        serde_json::to_string(&tag)?,
        serde_json::to_string(session_id)?,
    );
    append_to_session(session_id, &data, directory)
}

pub fn delete_session(session_id: &str, directory: Option<&Path>) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let Some(path) = resolve_session_file_path(session_id, directory) else {
        return Err(ClaudeSdkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Session {session_id} not found"),
        )));
    };
    fs::remove_file(&path)?;
    let _ = fs::remove_dir_all(
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(session_id),
    );
    Ok(())
}

pub fn fork_session(
    session_id: &str,
    directory: Option<&Path>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<ForkSessionResult> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    if let Some(up_to) = up_to_message_id {
        if validate_uuid(up_to).is_none() {
            return Err(ClaudeSdkError::InvalidOptions(format!(
                "Invalid up_to_message_id: {up_to}"
            )));
        }
    }
    let Some(file_path) = resolve_session_file_path(session_id, directory) else {
        return Err(ClaudeSdkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Session {session_id} not found"),
        )));
    };
    let project_dir = file_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let content = fs::read(&file_path)?;
    if content.is_empty() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Session {session_id} has no messages to fork"
        )));
    }
    let (transcript, content_replacements) = parse_fork_transcript(&content, session_id);
    let derive_title = || {
        let head =
            String::from_utf8_lossy(&content[..LITE_READ_BUF_SIZE.min(content.len())]).to_string();
        let tail_start = content.len().saturating_sub(LITE_READ_BUF_SIZE);
        let tail = String::from_utf8_lossy(&content[tail_start..]).to_string();
        extract_last_json_string_field(&tail, "customTitle")
            .or_else(|| extract_last_json_string_field(&head, "customTitle"))
            .or_else(|| extract_last_json_string_field(&tail, "aiTitle"))
            .or_else(|| extract_last_json_string_field(&head, "aiTitle"))
            .or_else(|| {
                let first = extract_first_prompt_from_head(&head);
                (!first.is_empty()).then_some(first)
            })
    };
    let (forked_session_id, lines) = build_fork_lines(
        transcript,
        content_replacements,
        session_id,
        up_to_message_id,
        title,
        derive_title,
    )?;
    let fork_path = project_dir.join(format!("{forked_session_id}.jsonl"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(fork_path)?;
    file.write_all((lines.join("\n") + "\n").as_bytes())?;
    Ok(ForkSessionResult {
        session_id: forked_session_id,
    })
}

pub(crate) fn build_fork_lines<F>(
    transcript: Vec<Value>,
    content_replacements: Vec<Value>,
    session_id: &str,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
    derive_title: F,
) -> Result<(String, Vec<String>)>
where
    F: FnOnce() -> Option<String>,
{
    let mut transcript: Vec<Value> = transcript
        .into_iter()
        .filter(|e| e.get("isSidechain").and_then(Value::as_bool) != Some(true))
        .collect();
    if transcript.is_empty() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Session {session_id} has no messages to fork"
        )));
    }
    if let Some(up_to) = up_to_message_id {
        let Some(cutoff) = transcript
            .iter()
            .position(|entry| entry.get("uuid").and_then(Value::as_str) == Some(up_to))
        else {
            return Err(ClaudeSdkError::InvalidOptions(format!(
                "Message {up_to} not found in session {session_id}"
            )));
        };
        transcript.truncate(cutoff + 1);
    }

    let mut uuid_mapping = HashMap::new();
    for entry in &transcript {
        if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
            uuid_mapping.insert(uuid.to_string(), Uuid::new_v4().to_string());
        }
    }
    let writable: Vec<Value> = transcript
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) != Some("progress"))
        .cloned()
        .collect();
    if writable.is_empty() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Session {session_id} has no messages to fork"
        )));
    }
    let by_uuid: HashMap<String, Value> = transcript
        .iter()
        .filter_map(|entry| {
            entry
                .get("uuid")
                .and_then(Value::as_str)
                .map(|uuid| (uuid.to_string(), entry.clone()))
        })
        .collect();
    let forked_session_id = Uuid::new_v4().to_string();
    let now = iso_now();
    let mut lines = Vec::new();

    for (i, original) in writable.iter().enumerate() {
        let Some(original_uuid) = original.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        let new_uuid = uuid_mapping
            .get(original_uuid)
            .cloned()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut new_parent_uuid = Value::Null;
        let mut parent_id = original
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        while let Some(parent) = parent_id {
            let Some(parent_entry) = by_uuid.get(&parent) else {
                break;
            };
            if parent_entry.get("type").and_then(Value::as_str) != Some("progress") {
                new_parent_uuid = uuid_mapping
                    .get(&parent)
                    .cloned()
                    .map(Value::String)
                    .unwrap_or(Value::Null);
                break;
            }
            parent_id = parent_entry
                .get("parentUuid")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }

        let timestamp = if i == writable.len() - 1 {
            now.clone()
        } else {
            original
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or(&now)
                .to_string()
        };
        let logical_parent = original.get("logicalParentUuid").and_then(Value::as_str);
        let new_logical_parent = logical_parent
            .and_then(|lp| uuid_mapping.get(lp).cloned())
            .map(Value::String)
            .unwrap_or_else(|| {
                logical_parent
                    .map(|s| Value::String(s.to_string()))
                    .unwrap_or(Value::Null)
            });

        let mut forked = original.as_object().cloned().unwrap_or_default();
        forked.insert("uuid".to_string(), Value::String(new_uuid));
        forked.insert("parentUuid".to_string(), new_parent_uuid);
        forked.insert("logicalParentUuid".to_string(), new_logical_parent);
        forked.insert(
            "sessionId".to_string(),
            Value::String(forked_session_id.clone()),
        );
        forked.insert("timestamp".to_string(), Value::String(timestamp));
        forked.insert("isSidechain".to_string(), Value::Bool(false));
        forked.insert(
            "forkedFrom".to_string(),
            json!({"sessionId": session_id, "messageUuid": original_uuid}),
        );
        for key in ["teamName", "agentName", "slug", "sourceToolAssistantUUID"] {
            forked.remove(key);
        }
        lines.push(Value::Object(forked).to_string());
    }

    if !content_replacements.is_empty() {
        lines.push(
            json!({
                "type": "content-replacement",
                "sessionId": forked_session_id,
                "replacements": content_replacements,
                "uuid": Uuid::new_v4().to_string(),
                "timestamp": now,
            })
            .to_string(),
        );
    }

    let fork_title = title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            format!(
                "{} (fork)",
                derive_title().unwrap_or_else(|| "Forked session".to_string())
            )
        });
    lines.push(
        json!({
            "type": "custom-title",
            "sessionId": forked_session_id,
            "customTitle": fork_title,
            "uuid": Uuid::new_v4().to_string(),
            "timestamp": now,
        })
        .to_string(),
    );

    Ok((forked_session_id, lines))
}

fn parse_fork_transcript(content: &[u8], session_id: &str) -> (Vec<Value>, Vec<Value>) {
    let mut transcript = Vec::new();
    let mut content_replacements = Vec::new();
    let text = String::from_utf8_lossy(content);
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let entry_type = entry.get("type").and_then(Value::as_str);
        if matches!(
            entry_type,
            Some("user" | "assistant" | "attachment" | "system" | "progress")
        ) && entry.get("uuid").and_then(Value::as_str).is_some()
        {
            transcript.push(entry);
        } else if entry_type == Some("content-replacement")
            && entry.get("sessionId").and_then(Value::as_str) == Some(session_id)
        {
            if let Some(replacements) = entry.get("replacements").and_then(Value::as_array) {
                content_replacements.extend(replacements.clone());
            }
        }
    }
    (transcript, content_replacements)
}

fn append_to_session(session_id: &str, data: &str, directory: Option<&Path>) -> Result<()> {
    let file_name = format!("{session_id}.jsonl");

    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical) {
            if try_append(&project_dir.join(&file_name), data)? {
                return Ok(());
            }
        }
        for worktree in get_worktree_paths(&canonical) {
            if worktree == canonical {
                continue;
            }
            if let Some(project_dir) = find_project_dir(&worktree) {
                if try_append(&project_dir.join(&file_name), data)? {
                    return Ok(());
                }
            }
        }
        return Err(ClaudeSdkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Session {session_id} not found in project directory for {}",
                directory.display()
            ),
        )));
    }

    let projects_dir = get_projects_dir(None);
    let dirents = fs::read_dir(&projects_dir).map_err(|err| {
        ClaudeSdkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Session {session_id} not found (no projects directory): {err}"),
        ))
    })?;
    for entry in dirents.flatten() {
        if try_append(&entry.path().join(&file_name), data)? {
            return Ok(());
        }
    }
    Err(ClaudeSdkError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("Session {session_id} not found in any project directory"),
    )))
}

pub fn try_append(path: &Path, data: &str) -> Result<bool> {
    let mut file = match OpenOptions::new().append(true).open(path) {
        Ok(file) => file,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(err) => return Err(err.into()),
    };
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.write_all(data.as_bytes())?;
    Ok(true)
}

pub fn sanitize_unicode(value: &str) -> String {
    let mut current = value.to_string();
    for _ in 0..10 {
        let previous = current.clone();
        current = current
            .nfkc()
            .filter(|c| !is_dangerous_unicode(*c))
            .collect();
        if current == previous {
            break;
        }
    }
    current
}

fn is_dangerous_unicode(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::Format | GeneralCategory::PrivateUse | GeneralCategory::Unassigned
    ) || matches!(
        c as u32,
        0x200B..=0x200F | 0x202A..=0x202E | 0x2066..=0x2069 | 0xFEFF | 0xE000..=0xF8FF
    )
}

fn iso_now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub async fn rename_session_via_store(
    session_store: &dyn SessionStore,
    session_id: &str,
    title: &str,
    directory: Option<&Path>,
) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(ClaudeSdkError::InvalidOptions(
            "title must be non-empty".to_string(),
        ));
    }
    let key = SessionKey {
        project_key: project_key_for_directory(directory),
        session_id: session_id.to_string(),
        subpath: None,
    };
    session_store
        .append(
            key,
            vec![json!({
                "type": "custom-title",
                "customTitle": stripped,
                "sessionId": session_id,
                "uuid": Uuid::new_v4().to_string(),
                "timestamp": iso_now(),
            })],
        )
        .await
}

pub async fn tag_session_via_store(
    session_store: &dyn SessionStore,
    session_id: &str,
    tag: Option<&str>,
    directory: Option<&Path>,
) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let tag = match tag {
        Some(tag) => {
            let sanitized = sanitize_unicode(tag).trim().to_string();
            if sanitized.is_empty() {
                return Err(ClaudeSdkError::InvalidOptions(
                    "tag must be non-empty (use None to clear)".to_string(),
                ));
            }
            sanitized
        }
        None => String::new(),
    };
    let key = SessionKey {
        project_key: project_key_for_directory(directory),
        session_id: session_id.to_string(),
        subpath: None,
    };
    session_store
        .append(
            key,
            vec![json!({
                "type": "tag",
                "tag": tag,
                "sessionId": session_id,
                "uuid": Uuid::new_v4().to_string(),
                "timestamp": iso_now(),
            })],
        )
        .await
}

pub async fn delete_session_via_store(
    session_store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    if !session_store.supports_delete() {
        return Ok(());
    }
    session_store
        .delete(SessionKey {
            project_key: project_key_for_directory(directory),
            session_id: session_id.to_string(),
            subpath: None,
        })
        .await
}

pub async fn fork_session_via_store(
    session_store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&Path>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<ForkSessionResult> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    if let Some(up_to) = up_to_message_id {
        if validate_uuid(up_to).is_none() {
            return Err(ClaudeSdkError::InvalidOptions(format!(
                "Invalid up_to_message_id: {up_to}"
            )));
        }
    }
    let project_key = project_key_for_directory(directory);
    let src_key = SessionKey {
        project_key: project_key.clone(),
        session_id: session_id.to_string(),
        subpath: None,
    };
    let Some(loaded) = session_store.load(src_key).await? else {
        return Err(ClaudeSdkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Session {session_id} not found"),
        )));
    };
    let mut transcript = Vec::new();
    let mut content_replacements = Vec::new();
    for entry in &loaded {
        let entry_type = entry.get("type").and_then(Value::as_str);
        if matches!(
            entry_type,
            Some("user" | "assistant" | "attachment" | "system" | "progress")
        ) && entry.get("uuid").and_then(Value::as_str).is_some()
        {
            transcript.push(entry.clone());
        } else if entry_type == Some("content-replacement")
            && entry.get("sessionId").and_then(Value::as_str) == Some(session_id)
        {
            if let Some(replacements) = entry.get("replacements").and_then(Value::as_array) {
                content_replacements.extend(replacements.clone());
            }
        }
    }
    let derive_title = || derive_title_from_entries(&loaded);
    let (forked_session_id, lines) = build_fork_lines(
        transcript,
        content_replacements,
        session_id,
        up_to_message_id,
        title,
        derive_title,
    )?;
    let dst_key = SessionKey {
        project_key,
        session_id: forked_session_id.clone(),
        subpath: None,
    };
    let entries: Vec<Value> = lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    session_store.append(dst_key, entries).await?;
    Ok(ForkSessionResult {
        session_id: forked_session_id,
    })
}

fn derive_title_from_entries(raw: &[Value]) -> Option<String> {
    let mut custom = None;
    let mut ai = None;
    for entry in raw {
        if let Some(ct) = entry
            .get("customTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            custom = Some(ct.to_string());
        }
        if let Some(at) = entry
            .get("aiTitle")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            ai = Some(at.to_string());
        }
    }
    custom.or(ai).or_else(|| {
        let jsonl = raw
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let first = extract_first_prompt_from_head(&jsonl);
        (!first.is_empty()).then_some(first)
    })
}
