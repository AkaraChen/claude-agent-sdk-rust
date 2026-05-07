use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use claude_agent_sdk::internal::sessions::{
    build_conversation_chain, canonicalize_path, extract_first_prompt_from_head,
    extract_json_string_field, extract_last_json_string_field, parse_session_info_from_lite,
    read_session_lite, sanitize_path, simple_hash, validate_uuid,
};
use claude_agent_sdk::{
    SdkSessionInfo, SessionMessage, get_session_info, get_session_messages, get_subagent_messages,
    list_sessions, list_subagents,
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
    tmp: TempDir,
    config_dir: PathBuf,
}

fn test_env() -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".claude");
    fs::create_dir_all(config_dir.join("projects")).unwrap();
    let guard = EnvVarGuard::set_claude_config_dir(&config_dir);
    TestEnv {
        _guard: guard,
        tmp,
        config_dir,
    }
}

#[cfg(unix)]
fn set_mtime_secs(path: &Path, secs: i64) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let times = [
        libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        },
    ];
    let rc = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
    assert_eq!(rc, 0);
}

#[derive(Default)]
struct SessionFileOptions {
    session_id: Option<String>,
    first_prompt: String,
    summary: Option<String>,
    custom_title: Option<String>,
    git_branch: Option<String>,
    cwd: Option<String>,
    is_sidechain: bool,
    is_meta_only: bool,
    mtime_secs: Option<i64>,
}

fn make_project_dir(config_dir: &Path, project_path: &str) -> PathBuf {
    let project_dir = config_dir
        .join("projects")
        .join(sanitize_path(project_path));
    fs::create_dir_all(&project_dir).unwrap();
    project_dir
}

fn make_project_dir_for_existing(config_dir: &Path, project_path: &Path) -> PathBuf {
    fs::create_dir_all(project_path).unwrap();
    let canonical = fs::canonicalize(project_path).unwrap();
    make_project_dir(config_dir, &canonical.to_string_lossy())
}

fn make_session_file(project_dir: &Path, options: SessionFileOptions) -> (String, PathBuf) {
    let sid = options
        .session_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let file_path = project_dir.join(format!("{sid}.jsonl"));
    let first_prompt = if options.first_prompt.is_empty() {
        "Hello Claude".to_string()
    } else {
        options.first_prompt
    };

    let mut first_entry = serde_json::Map::new();
    first_entry.insert("type".to_string(), json!("user"));
    first_entry.insert(
        "message".to_string(),
        json!({"role": "user", "content": first_prompt}),
    );
    if let Some(cwd) = options.cwd {
        first_entry.insert("cwd".to_string(), json!(cwd));
    }
    if let Some(git_branch) = &options.git_branch {
        first_entry.insert("gitBranch".to_string(), json!(git_branch));
    }
    if options.is_sidechain {
        first_entry.insert("isSidechain".to_string(), json!(true));
    }
    if options.is_meta_only {
        first_entry.insert("isMeta".to_string(), json!(true));
    }

    let mut lines = vec![
        Value::Object(first_entry).to_string(),
        json!({"type": "assistant", "message": {"role": "assistant", "content": "Hi there!"}})
            .to_string(),
    ];
    let mut tail = serde_json::Map::new();
    tail.insert("type".to_string(), json!("summary"));
    if let Some(summary) = options.summary {
        tail.insert("summary".to_string(), json!(summary));
    }
    if let Some(custom_title) = options.custom_title {
        tail.insert("customTitle".to_string(), json!(custom_title));
    }
    if let Some(git_branch) = options.git_branch {
        tail.insert("gitBranch".to_string(), json!(git_branch));
    }
    lines.push(Value::Object(tail).to_string());

    fs::write(&file_path, lines.join("\n") + "\n").unwrap();
    #[cfg(unix)]
    if let Some(mtime_secs) = options.mtime_secs {
        set_mtime_secs(&file_path, mtime_secs);
    }
    (sid, file_path)
}

fn compact_tag_line(sid: &str, tag: &str) -> String {
    format!(r#"{{"type":"tag","tag":"{tag}","sessionId":"{sid}"}}"#)
}

fn transcript_entry(
    entry_type: &str,
    entry_uuid: &str,
    parent_uuid: Option<&str>,
    session_id: &str,
    content: Option<Value>,
    extras: Value,
) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert("type".to_string(), json!(entry_type));
    entry.insert("uuid".to_string(), json!(entry_uuid));
    entry.insert(
        "parentUuid".to_string(),
        parent_uuid.map(Value::from).unwrap_or(Value::Null),
    );
    entry.insert("sessionId".to_string(), json!(session_id));
    if let Some(content) = content {
        let role = if entry_type == "assistant" {
            "assistant"
        } else {
            "user"
        };
        entry.insert(
            "message".to_string(),
            json!({"role": role, "content": content}),
        );
    }
    if let Value::Object(obj) = extras {
        entry.extend(obj);
    }
    Value::Object(entry)
}

