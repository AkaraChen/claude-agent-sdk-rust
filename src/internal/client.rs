use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_stream::try_stream;
use futures::FutureExt;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;

use crate::errors::{ClaudeSdkError, Result};
use crate::internal::message_parser::parse_message;
use crate::internal::query::Query;
use crate::internal::session_resume::{
    apply_materialized_options, build_mirror_batcher, materialize_resume_session,
};
use crate::internal::session_store_validation::validate_session_store_options;
use crate::query::Prompt;
use crate::transport::{SubprocessCliTransport, Transport};
use crate::types::{ClaudeAgentOptions, HookMatcher, Message, SystemPrompt};

pub struct InternalClient;

pub(crate) fn initialize_timeout_from_env() -> Result<Duration> {
    let initialize_timeout_ms = match std::env::var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT") {
        Ok(raw) => raw.parse::<i64>().map_err(|err| {
            ClaudeSdkError::InvalidOptions(format!(
                "Invalid CLAUDE_CODE_STREAM_CLOSE_TIMEOUT: {raw} ({err})"
            ))
        })?,
        Err(std::env::VarError::NotPresent) => 60_000,
        Err(err) => {
            return Err(ClaudeSdkError::InvalidOptions(format!(
                "Invalid CLAUDE_CODE_STREAM_CLOSE_TIMEOUT: {err}"
            )));
        }
    };
    Ok(Duration::from_millis(
        initialize_timeout_ms.max(60_000) as u64
    ))
}

struct ProcessQueryCleanup {
    query: Option<Arc<Query>>,
    materialized: Option<crate::internal::session_resume::MaterializedResume>,
}

impl ProcessQueryCleanup {
    fn new(materialized: Option<crate::internal::session_resume::MaterializedResume>) -> Self {
        Self {
            query: None,
            materialized,
        }
    }

    fn materialized(&self) -> Option<&crate::internal::session_resume::MaterializedResume> {
        self.materialized.as_ref()
    }

    fn set_query(&mut self, query: Arc<Query>) {
        self.query = Some(query);
    }

    async fn cleanup_now(&mut self) -> Result<()> {
        let close_result = if let Some(query) = self.query.take() {
            query.close().await
        } else {
            Ok(())
        };
        if let Some(materialized) = self.materialized.take() {
            materialized.cleanup().await;
        }
        close_result
    }
}

impl Drop for ProcessQueryCleanup {
    fn drop(&mut self) {
        let query = self.query.take();
        let materialized = self.materialized.take();
        if query.is_none() && materialized.is_none() {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Some(query) = query {
                    let _ = query.close().await;
                }
                if let Some(materialized) = materialized {
                    materialized.cleanup().await;
                }
            });
        }
    }
}

impl InternalClient {
    pub fn new() -> Self {
        Self
    }

    fn convert_hooks_to_internal_format(
        hooks: Option<HashMap<String, Vec<HookMatcher>>>,
    ) -> Option<HashMap<String, Vec<HookMatcher>>> {
        hooks
    }

