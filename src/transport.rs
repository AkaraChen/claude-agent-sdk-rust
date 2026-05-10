use std::collections::HashMap;
use std::env;
#[cfg(unix)]
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use opentelemetry::global;
use opentelemetry::propagation::Injector;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::VERSION;
use crate::errors::{
    ClaudeSdkError, CliConnectionError, CliJsonDecodeError, CliNotFoundError, ProcessError, Result,
};
use crate::types::{
    ClaudeAgentOptions, McpConfig, OutputFormat, SdkPluginConfig, Skills, SystemPrompt,
    ThinkingConfig, Tools,
};

const DEFAULT_MAX_BUFFER_SIZE: usize = 1024 * 1024;
const MINIMUM_CLAUDE_CODE_VERSION: &str = "2.0.0";

#[async_trait]
pub trait Transport: Send + Sync {
    async fn connect(&self) -> Result<()>;
    async fn write(&self, data: &str) -> Result<()>;
    fn read_messages(&self) -> BoxStream<'static, Result<Value>>;
    async fn close(&self) -> Result<()>;
    fn is_ready(&self) -> bool;
    async fn end_input(&self) -> Result<()>;
}

#[derive(Clone)]
pub struct SubprocessCliTransport {
    inner: Arc<SubprocessInner>,
}

struct SubprocessInner {
    options: ClaudeAgentOptions,
    cli_path: Mutex<Option<PathBuf>>,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    ready: std::sync::atomic::AtomicBool,
    exit_error: Mutex<Option<String>>,
    max_buffer_size: usize,
}

impl Drop for SubprocessInner {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            if let Some(mut child) = child.take() {
                let _ = child.start_kill();
            }
        }
    }
}

impl SubprocessCliTransport {
    pub fn new(options: ClaudeAgentOptions) -> Self {
        let cli_path = options.cli_path.clone();
        let max_buffer_size = options.max_buffer_size.unwrap_or(DEFAULT_MAX_BUFFER_SIZE);
        Self {
            inner: Arc::new(SubprocessInner {
                options,
                cli_path: Mutex::new(cli_path),
                child: Mutex::new(None),
                stdin: Mutex::new(None),
                ready: std::sync::atomic::AtomicBool::new(false),
                exit_error: Mutex::new(None),
                max_buffer_size,
            }),
        }
    }

    async fn find_cli(&self) -> Result<PathBuf> {
        if let Some(path) = self.find_bundled_cli() {
            return Ok(path);
        }
        if let Some(path) = find_on_path("claude") {
            return Ok(path);
        }

        let mut locations = Vec::new();
        if let Some(home) = dirs::home_dir() {
            locations.push(home.join(".npm-global/bin/claude"));
            locations.push(PathBuf::from("/usr/local/bin/claude"));
            locations.push(home.join(".local/bin/claude"));
            locations.push(home.join("node_modules/.bin/claude"));
            locations.push(home.join(".yarn/bin/claude"));
            locations.push(home.join(".claude/local/claude"));
        } else {
            locations.push(PathBuf::from("/usr/local/bin/claude"));
        }

        for path in locations {
            if path.is_file() {
                return Ok(path);
            }
        }

        Err(CliNotFoundError::new(
            "Claude Code not found. Install with:\n  npm install -g @anthropic-ai/claude-code\n\nIf already installed locally, try:\n  export PATH=\"$HOME/node_modules/.bin:$PATH\"\n\nOr provide the path via ClaudeAgentOptions.cli_path",
            None,
        )
        .into())
    }

