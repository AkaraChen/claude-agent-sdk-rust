use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::time::{sleep, timeout};

use crate::errors::{ClaudeSdkError, Result};
use crate::types::{ClaudeAgentOptions, SessionKey, SessionStore, SessionStoreFlushMode};

use super::sessions::{get_projects_dir, project_key_for_directory, validate_uuid};
use super::transcript_mirror_batcher::{
    MAX_PENDING_BYTES, MAX_PENDING_ENTRIES, MirrorErrorCallback, TranscriptMirrorBatcher,
};

const KEYCHAIN_SERVICE_NAME: &str = "Claude Code-credentials";
const RMTREE_RETRIES: usize = 4;
const RMTREE_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct MaterializedResume {
    pub config_dir: PathBuf,
    pub resume_session_id: String,
}

impl MaterializedResume {
    pub async fn cleanup(&self) {
        rmtree_with_retry(&self.config_dir).await;
    }
}

pub fn apply_materialized_options(
    options: &ClaudeAgentOptions,
    materialized: &MaterializedResume,
) -> ClaudeAgentOptions {
    let mut cloned = options.clone();
    cloned.env.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        materialized.config_dir.to_string_lossy().to_string(),
    );
    cloned.resume = Some(materialized.resume_session_id.clone());
    cloned.continue_conversation = false;
    cloned
}

pub fn build_mirror_batcher(
    store: Arc<dyn SessionStore>,
    materialized: Option<&MaterializedResume>,
    env: Option<&HashMap<String, String>>,
    on_error: MirrorErrorCallback,
    flush_mode: SessionStoreFlushMode,
) -> TranscriptMirrorBatcher {
    let projects_dir = materialized
        .map(|m| m.config_dir.join("projects"))
        .unwrap_or_else(|| get_projects_dir(env))
        .to_string_lossy()
        .to_string();
    let eager = flush_mode == SessionStoreFlushMode::Eager;
    TranscriptMirrorBatcher::new(
        store,
        projects_dir,
        on_error,
        if eager { 0 } else { MAX_PENDING_ENTRIES },
        if eager { 0 } else { MAX_PENDING_BYTES },
    )
}

pub async fn materialize_resume_session(
    options: &ClaudeAgentOptions,
) -> Result<Option<MaterializedResume>> {
    let Some(store) = &options.session_store else {
        return Ok(None);
    };
    if options.resume.is_none() && !options.continue_conversation {
        return Ok(None);
    }

    let timeout_dur = Duration::from_millis(options.load_timeout_ms);
    let project_key = project_key_for_directory(options.cwd.as_deref());

    let resolved = if let Some(resume) = &options.resume {
        if validate_uuid(resume).is_none() {
            return Ok(None);
        }
        load_candidate(store.as_ref(), &project_key, resume, timeout_dur).await?
    } else {
        resolve_continue_candidate(store.as_ref(), &project_key, timeout_dur).await?
    };
    let Some((session_id, entries)) = resolved else {
        return Ok(None);
    };

    let tmp = tempfile::Builder::new()
        .prefix("claude-resume-")
        .tempdir()
        .map_err(ClaudeSdkError::Io)?;
    let tmp_base = tmp.path().to_path_buf();
    let project_dir = tmp_base.join("projects").join(&project_key);

    if let Err(err) = (|| -> Result<()> {
        fs::create_dir_all(&project_dir)?;
        write_jsonl(&project_dir.join(format!("{session_id}.jsonl")), &entries)?;
        copy_auth_files(&tmp_base, &options.env)?;
        Ok(())
    })() {
        rmtree_with_retry(&tmp_base).await;
        return Err(err);
    }

    if store.supports_list_subkeys() {
        if let Err(err) = materialize_subkeys(
            store.as_ref(),
            &project_dir,
            &project_key,
            &session_id,
            timeout_dur,
        )
        .await
        {
            rmtree_with_retry(&tmp_base).await;
            return Err(err);
        }
    }

    let tmp_base = tmp.keep();
    Ok(Some(MaterializedResume {
        config_dir: tmp_base,
        resume_session_id: session_id,
    }))
}

