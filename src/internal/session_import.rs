use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::errors::{ClaudeSdkError, Result};
use crate::types::{SessionKey, SessionStore};

use super::sessions::{resolve_session_file_path, validate_uuid};
use super::transcript_mirror_batcher::{MAX_PENDING_BYTES, MAX_PENDING_ENTRIES};

pub async fn import_session_to_store(
    session_id: &str,
    store: &dyn SessionStore,
    directory: Option<&Path>,
    include_subagents: bool,
    batch_size: Option<usize>,
) -> Result<()> {
    if validate_uuid(session_id).is_none() {
        return Err(ClaudeSdkError::InvalidOptions(format!(
            "Invalid session_id: {session_id}"
        )));
    }
    let Some(resolved) = resolve_session_file_path(session_id, directory) else {
        return Err(ClaudeSdkError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Session {session_id} not found"),
        )));
    };
    let project_key = resolved
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let batch_size = batch_size.filter(|s| *s > 0).unwrap_or(MAX_PENDING_ENTRIES);
    let main_key = SessionKey {
        project_key: project_key.clone(),
        session_id: session_id.to_string(),
        subpath: None,
    };
    append_jsonl_file_in_batches(&resolved, main_key, store, batch_size).await?;

    if !include_subagents {
        return Ok(());
    }

    let session_dir = resolved.with_extension("");
    let subagents_dir = session_dir.join("subagents");
    for file_path in collect_jsonl_files(&subagents_dir) {
        let Ok(rel) = file_path.strip_prefix(&session_dir) else {
            continue;
        };
        let mut parts: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        if let Some(last) = parts.last_mut() {
            *last = last.trim_end_matches(".jsonl").to_string();
        }
        let sub_key = SessionKey {
            project_key: project_key.clone(),
            session_id: session_id.to_string(),
            subpath: Some(parts.join("/")),
        };
        append_jsonl_file_in_batches(&file_path, sub_key.clone(), store, batch_size).await?;
        let meta_path = file_path.with_file_name(format!(
            "{}.meta.json",
            file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("agent")
        ));
        match fs::read_to_string(&meta_path) {
            Ok(meta_text) => {
                let meta = serde_json::from_str::<Value>(&meta_text)?;
                let Value::Object(obj) = meta else {
                    return Err(ClaudeSdkError::InvalidOptions(format!(
                        "Subagent metadata sidecar must be a JSON object: {}",
                        meta_path.display()
                    )));
                };
                let mut entry = serde_json::Map::new();
                entry.insert(
                    "type".to_string(),
                    Value::String("agent_metadata".to_string()),
                );
                entry.extend(obj);
                store.append(sub_key, vec![Value::Object(entry)]).await?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

async fn append_jsonl_file_in_batches(
    file_path: &Path,
    key: SessionKey,
    store: &dyn SessionStore,
    batch_size: usize,
) -> Result<()> {
    let file = fs::File::open(file_path)?;
    let mut batch = Vec::new();
    let mut nbytes = 0;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        batch.push(serde_json::from_str::<Value>(line)?);
        nbytes += line.len();
        if batch.len() >= batch_size || nbytes >= MAX_PENDING_BYTES {
            store
                .append(key.clone(), std::mem::take(&mut batch))
                .await?;
            nbytes = 0;
        }
    }
    if !batch.is_empty() {
        store.append(key, batch).await?;
    }
    Ok(())
}

fn collect_jsonl_files(base_dir: &Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    fn walk(dir: &Path, results: &mut Vec<PathBuf>) {
        let Ok(mut entries): std::io::Result<Vec<_>> =
            fs::read_dir(dir).map(|it| it.flatten().collect())
        else {
            return;
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, results);
            } else if path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.ends_with(".jsonl"))
            {
                results.push(path);
            }
        }
    }
    walk(base_dir, &mut results);
    results
}