    fn find_bundled_cli(&self) -> Option<PathBuf> {
        let cli_name = if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        };
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("_bundled")
            .join(cli_name);
        path.is_file().then_some(path)
    }

    fn build_settings_value(&self) -> Result<Option<String>> {
        let has_settings = self.inner.options.settings.is_some();
        let has_sandbox = self.inner.options.sandbox.is_some();
        if !has_settings && !has_sandbox {
            return Ok(None);
        }
        if has_settings && !has_sandbox {
            return Ok(self.inner.options.settings.clone());
        }

        let mut settings_obj = serde_json::Map::new();
        if let Some(settings) = &self.inner.options.settings {
            let trimmed = settings.trim();
            let parsed = if trimmed.starts_with('{') && trimmed.ends_with('}') {
                serde_json::from_str::<Value>(trimmed).ok()
            } else {
                std::fs::read_to_string(trimmed)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            };
            if let Some(Value::Object(obj)) = parsed {
                settings_obj = obj;
            }
        }
        if let Some(sandbox) = &self.inner.options.sandbox {
            settings_obj.insert("sandbox".to_string(), serde_json::to_value(sandbox)?);
        }
        Ok(Some(python_json_dumps(&Value::Object(settings_obj))))
    }

    fn apply_skills_defaults(&self) -> (Vec<String>, Option<Vec<String>>) {
        let mut allowed_tools = self.inner.options.allowed_tools.clone();
        let mut setting_sources = self.inner.options.setting_sources.clone();

        match &self.inner.options.skills {
            None => {}
            Some(Skills::All(value)) if value == "all" => {
                if !allowed_tools.iter().any(|t| t == "Skill") {
                    allowed_tools.push("Skill".to_string());
                }
                if setting_sources.is_none() {
                    setting_sources = Some(vec!["user".to_string(), "project".to_string()]);
                }
            }
            Some(Skills::List(skills)) => {
                for name in skills {
                    let pattern = format!("Skill({name})");
                    if !allowed_tools.iter().any(|t| t == &pattern) {
                        allowed_tools.push(pattern);
                    }
                }
                if setting_sources.is_none() {
                    setting_sources = Some(vec!["user".to_string(), "project".to_string()]);
                }
            }
            Some(Skills::All(_)) => {}
        }

        (allowed_tools, setting_sources)
    }

    async fn build_command(&self) -> Result<Vec<String>> {
        let cli_path = self.inner.cli_path.lock().await.clone().ok_or_else(|| {
            CliNotFoundError::new("CLI path not resolved. Call connect() first.", None)
        })?;

        let options = &self.inner.options;
        let mut cmd = vec![
            cli_path.to_string_lossy().to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ];

        match &options.system_prompt {
            None => {
                cmd.push("--system-prompt".to_string());
                cmd.push(String::new());
            }
            Some(SystemPrompt::Text(text)) => {
                cmd.push("--system-prompt".to_string());
                cmd.push(text.clone());
            }
            Some(SystemPrompt::File { path }) => {
                cmd.push("--system-prompt-file".to_string());
                cmd.push(path.clone());
            }
            Some(SystemPrompt::Preset { append, .. }) => {
                if let Some(append) = append {
                    cmd.push("--append-system-prompt".to_string());
                    cmd.push(append.clone());
                }
            }
        }

        if let Some(tools) = &options.tools {
            match tools {
                Tools::List(list) if list.is_empty() => {
                    cmd.push("--tools".to_string());
                    cmd.push(String::new());
                }
                Tools::List(list) => {
                    cmd.push("--tools".to_string());
                    cmd.push(list.join(","));
                }
                Tools::Preset { .. } => {
                    cmd.push("--tools".to_string());
                    cmd.push("default".to_string());
                }
            }
        }

        let (effective_allowed_tools, effective_setting_sources) = self.apply_skills_defaults();
        if !effective_allowed_tools.is_empty() {
            cmd.push("--allowedTools".to_string());
            cmd.push(effective_allowed_tools.join(","));
        }
        if let Some(max_turns) = options.max_turns {
            cmd.push("--max-turns".to_string());
            cmd.push(max_turns.to_string());
        }
        if let Some(max_budget_usd) = options.max_budget_usd {
            cmd.push("--max-budget-usd".to_string());
            cmd.push(max_budget_usd.to_string());
        }
        if !options.disallowed_tools.is_empty() {
            cmd.push("--disallowedTools".to_string());
            cmd.push(options.disallowed_tools.join(","));
        }
        if let Some(task_budget) = &options.task_budget {
            cmd.push("--task-budget".to_string());
            cmd.push(task_budget.total.to_string());
        }
        if let Some(model) = &options.model {
            cmd.push("--model".to_string());
            cmd.push(model.clone());
        }
        if let Some(model) = &options.fallback_model {
            cmd.push("--fallback-model".to_string());
            cmd.push(model.clone());
        }
        if !options.betas.is_empty() {
            cmd.push("--betas".to_string());
            cmd.push(options.betas.join(","));
        }
        if let Some(tool_name) = &options.permission_prompt_tool_name {
            cmd.push("--permission-prompt-tool".to_string());
            cmd.push(tool_name.clone());
        }
        if let Some(mode) = &options.permission_mode {
            cmd.push("--permission-mode".to_string());
            cmd.push(mode.to_string());
        }
        if options.continue_conversation {
            cmd.push("--continue".to_string());
        }
        if let Some(resume) = &options.resume {
            cmd.push("--resume".to_string());
            cmd.push(resume.clone());
        }
        if let Some(session_id) = &options.session_id {
            cmd.push("--session-id".to_string());
            cmd.push(session_id.clone());
        }
        if let Some(settings) = self.build_settings_value()? {
            cmd.push("--settings".to_string());
            cmd.push(settings);
        }
        for directory in &options.add_dirs {
            cmd.push("--add-dir".to_string());
            cmd.push(directory.to_string_lossy().to_string());
        }
        let mut sdk_server_configs = serde_json::Map::new();
        for (name, server) in &options.sdk_mcp_servers {
            sdk_server_configs.insert(name.clone(), server.serializable_config());
        }
        if !sdk_server_configs.is_empty() {
            let mut servers = match &options.mcp_servers {
                Some(McpConfig::Servers(servers)) => match serde_json::to_value(servers)? {
                    Value::Object(obj) => obj,
                    _ => serde_json::Map::new(),
                },
                _ => serde_json::Map::new(),
            };
            servers.extend(sdk_server_configs);
            cmd.push("--mcp-config".to_string());
            cmd.push(python_json_dumps(&json!({ "mcpServers": servers })));
        } else if let Some(mcp_servers) = &options.mcp_servers {
            match mcp_servers {
                McpConfig::Servers(servers) if !servers.is_empty() => {
                    cmd.push("--mcp-config".to_string());
                    cmd.push(python_json_dumps(&json!({ "mcpServers": servers })));
                }
                McpConfig::Path(path) if !path.is_empty() => {
                    cmd.push("--mcp-config".to_string());
                    cmd.push(path.clone());
                }
                _ => {}
            }
        }
        if options.include_partial_messages {
            cmd.push("--include-partial-messages".to_string());
        }
        if options.include_hook_events {
            cmd.push("--include-hook-events".to_string());
        }
        if options.strict_mcp_config {
            cmd.push("--strict-mcp-config".to_string());
        }
        if options.fork_session {
            cmd.push("--fork-session".to_string());
        }
        if options.session_store.is_some() {
            cmd.push("--session-mirror".to_string());
        }
        if let Some(setting_sources) = effective_setting_sources {
            cmd.push(format!("--setting-sources={}", setting_sources.join(",")));
        }
        for plugin in &options.plugins {
            match plugin {
                SdkPluginConfig::Local { path } => {
                    cmd.push("--plugin-dir".to_string());
                    cmd.push(path.clone());
                }
            }
        }
        for (flag, value) in &options.extra_args {
            cmd.push(format!("--{flag}"));
            if let Some(value) = value {
                cmd.push(value.clone());
            }
        }
        if let Some(thinking) = &options.thinking {
            match thinking {
                ThinkingConfig::Adaptive { display } => {
                    cmd.push("--thinking".to_string());
                    cmd.push("adaptive".to_string());
                    if let Some(display) = display {
                        cmd.push("--thinking-display".to_string());
                        cmd.push(display.clone());
                    }
                }
                ThinkingConfig::Enabled {
                    budget_tokens,
                    display,
                } => {
                    cmd.push("--max-thinking-tokens".to_string());
                    cmd.push(budget_tokens.to_string());
                    if let Some(display) = display {
                        cmd.push("--thinking-display".to_string());
                        cmd.push(display.clone());
                    }
                }
                ThinkingConfig::Disabled => {
                    cmd.push("--thinking".to_string());
                    cmd.push("disabled".to_string());
                }
            }
        } else if let Some(max_thinking_tokens) = options.max_thinking_tokens {
            cmd.push("--max-thinking-tokens".to_string());
            cmd.push(max_thinking_tokens.to_string());
        }
        if let Some(effort) = &options.effort {
            cmd.push("--effort".to_string());
            cmd.push(effort.clone());
        }
        if let Some(output_format) = &options.output_format {
            match output_format {
                OutputFormat::JsonSchema(schema) => {
                    cmd.push("--json-schema".to_string());
                    cmd.push(python_json_dumps(schema.schema()));
                }
            }
        }

        cmd.push("--input-format".to_string());
        cmd.push("stream-json".to_string());
        Ok(cmd)
    }

    fn build_process_env(&self) -> HashMap<String, String> {
        let mut process_env: HashMap<String, String> =
            env::vars().filter(|(k, _)| k != "CLAUDECODE").collect();
        process_env.insert("CLAUDE_CODE_ENTRYPOINT".to_string(), "sdk-py".to_string());
        process_env.extend(self.inner.options.env.clone());
        process_env.insert("CLAUDE_AGENT_SDK_VERSION".to_string(), VERSION.to_string());
        inject_otel_trace_context(&mut process_env, &self.inner.options.env);
        if self.inner.options.enable_file_checkpointing {
            process_env.insert(
                "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".to_string(),
                "true".to_string(),
            );
        }
        if let Some(cwd) = &self.inner.options.cwd {
            process_env.insert("PWD".to_string(), cwd.to_string_lossy().to_string());
        }
        process_env
    }

    async fn check_claude_version(&self) {
        let Some(cli_path) = self.inner.cli_path.lock().await.clone() else {
            return;
        };
        let Ok(mut child) = Command::new(&cli_path)
            .arg("-v")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };

        let mut output = Vec::new();
        let read_result = timeout(Duration::from_secs(2), async {
            if let Some(stdout) = child.stdout.as_mut() {
                let mut buf = vec![0; 4096];
                match stdout.read(&mut buf).await {
                    Ok(n) => output.extend_from_slice(&buf[..n]),
                    Err(_) => {}
                }
            }
        })
        .await;
        cleanup_version_child(&mut child).await;

        if read_result.is_err() {
            return;
        };
        let version_output = String::from_utf8_lossy(&output);
        if let Some(version) = extract_cli_version(&version_output) {
            if is_version_less(version, MINIMUM_CLAUDE_CODE_VERSION) {
                tracing::warn!("{}", unsupported_version_warning(version, &cli_path));
            }
        }
    }
}

