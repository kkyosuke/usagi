//! `usagi agent-phase <phase>` — エージェントのライフサイクル phase を daemon へ報告する内部コマンド。
//!
//! usagi がエージェント起動時に各 provider のライフサイクルフックへ配線し、フックが phase
//! （例: `ended`）を引数に渡して呼ぶ。人手で叩くものではない（`--help` 非表示）。フックは
//! 終了コードだけを見るため、標準出力には何も書かない。
//!
//! 報告元の runtime は daemon が hook process の OS peer lineage から exact live runtime を
//! 解決して束縛する（caller は runtime / session / path を名指しできない）。phase 引数は
//! [`usagi_core::domain::session_lifecycle::AgentPhase`] の closed vocabulary であり、hook の
//! stdin JSON が名乗る `hook_event_name` が usagi の配線どおりその phase を意味することも
//! 検証する。実 stdin と env の読み取りは合成ルートが束ね、この module は純粋な request
//! 組み立てだけを持つ。

use std::io::{self, Read, Write};

use serde::Deserialize;
use usagi_core::domain::{
    agent::ProviderSessionId, session_lifecycle::AgentPhase as ReportedPhase,
};
use usagi_core::infrastructure::client::{DaemonRequest, McpCallerContext};

use crate::cli::{Run, RunOutcome};

/// `usagi agent-phase <phase>` のハンドラ。
pub struct AgentPhase {
    pub phase: String,
    pub hook_event: Option<String>,
}

impl Run for AgentPhase {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::ReportAgentPhase {
            phase: self.phase.clone(),
            hook_event: self.hook_event.clone(),
        })
    }
}

/// hook JSON のうち、この報告が読む field。`session_id` は `SessionStart` でだけ
/// daemon へ渡し、`transcript_path` などの他 field は deserialize 対象にせず file も開かない。
#[derive(Debug, Deserialize)]
struct PhaseHookInput {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default, alias = "conversationId")]
    session_id: Option<ProviderSessionId>,
}

/// Safe input failure. It deliberately carries neither the reported phase nor
/// the daemon credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseInputError {
    UnknownPhase,
    InvalidPayload,
    WrongEvent,
}

impl std::fmt::Display for PhaseInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnknownPhase => "unknown agent lifecycle phase",
            Self::InvalidPayload => "invalid agent lifecycle hook payload",
            Self::WrongEvent => "unexpected agent lifecycle hook event for this phase",
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
/// event which usagi does not wire to that phase, a `SessionStart` payload
/// without its provider session ID, or a missing daemon-issued runtime
/// credential. Authentication is derived from the hook process at the daemon.
pub fn request_from_hook(
    reader: &mut dyn Read,
    phase: &str,
    declared_event: Option<&str>,
    credential: Option<String>,
) -> Result<DaemonRequest, PhaseInputError> {
    let phase = ReportedPhase::parse_token(phase)
        .filter(|phase| phase.is_reportable())
        .ok_or(PhaseInputError::UnknownPhase)?;
    let input = serde_json::from_reader::<_, PhaseHookInput>(reader)
        .map_err(|_| PhaseInputError::InvalidPayload)?;
    let event = declared_event
        .or(input.hook_event_name.as_deref())
        .ok_or(PhaseInputError::InvalidPayload)?;
    let canonical = ReportedPhase::for_hook_event(event).or(match event {
        // Antigravity's documented hooks do not repeat the configured event
        // name in stdin. `PreInvocation` begins a model turn and also carries
        // the provider conversation ID.
        "PreInvocation" => Some(ReportedPhase::Running),
        _ => None,
    });
    // v2.9-era launch material wired `PostToolUse` to `running`. Accept that
    // one historical pairing so an already-running Agent does not fail its
    // hook after the `usagi` executable is updated in place. New launch
    // material reports `waiting`, which is the canonical mapping above.
    let legacy_post_tool = event == "PostToolUse" && phase == ReportedPhase::Running;
    if canonical != Some(phase) && !legacy_post_tool {
        return Err(PhaseInputError::WrongEvent);
    }
    let native_session_id = if matches!(event, "SessionStart" | "PreInvocation") {
        Some(input.session_id.ok_or(PhaseInputError::InvalidPayload)?)
    } else {
        None
    };
    Ok(DaemonRequest::AgentPhaseReport {
        phase,
        native_session_id,
        caller_context: credential
            .filter(|value| !value.is_empty())
            .map(|credential| McpCallerContext { credential }),
    })
}

