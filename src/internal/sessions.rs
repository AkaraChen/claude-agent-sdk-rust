use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use futures::{StreamExt, stream};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

use crate::errors::{ClaudeSdkError, Result};
use crate::types::{
    SdkSessionInfo, SessionKey, SessionListSubkeysKey, SessionMessage, SessionStore,
    SessionStoreListEntry,
};

use super::session_summary::summary_entry_to_sdk_info;

pub const LITE_READ_BUF_SIZE: usize = 65_536;
pub const STORE_LIST_LOAD_CONCURRENCY: usize = 16;
pub const MAX_SANITIZED_LENGTH: usize = 200;

static UUID_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
});

static SKIP_FIRST_PROMPT_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:<local-command-stdout>|<session-start-hook>|<tick>|<goal>|\[Request interrupted by user[^\]]*\]|\s*<ide_opened_file>[\s\S]*</ide_opened_file>\s*$|\s*<ide_selection>[\s\S]*</ide_selection>\s*$)",
    )
    .unwrap()
});

static COMMAND_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<command-name>(.*?)</command-name>").unwrap());

pub(crate) fn skip_first_prompt_regex() -> &'static Regex {
    &SKIP_FIRST_PROMPT_PATTERN
}

pub(crate) fn command_name_regex() -> &'static Regex {
    &COMMAND_NAME_RE
}

#[derive(Debug, Clone)]
pub struct LiteSessionFile {
    pub mtime: i64,
    pub size: u64,
    pub head: String,
    pub tail: String,
}

pub fn validate_uuid(maybe_uuid: &str) -> Option<String> {
    UUID_RE.is_match(maybe_uuid).then(|| maybe_uuid.to_string())
}

pub fn simple_hash(s: &str) -> String {
    let mut h: i32 = 0;
    for ch in s.chars() {
        h = h.wrapping_shl(5).wrapping_sub(h).wrapping_add(ch as i32);
    }
    let mut n = h.unsigned_abs();
    if n == 0 {
        return "0".to_string();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize] as char);
        n /= 36;
    }
    out.iter().rev().collect()
}

pub fn sanitize_path(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    format!(
        "{}-{}",
        &sanitized[..MAX_SANITIZED_LENGTH],
        simple_hash(name)
    )
}

pub fn get_claude_config_home_dir() -> PathBuf {
    if let Ok(config_dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !config_dir.is_empty()
    {
        return PathBuf::from(config_dir.nfc().collect::<String>());
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
}

pub fn get_projects_dir(env_override: Option<&HashMap<String, String>>) -> PathBuf {
    if let Some(env_override) = env_override {
        if let Some(config_dir) = env_override
            .get("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
        {
            return PathBuf::from(config_dir.nfc().collect::<String>()).join("projects");
        }
    }
    get_claude_config_home_dir().join("projects")
}

pub fn get_project_dir(project_path: &str) -> PathBuf {
    get_projects_dir(None).join(sanitize_path(project_path))
}

pub fn canonicalize_path(path: impl AsRef<Path>) -> String {
    let raw = path.as_ref();
    fs::canonicalize(raw)
        .unwrap_or_else(|_| realpath_like_fallback(raw))
        .to_string_lossy()
        .nfc()
        .collect()
}

fn realpath_like_fallback(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    normalize_lexically(resolve_existing_prefix(normalize_lexically(absolute)))
}

fn resolve_existing_prefix(path: PathBuf) -> PathBuf {
    let mut missing = Vec::<OsString>::new();
    let mut cursor = path.as_path();
    loop {
        if let Ok(existing) = fs::canonicalize(cursor) {
            let mut resolved = existing;
            for segment in missing.iter().rev() {
                resolved.push(segment);
            }
            return resolved;
        }
        let Some(name) = cursor.file_name() else {
            return path;
        };
        missing.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            return path;
        };
        cursor = parent;
    }
}

fn normalize_lexically(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let can_pop = normalized
                    .components()
                    .last()
                    .is_some_and(|last| matches!(last, Component::Normal(_)));
                if can_pop {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

pub fn find_project_dir(project_path: &str) -> Option<PathBuf> {
    let exact = get_project_dir(project_path);
    if exact.is_dir() {
        return Some(exact);
    }
    let sanitized = sanitize_path(project_path);
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return None;
    }
    let prefix = &sanitized[..MAX_SANITIZED_LENGTH];
    let projects_dir = get_projects_dir(None);
    for entry in fs::read_dir(projects_dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with(&format!("{prefix}-")))
        {
            return Some(path);
        }
    }
    None
}

pub fn unescape_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    serde_json::from_str::<String>(&format!("\"{raw}\"")).unwrap_or_else(|_| raw.to_string())
}

pub fn extract_json_string_field(text: &str, key: &str) -> Option<String> {
    for pattern in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let Some(idx) = text.find(&pattern) else {
            continue;
        };
        let value_start = idx + pattern.len();
        let bytes = text.as_bytes();
        let mut i = value_start;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => return Some(unescape_json_string(&text[value_start..i])),
                _ => i += 1,
            }
        }
    }
    None
}

