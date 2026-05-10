use futures::stream::BoxStream;

use crate::Result;
use crate::internal::client::InternalClient;
use crate::transport::Transport;
use crate::types::{ClaudeAgentOptions, InputMessage, Message};
use std::sync::Arc;

pub enum Prompt {
    Text(String),
    Stream(BoxStream<'static, InputMessage>),
}

impl From<String> for Prompt {
    fn from(value: String) -> Self {
        Prompt::Text(value)
    }
}

impl From<&str> for Prompt {
    fn from(value: &str) -> Self {
        Prompt::Text(value.to_string())
    }
}

pub fn query(
    prompt: Prompt,
    options: Option<ClaudeAgentOptions>,
    transport: Option<Arc<dyn Transport>>,
) -> BoxStream<'static, Result<Message>> {
    InternalClient::new().process_query(prompt, options.unwrap_or_default(), transport)
}