/// Provider-native stdout payload for a successfully reported Antigravity
/// hook. Claude and Codex omit `declared_event` and therefore keep their
/// existing empty stdout contract.
#[must_use]
pub fn response_for_declared_event(declared_event: Option<&str>) -> Option<&'static str> {
    match declared_event {
        Some("PreToolUse") => Some(r#"{"decision":"allow"}"#),
        Some("PreInvocation" | "PostToolUse") => Some("{}"),
        Some("Stop") => Some(r#"{"decision":"stop"}"#),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{PhaseInputError, request_from_hook, response_for_declared_event};
    use crate::cli::{Command, RunOutcome, execute};

    #[test]
    fn hidden_handler_requests_composition_report_without_output() {
        let (outcome, output) = execute(Command::AgentPhase {
            phase: "ended".into(),
            hook_event: None,
        });
        assert_eq!(
            outcome,
            RunOutcome::ReportAgentPhase {
                phase: "ended".into(),
                hook_event: None,
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
            ("PostToolUse", "waiting"),
            ("PermissionRequest", "waiting"),
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
                None,
                Some("runtime-secret".into()),
            )
            .unwrap();
            let mut expected = serde_json::json!({
                "kind": "agent_phase_report",
                "phase": phase,
                "caller_context": {"credential": "runtime-secret"}
            });
            if event == "SessionStart" {
                expected["native_session_id"] = serde_json::json!("provider-session");
            }
            assert_eq!(serde_json::to_value(request).unwrap(), expected);
        }
    }

    #[test]
    fn legacy_post_tool_use_running_hook_remains_compatible() {
        let request = request_from_hook(
            &mut Cursor::new(
                br#"{"session_id":"provider-session","hook_event_name":"PostToolUse"}"#,
            ),
            "running",
            None,
            Some("runtime-secret".to_owned()),
        )
        .unwrap();
        assert_eq!(serde_json::to_value(request).unwrap()["phase"], "running");
    }

    #[test]
    fn unknown_phase_malformed_and_wrong_event_fail_closed() {
        for (payload, phase, credential, expected) in [
            (
                br#"{"hook_event_name":"Stop"}"#.as_slice(),
                "interrupted",
                Some("runtime-secret".to_owned()),
                PhaseInputError::UnknownPhase,
            ),
            (
                br#"{"hook_event_name":"Stop"}"#.as_slice(),
                "none",
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
                br"{}".as_slice(),
                "ended",
                Some("runtime-secret".to_owned()),
                PhaseInputError::InvalidPayload,
            ),
            (
                br#"{"hook_event_name":"SessionStart"}"#.as_slice(),
                "ready",
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
        ] {
            let error =
                request_from_hook(&mut Cursor::new(payload), phase, None, credential).unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("runtime-secret"));
        }
        let request = request_from_hook(
            &mut Cursor::new(br#"{"session_id":"provider-session","hook_event_name":"Stop"}"#),
            "ended",
            None,
            None,
        )
        .unwrap();
        assert!(
            serde_json::to_value(request).unwrap()["caller_context"].is_null(),
            "hook credentials must not be inherited through the Agent environment"
        );
    }

    #[test]
    fn antigravity_declared_event_accepts_camel_case_conversation_id() {
        let request = request_from_hook(
            &mut Cursor::new(
                br#"{"conversationId":"agy-conversation","workspacePaths":["/worktree"]}"#,
            ),
            "running",
            Some("PreInvocation"),
            Some("runtime-secret".into()),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "kind": "agent_phase_report",
                "phase": "running",
                "native_session_id": "agy-conversation",
                "caller_context": {"credential": "runtime-secret"}
            })
        );
    }

    #[test]
    fn antigravity_hook_responses_follow_each_event_output_contract() {
        assert_eq!(
            response_for_declared_event(Some("PreToolUse")),
            Some(r#"{"decision":"allow"}"#)
        );
        assert_eq!(response_for_declared_event(Some("PostToolUse")), Some("{}"));
        assert_eq!(
            response_for_declared_event(Some("PreInvocation")),
            Some("{}")
        );
        assert_eq!(
            response_for_declared_event(Some("Stop")),
            Some(r#"{"decision":"stop"}"#)
        );
        assert_eq!(response_for_declared_event(None), None);
    }
}
