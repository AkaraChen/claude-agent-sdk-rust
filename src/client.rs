use std::sync::Arc;

use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};

use crate::errors::{ClaudeSdkError, CliConnectionError, Result};
use crate::internal::client::initialize_timeout_from_env;
use crate::internal::message_parser::parse_message;
use crate::internal::query::Query;
use crate::internal::session_resume::{
    MaterializedResume, apply_materialized_options, build_mirror_batcher,
    materialize_resume_session,
};
use crate::internal::session_store_validation::validate_session_store_options;
use crate::query::Prompt;
use crate::transport::{SubprocessCliTransport, Transport};
use crate::types::{
    ClaudeAgentOptions, ContextUsageResponse, McpStatusResponse, Message, PermissionMode,
    SystemPrompt,
};
use futures::FutureExt;

pub struct ClaudeSdkClient {
    pub options: ClaudeAgentOptions,
    custom_transport: Option<Arc<dyn Transport>>,
    transport: Option<Arc<dyn Transport>>,
    query: Option<Arc<Query>>,
    materialized: Option<MaterializedResume>,
}

impl ClaudeSdkClient {
    pub fn new(options: Option<ClaudeAgentOptions>, transport: Option<Arc<dyn Transport>>) -> Self {
        Self {
            options: options.unwrap_or_default(),
            custom_transport: transport,
            transport: None,
            query: None,
            materialized: None,
        }
    }

    pub async fn connect(&mut self, prompt: Option<Prompt>) -> Result<()> {
        validate_session_store_options(&self.options)?;
        self.materialized = if self.custom_transport.is_none() {
            materialize_resume_session(&self.options).await?
        } else {
            None
        };
        if let Err(err) = self.connect_inner(prompt).await {
            let _ = self.disconnect().await;
            return Err(err);
        }
        Ok(())
    }

