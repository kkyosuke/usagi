//! Closeup `agent` command grammar: which agent CLI a launch selects.
//!
//! The parser resolves a typed token to the closed
//! [`DefaultModel`] vocabulary and refuses anything the machine cannot run, so
//! the Closeup command surface never offers, completes, or submits a CLI that is
//! not installed. Adapter argv, models, and secrets remain daemon concerns; this
//! module produces only a product-neutral selection.

use usagi_core::domain::settings::{AvailableModels, DefaultModel};

/// The `-m` short flag and its long form, in completion order.
const MODEL_FLAGS: [&str; 2] = ["-m", "--model"];

/// A validated `agent` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentRequest {
    /// The selected CLI. An omitted `-m` resolves to the configured default.
    pub model: DefaultModel,
    /// Whether the selection came from the configured default rather than the
    /// typed arguments. The Closeup notice uses it to name what will launch.
    pub from_default: bool,
}

/// Parse `[-m|--model <cli>]`, resolving an omitted flag to `default`.
///
/// A positional CLI name (`agent codex`) is accepted as the flagless form the
/// command has always taken. Both forms go through the same vocabulary and the
/// same `available` gate.
///
/// # Errors
///
/// Returns a stable, user-safe message for an unknown flag, a repeated `-m`, a
/// missing or unknown CLI name, more than one selection, or a CLI whose
/// executable is not installed.
pub fn parse(
    arguments: &str,
    default: DefaultModel,
    available: AvailableModels,
) -> Result<AgentRequest, &'static str> {
    let mut selected = None;
    let mut expects_model = false;
    for token in arguments.split_whitespace() {
        if expects_model {
            selected = Some(resolve(token)?);
            expects_model = false;
            continue;
        }
        match token {
            flag if MODEL_FLAGS.contains(&flag) && selected.is_some() => {
                return Err("agent accepts one -m selection");
            }
            flag if MODEL_FLAGS.contains(&flag) => expects_model = true,
            flag if flag.starts_with('-') => return Err("unknown agent flag"),
            _ if selected.is_some() => return Err("agent accepts one -m selection"),
            positional => selected = Some(resolve(positional)?),
        }
    }
    if expects_model {
        return Err("-m requires an agent CLI name");
    }
    let model = selected.unwrap_or(default);
    if !available.contains(model) {
        return Err(if selected.is_some() {
            "that agent CLI is not installed"
        } else {
            "the configured agent CLI is not installed"
        });
    }
    Ok(AgentRequest {
        model,
        from_default: selected.is_none(),
    })
}

fn resolve(token: &str) -> Result<DefaultModel, &'static str> {
    DefaultModel::from_selector(token).ok_or("unknown agent CLI")
}

/// Completion candidates for the argument text typed after `agent`.
///
/// Only installed CLIs are offered. The result is the full argument text the
/// caller should replace the current arguments with, so a caller can join it to
/// the command name without knowing the grammar.
#[must_use]
pub fn completions(arguments: &str, available: AvailableModels) -> Vec<String> {
    // A trailing space means the previous token is finished: complete the next
    // position rather than re-completing what was typed.
    let closed = arguments.ends_with(char::is_whitespace);
    let mut tokens = arguments.split_whitespace().collect::<Vec<_>>();
    let prefix = if closed {
        ""
    } else {
        tokens.pop().unwrap_or("")
    };
    match tokens.as_slice() {
        // `agent ` / `agent -<prefix>`: offer the flag, then the bare CLI names
        // so a flagless `agent codex` still completes.
        [] => MODEL_FLAGS
            .into_iter()
            .map(str::to_owned)
            .chain(selectors(available))
            .filter(|candidate| candidate.starts_with(prefix))
            .collect(),
        // `agent -m <prefix>`: only installed CLI names.
        [flag] if MODEL_FLAGS.contains(flag) => selectors(available)
            .into_iter()
            .filter(|candidate| candidate.starts_with(prefix))
            .map(|candidate| format!("{flag} {candidate}"))
            .collect(),
        _ => Vec::new(),
    }
}

fn selectors(available: AvailableModels) -> Vec<String> {
    available
        .iter()
        .map(|model| model.selector().to_owned())
        .collect()
}

/// The `-m <cli>` choices offered by the Closeup action picker, in
/// [`DefaultModel::ALL`] order and restricted to installed CLIs.
#[must_use]
pub fn model_choices(available: AvailableModels, default: DefaultModel) -> Vec<ModelChoice> {
    available
        .iter()
        .map(|model| ModelChoice {
            label: if model == default {
                format!("-m {}  (default)", model.selector())
            } else {
                format!("-m {}", model.selector())
            },
            value: format!("-m {}", model.selector()),
        })
        .collect()
}

