//! Rust port of the Python `claude_agent_sdk` package.

pub mod client;
pub mod errors;
pub mod internal;
pub mod mcp;
pub mod query;
pub mod session_stores;
pub mod testing;
pub mod transport;
pub mod types;

pub use claude_agent_sdk_macros::sdk_tool;
pub use client::ClaudeSdkClient;
pub use errors::{
    ClaudeSdkError, CliConnectionError, CliJsonDecodeError, CliNotFoundError, MessageParseError,
    ProcessError, Result,
};
pub use internal::message_parser::parse_message;
pub use internal::session_import::import_session_to_store;
pub use internal::session_mutations::{
    ForkSessionResult, delete_session, delete_session_via_store, fork_session,
    fork_session_via_store, rename_session, rename_session_via_store, tag_session,
    tag_session_via_store,
};
pub use internal::session_store::{
    InMemorySessionStore, file_path_to_session_key, project_key_for_directory,
};
pub use internal::session_summary::{fold_session_summary, summary_entry_to_sdk_info};
pub use internal::sessions::{
    get_session_info, get_session_info_from_store, get_session_messages,
    get_session_messages_from_store, get_subagent_messages, get_subagent_messages_from_store,
    list_sessions, list_sessions_from_store, list_subagents, list_subagents_from_store,
};
pub use mcp::{SdkMcpServer, SdkMcpTool, SdkMcpToolHandler, create_sdk_mcp_server, tool};
pub use query::{Prompt, query};
pub use rmcp::model::ToolAnnotations;
pub use rmcp::{tool as rmcp_tool, tool_handler, tool_router};
pub use session_stores::{PostgresSessionStore, RedisSessionStore, S3SessionStore};
pub use transport::{SubprocessCliTransport, Transport};
pub use types::*;

pub const VERSION: &str = "0.1.75";
pub const CLI_VERSION: &str = "2.1.131";
