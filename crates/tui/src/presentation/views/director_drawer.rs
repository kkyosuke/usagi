//! Director mode drawer shell.
//!
//! This view owns only presentation and geometry. It does not inventory,
//! launch, resume, attach, or forward input to an Agent runtime. The controller
//! supplies the installed CLI picker projection and the runtime supplies
//! conversation/terminal rows.

use crate::presentation::theme::{Role, Style};
use crate::presentation::views::work_run::{WorkRunFreshness, WorkRunProgress, WorkRunProjection};
use crate::presentation::views::workspace::TerminalViewProjection;
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::terminal_selection::TerminalPoint;
use crate::usecase::application::work_run_control::WorkRunControlMode;
use usagi_core::domain::supervisor::{
    EscalationDecision, SupervisorRunId, SupervisorRunQuery, SupervisorRunState, TaskState,
};

/// Desired lower bound while the drawer can coexist with a visible background.
pub const MIN_DRAWER_WIDTH: usize = 56;
/// Maximum drawer width on wide terminals.
pub const MAX_DRAWER_WIDTH: usize = 96;
/// Minimum background strip kept visible beside a non-full-width drawer.
const MIN_BACKGROUND_WIDTH: usize = 24;
/// Material Design robot glyph from the repository's Nerd Font vocabulary.
///
/// Like the existing CPU/memory/mode glyphs, unsupported fonts may render a
/// missing-glyph cell; Unicode-width clipping keeps layout and hit-testing safe.
pub const DIRECTOR_ICON: char = '♛';
/// Rows of drawer chrome the New picker's candidate rows never get: the Home
/// header row above the drawer, the panel's two borders and two vertical padding
/// rows, the conversation selector, its separator, and the footer hint.
const PICKER_CHROME_ROWS: usize = 8;
const _: () = assert!(
    PICKER_CHROME_ROWS == crate::usecase::application::controller::DIRECTOR_PICKER_CHROME_ROWS
);
/// Goal label, input, and provider label above the candidate rows.
const GOAL_COMPOSER_EXTRA_ROWS: usize = 3;
const GOAL_COMPOSER_CHROME_ROWS: usize = PICKER_CHROME_ROWS + GOAL_COMPOSER_EXTRA_ROWS;
const _: () = assert!(
    GOAL_COMPOSER_CHROME_ROWS
        == crate::usecase::application::controller::DIRECTOR_GOAL_COMPOSER_CHROME_ROWS
);
/// Footer shown while the picker has room for the highlighted candidate.
const PICKER_HINT: &str = "↑↓: select  ·  Enter: launch  ·  Esc: cancel";
/// Footer shown when the drawer cannot draw a single candidate row. The reducer
/// gates Enter on the same capacity, so this states the only way forward.
const PICKER_TOO_SHORT_HINT: &str = "Terminal too short to choose  ·  Esc: cancel";

/// One presentation-safe conversation choice.
///
/// Inventory identity remains outside the view. A later controller/runtime may
/// associate this display value with its own stable key and feed the selected
/// projection into this shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorConversation {
    pub label: String,
    pub selected: bool,
}

/// One safe row in the Director's organization overview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectorOrganizationRow {
    pub depth: usize,
    pub label: String,
    pub status: String,
}

/// Presentation-safe state of the drawer's explicit `New` chooser.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DirectorNewProjection {
    /// The chooser is closed and New may be opened.
    #[default]
    Ready,
    /// Installed CLI labels in deterministic order, with one highlighted row.
    Choosing {
        candidates: Vec<String>,
        selected: usize,
    },
    /// Goal-driven New: one objective plus the provider that will own it.
    GoalComposer {
        candidates: Vec<String>,
        selected: usize,
        goal: String,
    },
    /// No supported Agent CLI is installed.
    Empty,
    /// One confirmed root launch is fenced until its matching completion.
    Launching,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRunControlProjection {
    pub mode: WorkRunControlMode,
    pub selected: Option<SupervisorRunId>,
    pub decision: EscalationDecision,
    pub feedback: Option<String>,
}

impl Default for WorkRunControlProjection {
    fn default() -> Self {
        Self {
            mode: WorkRunControlMode::Closed,
            selected: None,
            decision: EscalationDecision::Resume,
            feedback: None,
        }
    }
}

/// Pure material accepted by the drawer renderer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectorDrawerProjection {
    /// Whether this drawer represents the opt-in objective-driven workflow.
    pub goal_driven: bool,
    pub conversations: Vec<DirectorConversation>,
    pub organization: Vec<DirectorOrganizationRow>,
    pub terminal_view: Option<TerminalViewProjection>,
    /// Safe reason for a selected interrupted conversation, outside PTY output.
    pub interrupted_detail: Option<String>,
    /// Drawer feedback used when the selected conversation has no live terminal.
    pub feedback: Option<String>,
    pub new: DirectorNewProjection,
    /// Daemon-owned, redaction-safe progress. The shared projection owns
    /// ordering, aggregation, and observation freshness for every surface.
    pub work_runs: WorkRunProjection,
    /// Explicit, confirm-before-mutate Work Run interaction.
    pub work_run_control: WorkRunControlProjection,
}

impl DirectorDrawerProjection {
    #[must_use]
    pub fn with_work_runs(mut self, runs: WorkRunProjection) -> Self {
        self.work_runs = runs;
        self
    }

    #[must_use]
    pub fn with_work_run_control(mut self, control: WorkRunControlProjection) -> Self {
        self.work_run_control = control;
        self
    }
}

/// Right-anchored drawer rectangle in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectorDrawerGeometry {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub full_width: bool,
}

/// Future Agent terminal viewport inside the drawer, independent from the
/// managed-session Closeup pane's viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectorTerminalViewport {
    pub rows: usize,
    pub cols: usize,
}

/// Compute the drawer rectangle from terminal geometry.
///
/// The normal width is 60%, clamped to 56…96 columns. If keeping that minimum
/// would leave less than 24 columns of background, the drawer becomes full
/// width. A zero terminal dimension follows the TUI-wide 80×24 normalization.
#[must_use]
pub fn geometry(raw_height: usize, raw_width: usize) -> DirectorDrawerGeometry {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let desired = width.saturating_mul(3) / 5;
    let coexist_width = desired.clamp(MIN_DRAWER_WIDTH, MAX_DRAWER_WIDTH).min(width);
    let full_width = width.saturating_sub(coexist_width) < MIN_BACKGROUND_WIDTH;
    let drawer_width = if full_width { width } else { coexist_width };
    DirectorDrawerGeometry {
        left: width.saturating_sub(drawer_width),
        // Home's top header remains visible and owns the drawer toggle button.
        top: 1.min(height),
        width: drawer_width,
        height: height.saturating_sub(1),
        full_width,
    }
}

