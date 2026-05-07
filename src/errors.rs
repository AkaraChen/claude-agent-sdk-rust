use thiserror::Error;

pub type Result<T> = std::result::Result<T, ClaudeSdkError>;

#[derive(Debug, Error)]
pub enum ClaudeSdkError {
    #[error(transparent)]
    CliConnection(#[from] CliConnectionError),

    #[error(transparent)]
    CliNotFound(#[from] CliNotFoundError),

    #[error(transparent)]
    Process(#[from] ProcessError),

    #[error(transparent)]
    CliJsonDecode(#[from] CliJsonDecodeError),

    #[error(transparent)]
    MessageParse(#[from] MessageParseError),

    #[error("{0}")]
    InvalidOptions(String),

    #[error("{0}")]
    Runtime(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct CliConnectionError {
    pub message: String,
}

impl CliConnectionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct CliNotFoundError {
    pub message: String,
    pub cli_path: Option<String>,
}

impl CliNotFoundError {
    pub fn new(message: impl Into<String>, cli_path: Option<String>) -> Self {
        let base = message.into();
        let message = match &cli_path {
            Some(path) => format!("{base}: {path}"),
            None => base,
        };
        Self { message, cli_path }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct ProcessError {
    pub message: String,
    pub exit_code: Option<i32>,
    pub stderr: Option<String>,
}

impl ProcessError {
    pub fn new(message: impl Into<String>, exit_code: Option<i32>, stderr: Option<String>) -> Self {
        let mut message = message.into();
        if let Some(code) = exit_code {
            message = format!("{message} (exit code: {code})");
        }
        if let Some(stderr) = &stderr {
            if !stderr.is_empty() {
                message = format!("{message}\nError output: {stderr}");
            }
        }
        Self {
            message,
            exit_code,
            stderr,
        }
    }
}

#[derive(Debug, Error)]
#[error("Failed to decode JSON: {preview}...")]
pub struct CliJsonDecodeError {
    pub line: String,
    pub preview: String,
    pub original_error: String,
}

impl CliJsonDecodeError {
    pub fn new(line: impl Into<String>, original_error: impl ToString) -> Self {
        let line = line.into();
        let preview = line.chars().take(100).collect();
        Self {
            line,
            preview,
            original_error: original_error.to_string(),
        }
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct MessageParseError {
    pub message: String,
    pub data: Option<serde_json::Value>,
}

impl MessageParseError {
    pub fn new(message: impl Into<String>, data: Option<serde_json::Value>) -> Self {
        Self {
            message: message.into(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_not_found_error_formats_message_and_path() {
        let error = CliNotFoundError::new("Claude Code not found", None);
        assert!(error.to_string().contains("Claude Code not found"));
        let wrapped: ClaudeSdkError = error.into();
        assert!(wrapped.to_string().contains("Claude Code not found"));

        let with_path = CliNotFoundError::new("Claude Code not found at", Some("/bin/nope".into()));
        assert_eq!(with_path.cli_path.as_deref(), Some("/bin/nope"));
        assert!(with_path.to_string().contains("/bin/nope"));
    }

    #[test]
    fn cli_connection_error_formats_message() {
        let error = CliConnectionError::new("Failed to connect to CLI");
        assert!(error.to_string().contains("Failed to connect to CLI"));
        let wrapped: ClaudeSdkError = error.into();
        assert!(wrapped.to_string().contains("Failed to connect to CLI"));
    }

    #[test]
    fn process_error_preserves_exit_code_and_stderr() {
        let error = ProcessError::new("Process failed", Some(1), Some("Command not found".into()));
        assert_eq!(error.exit_code, Some(1));
        assert_eq!(error.stderr.as_deref(), Some("Command not found"));
        let text = error.to_string();
        assert!(text.contains("Process failed"));
        assert!(text.contains("exit code: 1"));
        assert!(text.contains("Command not found"));
    }

    #[test]
    fn json_decode_error_preserves_line_and_original_error() {
        let original = serde_json::from_str::<serde_json::Value>("{invalid json}")
            .unwrap_err()
            .to_string();
        let error = CliJsonDecodeError::new("{invalid json}", &original);
        assert_eq!(error.line, "{invalid json}");
        assert_eq!(error.original_error, original);
        assert!(error.to_string().contains("Failed to decode JSON"));
    }

    #[test]
    fn message_parse_error_preserves_data() {
        let data = serde_json::json!({"bad": true});
        let error = MessageParseError::new("missing type", Some(data.clone()));
        assert_eq!(error.data, Some(data));
        assert_eq!(error.to_string(), "missing type");
    }
}