#[async_trait]
impl Transport for SubprocessCliTransport {
    async fn connect(&self) -> Result<()> {
        if self.inner.child.lock().await.is_some() {
            return Ok(());
        }

        if self.inner.cli_path.lock().await.is_none() {
            let cli = self.find_cli().await?;
            *self.inner.cli_path.lock().await = Some(cli);
        }

        if env::var_os("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK").is_none() {
            self.check_claude_version().await;
        }

        let cmd = self.build_command().await?;
        let (program, args) = cmd
            .split_first()
            .ok_or_else(|| CliConnectionError::new("Failed to build Claude Code command"))?;

        let process_env = self.build_process_env();

        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .env_clear()
            .envs(process_env);
        if let Some(cwd) = &self.inner.options.cwd {
            command.current_dir(cwd);
        }
        if let Some(user) = &self.inner.options.user {
            apply_process_user(&mut command, user)?;
        }
        if self.inner.options.stderr.is_some() {
            command.stderr(std::process::Stdio::piped());
        }

        let mut child = command.spawn().map_err(|e| -> ClaudeSdkError {
            if let Some(cwd) = &self.inner.options.cwd {
                if !cwd.exists() {
                    return CliConnectionError::new(format!(
                        "Working directory does not exist: {}",
                        cwd.display()
                    ))
                    .into();
                }
            }
            if e.kind() == std::io::ErrorKind::NotFound {
                return CliNotFoundError::new(format!("Claude Code not found at: {program}"), None)
                    .into();
            }
            CliConnectionError::new(format!("Failed to start Claude Code: {e}")).into()
        })?;

        let stdin = child.stdin.take();
        if let Some(stderr) = child.stderr.take() {
            if let Some(cb) = self.inner.options.stderr.clone() {
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let trimmed = line.trim_end().to_string();
                        if !trimmed.is_empty() {
                            cb(trimmed);
                        }
                    }
                });
            }
        }

        *self.inner.stdin.lock().await = stdin;
        *self.inner.child.lock().await = Some(child);
        self.inner
            .ready
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn write(&self, data: &str) -> Result<()> {
        let mut stdin = self.inner.stdin.lock().await;
        if !self.is_ready() || stdin.is_none() {
            return Err(
                CliConnectionError::new("ProcessTransport is not ready for writing").into(),
            );
        }
        if let Some(exit_error) = self.inner.exit_error.lock().await.clone() {
            return Err(CliConnectionError::new(format!(
                "Cannot write to process that exited with error: {exit_error}"
            ))
            .into());
        }
        if let Some(child) = self.inner.child.lock().await.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "None".to_string());
                    return Err(CliConnectionError::new(format!(
                        "Cannot write to terminated process (exit code: {code})"
                    ))
                    .into());
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(CliConnectionError::new(format!(
                        "Failed to check process status before write: {e}"
                    ))
                    .into());
                }
            }
        }
        let Some(stdin) = stdin.as_mut() else {
            return Err(
                CliConnectionError::new("ProcessTransport is not ready for writing").into(),
            );
        };
        stdin.write_all(data.as_bytes()).await.map_err(|e| {
            self.inner
                .ready
                .store(false, std::sync::atomic::Ordering::SeqCst);
            CliConnectionError::new(format!("Failed to write to process stdin: {e}")).into()
        })
    }

    fn read_messages(&self) -> BoxStream<'static, Result<Value>> {
        let inner = self.inner.clone();
        try_stream! {
            let stdout = {
                let mut child_guard = inner.child.lock().await;
                let child = child_guard
                    .as_mut()
                    .ok_or_else(|| CliConnectionError::new("Not connected"))?;
                child
                    .stdout
                    .take()
                    .ok_or_else(|| CliConnectionError::new("Not connected"))?
            };

            let mut lines = BufReader::new(stdout).lines();
            let mut json_buffer = String::new();

            while let Some(line) = lines.next_line().await? {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                for raw in line.split('\n') {
                    let json_line = raw.trim();
                    if json_line.is_empty() {
                        continue;
                    }
                    if json_buffer.is_empty() && !json_line.starts_with('{') {
                        tracing::debug!("Skipping non-JSON line from CLI stdout: {}", json_line.chars().take(200).collect::<String>());
                        continue;
                    }
                    json_buffer.push_str(json_line);
                    if json_buffer.len() > inner.max_buffer_size {
                        let len = json_buffer.len();
                        json_buffer.clear();
                        Err(CliJsonDecodeError::new(
                            format!("JSON message exceeded maximum buffer size of {} bytes", inner.max_buffer_size),
                            format!("Buffer size {len} exceeds limit {}", inner.max_buffer_size),
                        ))?;
                    }
                    match serde_json::from_str::<Value>(&json_buffer) {
                        Ok(data) => {
                            json_buffer.clear();
                            yield data;
                        }
                        Err(_) => continue,
                    }
                }
            }

            let status = {
                let mut child = inner.child.lock().await;
                if let Some(child) = child.as_mut() {
                    child.wait().await.ok()
                } else {
                    None
                }
            };
            if let Some(status) = status {
                if !status.success() {
                    let code = status.code();
                    let err = ProcessError::new(
                        format!("Command failed with exit code {}", code.unwrap_or(-1)),
                        code,
                        Some("Check stderr output for details".to_string()),
                    );
                    *inner.exit_error.lock().await = Some(err.to_string());
                    Err::<(), ClaudeSdkError>(err.into())?;
                }
            }
        }
        .boxed()
    }

    async fn close(&self) -> Result<()> {
        self.inner
            .ready
            .store(false, std::sync::atomic::Ordering::SeqCst);
        {
            let mut stdin = self.inner.stdin.lock().await;
            if let Some(mut stdin) = stdin.take() {
                let _ = stdin.shutdown().await;
            }
        }

        let mut child_opt = self.inner.child.lock().await;
        if let Some(mut child) = child_opt.take() {
            match timeout(Duration::from_secs(5), child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    terminate_child(&mut child);
                    if timeout(Duration::from_secs(5), child.wait()).await.is_err() {
                        let _ = child.kill().await;
                        let _ = timeout(Duration::from_secs(5), child.wait()).await;
                    }
                }
            }
        }
        *self.inner.exit_error.lock().await = None;
        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.inner.ready.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn end_input(&self) -> Result<()> {
        let mut stdin = self.inner.stdin.lock().await;
        if let Some(mut stdin) = stdin.take() {
            let _ = stdin.shutdown().await;
        }
        Ok(())
    }
}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // Match Python's Process.terminate() phase before escalating to SIGKILL.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            return;
        }
    }
    #[cfg(windows)]
    {
        let _ = child.start_kill();
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = child.start_kill();
    }
}