/// Compute the terminal viewport reserved inside the drawer.
///
/// This intentionally does not call `workspace::terminal_viewport`: the drawer
/// has its own border, selector, breathing row, and footer chrome. Runtime work
/// can therefore resize a director terminal without confusing it with
/// the managed-session Closeup terminal.
#[must_use]
pub fn terminal_viewport(raw_height: usize, raw_width: usize) -> DirectorTerminalViewport {
    let drawer = geometry(raw_height, raw_width);
    DirectorTerminalViewport {
        // top/bottom borders + vertical padding + selector + separator + footer
        rows: drawer.height.saturating_sub(7),
        // left/right borders and one cell of padding on both sides
        cols: drawer.width.saturating_sub(4),
    }
}

/// Candidate rows the `New` picker can draw at this terminal size.
///
/// The picker's viewport follows the selection, so a non-zero capacity always
/// shows the highlighted CLI. A zero capacity draws no candidate at all, which
/// is why the reducer refuses to launch from it.
#[must_use]
pub fn picker_capacity(raw_height: usize, raw_width: usize) -> usize {
    let (height, _) = widgets::normalize_size(raw_height, raw_width);
    height.saturating_sub(PICKER_CHROME_ROWS)
}

/// Provider rows visible inside Goal Composer at this terminal size.
#[must_use]
pub fn goal_composer_picker_capacity(raw_height: usize, raw_width: usize) -> usize {
    let (height, _) = widgets::normalize_size(raw_height, raw_width);
    height.saturating_sub(GOAL_COMPOSER_CHROME_ROWS)
}

/// Map a frame-cell pointer into the retained root Agent terminal viewport.
#[must_use]
pub fn terminal_point_at(
    raw_height: usize,
    raw_width: usize,
    rows_len: usize,
    scroll: usize,
    column: u16,
    row: u16,
) -> Option<TerminalPoint> {
    let drawer = geometry(raw_height, raw_width);
    let viewport = terminal_viewport(raw_height, raw_width);
    let column = usize::from(column).checked_sub(drawer.left.saturating_add(2))?;
    let content_row = usize::from(row).checked_sub(drawer.top.saturating_add(4))?;
    if column >= viewport.cols || content_row >= viewport.rows {
        return None;
    }
    let start = widgets::live_terminal::window_start(rows_len, viewport.rows, scroll);
    Some(TerminalPoint {
        row: start + content_row,
        column,
    })
}

/// Whether a frame-cell press lands on the drawer's right-aligned `New`
/// affordance. The launch-in-progress label is inert.
#[must_use]
pub fn new_button_at(
    raw_height: usize,
    raw_width: usize,
    column: u16,
    row: u16,
    launching: bool,
) -> bool {
    if launching {
        return false;
    }
    let drawer = geometry(raw_height, raw_width);
    if usize::from(row) != drawer.top.saturating_add(2) {
        return false;
    }
    let right = drawer.left.saturating_add(drawer.width).saturating_sub(2);
    let left = right.saturating_sub(widgets::display_width("[ New ]"));
    (left..right).contains(&usize::from(column))
}

/// Render the drawer over a dimmed Home frame.
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    projection: &DirectorDrawerProjection,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let drawer = geometry(raw_height, raw_width);
    let mut frame = (0..height)
        .map(|row| {
            let line = modal::columns(base.get(row).map_or("", String::as_str), 0, width);
            if row == 0 {
                line
            } else {
                widgets::dim_ansi(&line)
            }
        })
        .collect::<Vec<_>>();

    if drawer.width < 4 || drawer.height == 0 {
        return frame;
    }

    let inner_width = drawer.width.saturating_sub(4);
    // `modal::boxed` adds the top/bottom borders and one padding row inside
    // each border. Reserve all four rows so the bottom border stays on-screen.
    let body_height = drawer.height.saturating_sub(4);
    let body = drawer_body(inner_width, body_height, projection);
    let title = Role::Accent
        .style()
        .bold()
        .paint(&format!("{DIRECTOR_ICON} Director"));
    let panel = modal::boxed(&title, inner_width, &body);

    // The panel is `drawer.height` rows and is anchored at `drawer.top`, so it
    // always fits inside the `frame.len()` == height rows built above. Bound the
    // splice by the remaining band so the row index can never leave the frame.
    let band = frame.len().saturating_sub(drawer.top);
    for (offset, panel_line) in panel.iter().take(band).enumerate() {
        let row = drawer.top + offset;
        let background = &frame[row];
        let prefix = modal::columns(background, 0, drawer.left);
        frame[row] = format!("{prefix}{panel_line}\u{1b}[0m");
    }
    frame
}

fn drawer_body(width: usize, height: usize, projection: &DirectorDrawerProjection) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let mut rows = vec![selector_row(width, projection)];
    if height > 1 {
        rows.push(Style::new().dim().paint(&"─".repeat(width)));
    }

    if let DirectorNewProjection::Choosing {
        candidates,
        selected,
    } = &projection.new
    {
        return provider_picker_body(width, height, rows, candidates, *selected);
    }
    if let DirectorNewProjection::GoalComposer {
        candidates,
        selected,
        goal,
    } = &projection.new
    {
        return goal_composer_body(width, height, rows, candidates, *selected, goal);
    }

    if projection.work_run_control.mode != WorkRunControlMode::Closed {
        return work_run_control_body(width, height, rows, projection);
    }

    if let Some(run) = projection.work_runs.primary() {
        rows.extend(work_run_rows(width, run, projection.work_runs.freshness()));
    } else if projection.work_runs.freshness() == WorkRunFreshness::Unavailable {
        rows.push(
            Role::Warning
                .style()
                .bold()
                .paint("Work Run progress unavailable"),
        );
    }

    let footer_hint = if projection.goal_driven {
        "Ctrl-O w: Work Runs/actions · Esc: close"
    } else {
        "Ctrl-O n / New: choose CLI  ·  Esc / Ctrl-O Ctrl-G: close"
    };
    let content_capacity = height.saturating_sub(rows.len() + 1);
    if matches!(projection.new, DirectorNewProjection::Empty) {
        let before = content_capacity.saturating_sub(3) / 2;
        rows.extend(std::iter::repeat_n(String::new(), before));
        if content_capacity > before {
            rows.push(Role::Accent.style().bold().paint("No Agent CLI installed"));
        }
        if content_capacity > before + 1 {
            rows.push(
                Style::new()
                    .dim()
                    .paint("Install claude, codex, or sakana.ai and check Config."),
            );
        }
        if content_capacity > before + 2 {
            rows.push(Style::new().dim().paint("Esc returns to conversations."));
        }
    } else if let Some(view) = &projection.terminal_view {
        rows.extend(widgets::live_terminal::render(
            view,
            width,
            height.saturating_sub(rows.len()),
            content_capacity,
            footer_hint,
        ));
        return rows;
    } else if let Some(detail) = &projection.interrupted_detail {
        rows.push(Style::new().dim().paint(detail));
    } else if !projection.organization.is_empty() {
        rows.push(Role::Accent.style().bold().paint("Organization"));
        for member in &projection.organization {
            let branch = if member.depth == 0 { "" } else { "└─ " };
            rows.push(format!(
                "{}{}{}  {}",
                "  ".repeat(member.depth),
                branch,
                member.label,
                Style::new().dim().paint(&member.status)
            ));
        }
    } else if projection.conversations.is_empty() {
        empty_conversation_rows(&mut rows, projection, content_capacity);
    }
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(
        Style::new()
            .dim()
            .paint(projection.feedback.as_deref().unwrap_or(footer_hint)),
    );
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

