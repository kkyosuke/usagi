//! `usagi agent-phase <phase>` — エージェントのライフサイクル phase を daemon へ報告する内部コマンド。
//!
//! usagi がエージェント起動時に Claude のライフサイクルフックへ配線し、フックが phase
//! （例: `ended`）を引数に渡して呼ぶ。人手で叩くものではない（`--help` 非表示）。フックは
//! 終了コードだけを見るため、標準出力には何も書かない。
//!
//! 報告元の runtime は daemon が発行して process environment に閉じ込めた credential だけで
//! 束縛する（caller は runtime / session / path を名指しできない）。phase 引数は
//! [`usagi_core::domain::session_lifecycle::AgentPhase`] の closed vocabulary であり、hook の
//! stdin JSON が名乗る `hook_event_name` が usagi の配線どおりその phase を意味することも
//! 検証する。実 stdin と env の読み取りは合成ルートが束ね、この module は純粋な request
//! 組み立てだけを持つ。

use std::io::{self, Read, Write};

use serde::Deserialize;
use usagi_core::domain::session_lifecycle::AgentPhase as ReportedPhase;
use usagi_core::usecase::client::{DaemonRequest, McpCallerContext};

use crate::cli::{Run, RunOutcome};

/// `usagi agent-phase <phase>` のハンドラ。
pub struct AgentPhase {
    pub phase: String,
}

impl Run for AgentPhase {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::ReportAgentPhase {
            phase: self.phase.clone(),
        })
    }
}

/// hook JSON のうち、この報告が読む唯一の field。`transcript_path` や `session_id` などの
/// 他 field は deserialize 対象にせず、file も開かない。
#[derive(Debug, Deserialize)]
struct PhaseHookInput {
    hook_event_name: String,
}

/// Safe input failure. It deliberately carries neither the reported phase nor
/// the daemon credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseInputError {
    UnknownPhase,
    InvalidPayload,
    WrongEvent,
    MissingCredential,
}

impl std::fmt::Display for PhaseInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnknownPhase => "unknown agent lifecycle phase",
            Self::InvalidPayload => "invalid agent lifecycle hook payload",
            Self::WrongEvent => "unexpected agent lifecycle hook event for this phase",
            Self::MissingCredential => "agent runtime credential is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for PhaseInputError {}

/// Converts one documented lifecycle hook invocation into the private daemon
/// request. The phase argument must be the phase usagi wired to the event which
/// the payload names, so a report cannot claim a phase for another event.
///
/// # Errors
///
/// Returns a non-sensitive error for an unknown phase token, malformed JSON, an
/// event which usagi does not wire to that phase, or a missing daemon-issued
/// runtime credential.
pub fn request_from_hook(
    reader: &mut dyn Read,
    phase: &str,
    credential: Option<String>,
) -> Result<DaemonRequest, PhaseInputError> {
    let phase = ReportedPhase::parse_token(phase).ok_or(PhaseInputError::UnknownPhase)?;
    let input = serde_json::from_reader::<_, PhaseHookInput>(reader)
        .map_err(|_| PhaseInputError::InvalidPayload)?;
    if ReportedPhase::for_hook_event(&input.hook_event_name) != Some(phase) {
        return Err(PhaseInputError::WrongEvent);
    }
    let credential = credential
        .filter(|value| !value.is_empty())
        .ok_or(PhaseInputError::MissingCredential)?;
    Ok(DaemonRequest::AgentPhaseReport {
        phase,
        caller_context: McpCallerContext { credential },
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{PhaseInputError, request_from_hook};
    use crate::cli::{Command, RunOutcome, execute};

    #[test]
    fn hidden_handler_requests_composition_report_without_output() {
        let (outcome, output) = execute(Command::AgentPhase {
            phase: "ended".into(),
        });
        assert_eq!(
            outcome,
            RunOutcome::ReportAgentPhase {
                phase: "ended".into()
            }
        );
        assert!(output.is_empty());
    }

    #[test]
    fn wired_lifecycle_events_become_credential_fenced_phase_requests() {
        for (event, phase) in [
            ("SessionStart", "ready"),
            ("UserPromptSubmit", "running"),
            ("PreToolUse", "running"),
            ("Notification", "waiting"),
            ("Stop", "ended"),
            ("SessionEnd", "exited"),
        ] {
            let payload = format!(
                r#"{{
                    "session_id":"provider-session",
                    "transcript_path":"/must/not/be/read.jsonl",
                    "cwd":"/worktree",
                    "hook_event_name":"{event}"
                }}"#
            );
            let request = request_from_hook(
                &mut Cursor::new(payload),
                phase,
                Some("runtime-secret".into()),
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(request).unwrap(),
                serde_json::json!({
                    "kind": "agent_phase_report",
                    "phase": phase,
                    "caller_context": {"credential": "runtime-secret"}
                })
            );
        }
    }

    #[test]
    fn unknown_phase_malformed_wrong_event_and_missing_credential_fail_closed() {
        for (payload, phase, credential, expected) in [
            (
                br#"{"hook_event_name":"Stop"}"#.as_slice(),
                "interrupted",
                Some("runtime-secret".to_owned()),
                PhaseInputError::UnknownPhase,
            ),
            (
                br#"{"hook_event_name":42}"#.as_slice(),
                "ended",
                Some("runtime-secret".to_owned()),
                PhaseInputError::InvalidPayload,
            ),
            (
                br#"{}"#.as_slice(),
                "ended",
                Some("runtime-secret".to_owned()),
                PhaseInputError::InvalidPayload,
            ),
            (
                // `Stop` は `ended` にだけ配線されており、別 phase を名乗れない。
                br#"{"hook_event_name":"Stop"}"#.as_slice(),
                "waiting",
                Some("runtime-secret".to_owned()),
                PhaseInputError::WrongEvent,
            ),
            (
                br#"{"hook_event_name":"PostToolUse"}"#.as_slice(),
                "running",
                Some("runtime-secret".to_owned()),
                PhaseInputError::WrongEvent,
            ),
            (
                br#"{"hook_event_name":"Stop"}"#.as_slice(),
                "ended",
                None,
                PhaseInputError::MissingCredential,
            ),
            (
                br#"{"hook_event_name":"Stop"}"#.as_slice(),
                "ended",
                Some(String::new()),
                PhaseInputError::MissingCredential,
            ),
        ] {
            let error =
                request_from_hook(&mut Cursor::new(payload), phase, credential).unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("runtime-secret"));
        }
    }
}
