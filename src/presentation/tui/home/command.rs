//! Parsing, dispatch, and completion for the home screen's command mode.
//!
//! The home screen doubles as a small command shell: in command mode the user
//! types a command, which this module turns into log lines and a side effect.
//! Everything here is pure (no terminal IO), so the whole command surface is
//! directly testable.

use super::state::LogLine;

/// A side effect a command asks the screen / event loop to perform, beyond
/// appending its produced log lines.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing extra — just append the produced lines.
    None,
    /// Clear the output log.
    Clear,
    /// Quit the whole application.
    Quit,
}

/// The result of running a command: lines to append plus a side effect.
#[derive(Debug)]
pub struct CommandResult {
    pub lines: Vec<LogLine>,
    pub effect: Effect,
}

/// Known commands and their one-line descriptions. Drives `man`/`help` output
/// and Tab completion. Kept in display order.
pub const COMMANDS: &[(&str, &str)] = &[
    ("session", "Create or manage sessions (branch + worktree)"),
    ("space", "Switch between worktrees"),
    ("ai", "Talk to the AI agent"),
    ("terminal", "Open an interactive terminal"),
    ("history", "Show the command history"),
    ("doctor", "Check that required tools are installed"),
    ("man", "Show help for commands (man <command> for details)"),
    ("clear", "Clear the output pane"),
    ("quit", "Leave usagi and return to the project list"),
];

/// Commands that are recognised but whose real behaviour is not built yet.
/// They produce a friendly "coming soon" line so the surface is discoverable.
const COMING_SOON: &[&str] = &["session", "space", "ai", "terminal", "doctor"];

/// Looks up a command's description by name.
fn describe(name: &str) -> Option<&'static str> {
    COMMANDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, desc)| *desc)
}

/// Builds the `man`/`help` output. With no argument it lists every command;
/// with an argument it shows that command's description (or an error).
fn man(arg: &str) -> CommandResult {
    if arg.is_empty() {
        let mut lines = vec![LogLine::output("Available commands:")];
        for (name, desc) in COMMANDS {
            lines.push(LogLine::output(format!("  {name:<9}{desc}")));
        }
        return CommandResult {
            lines,
            effect: Effect::None,
        };
    }

    match describe(arg) {
        Some(desc) => CommandResult {
            lines: vec![LogLine::output(format!("{arg} — {desc}"))],
            effect: Effect::None,
        },
        None => CommandResult {
            lines: vec![LogLine::error(format!("no manual entry for \"{arg}\""))],
            effect: Effect::None,
        },
    }
}

/// Builds the `history` output from the commands entered so far.
fn history(entries: &[String]) -> CommandResult {
    let lines = if entries.is_empty() {
        vec![LogLine::output("No commands in history yet.")]
    } else {
        entries
            .iter()
            .enumerate()
            .map(|(i, entry)| LogLine::output(format!("  {:>3}  {entry}", i + 1)))
            .collect()
    };
    CommandResult {
        lines,
        effect: Effect::None,
    }
}

/// Parses and runs `input`, given the command `history` entered so far (not
/// including the current input). Returns the lines to append and a side effect.
pub fn dispatch(input: &str, history_entries: &[String]) -> CommandResult {
    let trimmed = input.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();

    match name {
        "" => CommandResult {
            lines: Vec::new(),
            effect: Effect::None,
        },
        "man" | "help" => man(rest),
        "history" => history(history_entries),
        "clear" => CommandResult {
            lines: Vec::new(),
            effect: Effect::Clear,
        },
        "quit" | "exit" => CommandResult {
            lines: vec![LogLine::output("USAGI run away ( ^-^)ノ")],
            effect: Effect::Quit,
        },
        other if COMING_SOON.contains(&other) => CommandResult {
            lines: vec![LogLine::output(format!("\"{other}\" is coming soon 🐰"))],
            effect: Effect::None,
        },
        other => CommandResult {
            lines: vec![LogLine::error(format!(
                "unknown command: \"{other}\" (try \"man\")"
            ))],
            effect: Effect::None,
        },
    }
}

/// The result of a Tab completion: the (possibly extended) input, plus the
/// candidate command names when the completion is ambiguous.
#[derive(Debug, PartialEq, Eq)]
pub struct Completion {
    pub input: String,
    pub candidates: Vec<String>,
}

/// Longest common prefix shared by every string in `names`.
fn common_prefix(names: &[&str]) -> String {
    let first = match names.first() {
        Some(first) => *first,
        None => return String::new(),
    };
    let mut end = first.len();
    for name in &names[1..] {
        end = end.min(name.len());
        while !name.is_char_boundary(end) || first[..end] != name[..end] {
            end -= 1;
        }
    }
    first[..end].to_string()
}