async fn rmtree_with_retry(path: &Path) {
    for attempt in 0..RMTREE_RETRIES {
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => return,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) if attempt + 1 < RMTREE_RETRIES => sleep(RMTREE_RETRY_DELAY).await,
            Err(_) => break,
        }
    }
    let _ = tokio::fs::remove_dir_all(path).await;
}

async fn load_candidate(
    store: &dyn SessionStore,
    project_key: &str,
    session_id: &str,
    timeout_dur: Duration,
) -> Result<Option<(String, Vec<Value>)>> {
    let key = SessionKey {
        project_key: project_key.to_string(),
        session_id: session_id.to_string(),
        subpath: None,
    };
    let entries = with_timeout(
        store.load(key),
        timeout_dur,
        format!("SessionStore.load() for session {session_id}"),
    )
    .await?;
    Ok(entries
        .filter(|e| !e.is_empty())
        .map(|e| (session_id.to_string(), e)))
}

async fn resolve_continue_candidate(
    store: &dyn SessionStore,
    project_key: &str,
    timeout_dur: Duration,
) -> Result<Option<(String, Vec<Value>)>> {
    let sessions = with_timeout(
        store.list_sessions(project_key.to_string()),
        timeout_dur,
        "SessionStore.list_sessions()".to_string(),
    )
    .await?;
    let mut sessions = sessions;
    sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    for cand in sessions {
        if validate_uuid(&cand.session_id).is_none() {
            continue;
        }
        let Some(loaded) =
            load_candidate(store, project_key, &cand.session_id, timeout_dur).await?
        else {
            continue;
        };
        if loaded
            .1
            .first()
            .and_then(Value::as_object)
            .and_then(|o| o.get("isSidechain"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            continue;
        }
        return Ok(Some(loaded));
    }
    Ok(None)
}

async fn with_timeout<F, T>(future: F, timeout_dur: Duration, what: String) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    match timeout(timeout_dur, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(ClaudeSdkError::Runtime(format!(
            "{what} failed during resume materialization: {err}"
        ))),
        Err(_) => Err(ClaudeSdkError::Runtime(format!(
            "{what} timed out after {}ms during resume materialization",
            timeout_dur.as_millis()
        ))),
    }
}