pub fn extract_last_json_string_field(text: &str, key: &str) -> Option<String> {
    let mut last = None;
    for pattern in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let mut search_from = 0;
        while let Some(rel_idx) = text[search_from..].find(&pattern) {
            let idx = search_from + rel_idx;
            let value_start = idx + pattern.len();
            let bytes = text.as_bytes();
            let mut i = value_start;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'"' => {
                        last = Some(unescape_json_string(&text[value_start..i]));
                        break;
                    }
                    _ => i += 1,
                }
            }
            search_from = i.saturating_add(1);
            if search_from >= text.len() {
                break;
            }
        }
    }
    last
}

fn extract_last_tag_from_tail(tail: &str) -> Option<String> {
    tail.lines().rev().find_map(|line| {
        if !line.starts_with(r#"{"type":"tag""#) {
            return None;
        }
        extract_last_json_string_field(line, "tag")
    })
}

pub fn extract_first_prompt_from_head(head: &str) -> String {
    let mut command_fallback = String::new();
    for line in head.lines() {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"")
            || line.contains("\"isMeta\":true")
            || line.contains("\"isMeta\": true")
            || line.contains("\"isCompactSummary\":true")
            || line.contains("\"isCompactSummary\": true")
        {
            continue;
        }
        let Ok(Value::Object(entry)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(message) = entry.get("message").and_then(Value::as_object) else {
            continue;
        };
        let texts: Vec<String> = match message.get("content") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| {
                    (b.get("type").and_then(Value::as_str) == Some("text"))
                        .then(|| {
                            b.get("text")
                                .and_then(Value::as_str)
                                .map(ToString::to_string)
                        })
                        .flatten()
                })
                .collect(),
            _ => Vec::new(),
        };
        for raw in texts {
            let mut result = raw.replace('\n', " ").trim().to_string();
            if result.is_empty() {
                continue;
            }
            if let Some(caps) = COMMAND_NAME_RE.captures(&result) {
                if command_fallback.is_empty() {
                    command_fallback = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                }
                continue;
            }
            if SKIP_FIRST_PROMPT_PATTERN.is_match(&result) {
                continue;
            }
            if result.chars().count() > 200 {
                result = result
                    .chars()
                    .take(200)
                    .collect::<String>()
                    .trim_end()
                    .to_string();
                result.push('\u{2026}');
            }
            return result;
        }
    }
    command_fallback
}

pub fn read_session_lite(file_path: &Path) -> Option<LiteSessionFile> {
    let mut file = fs::File::open(file_path).ok()?;
    let stat = file.metadata().ok()?;
    let size = stat.len();
    if size == 0 {
        return None;
    }
    let mtime = stat
        .modified()
        .ok()
        .and_then(system_time_to_epoch_ms)
        .unwrap_or_default();
    let mut head_bytes = vec![0; LITE_READ_BUF_SIZE.min(size as usize)];
    file.read_exact(&mut head_bytes).ok()?;
    let head = String::from_utf8_lossy(&head_bytes).to_string();

    let tail = if size as usize <= LITE_READ_BUF_SIZE {
        head.clone()
    } else {
        file.seek(SeekFrom::Start(
            size.saturating_sub(LITE_READ_BUF_SIZE as u64),
        ))
        .ok()?;
        let mut tail_bytes = Vec::new();
        file.read_to_end(&mut tail_bytes).ok()?;
        String::from_utf8_lossy(&tail_bytes).to_string()
    };

    Some(LiteSessionFile {
        mtime,
        size,
        head,
        tail,
    })
}

pub fn get_worktree_paths(cwd: &str) -> Vec<String> {
    let Ok(mut child) = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return Vec::new(),
        }
    };
    if !status.success() {
        return Vec::new();
    }

    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut stdout);
    }
    String::from_utf8_lossy(&stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|path| path.nfc().collect())
        .collect()
}

