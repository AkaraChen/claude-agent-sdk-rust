use std::error::Error;

use claude_agent_sdk::{
    ClaudeSdkError, CliConnectionError, CliJsonDecodeError, CliNotFoundError, ProcessError,
};

fn assert_error_trait<E: Error>(_err: &E) {}

#[test]
fn base_sdk_error_formats_message() {
    let error = ClaudeSdkError::Runtime("Something went wrong".to_string());
    assert_eq!(error.to_string(), "Something went wrong");
    assert_error_trait(&error);
}

#[test]
fn cli_not_found_error_formats_and_wraps() {
    let error = CliNotFoundError::new("Claude Code not found", None);
    assert!(error.to_string().contains("Claude Code not found"));
    let wrapped: ClaudeSdkError = error.into();
    assert!(wrapped.to_string().contains("Claude Code not found"));

    let with_path = CliNotFoundError::new("Claude Code not found at", Some("/bin/nope".into()));
    assert_eq!(with_path.cli_path.as_deref(), Some("/bin/nope"));
    assert!(with_path.to_string().contains("/bin/nope"));
}

#[test]
fn connection_error_formats_and_wraps() {
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
