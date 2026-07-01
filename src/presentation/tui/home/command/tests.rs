use super::registry::common_prefix;
use super::*;
use crate::domain::issue::{Issue, IssuePriority, IssueStatus};
use crate::presentation::tui::home::state::LineKind;
use chrono::{TimeZone, Utc};

fn registry() -> CommandRegistry {
    CommandRegistry::with_builtins()
}

/// Build a minimal issue for the `issue` command tests.
fn issue(number: u32, title: &str, status: IssueStatus, dependson: Vec<u32>) -> Issue {
    let ts = Utc.with_ymd_and_hms(2026, 6, 14, 0, 0, 0).unwrap();
    Issue {
        number,
        title: title.to_string(),
        status,
        priority: IssuePriority::Medium,
        labels: vec![],
        dependson,
        related: vec![],
        parent: None,
        milestone: None,
        created_at: ts,
        updated_at: ts,
        body: format!("Body for {title}."),
    }
}

/// Join a result's lines into one string for substring assertions.
fn joined(result: &CommandResult) -> String {
    result
        .lines
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn empty_input_does_nothing() {
    let result = registry().dispatch("   ", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::None);
}

#[test]
fn dispatch_in_scope_refuses_a_command_outside_the_surface_scope() {
    // The workspace `:` palette ([`CommandScope::Workspace`]) refuses the
    // session-scoped `terminal` / `agent` / `close` even when typed in full: an
    // error line, no effect — not the launch effect.
    for cmd in ["terminal", "agent codex", "close"] {
        let result = registry().dispatch_in_scope(cmd, CommandScope::Workspace, &[], &[], &[]);
        assert_eq!(result.effect, Effect::None);
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].kind, LineKind::Error);
        assert!(result.lines[0].text.contains("not available here"));
    }
    // Symmetrically, the 在席 prompt ([`CommandScope::Session`]) refuses the
    // workspace-scoped `config` / `session`.
    for cmd in ["config", "session list"] {
        let result = registry().dispatch_in_scope(cmd, CommandScope::Session, &[], &[], &[]);
        assert_eq!(result.lines[0].kind, LineKind::Error);
        assert!(result.lines[0].text.contains("not available here"));
    }
}

#[test]
fn dispatch_in_scope_runs_in_scope_and_shared_commands() {
    // In-scope commands run: `agent` in Session, `config` in Workspace.
    assert!(matches!(
        registry()
            .dispatch_in_scope("agent codex", CommandScope::Session, &[], &[], &[])
            .effect,
        Effect::OpenAgent(Some(_))
    ));
    assert_eq!(
        registry()
            .dispatch_in_scope("config", CommandScope::Workspace, &[], &[], &[])
            .effect,
        Effect::OpenConfig
    );
    // [`CommandScope::Both`] utilities run in either surface.
    for scope in [CommandScope::Workspace, CommandScope::Session] {
        assert_eq!(
            registry()
                .dispatch_in_scope("clear", scope, &[], &[], &[])
                .effect,
            Effect::Clear
        );
    }
    // An unknown command falls through to the usual "unknown command" error
    // rather than the scope refusal.
    let unknown = registry().dispatch_in_scope("nope", CommandScope::Workspace, &[], &[], &[]);
    assert_eq!(unknown.lines[0].kind, LineKind::Error);
    assert!(unknown.lines[0].text.contains("unknown command"));
}

#[test]
fn man_without_argument_lists_every_command() {
    let registry = registry();
    let result = registry.dispatch("man", &[], &[]);
    // `man`'s help text opens a large scrollable modal rather than the band.
    assert_eq!(
        result.effect,
        Effect::ShowText {
            title: "Help",
            size: ModalSize::Large,
        }
    );
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
            workspace: None,
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
            workspace: None,
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
                workspace: None,
                name: "old".to_string(),
                force: true,
            }
        );
    }
}