/// Completes the command word in `input` against the known command names.
///
/// Completion only applies to the first word: once the input contains
/// whitespace (i.e. arguments are being typed) the input is returned unchanged.
/// A unique match is filled in; an ambiguous one extends to the longest common
/// prefix and reports the candidates; no match leaves the input untouched.
pub fn complete(input: &str) -> Completion {
    if input.contains(char::is_whitespace) {
        return Completion {
            input: input.to_string(),
            candidates: Vec::new(),
        };
    }

    let matches: Vec<&str> = COMMANDS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| name.starts_with(input))
        .collect();

    match matches.as_slice() {
        [] => Completion {
            input: input.to_string(),
            candidates: Vec::new(),
        },
        [only] => Completion {
            input: only.to_string(),
            candidates: Vec::new(),
        },
        many => Completion {
            input: common_prefix(many),
            candidates: many.iter().map(|name| name.to_string()).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::tui::home::state::LineKind;

    #[test]
    fn empty_input_does_nothing() {
        let result = dispatch("   ", &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::None);
    }

    #[test]
    fn man_without_argument_lists_every_command() {
        let result = dispatch("man", &[]);
        assert_eq!(result.effect, Effect::None);
        let joined = result
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Available commands"));
        for (name, _) in COMMANDS {
            assert!(joined.contains(name));
        }
    }

    #[test]
    fn help_is_an_alias_for_man() {
        let result = dispatch("help", &[]);
        assert!(result.lines[0].text.contains("Available commands"));
    }

    #[test]
    fn man_with_a_known_command_shows_its_description() {
        let result = dispatch("man session", &[]);
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].kind, LineKind::Output);
        assert!(result.lines[0].text.starts_with("session —"));
    }

    #[test]
    fn man_with_an_unknown_command_is_an_error() {
        let result = dispatch("man nope", &[]);
        assert_eq!(result.lines[0].kind, LineKind::Error);
        assert!(result.lines[0].text.contains("no manual entry"));
    }

    #[test]
    fn history_is_empty_when_nothing_was_entered() {
        let result = dispatch("history", &[]);
        assert_eq!(result.lines.len(), 1);
        assert!(result.lines[0].text.contains("No commands in history"));
    }

    #[test]
    fn history_numbers_previous_entries() {
        let entries = vec!["man".to_string(), "doctor".to_string()];
        let result = dispatch("history", &entries);
        assert_eq!(result.lines.len(), 2);
        assert!(result.lines[0].text.contains("1"));
        assert!(result.lines[0].text.contains("man"));
        assert!(result.lines[1].text.contains("doctor"));
    }

    #[test]
    fn clear_requests_the_clear_effect_with_no_lines() {
        let result = dispatch("clear", &[]);
        assert!(result.lines.is_empty());
        assert_eq!(result.effect, Effect::Clear);
    }

    #[test]
    fn quit_and_exit_request_the_quit_effect() {
        assert_eq!(dispatch("quit", &[]).effect, Effect::Quit);
        assert_eq!(dispatch("exit", &[]).effect, Effect::Quit);
    }

    #[test]
    fn coming_soon_commands_are_recognised() {
        for name in COMING_SOON {
            let result = dispatch(name, &[]);
            assert_eq!(result.effect, Effect::None);
            assert_eq!(result.lines[0].kind, LineKind::Output);
            assert!(result.lines[0].text.contains("coming soon"));
        }
    }

    #[test]
    fn unknown_command_is_reported_as_an_error() {
        let result = dispatch("frobnicate", &[]);
        assert_eq!(result.lines[0].kind, LineKind::Error);
        assert!(result.lines[0].text.contains("unknown command"));
    }

    #[test]
    fn complete_fills_in_a_unique_match() {
        // "doc" only matches "doctor".
        let completion = complete("doc");
        assert_eq!(completion.input, "doctor");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn complete_extends_to_the_common_prefix_and_lists_candidates() {
        // "s" matches both "session" and "space"; common prefix is "s".
        let completion = complete("s");
        assert_eq!(completion.input, "s");
        assert_eq!(completion.candidates, vec!["session", "space"]);
    }

    #[test]
    fn complete_with_no_match_leaves_input_untouched() {
        let completion = complete("zzz");
        assert_eq!(completion.input, "zzz");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn complete_does_not_touch_input_with_arguments() {
        let completion = complete("man ses");
        assert_eq!(completion.input, "man ses");
        assert!(completion.candidates.is_empty());
    }

    #[test]
    fn common_prefix_handles_the_empty_case() {
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn common_prefix_finds_the_shared_start() {
        assert_eq!(common_prefix(&["session", "space"]), "s");
        assert_eq!(common_prefix(&["terminal", "terminal"]), "terminal");
    }
}