    async fn connect_inner(&mut self, prompt: Option<Prompt>) -> Result<()> {
        let mut options = self.options.clone();
        if options.can_use_tool.is_some() {
            if matches!(prompt, Some(Prompt::Text(_))) {
                return Err(ClaudeSdkError::InvalidOptions(
                    "can_use_tool callback requires streaming mode. Please provide prompt as a stream instead of a string.".to_string(),
                ));
            }
            if options.permission_prompt_tool_name.is_some() {
                return Err(ClaudeSdkError::InvalidOptions(
                    "can_use_tool callback cannot be used with permission_prompt_tool_name. Please use one or the other.".to_string(),
                ));
            }
            options.permission_prompt_tool_name = Some("stdio".to_string());
        }
        if let Some(materialized) = &self.materialized {
            options = apply_materialized_options(&options, materialized);
        }

        let transport: Arc<dyn Transport> = self
            .custom_transport
            .clone()
            .unwrap_or_else(|| Arc::new(SubprocessCliTransport::new(options.clone())));
        transport.connect().await?;

        let exclude_dynamic_sections = match &options.system_prompt {
            Some(SystemPrompt::Preset {
                exclude_dynamic_sections,
                ..
            }) => *exclude_dynamic_sections,
            _ => None,
        };
        let agents = options
            .agents
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        let query = Arc::new(Query::new(
            transport.clone(),
            true,
            options.can_use_tool.clone(),
            options.hooks.clone(),
            options.sdk_mcp_servers.clone(),
            initialize_timeout_from_env()?,
            agents,
            exclude_dynamic_sections,
            options.skills.clone(),
        ));

        if let Some(store) = options.session_store.clone() {
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
                    self.materialized.as_ref(),
                    Some(&options.env),
                    on_error,
                    options.session_store_flush,
                ))
                .await;
        }

        query.start().await;
        if let Err(err) = async {
            query.initialize().await?;

            match prompt {
                Some(Prompt::Text(text)) => {
                    let message = json!({
                        "type": "user",
                        "message": {"role": "user", "content": text},
                        "parent_tool_use_id": null,
                        "session_id": "default",
                    });
                    transport.write(&(message.to_string() + "\n")).await?;
                }
                Some(Prompt::Stream(stream)) => {
                    let q = query.clone();
                    query.spawn_task(q.stream_input(stream)).await;
                }
                None => {}
            }
            Ok::<(), ClaudeSdkError>(())
        }
        .await
        {
            let _ = query.close().await;
            return Err(err);
        }

        self.transport = Some(transport);
        self.query = Some(query);
        Ok(())
    }

    pub fn receive_messages(&self) -> Result<BoxStream<'static, Result<Message>>> {
        let query = self
            .query
            .clone()
            .ok_or_else(|| CliConnectionError::new("Not connected. Call connect() first."))?;
        Ok(Box::pin(async_stream::try_stream! {
            let mut raw = query.receive_messages();
            while let Some(item) = raw.next().await {
                if let Some(message) = parse_message(item?)? {
                    yield message;
                }
            }
        }))
    }

    pub async fn send_query(&self, prompt: Prompt, session_id: Option<&str>) -> Result<()> {
        self.connected_query()?;
        let transport = self
            .transport
            .clone()
            .ok_or_else(|| CliConnectionError::new("Not connected. Call connect() first."))?;
        let session_id = session_id.unwrap_or("default");
        match prompt {
            Prompt::Text(text) => {
                let message = json!({
                    "type": "user",
                    "message": {"role": "user", "content": text},
                    "parent_tool_use_id": null,
                    "session_id": session_id,
                });
                transport.write(&(message.to_string() + "\n")).await
            }
            Prompt::Stream(mut stream) => {
                while let Some(mut msg) = stream.next().await {
                    if msg.session_id().is_none() {
                        msg.set_session_id(session_id);
                    }
                    transport
                        .write(&(serde_json::to_string(&msg)? + "\n"))
                        .await?;
                }
                Ok(())
            }
        }
    }

    pub async fn interrupt(&self) -> Result<()> {
        self.connected_query()?.interrupt().await
    }

    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        self.connected_query()?.set_permission_mode(mode).await
    }

    pub async fn set_model(&self, model: Option<String>) -> Result<()> {
        self.connected_query()?.set_model(model).await
    }

    pub async fn rewind_files(&self, user_message_id: String) -> Result<()> {
        self.connected_query()?.rewind_files(user_message_id).await
    }

    pub async fn reconnect_mcp_server(&self, server_name: String) -> Result<()> {
        self.connected_query()?
            .reconnect_mcp_server(server_name)
            .await
    }

    pub async fn toggle_mcp_server(&self, server_name: String, enabled: bool) -> Result<()> {
        self.connected_query()?
            .toggle_mcp_server(server_name, enabled)
            .await
    }

    pub async fn stop_task(&self, task_id: String) -> Result<()> {
        self.connected_query()?.stop_task(task_id).await
    }

    pub async fn get_mcp_status(&self) -> Result<McpStatusResponse> {
        Ok(serde_json::from_value(
            self.connected_query()?.get_mcp_status().await?,
        )?)
    }

    pub async fn get_context_usage(&self) -> Result<ContextUsageResponse> {
        Ok(serde_json::from_value(
            self.connected_query()?.get_context_usage().await?,
        )?)
    }

    pub async fn get_server_info(&self) -> Result<Option<Value>> {
        Ok(self.connected_query()?.initialization_result().await)
    }

    pub fn receive_response(&self) -> Result<BoxStream<'static, Result<Message>>> {
        let stream = self.receive_messages()?;
        Ok(Box::pin(async_stream::try_stream! {
            futures::pin_mut!(stream);
            while let Some(message) = stream.next().await {
                let message = message?;
                let done = message.is_result();
                yield message;
                if done {
                    break;
                }
            }
        }))
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        if let Some(query) = &self.query {
            query.close().await?;
        }
        self.query = None;
        self.transport = None;
        if let Some(materialized) = &self.materialized {
            materialized.cleanup().await;
        }
        self.materialized = None;
        Ok(())
    }

    fn connected_query(&self) -> Result<Arc<Query>> {
        self.query
            .clone()
            .ok_or_else(|| CliConnectionError::new("Not connected. Call connect() first.").into())
    }
}