pub fn parse_session_info_from_lite(
    session_id: &str,
    lite: &LiteSessionFile,
    project_path: Option<&str>,
) -> Option<SdkSessionInfo> {
    let first_line = lite.head.lines().next().unwrap_or(&lite.head);
    if first_line.contains("\"isSidechain\":true") || first_line.contains("\"isSidechain\": true") {
        return None;
    }

    let custom_title = extract_last_json_string_field(&lite.tail, "customTitle")
        .or_else(|| extract_last_json_string_field(&lite.head, "customTitle"))
        .or_else(|| extract_last_json_string_field(&lite.tail, "aiTitle"))
        .or_else(|| extract_last_json_string_field(&lite.head, "aiTitle"));
    let first_prompt = non_empty(extract_first_prompt_from_head(&lite.head));
    let summary = custom_title
        .clone()
        .or_else(|| extract_last_json_string_field(&lite.tail, "lastPrompt"))
        .or_else(|| extract_last_json_string_field(&lite.tail, "summary"))
        .or_else(|| first_prompt.clone())?;

    let git_branch = extract_last_json_string_field(&lite.tail, "gitBranch")
        .or_else(|| extract_json_string_field(&lite.head, "gitBranch"));
    let cwd = extract_json_string_field(&lite.head, "cwd")
        .or_else(|| project_path.map(ToString::to_string));
    let tag = extract_last_tag_from_tail(&lite.tail).filter(|s| !s.is_empty());
    let created_at = extract_json_string_field(&lite.head, "timestamp")
        .and_then(|ts| DateTime::parse_from_rfc3339(&ts.replace('Z', "+00:00")).ok())
        .map(|dt| dt.timestamp_millis());

    Some(SdkSessionInfo {
        session_id: session_id.to_string(),
        summary,
        last_modified: lite.mtime,
        file_size: Some(lite.size),
        custom_title,
        first_prompt,
        git_branch,
        cwd,
        tag,
        created_at,
    })
}

fn read_sessions_from_dir(project_dir: &Path, project_path: Option<&str>) -> Vec<SdkSessionInfo> {
    let mut results = Vec::new();
    let Ok(entries) = fs::read_dir(project_dir) else {
        return results;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Some(session_id) = validate_uuid(stem) else {
            continue;
        };
        let Some(lite) = read_session_lite(&path) else {
            continue;
        };
        if let Some(info) = parse_session_info_from_lite(&session_id, &lite, project_path) {
            results.push(info);
        }
    }
    results
}

fn deduplicate_by_session_id(sessions: Vec<SdkSessionInfo>) -> Vec<SdkSessionInfo> {
    let mut by_id: HashMap<String, SdkSessionInfo> = HashMap::new();
    for session in sessions {
        match by_id.get(&session.session_id) {
            Some(existing) if existing.last_modified >= session.last_modified => {}
            _ => {
                by_id.insert(session.session_id.clone(), session);
            }
        }
    }
    by_id.into_values().collect()
}

