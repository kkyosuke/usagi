use super::registry::common_prefix;
use super::*;
use crate::presentation::tui::home::state::LineKind;

fn registry() -> CommandRegistry {
    CommandRegistry::with_builtins()
}

#[test]
fn empty_input_does_nothing() {
    let result = registry().dispatch("   ", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::None);
}

#[test]
fn man_without_argument_lists_every_command() {
    let registry = registry();
    let result = registry.dispatch("man", &[], &[]);
    // `man`'s help text opens a scrollable modal rather than the band.
    assert_eq!(result.effect, Effect::ShowText("Help"));
    let joined = result
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Available commands"));
    for info in registry.infos() {
        assert!(joined.contains(info.name));
    }
}

#[test]
fn help_is_an_alias_for_man() {
    let result = registry().dispatch("help", &[], &[]);
    assert!(result.lines[0].text.contains("Available commands"));
}

#[test]
fn man_without_argument_hints_at_per_command_help() {
    let result = registry().dispatch("man", &[], &[]);
    let last = result.lines.last().unwrap();
    assert!(last.text.contains("man <command>"));
}

#[test]
fn man_with_a_known_command_shows_usage_and_examples() {
    let result = registry().dispatch("man session", &[], &[]);
    assert!(result.lines.len() > 1);
    // Header, then a Usage block, then an Examples block.
    assert_eq!(result.lines[0].kind, LineKind::Output);
    assert!(result.lines[0].text.starts_with("session —"));
    let joined = result
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("Usage:"));
    assert!(joined.contains("session [create|list|switch|remove] <name>"));
    assert!(joined.contains("Examples:"));
    assert!(joined.contains("session switch feature-x"));
}

#[test]
fn man_with_a_command_without_examples_omits_the_examples_block() {
    // `clear` takes no arguments and has no examples (trait defaults).
    let result = registry().dispatch("man clear", &[], &[]);
    let joined = result
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.starts_with("clear —"));
    assert!(joined.contains("Usage:"));
    // Default usage is just the command name.
    assert!(joined.contains("  clear"));
    assert!(!joined.contains("Examples:"));
}

#[test]
fn man_with_an_unknown_command_is_an_error() {
    let result = registry().dispatch("man nope", &[], &[]);
    assert_eq!(result.lines[0].kind, LineKind::Error);
    assert!(result.lines[0].text.contains("no manual entry"));
}

#[test]
fn history_is_empty_when_nothing_was_entered() {
    let result = registry().dispatch("history", &[], &[]);
    assert_eq!(result.lines.len(), 1);
    assert!(result.lines[0].text.contains("No commands in history"));
}

#[test]
fn history_numbers_previous_entries() {
    let entries = vec!["man".to_string(), "doctor".to_string()];
    let result = registry().dispatch("history", &entries, &[]);
    assert_eq!(result.lines.len(), 2);
    assert!(result.lines[0].text.contains("1"));
    assert!(result.lines[0].text.contains("man"));
    assert!(result.lines[1].text.contains("doctor"));
}

#[test]
fn clear_requests_the_clear_effect_with_no_lines() {
    let result = registry().dispatch("clear", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::Clear);
}

#[test]
fn quit_and_exit_request_the_quit_effect() {
    assert_eq!(registry().dispatch("quit", &[], &[]).effect, Effect::Quit);
    assert_eq!(registry().dispatch("exit", &[], &[]).effect, Effect::Quit);
}

#[test]
fn session_new_without_a_name_opens_the_modal() {
    // `session new` asks for a name via the modal.
    let result = registry().dispatch("session new", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::OpenSessionModal);
}