fn work_run_control_body(
    width: usize,
    height: usize,
    mut rows: Vec<String>,
    projection: &DirectorDrawerProjection,
) -> Vec<String> {
    let control = &projection.work_run_control;
    rows.push(Role::Accent.style().bold().paint("Work Runs"));
    if projection.work_runs.freshness() == WorkRunFreshness::Unavailable {
        rows.push(
            Role::Warning
                .style()
                .paint("Cached · refresh required before actions"),
        );
    }
    if let Some(feedback) = &control.feedback {
        let style = if control.mode == WorkRunControlMode::Retry {
            Role::Warning.style()
        } else {
            Style::new().dim()
        };
        rows.push(style.paint(feedback));
    }

    match control.mode {
        WorkRunControlMode::Closed => unreachable!("closed control uses the normal drawer"),
        WorkRunControlMode::List => {
            let footer = "↑↓ select · Enter actions · Esc close";
            let capacity = height.saturating_sub(rows.len() + 1);
            let runs = projection.work_runs.runs();
            let selected = control
                .selected
                .and_then(|id| runs.iter().position(|run| run.supervisor_run_id == id))
                .unwrap_or(0);
            let start = selected.saturating_sub(capacity.saturating_sub(1));
            for run in runs.iter().skip(start).take(capacity) {
                let marker = if Some(run.supervisor_run_id) == control.selected {
                    "›"
                } else {
                    " "
                };
                let short_id: String = run.supervisor_run_id.to_string().chars().take(8).collect();
                let progress = WorkRunProgress::from_run(run);
                rows.push(format!(
                    "{marker} #{short_id}  {:<15} {}/{}",
                    run_state_label(run.state),
                    progress.succeeded_tasks,
                    progress.total_tasks
                ));
            }
            if runs.is_empty() && capacity > 0 {
                rows.push(Style::new().dim().paint("No Work Runs yet"));
            }
            rows.truncate(height.saturating_sub(1));
            rows.resize(height.saturating_sub(1), String::new());
            rows.push(Style::new().dim().paint(footer));
        }
        WorkRunControlMode::ConfirmCancel => {
            rows.extend(control_prompt_rows(
                control.selected,
                "Cancel this Work Run and stop its active Agents?",
                "Enter confirm · Esc back",
            ));
            finish_control_rows(&mut rows, height);
        }
        WorkRunControlMode::ResolveEscalation => {
            rows.extend(control_prompt_rows(
                control.selected,
                "Resolve the current decision",
                "↑↓ choose · Enter confirm · Esc back",
            ));
            for (decision, label) in [
                (EscalationDecision::Resume, "Retry work"),
                (EscalationDecision::Cancel, "Cancel run"),
                (EscalationDecision::Fail, "Mark failed"),
            ] {
                let marker = if decision == control.decision {
                    "›"
                } else {
                    " "
                };
                rows.push(format!("{marker} {label}"));
            }
            finish_control_rows(&mut rows, height);
        }
        WorkRunControlMode::Submitting => {
            rows.extend(control_prompt_rows(
                control.selected,
                "Applying the durable action…",
                "Waiting for the daemon · do not repeat",
            ));
            finish_control_rows(&mut rows, height);
        }
        WorkRunControlMode::Retry => {
            rows.extend(control_prompt_rows(
                control.selected,
                "The action outcome is not confirmed",
                "Enter retry same operation · Esc close",
            ));
            finish_control_rows(&mut rows, height);
        }
    }
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

fn control_prompt_rows(
    selected: Option<SupervisorRunId>,
    prompt: &str,
    footer: &str,
) -> Vec<String> {
    let id = selected.map_or_else(
        || "unknown".to_owned(),
        |id| id.to_string().chars().take(8).collect(),
    );
    vec![format!("#{id}"), prompt.to_owned(), footer.to_owned()]
}

fn finish_control_rows(rows: &mut Vec<String>, height: usize) {
    let footer = rows.pop().unwrap_or_default();
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(Style::new().dim().paint(&footer));
}

fn work_run_rows(
    width: usize,
    run: &SupervisorRunQuery,
    freshness: WorkRunFreshness,
) -> Vec<String> {
    let progress = WorkRunProgress::from_run(run);
    let state = run_state_label(run.state);
    let short_id: String = run.supervisor_run_id.to_string().chars().take(8).collect();
    let bar_width = width.saturating_sub(29).clamp(4, 16);
    let bar = crate::presentation::widgets::loading::progress_bar(
        progress.succeeded_tasks,
        progress.total_tasks,
        bar_width,
    );
    let mut rows = vec![
        Role::Accent
            .style()
            .bold()
            .paint(&format!("Active work  #{short_id}  {state}")),
        format!(
            "Progress  {bar}  {}/{} tasks  Agents {}/{}",
            progress.succeeded_tasks,
            progress.total_tasks,
            progress.active_agents,
            progress.max_agents,
        ),
    ];
    if freshness == WorkRunFreshness::Unavailable {
        rows.push(
            Role::Warning
                .style()
                .paint("Stale · last daemon update unavailable"),
        );
    }
    for task in run.tasks.iter().take(5) {
        let icon = match task.state {
            TaskState::Succeeded => "✓",
            TaskState::Dispatched | TaskState::Running => "●",
            TaskState::AwaitingDecision => "!",
            TaskState::Retrying | TaskState::Verifying => "◐",
            TaskState::Failed | TaskState::Blocked => "×",
            TaskState::Cancelled => "−",
            TaskState::Pending | TaskState::Ready => "◌",
        };
        rows.push(format!(
            "{icon} {}  {}",
            task.task_id.0,
            task_state_label(task.state)
        ));
    }
    if run.tasks.len() > 5 {
        rows.push(
            Style::new()
                .dim()
                .paint(&format!("… {} more tasks", run.tasks.len() - 5)),
        );
    }
    let stop = run
        .escalation
        .as_ref()
        .map(|escalation| escalation.reason.as_str())
        .or(run.terminal_reason.as_deref())
        .unwrap_or("—");
    rows.push(format!("Stop reason: {stop}"));
    rows.push(Style::new().dim().paint(&"─".repeat(width)));
    rows
}

const fn run_state_label(state: SupervisorRunState) -> &'static str {
    match state {
        SupervisorRunState::Planning => "Planning",
        SupervisorRunState::Running => "Working",
        SupervisorRunState::WaitingForDecision => "Waiting for you",
        SupervisorRunState::Verifying => "Verifying",
        SupervisorRunState::Succeeded => "Completed",
        SupervisorRunState::Failed => "Failed",
        SupervisorRunState::Cancelled => "Cancelled",
        SupervisorRunState::Escalated => "Needs attention",
    }
}