#[test]
fn session_remove_parses_a_workspace_qualifier() {
    // In 統合(unite) mode a `workspace:session` target carries the workspace so
    // the event loop can route the removal to the owning workspace's root.
    let result = registry().dispatch("session remove app:old", &[], &[]);
    assert_eq!(
        result.effect,
        Effect::RemoveSession {
            workspace: Some("app".to_string()),
            name: "old".to_string(),
            force: false,
        }
    );

    // The qualifier composes with `--force` and the `rm` alias.
    let forced = registry().dispatch("session rm app:old --force", &[], &[]);
    assert_eq!(
        forced.effect,
        Effect::RemoveSession {
            workspace: Some("app".to_string()),
            name: "old".to_string(),
            force: true,
        }
    );

    // A lone or trailing `:` is not a qualifier — the whole token is the name.
    for (input, name) in [
        ("session remove :old", ":old"),
        ("session remove old:", "old:"),
    ] {
        let result = registry().dispatch(input, &[], &[]);
        assert_eq!(
            result.effect,
            Effect::RemoveSession {
                workspace: None,
                name: name.to_string(),
                force: false,
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
fn close_requests_the_close_session_effect() {
    // `close` is a session-scope command; it carries no arguments and asks the
    // event loop to remove the focused session (the equivalent of
    // `session remove <name>`).
    let result = registry().dispatch("close", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::CloseSession);
}

#[test]
fn coming_soon_commands_are_recognised() {
    let registry = registry();
    let result = registry.dispatch("doctor", &[], &[]);
    assert_eq!(result.effect, Effect::None);
    assert_eq!(result.lines[0].kind, LineKind::Output);
    assert!(result.lines[0].text.contains("coming soon"));
    assert!(result.lines[0].text.contains("doctor"));
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
    // No name: launch the configured agent (None payload).
    let result = registry().dispatch("agent", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::OpenAgent(None));
}

#[test]
fn ai_requests_opening_the_configured_agent_with_a_prompt() {
    let result = registry().dispatch("ai fix the failing test", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(
        result.effect,
        Effect::OpenAgentPrompt("fix the failing test".to_string())
    );
}

#[test]
fn ai_requires_a_prompt() {
    let result = registry().dispatch("ai   ", &[], &[]);
    assert_eq!(result.effect, Effect::None);
    assert_eq!(result.lines[0].kind, LineKind::Error);
    assert!(result.lines[0].text.contains("usage: ai <prompt>"));
}

#[test]
fn agent_with_a_name_overrides_which_cli_to_launch() {
    use crate::domain::settings::AgentCli;
    // A recognised name (command or display name) selects that CLI.
    let result = registry().dispatch("agent codex", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::OpenAgent(Some(AgentCli::Codex)));
    let result = registry().dispatch("agent sakana.ai", &[], &[]);
    assert_eq!(result.effect, Effect::OpenAgent(Some(AgentCli::CodexFugu)));
}

#[test]
fn agent_with_an_unknown_name_is_rejected_without_launching() {
    let result = registry().dispatch("agent emacs", &[], &[]);
    assert_eq!(result.effect, Effect::None);
    assert!(result.lines[0].text.contains("unknown agent \"emacs\""));
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
fn complete_fills_in_a_command_argument() {
    // `man [command]` completes its argument against the command names: "ses"
    // uniquely extends to "session", leaving the command word and spacing.
    let completion = registry().complete("man ses", CommandScope::Workspace);
    assert_eq!(completion.input, "man session");
    assert!(completion.candidates.is_empty());
}

#[test]
fn complete_lists_ambiguous_command_arguments() {
    // "c" matches several command names, so `man c` extends to their common
    // prefix and lists them rather than guessing.
    let completion = registry().complete("man c", CommandScope::Workspace);
    assert_eq!(completion.input, "man c");
    assert!(completion.candidates.iter().any(|c| c == "config"));
    assert!(completion.candidates.iter().any(|c| c == "clear"));
}

#[test]
fn complete_offers_session_subcommands() {
    // After the command word, `session ` offers its subcommands; a prefix
    // narrows to a unique one and fills it in.
    let all = registry().complete("session ", CommandScope::Workspace);
    assert_eq!(all.input, "session ");
    assert!(all.candidates.iter().any(|c| c == "create"));
    assert!(all.candidates.iter().any(|c| c == "switch"));

    let one = registry().complete("session cr", CommandScope::Workspace);
    assert_eq!(one.input, "session create");
    assert!(one.candidates.is_empty());
}

#[test]
fn complete_offers_the_force_flag_after_session_remove() {
    // `session remove` (and its `rm` alias) accept --force, offered once the
    // subcommand is settled. Earlier arguments and spacing are preserved.
    let completion = registry().complete("session remove feature-x --f", CommandScope::Workspace);
    assert_eq!(completion.input, "session remove feature-x --force");
    assert!(completion.candidates.is_empty());

    let aliased = registry().complete("session rm --", CommandScope::Workspace);
    assert_eq!(aliased.input, "session rm --force");
}

#[test]
fn complete_offers_session_names_for_switch_and_remove() {
    // `session switch` completes against `session_names`; `session remove`
    // against `removable_sessions`. In single-workspace mode they are the same
    // plain session names.
    let names = ["feature-x", "feature-y", "main-fix"];

    // A unique-prefix name fills in for `switch`.
    let switch = registry().complete_with(
        "session switch fea",
        CommandScope::Workspace,
        &names,
        &names,
    );
    assert!(switch
        .candidates
        .iter()
        .all(|c| c == "feature-x" || c == "feature-y"));
    assert_eq!(switch.input, "session switch feature-"); // longest common prefix

    // A fully matching prefix lands on the single name.
    let unique =
        registry().complete_with("session rm main", CommandScope::Workspace, &names, &names);
    assert_eq!(unique.input, "session rm main-fix");
    assert!(unique.candidates.is_empty());

    // `remove` offers the session names alongside --force before a name is chosen.
    let remove =
        registry().complete_with("session remove ", CommandScope::Workspace, &names, &names);
    assert!(remove.candidates.iter().any(|c| c == "feature-x"));
    assert!(remove.candidates.iter().any(|c| c == "--force"));

    // Once a name is settled, only --force remains.
    let after = registry().complete_with(
        "session remove feature-x --",
        CommandScope::Workspace,
        &names,
        &names,
    );
    assert_eq!(after.input, "session remove feature-x --force");

    // With no session data, only --force is offered (the prior behaviour): the
    // single candidate fills straight in.
    let bare = registry().complete("session remove ", CommandScope::Workspace);
    assert_eq!(bare.input, "session remove --force");
    assert!(bare.candidates.is_empty());
}

#[test]
fn complete_offers_qualified_names_for_remove_in_unite_mode() {
    // In 統合(unite) mode `session remove` completes against the qualified
    // `workspace:session` names (its own `removable_sessions` list), while
    // `session switch` keeps completing the plain `session_names`.
    let switch_names = ["feature-x", "feature-y"];
    let removable = ["app:feature-x", "app:deploy", "web:feature-x"];

    // A `workspace:` prefix narrows to that workspace's sessions.
    let remove = registry().complete_with(
        "session remove app:",
        CommandScope::Workspace,
        &switch_names,
        &removable,
    );
    assert!(remove.candidates.iter().any(|c| c == "app:feature-x"));
    assert!(remove.candidates.iter().any(|c| c == "app:deploy"));
    assert!(!remove.candidates.iter().any(|c| c == "web:feature-x"));

    // A unique qualified prefix fills straight in.
    let unique = registry().complete_with(
        "session rm web:fea",
        CommandScope::Workspace,
        &switch_names,
        &removable,
    );
    assert_eq!(unique.input, "session rm web:feature-x");
    assert!(unique.candidates.is_empty());

    // `switch` still completes plain names, ignoring the qualified list.
    let switch = registry().complete_with(
        "session switch fea",
        CommandScope::Workspace,
        &switch_names,
        &removable,
    );
    assert_eq!(switch.input, "session switch feature-");
}

#[test]
fn complete_offers_nothing_after_free_form_session_subcommands() {
    // Subcommands other than `switch`/`remove` take a free-form `<name>` with
    // nothing to complete: once the subcommand word is settled, the completer
    // offers no further candidates and leaves the input untouched.
    for input in ["session create ", "session list ", "session bogus "] {
        let completion = registry().complete(input, CommandScope::Workspace);
        assert_eq!(completion.input, input);
        assert!(completion.candidates.is_empty());
    }
}

#[test]
fn complete_offers_issue_subcommands() {
    let completion = registry().complete("issue ga", CommandScope::Workspace);
    assert_eq!(completion.input, "issue gantt");
    assert!(completion.candidates.is_empty());
}

#[test]
fn complete_offers_nothing_past_a_completable_position() {
    // Beyond the subcommand word, `session switch`/`issue show` take a free-form
    // name or number, `session create`/`list` name a new session (no `<name>`
    // vocabulary to complete), and `man`'s lone argument is done — so a further
    // token has no candidates and the input is left as typed.
    for input in [
        "session switch fea",
        "session create x",
        "session list foo",
        "issue show 3",
        "man session x",
    ] {
        let completion = registry().complete(input, CommandScope::Workspace);
        assert_eq!(completion.input, input);
        assert!(completion.candidates.is_empty());
    }
}

#[test]
fn complete_offers_nothing_for_argless_commands() {
    // Commands without argument completion (the default) leave the input alone.
    let completion = registry().complete("clear foo", CommandScope::Workspace);
    assert_eq!(completion.input, "clear foo");
    assert!(completion.candidates.is_empty());
}

#[test]
fn complete_leaves_arguments_to_an_unknown_command_untouched() {
    let completion = registry().complete("frob ba", CommandScope::Workspace);
    assert_eq!(completion.input, "frob ba");
    assert!(completion.candidates.is_empty());
}

#[test]
fn complete_respects_scope_for_arguments() {
    // `session` is a workspace command, so its arguments do not complete in the
    // session scope (the 在席 prompt). `man`, a shared utility, still does.
    let out_of_scope = registry().complete("session cr", CommandScope::Session);
    assert_eq!(out_of_scope.input, "session cr");
    assert!(out_of_scope.candidates.is_empty());

    let utility = registry().complete("man ses", CommandScope::Session);
    assert_eq!(utility.input, "man session");
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

    // The `:` command palette offers the workspace commands and the shared
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
    assert!(has(&session, "close"));
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
                    "session [create|list|switch|remove] <name>  (remove in unite: workspace:name; aliases: create=c/new, list=ls, remove=rm)",
                examples: &[
                    "session create feature-x",
                    "session switch feature-x",
                    "session ls",
                    "session rm feature-x",
                    "session rm app:feature-x  (unite: pick app's session)",
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
    // `commands_in_scope` returns exactly the Session-scope commands, in
    // registry order, excluding the shared utilities. (The 在席 menu sorts these
    // alphabetically itself before displaying them.)
    let session: Vec<&str> = registry()
        .commands_in_scope(CommandScope::Session)
        .iter()
        .map(|i| i.name)
        .collect();
    assert_eq!(session, vec!["terminal", "agent", "ai", "close"]);
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
fn issue_is_a_workspace_command() {
    let registry = registry();
    let info = registry.infos().into_iter().find(|i| i.name == "issue");
    assert_eq!(info.unwrap().scope, CommandScope::Workspace);
}

#[test]
fn issue_list_shows_readiness_and_progress_in_a_modal() {
    let issues = vec![
        issue(1, "base", IssueStatus::Done, vec![]),
        issue(2, "next", IssueStatus::Todo, vec![1]),
        issue(3, "blocked", IssueStatus::Todo, vec![2]),
    ];
    let result = registry().dispatch_with("issue", &[], &[], &issues);
    assert_eq!(
        result.effect,
        Effect::ShowText {
            title: "Issues",
            size: ModalSize::Normal,
        }
    );
    let text = joined(&result);
    assert!(text.contains("#1"));
    assert!(text.contains("done"));
    // #2's dependency (#1) is done, so it is ready; #3 is blocked by #2.
    assert!(text.contains("ready"));
    assert!(text.contains("blocked by 2"));
    // Progress footer: 1 of 3 done.
    assert!(text.contains("3 issues · 1 done (33%)"));
}

#[test]
fn issue_list_alias_and_empty_report() {
    // `ls` is an alias for the default list view.
    let issues = vec![issue(1, "only", IssueStatus::Todo, vec![])];
    assert_eq!(
        registry()
            .dispatch_with("issue ls", &[], &[], &issues)
            .effect,
        Effect::ShowText {
            title: "Issues",
            size: ModalSize::Normal,
        },
    );
    // With no issues the command logs a single line (no modal).
    let empty = registry().dispatch_with("issue list", &[], &[], &[]);
    assert_eq!(empty.effect, Effect::None);
    assert!(empty.lines[0].text.contains("No issues yet"));
}

#[test]
fn issue_graph_renders_the_dependency_tree() {
    // done root → ✓ (dim), its ready child → ○, that child's blocked child → ⊘.
    let issues = vec![
        issue(1, "root", IssueStatus::Done, vec![]),
        issue(2, "child", IssueStatus::Todo, vec![1]),
        issue(3, "grandchild", IssueStatus::Todo, vec![2]),
    ];
    let result = registry().dispatch_with("issue tree", &[], &[], &issues);
    assert_eq!(
        result.effect,
        Effect::ShowText {
            title: "Issue graph",
            size: ModalSize::Large,
        }
    );
    let text = joined(&result);
    assert!(text.contains("✓ #1 root"));
    assert!(text.contains("○ #2 child"));
    assert!(text.contains("⊘ #3 grandchild"));

    // Empty graph degrades to a single log line.
    let empty = registry().dispatch_with("issue graph", &[], &[], &[]);
    assert_eq!(empty.effect, Effect::None);
    assert!(empty.lines[0].text.contains("No issues yet"));
}

#[test]
fn issue_gantt_renders_a_dated_chart() {
    let issues = vec![
        issue(1, "root", IssueStatus::Done, vec![]),
        issue(2, "child", IssueStatus::Todo, vec![1]),
    ];
    let result = registry().dispatch_with("issue chart", &[], &[], &issues);
    assert_eq!(
        result.effect,
        Effect::ShowText {
            title: "Issue gantt",
            size: ModalSize::Large,
        }
    );
    let text = joined(&result);
    // Header span line and per-issue rows with the dependency annotation.
    assert!(text.contains("→"));
    assert!(text.contains("#1"));
    assert!(text.contains("#2"));
    assert!(text.contains("←1"));
    // Progress footer is shared with the other issue views.
    assert!(text.contains("2 issues · 1 done"));

    // Empty chart degrades to a single log line.
    let empty = registry().dispatch_with("issue gantt", &[], &[], &[]);
    assert_eq!(empty.effect, Effect::None);
    assert!(empty.lines[0].text.contains("No issues yet"));
}

#[test]
fn issue_show_renders_one_issue_or_reports_missing() {
    let issues = vec![issue(7, "visible", IssueStatus::Todo, vec![])];
    let shown = registry().dispatch_with("issue show 7", &[], &[], &issues);
    assert_eq!(
        shown.effect,
        Effect::ShowText {
            title: "Issue",
            size: ModalSize::Normal,
        }
    );
    let text = joined(&shown);
    assert!(text.contains("title: visible"));
    assert!(text.contains("Body for visible."));

    // A missing number is an error line.
    let missing = registry().dispatch_with("issue show 9", &[], &[], &issues);
    assert_eq!(missing.effect, Effect::None);
    assert!(missing.lines[0].text.contains("no issue #9"));

    // A non-numeric argument reports usage.
    let bad = registry().dispatch_with("issue show xyz", &[], &[], &issues);
    assert!(bad.lines[0].text.contains("usage: issue show"));
}

#[test]
fn issue_with_an_unknown_subcommand_reports_usage() {
    let result = registry().dispatch_with("issue frobnicate", &[], &[], &[]);
    assert!(result.lines[0].text.contains("usage: issue"));
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

#[test]
fn preview_with_a_target_requests_opening_the_markdown_pane() {
    let result = registry().dispatch("preview README", &[], &[]);
    assert!(result.lines.is_empty());
    assert_eq!(result.effect, Effect::OpenPreview("README".to_string()));
}

#[test]
fn preview_without_an_argument_reports_its_usage() {
    let result = registry().dispatch("preview", &[], &[]);
    assert_eq!(result.effect, Effect::None);
    assert_eq!(result.lines.len(), 1);
    assert_eq!(result.lines[0].kind, LineKind::Error);
    assert!(joined(&result).contains("usage: preview"));
}

#[test]
fn preview_diff_reports_that_it_is_not_built_yet() {
    let result = registry().dispatch("preview diff", &[], &[]);
    // `diff` is recognised so the surface reads coherently, but opens nothing.
    assert_eq!(result.effect, Effect::None);
    assert_eq!(result.lines[0].kind, LineKind::Output);
    assert!(joined(&result).contains("Diff preview"));
}

#[test]
fn unite_add_and_remove_emit_their_effects() {
    let add = registry().dispatch("unite add backend", &[], &[]);
    assert_eq!(add.effect, Effect::UniteAdd("backend".to_string()));
    let remove = registry().dispatch("unite remove backend", &[], &[]);
    assert_eq!(remove.effect, Effect::UniteRemove("backend".to_string()));
    // `rm` is an accepted alias for remove.
    let rm = registry().dispatch("unite rm backend", &[], &[]);
    assert_eq!(rm.effect, Effect::UniteRemove("backend".to_string()));
}

#[test]
fn unite_without_a_name_or_with_a_bad_subcommand_shows_usage() {
    for input in ["unite", "unite add", "unite wat backend"] {
        let result = registry().dispatch(input, &[], &[]);
        assert_eq!(result.effect, Effect::None);
        assert_eq!(result.lines[0].kind, LineKind::Error);
        assert!(joined(&result).contains("usage"));
    }
}

#[test]
fn unite_completes_its_subcommands_then_nothing() {
    // On the subcommand word, offer add / remove.
    let completion = registry().complete("unite ", CommandScope::Workspace);
    assert_eq!(completion.candidates, vec!["add", "remove"]);
    // Once a subcommand is chosen, the workspace name is free-form (nothing more).
    let after = registry().complete("unite add ", CommandScope::Workspace);
    assert!(after.candidates.is_empty());
}
