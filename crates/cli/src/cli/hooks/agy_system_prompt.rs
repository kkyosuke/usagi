//! Provider-native transient system context for Antigravity's `PreInvocation` hook.

use std::io::{self, Write};

use crate::cli::{Run, RunOutcome};

/// The composition root supplies the daemon-issued private launch environment.
pub struct AgySystemPrompt;

impl Run for AgySystemPrompt {
    fn run(&self, _out: &mut dyn Write) -> io::Result<RunOutcome> {
        Ok(RunOutcome::EmitAgySystemPrompt)
    }
}

/// Writes transient system context before every model invocation, including a
/// resumed conversation. It adds no user turn or persistent trajectory step.
///
/// # Errors
/// Returns a safe error for missing launch context, or propagates output IO.
pub fn write_response(prompt: Option<&str>, out: &mut dyn Write) -> io::Result<()> {
    let prompt = prompt
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "AGY system context is unavailable",
            )
        })?;
    let response = serde_json::json!({"injectSteps":[{"ephemeralMessage":prompt}]});
    writeln!(out, "{response}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Command, execute};

    #[test]
    fn handler_defers_environment_access_to_composition() {
        let (outcome, output) = execute(Command::AgySystemPrompt);
        assert_eq!(outcome, RunOutcome::EmitAgySystemPrompt);
        assert!(output.is_empty());
    }

    #[test]
    fn emits_system_context_as_one_escaped_transient_step() {
        let prompt = "Scope: session\nRole: reviewer \"日本語\"\\\n";
        let mut output = Vec::new();
        write_response(Some(prompt), &mut output).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
            serde_json::json!({"injectSteps":[{"ephemeralMessage":prompt}]})
        );
        assert!(output.ends_with(b"\n"));
    }

    #[test]
    fn missing_context_and_output_failures_do_not_emit_a_partial_prompt() {
        for prompt in [None, Some(""), Some(" \n")] {
            let mut output = Vec::new();
            assert_eq!(
                write_response(prompt, &mut output).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(output.is_empty());
        }
        assert!(write_response(Some("private role"), &mut io::Cursor::new([0_u8; 0])).is_err());
    }
}