#[test]
fn session_new_with_a_name_requests_creation() {
    // Creation goes through `session new <name>` only.
    let result = registry().dispatch("session new feature-x", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(
        result.effect,
        Effect::CreateSession("feature-x".to_string())
    );
}

#[test]
fn session_create_and_its_aliases_behave_like_new() {
    // `create` is the canonical name; `c` and `new` are aliases. Each opens
    // the modal with no name and creates with one.
    for sub in ["create", "c", "new"] {
        let opened = registry().dispatch(&format!("session {sub}"), &[], &[]);
        assert_eq!(opened.effect, Effect::OpenSessionModal, "{sub} (no name)");

        let created = registry().dispatch(&format!("session {sub} feature-x"), &[], &[]);
        assert_eq!(
            created.effect,
            Effect::CreateSession("feature-x".to_string()),
            "{sub} feature-x"
        );
    }
}

#[test]
fn session_ls_is_an_alias_for_list() {
    let result = registry().dispatch("session ls", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::ListSessions);
}

#[test]
fn session_rm_is_an_alias_for_remove() {
    // `rm` with a name removes directly...
    let result = registry().dispatch("session rm old", &[], &[]);
    assert_eq!(
        result.effect,
        Effect::RemoveSession {
            name: "old".to_string(),
            force: false,
        }
    );
    // ...and `rm` with no name opens the picker, just like `remove`.
    let bare = registry().dispatch("session rm", &[], &[]);
    assert_eq!(bare.effect, Effect::OpenRemoveModal { force: false });
}

#[test]
fn bare_session_and_the_old_name_shorthand_show_usage() {
    // Bare `session` and the removed `session <name>` shorthand no longer
    // create or open the modal; they fall through to a usage error.
    for input in ["session", "session feature-x"] {
        let result = registry().dispatch(input, &[], &[]);
        assert_eq!(result.effect, Effect::None);
        assert_eq!(result.lines.len(), 1);
        assert!(result.lines[0].text.contains("usage:"));
    }
}

#[test]
fn session_list_requests_the_list_effect() {
    let result = registry().dispatch("session list", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::ListSessions);
}

#[test]
fn session_remove_parses_name_and_force_flag() {
    // A bare name removes without force.
    let result = registry().dispatch("session remove old", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(
        result.effect,
        Effect::RemoveSession {
            name: "old".to_string(),
            force: false,
        }
    );

    // `--force` (in any position) sets the force flag; extra positional
    // tokens after the name are ignored.
    for input in [
        "session remove old --force",
        "session remove -f old",
        "session remove old --force extra",
    ] {
        let result = registry().dispatch(input, &[], &[]);
        assert_eq!(
            result.effect,
            Effect::RemoveSession {
                name: "old".to_string(),
                force: true,
            }
        );
    }
}

#[test]
fn session_remove_without_a_name_opens_the_removal_modal() {
    // A bare `session remove` opens the picker (no force).
    let result = registry().dispatch("session remove", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::OpenRemoveModal { force: false });

    // `session remove --force` opens the picker carrying the force flag.
    let forced = registry().dispatch("session remove --force", &[], &[]);
    assert!(forced.lines.is_empty());
    assert_eq!(forced.effect, Effect::OpenRemoveModal { force: true });
}

#[test]
fn coming_soon_commands_are_recognised() {
    let registry = registry();
    for name in ["ai", "doctor"] {
        let result = registry.dispatch(name, &[], &[]);
        assert_eq!(result.effect, Effect::None);
        assert_eq!(result.lines[0].kind, LineKind::Output);
        assert!(result.lines[0].text.contains("coming soon"));
        assert!(result.lines[0].text.contains(name));
    }
}

fn worktree_refs() -> Vec<WorktreeRef> {
    vec![
        WorktreeRef {
            name: "main".to_string(),
            active: true,
        },
        WorktreeRef {
            name: "feature".to_string(),
            active: false,
        },
    ]
}

#[test]
fn session_switch_with_a_name_requests_activation() {
    let result = registry().dispatch("session switch feature", &[], &worktree_refs());
    assert_eq!(result.effect, Effect::Activate("feature".to_string()));
    // Resolution and messaging happen in the screen, so no lines here.
    assert!(result.lines.is_empty());
}

#[test]
fn session_switch_without_a_name_enters_switch_mode() {
    // `session switch` with no name hands off to 切替 (Switch); the event loop
    // owns the mode transition, so no lines are produced here.
    let result = registry().dispatch("session switch", &[], &worktree_refs());
    assert_eq!(result.effect, Effect::EnterSwitch);
    assert!(result.lines.is_empty());
    // Even with no sessions it still enters Switch (the left pane has the root
    // row to pick or create from).
    assert_eq!(
        registry().dispatch("session switch", &[], &[]).effect,
        Effect::EnterSwitch
    );
}

#[test]
fn terminal_requests_opening_a_shell() {
    let result = registry().dispatch("terminal", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::OpenTerminal);
}

#[test]
fn agent_requests_opening_the_agent() {
    let result = registry().dispatch("agent", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::OpenAgent);
}

#[test]
fn config_requests_opening_the_settings_screen() {
    let result = registry().dispatch("config", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::OpenConfig);
}

#[test]
fn unknown_command_is_reported_as_an_error() {
    let result = registry().dispatch("frobnicate", &[], &[]);
    assert_eq!(result.lines[0].kind, LineKind::Error);
    assert!(result.lines[0].text.contains("unknown command"));
}

#[test]
fn registered_command_is_dispatchable_and_listed() {
    struct Greet;
    impl Command for Greet {
        fn name(&self) -> &'static str {
            "greet"
        }
        fn description(&self) -> &'static str {
            "Say hello"
        }
        fn run(&self, args: &str, _ctx: &CommandContext) -> CommandResult {
            CommandResult::line(LogLine::output(format!("hello {args}")))
        }
    }

    let mut registry = registry();
    registry.register(Box::new(Greet));
    let result = registry.dispatch("greet world", &[], &[]);
    assert_eq!(result.lines[0].text, "hello world");
    // The newcomer also shows up in `man` (via the shared info list).
    assert!(registry.infos().iter().any(|i| i.name == "greet"));
}