fn apply_sort_limit_offset(
    mut sessions: Vec<SdkSessionInfo>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SdkSessionInfo> {
    sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    let sessions = if offset > 0 {
        sessions.into_iter().skip(offset).collect()
    } else {
        sessions
    };
    if let Some(limit) = limit.filter(|l| *l > 0) {
        sessions.into_iter().take(limit).collect()
    } else {
        sessions
    }
}

fn list_sessions_for_project(
    directory: &Path,
    limit: Option<usize>,
    offset: usize,
    include_worktrees: bool,
) -> Vec<SdkSessionInfo> {
    let canonical_dir = canonicalize_path(directory);
    let worktree_paths = if include_worktrees {
        get_worktree_paths(&canonical_dir)
    } else {
        Vec::new()
    };
    if worktree_paths.len() <= 1 {
        let Some(project_dir) = find_project_dir(&canonical_dir) else {
            return Vec::new();
        };
        return apply_sort_limit_offset(
            read_sessions_from_dir(&project_dir, Some(&canonical_dir)),
            limit,
            offset,
        );
    }

    let projects_dir = get_projects_dir(None);
    let mut indexed: Vec<(String, String)> = worktree_paths
        .iter()
        .map(|wt| (wt.clone(), sanitize_path(wt)))
        .collect();
    indexed.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let Ok(all_dirents) = fs::read_dir(projects_dir) else {
        let Some(project_dir) = find_project_dir(&canonical_dir) else {
            return Vec::new();
        };
        return apply_sort_limit_offset(
            read_sessions_from_dir(&project_dir, Some(&canonical_dir)),
            limit,
            offset,
        );
    };

    let mut all_sessions = Vec::new();
    let mut seen_dirs = HashSet::new();
    if let Some(project_dir) = find_project_dir(&canonical_dir) {
        if let Some(name) = project_dir.file_name().and_then(|n| n.to_str()) {
            seen_dirs.insert(name.to_string());
        }
        all_sessions.extend(read_sessions_from_dir(&project_dir, Some(&canonical_dir)));
    }

    for entry in all_dirents.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(dir_name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(ToString::to_string)
        else {
            continue;
        };
        if seen_dirs.contains(&dir_name) {
            continue;
        }
        for (wt_path, prefix) in &indexed {
            let is_match = dir_name == *prefix
                || (prefix.len() >= MAX_SANITIZED_LENGTH
                    && dir_name.starts_with(&format!("{prefix}-")));
            if is_match {
                seen_dirs.insert(dir_name.clone());
                all_sessions.extend(read_sessions_from_dir(&path, Some(wt_path)));
                break;
            }
        }
    }

    apply_sort_limit_offset(deduplicate_by_session_id(all_sessions), limit, offset)
}

fn list_all_sessions(limit: Option<usize>, offset: usize) -> Vec<SdkSessionInfo> {
    let Ok(project_dirs) = fs::read_dir(get_projects_dir(None)) else {
        return Vec::new();
    };
    let mut all_sessions = Vec::new();
    for entry in project_dirs.flatten() {
        let path = entry.path();
        if path.is_dir() {
            all_sessions.extend(read_sessions_from_dir(&path, None));
        }
    }
    apply_sort_limit_offset(deduplicate_by_session_id(all_sessions), limit, offset)
}

pub fn list_sessions(
    directory: Option<&Path>,
    limit: Option<usize>,
    offset: usize,
    include_worktrees: bool,
) -> Vec<SdkSessionInfo> {
    match directory {
        Some(directory) => list_sessions_for_project(directory, limit, offset, include_worktrees),
        None => list_all_sessions(limit, offset),
    }
}

pub fn get_session_info(session_id: &str, directory: Option<&Path>) -> Option<SdkSessionInfo> {
    let uuid = validate_uuid(session_id)?;
    let file_name = format!("{uuid}.jsonl");
    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical) {
            if let Some(lite) = read_session_lite(&project_dir.join(&file_name)) {
                return parse_session_info_from_lite(&uuid, &lite, Some(&canonical));
            }
        }
        for wt in get_worktree_paths(&canonical) {
            if wt == canonical {
                continue;
            }
            if let Some(project_dir) = find_project_dir(&wt) {
                if let Some(lite) = read_session_lite(&project_dir.join(&file_name)) {
                    return parse_session_info_from_lite(&uuid, &lite, Some(&wt));
                }
            }
        }
        return None;
    }
    for entry in fs::read_dir(get_projects_dir(None)).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(lite) = read_session_lite(&path.join(&file_name)) {
                return parse_session_info_from_lite(&uuid, &lite, None);
            }
        }
    }
    None
}

const TRANSCRIPT_ENTRY_TYPES: &[&str] = &["user", "assistant", "progress", "system", "attachment"];

fn try_read_session_file(project_dir: &Path, file_name: &str) -> Option<String> {
    fs::read_to_string(project_dir.join(file_name))
        .ok()
        .filter(|s| !s.is_empty())
}

fn read_session_file(session_id: &str, directory: Option<&Path>) -> Option<String> {
    let file_name = format!("{session_id}.jsonl");
    if let Some(directory) = directory {
        let canonical_dir = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical_dir) {
            if let Some(content) = try_read_session_file(&project_dir, &file_name) {
                return Some(content);
            }
        }
        for wt in get_worktree_paths(&canonical_dir) {
            if wt == canonical_dir {
                continue;
            }
            if let Some(project_dir) = find_project_dir(&wt) {
                if let Some(content) = try_read_session_file(&project_dir, &file_name) {
                    return Some(content);
                }
            }
        }
        return None;
    }
    for entry in fs::read_dir(get_projects_dir(None)).ok()?.flatten() {
        if let Some(content) = try_read_session_file(&entry.path(), &file_name) {
            return Some(content);
        }
    }
    None
}

fn parse_transcript_entries(content: &str) -> Vec<Value> {
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|entry| {
            entry
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| TRANSCRIPT_ENTRY_TYPES.contains(&t))
                && entry.get("uuid").and_then(Value::as_str).is_some()
        })
        .collect()
}