pub(crate) fn write_jsonl(path: &Path, entries: &[Value]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for entry in entries {
        out.push_str(&entry.to_string());
        out.push('\n');
    }
    fs::write(path, out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn copy_auth_files(tmp_base: &Path, opt_env: &HashMap<String, String>) -> Result<()> {
    let caller_config_dir = opt_env
        .get("CLAUDE_CONFIG_DIR")
        .cloned()
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok());
    let source_config_dir = caller_config_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
        .unwrap_or_else(|| PathBuf::from(".claude"));

    let mut creds_json = fs::read_to_string(source_config_dir.join(".credentials.json")).ok();
    if caller_config_dir.is_none()
        && opt_env.get("ANTHROPIC_API_KEY").is_none()
        && std::env::var("ANTHROPIC_API_KEY").is_err()
        && opt_env.get("CLAUDE_CODE_OAUTH_TOKEN").is_none()
        && std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_err()
    {
        if let Some(keychain) = read_keychain_credentials() {
            creds_json = Some(keychain);
        }
    }
    write_redacted_credentials(creds_json.as_deref(), &tmp_base.join(".credentials.json"))?;

    let claude_json_src = caller_config_dir
        .as_ref()
        .map(|d| PathBuf::from(d).join(".claude.json"))
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude.json")))
        .unwrap_or_else(|| PathBuf::from(".claude.json"));
    if claude_json_src.exists() {
        let _ = fs::copy(claude_json_src, tmp_base.join(".claude.json"));
    }
    Ok(())
}

fn write_redacted_credentials(creds_json: Option<&str>, dst: &Path) -> Result<()> {
    let Some(creds_json) = creds_json else {
        return Ok(());
    };
    let out = match serde_json::from_str::<Value>(creds_json) {
        Ok(mut value) => {
            if let Some(oauth) = value
                .get_mut("claudeAiOauth")
                .and_then(Value::as_object_mut)
            {
                oauth.remove("refreshToken");
            }
            value.to_string()
        }
        Err(_) => creds_json.to_string(),
    };
    fs::write(dst, out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dst, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn read_keychain_credentials() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "claude-code-user".to_string());
    let mut child = Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            &user,
            "-w",
            "-s",
            KEYCHAIN_SERVICE_NAME,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                let out = out.trim().to_string();
                return (!out.is_empty()).then_some(out);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

async fn materialize_subkeys(
    store: &dyn SessionStore,
    project_dir: &Path,
    project_key: &str,
    session_id: &str,
    timeout_dur: Duration,
) -> Result<()> {
    let session_dir = project_dir.join(session_id);
    let subkeys = with_timeout(
        store.list_subkeys(crate::types::SessionListSubkeysKey {
            project_key: project_key.to_string(),
            session_id: session_id.to_string(),
        }),
        timeout_dur,
        format!("SessionStore.list_subkeys() for session {session_id}"),
    )
    .await?;

    for subpath in subkeys {
        if !is_safe_subpath(&subpath) {
            tracing::warn!("[SessionStore] skipping unsafe subpath from list_subkeys: {subpath:?}");
            continue;
        }
        let sub_key = SessionKey {
            project_key: project_key.to_string(),
            session_id: session_id.to_string(),
            subpath: Some(subpath.clone()),
        };
        let Some(sub_entries) = with_timeout(
            store.load(sub_key),
            timeout_dur,
            format!("SessionStore.load() for session {session_id} subpath {subpath}"),
        )
        .await?
        else {
            continue;
        };

        let mut metadata = Vec::new();
        let mut transcript = Vec::new();
        for entry in sub_entries {
            if entry.get("type").and_then(Value::as_str) == Some("agent_metadata") {
                metadata.push(entry);
            } else {
                transcript.push(entry);
            }
        }

        let target = session_dir.join(&subpath);
        let sub_file = target.with_file_name(format!(
            "{}.jsonl",
            target
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("agent")
        ));
        if !transcript.is_empty() {
            write_jsonl(&sub_file, &transcript)?;
        }
        if let Some(Value::Object(meta)) = metadata.last() {
            let mut meta_content = meta.clone();
            meta_content.remove("type");
            let meta_file = sub_file.with_file_name(format!(
                "{}.meta.json",
                sub_file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("agent")
            ));
            if let Some(parent) = meta_file.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&meta_file, Value::Object(meta_content).to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(meta_file, fs::Permissions::from_mode(0o600));
            }
        }
    }
    Ok(())
}

fn is_safe_subpath(subpath: &str) -> bool {
    if subpath.is_empty()
        || subpath.contains('\0')
        || subpath.starts_with('/')
        || subpath.starts_with('\\')
    {
        return false;
    }
    if subpath.contains(':') {
        return false;
    }
    if subpath
        .split(['/', '\\'])
        .any(|segment| matches!(segment, "." | ".."))
    {
        return false;
    }
    let path = Path::new(subpath);
    if path.is_absolute() {
        return false;
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::CurDir | Component::RootDir | Component::Prefix(_)
        ) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn write_jsonl_preserves_object_order_like_python_json_dumps() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("stream.jsonl");
        let entries = vec![json!({
            "type": "user",
            "uuid": "uuid-0",
            "message": {"role": "user", "content": "line 0 \"q\" \n nl"},
            "nested": {"a": [0, 1], "b": null},
        })];

        write_jsonl(&out, &entries).unwrap();

        let written = std::fs::read_to_string(&out).unwrap();
        assert!(written.starts_with(r#"{"type":"user","uuid":"uuid-0","message":"#));
        assert!(written.contains(r#""nested":{"a":[0,1],"b":null}"#));
        assert!(written.ends_with('\n'));
        let parsed = written
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(parsed, entries);
    }
}