    pub fn process_query(
        self,
        prompt: Prompt,
        options: ClaudeAgentOptions,
        transport: Option<Arc<dyn Transport>>,
    ) -> BoxStream<'static, Result<Message>> {
        try_stream! {
            validate_session_store_options(&options)?;

            let materialized = if transport.is_none() {
                materialize_resume_session(&options).await?
            } else {
                None
            };
            let mut cleanup = ProcessQueryCleanup::new(materialized);

            let mut configured_options = options.clone();
            if configured_options.can_use_tool.is_some() {
                if matches!(prompt, Prompt::Text(_)) {
                    Err(ClaudeSdkError::InvalidOptions(
                        "can_use_tool callback requires streaming mode. Please provide prompt as a stream instead of a string.".to_string(),
                    ))?;
                }
                if configured_options.permission_prompt_tool_name.is_some() {
                    Err(ClaudeSdkError::InvalidOptions(
                        "can_use_tool callback cannot be used with permission_prompt_tool_name. Please use one or the other.".to_string(),
                    ))?;
                }
                configured_options.permission_prompt_tool_name = Some("stdio".to_string());
            }

            if let Some(materialized) = cleanup.materialized() {
                configured_options = apply_materialized_options(&configured_options, materialized);
            }

            let chosen_transport: Arc<dyn Transport> = transport.unwrap_or_else(|| {
                Arc::new(SubprocessCliTransport::new(configured_options.clone()))
            });
            chosen_transport.connect().await?;

            let exclude_dynamic_sections = match &configured_options.system_prompt {
                Some(SystemPrompt::Preset { exclude_dynamic_sections, .. }) => *exclude_dynamic_sections,
                _ => None,
            };
            let agents = configured_options
                .agents
                .as_ref()
                .map(serde_json::to_value)
                .transpose()?;
            let query = Arc::new(Query::new(
                chosen_transport.clone(),
                true,
                configured_options.can_use_tool.clone(),
                Self::convert_hooks_to_internal_format(configured_options.hooks.clone()),
                configured_options.sdk_mcp_servers.clone(),
                initialize_timeout_from_env()?,
                agents,
                exclude_dynamic_sections,
                configured_options.skills.clone(),
            ));
            cleanup.set_query(query.clone());

            if let Some(store) = configured_options.session_store.clone() {
                let query_for_error = query.clone();
                let on_error = Arc::new(move |key, error| {
                    let query_for_error = query_for_error.clone();
                    async move {
                        query_for_error.report_mirror_error(key, error).await;
                    }
                    .boxed()
                });
                query
                    .set_transcript_mirror_batcher(build_mirror_batcher(
                        store,
                        cleanup.materialized(),
                        Some(&configured_options.env),
                        on_error,
                        configured_options.session_store_flush,
                    ))
                    .await;
            }

            query.start().await;
            query.initialize().await?;

            match prompt {
                Prompt::Text(text) => {
                    let user_message = json!({
                        "type": "user",
                        "session_id": "",
                        "message": {"role": "user", "content": text},
                        "parent_tool_use_id": null,
                    });
                    chosen_transport.write(&(user_message.to_string() + "\n")).await?;
                    let q = query.clone();
                    query.spawn_task(async move {
                        let _ = q.wait_for_result_and_end_input().await;
                    }).await;
                }
                Prompt::Stream(stream) => {
                    let q = query.clone();
                    query.spawn_task(q.stream_input(stream)).await;
                }
            }

            let mut raw_messages = query.receive_messages();
            while let Some(raw) = raw_messages.next().await {
                if let Some(message) = parse_message(raw?)? {
                    yield message;
                }
            }

            cleanup.cleanup_now().await?;
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::sync::MutexGuard;
    use std::time::Duration;

    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        old: Option<String>,
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.old.take() {
                Some(value) => unsafe { env::set_var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT", value) },
                None => unsafe { env::remove_var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT") },
            }
        }
    }

    fn set_timeout_env(value: Option<&str>) -> EnvGuard {
        let lock = ENV_LOCK.lock().unwrap();
        let old = env::var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT").ok();
        match value {
            Some(value) => unsafe { env::set_var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT", value) },
            None => unsafe { env::remove_var("CLAUDE_CODE_STREAM_CLOSE_TIMEOUT") },
        }
        EnvGuard { old, _lock: lock }
    }

    #[test]
    fn initialize_timeout_defaults_to_sixty_seconds() {
        let _guard = set_timeout_env(None);
        assert_eq!(
            initialize_timeout_from_env().unwrap(),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn initialize_timeout_reads_env_milliseconds() {
        let _guard = set_timeout_env(Some("120000"));
        assert_eq!(
            initialize_timeout_from_env().unwrap(),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn initialize_timeout_enforces_sixty_second_minimum() {
        {
            let _guard = set_timeout_env(Some("1000"));
            assert_eq!(
                initialize_timeout_from_env().unwrap(),
                Duration::from_secs(60)
            );
        }
        {
            let _guard = set_timeout_env(Some("-1000"));
            assert_eq!(
                initialize_timeout_from_env().unwrap(),
                Duration::from_secs(60)
            );
        }
    }

    #[test]
    fn initialize_timeout_rejects_invalid_env_like_python_int() {
        let _guard = set_timeout_env(Some("not-a-number"));
        let err = initialize_timeout_from_env().unwrap_err();
        assert!(
            err.to_string()
                .contains("Invalid CLAUDE_CODE_STREAM_CLOSE_TIMEOUT")
        );
    }
}
