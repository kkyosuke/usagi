//! The Session-scope **action definition table** that drives the 集中 (Closeup)
//! Focus menu.
//!
//! The Focus menu, its inline pickers and the single-key shortcuts all speak the
//! same small vocabulary of Session-scope actions (`agent` / `chat` / `close` /
//! `diff` / `terminal`). Before this table each surface — the state filter, the
//! renderer, and the key handler — re-derived the menu-visibility rules, the
//! picker shapes and the shortcut map from scattered `match name { … }` arms.
//!
//! [`SessionActionSpec`] centralises the *behavioural* metadata that is **not**
//! already carried by the command registry's [`CommandInfo`](super::command::CommandInfo):
//! whether an action shows in the menu, whether it is allowed on the `⌂ root`
//! row, whether it is gated on the local LLM, its single-key shortcut, and which
//! inline picker (if any) it expands into. The command registry still decides
//! which commands exist, and this table provides Focus-menu metadata for the
//! commands that need action-specific behaviour.

/// The inline sub-picker a Session action expands into under its Focus-menu row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPicker {
    /// No picker: the action runs directly on `Enter`.
    None,
    /// The installed-agent CLI picker (`agent`): pick which CLI to launch.
    Agent,
    /// The `open` / `new` picker (`terminal`): embedded tab vs. native terminal.
    Terminal,
    /// The plain / `--force` picker (`close`): safe close vs. discard changes.
    Close,
}

/// The logical side effect a Focus-menu action produces when it is run directly
/// from the menu. Picker selections can refine this effect (`terminal new`,
/// `close --force`, or a chosen agent CLI), but the base action still declares
/// the effect family here so handlers do not need to rediscover it from strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActionEffect {
    /// Add an embedded terminal pane/tab.
    OpenTerminal,
    /// Open a native terminal app outside usagi.
    OpenExternalTerminal,
    /// Add an embedded agent pane/tab using the configured default CLI.
    OpenAgentDefault,
    /// Open the local-LLM chat overlay.
    OpenChat,
    /// Open the focused session's diff view.
    OpenDiff,
    /// Close the focused session; `force` mirrors `close --force`.
    CloseSession { force: bool },
}

/// A single-key Focus-menu shortcut and the command line it runs. The command
/// need not equal the action's own name — `close`'s `C` shortcut deliberately
/// runs the discard path `close --force`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionShortcut {
    /// The key the Focus menu binds (e.g. `t`, `a`, `C`).
    pub key: char,
    /// The command line the shortcut runs (e.g. `terminal`, `close --force`).
    pub command: &'static str,
}

/// The `open` / `new` choices the `terminal` action's picker offers, in display
/// (and default) order. `open` adds an embedded usagi pane/tab (the fast path);
/// `new` opens the platform's native terminal at the same directory.
pub const TERMINAL_ACTIONS: [&str; 2] = ["open", "new"];

/// One choice in the `close` action's picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseAction {
    /// The command line to run when this option is selected.
    pub command: &'static str,
    /// Whether this option discards uncommitted changes.
    pub force: bool,
    /// The row label shown in the picker.
    pub label: &'static str,
    /// The dimmed row hint shown in the picker.
    pub hint: &'static str,
}

/// The safe / force choices the `close` action's picker offers, in display (and
/// default) order. The safe option is first so `Enter` on an expanded picker does
/// not discard work unless the user moves to `close --force`.
pub const CLOSE_ACTIONS: [CloseAction; 2] = [
    CloseAction {
        command: "close",
        force: false,
        label: "close",
        hint: "(safe)",
    },
    CloseAction {
        command: "close --force",
        force: true,
        label: "close --force",
        hint: "(discard uncommitted changes)",
    },
];