#[test]
fn default_registry_matches_with_builtins() {
    assert_eq!(
        CommandRegistry::default().infos().len(),
        CommandRegistry::with_builtins().infos().len()
    );
}

#[test]
fn complete_fills_in_a_unique_match() {
    // "doc" only matches "doctor" (a workspace command).
    let completion = registry().complete("doc", CommandScope::Workspace);
    assert_eq!(completion.input, "doctor");
    assert!(completion.candidates.is_empty());
}

#[test]
fn complete_extends_to_the_common_prefix_and_lists_candidates() {
    // Register a second "s…" command so the prefix is ambiguous.
    struct Sync;
    impl Command for Sync {
        fn name(&self) -> &'static str {
            "sync"
        }
        fn description(&self) -> &'static str {
            "Sync"
        }
        fn run(&self, _args: &str, _ctx: &CommandContext) -> CommandResult {
            CommandResult::lines(Vec::new())
        }
    }
    let mut registry = registry();
    registry.register(Box::new(Sync));
    // The newcomer is fully wired (listed in `man`, dispatchable).
    assert!(registry.infos().iter().any(|i| i.name == "sync"));
    assert!(registry.dispatch("sync", &[], &[]).lines.is_empty());
    // "s" matches both "session" (workspace) and "sync" (a `Both` utility);
    // common prefix is "s". Completing in workspace scope offers both.
    let completion = registry.complete("s", CommandScope::Workspace);
    assert_eq!(completion.input, "s");
    assert_eq!(completion.candidates, vec!["session", "sync"]);
}

#[test]
fn complete_with_no_match_leaves_input_untouched() {
    let completion = registry().complete("zzz", CommandScope::Workspace);
    assert_eq!(completion.input, "zzz");
    assert!(completion.candidates.is_empty());
}

#[test]
fn complete_does_not_touch_input_with_arguments() {
    let completion = registry().complete("man ses", CommandScope::Workspace);
    assert_eq!(completion.input, "man ses");
    assert!(completion.candidates.is_empty());
}

