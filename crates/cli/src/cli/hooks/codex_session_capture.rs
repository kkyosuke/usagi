//! `usagi codex-session-capture` — Codex `SessionStart` hook の内部入口。
//!
//! Codex が documented hook channel の stdin に渡す JSON から current
//! `session_id` だけを受理する。`transcript_path` を含む他 field は読み取らず、daemon が
//! 発行して process environment に閉じ込めた credential と組み合わせて private IPC
//! request にする。

use std::io::{self, Read, Write};

use serde::Deserialize;
use usagi_core::{
    domain::agent::ProviderSessionId,
    usecase::client::{DaemonRequest, McpCallerContext},
};

use crate::cli::{Run, RunOutcome};

/// `usagi codex-session-capture` の handler。実 stdin/env は合成ルートが束ねる。
pub struct CodexSessionCapture;

impl Run for CodexSessionCapture {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::CaptureCodexSession)
    }
}

#[derive(Debug, Deserialize)]
struct SessionStartInput {
    session_id: ProviderSessionId,
    hook_event_name: String,
}

/// Safe input failure. It deliberately carries neither the provider ID nor
/// the daemon credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureInputError {
    InvalidPayload,
    WrongEvent,
    MissingCredential,
}

impl std::fmt::Display for CaptureInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidPayload => "invalid Codex SessionStart hook payload",
            Self::WrongEvent => "unexpected Codex hook event",
            Self::MissingCredential => "Codex runtime credential is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CaptureInputError {}

/// Converts one documented Codex hook object into the private daemon request.
/// Unknown fields, including `transcript_path`, are ignored and never opened.
///
/// # Errors
///
/// Returns a non-sensitive error for malformed JSON, a non-`SessionStart`
/// event, or a missing daemon-issued runtime credential.
pub fn request_from_hook(
    reader: &mut dyn Read,
    credential: Option<String>,
) -> Result<DaemonRequest, CaptureInputError> {
    let input = serde_json::from_reader::<_, SessionStartInput>(reader)
        .map_err(|_| CaptureInputError::InvalidPayload)?;
    if input.hook_event_name != "SessionStart" {
        return Err(CaptureInputError::WrongEvent);
    }
    let credential = credential
        .filter(|value| !value.is_empty())
        .ok_or(CaptureInputError::MissingCredential)?;
    Ok(DaemonRequest::CodexSessionCapture {
        native_session_id: input.session_id,
        caller_context: McpCallerContext { credential },
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{CaptureInputError, request_from_hook};
    use crate::cli::{Command, RunOutcome, execute};

    #[test]
    fn hidden_handler_requests_composition_capture_without_output() {
        let (outcome, output) = execute(Command::CodexSessionCapture);
        assert_eq!(outcome, RunOutcome::CaptureCodexSession);
        assert!(output.is_empty());
    }

    #[test]
    fn official_session_start_payload_becomes_credential_fenced_request() {
        let mut input = Cursor::new(
            br#"{
                "session_id":"codex-session",
                "transcript_path":"/must/not/be/read.jsonl",
                "cwd":"/worktree",
                "hook_event_name":"SessionStart",
                "model":"test"
            }"#,
        );
        let request = request_from_hook(&mut input, Some("runtime-secret".into())).unwrap();
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "kind": "codex_session_capture",
                "native_session_id": "codex-session",
                "caller_context": {"credential": "runtime-secret"}
            })
        );
    }

    #[test]
    fn malformed_wrong_event_and_missing_credential_fail_closed() {
        for (payload, credential, expected) in [
            (
                br#"{"session_id":""}"#.as_slice(),
                Some("credential".to_owned()),
                CaptureInputError::InvalidPayload,
            ),
            (
                br#"{"session_id":"secret-id","hook_event_name":"Stop"}"#.as_slice(),
                Some("credential".to_owned()),
                CaptureInputError::WrongEvent,
            ),
            (
                br#"{"session_id":"secret-id","hook_event_name":"SessionStart"}"#.as_slice(),
                None,
                CaptureInputError::MissingCredential,
            ),
        ] {
            let error = request_from_hook(&mut Cursor::new(payload), credential).unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("secret-id"));
        }
    }
}
