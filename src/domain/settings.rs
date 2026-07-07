use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentWiring;

/// UI color theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    Light,
    Dark,
    /// Follow the OS appearance.
    #[default]
    System,
}

/// The AI agent CLI usagi drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCli {
    /// Anthropic's Claude Code CLI.
    #[default]
    Claude,
    /// OpenAI's Codex CLI.
    Codex,
    /// A Codex-compatible CLI invoked as `codex-fugu` (same invocation surface as
    /// Codex — `-c` overrides, lifecycle hooks, `resume --last` — with its own
    /// rollout store under `~/.codex-fugu`).
    CodexFugu,
    /// Google's Gemini CLI.
    Gemini,
    /// Google's Antigravity CLI, invoked as `agy` (the successor to the Gemini
    /// CLI). Like Gemini it exposes no inline flag for usagi's MCP servers, hooks,
    /// or a system-prompt addendum — only plain flags (opening prompt, resume,
    /// model), so usagi wires the same feature set as Gemini.
    Antigravity,
}

/// How the **集中 (Closeup)** mode presents a session's runnable commands in the
/// right pane: as a pickable menu, or as a typed command prompt.
///
/// In the home screen's Closeup mode the right pane is the session's action
/// surface. `Menu` lists the runnable commands (`terminal` / `agent`) for the
/// user to pick; `Prompt` offers a session-scoped command line to type into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActionUi {
    /// A pickable list of the session's runnable commands (the default).
    #[default]
    Menu,
    /// A session-scoped command prompt the user types into.
    Prompt,
}

/// How the embedded terminal (**没入 / Attached**) reserves its own keys, so
/// everything else reaches the shell / agent running inside the pane.
///
/// The pane has to claim a few keys for its own navigation (switch tab, zoom
/// out, …). `Prefix` is the tmux / screen-style leader: `Ctrl-O` then a letter
/// runs the action, so `Ctrl-O` is the *only* chord the pane claims and every
/// other Ctrl key (`Ctrl-E` end-of-line, `Ctrl-N`/`Ctrl-P` history, …) flows to
/// the shell untouched. `Alt` instead binds each action to a single `Alt`-chord
/// (zellij-style): no bare Ctrl key is claimed and navigation stays one
/// keystroke, but the terminal must send `Alt`/`Option` as Meta (on macOS the
/// terminal's "Option as Meta" / "Esc+" setting). The per-scheme keymaps live in
/// the 没入 input layer (`presentation::tui::home::pane_input`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyScheme {
    /// `Ctrl-O` leader, then a key — the default. Claims only `Ctrl-O`; works on
    /// any terminal with no extra setup.
    #[default]
    Prefix,
    /// Single `Alt`-chords. Claims no bare Ctrl key and keeps navigation to one
    /// keystroke, but needs the terminal to deliver `Alt`/`Option` as Meta.
    Alt,
}

impl KeyScheme {
    /// Both schemes, in the order the config screen cycles through them.
    pub const ALL: [KeyScheme; 2] = [KeyScheme::Prefix, KeyScheme::Alt];
}

/// How the home screen's left **session sidebar** is sized: spelled out at its
/// full width, or collapsed to a compact rail.
///
/// `Full` lists each session with its name, git status, and agent state. `Rail`
/// collapses the list to a narrow vertical strip — a gutter bar marking the
/// active session, a 1-based index, and the agent-state icon — giving the right
/// pane (notably the embedded terminal) more width while still showing which
/// session is active. `Ctrl-B` toggles between them at runtime; this setting is
/// only the state the screen opens in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sidebar {
    /// The full-width session list (the default).
    #[default]
    Full,
    /// The collapsed rail: a gutter bar, index, and agent-state icon per session.
    Rail,
}

impl Sidebar {
    /// The sidebar's state after a toggle (`Ctrl-B`): full ⇄ rail.
    pub fn toggled(self) -> Self {
        match self {
            Sidebar::Full => Sidebar::Rail,
            Sidebar::Rail => Sidebar::Full,
        }
    }
}

/// The local LLM models usagi can delegate work to, in the order the config
/// screen cycles through them. All are Qwen variants pullable with
/// `ollama pull <model>`; the coder variants are tuned for code/technical
/// tasks, with smaller sizes for lower-spec machines.
pub const LOCAL_LLM_MODELS: [&str; 4] = [
    "qwen2.5-coder:7b",
    "qwen2.5-coder:3b",
    "qwen2.5-coder:1.5b",
    "qwen2.5:7b",
];

/// The model selected by default — the most capable coder variant in
/// [`LOCAL_LLM_MODELS`].
pub const DEFAULT_LOCAL_LLM_MODEL: &str = LOCAL_LLM_MODELS[0];

/// Configuration for the optional local LLM the agent can offload work to.
///
/// Disabled by default: usagi never enables it automatically. When enabled, the
/// chosen `model` is served to the agent through usagi's local LLM MCP server
/// (`usagi llm-mcp`) so the cloud agent can delegate light tasks to it and spend
/// fewer of its own tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalLlm {
    /// Whether the local LLM MCP server is wired into launched agents.
    pub enabled: bool,
    /// The Ollama model name delegated work runs against (e.g.
    /// `qwen2.5-coder:7b`).
    pub model: String,
}

impl Default for LocalLlm {
    fn default() -> Self {
        Self {
            // Opt-in: off until the user turns it on (and installs the model).
            enabled: false,
            model: DEFAULT_LOCAL_LLM_MODEL.to_string(),
        }
    }
}

impl AgentCli {
    /// Every agent CLI variant, in the canonical order menus, the config-screen
    /// selector, and the `usagi feature` table list them.
    pub const ALL: [AgentCli; 5] = [
        AgentCli::Claude,
        AgentCli::Codex,
        AgentCli::CodexFugu,
        AgentCli::Gemini,
        AgentCli::Antigravity,
    ];

    /// The shell command (program name) usagi launches for this agent — the
    /// word the `agent` command runs inside the embedded terminal.
    ///
    /// How the full launch command line is built (MCP servers, system prompt,
    /// lifecycle hooks) is the agent adapter's job, not the domain's — see the
    /// [`Agent`](crate::domain::agent::Agent) port and its
    /// `infrastructure::agent` implementations.
    pub fn command(self) -> &'static str {
        match self {
            AgentCli::Claude => "claude",
            AgentCli::Codex => "codex",
            AgentCli::CodexFugu => "codex-fugu",
            AgentCli::Gemini => "gemini",
            AgentCli::Antigravity => "agy",
        }
    }

    /// The human-facing display name shown wherever the CLI is presented to the
    /// user (the config-screen selector, the `usagi feature` table, `doctor`).
    /// Distinct from [`command`](Self::command): the codex-fugu variant launches
    /// `codex-fugu` but is presented as `sakana.ai`. The single source of truth
    /// for these labels.
    pub fn display_name(self) -> &'static str {
        match self {
            AgentCli::Claude => "Claude",
            AgentCli::Codex => "Codex",
            AgentCli::CodexFugu => "sakana.ai",
            AgentCli::Gemini => "Gemini",
            AgentCli::Antigravity => "Antigravity",
        }
    }

    /// Resolve a user-typed agent name to its variant, accepting the launch
    /// [`command`](Self::command) (`claude` / `codex` / `codex-fugu` / `gemini` / `agy`),
    /// the [`display_name`](Self::display_name) (`sakana.ai` for codex-fugu), and
    /// the on-disk serde label (`codex_fugu`) that `usagi config` prints — all
    /// case-insensitively. Used by the 集中 prompt's `agent <name>` and
    /// `clean --agent`. Returns `None` for an unrecognised name.
    ///
    /// `-` and `_` are treated as the same separator so the serde label resolves:
    /// `codex_fugu` (what `config` shows) and `codex-fugu` (the launch command)
    /// differ only there, and a user copying the displayed name would otherwise
    /// hit "unknown agent CLI".
    pub fn from_name(name: &str) -> Option<AgentCli> {
        let normalize = |s: &str| s.trim().to_ascii_lowercase().replace('_', "-");
        let name = normalize(name);
        AgentCli::ALL
            .into_iter()
            .find(|cli| normalize(cli.command()) == name || normalize(cli.display_name()) == name)
    }
}

/// Which ref a new session worktree is branched from.
///
/// When a session is created, each repository's worktree is cut on a new branch
/// off this base. The choice is project-local (see [`LocalSettings`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchSource {
    /// Branch off the repository's local default branch (e.g. `main`).
    Local,
    /// Branch off the remote-tracking default branch (e.g. `origin/main`), so
    /// sessions start from what has landed on the remote.
    #[default]
    Remote,
}

/// A toggleable group of the Claude Code skills usagi ships to its agents.
///
/// usagi embeds a set of skills in its binary and symlinks them into every
/// session worktree (see [`crate::infrastructure::skills`]). The mandatory
/// `usagi-session` skill belongs to no feature and is always linked; every other
/// shipped skill is grouped under one of these features so the user can turn the
/// whole group on or off — globally in [`Settings`] and per-project in
/// [`LocalSettings`]. Adding a new toggleable skill means adding (or reusing) a
/// variant here and tagging the skill with it in `infrastructure::skills`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillFeature {
    /// The PR-workflow skills: creating, updating, and fixing a pull request
    /// (`usagi-pr-create` / `usagi-pr-update` / `usagi-pr-fix`).
    PullRequest,
}

