use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use claude_agent_sdk::internal::session_mutations::{sanitize_unicode, try_append};
use claude_agent_sdk::internal::sessions::sanitize_path;
use claude_agent_sdk::{
    delete_session, fork_session, get_session_messages, list_sessions, rename_session, tag_session,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    _guard: MutexGuard<'static, ()>,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set_claude_config_dir(path: &Path) -> Self {
        let guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", path);
        }
        Self {
            _guard: guard,
            previous,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
                None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
            }
        }
    }
}

struct TestEnv {
    _guard: EnvVarGuard,
    _tmp: TempDir,
    config_dir: PathBuf,
}

fn test_env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".claude");
    fs::create_dir_all(config_dir.join("projects")).unwrap();
    let guard = EnvVarGuard::set_claude_config_dir(&config_dir);
    TestEnv {
        _guard: guard,
        _tmp: tmp,
        config_dir,
    }
}

fn make_project_dir(config_dir: &Path, project_path: &Path) -> PathBuf {
    fs::create_dir_all(project_path).unwrap();
    let canonical = fs::canonicalize(project_path).unwrap();
    let project_dir = config_dir
        .join("projects")
        .join(sanitize_path(&canonical.to_string_lossy()));
    fs::create_dir_all(&project_dir).unwrap();
    project_dir
}

fn make_session_file(
    project_dir: &Path,
    session_id: Option<String>,
    first_prompt: &str,
) -> (String, PathBuf) {
    let sid = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let file_path = project_dir.join(format!("{sid}.jsonl"));
    let lines = [
        json!({"type": "user", "message": {"role": "user", "content": first_prompt}}).to_string(),
        json!({"type": "assistant", "message": {"role": "assistant", "content": "Hi!"}})
            .to_string(),
    ];
    fs::write(&file_path, lines.join("\n") + "\n").unwrap();
    (sid, file_path)
}

fn read_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .trim()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn try_append_matches_python_file_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("test.jsonl");
    fs::write(&file, "line1\n").unwrap();
    assert!(try_append(&file, "line2\n").unwrap());
    assert_eq!(fs::read_to_string(&file).unwrap(), "line1\nline2\n");

    assert!(!try_append(&tmp.path().join("missing.jsonl"), "data\n").unwrap());
    assert!(!tmp.path().join("missing.jsonl").exists());
    assert!(!try_append(&tmp.path().join("missing").join("file.jsonl"), "data\n").unwrap());

    let stub = tmp.path().join("stub.jsonl");
    fs::write(&stub, "").unwrap();
    assert!(!try_append(&stub, "data\n").unwrap());
    assert_eq!(fs::read_to_string(&stub).unwrap(), "");

    assert!(try_append(&file, "line3\n").unwrap());
    assert_eq!(fs::read_to_string(&file).unwrap(), "line1\nline2\nline3\n");
}

#[test]
fn rename_session_appends_compact_custom_title_and_list_sessions_last_wins() {
    let env = test_env();
    let project_path = env._tmp.path().join("proj");
    let project_dir = make_project_dir(&env.config_dir, &project_path);
    let (sid, file_path) = make_session_file(&project_dir, None, "original");

    rename_session(&sid, "  First Title  ", Some(&project_path)).unwrap();
    rename_session(&sid, "Second Title", Some(&project_path)).unwrap();

    let lines = fs::read_to_string(&file_path).unwrap();
    let last_line = lines.trim().lines().last().unwrap();
    assert_eq!(
        last_line,
        format!(r#"{{"type":"custom-title","customTitle":"Second Title","sessionId":"{sid}"}}"#)
    );

    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].custom_title.as_deref(), Some("Second Title"));
    assert_eq!(sessions[0].summary, "Second Title");
}

#[test]
fn rename_session_validates_input_and_searches_all_projects() {
    let env = test_env();
    assert!(
        rename_session("not-a-uuid", "title", None)
            .unwrap_err()
            .to_string()
            .contains("Invalid session_id")
    );

    let project_path = env._tmp.path().join("proj");
    let project_dir = make_project_dir(&env.config_dir, &project_path);
    let (sid, file_path) = make_session_file(&project_dir, None, "hello");
    assert!(
        rename_session(&sid, "   ", Some(&project_path))
            .unwrap_err()
            .to_string()
            .contains("title must be non-empty")
    );
    assert!(
        rename_session(&Uuid::new_v4().to_string(), "title", Some(&project_path))
            .unwrap_err()
            .to_string()
            .contains("not found in project directory")
    );

    rename_session(&sid, "Found Without Dir", None).unwrap();
    assert_eq!(
        read_lines(&file_path).last().unwrap()["customTitle"],
        "Found Without Dir"
    );
}