const fn task_state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Pending => "waiting",
        TaskState::Ready => "ready",
        TaskState::Dispatched => "starting",
        TaskState::Running => "working",
        TaskState::AwaitingDecision => "waiting for you",
        TaskState::Retrying => "retrying",
        TaskState::Verifying => "verifying",
        TaskState::Succeeded => "done",
        TaskState::Failed => "failed",
        TaskState::Cancelled => "cancelled",
        TaskState::Blocked => "blocked",
    }
}

fn empty_conversation_rows(
    rows: &mut Vec<String>,
    projection: &DirectorDrawerProjection,
    content_capacity: usize,
) {
    let before = content_capacity.saturating_sub(3) / 2;
    rows.extend(std::iter::repeat_n(String::new(), before));
    if content_capacity > before {
        rows.push(
            Role::Accent
                .style()
                .bold()
                .paint(if projection.goal_driven {
                    "No Work Runs yet"
                } else {
                    "No conversations yet"
                }),
        );
    }
    if content_capacity > before + 1 {
        rows.push(Style::new().dim().paint(if projection.goal_driven {
            "Work Run output and stop reasons appear here."
        } else {
            "Conversation inventory is not connected."
        }));
    }
    if content_capacity <= before + 2 {
        return;
    }
    let detail = if matches!(projection.new, DirectorNewProjection::Launching) {
        if projection.goal_driven {
            "Waiting for the daemon to start the Work Run."
        } else {
            "Waiting for the daemon to start the conversation."
        }
    } else if projection.goal_driven {
        "Choose New, enter one goal, and start the Work Run."
    } else {
        "Choose New to start a conversation."
    };
    rows.push(Style::new().dim().paint(detail));
}

fn provider_picker_body(
    width: usize,
    height: usize,
    mut rows: Vec<String>,
    candidates: &[String],
    selected: usize,
) -> Vec<String> {
    let content_capacity = height.saturating_sub(rows.len() + 1);
    rows.extend(picker_rows(candidates, selected, content_capacity));
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(Style::new().dim().paint(if content_capacity == 0 {
        PICKER_TOO_SHORT_HINT
    } else {
        PICKER_HINT
    }));
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

fn goal_composer_body(
    width: usize,
    height: usize,
    mut rows: Vec<String>,
    candidates: &[String],
    selected: usize,
    goal: &str,
) -> Vec<String> {
    let content_capacity = height.saturating_sub(rows.len() + 1);
    let provider_capacity = content_capacity.saturating_sub(GOAL_COMPOSER_EXTRA_ROWS);
    if content_capacity > 0 {
        rows.push(Role::Accent.style().bold().paint("Goal"));
    }
    if content_capacity > 1 {
        let caret = widgets::block_caret(goal, goal.len(), &Style::new());
        let available = content_capacity.saturating_sub(3);
        let mut input = widgets::wrap_to_width(&caret, width)
            .into_iter()
            .collect::<Vec<_>>();
        if input.is_empty() {
            input.push(caret);
        }
        rows.extend(
            input
                .into_iter()
                .rev()
                .take(available)
                .collect::<Vec<_>>()
                .into_iter()
                .rev(),
        );
    }
    if content_capacity > 2 {
        rows.push(Style::new().dim().paint("Provider (↑↓)"));
    }
    if content_capacity > 3 {
        rows.extend(picker_rows(candidates, selected, provider_capacity));
    }
    rows.truncate(height.saturating_sub(1));
    rows.resize(height.saturating_sub(1), String::new());
    rows.push(Style::new().dim().paint(if provider_capacity == 0 {
        "Terminal too short to choose provider  ·  Esc: cancel"
    } else if goal.trim().is_empty() {
        "Type a goal  ·  Enter: start when ready  ·  Esc: cancel"
    } else {
        "Enter: start Work Run  ·  ↑↓: provider  ·  Esc: cancel"
    }));
    rows.into_iter()
        .map(|row| widgets::clip_to_width(&row, width))
        .collect()
}

/// The picker's candidate rows for a `capacity`-row content area.
///
/// The window follows the selection, so the highlighted CLI is on screen
/// whenever there is a content row at all to put it on. A zero capacity draws
/// nothing and the footer says why; the reducer refuses the launch at the same
/// capacity, so `Enter` can never confirm an off-screen candidate.
fn picker_rows(candidates: &[String], selected: usize, capacity: usize) -> Vec<String> {
    let rows = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let marker = if index == selected { "›" } else { " " };
            let line = format!("{marker} {candidate}");
            if index == selected {
                Role::Accent.style().bold().paint(&line)
            } else {
                line
            }
        })
        .collect::<Vec<_>>();
    modal::bounded_list_rows(&rows, selected, capacity)
}