/// The behavioural definition of one Session-scope action, keyed by its command
/// name. The command registry is still matched against this table before an
/// action appears, so a spec entry alone does not create a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionActionSpec {
    /// The command name this action maps to (`CommandInfo::name`).
    pub command: &'static str,
    /// The menu label for this action. It currently matches `command`, but lives
    /// here so the Focus menu can render from the action definition table.
    pub label: &'static str,
    /// The default menu description. Rows with dynamic context (`agent`,
    /// `terminal`, `close`) may decorate or replace this text at render time.
    pub description: &'static str,
    /// Whether the action shows in the Focus menu (a prompt-only action would
    /// set this `false` so it is typable but not listed).
    pub in_menu: bool,
    /// Whether the action is allowed on the `⌂ root` row (which belongs to no
    /// session). Session-only actions (`close` / `diff`) set this `false`.
    pub root_allowed: bool,
    /// Whether the action is gated on the local LLM being usable (`chat`).
    pub requires_ai: bool,
    /// The action's single-key Focus-menu shortcut, or `None` when it has none.
    pub shortcut: Option<ActionShortcut>,
    /// The inline picker the action expands into under its menu row.
    pub picker: ActionPicker,
    /// The logical effect produced by the action's default command.
    pub effect: SessionActionEffect,
}

impl SessionActionSpec {
    /// Whether this action appears in the Focus menu in the given context: it
    /// must be a menu action, its local-LLM gate must be satisfied, and — on the
    /// `⌂ root` row — it must be root-allowed. This is the single predicate the
    /// state filter applies, so the menu's visibility rules live in one place.
    pub fn visible_in_menu(&self, root_row: bool, ai_available: bool) -> bool {
        self.in_menu && (!self.requires_ai || ai_available) && (!root_row || self.root_allowed)
    }

    /// The picker sub-actions shown below this row, derived from its picker kind.
    /// Only the `terminal` picker has a static candidate list (`open` / `new`);
    /// the `agent` picker is built from the installed CLIs and the `close` picker
    /// from its two options, both at render time, so those return empty here.
    pub fn picker_actions(&self) -> &'static [&'static str] {
        match self.picker {
            ActionPicker::Terminal => &TERMINAL_ACTIONS,
            _ => &[],
        }
    }

    /// The close-picker options for this action. Non-`close` actions return an
    /// empty slice so callers can derive row counts from the spec without a
    /// separate command-name match.
    pub fn close_actions(&self) -> &'static [CloseAction] {
        match self.picker {
            ActionPicker::Close => &CLOSE_ACTIONS,
            _ => &[],
        }
    }
}

/// The Session-scope action table, in command-name order (matching the Focus
/// menu's alphabetical listing). Every Session-scope command that participates
/// in the Focus menu has an entry; a Session command *without* an entry still
/// lists (it is treated as a plain, root-safe, menu action) so the surface stays
/// discoverable ahead of a dedicated spec.
pub const SESSION_ACTIONS: &[SessionActionSpec] = &[
    SessionActionSpec {
        command: "ai",
        label: "ai",
        description: "Ask the local LLM from the prompt",
        in_menu: false,
        root_allowed: true,
        requires_ai: true,
        shortcut: None,
        picker: ActionPicker::None,
        effect: SessionActionEffect::OpenChat,
    },
    SessionActionSpec {
        command: "agent",
        label: "agent",
        description: "Open an AI agent in the selected session (terminal + agent CLI)",
        in_menu: true,
        root_allowed: true,
        requires_ai: false,
        shortcut: Some(ActionShortcut {
            key: 'a',
            command: "agent",
        }),
        picker: ActionPicker::Agent,
        effect: SessionActionEffect::OpenAgentDefault,
    },
    SessionActionSpec {
        command: "chat",
        label: "chat",
        description: "Chat with the local LLM in a dedicated screen",
        in_menu: true,
        root_allowed: true,
        requires_ai: true,
        shortcut: None,
        picker: ActionPicker::None,
        effect: SessionActionEffect::OpenChat,
    },
    SessionActionSpec {
        command: "close",
        label: "close",
        description: "Close the focused session (remove it; kept if it has uncommitted changes)",
        in_menu: true,
        root_allowed: false,
        requires_ai: false,
        shortcut: Some(ActionShortcut {
            key: 'C',
            command: "close --force",
        }),
        picker: ActionPicker::Close,
        effect: SessionActionEffect::CloseSession { force: false },
    },
    SessionActionSpec {
        command: "diff",
        label: "diff",
        description: "Show the focused session's diff against the base branch",
        in_menu: true,
        root_allowed: false,
        requires_ai: false,
        shortcut: None,
        picker: ActionPicker::None,
        effect: SessionActionEffect::OpenDiff,
    },
    SessionActionSpec {
        command: "terminal",
        label: "terminal",
        description: "Open an interactive terminal in the selected session",
        in_menu: true,
        root_allowed: true,
        requires_ai: false,
        shortcut: Some(ActionShortcut {
            key: 't',
            command: "terminal",
        }),
        picker: ActionPicker::Terminal,
        effect: SessionActionEffect::OpenTerminal,
    },
];