#[test]
fn rename_session_reports_missing_projects_dir_like_python() {
    let tmp = tempfile::tempdir().unwrap();
    let missing_config_dir = tmp.path().join("does-not-exist");
    let _guard = EnvVarGuard::set_claude_config_dir(&missing_config_dir);
    let err = rename_session(&Uuid::new_v4().to_string(), "title", None).unwrap_err();
    assert!(err.to_string().contains("no projects directory"));
}

#[test]
fn tag_session_appends_compact_tag_trims_clears_and_sanitizes_unicode() {
    let env = test_env();
    let project_path = env._tmp.path().join("proj");
    let project_dir = make_project_dir(&env.config_dir, &project_path);
    let (sid, file_path) = make_session_file(&project_dir, None, "hello");

    tag_session(&sid, Some("  mytag  "), Some(&project_path)).unwrap();
    let last_line = fs::read_to_string(&file_path)
        .unwrap()
        .trim()
        .lines()
        .last()
        .unwrap()
        .to_string();
    assert_eq!(
        last_line,
        format!(r#"{{"type":"tag","tag":"mytag","sessionId":"{sid}"}}"#)
    );

    tag_session(&sid, Some("clean\u{200b}tag\u{feff}"), Some(&project_path)).unwrap();
    assert_eq!(read_lines(&file_path).last().unwrap()["tag"], "cleantag");

    tag_session(&sid, None, Some(&project_path)).unwrap();
    assert_eq!(read_lines(&file_path).last().unwrap()["tag"], "");

    assert!(
        tag_session(&sid, Some("\u{200b}\u{200c}\u{feff}"), Some(&project_path))
            .unwrap_err()
            .to_string()
            .contains("tag must be non-empty")
    );
}

#[test]
fn sanitize_unicode_matches_python_helper_cases() {
    assert_eq!(sanitize_unicode("hello"), "hello");
    assert_eq!(
        sanitize_unicode("tag-with-dashes_123"),
        "tag-with-dashes_123"
    );
    assert_eq!(sanitize_unicode("a\u{200b}b"), "ab");
    assert_eq!(sanitize_unicode("a\u{200c}b"), "ab");
    assert_eq!(sanitize_unicode("a\u{200d}b"), "ab");
    assert_eq!(sanitize_unicode("\u{feff}hello"), "hello");
    assert_eq!(sanitize_unicode("a\u{202a}b\u{202c}c"), "abc");
    assert_eq!(sanitize_unicode("a\u{2066}b\u{2069}c"), "abc");
    assert_eq!(sanitize_unicode("a\u{e000}b"), "ab");
    assert_eq!(sanitize_unicode("a\u{f8ff}b"), "ab");
    assert_eq!(sanitize_unicode("a\u{f0000}b"), "ab");
    assert_eq!(sanitize_unicode("a\u{0378}b"), "ab");
    assert_eq!(sanitize_unicode("\u{ff21}"), "A");
    assert_eq!(
        sanitize_unicode(&format!("a{}b", "\u{200b}".repeat(20))),
        "ab"
    );
}

#[test]
fn delete_session_removes_file_and_subagent_directory() {
    let env = test_env();
    let project_path = env._tmp.path().join("proj");
    let project_dir = make_project_dir(&env.config_dir, &project_path);
    let (sid, file_path) = make_session_file(&project_dir, None, "hello");
    let subagent_dir = project_dir.join(&sid);
    fs::create_dir_all(&subagent_dir).unwrap();
    fs::write(
        subagent_dir.join(format!("{}.jsonl", Uuid::new_v4())),
        "{}\n",
    )
    .unwrap();

    delete_session(&sid, Some(&project_path)).unwrap();
    assert!(!file_path.exists());
    assert!(!subagent_dir.exists());
    assert!(
        delete_session(&Uuid::new_v4().to_string(), Some(&project_path))
            .unwrap_err()
            .to_string()
            .contains("not found")
    );
}

fn make_transcript_session(
    project_dir: &Path,
    session_id: Option<String>,
    num_turns: usize,
) -> (String, PathBuf, Vec<String>) {
    let sid = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let file_path = project_dir.join(format!("{sid}.jsonl"));
    let mut uuids = Vec::new();
    let mut lines = Vec::new();
    let mut parent_uuid: Option<String> = None;

    for i in 0..num_turns {
        let user_uuid = Uuid::new_v4().to_string();
        uuids.push(user_uuid.clone());
        lines.push(json!({
            "type": "user",
            "uuid": user_uuid,
            "parentUuid": parent_uuid,
            "sessionId": sid,
            "timestamp": "2026-03-01T00:00:00Z",
            "message": {"role": "user", "content": format!("Turn {} question", i + 1)},
        }));
        parent_uuid = Some(uuids.last().unwrap().clone());

        let assistant_uuid = Uuid::new_v4().to_string();
        uuids.push(assistant_uuid.clone());
        lines.push(json!({
            "type": "assistant",
            "uuid": assistant_uuid,
            "parentUuid": parent_uuid,
            "sessionId": sid,
            "timestamp": "2026-03-01T00:00:00Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": format!("Turn {} answer", i + 1)}],
            },
        }));
        parent_uuid = Some(uuids.last().unwrap().clone());
    }

    fs::write(
        &file_path,
        lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    (sid, file_path, uuids)
}