fn write_transcript(project_dir: &Path, session_id: &str, entries: &[Value]) -> PathBuf {
    let file_path = project_dir.join(format!("{session_id}.jsonl"));
    let body = entries
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(&file_path, body).unwrap();
    file_path
}

fn make_session_with_subagents(
    config_dir: &Path,
    project_path: &Path,
    agent_ids: &[&str],
) -> (String, PathBuf) {
    let project_dir = make_project_dir_for_existing(config_dir, project_path);
    let (sid, _) = make_session_file(&project_dir, SessionFileOptions::default());
    let subagents_dir = project_dir.join(&sid).join("subagents");
    fs::create_dir_all(&subagents_dir).unwrap();
    for agent_id in agent_ids {
        fs::write(
            subagents_dir.join(format!("agent-{agent_id}.jsonl")),
            json!({"type": "user", "uuid": "u", "parentUuid": null}).to_string() + "\n",
        )
        .unwrap();
    }
    (sid, subagents_dir)
}

#[test]
fn helper_functions_match_python_sessions_helpers() {
    assert_eq!(
        validate_uuid("550e8400-e29b-41d4-a716-446655440000").as_deref(),
        Some("550e8400-e29b-41d4-a716-446655440000")
    );
    assert!(validate_uuid("not-a-uuid").is_none());
    assert_eq!(
        sanitize_path("/Users/foo/my-project"),
        "-Users-foo-my-project"
    );
    assert_eq!(sanitize_path("plugin:name:server"), "plugin-name-server");

    let long_path = "/x".repeat(150);
    let sanitized = sanitize_path(&long_path);
    assert!(sanitized.len() > 200);
    assert!(sanitized.starts_with("-x-x"));
    assert_eq!(simple_hash("hello"), simple_hash("hello"));
    assert_ne!(simple_hash("hello"), simple_hash("world"));
    assert_eq!(simple_hash(""), "0");
    let realpath_tmp = tempfile::tempdir().unwrap();
    let missing_with_parent = realpath_tmp
        .path()
        .join("missing")
        .join("..")
        .join("target");
    let expected_realpath = fs::canonicalize(realpath_tmp.path())
        .unwrap()
        .join("target")
        .to_string_lossy()
        .to_string();
    assert_eq!(canonicalize_path(&missing_with_parent), expected_realpath);

    assert_eq!(
        extract_json_string_field(r#"{"foo":"bar","baz":"qux"}"#, "foo").as_deref(),
        Some("bar")
    );
    assert_eq!(
        extract_json_string_field(r#"{"foo": "bar"}"#, "foo").as_deref(),
        Some("bar")
    );
    assert_eq!(
        extract_json_string_field(r#"{"foo":"bar\"baz"}"#, "foo").as_deref(),
        Some("bar\"baz")
    );
    assert!(extract_json_string_field(r#"{"foo":"bar"}"#, "missing").is_none());
    assert_eq!(
        extract_last_json_string_field(
            r#"{"summary":"first"}\n{"summary":"second"}\n{"summary":"third"}"#,
            "summary",
        )
        .as_deref(),
        Some("third")
    );
}

#[test]
fn extract_first_prompt_matches_python_skip_and_fallback_rules() {
    assert_eq!(
        extract_first_prompt_from_head(
            &(json!({"type": "user", "message": {"content": "Hello!"}}).to_string() + "\n"),
        ),
        "Hello!"
    );
    let head = json!({"type": "user", "isMeta": true, "message": {"content": "meta"}}).to_string()
        + "\n"
        + &json!({"type": "user", "message": {"content": "real prompt"}}).to_string()
        + "\n";
    assert_eq!(extract_first_prompt_from_head(&head), "real prompt");

    let tool_result = json!({
        "type": "user",
        "message": {"content": [{"type": "tool_result", "content": "x"}]},
    })
    .to_string()
        + "\n"
        + &json!({"type": "user", "message": {"content": "actual prompt"}}).to_string()
        + "\n";
    assert_eq!(
        extract_first_prompt_from_head(&tool_result),
        "actual prompt"
    );

    let blocks =
        json!({"type": "user", "message": {"content": [{"type": "text", "text": "block prompt"}]}})
            .to_string()
            + "\n";
    assert_eq!(extract_first_prompt_from_head(&blocks), "block prompt");

    let long_prompt = "x".repeat(300);
    let truncated = extract_first_prompt_from_head(
        &(json!({"type": "user", "message": {"content": long_prompt}}).to_string() + "\n"),
    );
    assert!(truncated.chars().count() <= 201);
    assert!(truncated.ends_with('\u{2026}'));

    let command =
        json!({"type": "user", "message": {"content": "<command-name>/help</command-name>stuff"}})
            .to_string()
            + "\n";
    assert_eq!(extract_first_prompt_from_head(&command), "/help");
    assert_eq!(extract_first_prompt_from_head(""), "");
    assert_eq!(
        extract_first_prompt_from_head("{\"type\":\"assistant\"}\n"),
        ""
    );
}

#[test]
fn list_sessions_handles_empty_missing_and_single_session_metadata() {
    let env = test_env();
    assert_eq!(list_sessions(None, None, 0, true), vec![]);

    let project_path = env.tmp.path().join("my-project");
    let project_dir = make_project_dir_for_existing(&env.config_dir, &project_path);
    let canonical = fs::canonicalize(&project_path).unwrap();
    let canonical = canonical.to_string_lossy().to_string();
    let (sid, _) = make_session_file(
        &project_dir,
        SessionFileOptions {
            first_prompt: "What is 2+2?".to_string(),
            git_branch: Some("main".to_string()),
            cwd: Some(project_path.to_string_lossy().to_string()),
            ..Default::default()
        },
    );

    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session.session_id, sid);
    assert_eq!(session.first_prompt.as_deref(), Some("What is 2+2?"));
    assert_eq!(session.summary, "What is 2+2?");
    assert_eq!(session.git_branch.as_deref(), Some("main"));
    assert_eq!(
        session.cwd.as_deref(),
        Some(project_path.to_string_lossy().as_ref())
    );
    assert!(session.file_size.unwrap() > 0);
    assert!(session.last_modified > 0);
    assert!(session.custom_title.is_none());

    let no_session_project = env.tmp.path().join("never-used");
    fs::create_dir_all(&no_session_project).unwrap();
    assert_eq!(
        list_sessions(Some(&no_session_project), None, 0, false),
        vec![]
    );

    let fallback_project = env.tmp.path().join("fallback-cwd");
    let fallback_dir = make_project_dir(&env.config_dir, &canonical);
    make_session_file(
        &fallback_dir,
        SessionFileOptions {
            first_prompt: "no cwd field".to_string(),
            ..Default::default()
        },
    );
    fs::create_dir_all(&fallback_project).unwrap();
    let _ = fallback_project;
}

#[test]
fn list_sessions_summary_precedence_and_filters() {
    let env = test_env();
    let project_path = env.tmp.path().join("proj");
    let project_dir = make_project_dir_for_existing(&env.config_dir, &project_path);

    make_session_file(
        &project_dir,
        SessionFileOptions {
            first_prompt: "original question".to_string(),
            summary: Some("auto summary".to_string()),
            custom_title: Some("My Custom Title".to_string()),
            ..Default::default()
        },
    );
    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert_eq!(sessions[0].summary, "My Custom Title");
    assert_eq!(sessions[0].custom_title.as_deref(), Some("My Custom Title"));
    assert_eq!(
        sessions[0].first_prompt.as_deref(),
        Some("original question")
    );

    fs::remove_dir_all(&project_dir).unwrap();
    fs::create_dir_all(&project_dir).unwrap();
    make_session_file(
        &project_dir,
        SessionFileOptions {
            first_prompt: "question".to_string(),
            summary: Some("better summary".to_string()),
            ..Default::default()
        },
    );
    assert_eq!(
        list_sessions(Some(&project_path), None, 0, false)[0].summary,
        "better summary"
    );

    make_session_file(
        &project_dir,
        SessionFileOptions {
            first_prompt: "sidechain".to_string(),
            is_sidechain: true,
            ..Default::default()
        },
    );
    make_session_file(
        &project_dir,
        SessionFileOptions {
            first_prompt: "ignored meta".to_string(),
            is_meta_only: true,
            ..Default::default()
        },
    );
    fs::write(
        project_dir.join("not-a-uuid.jsonl"),
        json!({"type": "user", "message": {"content": "x"}}).to_string() + "\n",
    )
    .unwrap();
    fs::write(project_dir.join("README.md"), "not a session").unwrap();
    fs::write(project_dir.join(format!("{}.jsonl", Uuid::new_v4())), "").unwrap();

    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].summary, "better summary");
}

#[cfg(unix)]
#[test]
fn list_sessions_sort_limit_offset_all_projects_and_dedupe() {
    let env = test_env();
    let project_path = env.tmp.path().join("proj");
    let project_dir = make_project_dir_for_existing(&env.config_dir, &project_path);
    let old = make_session_file(
        &project_dir,
        SessionFileOptions {
            first_prompt: "old".to_string(),
            mtime_secs: Some(1000),
            ..Default::default()
        },
    )
    .0;
    let new = make_session_file(
        &project_dir,
        SessionFileOptions {
            first_prompt: "new".to_string(),
            mtime_secs: Some(3000),
            ..Default::default()
        },
    )
    .0;
    let mid = make_session_file(
        &project_dir,
        SessionFileOptions {
            first_prompt: "mid".to_string(),
            mtime_secs: Some(2000),
            ..Default::default()
        },
    )
    .0;

    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert_eq!(
        sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![new.as_str(), mid.as_str(), old.as_str()]
    );
    assert_eq!(sessions[0].last_modified, 3_000_000);
    assert_eq!(sessions[1].last_modified, 2_000_000);
    assert_eq!(sessions[2].last_modified, 1_000_000);

    assert_eq!(
        list_sessions(Some(&project_path), Some(2), 0, false).len(),
        2
    );
    let page1 = list_sessions(Some(&project_path), Some(2), 0, false);
    let page2 = list_sessions(Some(&project_path), Some(2), 2, false);
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 1);
    assert!(page1[0].last_modified > page2[0].last_modified);
    assert_eq!(
        list_sessions(Some(&project_path), None, 100, false).len(),
        0
    );
    assert_eq!(
        list_sessions(Some(&project_path), Some(0), 0, false).len(),
        3
    );

    let proj1 = make_project_dir(&env.config_dir, "/path/one");
    let proj2 = make_project_dir(&env.config_dir, "/path/two");
    let shared = Uuid::new_v4().to_string();
    make_session_file(
        &proj1,
        SessionFileOptions {
            session_id: Some(shared.clone()),
            first_prompt: "older".to_string(),
            mtime_secs: Some(4000),
            ..Default::default()
        },
    );
    make_session_file(
        &proj2,
        SessionFileOptions {
            session_id: Some(shared.clone()),
            first_prompt: "newer".to_string(),
            mtime_secs: Some(5000),
            ..Default::default()
        },
    );
    let all = list_sessions(None, None, 0, true);
    let deduped = all.iter().find(|s| s.session_id == shared).unwrap();
    assert_eq!(deduped.first_prompt.as_deref(), Some("newer"));
    assert_eq!(deduped.last_modified, 5_000_000);
}

#[test]
fn list_sessions_worktree_disabled_cwd_tail_branch_tag_and_created_at() {
    let env = test_env();
    let project_path = env.tmp.path().join("main-proj");
    let canonical = {
        fs::create_dir_all(&project_path).unwrap();
        fs::canonicalize(&project_path)
            .unwrap()
            .to_string_lossy()
            .to_string()
    };
    let main_dir = make_project_dir(&env.config_dir, &canonical);
    make_session_file(
        &main_dir,
        SessionFileOptions {
            first_prompt: "main session".to_string(),
            ..Default::default()
        },
    );
    let other_dir = make_project_dir(&env.config_dir, &(canonical.clone() + "-worktree"));
    make_session_file(
        &other_dir,
        SessionFileOptions {
            first_prompt: "worktree session".to_string(),
            ..Default::default()
        },
    );
    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].first_prompt.as_deref(), Some("main session"));
    assert_eq!(sessions[0].cwd.as_deref(), Some(canonical.as_str()));

    fs::remove_dir_all(&main_dir).unwrap();
    fs::create_dir_all(&main_dir).unwrap();
    let sid = Uuid::new_v4().to_string();
    let file_path = main_dir.join(format!("{sid}.jsonl"));
    let lines = [
        json!({
            "type": "permission-mode",
            "permissionMode": "acceptEdits",
        })
        .to_string(),
        json!({
            "type": "user",
            "message": {"content": "hello"},
            "gitBranch": "old-branch",
            "timestamp": "2026-01-15T10:30:00.000Z",
        })
        .to_string(),
        json!({"type": "summary", "gitBranch": "new-branch"}).to_string(),
        compact_tag_line(&sid, "first-tag"),
        compact_tag_line(&sid, "second-tag"),
    ];
    fs::write(&file_path, lines.join("\n") + "\n").unwrap();

    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].git_branch.as_deref(), Some("new-branch"));
    assert_eq!(sessions[0].tag.as_deref(), Some("second-tag"));
    assert_eq!(sessions[0].created_at, Some(1_768_473_000_000));

    fs::write(
        &file_path,
        [
            json!({"type": "user", "message": {"content": "hello"}}).to_string(),
            compact_tag_line(&sid, "old-tag"),
            compact_tag_line(&sid, ""),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert!(sessions[0].tag.is_none());
}

#[test]
fn tag_extraction_ignores_tool_use_inputs_and_lite_helper_parses_fields() {
    let env = test_env();
    let project_path = env.tmp.path().join("proj");
    let project_dir = make_project_dir_for_existing(&env.config_dir, &project_path);
    let sid = Uuid::new_v4().to_string();
    let file_path = project_dir.join(format!("{sid}.jsonl"));
    fs::write(
        &file_path,
        [
            json!({"type": "user", "message": {"content": "tag this v1.0"}, "cwd": "/workspace"})
                .to_string(),
            compact_tag_line(&sid, "real-tag"),
            json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "tool_use",
                        "name": "mcp__docker__build",
                        "input": {"tag": "myapp:v2", "context": "."},
                    }],
                },
            })
            .to_string(),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();

    let sessions = list_sessions(Some(&project_path), None, 0, false);
    assert_eq!(sessions[0].tag.as_deref(), Some("real-tag"));
    let lite = read_session_lite(&file_path).unwrap();
    let info = parse_session_info_from_lite(&sid, &lite, Some("/fallback")).unwrap();
    assert_eq!(info.summary, "tag this v1.0");
    assert_eq!(info.tag.as_deref(), Some("real-tag"));
    assert_eq!(info.cwd.as_deref(), Some("/workspace"));

    fs::write(
        &file_path,
        [
            json!({"type": "user", "message": {"content": "build docker"}}).to_string(),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "input": {"tag": "prod"}}]}})
                .to_string(),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    assert!(
        list_sessions(Some(&project_path), None, 0, false)[0]
            .tag
            .is_none()
    );

    fs::write(
        &file_path,
        [
            json!({"type": "user", "message": {"content": "pretty tag"}}).to_string(),
            r#"{"type": "tag","tag":"space-after-type-colon"}"#.to_string(),
            r#"{"tag":"type-not-first","type":"tag"}"#.to_string(),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    assert!(
        list_sessions(Some(&project_path), None, 0, false)[0]
            .tag
            .is_none()
    );
}