async fn cleanup_version_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    terminate_child(child);
    if timeout(Duration::from_secs(1), child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = timeout(Duration::from_secs(1), child.wait()).await;
    }
}

fn python_json_dumps(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => python_json_string(value),
        Value::Array(values) => {
            let items = values.iter().map(python_json_dumps).collect::<Vec<_>>();
            format!("[{}]", items.join(", "))
        }
        Value::Object(values) => {
            let items = values
                .iter()
                .map(|(key, value)| {
                    format!("{}: {}", python_json_string(key), python_json_dumps(value))
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", items.join(", "))
        }
    }
}

fn python_json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch if (ch as u32) <= 0x7f => out.push(ch),
            ch if (ch as u32) <= 0xffff => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => {
                let code = ch as u32 - 0x1_0000;
                let high = 0xd800 + ((code >> 10) & 0x3ff);
                let low = 0xdc00 + (code & 0x3ff);
                out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
        }
    }
    out.push('"');
    out
}

#[cfg(unix)]
fn apply_process_user(command: &mut Command, user: &str) -> Result<()> {
    let uid = match user.parse::<u32>() {
        Ok(uid) => uid,
        Err(_) => {
            let c_user = CString::new(user)
                .map_err(|_| CliConnectionError::new("Process user must not contain NUL bytes"))?;
            // getpwnam returns a pointer to static storage owned by libc.
            let passwd = unsafe { libc::getpwnam(c_user.as_ptr()) };
            if passwd.is_null() {
                return Err(
                    CliConnectionError::new(format!("Process user not found: {user}")).into(),
                );
            }
            unsafe { (*passwd).pw_uid }
        }
    };
    command.uid(uid);
    Ok(())
}

#[cfg(not(unix))]
fn apply_process_user(_command: &mut Command, user: &str) -> Result<()> {
    tracing::warn!("Ignoring process user option on this platform: {user}");
    Ok(())
}

struct HashMapInjector<'a>(&'a mut HashMap<String, String>);

impl Injector for HashMapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_string(), value);
    }
}