pub fn build_conversation_chain(entries: &[Value]) -> Vec<Value> {
    if entries.is_empty() {
        return Vec::new();
    }
    let mut by_uuid = HashMap::new();
    let mut entry_index = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
            by_uuid.insert(uuid.to_string(), entry.clone());
            entry_index.insert(uuid.to_string(), i);
        }
    }
    let parent_uuids: HashSet<String> = entries
        .iter()
        .filter_map(|e| {
            e.get("parentUuid")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect();
    let terminals: Vec<Value> = entries
        .iter()
        .filter(|e| {
            e.get("uuid")
                .and_then(Value::as_str)
                .is_some_and(|uuid| !parent_uuids.contains(uuid))
        })
        .cloned()
        .collect();
    let mut leaves = Vec::new();
    for terminal in terminals {
        let mut current = Some(terminal);
        let mut seen = HashSet::new();
        while let Some(cur) = current {
            let Some(uid) = cur.get("uuid").and_then(Value::as_str) else {
                break;
            };
            if !seen.insert(uid.to_string()) {
                break;
            }
            if matches!(
                cur.get("type").and_then(Value::as_str),
                Some("user" | "assistant")
            ) {
                leaves.push(cur);
                break;
            }
            current = cur
                .get("parentUuid")
                .and_then(Value::as_str)
                .and_then(|parent| by_uuid.get(parent).cloned());
        }
    }
    if leaves.is_empty() {
        return Vec::new();
    }
    let main_leaves: Vec<Value> = leaves
        .iter()
        .filter(|leaf| {
            leaf.get("isSidechain").and_then(Value::as_bool) != Some(true)
                && leaf.get("teamName").is_none()
                && leaf.get("isMeta").and_then(Value::as_bool) != Some(true)
        })
        .cloned()
        .collect();
    let candidates = if main_leaves.is_empty() {
        leaves
    } else {
        main_leaves
    };
    let leaf = candidates
        .into_iter()
        .max_by_key(|e| {
            e.get("uuid")
                .and_then(Value::as_str)
                .and_then(|uuid| entry_index.get(uuid).copied())
                .unwrap_or(0)
        })
        .unwrap();
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(leaf);
    while let Some(cur) = current {
        let Some(uid) = cur.get("uuid").and_then(Value::as_str) else {
            break;
        };
        if !seen.insert(uid.to_string()) {
            break;
        }
        chain.push(cur.clone());
        current = cur
            .get("parentUuid")
            .and_then(Value::as_str)
            .and_then(|parent| by_uuid.get(parent).cloned());
    }
    chain.reverse();
    chain
}

fn is_visible_message(entry: &Value) -> bool {
    matches!(
        entry.get("type").and_then(Value::as_str),
        Some("user" | "assistant")
    ) && entry.get("isMeta").and_then(Value::as_bool) != Some(true)
        && entry.get("isSidechain").and_then(Value::as_bool) != Some(true)
        && entry.get("teamName").is_none()
}

fn to_session_message(entry: &Value) -> SessionMessage {
    SessionMessage {
        message_type: entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        uuid: entry
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        session_id: entry
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        message: entry.get("message").cloned().unwrap_or(Value::Null),
        parent_tool_use_id: None,
    }
}

pub fn get_session_messages(
    session_id: &str,
    directory: Option<&Path>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if validate_uuid(session_id).is_none() {
        return Vec::new();
    }
    let Some(content) = read_session_file(session_id, directory) else {
        return Vec::new();
    };
    entries_to_session_messages(parse_transcript_entries(&content), limit, offset)
}

pub(crate) fn entries_to_session_messages(
    entries: Vec<Value>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    let messages: Vec<SessionMessage> = build_conversation_chain(&entries)
        .iter()
        .filter(|e| is_visible_message(e))
        .map(to_session_message)
        .collect();
    page(messages, limit, offset)
}

pub(crate) fn resolve_session_file_path(
    session_id: &str,
    directory: Option<&Path>,
) -> Option<PathBuf> {
    let file_name = format!("{session_id}.jsonl");
    let stat_candidate = |project_dir: &Path| -> Option<PathBuf> {
        let candidate = project_dir.join(&file_name);
        candidate
            .metadata()
            .ok()
            .filter(|m| m.len() > 0)
            .map(|_| candidate)
    };
    if let Some(directory) = directory {
        let canonical = canonicalize_path(directory);
        if let Some(project_dir) = find_project_dir(&canonical) {
            if let Some(found) = stat_candidate(&project_dir) {
                return Some(found);
            }
        }
        for wt in get_worktree_paths(&canonical) {
            if wt == canonical {
                continue;
            }
            if let Some(project_dir) = find_project_dir(&wt) {
                if let Some(found) = stat_candidate(&project_dir) {
                    return Some(found);
                }
            }
        }
        return None;
    }
    for entry in fs::read_dir(get_projects_dir(None)).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        if let Some(found) = stat_candidate(&entry.path()) {
            return Some(found);
        }
    }
    None
}

fn resolve_subagents_dir(session_id: &str, directory: Option<&Path>) -> Option<PathBuf> {
    let resolved = resolve_session_file_path(session_id, directory)?;
    Some(resolved.with_extension("").join("subagents"))
}

fn collect_agent_files(base_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut results = Vec::new();
    fn walk(current: &Path, results: &mut Vec<(String, PathBuf)>) {
        let mut dirents: Vec<std::fs::DirEntry> = match fs::read_dir(current) {
            Ok(iter) => iter.flatten().collect(),
            Err(_) => return,
        };
        dirents.sort_by_key(|e| e.file_name());
        for entry in dirents {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if path.is_file() && name.starts_with("agent-") && name.ends_with(".jsonl") {
                results.push((
                    name.trim_start_matches("agent-")
                        .trim_end_matches(".jsonl")
                        .to_string(),
                    path,
                ));
            } else if path.is_dir() {
                walk(&path, results);
            }
        }
    }
    walk(base_dir, &mut results);
    results
}