/// The spec for a Session-scope command by name, or `None` when the command has
/// no dedicated entry (e.g. a newly registered Session command).
pub fn spec_for(command: &str) -> Option<&'static SessionActionSpec> {
    SESSION_ACTIONS.iter().find(|spec| spec.command == command)
}

/// The picker a Session-scope command expands into, or [`ActionPicker::None`]
/// when it has no spec or no picker.
pub fn picker_for(command: &str) -> ActionPicker {
    spec_for(command).map_or(ActionPicker::None, |spec| spec.picker)
}

/// The command line a Focus-menu shortcut key runs, or `None` when the key is
/// not a shortcut. Lets the key handler map a single keystroke to a command
/// without re-listing `t` / `a` / `C` inline.
pub fn shortcut_command(key: char) -> Option<&'static str> {
    SESSION_ACTIONS.iter().find_map(|spec| {
        spec.shortcut
            .filter(|sc| sc.key == key)
            .map(|sc| sc.command)
    })
}

/// Resolve a Focus-menu command line to its logical effect. This intentionally
/// covers the base action names plus the picker/shortcut variants the menu can
/// generate; named agent CLI selection is handled before this by the agent
/// picker, and typed prompt commands still flow through the command registry.
pub fn effect_for_command_line(command: &str) -> Option<SessionActionEffect> {
    let command = command.trim();
    if let Some(spec) = spec_for(command) {
        return Some(spec.effect);
    }
    match command {
        "terminal open" => Some(SessionActionEffect::OpenTerminal),
        "terminal new" => Some(SessionActionEffect::OpenExternalTerminal),
        "close --force" | "close -f" => Some(SessionActionEffect::CloseSession { force: true }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_for_finds_known_actions_and_misses_unknown() {
        assert_eq!(spec_for("agent").map(|s| s.command), Some("agent"));
        assert_eq!(spec_for("agent").map(|s| s.label), Some("agent"));
        assert_eq!(
            spec_for("agent").map(|s| s.description),
            Some("Open an AI agent in the selected session (terminal + agent CLI)")
        );
        assert_eq!(
            spec_for("terminal").map(|s| s.picker),
            Some(ActionPicker::Terminal)
        );
        assert!(spec_for("zzz").is_none());
    }

    #[test]
    fn picker_for_maps_each_action_to_its_picker() {
        assert_eq!(picker_for("agent"), ActionPicker::Agent);
        assert_eq!(picker_for("terminal"), ActionPicker::Terminal);
        assert_eq!(picker_for("close"), ActionPicker::Close);
        assert_eq!(picker_for("diff"), ActionPicker::None);
        assert_eq!(picker_for("chat"), ActionPicker::None);
        // An unknown command has no picker.
        assert_eq!(picker_for("zzz"), ActionPicker::None);
    }

    #[test]
    fn shortcut_command_maps_keys_to_their_command_lines() {
        assert_eq!(shortcut_command('t'), Some("terminal"));
        assert_eq!(shortcut_command('a'), Some("agent"));
        // `C` deliberately runs the discard path, not bare `close`.
        assert_eq!(shortcut_command('C'), Some("close --force"));
        // A key with no shortcut binding.
        assert_eq!(shortcut_command('z'), None);
    }

    #[test]
    fn effect_for_command_line_maps_base_actions_and_picker_variants() {
        assert_eq!(
            effect_for_command_line("terminal"),
            Some(SessionActionEffect::OpenTerminal)
        );
        assert_eq!(
            effect_for_command_line("terminal open"),
            Some(SessionActionEffect::OpenTerminal)
        );
        assert_eq!(
            effect_for_command_line("terminal new"),
            Some(SessionActionEffect::OpenExternalTerminal)
        );
        assert_eq!(
            effect_for_command_line("agent"),
            Some(SessionActionEffect::OpenAgentDefault)
        );
        assert_eq!(
            effect_for_command_line("chat"),
            Some(SessionActionEffect::OpenChat)
        );
        assert_eq!(
            effect_for_command_line("diff"),
            Some(SessionActionEffect::OpenDiff)
        );
        assert_eq!(
            effect_for_command_line("close"),
            Some(SessionActionEffect::CloseSession { force: false })
        );
        assert_eq!(
            effect_for_command_line("close --force"),
            Some(SessionActionEffect::CloseSession { force: true })
        );
        assert_eq!(effect_for_command_line("zzz"), None);
    }

    #[test]
    fn picker_actions_lists_only_the_terminal_choices() {
        assert_eq!(
            spec_for("terminal").unwrap().picker_actions(),
            &["open", "new"]
        );
        // Agent / close build their candidates dynamically, so the static list is
        // empty; a pickerless action likewise has none.
        assert!(spec_for("agent").unwrap().picker_actions().is_empty());
        assert!(spec_for("close").unwrap().picker_actions().is_empty());
        assert!(spec_for("diff").unwrap().picker_actions().is_empty());
    }

    #[test]
    fn close_actions_list_safe_then_force_only_for_close() {
        let close = spec_for("close").unwrap().close_actions();
        assert_eq!(close.len(), 2);
        assert_eq!(close[0].command, "close");
        assert!(!close[0].force);
        assert_eq!(close[1].command, "close --force");
        assert!(close[1].force);
        assert!(spec_for("terminal").unwrap().close_actions().is_empty());
    }

    #[test]
    fn visible_in_menu_gates_on_menu_root_and_ai() {
        // `ai` is prompt-only: if registered, it stays typable but never lists.
        let ai = spec_for("ai").unwrap();
        assert!(!ai.visible_in_menu(false, true));

        // A plain, root-safe menu action shows everywhere.
        let agent = spec_for("agent").unwrap();
        assert!(agent.visible_in_menu(false, false));
        assert!(agent.visible_in_menu(true, false));

        // `chat` needs the local LLM; it is hidden until it is available.
        let chat = spec_for("chat").unwrap();
        assert!(!chat.visible_in_menu(false, false));
        assert!(chat.visible_in_menu(false, true));

        // `close` is session-only: shown on a session row, hidden on `⌂ root`.
        let close = spec_for("close").unwrap();
        assert!(close.visible_in_menu(false, false));
        assert!(!close.visible_in_menu(true, false));

        // A prompt-only action (in_menu = false) never lists, whatever the context.
        let prompt_only = SessionActionSpec {
            command: "hidden",
            label: "hidden",
            description: "hidden prompt-only action",
            in_menu: false,
            root_allowed: true,
            requires_ai: false,
            shortcut: None,
            picker: ActionPicker::None,
            effect: SessionActionEffect::OpenChat,
        };
        assert!(!prompt_only.visible_in_menu(false, true));
    }
}