#[test]
fn fork_session_creates_new_session_remaps_ids_and_can_slice() {
    let env = test_env();
    let project_path = env._tmp.path().join("proj");
    let project_dir = make_project_dir(&env.config_dir, &project_path);
    let (sid, _, original_uuids) = make_transcript_session(&project_dir, None, 3);

    let result = fork_session(
        &sid,
        Some(&project_path),
        Some(&original_uuids[1]),
        Some("My Fork"),
    )
    .unwrap();
    assert_ne!(result.session_id, sid);
    let fork_path = project_dir.join(format!("{}.jsonl", result.session_id));
    assert!(fork_path.exists());

    let fork_entries = read_lines(&fork_path);
    for entry in &fork_entries {
        assert_eq!(
            entry.get("sessionId").and_then(Value::as_str),
            Some(result.session_id.as_str())
        );
        if matches!(
            entry.get("type").and_then(Value::as_str),
            Some("user" | "assistant")
        ) {
            assert!(!original_uuids.contains(&entry["uuid"].as_str().unwrap().to_string()));
            if let Some(parent) = entry.get("parentUuid").and_then(Value::as_str) {
                assert!(!original_uuids.contains(&parent.to_string()));
            }
            assert_eq!(entry["forkedFrom"]["sessionId"], sid);
        }
    }

    let fork_messages = get_session_messages(&result.session_id, Some(&project_path), None, 0);
    assert_eq!(fork_messages.len(), 2);
    let sessions = list_sessions(Some(&project_path), None, 0, true);
    let fork_info = sessions
        .iter()
        .find(|session| session.session_id == result.session_id)
        .unwrap();
    assert_eq!(fork_info.custom_title.as_deref(), Some("My Fork"));
}

#[test]
fn fork_session_default_title_clears_stale_fields_and_validates_cutoff() {
    let env = test_env();
    let project_path = env._tmp.path().join("proj");
    let project_dir = make_project_dir(&env.config_dir, &project_path);
    let sid = Uuid::new_v4().to_string();
    let file_path = project_dir.join(format!("{sid}.jsonl"));
    fs::write(
        &file_path,
        json!({
            "type": "user",
            "uuid": Uuid::new_v4().to_string(),
            "parentUuid": null,
            "sessionId": sid,
            "timestamp": "2026-03-01T00:00:00Z",
            "teamName": "test-team",
            "agentName": "test-agent",
            "slug": "test-slug",
            "message": {"role": "user", "content": "Hello"},
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    assert!(
        fork_session(&sid, Some(&project_path), Some("not-valid"), None)
            .unwrap_err()
            .to_string()
            .contains("Invalid up_to_message_id")
    );
    assert!(
        fork_session(
            &sid,
            Some(&project_path),
            Some(&Uuid::new_v4().to_string()),
            None
        )
        .unwrap_err()
        .to_string()
        .contains("not found in session")
    );

    let result = fork_session(&sid, Some(&project_path), None, None).unwrap();
    let fork_path = project_dir.join(format!("{}.jsonl", result.session_id));
    let fork_entries = read_lines(&fork_path);
    let forked_user = fork_entries
        .iter()
        .find(|entry| entry.get("type").and_then(Value::as_str) == Some("user"))
        .unwrap();
    assert!(forked_user.get("teamName").is_none());
    assert!(forked_user.get("agentName").is_none());
    assert!(forked_user.get("slug").is_none());

    let sessions = list_sessions(Some(&project_path), None, 0, true);
    let fork_info = sessions
        .iter()
        .find(|session| session.session_id == result.session_id)
        .unwrap();
    assert!(
        fork_info
            .custom_title
            .as_deref()
            .unwrap()
            .ends_with("(fork)")
    );
}