impl SkillFeature {
    /// Every toggleable skill feature, in the order the config screen lists them.
    pub const ALL: [SkillFeature; 1] = [SkillFeature::PullRequest];

    /// The stable on-disk identifier used as the [`Settings::skill_features`] /
    /// [`LocalSettings::skill_features`] map key. Never change an existing id: it
    /// is what a saved `settings.json` keys the toggle under.
    pub fn id(self) -> &'static str {
        match self {
            SkillFeature::PullRequest => "pull-request",
        }
    }

    /// The human-facing label shown for this feature on the config screen.
    pub fn label(self) -> &'static str {
        match self {
            SkillFeature::PullRequest => "PR Skills",
        }
    }

    /// Whether the feature is enabled when neither the global settings nor a
    /// project override say otherwise. Shipped skills are opt-out: on by default.
    pub fn default_enabled(self) -> bool {
        match self {
            SkillFeature::PullRequest => true,
        }
    }

    /// Resolve a stored [`id`](Self::id) back to its feature, or `None` for an
    /// id no current usagi knows (e.g. a feature removed since the file was
    /// written, or a hand-edited typo).
    pub fn from_id(id: &str) -> Option<SkillFeature> {
        SkillFeature::ALL.into_iter().find(|f| f.id() == id)
    }
}

/// A colour role a session label ([`SessionLabelDef`]) is painted in.
///
/// The variants are spelled as intuitive colour names (what a user hand-editing
/// `settings.json` reaches for), but they are resolved through usagi's semantic
/// [`Palette`](crate::presentation::theme::Palette) at render time — the domain
/// only records the choice, the presentation layer decides the concrete colour —
/// so the sidebar's manual-status column follows a theme retune like every other
/// coloured element. `Gray` (the default) reads as a dim, unobtrusive tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelColor {
    /// Dim / neutral — the default, an unobtrusive tag.
    #[default]
    Gray,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
}

impl LabelColor {
    /// Every colour, in the order the config screen cycles through them (the
    /// neutral default first). The single source of truth for the choice list.
    pub const ALL: [LabelColor; 7] = [
        LabelColor::Gray,
        LabelColor::Red,
        LabelColor::Green,
        LabelColor::Yellow,
        LabelColor::Blue,
        LabelColor::Magenta,
        LabelColor::Cyan,
    ];

    /// The lowercase token used for this colour in the config-screen label editor
    /// and in `settings.json` (matching the serde `snake_case` naming).
    pub fn as_str(self) -> &'static str {
        match self {
            LabelColor::Gray => "gray",
            LabelColor::Red => "red",
            LabelColor::Green => "green",
            LabelColor::Yellow => "yellow",
            LabelColor::Blue => "blue",
            LabelColor::Magenta => "magenta",
            LabelColor::Cyan => "cyan",
        }
    }

    /// Parse a colour token (case-insensitive), falling back to the neutral
    /// [`Gray`](Self::Gray) default for an empty or unrecognised value — the same
    /// degradation the serde loader applies to a stored colour.
    pub fn parse(token: &str) -> LabelColor {
        match token.trim().to_ascii_lowercase().as_str() {
            "red" => LabelColor::Red,
            "green" => LabelColor::Green,
            "yellow" => LabelColor::Yellow,
            "blue" => LabelColor::Blue,
            "magenta" => LabelColor::Magenta,
            "cyan" => LabelColor::Cyan,
            _ => LabelColor::Gray,
        }
    }
}

/// The glyph a session label falls back to when its [`SessionLabelDef::icon`] is
/// unset — a small filled bullet, one terminal column wide.
pub const DEFAULT_LABEL_ICON: char = '●';

/// One user-defined **manual session status** — a label the user assigns to a
/// session in the home screen's 選択 (Overview) mode (`Tab` cycles through them,
/// `1`–`9` jump straight to one). Distinct from the git-derived
/// [`BranchStatus`](crate::domain::workspace_state::BranchStatus) and the runtime
/// agent state: this is a human-assigned tag (todo / doing / review …), stored on
/// the session ([`SessionRecord::label_id`](crate::domain::workspace_state::SessionRecord::label_id))
/// as this def's [`id`](Self::id) and resolved back through the master for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLabelDef {
    /// Stable identifier persisted on a session and used to resolve back to this
    /// def. Never reuse an id for a different meaning: it is what a session's
    /// stored `label_id` points at. Renaming the [`name`](Self::name) is safe; the
    /// id is the identity.
    pub id: String,
    /// The human-facing text shown in the sidebar (e.g. "Review").
    pub name: String,
    /// The colour the label is painted in. An unrecognised stored value degrades
    /// to [`LabelColor::Gray`] rather than failing the whole master — see
    /// [`crate::domain::serde_fallback`].
    #[serde(
        default,
        deserialize_with = "crate::domain::serde_fallback::or_default"
    )]
    pub color: LabelColor,
    /// An optional single-glyph icon shown before the name; `None` falls back to
    /// [`DEFAULT_LABEL_ICON`]. Omitted from the file when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

impl SessionLabelDef {
    /// The icon glyph to render before the name: the first character of a set
    /// [`icon`](Self::icon), or [`DEFAULT_LABEL_ICON`] when unset/blank. Kept to a
    /// single character so the sidebar's label column stays one glyph wide.
    pub fn glyph(&self) -> char {
        self.icon
            .as_deref()
            .and_then(|s| s.trim().chars().next())
            .unwrap_or(DEFAULT_LABEL_ICON)
    }
}

/// The set of [`SessionLabelDef`]s a user can assign — the **master** the
/// config screen and a hand-edited `settings.json` define, and 選択's `Tab` /
/// digit keys cycle through.
///
/// Global by default ([`Settings::session_labels`]); a project may replace the
/// whole set with its own ([`LocalSettings::session_labels`]). Empty means the
/// feature is dormant — `Tab` becomes a no-op — so [`Default`] ships a small,
/// generic kanban-style set to make it usable with zero configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionLabelMaster {
    /// The labels, in the order `Tab` cycles and the digit keys index (`1` is the
    /// first). Resolution and cycling go through the accessors, never this field
    /// directly.
    pub labels: Vec<SessionLabelDef>,
}

impl Default for SessionLabelMaster {
    fn default() -> Self {
        // A generic kanban-style set so the feature works out of the box; a user
        // overrides it wholesale in settings.json.
        let of = |id: &str, name: &str, color: LabelColor, icon: char| SessionLabelDef {
            id: id.to_string(),
            name: name.to_string(),
            color,
            icon: Some(icon.to_string()),
        };
        Self {
            labels: vec![
                of("todo", "Todo", LabelColor::Gray, '○'),
                of("doing", "Doing", LabelColor::Blue, '▸'),
                of("review", "Review", LabelColor::Magenta, '◇'),
                of("blocked", "Blocked", LabelColor::Red, '✕'),
                of("done", "Done", LabelColor::Green, '✓'),
            ],
        }
    }
}

impl SessionLabelMaster {
    /// The labels, in cycle / index order.
    pub fn labels(&self) -> &[SessionLabelDef] {
        &self.labels
    }

    /// Whether no label is defined — the manual-status feature is then dormant
    /// (`Tab` is a no-op and no column is drawn).
    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }

    /// The number of defined labels — the range the digit keys (`1`..) address.
    pub fn len(&self) -> usize {
        self.labels.len()
    }

    /// The label with the given `id`, or `None` when no label matches (an id from
    /// a since-removed def, so a session pointing at it reads as unset).
    pub fn get(&self, id: &str) -> Option<&SessionLabelDef> {
        self.labels.iter().find(|l| l.id == id)
    }

    /// The 0-based position of the label with `id`, or `None` when absent. Used to
    /// resume cycling from a session's current label.
    pub fn position(&self, id: &str) -> Option<usize> {
        self.labels.iter().position(|l| l.id == id)
    }

    /// The label at 0-based `index` (what digit key `index + 1` selects), or
    /// `None` when out of range.
    pub fn at(&self, index: usize) -> Option<&SessionLabelDef> {
        self.labels.get(index)
    }

    /// Coerce a loaded master into a trusted state: drop labels with a blank id or
    /// name, and keep only the first def for each id so a hand-edited duplicate id
    /// cannot make a session's `label_id` ambiguous. Order is otherwise preserved.
    pub fn sanitized(mut self) -> Self {
        let mut seen = std::collections::BTreeSet::new();
        self.labels.retain(|l| {
            !l.id.trim().is_empty() && !l.name.trim().is_empty() && seen.insert(l.id.clone())
        });
        self
    }
}

/// How many lines of scrolled-off output each embedded terminal pane keeps by
/// default, so the user can scroll a pane back over earlier output.
///
/// Every live pane holds its own scrollback buffer, so this cap is paid once per
/// pane: with many sessions and panes open at once it is the dominant slice of
/// the TUI's memory. The pool keeps every pane of every session alive in the
/// background, and each pane's parser holds up to this many scrollback rows of
/// `cols` cells, so worst-case resident grid memory is on the order of
/// `panes × terminal_scrollback_lines × cols × sizeof(cell)` — bounded (no
/// leak), but it scales with the number of open panes. The default is
/// deliberately modest — enough to scroll back over a command's recent output
/// without each pane reserving a large buffer — and the user can raise it (up to
/// [`MAX_TERMINAL_SCROLLBACK_LINES`]) when they want deeper history.
pub const DEFAULT_TERMINAL_SCROLLBACK_LINES: usize = 2_000;

