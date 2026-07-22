//! Config screen state and rendering.

use std::time::Duration;

use usagi_core::domain::settings::{DefaultModel, ModalSelectionMode, Settings, Theme};
use usagi_core::usecase::settings::{SettingsPort, SettingsScope};

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::Style;
use crate::presentation::widgets::{self, modal, select};

const TITLE: &str = "Config";
const FOOTER: &str = "↑↓: select  ←→: change  ●: unsaved  Enter: save  Esc: back";
const MODAL_INNER_WIDTH: usize = 64;
const MODAL_BODY_HEIGHT: usize = 9;
const MODAL_FOOTER: &str = "↑↓: select  ←→: change  Enter: save  Esc: back";
const SECTION_HEADING_WIDTH: usize = 35;

/// Time between frames while the Save button's highlight wave is moving.
pub const SAVE_WAVE_TICK: Duration = Duration::from_millis(60);
/// A full sweep across the four-letter Save caption, including its fade-out.
pub const SAVE_WAVE_FRAMES: usize = 6;

/// How long the `done` confirmation stays on screen before the Config screen
/// returns home on its own, with no key press. Short enough to feel immediate,
/// long enough to read — a peer of the other screen-timing constants
/// (`splash::ANIM_TICK`, `SIDEBAR_DOUBLE_CLICK`). This constant is the single
/// source of truth for the Config save confirmation dwell.
pub const DONE_DISPLAY: Duration = Duration::from_millis(600);

/// The Save action's lifecycle across a single save. The screen graph draws the
/// `Saving` wave before the blocking write and holds the `Done` frame for
/// [`DONE_DISPLAY`] before returning home; a failed write drops back to `Idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SavePhase {
    /// No save in flight; the button reads `Save`.
    #[default]
    Idle,
    /// A save has begun and the blocking write is about to run (loading).
    Saving,
    /// The write succeeded; the confirmation is on screen until the dwell ends.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Field {
    #[default]
    Theme,
    ModalSelectionMode,
    DefaultModel,
    Issue,
    Memory,
    Save,
}

/// Agent-model CLIs available to the Config screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvailableAgentModels {
    claude: bool,
    open_ai: bool,
}

impl AvailableAgentModels {
    /// Construct availability from the installed Claude and Codex CLIs.
    #[must_use]
    pub const fn new(claude: bool, open_ai: bool) -> Self {
        Self { claude, open_ai }
    }

    /// Availability used by callers that do not supply a system probe.
    #[must_use]
    pub const fn all() -> Self {
        Self::new(true, true)
    }

    const fn is_empty(self) -> bool {
        !self.claude && !self.open_ai
    }

    const fn contains(self, model: DefaultModel) -> bool {
        match model {
            DefaultModel::Claude => self.claude,
            DefaultModel::OpenAi => self.open_ai,
        }
    }

    const fn first(self) -> Option<DefaultModel> {
        if self.open_ai {
            Some(DefaultModel::OpenAi)
        } else if self.claude {
            Some(DefaultModel::Claude)
        } else {
            None
        }
    }

    const fn next(self, model: DefaultModel) -> Option<DefaultModel> {
        match (self.claude, self.open_ai, model) {
            (false, false, _) => None,
            (true, true, DefaultModel::Claude) | (false, true, _) => Some(DefaultModel::OpenAi),
            (true, true, DefaultModel::OpenAi) | (true, false, _) => Some(DefaultModel::Claude),
        }
    }
}

#[derive(Debug, Clone)]
struct ScopeSettings {
    saved: Settings,
    draft: Settings,
}

impl ScopeSettings {
    fn is_dirty(&self) -> bool {
        self.draft != self.saved
    }
}

/// Editable Config screen state. Global Config edits application preferences
/// and workspace defaults; the Overview modal edits only the current
/// workspace's Agent, Issue, and Memory choices.
#[derive(Debug, Clone)]
pub struct Config {
    scope: SettingsScope,
    field: Field,
    settings: ScopeSettings,
    available_models: AvailableAgentModels,
    notice: Option<String>,
    save_phase: SavePhase,
    save_animation_frame: usize,
}

impl Config {
    /// Read Global settings from `port` and initialize its draft. A failed read
    /// falls back to defaults while surfacing a safe error.
    #[must_use]
    pub fn load(port: &mut dyn SettingsPort) -> Self {
        Self::load_with_available_models(port, AvailableAgentModels::all())
    }

    /// Read Global settings and constrain Agent choices to installed CLIs.
    #[must_use]
    pub fn load_with_available_models(
        port: &mut dyn SettingsPort,
        available_models: AvailableAgentModels,
    ) -> Self {
        Self::load_scope(port, SettingsScope::Global, available_models)
    }