fn build_subagent_chain(entries: &[Value]) -> Vec<Value> {
    let mut by_uuid = HashMap::new();
    for entry in entries {
        if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
            by_uuid.insert(uuid.to_string(), entry.clone());
        }
    }
    let Some(leaf) = entries
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.get("type").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
        .cloned()
    else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(leaf);
    while let Some(cur) = current {
        let Some(uid) = cur.get("uuid").and_then(Value::as_str) else {
            break;
        };
        if !seen.insert(uid.to_string()) {
            break;
        }
        chain.push(cur.clone());
        current = cur
            .get("parentUuid")
            .and_then(Value::as_str)
            .and_then(|parent| by_uuid.get(parent).cloned());
    }
    chain.reverse();
    chain
}

pub fn list_subagents(session_id: &str, directory: Option<&Path>) -> Vec<String> {
    if validate_uuid(session_id).is_none() {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(session_id, directory) else {
        return Vec::new();
    };
    collect_agent_files(&subagents_dir)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

pub fn get_subagent_messages(
    session_id: &str,
    agent_id: &str,
    directory: Option<&Path>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if validate_uuid(session_id).is_none() || agent_id.is_empty() {
        return Vec::new();
    }
    let Some(subagents_dir) = resolve_subagents_dir(session_id, directory) else {
        return Vec::new();
    };
    let Some((_, file_path)) = collect_agent_files(&subagents_dir)
        .into_iter()
        .find(|(id, _)| id == agent_id)
    else {
        return Vec::new();
    };
    let Ok(content) = fs::read_to_string(file_path) else {
        return Vec::new();
    };
    entries_to_subagent_messages(parse_transcript_entries(&content), limit, offset)
}

pub(crate) fn entries_to_subagent_messages(
    entries: Vec<Value>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    let messages: Vec<SessionMessage> = build_subagent_chain(&entries)
        .iter()
        .filter(|e| {
            matches!(
                e.get("type").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
        .map(to_session_message)
        .collect();
    page(messages, limit, offset)
}

pub fn project_key_for_directory(directory: Option<&Path>) -> String {
    let path = directory.unwrap_or_else(|| Path::new("."));
    sanitize_path(&canonicalize_path(path))
}

pub(crate) fn entries_to_jsonl(entries: &[Value]) -> String {
    let mut lines = Vec::new();
    for entry in entries {
        if let Value::Object(obj) = entry {
            if obj.contains_key("type") {
                lines.push(object_to_json_type_first(obj));
                continue;
            }
        }
        lines.push(entry.to_string());
    }
    lines.join("\n") + "\n"
}

fn object_to_json_type_first(obj: &serde_json::Map<String, Value>) -> String {
    let mut out = String::from("{\"type\":");
    out.push_str(&serde_json::to_string(obj.get("type").unwrap_or(&Value::Null)).unwrap());
    for (key, value) in obj {
        if key == "type" {
            continue;
        }
        out.push(',');
        out.push_str(&serde_json::to_string(key).unwrap());
        out.push(':');
        out.push_str(&serde_json::to_string(value).unwrap());
    }
    out.push('}');
    out
}

fn jsonl_to_lite(jsonl: &str, mtime: i64) -> LiteSessionFile {
    let buf = jsonl.as_bytes();
    let size = buf.len();
    let head = String::from_utf8_lossy(&buf[..LITE_READ_BUF_SIZE.min(size)]).to_string();
    let tail = if size > LITE_READ_BUF_SIZE {
        String::from_utf8_lossy(&buf[size - LITE_READ_BUF_SIZE..]).to_string()
    } else {
        head.clone()
    };
    LiteSessionFile {
        mtime,
        size: size as u64,
        head,
        tail,
    }
}

fn mtime_from_jsonl_tail(jsonl: &str) -> i64 {
    let trimmed = jsonl.trim_end();
    let last_line = trimmed.rsplit('\n').next().unwrap_or(trimmed);
    serde_json::from_str::<Value>(last_line)
        .ok()
        .and_then(|obj| {
            obj.get("timestamp")
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .and_then(|ts| DateTime::parse_from_rfc3339(&ts.replace('Z', "+00:00")).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(now_ms)
}

pub(crate) fn filter_transcript_entries(entries: Vec<Value>) -> Vec<Value> {
    entries
        .into_iter()
        .filter(|e| {
            e.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| TRANSCRIPT_ENTRY_TYPES.contains(&t))
                && e.get("uuid").and_then(Value::as_str).is_some()
        })
        .collect()
}

async fn load_store_entries_as_jsonl(
    store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<Option<String>> {
    let key = SessionKey {
        project_key: project_key_for_directory(directory),
        session_id: session_id.to_string(),
        subpath: None,
    };
    Ok(store
        .load(key)
        .await?
        .map(|entries| entries_to_jsonl(&entries)))
}

async fn derive_infos_via_load(
    session_store: &dyn SessionStore,
    listing: Vec<SessionStoreListEntry>,
    directory: Option<&Path>,
    project_path: &str,
) -> Vec<SdkSessionInfo> {
    stream::iter(listing)
        .map(|entry| async move {
            let jsonl =
                load_store_entries_as_jsonl(session_store, &entry.session_id, directory).await;
            match jsonl {
                Ok(Some(jsonl)) => {
                    let mut parsed = parse_session_info_from_lite(
                        &entry.session_id,
                        &jsonl_to_lite(&jsonl, entry.mtime),
                        Some(project_path),
                    )?;
                    parsed.last_modified = entry.mtime;
                    Some(parsed)
                }
                Ok(None) => None,
                Err(_) => Some(SdkSessionInfo {
                    session_id: entry.session_id,
                    summary: String::new(),
                    last_modified: entry.mtime,
                    file_size: None,
                    custom_title: None,
                    first_prompt: None,
                    git_branch: None,
                    cwd: None,
                    tag: None,
                    created_at: None,
                }),
            }
        })
        .buffered(STORE_LIST_LOAD_CONCURRENCY)
        .filter_map(|info| async move { info })
        .collect()
        .await
}

pub async fn list_sessions_from_store(
    session_store: &dyn SessionStore,
    directory: Option<&Path>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SdkSessionInfo>> {
    let project_path = canonicalize_path(directory.unwrap_or_else(|| Path::new(".")));
    let project_key = sanitize_path(&project_path);
    let has_list_sessions = session_store.supports_list_sessions();

    if session_store.supports_list_session_summaries() {
        match session_store
            .list_session_summaries(project_key.clone())
            .await
        {
            Ok(summaries) => {
                let (listing, known_mtimes) = if has_list_sessions {
                    let listing = session_store.list_sessions(project_key.clone()).await?;
                    let known_mtimes = listing
                        .iter()
                        .map(|e| (e.session_id.clone(), e.mtime))
                        .collect::<HashMap<_, _>>();
                    (listing, known_mtimes)
                } else {
                    (Vec::new(), HashMap::new())
                };
                let mut slots = Vec::<(i64, Option<SdkSessionInfo>, Option<String>)>::new();
                let mut fresh_summary_ids = HashSet::new();
                for summary in summaries {
                    let sid = summary.session_id.clone();
                    if has_list_sessions {
                        let Some(known) = known_mtimes.get(&sid) else {
                            continue;
                        };
                        if summary.mtime < *known {
                            continue;
                        }
                    }
                    if let Some(info) = summary_entry_to_sdk_info(&summary, Some(&project_path)) {
                        slots.push((summary.mtime, Some(info), None));
                    }
                    fresh_summary_ids.insert(sid);
                }
                if has_list_sessions {
                    for entry in listing {
                        if !fresh_summary_ids.contains(&entry.session_id) {
                            slots.push((entry.mtime, None, Some(entry.session_id)));
                        }
                    }
                }
                slots.sort_by(|a, b| b.0.cmp(&a.0));
                let mut page_slots: Vec<_> = slots.into_iter().skip(offset).collect();
                if let Some(limit) = limit.filter(|l| *l > 0) {
                    page_slots.truncate(limit);
                }
                let to_fill = page_slots
                    .iter()
                    .filter_map(|slot| {
                        if slot.1.is_none() {
                            slot.2.as_ref().map(|sid| SessionStoreListEntry {
                                session_id: sid.clone(),
                                mtime: slot.0,
                            })
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                if !to_fill.is_empty() {
                    let filled =
                        derive_infos_via_load(session_store, to_fill, directory, &project_path)
                            .await;
                    let mut by_sid = filled
                        .into_iter()
                        .map(|info| (info.session_id.clone(), info))
                        .collect::<HashMap<_, _>>();
                    for slot in &mut page_slots {
                        if slot.1.is_none()
                            && let Some(sid) = &slot.2
                        {
                            slot.1 = by_sid.remove(sid);
                        }
                    }
                }
                return Ok(page_slots
                    .into_iter()
                    .filter_map(|(_, info, _)| info)
                    .collect());
            }
            Err(err) if is_list_session_summaries_not_implemented(&err) => {}
            Err(err) => return Err(err),
        }
    }

    if !has_list_sessions {
        return Err(ClaudeSdkError::InvalidOptions(
            "session_store implements neither list_session_summaries() nor list_sessions() -- cannot list sessions".to_string(),
        ));
    }
    let listing = session_store.list_sessions(project_key).await?;
    let results = derive_infos_via_load(session_store, listing, directory, &project_path).await;
    Ok(apply_sort_limit_offset(results, limit, offset))
}

fn is_list_session_summaries_not_implemented(err: &ClaudeSdkError) -> bool {
    matches!(
        err,
        ClaudeSdkError::Runtime(message)
            if message == "list_session_summaries is not implemented"
    )
}

pub async fn get_session_info_from_store(
    session_store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<Option<SdkSessionInfo>> {
    if validate_uuid(session_id).is_none() {
        return Ok(None);
    }
    let Some(jsonl) = load_store_entries_as_jsonl(session_store, session_id, directory).await?
    else {
        return Ok(None);
    };
    let lite = jsonl_to_lite(&jsonl, mtime_from_jsonl_tail(&jsonl));
    let project_path = canonicalize_path(directory.unwrap_or_else(|| Path::new(".")));
    Ok(parse_session_info_from_lite(
        session_id,
        &lite,
        Some(&project_path),
    ))
}

pub async fn get_session_messages_from_store(
    session_store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&Path>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SessionMessage>> {
    if validate_uuid(session_id).is_none() {
        return Ok(Vec::new());
    }
    let key = SessionKey {
        project_key: project_key_for_directory(directory),
        session_id: session_id.to_string(),
        subpath: None,
    };
    let Some(entries) = session_store.load(key).await? else {
        return Ok(Vec::new());
    };
    Ok(entries_to_session_messages(
        filter_transcript_entries(entries),
        limit,
        offset,
    ))
}

pub async fn list_subagents_from_store(
    session_store: &dyn SessionStore,
    session_id: &str,
    directory: Option<&Path>,
) -> Result<Vec<String>> {
    if validate_uuid(session_id).is_none() {
        return Ok(Vec::new());
    }
    if !session_store.supports_list_subkeys() {
        return Err(ClaudeSdkError::InvalidOptions(
            "session_store does not implement list_subkeys() -- cannot list subagents".to_string(),
        ));
    }
    let subkeys = session_store
        .list_subkeys(SessionListSubkeysKey {
            project_key: project_key_for_directory(directory),
            session_id: session_id.to_string(),
        })
        .await?;
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for subpath in subkeys {
        if !subpath.starts_with("subagents/") {
            continue;
        }
        let last = subpath.rsplit('/').next().unwrap_or("");
        if let Some(agent_id) = last.strip_prefix("agent-") {
            if seen.insert(agent_id.to_string()) {
                ids.push(agent_id.to_string());
            }
        }
    }
    Ok(ids)
}

pub async fn get_subagent_messages_from_store(
    session_store: &dyn SessionStore,
    session_id: &str,
    agent_id: &str,
    directory: Option<&Path>,
    limit: Option<usize>,
    offset: usize,
) -> Result<Vec<SessionMessage>> {
    if validate_uuid(session_id).is_none() || agent_id.is_empty() {
        return Ok(Vec::new());
    }
    let project_key = project_key_for_directory(directory);
    let mut subpath = format!("subagents/agent-{agent_id}");
    if session_store.supports_list_subkeys() {
        let subkeys = session_store
            .list_subkeys(SessionListSubkeysKey {
                project_key: project_key.clone(),
                session_id: session_id.to_string(),
            })
            .await?;
        let target = format!("agent-{agent_id}");
        let Some(found) = subkeys.into_iter().find(|sk| {
            sk.starts_with("subagents/") && sk.rsplit('/').next() == Some(target.as_str())
        }) else {
            return Ok(Vec::new());
        };
        subpath = found;
    }
    let key = SessionKey {
        project_key,
        session_id: session_id.to_string(),
        subpath: Some(subpath),
    };
    let Some(entries) = session_store.load(key).await? else {
        return Ok(Vec::new());
    };
    let transcript: Vec<Value> = entries
        .into_iter()
        .filter(|e| !(e.get("type").and_then(Value::as_str) == Some("agent_metadata")))
        .collect();
    Ok(entries_to_subagent_messages(
        filter_transcript_entries(transcript),
        limit,
        offset,
    ))
}

fn page<T>(items: Vec<T>, limit: Option<usize>, offset: usize) -> Vec<T> {
    let iter = items.into_iter().skip(offset);
    if let Some(limit) = limit.filter(|l| *l > 0) {
        iter.take(limit).collect()
    } else {
        iter.collect()
    }
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn system_time_to_epoch_ms(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