/// One selectable CLI in the Closeup action picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// The row shown to the user, marking the configured default.
    pub label: String,
    /// The arguments appended to `agent` when the row is confirmed.
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::{AgentRequest, completions, model_choices, parse};
    use usagi_core::domain::settings::{AvailableModels, DefaultModel};

    fn all() -> AvailableModels {
        AvailableModels::all()
    }

    #[test]
    fn omitted_flag_resolves_the_configured_default() {
        for default in DefaultModel::ALL {
            assert_eq!(
                parse("", default, all()),
                Ok(AgentRequest {
                    model: default,
                    from_default: true,
                })
            );
            assert_eq!(
                parse("   ", default, all()),
                Ok(AgentRequest {
                    model: default,
                    from_default: true,
                })
            );
        }
    }

    #[test]
    fn selects_every_cli_by_flag_long_flag_and_bare_name() {
        for (input, expected) in [
            ("-m claude", DefaultModel::Claude),
            ("--model codex", DefaultModel::OpenAi),
            ("-m sakana.ai", DefaultModel::SakanaAi),
            // The vocabulary accepts the profile ID, the executable name, and
            // separator-insensitive spellings.
            ("-m sakana-ai", DefaultModel::SakanaAi),
            ("-m sakana_ai", DefaultModel::SakanaAi),
            ("-m codex-fugu", DefaultModel::SakanaAi),
            ("-m SAKANA.AI", DefaultModel::SakanaAi),
            ("codex", DefaultModel::OpenAi),
            ("  claude  ", DefaultModel::Claude),
        ] {
            assert_eq!(
                parse(input, DefaultModel::OpenAi, all()),
                Ok(AgentRequest {
                    model: expected,
                    from_default: false,
                }),
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_unusable_and_ambiguous_arguments() {
        for (input, message) in [
            ("-m", "-m requires an agent CLI name"),
            ("--model", "-m requires an agent CLI name"),
            ("-m gemini", "unknown agent CLI"),
            ("emacs", "unknown agent CLI"),
            ("-x claude", "unknown agent flag"),
            ("-m claude -m codex", "agent accepts one -m selection"),
            ("-m claude codex", "agent accepts one -m selection"),
            ("claude codex", "agent accepts one -m selection"),
        ] {
            assert_eq!(parse(input, DefaultModel::OpenAi, all()), Err(message));
        }
    }

    #[test]
    fn refuses_a_cli_that_is_not_installed() {
        let only_codex = AvailableModels::new([DefaultModel::OpenAi]);
        assert_eq!(
            parse("-m claude", DefaultModel::OpenAi, only_codex),
            Err("that agent CLI is not installed")
        );
        assert_eq!(
            parse("-m sakana.ai", DefaultModel::OpenAi, only_codex),
            Err("that agent CLI is not installed")
        );
        // A configured default that is no longer installed is reported as a
        // configuration problem instead of silently launching another CLI.
        assert_eq!(
            parse("", DefaultModel::Claude, only_codex),
            Err("the configured agent CLI is not installed")
        );
        assert_eq!(
            parse("", DefaultModel::OpenAi, AvailableModels::default()),
            Err("the configured agent CLI is not installed")
        );
    }

    #[test]
    fn completes_the_flag_then_only_installed_cli_names() {
        assert_eq!(
            completions("", all()),
            ["-m", "--model", "claude", "codex", "sakana.ai"]
        );
        assert_eq!(completions("-", all()), ["-m", "--model"]);
        assert_eq!(completions("--", all()), ["--model"]);
        assert_eq!(completions("sak", all()), ["sakana.ai"]);
        assert_eq!(
            completions("-m ", all()),
            ["-m claude", "-m codex", "-m sakana.ai"]
        );
        assert_eq!(completions("-m sak", all()), ["-m sakana.ai"]);
        assert_eq!(
            completions("--model c", all()),
            ["--model claude", "--model codex"]
        );
        // Absent CLIs are neither offered nor completed.
        let only_sakana = AvailableModels::new([DefaultModel::SakanaAi]);
        assert_eq!(completions("-m ", only_sakana), ["-m sakana.ai"]);
        assert_eq!(completions("-m c", only_sakana), Vec::<String>::new());
        assert!(completions("-m codex ", all()).is_empty());
        assert!(completions("-m codex extra", all()).is_empty());
    }

    #[test]
    fn picker_choices_mark_the_configured_default() {
        let choices = model_choices(all(), DefaultModel::SakanaAi);
        assert_eq!(
            choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["-m claude", "-m codex", "-m sakana.ai  (default)"]
        );
        assert_eq!(
            choices.iter().map(|c| c.value.as_str()).collect::<Vec<_>>(),
            ["-m claude", "-m codex", "-m sakana.ai"]
        );
        assert!(model_choices(AvailableModels::default(), DefaultModel::OpenAi).is_empty());
        // Exercise the derived vocabulary used by the modal projection.
        let choice = choices[0].clone();
        assert_eq!(choice, choices[0]);
        assert!(format!("{choice:?}").contains("claude"));
    }
}