    fn load_scope(
        port: &mut dyn SettingsPort,
        scope: SettingsScope,
        available_models: AvailableAgentModels,
    ) -> Self {
        let (saved, error) = read_scope(port, scope);
        let draft = available_models
            .first()
            .filter(|_| !available_models.contains(saved.default_model))
            .map_or(saved.clone(), |model| Settings {
                default_model: model,
                ..saved.clone()
            });
        let field = match scope {
            SettingsScope::Global => Field::Theme,
            SettingsScope::Workspace if available_models.is_empty() => Field::Issue,
            SettingsScope::Workspace => Field::DefaultModel,
        };
        Self {
            scope,
            field,
            settings: ScopeSettings { saved, draft },
            available_models,
            notice: error,
            save_phase: SavePhase::Idle,
            save_animation_frame: 0,
        }
    }

    /// Read the current workspace settings and open its focused editor.
    ///
    /// Overview uses this entry point so `config` targets the workspace that owns
    /// the command palette instead of initially presenting the global defaults.
    #[must_use]
    pub fn load_workspace_with_available_models(
        port: &mut dyn SettingsPort,
        available_models: AvailableAgentModels,
    ) -> Self {
        Self::load_scope(port, SettingsScope::Workspace, available_models)
    }

    /// Returns the selected persistence scope.
    #[must_use]
    pub fn scope(&self) -> SettingsScope {
        self.scope
    }

    /// Returns the selected editable setting.
    #[must_use]
    pub fn field(&self) -> Field {
        self.field
    }

    /// Move to the next setting or Save action.
    pub fn next_field(&mut self) {
        self.field = match self.scope {
            SettingsScope::Global => match self.field {
                Field::Theme => Field::ModalSelectionMode,
                Field::ModalSelectionMode => Field::DefaultModel,
                Field::DefaultModel => Field::Issue,
                Field::Issue => Field::Memory,
                Field::Memory => Field::Save,
                Field::Save => Field::Theme,
            },
            SettingsScope::Workspace => match self.field {
                Field::DefaultModel => Field::Issue,
                Field::Issue => Field::Memory,
                Field::Memory => Field::Save,
                Field::Save | Field::Theme | Field::ModalSelectionMode => Field::DefaultModel,
            },
        };
        if self.field == Field::DefaultModel && self.available_models.is_empty() {
            self.field = Field::Issue;
        }
        self.notice = None;
    }

    /// Move to the previous editable setting.
    pub fn previous_field(&mut self) {
        self.field = match self.scope {
            SettingsScope::Global => match self.field {
                Field::Theme => Field::Save,
                Field::ModalSelectionMode => Field::Theme,
                Field::DefaultModel => Field::ModalSelectionMode,
                Field::Issue => Field::DefaultModel,
                Field::Memory => Field::Issue,
                Field::Save => Field::Memory,
            },
            SettingsScope::Workspace => match self.field {
                Field::Issue => Field::DefaultModel,
                Field::Memory => Field::Issue,
                Field::Save => Field::Memory,
                Field::DefaultModel | Field::Theme | Field::ModalSelectionMode => Field::Save,
            },
        };
        if self.field == Field::DefaultModel && self.available_models.is_empty() {
            self.field = match self.scope {
                SettingsScope::Global => Field::ModalSelectionMode,
                SettingsScope::Workspace => Field::Save,
            };
        }
        self.notice = None;
    }