/// The largest [`Settings::terminal_scrollback_lines`] value honoured. A
/// hand-edited `settings.json` could otherwise ask every pane to retain an
/// unbounded buffer; [`Settings::sanitized`] clamps to this so one setting
/// cannot blow up memory across every open pane.
pub const MAX_TERMINAL_SCROLLBACK_LINES: usize = 50_000;

/// The default used for a missing `terminal_scrollback_lines` field, so an older
/// `settings.json` (written before the field existed) loads with the modest
/// default rather than `0` — `#[serde(default)]` on the struct would otherwise
/// fall back to `usize`'s `0`, leaving panes with no scrollback at all.
fn default_terminal_scrollback_lines() -> usize {
    DEFAULT_TERMINAL_SCROLLBACK_LINES
}

/// A map of environment variables whose values are resolved from 1Password
/// references (`op://vault/item/field`) before launching an embedded agent or
/// terminal.
///
/// The map key is the environment variable name (e.g. `GH_TOKEN`), and the map
/// value is the 1Password secret reference to read. Values are intentionally
/// references, not secrets: the actual secret is fetched at process-launch time
/// and injected only into the child process environment.
pub type SecretEnv = BTreeMap<String, String>;

/// User-configurable application settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Enum fields degrade an unrecognised stored value to their default rather
    // than failing the whole file (see [`crate::domain::serde_fallback`]), so a
    // newer usagi's value — or a hand-edited typo — never blocks loading every
    // other setting.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub theme: Theme,
    /// Name of the workspace to open by default, if any.
    pub default_workspace: Option<String>,
    /// Base directory new projects are cloned under, if configured.
    ///
    /// When unset the New Project screen falls back to `~/git`.
    pub workspace_root: Option<PathBuf>,
    /// Whether desktop notifications are shown (e.g. on `hop`).
    pub notifications_enabled: bool,
    /// Whether the home screen restores each session's open panes (agent /
    /// terminal) on startup, re-spawning them in the background — an agent picks
    /// its conversation back up where it left off. On unless the user disables it.
    pub restore_panes_enabled: bool,
    /// Whether the home screen automatically launches a session's agent pane in
    /// the background as soon as it detects a prompt queued for that session (via
    /// MCP `session_prompt` / `session_delegate_issue`), handing the queued prompt
    /// to the agent as its first message — so a delegated issue starts being
    /// worked without a human opening the pane. The pane is spawned but not
    /// attached; its lifecycle hooks still move the sidebar badge. On unless the
    /// user disables it; when off, a queued prompt waits to be consumed by the
    /// next fresh launch of that session's agent pane as before.
    pub autostart_queued_prompts: bool,
    /// Which agent CLI usagi drives.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub agent_cli: AgentCli,
    /// How the home screen's 集中 (Closeup) mode presents a session's runnable
    /// commands in the right pane.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub session_action_ui: SessionActionUi,
    /// How the embedded terminal (没入) reserves its navigation keys — a `Ctrl-O`
    /// prefix or single `Alt`-chords — so the rest reach the shell / agent.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub key_scheme: KeyScheme,
    /// Which state the home screen's left session sidebar opens in (`Ctrl-B`
    /// toggles it at runtime).
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub sidebar: Sidebar,
    /// Whether the sidebar mascot reacts to interaction — a quick blink in 選択 /
    /// 集中 and the 没入 working rabbit's idle paw motion. Purely cosmetic and
    /// driven by paints that already happen (no idle timer), so turning it off
    /// just keeps the mascot perfectly still. On unless the user disables it.
    pub mascot_animation_enabled: bool,
    /// How many lines of scrolled-off output each embedded terminal pane keeps.
    /// Paid once per live pane, so it is the main lever on the TUI's memory when
    /// many sessions are open; [`sanitized`](Self::sanitized) clamps it to
    /// [`MAX_TERMINAL_SCROLLBACK_LINES`].
    #[serde(default = "default_terminal_scrollback_lines")]
    pub terminal_scrollback_lines: usize,
    /// The optional local LLM the agent can offload light work to.
    pub local_llm: LocalLlm,
    /// Which of usagi's optional shipped-skill features are enabled, keyed by
    /// [`SkillFeature::id`]. A feature absent from the map uses its
    /// [`default_enabled`](SkillFeature::default_enabled), so the map only ever
    /// records a value that differs from the default; query it through
    /// [`skill_feature_enabled`](Self::skill_feature_enabled) rather than reading
    /// the map directly. Unknown keys (a feature this usagi does not know) are
    /// dropped by [`sanitized`](Self::sanitized).
    #[serde(default)]
    pub skill_features: BTreeMap<String, bool>,
    /// Application-wide secret environment variables resolved from 1Password
    /// references when launching an embedded agent or terminal, keyed by the
    /// environment variable name (for example `GH_TOKEN`) with an `op://...`
    /// reference as the value. These apply to every workspace; a project can add
    /// to or override them through [`LocalSettings::env`] (a workspace binding
    /// for the same name wins). Only the reference is stored — never a resolved
    /// secret. Read the valid bindings through [`env`](Self::env).
    #[serde(default)]
    pub env: SecretEnv,
    /// The user-defined manual session-status labels 選択 (Overview) assigns with
    /// `Tab` / the digit keys, resolved onto the sidebar's status column. Ships a
    /// generic set by default (see [`SessionLabelMaster`]); an empty set leaves
    /// the feature dormant. A project may replace it wholesale
    /// ([`LocalSettings::session_labels`]).
    #[serde(default)]
    pub session_labels: SessionLabelMaster,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            default_workspace: None,
            workspace_root: None,
            // Notifications are opt-out: on unless the user disables them.
            notifications_enabled: true,
            // Restoring open panes is opt-out: on unless the user disables it.
            restore_panes_enabled: true,
            // Auto-starting queued prompts is opt-out: on unless disabled, so a
            // delegated issue begins work without a human opening the pane.
            autostart_queued_prompts: true,
            agent_cli: AgentCli::default(),
            session_action_ui: SessionActionUi::default(),
            key_scheme: KeyScheme::default(),
            sidebar: Sidebar::default(),
            // The mascot's reactions are opt-out: on unless the user disables them.
            mascot_animation_enabled: true,
            terminal_scrollback_lines: DEFAULT_TERMINAL_SCROLLBACK_LINES,
            local_llm: LocalLlm::default(),
            // No overrides recorded: every shipped-skill feature uses its own
            // default (see [`SkillFeature::default_enabled`]).
            skill_features: BTreeMap::new(),
            // No application-wide secret env is injected unless explicitly
            // configured.
            env: SecretEnv::new(),
            session_labels: SessionLabelMaster::default(),
        }
    }
}

impl Settings {
    /// Coerce loaded settings into a trusted state, dropping values that did not
    /// come from usagi's own UI.
    ///
    /// The config screen only ever stores a `local_llm.model` from the fixed
    /// [`LOCAL_LLM_MODELS`] allowlist, but `settings.json` is a plain file a user
    /// (or a synced dotfiles repo) can hand-edit. The model name is later
    /// interpolated into the agent launch command, so an unexpected value — for
    /// instance one containing a shell single quote — must not be trusted
    /// verbatim. Any model outside the allowlist is reset to
    /// [`DEFAULT_LOCAL_LLM_MODEL`]. This is defense-in-depth: the launch builder
    /// also escapes its arguments, so neither layer alone is load-bearing.
    pub fn sanitized(mut self) -> Self {
        if !LOCAL_LLM_MODELS.contains(&self.local_llm.model.as_str()) {
            self.local_llm.model = DEFAULT_LOCAL_LLM_MODEL.to_string();
        }
        // A hand-edited `settings.json` could ask every pane to keep an enormous
        // (or effectively unbounded) scrollback buffer; cap it so one setting
        // cannot exhaust memory across every open pane.
        self.terminal_scrollback_lines = self
            .terminal_scrollback_lines
            .min(MAX_TERMINAL_SCROLLBACK_LINES);
        // Drop skill-feature keys this usagi does not recognise (a feature
        // removed since the file was written, or a hand-edited typo) so a stale
        // entry never lingers in the saved file or the `usagi config` output.
        self.skill_features
            .retain(|id, _| SkillFeature::from_id(id).is_some());
        // Drop blank / duplicate-id labels a hand-edited file may carry, so a
        // session's stored `label_id` never resolves ambiguously.
        self.session_labels = std::mem::take(&mut self.session_labels).sanitized();
        self
    }

    /// Whether the shipped-skill `feature` is enabled, resolving an absent entry
    /// to the feature's [`default_enabled`](SkillFeature::default_enabled). This
    /// is the single read path for a feature's enablement — callers never index
    /// [`skill_features`](Self::skill_features) directly.
    pub fn skill_feature_enabled(&self, feature: SkillFeature) -> bool {
        self.skill_features
            .get(feature.id())
            .copied()
            .unwrap_or_else(|| feature.default_enabled())
    }

    /// The non-empty, valid application-wide environment bindings to resolve
    /// and inject when a process is launched. Blank names/references are
    /// ignored, as are names that are not portable environment identifiers; the
    /// remaining pairs keep the BTreeMap's stable sorted order.
    pub fn env(&self) -> impl Iterator<Item = (&str, &str)> {
        valid_env_bindings(&self.env)
    }
}

