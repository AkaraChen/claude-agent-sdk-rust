use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::SdkMcpServer;

pub type JsonMap = serde_json::Map<String, Value>;
pub type BoxFutureResult<T> = Pin<Box<dyn Future<Output = T> + Send>>;

// ---------------------------------------------------------------------------
// Core option and callback types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[serde(rename = "default")]
    Default,
    AcceptEdits,
    Plan,
    BypassPermissions,
    DontAsk,
    Auto,
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::DontAsk => "dontAsk",
            PermissionMode::Auto => "auto",
        };
        f.write_str(s)
    }
}

pub type SdkBeta = String;
pub type SettingSource = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SystemPrompt {
    #[serde(rename = "preset")]
    Preset {
        preset: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        append: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exclude_dynamic_sections: Option<bool>,
    },
    #[serde(rename = "file")]
    File { path: String },
    #[serde(untagged)]
    Text(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Tools {
    #[serde(rename = "preset")]
    Preset { preset: String },
    #[serde(untagged)]
    List(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskBudget {
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub description: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(rename = "disallowedTools", skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(rename = "mcpServers", skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<Value>>,
    #[serde(rename = "initialPrompt", skip_serializing_if = "Option::is_none")]
    pub initial_prompt: Option<String>,
    #[serde(rename = "maxTurns", skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Value>,
    #[serde(rename = "permissionMode", skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    UserSettings,
    ProjectSettings,
    LocalSettings,
    Session,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionRuleValue {
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionUpdate {
    #[serde(rename = "type")]
    pub update_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PermissionRuleValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behavior: Option<PermissionBehavior>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<PermissionUpdateDestination>,
}

impl PermissionUpdate {
    pub fn to_value(&self) -> Value {
        let mut result = JsonMap::new();
        result.insert("type".to_string(), Value::String(self.update_type.clone()));
        if let Some(destination) = &self.destination {
            result.insert(
                "destination".to_string(),
                serde_json::to_value(destination).unwrap_or(Value::Null),
            );
        }
        match self.update_type.as_str() {
            "addRules" | "replaceRules" | "removeRules" => {
                if let Some(rules) = &self.rules {
                    result.insert(
                        "rules".to_string(),
                        Value::Array(
                            rules
                                .iter()
                                .map(|rule| {
                                    let mut obj = JsonMap::new();
                                    obj.insert(
                                        "toolName".to_string(),
                                        Value::String(rule.tool_name.clone()),
                                    );
                                    obj.insert(
                                        "ruleContent".to_string(),
                                        rule.rule_content
                                            .clone()
                                            .map(Value::String)
                                            .unwrap_or(Value::Null),
                                    );
                                    Value::Object(obj)
                                })
                                .collect(),
                        ),
                    );
                }
                if let Some(behavior) = &self.behavior {
                    result.insert(
                        "behavior".to_string(),
                        serde_json::to_value(behavior).unwrap_or(Value::Null),
                    );
                }
            }
            "setMode" => {
                if let Some(mode) = &self.mode {
                    result.insert("mode".to_string(), Value::String(mode.to_string()));
                }
            }
            "addDirectories" | "removeDirectories" => {
                if let Some(directories) = &self.directories {
                    result.insert(
                        "directories".to_string(),
                        Value::Array(directories.iter().cloned().map(Value::String).collect()),
                    );
                }
            }
            _ => {}
        }
        Value::Object(result)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolPermissionContext {
    pub signal: Option<Value>,
    pub suggestions: Vec<Value>,
    pub tool_use_id: Option<String>,
    pub agent_id: Option<String>,
    pub blocked_path: Option<String>,
    pub decision_reason: Option<String>,
    pub title: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "lowercase")]
pub enum PermissionResult {
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_permissions: Option<Vec<PermissionUpdate>>,
    },
    Deny {
        #[serde(default)]
        message: String,
        #[serde(default)]
        interrupt: bool,
    },
}

pub type CanUseTool = Arc<
    dyn Fn(
            String,
            Value,
            ToolPermissionContext,
        ) -> BoxFutureResult<crate::errors::Result<PermissionResult>>
        + Send
        + Sync,
>;

pub type HookEvent = String;
pub type HookCallback = Arc<
    dyn Fn(Value, Option<String>, HookContext) -> BoxFutureResult<crate::errors::Result<Value>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub signal: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BaseHookInput {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreToolUseHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostToolUseHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_response: Value,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostToolUseFailureHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_interrupt: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserPromptSubmitHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StopHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub stop_hook_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubagentStopHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub stop_hook_active: bool,
    pub agent_id: String,
    pub agent_transcript_path: String,
    pub agent_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreCompactHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub trigger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub notification_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubagentStartHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub agent_id: String,
    pub agent_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionRequestHookInput {
    #[serde(flatten)]
    pub base: BaseHookInput,
    pub hook_event_name: String,
    pub tool_name: String,
    pub tool_input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_suggestions: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum HookInput {
    PreToolUse(PreToolUseHookInput),
    PostToolUse(PostToolUseHookInput),
    PostToolUseFailure(PostToolUseFailureHookInput),
    UserPromptSubmit(UserPromptSubmitHookInput),
    Stop(StopHookInput),
    SubagentStop(SubagentStopHookInput),
    PreCompact(PreCompactHookInput),
    Notification(NotificationHookInput),
    SubagentStart(SubagentStartHookInput),
    PermissionRequest(PermissionRequestHookInput),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreToolUseHookSpecificOutput {
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_decision_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PostToolUseHookSpecificOutput {
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_tool_output: Option<Value>,
    #[serde(
        rename = "updatedMCPToolOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub updated_mcp_tool_output: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PostToolUseFailureHookSpecificOutput {
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserPromptSubmitHookSpecificOutput {
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionStartHookSpecificOutput {
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NotificationHookSpecificOutput {
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubagentStartHookSpecificOutput {
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestHookSpecificOutput {
    pub hook_event_name: String,
    pub decision: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum HookSpecificOutput {
    PreToolUse(PreToolUseHookSpecificOutput),
    PostToolUse(PostToolUseHookSpecificOutput),
    PostToolUseFailure(PostToolUseFailureHookSpecificOutput),
    UserPromptSubmit(UserPromptSubmitHookSpecificOutput),
    SessionStart(SessionStartHookSpecificOutput),
    Notification(NotificationHookSpecificOutput),
    SubagentStart(SubagentStartHookSpecificOutput),
    PermissionRequest(PermissionRequestHookSpecificOutput),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AsyncHookJsonOutput {
    #[serde(rename = "async")]
    pub async_: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub async_timeout: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SyncHookJsonOutput {
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppress_output: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum HookJsonOutput {
    Async(AsyncHookJsonOutput),
    Sync(SyncHookJsonOutput),
}

#[derive(Clone)]
pub struct HookMatcher {
    pub matcher: Option<String>,
    pub hooks: Vec<HookCallback>,
    pub timeout: Option<f64>,
}

impl std::fmt::Debug for HookMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookMatcher")
            .field("matcher", &self.matcher)
            .field("hooks_len", &self.hooks.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// MCP / settings / sandbox option helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpServerConfig {
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        env: HashMap<String, String>,
    },
    #[serde(rename = "sse")]
    Sse {
        url: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
    #[serde(rename = "sdk")]
    Sdk { name: String },
    #[serde(untagged)]
    Raw(Value),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxNetworkConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_managed_domains_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_unix_sockets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all_unix_sockets: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_mach_lookup: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy_port: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socks_proxy_port: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxIgnoreViolations {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unsandboxed_commands: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetworkConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_violations: Option<SandboxIgnoreViolations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_weaker_nested_sandbox: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SdkPluginConfig {
    #[serde(rename = "local")]
    Local { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ThinkingConfig {
    #[serde(rename = "adaptive")]
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
    #[serde(rename = "enabled")]
    Enabled {
        budget_tokens: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<String>,
    },
    #[serde(rename = "disabled")]
    Disabled,
}

#[derive(Clone)]
pub struct ClaudeAgentOptions {
    pub tools: Option<Tools>,
    pub allowed_tools: Vec<String>,
    pub system_prompt: Option<SystemPrompt>,
    pub mcp_servers: Option<Value>,
    pub sdk_mcp_servers: HashMap<String, Arc<SdkMcpServer>>,
    pub strict_mcp_config: bool,
    pub permission_mode: Option<PermissionMode>,
    pub continue_conversation: bool,
    pub resume: Option<String>,
    pub session_id: Option<String>,
    pub max_turns: Option<i64>,
    pub max_budget_usd: Option<f64>,
    pub disallowed_tools: Vec<String>,
    pub model: Option<String>,
    pub fallback_model: Option<String>,
    pub betas: Vec<SdkBeta>,
    pub permission_prompt_tool_name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cli_path: Option<PathBuf>,
    pub settings: Option<String>,
    pub add_dirs: Vec<PathBuf>,
    pub env: HashMap<String, String>,
    pub extra_args: IndexMap<String, Option<String>>,
    pub max_buffer_size: Option<usize>,
    pub stderr: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub can_use_tool: Option<CanUseTool>,
    pub hooks: Option<HashMap<HookEvent, Vec<HookMatcher>>>,
    pub user: Option<String>,
    pub include_partial_messages: bool,
    pub include_hook_events: bool,
    pub fork_session: bool,
    pub agents: Option<HashMap<String, AgentDefinition>>,
    pub setting_sources: Option<Vec<SettingSource>>,
    pub skills: Option<Skills>,
    pub sandbox: Option<SandboxSettings>,
    pub plugins: Vec<SdkPluginConfig>,
    pub max_thinking_tokens: Option<i64>,
    pub thinking: Option<ThinkingConfig>,
    pub effort: Option<String>,
    pub output_format: Option<Value>,
    pub enable_file_checkpointing: bool,
    pub session_store: Option<Arc<dyn SessionStore>>,
    pub session_store_flush: SessionStoreFlushMode,
    pub load_timeout_ms: u64,
    pub task_budget: Option<TaskBudget>,
}

impl Default for ClaudeAgentOptions {
    fn default() -> Self {
        Self {
            tools: None,
            allowed_tools: Vec::new(),
            system_prompt: None,
            mcp_servers: Some(Value::Object(JsonMap::new())),
            sdk_mcp_servers: HashMap::new(),
            strict_mcp_config: false,
            permission_mode: None,
            continue_conversation: false,
            resume: None,
            session_id: None,
            max_turns: None,
            max_budget_usd: None,
            disallowed_tools: Vec::new(),
            model: None,
            fallback_model: None,
            betas: Vec::new(),
            permission_prompt_tool_name: None,
            cwd: None,
            cli_path: None,
            settings: None,
            add_dirs: Vec::new(),
            env: HashMap::new(),
            extra_args: IndexMap::new(),
            max_buffer_size: None,
            stderr: None,
            can_use_tool: None,
            hooks: None,
            user: None,
            include_partial_messages: false,
            include_hook_events: false,
            fork_session: false,
            agents: None,
            setting_sources: None,
            skills: None,
            sandbox: None,
            plugins: Vec::new(),
            max_thinking_tokens: None,
            thinking: None,
            effort: None,
            output_format: None,
            enable_file_checkpointing: false,
            session_store: None,
            session_store_flush: SessionStoreFlushMode::Batched,
            load_timeout_ms: 60_000,
            task_budget: None,
        }
    }
}

impl ClaudeAgentOptions {
    pub fn add_sdk_mcp_server(
        &mut self,
        server_name: impl Into<String>,
        server: Arc<SdkMcpServer>,
    ) {
        self.sdk_mcp_servers.insert(server_name.into(), server);
    }

    pub fn with_sdk_mcp_server(
        mut self,
        server_name: impl Into<String>,
        server: Arc<SdkMcpServer>,
    ) -> Self {
        self.add_sdk_mcp_server(server_name, server);
        self
    }
}

impl std::fmt::Debug for ClaudeAgentOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeAgentOptions")
            .field("tools", &self.tools)
            .field("allowed_tools", &self.allowed_tools)
            .field("system_prompt", &self.system_prompt)
            .field("mcp_servers", &self.mcp_servers)
            .field(
                "sdk_mcp_servers",
                &self.sdk_mcp_servers.keys().collect::<Vec<_>>(),
            )
            .field("strict_mcp_config", &self.strict_mcp_config)
            .field("permission_mode", &self.permission_mode)
            .field("continue_conversation", &self.continue_conversation)
            .field("resume", &self.resume)
            .field("session_id", &self.session_id)
            .field("max_turns", &self.max_turns)
            .field("model", &self.model)
            .field("cwd", &self.cwd)
            .field("cli_path", &self.cli_path)
            .field("has_can_use_tool", &self.can_use_tool.is_some())
            .field("has_hooks", &self.hooks.is_some())
            .field("has_session_store", &self.session_store.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Skills {
    All(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStoreFlushMode {
    Batched,
    Eager,
}

// ---------------------------------------------------------------------------
// Message content and stream types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "thinking")]
    Thinking { thinking: String, signature: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    #[serde(rename = "server_tool_use")]
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "advisor_tool_result")]
    ServerToolResult { tool_use_id: String, content: Value },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextBlock {
    pub text: String,
}

impl From<TextBlock> for ContentBlock {
    fn from(block: TextBlock) -> Self {
        ContentBlock::Text { text: block.text }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThinkingBlock {
    pub thinking: String,
    pub signature: String,
}

impl From<ThinkingBlock> for ContentBlock {
    fn from(block: ThinkingBlock) -> Self {
        ContentBlock::Thinking {
            thinking: block.thinking,
            signature: block.signature,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolUseBlock {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl From<ToolUseBlock> for ContentBlock {
    fn from(block: ToolUseBlock) -> Self {
        ContentBlock::ToolUse {
            id: block.id,
            name: block.name,
            input: block.input,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultBlock {
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl From<ToolResultBlock> for ContentBlock {
    fn from(block: ToolResultBlock) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: block.tool_use_id,
            content: block.content,
            is_error: block.is_error,
        }
    }
}

pub type ServerToolName = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerToolUseBlock {
    pub id: String,
    pub name: ServerToolName,
    pub input: Value,
}

impl From<ServerToolUseBlock> for ContentBlock {
    fn from(block: ServerToolUseBlock) -> Self {
        ContentBlock::ServerToolUse {
            id: block.id,
            name: block.name,
            input: block.input,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerToolResultBlock {
    pub tool_use_id: String,
    pub content: Value,
}

impl From<ServerToolResultBlock> for ContentBlock {
    fn from(block: ServerToolResultBlock) -> Self {
        ContentBlock::ServerToolResult {
            tool_use_id: block.tool_use_id,
            content: block.content,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum UserMessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserMessage {
    pub content: UserMessageContent,
    pub uuid: Option<String>,
    pub parent_tool_use_id: Option<String>,
    pub tool_use_result: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub parent_tool_use_id: Option<String>,
    pub error: Option<String>,
    pub usage: Option<Value>,
    pub message_id: Option<String>,
    pub stop_reason: Option<String>,
    pub session_id: Option<String>,
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemMessage {
    pub subtype: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskStartedMessage {
    pub subtype: String,
    pub data: Value,
    pub task_id: String,
    pub description: String,
    pub uuid: String,
    pub session_id: String,
    pub tool_use_id: Option<String>,
    pub task_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskUsage {
    pub total_tokens: i64,
    pub tool_uses: i64,
    pub duration_ms: i64,
}

pub type TaskNotificationStatus = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskProgressMessage {
    pub subtype: String,
    pub data: Value,
    pub task_id: String,
    pub description: String,
    pub usage: TaskUsage,
    pub uuid: String,
    pub session_id: String,
    pub tool_use_id: Option<String>,
    pub last_tool_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskNotificationMessage {
    pub subtype: String,
    pub data: Value,
    pub task_id: String,
    pub status: TaskNotificationStatus,
    pub output_file: String,
    pub summary: String,
    pub uuid: String,
    pub session_id: String,
    pub tool_use_id: Option<String>,
    pub usage: Option<TaskUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MirrorErrorMessage {
    pub subtype: String,
    pub data: Value,
    pub key: Option<SessionKey>,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookEventMessage {
    pub subtype: String,
    pub hook_event_name: String,
    pub data: Value,
    pub session_id: Option<String>,
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeferredToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResultMessage {
    pub subtype: String,
    pub duration_ms: i64,
    pub duration_api_ms: i64,
    pub is_error: bool,
    pub num_turns: i64,
    pub session_id: String,
    pub stop_reason: Option<String>,
    pub total_cost_usd: Option<f64>,
    pub usage: Option<Value>,
    pub result: Option<String>,
    pub structured_output: Option<Value>,
    pub model_usage: Option<Value>,
    pub permission_denials: Option<Value>,
    pub deferred_tool_use: Option<DeferredToolUse>,
    pub errors: Option<Vec<String>>,
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StreamEvent {
    pub uuid: String,
    pub session_id: String,
    pub event: Value,
    pub parent_tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RateLimitInfo {
    pub status: String,
    pub resets_at: Option<i64>,
    pub rate_limit_type: Option<String>,
    pub utilization: Option<f64>,
    pub overage_status: Option<String>,
    pub overage_resets_at: Option<i64>,
    pub overage_disabled_reason: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RateLimitEvent {
    pub rate_limit_info: RateLimitInfo,
    pub uuid: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    System(SystemMessage),
    TaskStarted(TaskStartedMessage),
    TaskProgress(TaskProgressMessage),
    TaskNotification(TaskNotificationMessage),
    MirrorError(MirrorErrorMessage),
    HookEvent(HookEventMessage),
    Result(ResultMessage),
    StreamEvent(StreamEvent),
    RateLimitEvent(RateLimitEvent),
}

impl Message {
    pub fn is_result(&self) -> bool {
        matches!(self, Message::Result(_))
    }
}

// ---------------------------------------------------------------------------
// Session store and session listing types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub project_key: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionListSubkeysKey {
    pub project_key: String,
    pub session_id: String,
}

pub type SessionStoreEntry = Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStoreListEntry {
    pub session_id: String,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSummaryEntry {
    pub session_id: String,
    pub mtime: i64,
    pub data: JsonMap,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn append(&self, key: SessionKey, entries: Vec<SessionStoreEntry>) -> crate::Result<()>;

    async fn load(&self, key: SessionKey) -> crate::Result<Option<Vec<SessionStoreEntry>>>;

    async fn list_sessions(
        &self,
        _project_key: String,
    ) -> crate::Result<Vec<SessionStoreListEntry>> {
        Err(crate::errors::ClaudeSdkError::Runtime(
            "list_sessions is not implemented".to_string(),
        ))
    }

    async fn list_session_summaries(
        &self,
        _project_key: String,
    ) -> crate::Result<Vec<SessionSummaryEntry>> {
        Err(crate::errors::ClaudeSdkError::Runtime(
            "list_session_summaries is not implemented".to_string(),
        ))
    }

    async fn delete(&self, _key: SessionKey) -> crate::Result<()> {
        Err(crate::errors::ClaudeSdkError::Runtime(
            "delete is not implemented".to_string(),
        ))
    }

    async fn list_subkeys(&self, _key: SessionListSubkeysKey) -> crate::Result<Vec<String>> {
        Err(crate::errors::ClaudeSdkError::Runtime(
            "list_subkeys is not implemented".to_string(),
        ))
    }

    fn supports_list_sessions(&self) -> bool {
        false
    }

    fn supports_list_session_summaries(&self) -> bool {
        false
    }

    fn supports_delete(&self) -> bool {
        false
    }

    fn supports_list_subkeys(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SdkSessionInfo {
    pub session_id: String,
    pub summary: String,
    pub last_modified: i64,
    pub file_size: Option<u64>,
    pub custom_title: Option<String>,
    pub first_prompt: Option<String>,
    pub git_branch: Option<String>,
    pub cwd: Option<String>,
    pub tag: Option<String>,
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMessage {
    #[serde(rename = "type")]
    pub message_type: String,
    pub uuid: String,
    pub session_id: String,
    pub message: Value,
    pub parent_tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerConnectionStatus {
    Connected,
    Failed,
    NeedsAuth,
    Pending,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
}

pub type McpServerStatusConfig = Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world: Option<bool>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub name: String,
    pub status: McpServerConnectionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_info: Option<McpServerInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<McpServerStatusConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolInfo>>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusResponse {
    pub mcp_servers: Vec<McpServerStatus>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageCategory {
    pub name: String,
    pub tokens: i64,
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_deferred: Option<bool>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageResponse {
    pub categories: Vec<ContextUsageCategory>,
    pub total_tokens: i64,
    pub max_tokens: i64,
    pub raw_max_tokens: i64,
    pub percentage: f64,
    pub model: String,
    pub is_auto_compact_enabled: bool,
    pub memory_files: Vec<Value>,
    pub mcp_tools: Vec<Value>,
    pub agents: Vec<Value>,
    pub grid_rows: Vec<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_builtin_tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_sections: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash_commands: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_breakdown: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_usage: Option<Value>,
    #[serde(flatten)]
    pub extra: JsonMap,
}