    /// Returns whether the selected scope has an unsaved draft.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.current().is_dirty()
    }

    /// Returns the selected scope's editable settings.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.current().draft
    }

    /// Returns the latest save or load feedback, if any.
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Cycle the selected scope's theme. Either arrow direction uses the same
    /// two non-system alternatives before returning to system.
    pub fn cycle_theme(&mut self, forward: bool) {
        let theme = &mut self.current_mut().draft.theme;
        *theme = match (*theme, forward) {
            (Theme::System, true) | (Theme::Light, false) => Theme::Dark,
            (Theme::Dark, true) | (Theme::System, false) => Theme::Light,
            (Theme::Light, true) | (Theme::Dark, false) => Theme::System,
        };
        self.notice = None;
    }

    /// Toggle how Overview and Closeup accept a command.
    pub fn cycle_modal_selection_mode(&mut self) {
        let mode = &mut self.current_mut().draft.modal_selection_mode;
        *mode = match *mode {
            ModalSelectionMode::Action => ModalSelectionMode::Prompt,
            ModalSelectionMode::Prompt => ModalSelectionMode::Action,
        };
        self.notice = None;
    }

    /// Switch the default cloud model between Claude and `OpenAI`.
    pub fn cycle_default_model(&mut self) {
        let model = self.current().draft.default_model;
        if let Some(next) = self.available_models.next(model) {
            self.current_mut().draft.default_model = next;
        }
        self.notice = None;
    }

    /// Toggle availability of the issue MCP tool family.
    pub fn cycle_issue_enabled(&mut self) {
        let enabled = &mut self.current_mut().draft.issue_enabled;
        *enabled = !*enabled;
        self.notice = None;
    }

    /// Toggle availability of the memory MCP tool family.
    pub fn cycle_memory_enabled(&mut self) {
        let enabled = &mut self.current_mut().draft.memory_enabled;
        *enabled = !*enabled;
        self.notice = None;
    }

    /// Change the focused select value. Returns false for the Save action.
    pub fn cycle_selected(&mut self, forward: bool) -> bool {
        match self.field {
            Field::Theme => self.cycle_theme(forward),
            Field::ModalSelectionMode => self.cycle_modal_selection_mode(),
            Field::DefaultModel => self.cycle_default_model(),
            Field::Issue => self.cycle_issue_enabled(),
            Field::Memory => self.cycle_memory_enabled(),
            Field::Save => return false,
        }
        true
    }

    /// Returns whether the focused row is the enabled Save action.
    #[must_use]
    pub fn can_save(&self) -> bool {
        self.field == Field::Save && self.is_dirty()
    }

    /// Begin a save: enter the loading phase so the caller can draw a `Saving`
    /// frame before the blocking write. Returns false — a no-op — unless the
    /// focused Save row is dirty and no save is already in flight, which makes a
    /// second Enter during a save (double press) safe.
    pub fn begin_save(&mut self) -> bool {
        if self.save_phase != SavePhase::Idle || !self.can_save() {
            return false;
        }
        self.save_phase = SavePhase::Saving;
        self.save_animation_frame = 0;
        self.notice = None;
        true
    }

    /// Advance the highlight wave drawn across the Save button while its write
    /// is pending. The screen graph calls this between animation frames.
    pub fn advance_save_animation(&mut self) {
        self.save_animation_frame = self.save_animation_frame.wrapping_add(1);
    }

    /// Persist the selected scope's dirty draft. On success it records the saved
    /// value, enters the `Done` phase, and returns true; on failure it drops
    /// back to `Idle`, keeps the draft dirty, and surfaces a safe error so the
    /// user can retry. Returns false without touching the port when not dirty.
    pub fn commit_save(&mut self, port: &mut dyn SettingsPort) -> bool {
        if !self.is_dirty() {
            self.save_phase = SavePhase::Idle;
            return false;
        }
        let scope = self.scope;
        let draft = self.current().draft.clone();
        match port.save(scope, &draft) {
            Ok(()) => {
                self.current_mut().saved = draft;
                self.save_phase = SavePhase::Done;
                self.notice = None;
                true
            }
            Err(error) => {
                self.save_phase = SavePhase::Idle;
                self.notice = Some(format!("Save failed: {error}"));
                false
            }
        }
    }

    /// Clear the confirmation once the dwell ends and the screen returns home,
    /// so a later visit to Config starts from a clean Save row.
    pub fn reset_save(&mut self) {
        self.save_phase = SavePhase::Idle;
        self.save_animation_frame = 0;
        self.notice = None;
    }

    /// The Save button's current label, driven by the save phase.
    fn save_label(&self) -> &'static str {
        match self.save_phase {
            SavePhase::Idle | SavePhase::Saving => "Save",
            SavePhase::Done => "done",
        }
    }

    fn current(&self) -> &ScopeSettings {
        &self.settings
    }

    fn current_mut(&mut self) -> &mut ScopeSettings {
        &mut self.settings
    }
}

fn read_scope(port: &mut dyn SettingsPort, scope: SettingsScope) -> (Settings, Option<String>) {
    match port.read(scope) {
        Ok(settings) => (settings, None),
        Err(error) => (Settings::default(), Some(format!("Load failed: {error}"))),
    }
}

/// Render a Config frame using its current scope, draft, and feedback.
#[must_use]
pub fn render(raw_height: usize, raw_width: usize, config: &Config) -> Vec<String> {
    mascot_screen::render(raw_height, raw_width, TITLE, FOOTER, |width| {
        form_rows(config)
            .into_iter()
            .map(|line| mascot_screen::centered_line(width, &line, Style::new()))
            .collect()
    })
}