impl Settings {
    /// Apply a project's [`LocalSettings`] over these global settings, returning
    /// the effective settings for that project.
    ///
    /// Each local field overrides its global counterpart only when set; an unset
    /// (`None`) local field leaves the global value untouched.
    pub fn with_local(mut self, local: &LocalSettings) -> Self {
        if let Some(agent_cli) = local.agent_cli {
            self.agent_cli = agent_cli;
        }
        if let Some(notifications_enabled) = local.notifications_enabled {
            self.notifications_enabled = notifications_enabled;
        }
        if let Some(restore_panes_enabled) = local.restore_panes_enabled {
            self.restore_panes_enabled = restore_panes_enabled;
        }
        if let Some(autostart_queued_prompts) = local.autostart_queued_prompts {
            self.autostart_queued_prompts = autostart_queued_prompts;
        }
        if let Some(local_llm_enabled) = local.local_llm_enabled {
            self.local_llm.enabled = local_llm_enabled;
        }
        // Each project-local skill-feature override replaces the global entry for
        // that feature; features the project does not mention keep the global
        // value. The merged map is still read through `skill_feature_enabled`.
        for (id, enabled) in &local.skill_features {
            self.skill_features.insert(id.clone(), *enabled);
        }
        // Workspace env augments the global map, overriding same-named
        // application-wide bindings so a project can pin a token/ref that is
        // specific to that repository while inheriting the rest.
        for (name, reference) in local.env() {
            self.env.insert(name.to_string(), reference.to_string());
        }
        // A project may define its own manual-status label set, replacing the
        // global one wholesale (an empty set turns the feature off for the
        // project). Sanitized on the way in, like the global set is.
        if let Some(labels) = &local.session_labels {
            self.session_labels = labels.clone().sanitized();
        }
        self
    }

    /// usagi's wiring policy for a launched agent: the resolved usagi binary path
    /// the agent invokes back through (MCP servers and lifecycle hooks) and the
    /// local-LLM model to offload light work to, when [`LocalLlm::enabled`] is set.
    /// An [`Agent`](crate::domain::agent::Agent) adapter renders this into its
    /// CLI's own invocation.
    ///
    /// `usagi_bin` is the absolute path of the running binary
    /// (`std::env::current_exe()`), so the wiring resolves even when usagi is run
    /// from a build and not installed on `$PATH`. How an adapter renders this is
    /// the [`Agent`](crate::domain::agent::Agent) port's
    /// [`launch_command`](crate::domain::agent::Agent::launch_command).
    pub fn agent_wiring(&self, usagi_bin: &str) -> AgentWiring {
        AgentWiring {
            usagi_bin: usagi_bin.to_string(),
            local_llm_model: self.local_llm.enabled.then(|| self.local_llm.model.clone()),
            // No agent-model source yet: launch each CLI on its configured
            // default. The adapters render `wiring.model` when it is `Some`, so
            // wiring a model in later (a setting or a launch-time argument) is a
            // change here alone.
            model: None,
            is_root: true,
            sandbox_writable_roots: Vec::new(),
        }
    }
}

/// Per-project overrides of selected [`Settings`], stored alongside a
/// repository in `<repo>/.usagi/settings.json`.
///
/// Every field is optional: `None` means "defer to the global setting". Only
/// the settings that make sense to vary per project are represented here.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalSettings {
    /// Override which agent CLI usagi drives for this project. An unrecognised
    /// stored value degrades to `None` (defer to the global setting) rather than
    /// failing the whole file — see [`crate::domain::serde_fallback`].
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub agent_cli: Option<AgentCli>,
    /// Override whether desktop notifications are shown for this project.
    pub notifications_enabled: Option<bool>,
    /// Override whether this project's session panes are restored on startup.
    /// `None` defers to the global setting.
    pub restore_panes_enabled: Option<bool>,
    /// Override whether queued prompts are auto-started for this project's
    /// sessions. `None` defers to the global setting.
    pub autostart_queued_prompts: Option<bool>,
    /// Which ref new session worktrees branch from in this repository. `None`
    /// defers to the default ([`BranchSource::Remote`]). An unrecognised stored
    /// value degrades to `None`.
    #[serde(deserialize_with = "crate::domain::serde_fallback::or_default")]
    pub default_branch_source: Option<BranchSource>,
    /// The branch new session worktrees are cut from in this repository. `None`
    /// means "use the repository's detected default branch" (e.g. `main`); a
    /// value names a specific branch (e.g. `develop`) to branch off instead. The
    /// [`default_branch_source`](Self::default_branch_source) still decides
    /// whether the local or remote-tracking form of that branch is used.
    pub default_branch: Option<String>,
    /// Override whether the local LLM is enabled for this project. `None` defers
    /// to the global [`LocalLlm::enabled`] setting.
    pub local_llm_enabled: Option<bool>,
    /// Per-project overrides of shipped-skill features, keyed by
    /// [`SkillFeature::id`]. A feature present here overrides the global setting
    /// for this project; a feature absent here defers to the global value. Read
    /// the override through [`skill_feature_override`](Self::skill_feature_override).
    #[serde(default)]
    pub skill_features: BTreeMap<String, bool>,
    /// Workspace-scoped environment variables resolved from 1Password references
    /// when launching an embedded agent or terminal. The key is the environment
    /// variable name (for example `GH_TOKEN`) and the value is an `op://...`
    /// reference. The resolved secret is never persisted in settings; it is read
    /// just-in-time via `op read --no-newline` and injected into the child
    /// process environment.
    #[serde(default)]
    pub env: SecretEnv,
    /// Shell commands run, in order, in each freshly built session worktree
    /// right after a session is created in this workspace — a per-project setup
    /// hook (e.g. `npm install`, `cp .env.example .env`). Empty by default (no
    /// commands run). Each entry is one command line, executed through the
    /// platform shell with the worktree as the working directory; a command's
    /// failure is logged but never aborts the already-built session. Read the
    /// list through [`setup_commands`](Self::setup_commands).
    #[serde(default)]
    pub setup_commands: Vec<String>,
    /// Replace the global manual-status label master
    /// ([`Settings::session_labels`]) for this project. `None` (the default,
    /// omitted from the file) defers to the global set; `Some` — including an empty
    /// set — overrides it wholesale (an empty set turns the feature off here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_labels: Option<SessionLabelMaster>,
}

impl LocalSettings {
    /// Whether every field is unset, i.e. the project adds no local override.
    pub fn is_empty(&self) -> bool {
        self.agent_cli.is_none()
            && self.notifications_enabled.is_none()
            && self.restore_panes_enabled.is_none()
            && self.autostart_queued_prompts.is_none()
            && self.default_branch_source.is_none()
            && self.default_branch.is_none()
            && self.local_llm_enabled.is_none()
            && self.skill_features.is_empty()
            && self.env.is_empty()
            && self.setup_commands.is_empty()
            && self.session_labels.is_none()
    }

    /// This project's override for the shipped-skill `feature`: `Some(enabled)`
    /// when the project pins it on or off, or `None` to defer to the global
    /// setting. The single read path for a local override — callers never index
    /// [`skill_features`](Self::skill_features) directly.
    pub fn skill_feature_override(&self, feature: SkillFeature) -> Option<bool> {
        self.skill_features.get(feature.id()).copied()
    }

    /// The branch source to use, resolving an unset value to the default.
    pub fn branch_source(&self) -> BranchSource {
        self.default_branch_source.unwrap_or_default()
    }

    /// The specific branch new sessions should branch from, or `None` to use the
    /// repository's detected default branch.
    pub fn default_branch(&self) -> Option<&str> {
        self.default_branch.as_deref()
    }

    /// The non-empty, valid environment bindings to resolve and inject when a
    /// process is launched inside this workspace. Blank names/references are
    /// ignored, as are names that are not portable environment identifiers; the
    /// remaining pairs keep the BTreeMap's stable sorted order.
    pub fn env(&self) -> impl Iterator<Item = (&str, &str)> {
        valid_env_bindings(&self.env)
    }

    /// The non-empty setup command lines to run for newly-created sessions in
    /// this workspace, in persisted order. Blank lines are ignored so the
    /// multi-line config editor can leave visual spacing without launching an
    /// empty shell command.
    pub fn setup_commands(&self) -> impl Iterator<Item = &str> {
        self.setup_commands
            .iter()
            .map(String::as_str)
            .filter(|command| !command.trim().is_empty())
    }
}

fn valid_env_bindings(env: &SecretEnv) -> impl Iterator<Item = (&str, &str)> {
    env.iter().filter_map(|(name, reference)| {
        let name = name.trim();
        let reference = reference.trim();
        if is_valid_env_name(name) && !reference.is_empty() {
            Some((name, reference))
        } else {
            None
        }
    })
}