fn inject_otel_trace_context(
    process_env: &mut HashMap<String, String>,
    explicit_env: &HashMap<String, String>,
) {
    let otel_context = tracing::Span::current().context();
    let mut carrier = HashMap::new();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&otel_context, &mut HashMapInjector(&mut carrier));
    });

    if !carrier.contains_key("traceparent") {
        return;
    }

    for key in ["TRACEPARENT", "TRACESTATE"] {
        if !explicit_env.contains_key(key) {
            process_env.remove(key);
        }
    }
    for (key, value) in carrier {
        let env_key = key.to_ascii_uppercase();
        if !explicit_env.contains_key(&env_key) {
            process_env.insert(env_key, value);
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if cfg!(windows) {
            let candidate = dir.join(format!("{name}.exe"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_version_less(version: &str, minimum: &str) -> bool {
    let parse = |s: &str| -> Vec<i64> {
        s.split(|c: char| !c.is_ascii_digit() && c != '.')
            .next()
            .unwrap_or(s)
            .split('.')
            .filter_map(|p| p.parse::<i64>().ok())
            .collect()
    };
    let mut a = parse(version);
    let mut b = parse(minimum);
    let max_len = a.len().max(b.len());
    a.resize(max_len, 0);
    b.resize(max_len, 0);
    a < b
}

fn extract_cli_version(output: &str) -> Option<&str> {
    let trimmed = output.trim_start();
    let end = trimmed
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_ascii_digit() && ch != '.').then_some(idx))
        .unwrap_or(trimmed.len());
    let candidate = &trimmed[..end];
    let mut parts = candidate.split('.');
    let (Some(major), Some(minor), Some(patch), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    [major, minor, patch]
        .iter()
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
        .then_some(candidate)
}

fn unsupported_version_warning(version: &str, cli_path: &Path) -> String {
    format!(
        "Claude Code version {} at {} is unsupported in the Agent SDK. Minimum required version is {}. Some features may not work correctly.",
        version,
        cli_path.display(),
        MINIMUM_CLAUDE_CODE_VERSION
    )
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;
    use std::sync::{Arc, MutexGuard};

    use futures::StreamExt;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::process::Command;

    use super::*;
    use crate::internal::session_store::InMemorySessionStore;
    use crate::mcp::create_sdk_mcp_server;
    use crate::types::{
        McpConfig, McpServerConfig, McpServers, OutputFormat, PermissionMode,
        SandboxIgnoreViolations, SandboxNetworkConfig, SandboxSettings, SessionStoreFlushMode,
        TaskBudget,
    };

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        old: Vec<(String, Option<String>)>,
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.old.drain(..) {
                match value {
                    Some(value) => unsafe { env::set_var(key, value) },
                    None => unsafe { env::remove_var(key) },
                }
            }
        }
    }

    fn set_env(vars: &[(&str, Option<&str>)]) -> EnvGuard {
        let lock = ENV_LOCK.lock().unwrap();
        let old = vars
            .iter()
            .map(|(key, _)| (key.to_string(), env::var(key).ok()))
            .collect::<Vec<_>>();
        for (key, value) in vars {
            match value {
                Some(value) => unsafe { env::set_var(key, value) },
                None => unsafe { env::remove_var(key) },
            }
        }
        EnvGuard { old, _lock: lock }
    }

    fn options() -> ClaudeAgentOptions {
        ClaudeAgentOptions {
            cli_path: Some(PathBuf::from("/usr/bin/claude")),
            ..Default::default()
        }
    }

    async fn command_for(options: ClaudeAgentOptions) -> Vec<String> {
        SubprocessCliTransport::new(options)
            .build_command()
            .await
            .unwrap()
    }

    fn value_after<'a>(cmd: &'a [String], flag: &str) -> &'a str {
        let idx = cmd.iter().position(|item| item == flag).unwrap();
        &cmd[idx + 1]
    }

    #[tokio::test]
    async fn init_defers_cli_discovery_and_preserves_provided_cli_path() {
        let transport = SubprocessCliTransport::new(ClaudeAgentOptions::default());
        assert!(transport.inner.cli_path.lock().await.is_none());

        let transport = SubprocessCliTransport::new(options());
        assert_eq!(
            transport.inner.cli_path.lock().await.as_deref(),
            Some(Path::new("/usr/bin/claude"))
        );
    }

    #[tokio::test]
    async fn build_command_basic_and_system_prompt_variants_match_python() {
        let cmd = command_for(options()).await;
        assert_eq!(cmd[0], "/usr/bin/claude");
        assert_eq!(value_after(&cmd, "--output-format"), "stream-json");
        assert_eq!(value_after(&cmd, "--input-format"), "stream-json");
        assert!(cmd.contains(&"--verbose".to_string()));
        assert!(!cmd.contains(&"--print".to_string()));
        assert_eq!(value_after(&cmd, "--system-prompt"), "");

        let mut opts = options();
        opts.system_prompt = Some(SystemPrompt::Text("Be helpful".to_string()));
        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--system-prompt"), "Be helpful");

        let mut opts = options();
        opts.system_prompt = Some(SystemPrompt::Preset {
            preset: "claude_code".to_string(),
            append: None,
            exclude_dynamic_sections: None,
        });
        let cmd = command_for(opts).await;
        assert!(!cmd.contains(&"--system-prompt".to_string()));
        assert!(!cmd.contains(&"--append-system-prompt".to_string()));

        let mut opts = options();
        opts.system_prompt = Some(SystemPrompt::Preset {
            preset: "claude_code".to_string(),
            append: Some("Be concise.".to_string()),
            exclude_dynamic_sections: None,
        });
        let cmd = command_for(opts).await;
        assert!(!cmd.contains(&"--system-prompt".to_string()));
        assert_eq!(value_after(&cmd, "--append-system-prompt"), "Be concise.");

        let mut opts = options();
        opts.system_prompt = Some(SystemPrompt::File {
            path: "/path/to/prompt.md".to_string(),
        });
        let cmd = command_for(opts).await;
        assert!(!cmd.contains(&"--system-prompt".to_string()));
        assert_eq!(
            value_after(&cmd, "--system-prompt-file"),
            "/path/to/prompt.md"
        );
    }

    #[tokio::test]
    async fn build_command_forwards_core_options_tools_and_sessions() {
        let mut opts = options();
        opts.allowed_tools = vec!["Read".to_string(), "Write".to_string()];
        opts.disallowed_tools = vec!["Bash".to_string()];
        opts.model = Some("claude-sonnet-4-5".to_string());
        opts.fallback_model = Some("sonnet".to_string());
        opts.betas = vec!["beta-a".to_string(), "beta-b".to_string()];
        opts.permission_mode = Some(PermissionMode::AcceptEdits);
        opts.max_turns = Some(5);
        opts.max_budget_usd = Some(1.25);
        opts.task_budget = Some(TaskBudget { total: 100_000 });
        opts.continue_conversation = true;
        opts.resume = Some("session-123".to_string());
        opts.session_id = Some("550e8400-e29b-41d4-a716-446655440000".to_string());
        opts.permission_prompt_tool_name = Some("stdio".to_string());
        opts.include_partial_messages = true;
        opts.include_hook_events = true;
        opts.strict_mcp_config = true;
        opts.fork_session = true;
        opts.enable_file_checkpointing = true;
        opts.session_store_flush = SessionStoreFlushMode::Eager;
        opts.tools = Some(Tools::List(vec![
            "Read".to_string(),
            "Edit".to_string(),
            "Bash".to_string(),
        ]));
        opts.add_dirs = vec![
            PathBuf::from("/path/to/dir1"),
            PathBuf::from("/path/to/dir2"),
        ];

        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--allowedTools"), "Read,Write");
        assert_eq!(value_after(&cmd, "--disallowedTools"), "Bash");
        assert_eq!(value_after(&cmd, "--model"), "claude-sonnet-4-5");
        assert_eq!(value_after(&cmd, "--fallback-model"), "sonnet");
        assert_eq!(value_after(&cmd, "--betas"), "beta-a,beta-b");
        assert_eq!(value_after(&cmd, "--permission-mode"), "acceptEdits");
        assert_eq!(value_after(&cmd, "--max-turns"), "5");
        assert_eq!(value_after(&cmd, "--max-budget-usd"), "1.25");
        assert_eq!(value_after(&cmd, "--task-budget"), "100000");
        assert_eq!(value_after(&cmd, "--resume"), "session-123");
        assert_eq!(
            value_after(&cmd, "--session-id"),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(value_after(&cmd, "--permission-prompt-tool"), "stdio");
        assert_eq!(value_after(&cmd, "--tools"), "Read,Edit,Bash");
        assert!(cmd.contains(&"--continue".to_string()));
        assert!(cmd.contains(&"--include-partial-messages".to_string()));
        assert!(cmd.contains(&"--include-hook-events".to_string()));
        assert!(cmd.contains(&"--strict-mcp-config".to_string()));
        assert!(cmd.contains(&"--fork-session".to_string()));
        let add_dir_values = cmd
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| (item == "--add-dir").then(|| cmd[idx + 1].as_str()))
            .collect::<Vec<_>>();
        assert_eq!(add_dir_values, vec!["/path/to/dir1", "/path/to/dir2"]);

        let mut opts = options();
        opts.tools = Some(Tools::List(Vec::new()));
        assert_eq!(value_after(&command_for(opts).await, "--tools"), "");

        let mut opts = options();
        opts.tools = Some(Tools::Preset {
            preset: "claude_code".to_string(),
        });
        assert_eq!(value_after(&command_for(opts).await, "--tools"), "default");

        let cmd = command_for(options()).await;
        assert!(!cmd.contains(&"--session-mirror".to_string()));
        let mut opts = options();
        opts.session_store = Some(Arc::new(InMemorySessionStore::new()));
        assert!(
            command_for(opts)
                .await
                .contains(&"--session-mirror".to_string())
        );
    }

    #[tokio::test]
    async fn build_command_thinking_skills_settings_sandbox_and_extra_args() {
        let mut opts = options();
        opts.max_thinking_tokens = Some(9999);
        opts.thinking = Some(ThinkingConfig::Adaptive {
            display: Some("summarized".to_string()),
        });
        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--thinking"), "adaptive");
        assert_eq!(value_after(&cmd, "--thinking-display"), "summarized");
        assert!(!cmd.contains(&"--max-thinking-tokens".to_string()));

        let mut opts = options();
        opts.thinking = Some(ThinkingConfig::Enabled {
            budget_tokens: 20_000,
            display: Some("omitted".to_string()),
        });
        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--max-thinking-tokens"), "20000");
        assert_eq!(value_after(&cmd, "--thinking-display"), "omitted");

        let mut opts = options();
        opts.thinking = Some(ThinkingConfig::Disabled);
        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--thinking"), "disabled");
        assert!(!cmd.contains(&"--max-thinking-tokens".to_string()));

        let mut opts = options();
        opts.skills = Some(Skills::All("all".to_string()));
        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--allowedTools"), "Skill");
        assert!(cmd.contains(&"--setting-sources=user,project".to_string()));

        let mut opts = options();
        opts.allowed_tools = vec!["Read".to_string(), "Bash".to_string()];
        opts.skills = Some(Skills::List(vec!["pdf".to_string(), "docx".to_string()]));
        let cmd = command_for(opts).await;
        assert_eq!(
            value_after(&cmd, "--allowedTools"),
            "Read,Bash,Skill(pdf),Skill(docx)"
        );
        assert!(cmd.contains(&"--setting-sources=user,project".to_string()));

        let mut opts = options();
        opts.skills = Some(Skills::List(Vec::new()));
        let cmd = command_for(opts).await;
        assert!(!cmd.contains(&"--allowedTools".to_string()));
        assert!(cmd.contains(&"--setting-sources=user,project".to_string()));

        let mut opts = options();
        opts.setting_sources = Some(Vec::new());
        assert!(
            command_for(opts)
                .await
                .contains(&"--setting-sources=".to_string())
        );

        let mut opts = options();
        opts.settings =
            Some(r#"{"permissions":{"allow":["Bash(ls:*)"]},"verbose":true}"#.to_string());
        opts.sandbox = Some(SandboxSettings {
            enabled: Some(true),
            auto_allow_bash_if_sandboxed: None,
            excluded_commands: vec!["git".to_string(), "docker".to_string()],
            allow_unsandboxed_commands: None,
            network: Some(SandboxNetworkConfig {
                allow_local_binding: Some(true),
                allow_unix_sockets: vec!["/tmp/ssh-agent.sock".to_string()],
                http_proxy_port: Some(8080),
                ..Default::default()
            }),
            ignore_violations: Some(SandboxIgnoreViolations {
                file: vec!["/tmp/noisy".to_string()],
                network: Vec::new(),
            }),
            enable_weaker_nested_sandbox: None,
        });
        let cmd = command_for(opts).await;
        let settings = serde_json::from_str::<Value>(value_after(&cmd, "--settings")).unwrap();
        assert_eq!(settings["permissions"], json!({"allow": ["Bash(ls:*)"]}));
        assert_eq!(settings["verbose"], true);
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(
            settings["sandbox"]["excludedCommands"],
            json!(["git", "docker"])
        );
        assert_eq!(
            settings["sandbox"]["network"]["allowUnixSockets"],
            json!(["/tmp/ssh-agent.sock"])
        );
        assert_eq!(settings["sandbox"]["network"]["allowLocalBinding"], true);
        assert_eq!(settings["sandbox"]["network"]["httpProxyPort"], 8080);
        assert_eq!(
            settings["sandbox"]["ignoreViolations"]["file"],
            json!(["/tmp/noisy"])
        );

        let mut opts = options();
        opts.extra_args
            .insert("new-flag".to_string(), Some("value".to_string()));
        opts.extra_args.insert("boolean-flag".to_string(), None);
        opts.extra_args
            .insert("another-option".to_string(), Some("test-value".to_string()));
        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--new-flag"), "value");
        assert_eq!(value_after(&cmd, "--another-option"), "test-value");
        assert!(cmd.contains(&"--boolean-flag".to_string()));
        let extra_flags = cmd
            .iter()
            .filter(|arg| {
                matches!(
                    arg.as_str(),
                    "--new-flag" | "--boolean-flag" | "--another-option"
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            extra_flags,
            vec!["--new-flag", "--boolean-flag", "--another-option"]
        );
    }

    #[tokio::test]
    async fn build_command_settings_and_skills_edge_cases_match_python() {
        let mut opts = options();
        opts.settings = Some("/path/to/settings.json".to_string());
        assert_eq!(
            value_after(&command_for(opts).await, "--settings"),
            "/path/to/settings.json"
        );

        let settings_json = r#"{"permissions":{"deny":["Bash(rm:*)"]}}"#.to_string();
        let mut opts = options();
        opts.settings = Some(settings_json.clone());
        assert_eq!(
            value_after(&command_for(opts).await, "--settings"),
            settings_json
        );

        let mut opts = options();
        opts.allowed_tools = vec!["Skill".to_string()];
        opts.setting_sources = Some(vec!["local".to_string()]);
        opts.skills = Some(Skills::All("all".to_string()));
        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--allowedTools"), "Skill");
        assert!(cmd.contains(&"--setting-sources=local".to_string()));

        let mut opts = options();
        opts.allowed_tools = vec!["Skill(pdf)".to_string()];
        opts.skills = Some(Skills::List(vec!["pdf".to_string(), "docx".to_string()]));
        let original = opts.clone();
        let cmd = command_for(opts.clone()).await;
        assert_eq!(
            value_after(&cmd, "--allowedTools"),
            "Skill(pdf),Skill(docx)"
        );
        assert_eq!(opts.allowed_tools, original.allowed_tools);
        assert_eq!(opts.setting_sources, original.setting_sources);
    }

    #[tokio::test]
    async fn build_command_mcp_plugins_output_format_and_agents_stay_off_cli() {
        let mut opts = options();
        opts.mcp_servers = Some(McpConfig::servers(McpServers::new().with_server(
            "test-server",
            McpServerConfig::Stdio {
                command: "/path/to/server".to_string(),
                args: vec!["--option".to_string(), "value".to_string()],
                env: Default::default(),
            },
        )));
        let cmd = command_for(opts).await;
        assert_eq!(
            value_after(&cmd, "--mcp-config"),
            "{\"mcpServers\": {\"test-server\": {\"type\": \"stdio\", \"command\": \"/path/to/server\", \"args\": [\"--option\", \"value\"]}}}"
        );
        let mcp_config = serde_json::from_str::<Value>(value_after(&cmd, "--mcp-config")).unwrap();
        assert_eq!(
            mcp_config["mcpServers"]["test-server"]["command"],
            "/path/to/server"
        );

        let mut opts = options();
        opts.mcp_servers = Some(McpConfig::path("/path/to/mcp-config.json"));
        assert_eq!(
            value_after(&command_for(opts).await, "--mcp-config"),
            "/path/to/mcp-config.json"
        );

        let mut opts = options();
        let mcp_json = "/path/to/generated-mcp-config.json";
        opts.mcp_servers = Some(McpConfig::path(mcp_json));
        assert_eq!(
            value_after(&command_for(opts).await, "--mcp-config"),
            mcp_json
        );

        let mut opts = options();
        opts.add_sdk_mcp_server("calc", create_sdk_mcp_server("calculator", "1.0.0", vec![]));
        let cmd = command_for(opts).await;
        let mcp_config = serde_json::from_str::<Value>(value_after(&cmd, "--mcp-config")).unwrap();
        assert_eq!(mcp_config["mcpServers"]["calc"]["type"], "sdk");
        assert_eq!(mcp_config["mcpServers"]["calc"]["name"], "calculator");

        let mut opts = options();
        opts.plugins = vec![SdkPluginConfig::Local {
            path: "/plugins/a".to_string(),
        }];
        opts.output_format = Some(
            OutputFormat::json_schema(json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}},
            }))
            .unwrap(),
        );
        opts.agents = Some(HashMap::from([(
            "test-agent".to_string(),
            crate::types::AgentDefinition {
                description: "A test agent".to_string(),
                prompt: "You are a test agent".to_string(),
                tools: None,
                disallowed_tools: None,
                model: None,
                skills: None,
                memory: None,
                mcp_servers: None,
                initial_prompt: None,
                max_turns: None,
                background: None,
                effort: None,
                permission_mode: None,
            },
        )]));
        let cmd = command_for(opts).await;
        assert_eq!(value_after(&cmd, "--plugin-dir"), "/plugins/a");
        assert_eq!(
            value_after(&cmd, "--json-schema"),
            "{\"type\": \"object\", \"properties\": {\"ok\": {\"type\": \"boolean\"}}}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(value_after(&cmd, "--json-schema")).unwrap(),
            json!({"type": "object", "properties": {"ok": {"type": "boolean"}}})
        );
        assert!(!cmd.contains(&"--agents".to_string()));
        assert_eq!(value_after(&cmd, "--input-format"), "stream-json");
    }

    #[test]
    fn python_json_dumps_matches_default_python_request_parameter_format() {
        let value = json!({
            "mcpServers": {
                "测试": {
                    "type": "stdio",
                    "args": ["😀", true, null]
                }
            }
        });
        assert_eq!(
            python_json_dumps(&value),
            "{\"mcpServers\": {\"\\u6d4b\\u8bd5\": {\"type\": \"stdio\", \"args\": [\"\\ud83d\\ude00\", true, null]}}}"
        );
    }

    #[test]
    fn version_compare_matches_python_warning_boundary() {
        assert!(is_version_less("1.0.0", "2.0.0"));
        assert!(is_version_less("1.9.9 (Claude Code)", "2.0.0"));
        assert!(!is_version_less("2.0.0", "2.0.0"));
        assert!(!is_version_less("99.99.99 (Claude Code)", "2.0.0"));
        assert!(!is_version_less("2.1", "2.0.0"));
    }

    #[test]
    fn version_warning_parsing_and_message_match_python_diagnostics() {
        assert_eq!(extract_cli_version("1.0.0 (Claude Code)"), Some("1.0.0"));
        assert_eq!(
            extract_cli_version("  1.2.3 (Claude Code)\n"),
            Some("1.2.3")
        );
        assert_eq!(extract_cli_version("Claude Code 1.0.0"), None);
        assert_eq!(extract_cli_version("2.1"), None);

        let warning = unsupported_version_warning("1.0.0", Path::new("/usr/bin/claude"));
        assert!(warning.contains("1.0.0"));
        assert!(warning.contains("/usr/bin/claude"));
        assert!(warning.contains(MINIMUM_CLAUDE_CODE_VERSION));
        assert!(warning.contains("Some features may not work correctly."));
    }

    #[tokio::test]
    async fn connect_with_nonexistent_cwd_returns_connection_error() {
        let _guard = set_env(&[("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", Some("1"))]);
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let mut opts = options();
        opts.cli_path = Some(std::env::current_exe().unwrap());
        opts.cwd = Some(missing.clone());

        let err = SubprocessCliTransport::new(opts)
            .connect()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Working directory does not exist"));
        assert!(err.to_string().contains(&missing.display().to_string()));
    }

    #[tokio::test]
    async fn connect_with_missing_cli_path_returns_cli_not_found() {
        let _guard = set_env(&[("CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK", Some("1"))]);
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing-claude");
        let mut opts = options();
        opts.cli_path = Some(missing.clone());

        let err = SubprocessCliTransport::new(opts)
            .connect()
            .await
            .unwrap_err();
        assert!(matches!(err, ClaudeSdkError::CliNotFound(_)));
        assert!(err.to_string().contains(&missing.display().to_string()));
    }

    #[test]
    fn process_env_passes_large_mcp_threshold_and_uses_options_precedence() {
        let _guard = set_env(&[
            ("MAX_MCP_OUTPUT_TOKENS", Some("1000")),
            ("CLAUDE_AGENT_SDK_VERSION", Some("0.0.0")),
        ]);
        let mut opts = options();
        opts.env
            .insert("MAX_MCP_OUTPUT_TOKENS".to_string(), "500000".to_string());
        opts.env
            .insert("CLAUDE_AGENT_SDK_VERSION".to_string(), "0.0.0".to_string());
        opts.env.insert(
            "CLAUDE_CODE_ENTRYPOINT".to_string(),
            "custom-app".to_string(),
        );

        let process_env = SubprocessCliTransport::new(opts).build_process_env();
        assert_eq!(
            process_env.get("MAX_MCP_OUTPUT_TOKENS").map(String::as_str),
            Some("500000")
        );
        assert_eq!(
            process_env
                .get("CLAUDE_AGENT_SDK_VERSION")
                .map(String::as_str),
            Some(VERSION)
        );
        assert_eq!(
            process_env
                .get("CLAUDE_CODE_ENTRYPOINT")
                .map(String::as_str),
            Some("custom-app")
        );
    }

    #[test]
    fn process_env_inherits_os_env_strips_claudecode_and_does_not_inject_threshold() {
        let _guard = set_env(&[
            ("MAX_MCP_OUTPUT_TOKENS", None),
            ("CLAUDECODE", Some("1")),
            ("OTHER_VAR", Some("kept")),
        ]);
        let process_env = SubprocessCliTransport::new(options()).build_process_env();
        assert!(!process_env.contains_key("MAX_MCP_OUTPUT_TOKENS"));
        assert!(!process_env.contains_key("CLAUDECODE"));
        assert_eq!(
            process_env.get("OTHER_VAR").map(String::as_str),
            Some("kept")
        );
        assert_eq!(
            process_env
                .get("CLAUDE_CODE_ENTRYPOINT")
                .map(String::as_str),
            Some("sdk-py")
        );

        drop(process_env);
        unsafe { env::set_var("MAX_MCP_OUTPUT_TOKENS", "200000") };
        let process_env = SubprocessCliTransport::new(options()).build_process_env();
        assert_eq!(
            process_env.get("MAX_MCP_OUTPUT_TOKENS").map(String::as_str),
            Some("200000")
        );
    }

    #[test]
    fn process_env_respects_explicit_entrypoint_claudecode_and_trace_context() {
        let _guard = set_env(&[
            ("CLAUDECODE", Some("parent")),
            (
                "TRACEPARENT",
                Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01"),
            ),
            ("TRACESTATE", Some("vendor=abc")),
        ]);
        let mut opts = options();
        opts.env.insert(
            "CLAUDE_CODE_ENTRYPOINT".to_string(),
            "custom-caller".to_string(),
        );
        opts.env.insert("CLAUDECODE".to_string(), "1".to_string());

        let process_env = SubprocessCliTransport::new(opts).build_process_env();
        assert_eq!(
            process_env
                .get("CLAUDE_CODE_ENTRYPOINT")
                .map(String::as_str),
            Some("custom-caller")
        );
        assert_eq!(process_env.get("CLAUDECODE").map(String::as_str), Some("1"));
        assert_eq!(
            process_env.get("TRACEPARENT").map(String::as_str),
            Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01")
        );
        assert_eq!(
            process_env.get("TRACESTATE").map(String::as_str),
            Some("vendor=abc")
        );
    }

    #[test]
    fn process_env_adds_checkpointing_and_pwd_overrides_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = options();
        opts.enable_file_checkpointing = true;
        opts.cwd = Some(tmp.path().to_path_buf());
        let process_env = SubprocessCliTransport::new(opts).build_process_env();
        assert_eq!(
            process_env
                .get("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            process_env.get("PWD").map(String::as_str),
            Some(tmp.path().to_string_lossy().as_ref())
        );
    }

    #[cfg(unix)]
    async fn transport_with_stdout(
        output: &str,
        max_buffer_size: Option<usize>,
    ) -> (SubprocessCliTransport, TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let output_path = tmp.path().join("stdout.txt");
        std::fs::write(&output_path, output).unwrap();
        let child = Command::new("cat")
            .arg(&output_path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut opts = options();
        opts.max_buffer_size = max_buffer_size;
        let transport = SubprocessCliTransport::new(opts);
        *transport.inner.child.lock().await = Some(child);
        (transport, tmp)
    }

    #[cfg(unix)]
    async fn collect_messages(transport: &SubprocessCliTransport) -> Result<Vec<Value>> {
        let mut stream = transport.read_messages();
        let mut messages = Vec::new();
        while let Some(item) = stream.next().await {
            messages.push(item?);
        }
        Ok(messages)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_messages_handles_buffered_json_and_debug_lines() {
        let msg1 = json!({"type": "system", "subtype": "start"}).to_string();
        let large = json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "y".repeat(5000)}]},
        })
        .to_string();
        let msg3 = json!({"type": "system", "subtype": "end"}).to_string();
        let output = format!(
            "[SandboxDebug] Seccomp filtering not available\n{msg1}\nWARNING: skip me\n{large}\n{msg3}\n"
        );
        let (transport, _tmp) = transport_with_stdout(&output, None).await;
        let messages = collect_messages(&transport).await.unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["type"], "system");
        assert_eq!(messages[1]["type"], "assistant");
        assert_eq!(
            messages[1]["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .len(),
            5000
        );
        assert_eq!(messages[2]["subtype"], "end");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_messages_handles_blank_lines_and_embedded_newlines() {
        let msg1 = json!({
            "type": "message",
            "id": "msg1",
            "content": "Line 1\nLine 2\nLine 3",
        })
        .to_string();
        let msg2 = json!({
            "type": "result",
            "id": "res1",
            "data": "Some\nMultiline\nContent",
        })
        .to_string();
        let output = format!("{msg1}\n\n\n{msg2}\n");
        let (transport, _tmp) = transport_with_stdout(&output, None).await;
        let messages = collect_messages(&transport).await.unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["id"], "msg1");
        assert_eq!(messages[0]["content"], "Line 1\nLine 2\nLine 3");
        assert_eq!(messages[1]["id"], "res1");
        assert_eq!(messages[1]["data"], "Some\nMultiline\nContent");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_messages_parses_large_minified_json_at_eof() {
        let large_data = json!({"data": (0..1000).map(|i| json!({"id": i, "value": "x".repeat(100)})).collect::<Vec<_>>()});
        let payload = json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "tool_use_id": "toolu_016fed1NhiaMLqnEvrj5NUaj",
                    "type": "tool_result",
                    "content": large_data.to_string(),
                }],
            },
        })
        .to_string();
        let (transport, _tmp) = transport_with_stdout(&payload, None).await;
        let messages = collect_messages(&transport).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["type"], "user");
        assert_eq!(
            messages[0]["message"]["content"][0]["tool_use_id"],
            "toolu_016fed1NhiaMLqnEvrj5NUaj"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_messages_errors_when_buffer_size_exceeded() {
        let output = format!("{{\"data\":\"{}", "x".repeat(600));
        let (transport, _tmp) = transport_with_stdout(&output, Some(512)).await;
        let mut stream = transport.read_messages();
        let err = stream.next().await.unwrap().unwrap_err();
        assert!(matches!(err, ClaudeSdkError::CliJsonDecode(_)));
        assert!(
            err.to_string()
                .contains("JSON message exceeded maximum buffer size of 512 bytes")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_to_exited_process_reports_terminated_status() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let transport = SubprocessCliTransport::new(options());
        *transport.inner.stdin.lock().await = stdin;
        *transport.inner.child.lock().await = Some(child);
        transport
            .inner
            .ready
            .store(true, std::sync::atomic::Ordering::SeqCst);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let err = transport.write("{}\n").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("Cannot write to terminated process")
        );
        assert!(err.to_string().contains("exit code: 7"));
    }
}