/// Render Workspace Config as a modal over the live Home frame.
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    config: &Config,
) -> Vec<String> {
    let mut lines = vec![String::new()];
    lines.extend(form_rows(config).into_iter().map(|line| {
        if line.is_empty() {
            line
        } else {
            modal::content_line(&line, MODAL_INNER_WIDTH)
        }
    }));
    lines.push(String::new());
    lines.push(modal::footer(MODAL_FOOTER));
    modal::render_body_over(
        raw_height,
        raw_width,
        base,
        TITLE,
        MODAL_INNER_WIDTH,
        MODAL_BODY_HEIGHT,
        lines,
    )
}

fn form_rows(config: &Config) -> Vec<String> {
    let mut lines = match config.scope() {
        SettingsScope::Global => global_rows(config),
        SettingsScope::Workspace => workspace_rows(config),
    };
    lines.push(String::new());
    lines.push(save_action(config));
    if let Some(notice) = config.notice() {
        lines.push(Style::new().dim().paint(notice));
    }
    lines
}

fn save_action(config: &Config) -> String {
    if config.save_phase == SavePhase::Saving {
        let marker = modal::selection_marker(config.field() == Field::Save);
        let caption = widgets::shimmer_text("Save", config.save_animation_frame);
        format!("{marker}   [ {caption} ]")
    } else {
        select::action(
            config.save_label(),
            config.field() == Field::Save,
            config.is_dirty() || config.save_phase == SavePhase::Done,
        )
    }
}

fn global_rows(config: &Config) -> Vec<String> {
    let mut lines = vec![
        section_heading("Global"),
        select::render(
            "Theme",
            theme_name(config.settings().theme),
            config.field() == Field::Theme,
            config.settings().theme != config.current().saved.theme,
        ),
        select::render(
            "Modal mode",
            modal_selection_mode_name(config.settings().modal_selection_mode),
            config.field() == Field::ModalSelectionMode,
            config.settings().modal_selection_mode != config.current().saved.modal_selection_mode,
        ),
        String::new(),
        section_heading("Workspace init"),
    ];
    lines.extend(workspace_setting_rows(config));
    lines
}

fn workspace_rows(config: &Config) -> Vec<String> {
    workspace_setting_rows(config)
}

fn workspace_setting_rows(config: &Config) -> Vec<String> {
    vec![
        if config.available_models.is_empty() {
            select::disabled("Agent", "none")
        } else {
            select::render(
                "Agent",
                default_model_name(config.settings().default_model),
                config.field() == Field::DefaultModel,
                config.settings().default_model != config.current().saved.default_model,
            )
        },
        select::render(
            "Issue",
            enabled_name(config.settings().issue_enabled),
            config.field() == Field::Issue,
            config.settings().issue_enabled != config.current().saved.issue_enabled,
        ),
        select::render(
            "Memory",
            enabled_name(config.settings().memory_enabled),
            config.field() == Field::Memory,
            config.settings().memory_enabled != config.current().saved.memory_enabled,
        ),
    ]
}

fn section_heading(label: &str) -> String {
    let rule_width = SECTION_HEADING_WIDTH - label.len() - 1;
    Style::new()
        .dim()
        .paint(&format!("{label} {}", "─".repeat(rule_width)))
}

fn theme_name(theme: Theme) -> &'static str {
    match theme {
        Theme::Light => "light",
        Theme::Dark => "dark",
        Theme::System => "system",
    }
}

fn modal_selection_mode_name(mode: ModalSelectionMode) -> &'static str {
    match mode {
        ModalSelectionMode::Action => "action",
        ModalSelectionMode::Prompt => "prompt",
    }
}

fn default_model_name(model: DefaultModel) -> &'static str {
    match model {
        DefaultModel::Claude => "Claude",
        DefaultModel::OpenAi => "OpenAI",
    }
}