#[test]
fn complete_does_not_offer_aliases() {
    // "h" matches "history" but not the "help" alias.
    let completion = registry().complete("h", CommandScope::Workspace);
    assert_eq!(completion.input, "history");
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

/// The command names offered for an empty input in `scope`. Completion lists
/// every in-scope command when the input is empty, so its candidates are the
/// scope's surface (avoiding an unreachable match arm on the hint enum).
fn suggested_names(scope: CommandScope) -> Vec<String> {
    registry().complete("", scope).candidates
}

#[test]
fn suggest_splits_the_command_surface_by_scope() {
    let has = |names: &[String], name: &str| names.iter().any(|n| n == name);

    // The 統括 (Overview) line offers the workspace commands and the shared
    // utilities, but never the session-specific ones.
    let workspace = suggested_names(CommandScope::Workspace);
    assert!(has(&workspace, "session"));
    assert!(has(&workspace, "config"));
    assert!(has(&workspace, "doctor"));
    assert!(has(&workspace, "man")); // a shared utility
    assert!(!has(&workspace, "terminal"));
    assert!(!has(&workspace, "agent"));
    assert!(!has(&workspace, "ai"));

    // The 在席 (Focus) prompt offers the session-specific commands and the
    // shared utilities, but never the workspace ones — the two surfaces are
    // physically separate, so they do not nest.
    let session = suggested_names(CommandScope::Session);
    assert!(has(&session, "terminal"));
    assert!(has(&session, "agent"));
    assert!(has(&session, "ai"));
    assert!(has(&session, "man")); // a shared utility
    assert!(!has(&session, "session"));
    assert!(!has(&session, "config"));
    assert!(!has(&session, "doctor"));
}

#[test]
fn suggest_filters_commands_by_prefix() {
    // "s" only matches "session" in workspace scope.
    assert_eq!(
        registry().suggest("s", CommandScope::Workspace),
        Hint::Commands(vec![CommandHint {
            name: "session",
            description: "Create, list, or switch sessions (branch + worktree)",
        }])
    );
    // The scopes are separate, so "s" matches nothing in session scope (no
    // session-specific command begins with it).
    assert_eq!(registry().suggest("s", CommandScope::Session), Hint::None);
}

#[test]
fn suggest_with_an_unknown_prefix_has_no_hint() {
    assert_eq!(
        registry().suggest("zzz", CommandScope::Workspace),
        Hint::None
    );
}

#[test]
fn suggest_shows_usage_and_examples_once_arguments_are_typed() {
    // A trailing space moves past the command word onto its arguments.
    assert_eq!(
            registry().suggest("session ", CommandScope::Workspace),
            Hint::Usage {
                usage:
                    "session [create|list|switch|remove] <name>  (aliases: create=c/new, list=ls, remove=rm)",
                examples: &[
                    "session create feature-x",
                    "session switch feature-x",
                    "session ls",
                    "session rm feature-x",
                ],
            }
        );
}

#[test]
fn suggest_with_arguments_to_an_unknown_command_has_no_hint() {
    assert_eq!(
        registry().suggest("frob bar", CommandScope::Workspace),
        Hint::None
    );
}

#[test]
fn command_scope_visibility_is_same_scope_or_both() {
    // A command is offered in its own scope only; `Both` utilities everywhere.
    assert!(CommandScope::Workspace.visible_in(CommandScope::Workspace));
    assert!(!CommandScope::Workspace.visible_in(CommandScope::Session));
    assert!(CommandScope::Session.visible_in(CommandScope::Session));
    assert!(!CommandScope::Session.visible_in(CommandScope::Workspace));
    assert!(CommandScope::Both.visible_in(CommandScope::Workspace));
    assert!(CommandScope::Both.visible_in(CommandScope::Session));
}

#[test]
fn commands_in_scope_lists_a_scopes_own_commands_in_order() {
    // The 在席 menu lists exactly the Session-scope commands, in registry
    // order, excluding the shared utilities. `terminal` comes first (and is
    // highlighted by default); the coming-soon `ai` placeholder comes last.
    let session: Vec<&str> = registry()
        .commands_in_scope(CommandScope::Session)
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(session, vec!["terminal", "agent", "ai"]);
    // Workspace scope lists its own commands and none of the session ones.
    let workspace: Vec<&str> = registry()
        .commands_in_scope(CommandScope::Workspace)
        .iter()
        .map(|i| i.name)
        .collect();
    assert!(workspace.contains(&"session"));
    assert!(workspace.contains(&"config"));
    assert!(!workspace.contains(&"terminal"));
}

#[test]
fn complete_respects_the_current_scope() {
    // "a" matches the session commands "agent" and "ai" — offered in session
    // scope, in registration order (common prefix "a")…
    let session = registry().complete("a", CommandScope::Session);
    assert_eq!(session.input, "a");
    assert_eq!(session.candidates, vec!["agent", "ai"]);
    // …but nothing in workspace scope, so the input is left untouched.
    let workspace = registry().complete("a", CommandScope::Workspace);
    assert_eq!(workspace.input, "a");
    assert!(workspace.candidates.is_empty());
}

#[test]
fn man_groups_commands_by_scope() {
    let joined = registry()
        .dispatch("man", &[], &[])
        .lines
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    // The listing is split into the two scopes plus the shared utilities.
    assert!(joined.contains("Workspace (root):"));
    assert!(joined.contains("Session (selected):"));
    assert!(joined.contains("General:"));
    // Every command still appears under one of the groups.
    for info in registry().infos() {
        assert!(joined.contains(info.name));
    }
}