/// Whether `name` is a portable environment variable name. Keep this strict so
/// a hand-edited settings file cannot smuggle shell syntax into diagnostics or
/// platform-specific odd names into child environments.
pub fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Parse a `NAME=op://vault/item/field` editor buffer (one binding per line)
/// into the valid bindings, keyed by name so a later line with the same name
/// wins and the map keeps its sorted order. Lines without a `=`, with a name that
/// is not a portable identifier, or with a blank reference are dropped — the same
/// filter [`LocalSettings::env`] applies at read time, so what is saved is exactly
/// what will be injected. Shared by the config-screen and command-palette editors.
pub fn parse_env_bindings(text: &str) -> SecretEnv {
    let mut env = SecretEnv::new();
    for line in text.lines() {
        let Some((name, reference)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let reference = reference.trim();
        if is_valid_env_name(name) && !reference.is_empty() {
            env.insert(name.to_string(), reference.to_string());
        }
    }
    env
}

/// Render `env` back into the editor buffer form ([`parse_env_bindings`]'s
/// inverse): one `NAME=reference` line per binding, in the map's sorted order.
pub fn format_env_bindings(env: &SecretEnv) -> String {
    env.iter()
        .map(|(name, reference)| format!("{name}={reference}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The field separator in the session-label editor buffer: one label per line as
/// `id | name | color | icon`. Parsing trims each field, so the exact spacing
/// around the bar is not significant; a label's `name` therefore cannot itself
/// contain a `|`.
const LABEL_FIELD_SEP: char = '|';

/// Parse a session-label editor buffer — one `id | name | color | icon` line per
/// label — into a [`SessionLabelMaster`]. The `color` field is optional (an
/// empty or unrecognised token degrades to [`LabelColor::Gray`]) and so is `icon`
/// (blank leaves the default bullet). Blank lines are skipped, and the result is
/// [`sanitized`](SessionLabelMaster::sanitized) so a line with a blank id or name
/// is dropped and a duplicate id keeps only its first definition — exactly the
/// coercion a hand-edited `settings.json` receives at load. Shared by the config
/// screen's label editor.
pub fn parse_session_labels(text: &str) -> SessionLabelMaster {
    let labels = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split(LABEL_FIELD_SEP);
            let id = fields.next().unwrap_or_default().trim().to_string();
            let name = fields.next().unwrap_or_default().trim().to_string();
            let color = fields.next().map(LabelColor::parse).unwrap_or_default();
            let icon = fields
                .next()
                .map(str::trim)
                .filter(|glyph| !glyph.is_empty())
                .map(str::to_string);
            SessionLabelDef {
                id,
                name,
                color,
                icon,
            }
        })
        .collect();
    SessionLabelMaster { labels }.sanitized()
}

/// Render a [`SessionLabelMaster`] back into the editor buffer form
/// ([`parse_session_labels`]'s inverse): one `id | name | color | icon` line per
/// label, in master order. The trailing `icon` field is omitted when unset, so
/// the line ends at the colour.
pub fn format_session_labels(master: &SessionLabelMaster) -> String {
    master
        .labels()
        .iter()
        .map(|label| {
            let head = format!("{} | {} | {}", label.id, label.name, label.color.as_str());
            match label.icon.as_deref().map(str::trim) {
                Some(icon) if !icon.is_empty() => format!("{head} | {icon}"),
                _ => head,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_local_overrides_only_the_set_fields() {
        let global = Settings::default(); // agent_cli = Claude, notifications = true
        let local = LocalSettings {
            agent_cli: Some(AgentCli::Gemini),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        // The set field is overridden...
        assert_eq!(effective.agent_cli, AgentCli::Gemini);
        // ...while the unset field keeps the global value.
        assert!(effective.notifications_enabled);
    }

    #[test]
    fn settings_env_filters_blank_and_invalid_bindings_but_keeps_valid_refs() {
        let settings = Settings {
            env: [
                (
                    "GLOBAL_TOKEN".to_string(),
                    "op://Private/Global/token".to_string(),
                ),
                ("1BAD".to_string(), "op://Private/Bad/token".to_string()),
                ("EMPTY".to_string(), "   ".to_string()),
                ("_OK".to_string(), "op://Private/Ok/token".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        assert_eq!(
            settings.env().collect::<Vec<_>>(),
            vec![
                ("GLOBAL_TOKEN", "op://Private/Global/token"),
                ("_OK", "op://Private/Ok/token"),
            ]
        );
    }

    #[test]
    fn with_local_merges_env_with_workspace_winning_same_name() {
        let global = Settings {
            env: [
                ("A_TOKEN".to_string(), "op://global/a".to_string()),
                ("SHARED".to_string(), "op://global/shared".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let local = LocalSettings {
            env: [
                ("B_TOKEN".to_string(), "op://local/b".to_string()),
                ("SHARED".to_string(), "op://local/shared".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        assert_eq!(
            effective.env().collect::<Vec<_>>(),
            vec![
                ("A_TOKEN", "op://global/a"),
                ("B_TOKEN", "op://local/b"),
                ("SHARED", "op://local/shared"),
            ]
        );
    }

    #[test]
    fn with_local_overrides_notifications_when_set() {
        let global = Settings::default();
        let local = LocalSettings {
            notifications_enabled: Some(false),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        assert_eq!(effective.agent_cli, AgentCli::Claude);
        assert!(!effective.notifications_enabled);
    }

    #[test]
    fn with_local_overrides_the_local_llm_toggle_when_set() {
        // The global default leaves the local LLM off; a local override turns it
        // on for just this project (the model is untouched).
        let global = Settings::default();
        assert!(!global.local_llm.enabled);
        let local = LocalSettings {
            local_llm_enabled: Some(true),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        assert!(effective.local_llm.enabled);
        assert_eq!(effective.local_llm.model, DEFAULT_LOCAL_LLM_MODEL);
    }

    #[test]
    fn with_local_overrides_restore_panes_when_set() {
        // The global default restores panes; a local override turns it off for
        // just this project.
        let global = Settings::default();
        assert!(global.restore_panes_enabled);
        let local = LocalSettings {
            restore_panes_enabled: Some(false),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        assert!(!effective.restore_panes_enabled);
        // Unrelated fields keep their global value.
        assert!(effective.notifications_enabled);
    }

    #[test]
    fn autostart_queued_prompts_defaults_on() {
        // Auto-starting queued prompts is opt-out, so a fresh install has it on.
        assert!(Settings::default().autostart_queued_prompts);
    }

    #[test]
    fn with_local_overrides_autostart_queued_prompts_when_set() {
        // The global default auto-starts queued prompts; a local override turns it
        // off for just this project.
        let global = Settings::default();
        assert!(global.autostart_queued_prompts);
        let local = LocalSettings {
            autostart_queued_prompts: Some(false),
            ..Default::default()
        };

        let effective = global.with_local(&local);

        assert!(!effective.autostart_queued_prompts);
        // Unrelated fields keep their global value.
        assert!(effective.restore_panes_enabled);
    }

    #[test]
    fn with_local_is_a_no_op_when_empty() {
        let global = Settings::default();
        assert_eq!(global.clone().with_local(&LocalSettings::default()), global);
    }

    #[test]
    fn is_empty_reflects_whether_any_field_is_set() {
        assert!(LocalSettings::default().is_empty());
        assert!(!LocalSettings {
            agent_cli: Some(AgentCli::Claude),
            ..Default::default()
        }
        .is_empty());
        assert!(!LocalSettings {
            notifications_enabled: Some(true),
            ..Default::default()
        }
        .is_empty());
        // The branch source counts as an override too.
        assert!(!LocalSettings {
            default_branch_source: Some(BranchSource::Local),
            ..Default::default()
        }
        .is_empty());
        // ...as does a specific default branch.
        assert!(!LocalSettings {
            default_branch: Some("develop".to_string()),
            ..Default::default()
        }
        .is_empty());
        // As does the restore-panes toggle.
        assert!(!LocalSettings {
            restore_panes_enabled: Some(false),
            ..Default::default()
        }
        .is_empty());
        // As does the autostart-queued-prompts toggle.
        assert!(!LocalSettings {
            autostart_queued_prompts: Some(false),
            ..Default::default()
        }
        .is_empty());
        // So does the local LLM toggle.
        assert!(!LocalSettings {
            local_llm_enabled: Some(false),
            ..Default::default()
        }
        .is_empty());
        // Workspace env references are project-local state too.
        assert!(!LocalSettings {
            env: [(
                "GH_TOKEN".to_string(),
                "op://Private/GitHub/token".to_string()
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        }
        .is_empty());
        // Setup commands are project-local state too.
        assert!(!LocalSettings {
            setup_commands: vec!["npm install".to_string()],
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn env_filters_blank_and_invalid_bindings_but_keeps_valid_refs() {
        let local = LocalSettings {
            env: [
                (
                    "GH_TOKEN".to_string(),
                    "op://Private/GitHub/token".to_string(),
                ),
                ("1BAD".to_string(), "op://Private/Bad/token".to_string()),
                ("EMPTY".to_string(), "   ".to_string()),
                ("_OK".to_string(), "op://Private/Ok/token".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        assert_eq!(
            local.env().collect::<Vec<_>>(),
            vec![
                ("GH_TOKEN", "op://Private/GitHub/token"),
                ("_OK", "op://Private/Ok/token"),
            ]
        );
        assert!(is_valid_env_name("GH_TOKEN"));
        assert!(is_valid_env_name("_TOKEN_1"));
        assert!(!is_valid_env_name("1TOKEN"));
        assert!(!is_valid_env_name("BAD-NAME"));
    }

    #[test]
    fn label_color_parse_is_case_insensitive_and_falls_back_to_gray() {
        assert_eq!(LabelColor::parse("Red"), LabelColor::Red);
        assert_eq!(LabelColor::parse("  MAGENTA "), LabelColor::Magenta);
        assert_eq!(LabelColor::parse("cyan"), LabelColor::Cyan);
        // Unknown / blank tokens degrade to the neutral default.
        assert_eq!(LabelColor::parse("puce"), LabelColor::Gray);
        assert_eq!(LabelColor::parse(""), LabelColor::Gray);
    }

    #[test]
    fn label_color_as_str_round_trips_through_parse_for_every_variant() {
        for color in LabelColor::ALL {
            assert_eq!(LabelColor::parse(color.as_str()), color);
        }
    }

    #[test]
    fn parse_session_labels_reads_fields_and_defaults_optional_ones() {
        let master = parse_session_labels(
            "todo | To Do | gray | ○\n\
             doing | In Progress | blue\n\
             plain | Plain",
        );
        let labels = master.labels();
        assert_eq!(labels.len(), 3);

        assert_eq!(labels[0].id, "todo");
        assert_eq!(labels[0].name, "To Do");
        assert_eq!(labels[0].color, LabelColor::Gray);
        assert_eq!(labels[0].icon.as_deref(), Some("○"));

        // No icon field: the icon stays unset (falls back to the default bullet).
        assert_eq!(labels[1].id, "doing");
        assert_eq!(labels[1].color, LabelColor::Blue);
        assert_eq!(labels[1].icon, None);

        // Neither colour nor icon: colour degrades to gray, icon unset.
        assert_eq!(labels[2].id, "plain");
        assert_eq!(labels[2].color, LabelColor::Gray);
        assert_eq!(labels[2].icon, None);
    }

    #[test]
    fn parse_session_labels_skips_blanks_and_sanitizes_blank_ids_and_duplicates() {
        let master = parse_session_labels(
            "\n   \n\
             todo | First\n\
             | Missing Id\n\
             blankname |    | red\n\
             todo | Duplicate Id | green",
        );
        let labels = master.labels();
        // Only the first `todo` survives; the blank-id and blank-name lines drop.
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].id, "todo");
        assert_eq!(labels[0].name, "First");
    }

    #[test]
    fn format_session_labels_is_the_inverse_and_omits_the_unset_icon() {
        let master = SessionLabelMaster {
            labels: vec![
                SessionLabelDef {
                    id: "todo".to_string(),
                    name: "To Do".to_string(),
                    color: LabelColor::Gray,
                    icon: Some("○".to_string()),
                },
                SessionLabelDef {
                    id: "doing".to_string(),
                    name: "Doing".to_string(),
                    color: LabelColor::Blue,
                    icon: None,
                },
            ],
        };
        let text = format_session_labels(&master);
        assert_eq!(text, "todo | To Do | gray | ○\ndoing | Doing | blue");
        // Round-trips back to the same master.
        assert_eq!(parse_session_labels(&text), master);
        // The empty master renders as an empty buffer.
        assert_eq!(
            format_session_labels(&SessionLabelMaster { labels: vec![] }),
            ""
        );
    }

    #[test]
    fn parse_env_bindings_keeps_valid_lines_drops_the_rest_and_lets_later_win() {
        let env = parse_env_bindings(
            "GH_TOKEN = op://Private/GH/token\n\
             no_equals_line\n\
             1BAD=op://x/y/z\n\
             EMPTY=   \n\
             DUP=op://v/i/first\n\
             DUP=op://v/i/second\n",
        );
        assert_eq!(
            env.get("GH_TOKEN").map(String::as_str),
            Some("op://Private/GH/token")
        );
        assert!(!env.contains_key("1BAD"));
        assert!(!env.contains_key("EMPTY"));
        // A later line with the same name wins.
        assert_eq!(env.get("DUP").map(String::as_str), Some("op://v/i/second"));
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn format_env_bindings_is_the_inverse_in_sorted_order() {
        let env: SecretEnv = [
            ("B_TOKEN".to_string(), "op://v/i/b".to_string()),
            ("A_TOKEN".to_string(), "op://v/i/a".to_string()),
        ]
        .into_iter()
        .collect();
        // Sorted by name (BTreeMap order), one binding per line.
        assert_eq!(
            format_env_bindings(&env),
            "A_TOKEN=op://v/i/a\nB_TOKEN=op://v/i/b"
        );
        // Round-trips back to the same map.
        assert_eq!(parse_env_bindings(&format_env_bindings(&env)), env);
        // The empty map formats to an empty buffer.
        assert_eq!(format_env_bindings(&SecretEnv::new()), "");
    }

    #[test]
    fn setup_commands_filters_blank_lines_but_keeps_order() {
        let local = LocalSettings {
            setup_commands: vec![
                "npm install".to_string(),
                "  ".to_string(),
                "cargo test".to_string(),
            ],
            ..Default::default()
        };

        assert_eq!(
            local.setup_commands().collect::<Vec<_>>(),
            vec!["npm install", "cargo test"]
        );
    }

    #[test]
    fn agent_cli_maps_to_its_program_command() {
        assert_eq!(AgentCli::Claude.command(), "claude");
        assert_eq!(AgentCli::Codex.command(), "codex");
        assert_eq!(AgentCli::CodexFugu.command(), "codex-fugu");
        assert_eq!(AgentCli::Gemini.command(), "gemini");
        assert_eq!(AgentCli::Antigravity.command(), "agy");
    }

    #[test]
    fn agent_cli_all_and_display_names_cover_every_variant() {
        // ALL lists each variant once, in canonical order.
        assert_eq!(
            AgentCli::ALL,
            [
                AgentCli::Claude,
                AgentCli::Codex,
                AgentCli::CodexFugu,
                AgentCli::Gemini,
                AgentCli::Antigravity
            ]
        );
        // Each has a non-empty display name; the codex-fugu variant shows as
        // `sakana.ai` even though its launch command is `codex-fugu`.
        for cli in AgentCli::ALL {
            assert!(!cli.display_name().is_empty());
        }
        assert_eq!(AgentCli::Claude.display_name(), "Claude");
        assert_eq!(AgentCli::Codex.display_name(), "Codex");
        assert_eq!(AgentCli::CodexFugu.display_name(), "sakana.ai");
        assert_eq!(AgentCli::Gemini.display_name(), "Gemini");
        // Antigravity launches `agy` but is presented as `Antigravity`.
        assert_eq!(AgentCli::Antigravity.display_name(), "Antigravity");
    }

    #[test]
    fn agent_cli_from_name_accepts_command_and_display_names_case_insensitively() {
        // The launch command resolves each variant.
        assert_eq!(AgentCli::from_name("claude"), Some(AgentCli::Claude));
        assert_eq!(AgentCli::from_name("codex"), Some(AgentCli::Codex));
        assert_eq!(AgentCli::from_name("codex-fugu"), Some(AgentCli::CodexFugu));
        assert_eq!(AgentCli::from_name("gemini"), Some(AgentCli::Gemini));
        // Antigravity resolves from both its launch command (`agy`) and its
        // display name (`Antigravity`).
        assert_eq!(AgentCli::from_name("agy"), Some(AgentCli::Antigravity));
        assert_eq!(
            AgentCli::from_name("antigravity"),
            Some(AgentCli::Antigravity)
        );
        // The display name resolves too — `sakana.ai` is codex-fugu's label.
        assert_eq!(AgentCli::from_name("sakana.ai"), Some(AgentCli::CodexFugu));
        // The on-disk serde label (`codex_fugu`) that `usagi config` prints
        // resolves as well — `-`/`_` are the same separator — so copying what
        // `config` shows into `agent` / `clean --agent` works.
        assert_eq!(AgentCli::from_name("codex_fugu"), Some(AgentCli::CodexFugu));
        assert_eq!(
            AgentCli::from_name(" Codex_Fugu "),
            Some(AgentCli::CodexFugu)
        );
        // Case and surrounding whitespace are ignored.
        assert_eq!(AgentCli::from_name("  Claude "), Some(AgentCli::Claude));
        assert_eq!(AgentCli::from_name("SAKANA.AI"), Some(AgentCli::CodexFugu));
        // An unrecognised name resolves to nothing.
        assert_eq!(AgentCli::from_name("emacs"), None);
        assert_eq!(AgentCli::from_name(""), None);
    }

    #[test]
    fn agent_cli_serializes_in_snake_case() {
        // The persisted (on-disk) form is snake_case, so `CodexFugu` is stored as
        // `codex_fugu` even though the launched program is `codex-fugu`.
        assert_eq!(
            serde_json::to_string(&AgentCli::CodexFugu).unwrap(),
            "\"codex_fugu\""
        );
        assert_eq!(
            serde_json::from_str::<AgentCli>("\"codex_fugu\"").unwrap(),
            AgentCli::CodexFugu
        );
        assert_eq!(AgentCli::CodexFugu.command(), "codex-fugu");
    }

    #[test]
    fn agent_wiring_carries_the_binary_and_the_local_llm_model_only_when_enabled() {
        // Disabled (the default): the binary path is carried, the model is None.
        let mut settings = Settings::default();
        let off = settings.agent_wiring("/opt/usagi/bin/usagi");
        assert_eq!(off.usagi_bin, "/opt/usagi/bin/usagi");
        assert_eq!(off.local_llm_model, None);
        // No agent-model source yet: the wiring leaves the agent CLI on its own
        // default. The adapters render this when it is `Some`.
        assert_eq!(off.model, None);
        assert!(off.sandbox_writable_roots.is_empty());

        // Enabled: the configured model rides along for the adapter to wire in.
        settings.local_llm.enabled = true;
        settings.local_llm.model = "qwen2.5-coder:3b".to_string();
        let on = settings.agent_wiring("usagi");
        assert_eq!(on.local_llm_model.as_deref(), Some("qwen2.5-coder:3b"));
    }

    #[test]
    fn local_llm_defaults_to_off_with_the_default_model() {
        let local_llm = LocalLlm::default();
        assert!(!local_llm.enabled);
        assert_eq!(local_llm.model, "qwen2.5-coder:7b");
        assert_eq!(DEFAULT_LOCAL_LLM_MODEL, LOCAL_LLM_MODELS[0]);
        // The default model is one of the offered choices.
        assert!(LOCAL_LLM_MODELS.contains(&DEFAULT_LOCAL_LLM_MODEL));
    }

    #[test]
    fn sanitized_resets_an_unknown_local_llm_model() {
        // A known model from the allowlist is preserved verbatim.
        let known = Settings {
            local_llm: LocalLlm {
                enabled: true,
                model: "qwen2.5-coder:3b".to_string(),
            },
            ..Default::default()
        };
        assert_eq!(
            known.clone().sanitized().local_llm.model,
            "qwen2.5-coder:3b"
        );

        // A model outside the allowlist — e.g. a hand-edited injection attempt —
        // is reset to the default, while every other field is left untouched.
        let tampered = Settings {
            theme: Theme::Dark,
            local_llm: LocalLlm {
                enabled: true,
                model: "evil';touch /tmp/pwned;'".to_string(),
            },
            ..Default::default()
        };
        let cleaned = tampered.sanitized();
        assert_eq!(cleaned.local_llm.model, DEFAULT_LOCAL_LLM_MODEL);
        assert!(cleaned.local_llm.enabled);
        assert_eq!(cleaned.theme, Theme::Dark);
    }

    #[test]
    fn terminal_scrollback_lines_defaults_to_the_modest_cap() {
        // The default is the modest per-pane cap, not `usize`'s `0` — every pane
        // keeps at least this much scrollback out of the box.
        assert_eq!(
            Settings::default().terminal_scrollback_lines,
            DEFAULT_TERMINAL_SCROLLBACK_LINES
        );
        assert_eq!(default_terminal_scrollback_lines(), 2_000);
    }

    #[test]
    fn terminal_scrollback_lines_falls_back_when_the_field_is_absent() {
        // An older `settings.json` written before the field existed must load with
        // the modest default rather than `0` (which would leave panes with no
        // scrollback). `{}` stands in for any file missing the key.
        let loaded: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            loaded.terminal_scrollback_lines,
            DEFAULT_TERMINAL_SCROLLBACK_LINES
        );
        // A stored value is taken verbatim (clamping is `sanitized`'s job).
        let stored: Settings =
            serde_json::from_str(r#"{"terminal_scrollback_lines": 500}"#).unwrap();
        assert_eq!(stored.terminal_scrollback_lines, 500);
    }

    #[test]
    fn sanitized_clamps_an_oversized_scrollback_but_leaves_a_sane_one() {
        // A value within the cap is preserved untouched.
        let sane = Settings {
            terminal_scrollback_lines: 1_000,
            ..Default::default()
        };
        assert_eq!(sane.sanitized().terminal_scrollback_lines, 1_000);

        // A value past the cap — e.g. a hand-edited file asking every pane to keep
        // an unbounded buffer — is clamped to the maximum.
        let huge = Settings {
            terminal_scrollback_lines: usize::MAX,
            ..Default::default()
        };
        assert_eq!(
            huge.sanitized().terminal_scrollback_lines,
            MAX_TERMINAL_SCROLLBACK_LINES
        );
    }

    #[test]
    fn branch_source_defaults_to_remote_when_unset() {
        // An unset override resolves to the default (Remote)...
        assert_eq!(
            LocalSettings::default().branch_source(),
            BranchSource::Remote
        );
        assert_eq!(BranchSource::default(), BranchSource::Remote);
        // ...and a set value is returned as-is.
        assert_eq!(
            LocalSettings {
                default_branch_source: Some(BranchSource::Local),
                ..Default::default()
            }
            .branch_source(),
            BranchSource::Local
        );
    }

    #[test]
    fn default_branch_resolves_to_none_or_the_named_branch() {
        // Unset: use the repository's detected default branch.
        assert_eq!(LocalSettings::default().default_branch(), None);
        // Set: the named branch is returned.
        assert_eq!(
            LocalSettings {
                default_branch: Some("develop".to_string()),
                ..Default::default()
            }
            .default_branch(),
            Some("develop")
        );
    }

    #[test]
    fn sidebar_defaults_to_full_and_serializes_in_snake_case() {
        // The screen opens with the full-width sidebar unless configured otherwise.
        assert_eq!(Sidebar::default(), Sidebar::Full);
        assert_eq!(Settings::default().sidebar, Sidebar::Full);
        // Round-trips through the snake_case JSON the rest of the settings use.
        assert_eq!(serde_json::to_string(&Sidebar::Full).unwrap(), "\"full\"");
        assert_eq!(serde_json::to_string(&Sidebar::Rail).unwrap(), "\"rail\"");
        assert_eq!(
            serde_json::from_str::<Sidebar>("\"rail\"").unwrap(),
            Sidebar::Rail
        );
    }

    #[test]
    fn key_scheme_defaults_to_prefix_and_serializes_in_snake_case() {
        // 没入 opens with the Ctrl-O prefix scheme unless configured otherwise —
        // it claims only one chord and works on any terminal without setup.
        assert_eq!(KeyScheme::default(), KeyScheme::Prefix);
        assert_eq!(Settings::default().key_scheme, KeyScheme::Prefix);
        // ALL lists both schemes once, in cycle order (prefix first).
        assert_eq!(KeyScheme::ALL, [KeyScheme::Prefix, KeyScheme::Alt]);
        // Round-trips through the snake_case JSON the rest of the settings use.
        assert_eq!(
            serde_json::to_string(&KeyScheme::Prefix).unwrap(),
            "\"prefix\""
        );
        assert_eq!(serde_json::to_string(&KeyScheme::Alt).unwrap(), "\"alt\"");
        assert_eq!(
            serde_json::from_str::<KeyScheme>("\"alt\"").unwrap(),
            KeyScheme::Alt
        );
    }

    #[test]
    fn key_scheme_falls_back_to_default_when_absent_or_unknown() {
        // An older settings.json (written before the field existed) loads with the
        // default scheme rather than failing.
        let loaded: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded.key_scheme, KeyScheme::Prefix);
        // A hand-edited unknown value degrades to the default too (serde_fallback).
        let bad: Settings = serde_json::from_str(r#"{"key_scheme": "chord"}"#).unwrap();
        assert_eq!(bad.key_scheme, KeyScheme::Prefix);
    }

    #[test]
    fn sidebar_toggles_between_full_and_rail() {
        assert_eq!(Sidebar::Full.toggled(), Sidebar::Rail);
        assert_eq!(Sidebar::Rail.toggled(), Sidebar::Full);
    }

    #[test]
    fn mascot_animation_defaults_on_and_round_trips() {
        // The mascot's reactions are opt-out: on unless explicitly disabled.
        assert!(Settings::default().mascot_animation_enabled);
        // An explicit `false` survives a JSON round-trip, and an absent field
        // falls back to the default (`true`) via `#[serde(default)]`.
        let off = Settings {
            mascot_animation_enabled: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&off).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), off);
        assert!(
            serde_json::from_str::<Settings>("{}")
                .unwrap()
                .mascot_animation_enabled
        );
    }

    #[test]
    fn branch_source_serializes_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&BranchSource::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(
            serde_json::to_string(&BranchSource::Remote).unwrap(),
            "\"remote\""
        );
        assert_eq!(
            serde_json::from_str::<BranchSource>("\"remote\"").unwrap(),
            BranchSource::Remote
        );
    }

    #[test]
    fn skill_feature_metadata_is_consistent() {
        // Every feature in ALL round-trips through its id and is on by default
        // (shipped skills are opt-out).
        for feature in SkillFeature::ALL {
            assert_eq!(SkillFeature::from_id(feature.id()), Some(feature));
            assert!(!feature.label().is_empty());
            assert!(feature.default_enabled());
        }
        assert_eq!(SkillFeature::PullRequest.id(), "pull-request");
        // An unknown id resolves to nothing.
        assert_eq!(SkillFeature::from_id("nope"), None);
    }

    #[test]
    fn skill_feature_enabled_defaults_on_and_honours_an_explicit_value() {
        // Absent from the map → the feature's default (on).
        let mut settings = Settings::default();
        assert!(settings.skill_feature_enabled(SkillFeature::PullRequest));
        // An explicit `false` is honoured...
        settings
            .skill_features
            .insert("pull-request".to_string(), false);
        assert!(!settings.skill_feature_enabled(SkillFeature::PullRequest));
        // ...as is an explicit `true`.
        settings
            .skill_features
            .insert("pull-request".to_string(), true);
        assert!(settings.skill_feature_enabled(SkillFeature::PullRequest));
    }

    #[test]
    fn sanitized_drops_unknown_skill_feature_keys() {
        let mut settings = Settings::default();
        settings
            .skill_features
            .insert("pull-request".to_string(), false);
        // A key no current usagi knows (a removed feature or a hand-edited typo).
        settings
            .skill_features
            .insert("ghost-feature".to_string(), true);
        let cleaned = settings.sanitized();
        // The known key survives; the unknown one is dropped.
        assert_eq!(
            cleaned.skill_features.get("pull-request").copied(),
            Some(false)
        );
        assert!(!cleaned.skill_features.contains_key("ghost-feature"));
    }

    #[test]
    fn with_local_overrides_a_skill_feature_when_set() {
        // Global leaves the PR skills on (the default); a project override turns
        // them off for just this project.
        let global = Settings::default();
        assert!(global.skill_feature_enabled(SkillFeature::PullRequest));
        let mut local = LocalSettings::default();
        local
            .skill_features
            .insert("pull-request".to_string(), false);

        let effective = global.with_local(&local);
        assert!(!effective.skill_feature_enabled(SkillFeature::PullRequest));
    }

    #[test]
    fn is_empty_counts_a_skill_feature_override() {
        assert!(LocalSettings::default().is_empty());
        let mut local = LocalSettings::default();
        local
            .skill_features
            .insert("pull-request".to_string(), false);
        assert!(!local.is_empty());
        assert_eq!(
            local.skill_feature_override(SkillFeature::PullRequest),
            Some(false)
        );
        // A feature the project does not mention defers to the global setting.
        assert_eq!(
            LocalSettings::default().skill_feature_override(SkillFeature::PullRequest),
            None
        );
    }

    #[test]
    fn skill_features_default_to_empty_and_round_trip() {
        // An older settings.json (written before the field existed) loads with an
        // empty map — every feature then uses its default.
        let loaded: Settings = serde_json::from_str("{}").unwrap();
        assert!(loaded.skill_features.is_empty());
        assert!(loaded.skill_feature_enabled(SkillFeature::PullRequest));
        // An explicit entry survives a JSON round-trip.
        let mut settings = Settings::default();
        settings
            .skill_features
            .insert("pull-request".to_string(), false);
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), settings);
    }

    #[test]
    fn label_color_all_lists_every_variant_and_defaults_to_gray() {
        // ALL lists each colour once, neutral (the default) first.
        assert_eq!(
            LabelColor::ALL,
            [
                LabelColor::Gray,
                LabelColor::Red,
                LabelColor::Green,
                LabelColor::Yellow,
                LabelColor::Blue,
                LabelColor::Magenta,
                LabelColor::Cyan,
            ]
        );
        assert_eq!(LabelColor::default(), LabelColor::Gray);
        // Round-trips through the snake_case JSON the rest of the settings use.
        assert_eq!(
            serde_json::to_string(&LabelColor::Magenta).unwrap(),
            "\"magenta\""
        );
        assert_eq!(
            serde_json::from_str::<LabelColor>("\"cyan\"").unwrap(),
            LabelColor::Cyan
        );
    }

    #[test]
    fn label_def_glyph_uses_first_char_or_the_default() {
        // A set icon shows its first character (kept to one glyph wide)...
        let with_icon = SessionLabelDef {
            id: "x".to_string(),
            name: "X".to_string(),
            color: LabelColor::Gray,
            icon: Some("◆".to_string()),
        };
        assert_eq!(with_icon.glyph(), '◆');
        // ...a blank icon falls back to the default bullet...
        let blank = SessionLabelDef {
            icon: Some("   ".to_string()),
            ..with_icon.clone()
        };
        assert_eq!(blank.glyph(), DEFAULT_LABEL_ICON);
        // ...and an unset icon does too.
        let none = SessionLabelDef {
            icon: None,
            ..with_icon
        };
        assert_eq!(none.glyph(), DEFAULT_LABEL_ICON);
    }

    #[test]
    fn label_def_color_degrades_an_unknown_value_and_omits_an_unset_icon() {
        // An unrecognised colour degrades to the default rather than failing.
        let def: SessionLabelDef =
            serde_json::from_str(r#"{"id":"a","name":"A","color":"chartreuse"}"#).unwrap();
        assert_eq!(def.color, LabelColor::Gray);
        assert_eq!(def.icon, None);
        // An unset icon is dropped from the serialized form.
        let json = serde_json::to_string(&def).unwrap();
        assert!(!json.contains("icon"));
    }

    #[test]
    fn label_master_defaults_to_a_non_empty_kanban_set() {
        let master = SessionLabelMaster::default();
        assert!(!master.is_empty());
        assert_eq!(master.len(), 5);
        // The default set is addressable by id, position, and index.
        assert_eq!(master.at(0).map(|l| l.id.as_str()), Some("todo"));
        assert_eq!(
            master.get("review").map(|l| l.name.as_str()),
            Some("Review")
        );
        assert_eq!(master.position("done"), Some(4));
        // An unknown id / out-of-range index resolves to nothing.
        assert_eq!(master.get("ghost"), None);
        assert_eq!(master.position("ghost"), None);
        assert!(master.at(99).is_none());
    }

    #[test]
    fn label_master_is_transparent_json_and_defaults_when_absent() {
        // The master serializes as a bare array (transparent), not a wrapper object.
        let master = SessionLabelMaster {
            labels: vec![SessionLabelDef {
                id: "todo".to_string(),
                name: "Todo".to_string(),
                color: LabelColor::Gray,
                icon: None,
            }],
        };
        let json = serde_json::to_string(&master).unwrap();
        assert!(json.starts_with('['));
        assert_eq!(
            serde_json::from_str::<SessionLabelMaster>(&json).unwrap(),
            master
        );

        // Absent from settings.json → the default (non-empty) set loads.
        let loaded: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(loaded.session_labels, SessionLabelMaster::default());
        // An explicit empty array leaves the feature dormant.
        let empty: Settings = serde_json::from_str(r#"{"session_labels":[]}"#).unwrap();
        assert!(empty.session_labels.is_empty());
    }

    #[test]
    fn label_master_sanitized_drops_blank_and_duplicate_ids() {
        let master = SessionLabelMaster {
            labels: vec![
                SessionLabelDef {
                    id: "todo".to_string(),
                    name: "Todo".to_string(),
                    color: LabelColor::Gray,
                    icon: None,
                },
                // Blank id — dropped.
                SessionLabelDef {
                    id: "  ".to_string(),
                    name: "Nameless".to_string(),
                    color: LabelColor::Gray,
                    icon: None,
                },
                // Blank name — dropped.
                SessionLabelDef {
                    id: "empty".to_string(),
                    name: "".to_string(),
                    color: LabelColor::Gray,
                    icon: None,
                },
                // Duplicate id — only the first "todo" survives.
                SessionLabelDef {
                    id: "todo".to_string(),
                    name: "Todo again".to_string(),
                    color: LabelColor::Red,
                    icon: None,
                },
                SessionLabelDef {
                    id: "done".to_string(),
                    name: "Done".to_string(),
                    color: LabelColor::Green,
                    icon: None,
                },
            ],
        };
        let clean = master.sanitized();
        assert_eq!(
            clean
                .labels()
                .iter()
                .map(|l| l.id.as_str())
                .collect::<Vec<_>>(),
            vec!["todo", "done"]
        );
        // The surviving "todo" is the first one (Gray), not the duplicate (Red).
        assert_eq!(clean.get("todo").unwrap().color, LabelColor::Gray);
    }

    #[test]
    fn sanitized_cleans_the_label_master() {
        let settings = Settings {
            session_labels: SessionLabelMaster {
                labels: vec![
                    SessionLabelDef {
                        id: "todo".to_string(),
                        name: "Todo".to_string(),
                        color: LabelColor::Gray,
                        icon: None,
                    },
                    SessionLabelDef {
                        id: "  ".to_string(),
                        name: "blank".to_string(),
                        color: LabelColor::Gray,
                        icon: None,
                    },
                ],
            },
            ..Default::default()
        };
        assert_eq!(settings.sanitized().session_labels.len(), 1);
    }

    #[test]
    fn with_local_replaces_the_label_master_only_when_set() {
        let global = Settings::default();
        // Unset → the global (default) set is kept.
        assert_eq!(
            global
                .clone()
                .with_local(&LocalSettings::default())
                .session_labels,
            SessionLabelMaster::default()
        );
        // A project set replaces it wholesale (and is sanitized on the way in — the
        // blank-id entry is dropped).
        let local = LocalSettings {
            session_labels: Some(SessionLabelMaster {
                labels: vec![
                    SessionLabelDef {
                        id: "wip".to_string(),
                        name: "WIP".to_string(),
                        color: LabelColor::Yellow,
                        icon: None,
                    },
                    SessionLabelDef {
                        id: "".to_string(),
                        name: "blank".to_string(),
                        color: LabelColor::Gray,
                        icon: None,
                    },
                ],
            }),
            ..Default::default()
        };
        let effective = global.with_local(&local);
        assert_eq!(
            effective
                .session_labels
                .labels()
                .iter()
                .map(|l| l.id.as_str())
                .collect::<Vec<_>>(),
            vec!["wip"]
        );
        // An empty project set turns the feature off for the project.
        let off = Settings::default().with_local(&LocalSettings {
            session_labels: Some(SessionLabelMaster { labels: vec![] }),
            ..Default::default()
        });
        assert!(off.session_labels.is_empty());
    }

    #[test]
    fn is_empty_counts_a_label_master_override() {
        assert!(LocalSettings::default().is_empty());
        assert!(!LocalSettings {
            session_labels: Some(SessionLabelMaster::default()),
            ..Default::default()
        }
        .is_empty());
    }
}