#[test]
fn created_at_invalid_and_offset_timestamps_match_python() {
    let sid = Uuid::new_v4().to_string();
    let tmp = tempfile::tempdir().unwrap();
    let file_path = tmp.path().join(format!("{sid}.jsonl"));
    fs::write(
        &file_path,
        json!({
            "type": "user",
            "message": {"content": "hello"},
            "timestamp": "not-a-valid-iso-date",
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    let lite = read_session_lite(&file_path).unwrap();
    let info = parse_session_info_from_lite(&sid, &lite, None).unwrap();
    assert!(info.created_at.is_none());

    fs::write(
        &file_path,
        json!({
            "type": "user",
            "message": {"content": "hello"},
            "timestamp": "2026-01-15T10:30:00+00:00",
        })
        .to_string()
            + "\n",
    )
    .unwrap();
    let lite = read_session_lite(&file_path).unwrap();
    let info = parse_session_info_from_lite(&sid, &lite, None).unwrap();
    assert_eq!(info.created_at, Some(1_768_473_000_000));
}

#[test]
fn get_session_info_matches_directory_and_all_project_lookup() {
    let env = test_env();
    assert!(get_session_info("not-a-uuid", None).is_none());
    assert!(get_session_info(&Uuid::new_v4().to_string(), None).is_none());

    let project_a = env.tmp.path().join("proj-a");
    let project_b = env.tmp.path().join("proj-b");
    let dir_a = make_project_dir_for_existing(&env.config_dir, &project_a);
    make_project_dir_for_existing(&env.config_dir, &project_b);
    let (sid, _) = make_session_file(
        &dir_a,
        SessionFileOptions {
            first_prompt: "hello".to_string(),
            git_branch: Some("main".to_string()),
            ..Default::default()
        },
    );
    let info = get_session_info(&sid, Some(&project_a)).unwrap();
    assert_eq!(info.session_id, sid);
    assert_eq!(info.summary, "hello");
    assert_eq!(info.git_branch.as_deref(), Some("main"));
    assert!(get_session_info(&sid, Some(&project_b)).is_none());
    assert!(get_session_info(&sid, None).is_some());

    let side_id = make_session_file(
        &dir_a,
        SessionFileOptions {
            first_prompt: "sidechain".to_string(),
            is_sidechain: true,
            ..Default::default()
        },
    )
    .0;
    assert!(get_session_info(&side_id, Some(&project_a)).is_none());
}

#[test]
fn get_session_messages_reconstructs_chain_and_filters_entries() {
    let env = test_env();
    assert_eq!(get_session_messages("not-a-uuid", None, None, 0), vec![]);

    let project_path = env.tmp.path().join("proj");
    let project_dir = make_project_dir_for_existing(&env.config_dir, &project_path);
    let sid = Uuid::new_v4().to_string();
    let u1 = Uuid::new_v4().to_string();
    let meta = Uuid::new_v4().to_string();
    let prog = Uuid::new_v4().to_string();
    let a1 = Uuid::new_v4().to_string();
    let u2 = Uuid::new_v4().to_string();
    let a2 = Uuid::new_v4().to_string();
    let entries = vec![
        transcript_entry("user", &u1, None, &sid, Some(json!("hello")), json!({})),
        transcript_entry(
            "user",
            &meta,
            Some(&u1),
            &sid,
            Some(json!("meta")),
            json!({"isMeta": true}),
        ),
        transcript_entry("progress", &prog, Some(&meta), &sid, None, json!({})),
        transcript_entry(
            "assistant",
            &a1,
            Some(&prog),
            &sid,
            Some(json!("hi!")),
            json!({}),
        ),
        transcript_entry(
            "user",
            &u2,
            Some(&a1),
            &sid,
            Some(json!("thanks")),
            json!({}),
        ),
        transcript_entry(
            "assistant",
            &a2,
            Some(&u2),
            &sid,
            Some(json!("welcome")),
            json!({}),
        ),
    ];
    write_transcript(&project_dir, &sid, &entries);

    let messages = get_session_messages(&sid, Some(&project_path), None, 0);
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages.iter().map(|m| m.uuid.as_str()).collect::<Vec<_>>(),
        vec![u1.as_str(), a1.as_str(), u2.as_str(), a2.as_str()]
    );
    assert_eq!(messages[0].message_type, "user");
    assert_eq!(messages[0].session_id, sid);
    assert_eq!(
        messages[0].message,
        json!({"role": "user", "content": "hello"})
    );
    assert!(messages[0].parent_tool_use_id.is_none());
    assert!(
        messages
            .iter()
            .all(|m| matches!(m.message_type.as_str(), "user" | "assistant"))
    );

    let page = get_session_messages(&sid, Some(&project_path), Some(2), 1);
    assert_eq!(
        page.iter().map(|m| m.uuid.as_str()).collect::<Vec<_>>(),
        vec![a1.as_str(), u2.as_str()]
    );
    assert_eq!(
        get_session_messages(&sid, Some(&project_path), Some(0), 0).len(),
        4
    );
    assert_eq!(
        get_session_messages(&sid, Some(&project_path), None, 100),
        vec![]
    );
}

#[test]
fn get_session_messages_leaf_selection_corrupt_lines_and_cycles() {
    let env = test_env();
    let project_path = env.tmp.path().join("proj");
    let project_dir = make_project_dir_for_existing(&env.config_dir, &project_path);
    let sid = Uuid::new_v4().to_string();
    let root = Uuid::new_v4().to_string();
    let main_leaf = Uuid::new_v4().to_string();
    let side_leaf = Uuid::new_v4().to_string();
    write_transcript(
        &project_dir,
        &sid,
        &[
            transcript_entry("user", &root, None, &sid, Some(json!("root")), json!({})),
            transcript_entry(
                "assistant",
                &side_leaf,
                Some(&root),
                &sid,
                Some(json!("side")),
                json!({"isSidechain": true}),
            ),
            transcript_entry(
                "assistant",
                &main_leaf,
                Some(&root),
                &sid,
                Some(json!("main")),
                json!({}),
            ),
        ],
    );
    let messages = get_session_messages(&sid, Some(&project_path), None, 0);
    assert_eq!(
        messages.iter().map(|m| m.uuid.as_str()).collect::<Vec<_>>(),
        vec![root.as_str(), main_leaf.as_str()]
    );

    let newer_leaf = Uuid::new_v4().to_string();
    write_transcript(
        &project_dir,
        &sid,
        &[
            transcript_entry("user", &root, None, &sid, Some(json!("root")), json!({})),
            transcript_entry(
                "assistant",
                &main_leaf,
                Some(&root),
                &sid,
                Some(json!("old")),
                json!({}),
            ),
            transcript_entry(
                "assistant",
                &newer_leaf,
                Some(&root),
                &sid,
                Some(json!("new")),
                json!({}),
            ),
        ],
    );
    let messages = get_session_messages(&sid, Some(&project_path), None, 0);
    assert_eq!(messages[1].uuid, newer_leaf);

    let progress = Uuid::new_v4().to_string();
    write_transcript(
        &project_dir,
        &sid,
        &[
            transcript_entry("user", &root, None, &sid, Some(json!("hi")), json!({})),
            transcript_entry(
                "assistant",
                &main_leaf,
                Some(&root),
                &sid,
                Some(json!("hello")),
                json!({}),
            ),
            transcript_entry(
                "progress",
                &progress,
                Some(&main_leaf),
                &sid,
                None,
                json!({}),
            ),
        ],
    );
    let messages = get_session_messages(&sid, Some(&project_path), None, 0);
    assert_eq!(messages[1].uuid, main_leaf);

    fs::write(
        project_dir.join(format!("{sid}.jsonl")),
        [
            transcript_entry("user", &root, None, &sid, Some(json!("hi")), json!({})).to_string(),
            "not valid json {{{".to_string(),
            String::new(),
            json!({"type": "summary", "summary": "ignored"}).to_string(),
            transcript_entry(
                "assistant",
                &main_leaf,
                Some(&root),
                &sid,
                Some(json!("hello")),
                json!({}),
            )
            .to_string(),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    assert_eq!(
        get_session_messages(&sid, Some(&project_path), None, 0).len(),
        2
    );

    let cycle_a = Uuid::new_v4().to_string();
    let cycle_b = Uuid::new_v4().to_string();
    write_transcript(
        &project_dir,
        &sid,
        &[
            transcript_entry(
                "user",
                &cycle_a,
                Some(&cycle_b),
                &sid,
                Some(json!("hi")),
                json!({}),
            ),
            transcript_entry(
                "assistant",
                &cycle_b,
                Some(&cycle_a),
                &sid,
                Some(json!("hello")),
                json!({}),
            ),
        ],
    );
    assert_eq!(
        get_session_messages(&sid, Some(&project_path), None, 0),
        vec![]
    );
}

#[test]
fn build_conversation_chain_helper_matches_python_unit_cases() {
    assert_eq!(build_conversation_chain(&[]), Vec::<Value>::new());
    let single = json!({"type": "user", "uuid": "a", "parentUuid": null});
    assert_eq!(
        build_conversation_chain(std::slice::from_ref(&single)),
        vec![single]
    );
    let entries = vec![
        json!({"type": "user", "uuid": "a", "parentUuid": null}),
        json!({"type": "assistant", "uuid": "b", "parentUuid": "a"}),
        json!({"type": "user", "uuid": "c", "parentUuid": "b"}),
    ];
    assert_eq!(
        build_conversation_chain(&entries)
            .iter()
            .map(|e| e["uuid"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    assert_eq!(
        build_conversation_chain(&[
            json!({"type": "progress", "uuid": "a", "parentUuid": null}),
            json!({"type": "progress", "uuid": "b", "parentUuid": "a"}),
        ]),
        Vec::<Value>::new()
    );
}

#[test]
fn session_message_and_session_info_are_constructible() {
    let info = SdkSessionInfo {
        session_id: "abc".to_string(),
        summary: "test".to_string(),
        last_modified: 1000,
        file_size: Some(42),
        custom_title: Some("title".to_string()),
        first_prompt: Some("prompt".to_string()),
        git_branch: Some("main".to_string()),
        cwd: Some("/foo".to_string()),
        tag: None,
        created_at: None,
    };
    assert_eq!(info.custom_title.as_deref(), Some("title"));
    assert!(info.tag.is_none());
    assert!(info.created_at.is_none());

    let msg = SessionMessage {
        message_type: "user".to_string(),
        uuid: "abc".to_string(),
        session_id: "sess".to_string(),
        message: json!({"role": "user", "content": "hi"}),
        parent_tool_use_id: None,
    };
    assert_eq!(msg.message_type, "user");
    assert_eq!(msg.uuid, "abc");
    assert_eq!(msg.session_id, "sess");
    assert_eq!(msg.message, json!({"role": "user", "content": "hi"}));
    assert!(msg.parent_tool_use_id.is_none());
}

#[test]
fn list_subagents_finds_agent_files_and_searches_projects() {
    let env = test_env();
    assert_eq!(list_subagents("not-a-uuid", None), Vec::<String>::new());
    assert_eq!(
        list_subagents(&Uuid::new_v4().to_string(), None),
        Vec::<String>::new()
    );

    let project_path = env.tmp.path().join("proj");
    let (sid, subagents_dir) =
        make_session_with_subagents(&env.config_dir, &project_path, &["abc123", "def456"]);
    fs::write(subagents_dir.join("agent-abc123.meta.json"), "{}").unwrap();
    fs::write(subagents_dir.join("other.jsonl"), "{}\n").unwrap();
    fs::write(subagents_dir.join("agent-noext"), "{}").unwrap();
    let nested = subagents_dir.join("workflows").join("run-1");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("agent-nested.jsonl"), "{}\n").unwrap();

    let mut result = list_subagents(&sid, Some(&project_path));
    result.sort();
    assert_eq!(result, vec!["abc123", "def456", "nested"]);

    let all_result = list_subagents(&sid, None);
    assert!(all_result.contains(&"abc123".to_string()));
}

#[test]
fn get_subagent_messages_reconstructs_nested_agent_transcripts() {
    let env = test_env();
    assert_eq!(
        get_subagent_messages("not-a-uuid", "abc", None, None, 0),
        vec![]
    );
    assert_eq!(
        get_subagent_messages(&Uuid::new_v4().to_string(), "", None, None, 0),
        vec![]
    );

    let project_path = env.tmp.path().join("proj");
    let (sid, subagents_dir) = make_session_with_subagents(&env.config_dir, &project_path, &[]);
    assert_eq!(
        get_subagent_messages(&sid, "missing", Some(&project_path), None, 0),
        vec![]
    );

    let u1 = Uuid::new_v4().to_string();
    let a1 = Uuid::new_v4().to_string();
    let u2 = Uuid::new_v4().to_string();
    let a2 = Uuid::new_v4().to_string();
    let entries = [
        transcript_entry("user", &u1, None, &sid, Some(json!("task")), json!({})),
        transcript_entry(
            "assistant",
            &a1,
            Some(&u1),
            &sid,
            Some(json!("working")),
            json!({}),
        ),
        transcript_entry(
            "user",
            &u2,
            Some(&a1),
            &sid,
            Some(json!("continue")),
            json!({}),
        ),
        transcript_entry(
            "assistant",
            &a2,
            Some(&u2),
            &sid,
            Some(json!("done")),
            json!({}),
        ),
    ];
    fs::write(
        subagents_dir.join("agent-abc.jsonl"),
        entries
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let messages = get_subagent_messages(&sid, "abc", Some(&project_path), None, 0);
    assert_eq!(
        messages.iter().map(|m| m.uuid.as_str()).collect::<Vec<_>>(),
        vec![u1.as_str(), a1.as_str(), u2.as_str(), a2.as_str()]
    );
    assert_eq!(messages[0].message_type, "user");
    assert_eq!(messages[0].session_id, sid);
    assert_eq!(
        messages[0].message,
        json!({"role": "user", "content": "task"})
    );
    assert_eq!(
        get_subagent_messages(&sid, "abc", Some(&project_path), Some(2), 2)
            .iter()
            .map(|m| m.uuid.as_str())
            .collect::<Vec<_>>(),
        vec![u2.as_str(), a2.as_str()]
    );
    assert_eq!(
        get_subagent_messages(&sid, "abc", Some(&project_path), Some(0), 0).len(),
        4
    );

    let nested = subagents_dir.join("workflows").join("run-1");
    fs::create_dir_all(&nested).unwrap();
    fs::write(
        nested.join("agent-deep.jsonl"),
        [
            transcript_entry("user", &u1, None, &sid, Some(json!("hi")), json!({})).to_string(),
            "not valid json {".to_string(),
            transcript_entry(
                "assistant",
                &a1,
                Some(&u1),
                &sid,
                Some(json!("hello")),
                json!({}),
            )
            .to_string(),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let nested_messages = get_subagent_messages(&sid, "deep", Some(&project_path), None, 0);
    assert_eq!(
        nested_messages
            .iter()
            .map(|m| m.uuid.as_str())
            .collect::<Vec<_>>(),
        vec![u1.as_str(), a1.as_str()]
    );

    fs::write(subagents_dir.join("agent-empty.jsonl"), "").unwrap();
    assert_eq!(
        get_subagent_messages(&sid, "empty", Some(&project_path), None, 0),
        vec![]
    );
}