fn selector_row(width: usize, projection: &DirectorDrawerProjection) -> String {
    let selected = projection
        .conversations
        .iter()
        .find(|conversation| conversation.selected)
        .or_else(|| projection.conversations.first())
        .map_or("No conversations", |conversation| {
            conversation.label.as_str()
        });
    let new = if matches!(projection.new, DirectorNewProjection::Launching) {
        Style::new().dim().paint("[ Starting… ]")
    } else {
        Role::Accent.style().bold().paint("[ New ]")
    };
    let subject = if projection.goal_driven {
        "Work Run"
    } else {
        "Conversation"
    };
    let prefix = format!("{subject}  [{selected}]");
    let reserved = widgets::display_width(&new).saturating_add(2);
    let prefix = widgets::clip_to_width(&prefix, width.saturating_sub(reserved));
    let gap = width
        .saturating_sub(widgets::display_width(&prefix))
        .saturating_sub(widgets::display_width(&new));
    format!("{prefix}{}{new}", " ".repeat(gap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::widgets::{display_width, strip_ansi};
    use chrono::Utc;
    use std::collections::BTreeSet;
    use usagi_core::domain::id::OperationId;
    use usagi_core::domain::supervisor::{
        ArtifactContract, EscalationRecord, ExecutionPolicy, SupervisorRunId, TaskId, TaskQuery,
    };

    fn work_run() -> SupervisorRunQuery {
        SupervisorRunQuery {
            supervisor_run_id: SupervisorRunId::new(),
            state_revision: 3,
            state: SupervisorRunState::Running,
            terminal_at: None,
            terminal_reason: None,
            policy: ExecutionPolicy::default(),
            escalation: None,
            tasks: [TaskState::Succeeded, TaskState::Running, TaskState::Pending]
                .into_iter()
                .enumerate()
                .map(|(index, state)| TaskQuery {
                    task_id: TaskId::new(format!("task-{index}")).unwrap(),
                    parent_task_id: None,
                    dependencies: BTreeSet::new(),
                    instruction_digest: format!("digest-{index}"),
                    required_artifact_contract: ArtifactContract::default(),
                    attempt: 1,
                    generation: 1,
                    assigned_dispatch_run: None,
                    verification_attempt: 0,
                    verification_retry_at: None,
                    state,
                })
                .collect(),
            provenance: Vec::new(),
        }
    }

    #[test]
    fn geometry_clamps_normal_boundary_and_wide_sizes() {
        assert_eq!(
            geometry(24, 100),
            DirectorDrawerGeometry {
                left: 40,
                top: 1,
                width: 60,
                height: 23,
                full_width: false,
            }
        );
        assert_eq!(geometry(24, 80).width, MIN_DRAWER_WIDTH);
        assert!(!geometry(24, 80).full_width);
        assert_eq!(geometry(24, 200).width, MAX_DRAWER_WIDTH);
    }

    #[test]
    fn narrow_and_zero_geometry_use_safe_full_width_fallbacks() {
        let narrow = geometry(5, 79);
        assert_eq!(narrow.left, 0);
        assert_eq!(narrow.width, 79);
        assert!(narrow.full_width);

        let zero = geometry(0, 0);
        assert_eq!(zero, geometry(24, 80));
        assert_eq!(
            terminal_viewport(0, 0),
            DirectorTerminalViewport { rows: 16, cols: 52 }
        );
        assert_eq!(
            terminal_viewport(1, 1),
            DirectorTerminalViewport { rows: 0, cols: 0 }
        );
    }

    #[test]
    fn terminal_viewport_is_independent_from_the_closeup_right_pane() {
        assert_eq!(
            terminal_viewport(24, 100),
            DirectorTerminalViewport { rows: 16, cols: 56 }
        );
        assert_ne!(
            (
                terminal_viewport(24, 100).rows,
                terminal_viewport(24, 100).cols
            ),
            crate::presentation::views::workspace::terminal_viewport(24, 100)
        );
    }

    #[test]
    fn terminal_pointer_mapping_uses_drawer_content_geometry() {
        let drawer = geometry(24, 100);
        assert_eq!(
            terminal_point_at(
                24,
                100,
                30,
                0,
                u16::try_from(drawer.left + 2).unwrap(),
                u16::try_from(drawer.top + 4).unwrap(),
            ),
            Some(TerminalPoint { row: 14, column: 0 })
        );
        assert_eq!(terminal_point_at(24, 100, 30, 0, 0, 0), None);
        assert_eq!(
            terminal_point_at(
                24,
                100,
                30,
                0,
                u16::try_from(drawer.left + 2).unwrap(),
                u16::try_from(drawer.top + 4 + terminal_viewport(24, 100).rows).unwrap(),
            ),
            None
        );
    }

    #[test]
    fn new_button_hit_test_matches_selector_row_and_is_inert_while_launching() {
        let drawer = geometry(24, 100);
        let row = u16::try_from(drawer.top + 2).unwrap();
        let right = u16::try_from(drawer.left + drawer.width - 3).unwrap();
        assert!(new_button_at(24, 100, right, row, false));
        assert!(!new_button_at(24, 100, right, row, true));
        assert!(!new_button_at(24, 100, 0, row, false));
        assert!(!new_button_at(24, 100, right, row + 1, false));
    }

    #[test]
    fn empty_drawer_dims_background_and_renders_new_affordance() {
        let base = (0..24)
            .map(|row| format!("background {row}"))
            .collect::<Vec<_>>();
        let frame = render_over(24, 100, &base, &DirectorDrawerProjection::default());
        assert_eq!(frame.len(), 24);
        assert!(frame.iter().all(|line| display_width(line) == 100));
        let text = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&format!("{DIRECTOR_ICON} Director")));
        assert!(text.contains("Conversation  [No conversations]"));
        assert!(text.contains("[ New ]"));
        assert!(text.contains("No conversations yet"));
        assert!(text.contains("Ctrl-O n / New: choose CLI"));
        assert!(frame[1].contains("\u{1b}[2m"));
        assert!(!frame[0].contains("\u{1b}[2m"));
        assert!(strip_ansi(&frame[23]).contains('└'));
        assert!(strip_ansi(&frame[23]).ends_with('┘'));
    }

    #[test]
    fn empty_drawer_omits_detail_when_height_is_too_small() {
        let body = drawer_body(40, 3, &DirectorDrawerProjection::default());
        let text = body
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(body.len(), 3);
        assert!(!text.contains("Choose New to start a conversation."));
    }

    #[test]
    fn organization_projection_renders_depth_and_status() {
        let projection = DirectorDrawerProjection {
            organization: vec![
                DirectorOrganizationRow {
                    depth: 0,
                    label: "Director".into(),
                    status: "active".into(),
                },
                DirectorOrganizationRow {
                    depth: 1,
                    label: "triage (manager)".into(),
                    status: "waiting".into(),
                },
                DirectorOrganizationRow {
                    depth: 2,
                    label: "implement (executor)".into(),
                    status: "stopped".into(),
                },
            ],
            ..DirectorDrawerProjection::default()
        };
        let body = drawer_body(52, 10, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("Organization"));
        assert!(body.contains("triage (manager)"));
        assert!(body.contains("implement (executor)"));
        assert!(body.contains("stopped"));
    }

    #[test]
    fn goal_composer_renders_objective_provider_and_terminal_condition() {
        let projection = DirectorDrawerProjection {
            goal_driven: true,
            new: DirectorNewProjection::GoalComposer {
                candidates: vec!["claude".into(), "codex".into()],
                selected: 1,
                goal: "Implement the work run".into(),
            },
            ..DirectorDrawerProjection::default()
        };
        let body = drawer_body(52, 12, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("Goal"));
        assert!(body.contains("Implement the work run"));
        assert!(body.contains("› codex"));
        assert!(body.contains("Enter: start Work Run"));
    }

    #[test]
    fn goal_driven_drawer_names_the_run_and_exposes_the_control_surface() {
        let projection = DirectorDrawerProjection {
            goal_driven: true,
            ..DirectorDrawerProjection::default()
        };
        let body = drawer_body(64, 8, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(body.contains("Work Run  [No conversations]"));
        assert!(body.contains("Work Run output and stop reasons appear here."));
        assert!(body.contains("Ctrl-O w: Work Runs/actions"));
        assert!(!body.contains("Choose New to start a conversation."));
    }

    #[test]
    fn director_drawer_renders_daemon_owned_task_progress_in_both_modes() {
        for goal_driven in [false, true] {
            let projection = DirectorDrawerProjection {
                goal_driven,
                work_runs: WorkRunProjection::fresh(vec![work_run()]),
                ..DirectorDrawerProjection::default()
            };
            let body = drawer_body(72, 16, &projection)
                .into_iter()
                .map(|row| strip_ansi(&row))
                .collect::<Vec<_>>()
                .join("\n");

            assert!(body.contains("Active work"));
            assert!(body.contains("1/3 tasks"));
            assert!(body.contains("Agents 1/4"));
            assert!(body.contains("✓ task-0  done"));
            assert!(body.contains("● task-1  working"));
            assert!(body.contains("Stop reason: —"));
        }
    }

    #[test]
    fn director_labels_cached_work_runs_and_unavailable_empty_observations() {
        let cached = DirectorDrawerProjection {
            work_runs: WorkRunProjection::fresh(vec![work_run()]).unavailable(),
            ..DirectorDrawerProjection::default()
        };
        let cached = drawer_body(72, 16, &cached)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(cached.contains("Stale · last daemon update unavailable"));
        assert!(cached.contains("1/3 tasks"));

        let unavailable = DirectorDrawerProjection {
            work_runs: WorkRunProjection::default().unavailable(),
            ..DirectorDrawerProjection::default()
        };
        let unavailable = drawer_body(72, 10, &unavailable)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(unavailable.contains("Work Run progress unavailable"));
        assert!(!unavailable.contains("Active work"));
    }

    #[test]
    fn work_run_projection_covers_every_priority_state_and_task_badge() {
        let run_states = [
            SupervisorRunState::Planning,
            SupervisorRunState::Running,
            SupervisorRunState::WaitingForDecision,
            SupervisorRunState::Verifying,
            SupervisorRunState::Succeeded,
            SupervisorRunState::Failed,
            SupervisorRunState::Cancelled,
            SupervisorRunState::Escalated,
        ];
        let projection =
            DirectorDrawerProjection::default().with_work_runs(WorkRunProjection::fresh(
                run_states
                    .into_iter()
                    .map(|state| {
                        let mut run = work_run();
                        run.state = state;
                        run
                    })
                    .collect(),
            ));
        let ordered = projection
            .work_runs
            .runs()
            .iter()
            .map(|run| run.state)
            .collect::<Vec<_>>();
        assert!(ordered[..2].iter().all(|state| matches!(
            state,
            SupervisorRunState::WaitingForDecision | SupervisorRunState::Escalated
        )));
        assert_eq!(ordered[2], SupervisorRunState::Failed);
        assert!(ordered[3..5].iter().all(|state| matches!(
            state,
            SupervisorRunState::Running | SupervisorRunState::Verifying
        )));
        assert_eq!(ordered[5], SupervisorRunState::Planning);
        assert!(ordered[6..].iter().all(|state| matches!(
            state,
            SupervisorRunState::Succeeded | SupervisorRunState::Cancelled
        )));
        assert_eq!(
            run_states.map(run_state_label),
            [
                "Planning",
                "Working",
                "Waiting for you",
                "Verifying",
                "Completed",
                "Failed",
                "Cancelled",
                "Needs attention",
            ]
        );

        let task_states = [
            TaskState::Pending,
            TaskState::Ready,
            TaskState::Dispatched,
            TaskState::Running,
            TaskState::AwaitingDecision,
            TaskState::Retrying,
            TaskState::Verifying,
            TaskState::Succeeded,
            TaskState::Failed,
            TaskState::Cancelled,
            TaskState::Blocked,
        ];
        for state in task_states {
            let mut run = work_run();
            run.tasks.truncate(1);
            run.tasks[0].state = state;
            assert!(
                work_run_rows(60, &run, WorkRunFreshness::Fresh)
                    .iter()
                    .any(|row| { strip_ansi(row).contains(task_state_label(state)) })
            );
        }

        let mut verbose = work_run();
        let template = verbose.tasks[0].clone();
        verbose.tasks = (0..7)
            .map(|index| TaskQuery {
                task_id: TaskId::new(format!("many-{index}")).unwrap(),
                ..template.clone()
            })
            .collect();
        verbose.escalation = Some(EscalationRecord {
            escalation_id: OperationId::new(),
            reason: "choose a recovery".into(),
            blocking_task_id: None,
            safe_evidence: "bounded".into(),
            choices: vec!["resume".into()],
            created_at: Utc::now(),
        });
        let rows = work_run_rows(60, &verbose, WorkRunFreshness::Fresh).join("\n");
        assert!(rows.contains("… 2 more tasks"));
        assert!(rows.contains("Stop reason: choose a recovery"));
    }

    #[test]
    fn work_run_control_renders_selection_confirmation_decision_and_retry() {
        let run = work_run();
        let selected = run.supervisor_run_id;
        let another = work_run();
        let base = DirectorDrawerProjection::default()
            .with_work_runs(WorkRunProjection::fresh(vec![run, another]))
            .with_work_run_control(WorkRunControlProjection {
                mode: WorkRunControlMode::List,
                selected: Some(selected),
                ..WorkRunControlProjection::default()
            });
        let list = drawer_body(60, 12, &base)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(list.contains("Work Runs"));
        assert!(list.contains("› #"));
        assert!(list.contains("  #"));
        assert!(list.contains("Enter actions"));

        let confirmation = DirectorDrawerProjection {
            work_run_control: WorkRunControlProjection {
                mode: WorkRunControlMode::ConfirmCancel,
                selected: Some(selected),
                feedback: Some("review this action".into()),
                ..WorkRunControlProjection::default()
            },
            ..base.clone()
        };
        let confirmation = drawer_body(60, 12, &confirmation)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(confirmation.contains("stop its active Agents"));
        assert!(confirmation.contains("Enter confirm"));
        assert!(confirmation.contains("review this action"));

        let decision = DirectorDrawerProjection {
            work_run_control: WorkRunControlProjection {
                mode: WorkRunControlMode::ResolveEscalation,
                selected: Some(selected),
                decision: EscalationDecision::Cancel,
                feedback: None,
            },
            ..base.clone()
        };
        let decision = drawer_body(60, 12, &decision)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(decision.contains("› Cancel run"));

        let submitting = DirectorDrawerProjection {
            work_run_control: WorkRunControlProjection {
                mode: WorkRunControlMode::Submitting,
                selected: Some(selected),
                ..WorkRunControlProjection::default()
            },
            ..base.clone()
        };
        let submitting = drawer_body(60, 12, &submitting)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(submitting.contains("Applying the durable action"));
        assert!(submitting.contains("do not repeat"));

        let cached = DirectorDrawerProjection {
            work_runs: base.work_runs.clone().unavailable(),
            ..base.clone()
        };
        let cached = drawer_body(60, 12, &cached)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(cached.contains("Cached · refresh required before actions"));

        let retry = DirectorDrawerProjection {
            work_run_control: WorkRunControlProjection {
                mode: WorkRunControlMode::Retry,
                selected: Some(selected),
                feedback: Some("outcome unavailable".into()),
                ..WorkRunControlProjection::default()
            },
            ..base
        };
        let retry = drawer_body(60, 12, &retry)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(retry.contains("outcome unavailable"));
        assert!(retry.contains("retry same operation"));

        let empty =
            DirectorDrawerProjection::default().with_work_run_control(WorkRunControlProjection {
                mode: WorkRunControlMode::List,
                ..WorkRunControlProjection::default()
            });
        let empty = drawer_body(60, 12, &empty)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(empty.contains("No Work Runs yet"));
    }

    #[test]
    #[should_panic(expected = "closed control uses the normal drawer")]
    fn closed_work_run_control_cannot_enter_the_control_renderer() {
        let projection = DirectorDrawerProjection::default();
        let _ = work_run_control_body(60, 12, vec![], &projection);
    }

    #[test]
    fn goal_driven_empty_and_launching_states_render_their_distinct_guidance() {
        let composer = DirectorDrawerProjection {
            goal_driven: true,
            new: DirectorNewProjection::GoalComposer {
                candidates: vec!["claude".into()],
                selected: 0,
                goal: String::new(),
            },
            ..DirectorDrawerProjection::default()
        };
        let body = drawer_body(52, 12, &composer)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>();
        assert!(body.iter().any(|row| row.contains("Type a goal")));

        let launching = DirectorDrawerProjection {
            goal_driven: true,
            new: DirectorNewProjection::Launching,
            ..DirectorDrawerProjection::default()
        };
        let body = drawer_body(52, 8, &launching)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>();
        assert!(
            body.iter()
                .any(|row| row.contains("Waiting for the daemon to start the Work Run."))
        );
    }

    #[test]
    fn zero_width_goal_composer_keeps_its_row_contract() {
        let projection = DirectorDrawerProjection {
            goal_driven: true,
            new: DirectorNewProjection::GoalComposer {
                candidates: vec!["claude".into()],
                selected: 0,
                goal: String::new(),
            },
            ..DirectorDrawerProjection::default()
        };

        let body = drawer_body(0, 7, &projection);
        assert_eq!(body.len(), 7);
        assert!(body.iter().all(|row| display_width(row) == 0));

        let compact = drawer_body(8, 3, &projection);
        assert_eq!(compact.len(), 3);
        assert!(compact.iter().all(|row| display_width(row) <= 8));
    }

    #[test]
    fn populated_projection_renders_selected_conversation_and_terminal_rows() {
        let projection = DirectorDrawerProjection {
            goal_driven: false,
            conversations: vec![
                DirectorConversation {
                    label: "older".to_owned(),
                    selected: false,
                },
                DirectorConversation {
                    label: "active conversation".to_owned(),
                    selected: true,
                },
            ],
            organization: Vec::new(),
            terminal_view: Some(TerminalViewProjection {
                rows: vec![
                    "agent output one".to_owned(),
                    "agent output two".to_owned(),
                    "agent output three".to_owned(),
                ],
                row_offset: 0,
                total_rows: 3,
                scroll: 0,
                feedback: None,
            }),
            interrupted_detail: None,
            feedback: None,
            new: DirectorNewProjection::Ready,
            work_runs: WorkRunProjection::default(),
            work_run_control: WorkRunControlProjection::default(),
        };
        let frame = render_over(12, 80, &vec![String::new(); 12], &projection);
        let text = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Conversation  [active conversation]"));
        assert!(text.contains("agent output one"));
        assert!(text.contains("agent output two"));
        assert!(text.contains("agent output three"));
        assert!(!text.contains("No conversations yet"));
    }

    #[test]
    fn terminal_rows_render_even_when_conversation_inventory_is_empty() {
        let projection = DirectorDrawerProjection {
            terminal_view: Some(TerminalViewProjection {
                rows: vec!["live output without inventory".to_owned()],
                row_offset: 0,
                total_rows: 1,
                scroll: 0,
                feedback: None,
            }),
            ..DirectorDrawerProjection::default()
        };
        let body = drawer_body(52, 9, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(body.contains("live output without inventory"));
        assert!(!body.contains("No conversations yet"));
        assert!(!body.contains("Conversation inventory is not connected."));
    }

    #[test]
    fn retained_selection_rows_render_the_live_bottom_and_scrolled_windows() {
        let retained = (0..10).map(|row| format!("row {row}")).collect::<Vec<_>>();
        let projection = DirectorDrawerProjection {
            goal_driven: false,
            conversations: vec![DirectorConversation {
                label: "active".to_owned(),
                selected: true,
            }],
            organization: Vec::new(),
            terminal_view: Some(TerminalViewProjection {
                rows: retained,
                row_offset: 0,
                total_rows: 10,
                scroll: 0,
                feedback: Some("copied 2 lines".to_owned()),
            }),
            interrupted_detail: None,
            feedback: None,
            new: DirectorNewProjection::Ready,
            work_runs: WorkRunProjection::default(),
            work_run_control: WorkRunControlProjection::default(),
        };
        let body = drawer_body(52, 9, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>();
        assert_eq!(
            &body[2..8],
            ["row 4", "row 5", "row 6", "row 7", "row 8", "row 9"]
        );
        assert!(!body.iter().any(|row| row == "row 0"));
        assert_eq!(body[8], "copied 2 lines");

        let mut scrolled = projection;
        scrolled
            .terminal_view
            .as_mut()
            .expect("terminal projection")
            .scroll = 2;
        let body = drawer_body(52, 9, &scrolled)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>();
        assert_eq!(
            &body[2..8],
            ["row 2", "row 3", "row 4", "row 5", "row 6", "row 7"]
        );
        assert!(!body.iter().any(|row| row == "row 9"));

        let drawer = geometry(12, 80);
        assert_eq!(
            terminal_point_at(
                12,
                80,
                10,
                0,
                u16::try_from(drawer.left + 2).unwrap(),
                u16::try_from(drawer.top + 4).unwrap(),
            ),
            Some(TerminalPoint { row: 6, column: 0 })
        );
    }

    #[test]
    fn interrupted_detail_has_a_dedicated_body_row_and_feedback_owns_the_footer() {
        let projection = DirectorDrawerProjection {
            goal_driven: false,
            conversations: vec![DirectorConversation {
                label: "interrupted".to_owned(),
                selected: true,
            }],
            organization: Vec::new(),
            terminal_view: None,
            interrupted_detail: Some("identity unavailable".to_owned()),
            feedback: Some("resume failed safely".to_owned()),
            new: DirectorNewProjection::Ready,
            work_runs: WorkRunProjection::default(),
            work_run_control: WorkRunControlProjection::default(),
        };
        let body = drawer_body(52, 9, &projection)
            .into_iter()
            .map(|row| strip_ansi(&row))
            .collect::<Vec<_>>();
        assert_eq!(body[2], "identity unavailable");
        assert_eq!(body[8], "resume failed safely");
    }

    #[test]
    fn picker_and_safe_empty_state_render_without_clipping_cjk() {
        let picker = DirectorDrawerProjection {
            new: DirectorNewProjection::Choosing {
                candidates: vec![
                    "claude".to_owned(),
                    "codex".to_owned(),
                    "sakana.ai 日本語".to_owned(),
                ],
                selected: 2,
            },
            ..DirectorDrawerProjection::default()
        };
        let frame = render_over(12, 56, &[], &picker);
        let text = frame
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("claude"));
        assert!(text.contains("codex"));
        assert!(text.contains("› sakana.ai 日本語"));
        assert!(text.contains("Enter: launch"));
        assert!(frame.iter().all(|line| display_width(line) == 56));

        let empty = DirectorDrawerProjection {
            new: DirectorNewProjection::Empty,
            ..DirectorDrawerProjection::default()
        };
        let text = render_over(12, 56, &[], &empty)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("No Agent CLI installed"));
        assert!(text.contains("Install claude, codex, or sakana.ai"));

        let launching = DirectorDrawerProjection {
            new: DirectorNewProjection::Launching,
            ..DirectorDrawerProjection::default()
        };
        let text = render_over(12, 56, &[], &launching)
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[ Starting… ]"));
        assert!(text.contains("Waiting for the daemon to start the conversation."));
    }

    fn picker_of(candidates: &[&str], selected: usize) -> DirectorDrawerProjection {
        DirectorDrawerProjection {
            new: DirectorNewProjection::Choosing {
                candidates: candidates.iter().map(|label| (*label).to_owned()).collect(),
                selected,
            },
            ..DirectorDrawerProjection::default()
        }
    }

    #[test]
    fn picker_viewport_follows_the_selection_on_short_terminals() {
        let candidates = ["claude", "codex", "sakana.ai"];
        // 10 rows leave two candidate rows, 9 leave one, 8 leave none.
        for height in 8..=10 {
            for selected in 0..candidates.len() {
                let label = format!("height {height}, selected {selected}");
                let frame = render_over(height, 80, &[], &picker_of(&candidates, selected));
                assert!(
                    frame.iter().all(|line| display_width(line) == 80),
                    "{label}"
                );
                let text = frame
                    .iter()
                    .map(|line| strip_ansi(line))
                    .collect::<Vec<_>>();
                let marked = text
                    .iter()
                    .filter(|line| line.contains('›'))
                    .collect::<Vec<_>>();

                if height == 8 {
                    // No content row survives the chrome, so nothing is
                    // highlighted and the footer stops offering Enter — the
                    // reducer refuses the same launch at this height.
                    assert!(marked.is_empty(), "{label}");
                    assert!(
                        text.iter().any(|line| line.contains(PICKER_TOO_SHORT_HINT)),
                        "{label}"
                    );
                    assert!(
                        !text.iter().any(|line| line.contains("Enter: launch")),
                        "{label}"
                    );
                    continue;
                }
                assert_eq!(marked.len(), 1, "{label}");
                assert!(marked[0].contains(candidates[selected]), "{label}");
                assert!(
                    text.iter().any(|line| line.contains("Enter: launch")),
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn picker_capacity_matches_the_rows_the_drawer_draws() {
        let candidates = (0..20).map(|index| format!("cli-{index:02}")).collect();
        let projection = DirectorDrawerProjection {
            new: DirectorNewProjection::Choosing {
                candidates,
                selected: 10,
            },
            ..DirectorDrawerProjection::default()
        };
        for height in 0..=16 {
            let frame = render_over(height, 80, &[], &projection);
            let text = frame
                .iter()
                .map(|line| strip_ansi(line))
                .collect::<Vec<_>>();
            let drawn = text
                .iter()
                .filter(|line| line.contains("cli-") || line.contains(" more"))
                .count();
            assert_eq!(drawn, picker_capacity(height, 80), "height {height}");
            if drawn > 0 {
                assert!(
                    text.iter().any(|line| line.contains("› cli-10")),
                    "height {height}"
                );
            }
        }
    }

    #[test]
    fn goal_composer_capacity_matches_visible_provider_rows() {
        let projection = DirectorDrawerProjection {
            goal_driven: true,
            new: DirectorNewProjection::GoalComposer {
                candidates: vec!["claude".into()],
                selected: 0,
                goal: "finish the PR".into(),
            },
            ..DirectorDrawerProjection::default()
        };
        for height in 9..=13 {
            let text = render_over(height, 80, &[], &projection)
                .into_iter()
                .map(|line| strip_ansi(&line))
                .collect::<Vec<_>>();
            let provider_visible = text.iter().any(|line| line.contains("› claude"));
            assert_eq!(
                provider_visible,
                goal_composer_picker_capacity(height, 80) > 0,
                "height {height}"
            );
            assert_eq!(
                text.iter()
                    .any(|line| line.contains("Terminal too short to choose provider")),
                !provider_visible,
                "height {height}"
            );
        }
    }

    #[test]
    fn picker_rows_keep_the_frame_width_with_wide_and_pre_styled_labels() {
        let candidates = [
            "日本語のエージェント",
            "\u{1b}[1;31mcodex\u{1b}[0m",
            "sakana.ai 日本語",
        ];
        for height in 0..=14 {
            for width in [40, 56, 100] {
                for selected in 0..candidates.len() {
                    let frame = render_over(height, width, &[], &picker_of(&candidates, selected));
                    let (height, width) = widgets::normalize_size(height, width);
                    assert_eq!(frame.len(), height);
                    assert!(
                        frame.iter().all(|line| display_width(line) == width),
                        "{height}x{width}, selected {selected}"
                    );
                    assert!(
                        frame
                            .iter()
                            .all(|line| line.ends_with("\u{1b}[0m") || !line.contains('\u{1b}'))
                    );
                }
            }
        }
    }

    #[test]
    fn renderer_handles_tiny_resize_and_cjk_choice_without_style_leak() {
        let projection = DirectorDrawerProjection {
            goal_driven: false,
            conversations: vec![DirectorConversation {
                label: "会話の履歴".to_owned(),
                selected: true,
            }],
            organization: Vec::new(),
            terminal_view: None,
            interrupted_detail: None,
            feedback: None,
            new: DirectorNewProjection::Ready,
            work_runs: WorkRunProjection::default(),
            work_run_control: WorkRunControlProjection::default(),
        };
        for (height, width) in [(0, 0), (1, 1), (3, 8), (12, 56), (24, 200)] {
            let frame = render_over(height, width, &[], &projection);
            let (height, width) = widgets::normalize_size(height, width);
            assert_eq!(frame.len(), height);
            assert!(frame.iter().all(|line| display_width(line) == width));
            assert!(
                frame
                    .iter()
                    .all(|line| line.ends_with("\u{1b}[0m") || !line.contains('\u{1b}'))
            );
        }
    }
}
