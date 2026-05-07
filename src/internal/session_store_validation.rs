use crate::errors::{ClaudeSdkError, Result};
use crate::types::ClaudeAgentOptions;

pub fn validate_session_store_options(options: &ClaudeAgentOptions) -> Result<()> {
    let Some(store) = &options.session_store else {
        return Ok(());
    };

    if options.continue_conversation && options.resume.is_none() && !store.supports_list_sessions()
    {
        return Err(ClaudeSdkError::InvalidOptions(
            "continue_conversation with session_store requires the store to implement list_sessions()"
                .to_string(),
        ));
    }

    if options.enable_file_checkpointing {
        return Err(ClaudeSdkError::InvalidOptions(
            "session_store cannot be combined with enable_file_checkpointing".to_string(),
        ));
    }

    Ok(())
}