fn enabled_name(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::{AvailableAgentModels, Config, Field, render, render_over};
    use crate::presentation::widgets::{display_width, strip_ansi};
    use std::io;
    use usagi_core::domain::settings::{DefaultModel, ModalSelectionMode, Settings, Theme};
    use usagi_core::usecase::settings::{SettingsPort, SettingsScope};

    #[derive(Default)]
    struct FakeSettingsPort {
        global: Settings,
        workspace: Settings,
        fail_read: Option<SettingsScope>,
        fail_save: bool,
    }

    impl SettingsPort for FakeSettingsPort {
        fn read(&mut self, scope: SettingsScope) -> io::Result<Settings> {
            if self.fail_read == Some(scope) {
                return Err(io::Error::other("settings unavailable"));
            }
            Ok(match scope {
                SettingsScope::Global => self.global.clone(),
                SettingsScope::Workspace => self.workspace.clone(),
            })
        }

        fn save(&mut self, scope: SettingsScope, settings: &Settings) -> io::Result<()> {
            if self.fail_save {
                return Err(io::Error::other("disk unavailable"));
            }
            match scope {
                SettingsScope::Global => self.global = settings.clone(),
                SettingsScope::Workspace => self.workspace = settings.clone(),
            }
            Ok(())
        }
    }

    /// Settings port that counts successful saves, used to prove a double press
    /// persists exactly once.
    #[derive(Default)]
    struct CountingSettingsPort {
        settings: Settings,
        saves: usize,
    }

    impl SettingsPort for CountingSettingsPort {
        fn read(&mut self, _scope: SettingsScope) -> io::Result<Settings> {
            Ok(self.settings.clone())
        }

        fn save(&mut self, _scope: SettingsScope, settings: &Settings) -> io::Result<()> {
            self.settings = settings.clone();
            self.saves += 1;
            Ok(())
        }
    }

    #[test]
    fn global_and_workspace_entries_save_only_their_own_target() {
        let mut port = FakeSettingsPort {
            global: Settings {
                theme: Theme::Light,
                ..Settings::default()
            },
            workspace: Settings::default(),
            ..FakeSettingsPort::default()
        };
        let mut config = Config::load(&mut port);
        let initial = render(24, 80, &config).join("\n");
        assert!(initial.contains("Theme") && initial.contains("light"));
        config.cycle_theme(true);
        config.commit_save(&mut port);
        assert_eq!(port.global.theme, Theme::System);

        let mut workspace =
            Config::load_workspace_with_available_models(&mut port, AvailableAgentModels::all());
        assert_eq!(workspace.scope(), SettingsScope::Workspace);
        assert_eq!(workspace.field(), Field::DefaultModel);
        workspace.next_field();
        workspace.cycle_issue_enabled();
        workspace.next_field();
        workspace.next_field();
        assert!(workspace.commit_save(&mut port));
        assert!(!port.workspace.issue_enabled);
        assert_eq!(port.global.theme, Theme::System);
    }

    #[test]
    fn failed_save_keeps_the_draft_dirty_for_retry() {
        let mut port = FakeSettingsPort {
            fail_save: true,
            ..FakeSettingsPort::default()
        };
        let mut config = Config::load(&mut port);
        config.cycle_theme(true);
        config.commit_save(&mut port);

        assert_eq!(config.settings().theme, Theme::Dark);
        assert!(config.is_dirty());
        assert_eq!(config.notice(), Some("Save failed: disk unavailable"));

        port.fail_save = false;
        config.commit_save(&mut port);
        assert!(!config.is_dirty());
        assert_eq!(port.global.theme, Theme::Dark);
    }

    #[test]
    fn global_render_groups_application_settings_and_workspace_defaults() {
        let mut port = FakeSettingsPort::default();
        let config = Config::load(&mut port);
        let plain = render(24, 80, &config)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        let frame = plain.join("\n");

        assert!(frame.contains("Config"));
        assert!(frame.contains("Global"));
        assert!(frame.contains("Theme") && frame.contains("system"));
        assert!(frame.contains("Modal mode") && frame.contains("action"));
        assert!(frame.contains("Workspace init"));
        assert!(frame.contains("Agent") && frame.contains("OpenAI"));
        assert!(frame.contains("Issue") && frame.contains("on"));
        assert!(frame.contains("Memory") && frame.contains("on"));
        assert!(!frame.contains("Scope:"));
        assert!(!frame.contains("Tab: scope"));
        assert!(frame.contains("[ Save ]"));
        assert!(frame.contains("Esc: back"));

        let global = plain.iter().find(|line| line.contains("Global")).unwrap();
        let workspace = plain
            .iter()
            .find(|line| line.contains("Workspace init"))
            .unwrap();
        assert_eq!(global.find("Global"), workspace.find("Workspace init"));
    }

    #[test]
    fn global_chevrons_align_with_the_heading_without_moving_controls() {
        let mut port = FakeSettingsPort::default();
        let mut config = Config::load(&mut port);
        let plain = render(24, 80, &config)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        let heading = plain.iter().find(|line| line.contains("Global")).unwrap();
        let theme = plain.iter().find(|line| line.contains("Theme")).unwrap();
        let chevron_column = heading.find("Global").unwrap() + "Globa".len();

        assert_eq!(theme.find('›'), Some(chevron_column));
        assert_eq!(display_width(&theme[..theme.find('<').unwrap()]), 43);

        for _ in 0..5 {
            config.next_field();
        }
        let save_frame = render(24, 80, &config)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        let save = save_frame
            .iter()
            .find(|line| line.contains("[ Save ]"))
            .unwrap();
        assert_eq!(save.find('›'), Some(chevron_column));
        assert_eq!(display_width(&save[..save.find('[').unwrap()]), 38);
    }

    #[test]
    fn workspace_entry_starts_on_agent_and_hides_global_only_settings() {
        let mut port = FakeSettingsPort {
            global: Settings {
                issue_enabled: true,
                ..Settings::default()
            },
            workspace: Settings {
                issue_enabled: false,
                ..Settings::default()
            },
            ..FakeSettingsPort::default()
        };

        let config =
            Config::load_workspace_with_available_models(&mut port, AvailableAgentModels::all());

        assert_eq!(config.scope(), SettingsScope::Workspace);
        assert_eq!(config.field(), Field::DefaultModel);
        assert!(!config.settings().issue_enabled);
        let frame = render(24, 80, &config).join("\n");
        assert!(frame.contains("Agent"));
        assert!(frame.contains("Issue"));
        assert!(frame.contains("Memory"));
        assert!(!frame.contains("Scope:"));
        assert!(!frame.contains("Theme"));
        assert!(!frame.contains("Modal mode"));
    }

    #[test]
    fn workspace_navigation_wraps_its_three_settings_and_skips_missing_agents() {
        let mut port = FakeSettingsPort::default();
        let mut config =
            Config::load_workspace_with_available_models(&mut port, AvailableAgentModels::all());
        config.previous_field();
        assert_eq!(config.field(), Field::Save);
        config.previous_field();
        assert_eq!(config.field(), Field::Memory);
        config.previous_field();
        assert_eq!(config.field(), Field::Issue);
        config.next_field();
        assert_eq!(config.field(), Field::Memory);
        config.next_field();
        assert_eq!(config.field(), Field::Save);
        config.next_field();
        assert_eq!(config.field(), Field::DefaultModel);

        let mut without_agents = Config::load_workspace_with_available_models(
            &mut port,
            AvailableAgentModels::new(false, false),
        );
        assert_eq!(without_agents.field(), Field::Issue);
        without_agents.previous_field();
        assert_eq!(without_agents.field(), Field::Save);
        without_agents.next_field();
        assert_eq!(without_agents.field(), Field::Issue);

        // Defensive normalization keeps an externally restored stale focus
        // inside the rows visible for Workspace Config.
        without_agents.field = Field::Theme;
        without_agents.next_field();
        assert_eq!(without_agents.field(), Field::Issue);
        without_agents.field = Field::ModalSelectionMode;
        without_agents.previous_field();
        assert_eq!(without_agents.field(), Field::Save);
    }

    #[test]
    fn workspace_config_renders_over_the_home_frame() {
        let mut port = FakeSettingsPort::default();
        let config =
            Config::load_workspace_with_available_models(&mut port, AvailableAgentModels::all());
        let base = (0..24)
            .map(|row| format!("home background {row}"))
            .collect::<Vec<_>>();

        let frame = render_over(24, 80, &base, &config);
        let plain = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        let joined = plain.join("\n");

        assert_eq!(frame.len(), 24);
        assert!(joined.contains("home background 0"));
        assert!(joined.contains("Config"));
        assert!(joined.contains("Agent"));
        assert!(!joined.contains("Scope:"));
        assert!(!joined.contains("Theme"));
        assert!(joined.contains("Esc: back"));
        assert!(plain.iter().all(|line| display_width(line) <= 80));

        let top = plain
            .iter()
            .position(|line| line.contains("Config"))
            .unwrap();
        let first_body = &plain[top + 1];
        let left_border = first_body.find('│').unwrap();
        let right_border = first_body.rfind('│').unwrap();
        assert!(
            first_body[left_border + '│'.len_utf8()..right_border]
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn load_error_and_workspace_draft_are_rendered_without_losing_the_form() {
        let mut port = FakeSettingsPort {
            fail_read: Some(SettingsScope::Workspace),
            workspace: Settings::default(),
            ..FakeSettingsPort::default()
        };
        let mut config =
            Config::load_workspace_with_available_models(&mut port, AvailableAgentModels::all());

        assert_eq!(config.notice(), Some("Load failed: settings unavailable"));
        let error_frame = render(24, 80, &config).join("\n");
        assert!(error_frame.contains("Load failed: settings unavailable"));
        config.next_field();
        config.cycle_issue_enabled();
        let frame = render(24, 80, &config).join("\n");

        assert!(!frame.contains("Scope:"));
        assert!(frame.contains("Issue") && frame.contains("off"));
        assert!(frame.contains('●'));
    }

    #[test]
    fn save_is_selectable_only_with_an_unsaved_change() {
        let mut port = FakeSettingsPort::default();
        let mut config = Config::load(&mut port);
        config.next_field();
        config.next_field();
        config.next_field();
        config.next_field();
        config.next_field();
        assert_eq!(config.field(), Field::Save);
        assert!(!config.can_save());

        config.previous_field();
        config.previous_field();
        config.previous_field();
        config.previous_field();
        config.cycle_modal_selection_mode();
        config.cycle_modal_selection_mode();
        config.cycle_selected(true);
        assert_eq!(
            config.settings().modal_selection_mode,
            ModalSelectionMode::Prompt
        );
        config.next_field();
        config.next_field();
        config.next_field();
        config.next_field();
        assert!(config.can_save());
        assert!(config.begin_save());
        assert!(config.commit_save(&mut port));
        assert_eq!(config.notice(), None);
        assert!(!config.is_dirty());
        assert!(render(24, 80, &config).join("\n").contains("[ done ]"));
    }

    #[test]
    fn field_navigation_wraps_and_save_refuses_a_clean_draft() {
        let mut port = FakeSettingsPort::default();
        let mut config = Config::load(&mut port);
        config.previous_field();
        assert_eq!(config.field(), Field::Save);
        assert!(!config.cycle_selected(true));
        assert!(!config.begin_save());

        config.previous_field();
        assert_eq!(config.field(), Field::Memory);
        config.previous_field();
        assert_eq!(config.field(), Field::Issue);
        config.previous_field();
        assert_eq!(config.field(), Field::DefaultModel);
        config.previous_field();
        assert_eq!(config.field(), Field::ModalSelectionMode);
        config.previous_field();
        assert_eq!(config.field(), Field::Theme);
        config.next_field();
        config.next_field();
        config.next_field();
        config.next_field();
        config.next_field();
        config.next_field();
        assert_eq!(config.field(), Field::Theme);
    }

    #[test]
    fn default_model_cycles_and_is_saved_with_the_global_settings() {
        let mut port = FakeSettingsPort::default();
        let mut config = Config::load(&mut port);
        config.next_field();
        config.next_field();
        assert_eq!(config.field(), Field::DefaultModel);
        config.cycle_selected(true);
        assert_eq!(config.settings().default_model, DefaultModel::Claude);
        assert!(render(24, 80, &config).join("\n").contains("Claude"));
        config.cycle_selected(true);
        assert_eq!(config.settings().default_model, DefaultModel::OpenAi);
        config.cycle_selected(true);
        assert_eq!(config.settings().default_model, DefaultModel::Claude);
        config.next_field();
        config.next_field();
        config.next_field();
        assert!(config.begin_save());
        assert!(config.commit_save(&mut port));
        assert_eq!(port.global.default_model, DefaultModel::Claude);
    }

    #[test]
    fn agent_model_uses_only_the_available_cli() {
        let mut port = FakeSettingsPort {
            global: Settings {
                default_model: DefaultModel::OpenAi,
                ..Settings::default()
            },
            workspace: Settings {
                default_model: DefaultModel::Claude,
                ..Settings::default()
            },
            ..FakeSettingsPort::default()
        };
        let mut config =
            Config::load_with_available_models(&mut port, AvailableAgentModels::new(true, false));

        assert_eq!(config.settings().default_model, DefaultModel::Claude);
        assert!(config.is_dirty());
        assert!(render(24, 80, &config).join("\n").contains("Claude"));
        config.cycle_default_model();
        assert_eq!(config.settings().default_model, DefaultModel::Claude);

        let mut open_ai_only =
            Config::load_with_available_models(&mut port, AvailableAgentModels::new(false, true));
        assert_eq!(open_ai_only.settings().default_model, DefaultModel::OpenAi);
        open_ai_only.cycle_default_model();
        assert_eq!(open_ai_only.settings().default_model, DefaultModel::OpenAi);

        port.global.default_model = DefaultModel::Claude;
        let claude_saved =
            Config::load_with_available_models(&mut port, AvailableAgentModels::new(true, false));
        assert_eq!(claude_saved.settings().default_model, DefaultModel::Claude);
        assert!(!claude_saved.is_dirty());
    }

    #[test]
    fn agent_model_is_dimmed_and_skipped_when_no_cli_is_available() {
        let mut port = FakeSettingsPort::default();
        let mut config =
            Config::load_with_available_models(&mut port, AvailableAgentModels::new(false, false));

        let frame = render(24, 80, &config).join("\n");
        assert!(frame.contains("Agent") && frame.contains("< none   >"));
        assert!(frame.contains("\u{1b}[2m"));
        config.cycle_default_model();
        assert_eq!(config.settings().default_model, DefaultModel::OpenAi);
        config.next_field();
        config.next_field();
        assert_eq!(config.field(), Field::Issue);
        config.previous_field();
        assert_eq!(config.field(), Field::ModalSelectionMode);
    }

    #[test]
    fn issue_and_memory_availability_toggle_independently() {
        let mut port = FakeSettingsPort::default();
        let mut config = Config::load(&mut port);
        config.next_field();
        config.next_field();
        config.next_field();
        assert_eq!(config.field(), Field::Issue);
        assert!(config.cycle_selected(true));
        assert!(!config.settings().issue_enabled);
        assert!(config.settings().memory_enabled);

        config.next_field();
        assert_eq!(config.field(), Field::Memory);
        assert!(config.cycle_selected(false));
        assert!(!config.settings().memory_enabled);
        let frame = render(24, 80, &config).join("\n");
        assert!(frame.contains("Issue") && frame.contains("off"));
        assert!(frame.contains("Memory") && frame.contains("off"));

        config.next_field();
        assert!(config.begin_save());
        assert!(config.commit_save(&mut port));
        assert!(!port.global.issue_enabled);
        assert!(!port.global.memory_enabled);
    }

    /// Drive the Save row to the dirty state used by the phase tests.
    fn dirty_on_save_row(port: &mut FakeSettingsPort) -> Config {
        let mut config = Config::load(port);
        config.cycle_theme(true);
        config.next_field();
        config.next_field();
        config.next_field();
        config.next_field();
        config.next_field();
        assert_eq!(config.field(), Field::Save);
        assert!(config.can_save());
        config
    }

    #[test]
    fn save_moves_from_an_animated_wave_to_done() {
        let mut port = FakeSettingsPort::default();
        let mut config = dirty_on_save_row(&mut port);
        assert!(render(24, 80, &config).join("\n").contains("[ Save ]"));

        // begin_save enters the loading phase and clears any earlier notice.
        assert!(config.begin_save());
        assert!(config.is_dirty());
        assert_eq!(config.notice(), None);
        let first_wave = render(24, 80, &config);
        let first_plain = first_wave
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(first_plain.contains("[ Save ]"));
        config.advance_save_animation();
        assert_ne!(first_wave, render(24, 80, &config));

        // commit_save persists, settles to Done, and stops being dirty.
        assert!(config.commit_save(&mut port));
        assert_eq!(config.notice(), None);
        assert!(!config.is_dirty());
        assert!(render(24, 80, &config).join("\n").contains("[ done ]"));
        assert_eq!(port.global.theme, Theme::Dark);
    }

    #[test]
    fn begin_save_is_a_no_op_while_saving_so_a_double_press_saves_once() {
        let mut port = CountingSettingsPort::default();
        let mut config = {
            let mut base = Config::load(&mut port);
            base.cycle_theme(true);
            base.next_field();
            base.next_field();
            base.next_field();
            base.next_field();
            base.next_field();
            base
        };
        assert_eq!(config.field(), Field::Save);

        assert!(config.begin_save());
        // A second Enter while Saving is rejected — no re-trigger, no re-write.
        assert!(!config.begin_save());
        assert!(config.commit_save(&mut port));
        // A press after the save settled cannot re-save the clean draft.
        assert!(!config.begin_save());

        assert_eq!(port.saves, 1);
    }

    #[test]
    fn failed_save_stays_idle_and_dirty_for_retry() {
        let mut port = FakeSettingsPort {
            fail_save: true,
            ..FakeSettingsPort::default()
        };
        let mut config = dirty_on_save_row(&mut port);

        assert!(config.begin_save());
        assert!(!config.commit_save(&mut port));
        assert!(config.is_dirty());
        assert_eq!(config.notice(), Some("Save failed: disk unavailable"));
        // Back in Idle, the button reads Save so the user can retry.
        assert!(render(24, 80, &config).join("\n").contains("[ Save ]"));

        port.fail_save = false;
        assert!(config.begin_save());
        assert!(config.commit_save(&mut port));
        assert!(!config.is_dirty());
        assert!(render(24, 80, &config).join("\n").contains("[ done ]"));
    }

    #[test]
    fn reset_save_clears_the_confirmation_for_the_next_visit() {
        let mut port = FakeSettingsPort::default();
        let mut config = dirty_on_save_row(&mut port);
        assert!(config.begin_save());
        assert!(config.commit_save(&mut port));
        assert_eq!(config.notice(), None);

        config.reset_save();
        assert_eq!(config.notice(), None);
        assert!(render(24, 80, &config).join("\n").contains("[ Save ]"));
    }

    #[test]
    fn commit_save_without_a_dirty_draft_is_a_no_op() {
        let mut port = FakeSettingsPort::default();
        let mut config = Config::load(&mut port);
        assert!(!config.commit_save(&mut port));
        assert_eq!(config.notice(), None);
    }
}
