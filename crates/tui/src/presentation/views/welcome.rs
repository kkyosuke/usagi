//! Welcome: resume the last project deck or choose projects to open.

use crate::presentation::layouts::mascot_screen;
use crate::presentation::theme::{Color, Role, Style};
use crate::presentation::widgets;
use chrono::{DateTime, Utc};
use usagi_core::domain::recent::{LastProjectSet, Recent};
use usagi_core::domain::workspace::Workspace;

const FOOTER: &str = "↑↓/jk select · Enter open · Ctrl-? help";
const BLOCK_WIDTH: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Open,
    New,
    Config,
    Quit,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItem {
    pub label: &'static str,
    pub key: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    items: Vec<MenuItem>,
    recent: Vec<Recent>,
    last_projects: Option<LastProjectSet>,
    selected_index: usize,
    notice: Option<String>,
}

fn activate(key: char) -> MenuAction {
    match key {
        'r' => MenuAction::Resume,
        'o' => MenuAction::Open,
        'e' => MenuAction::New,
        'c' => MenuAction::Config,
        _ => MenuAction::Quit,
    }
}

impl Welcome {
    /// Existing installations use their latest history entry until a deck is saved.
    #[must_use]
    pub fn new(recent: Vec<Recent>) -> Self {
        let paths = recent
            .first()
            .map(|recent| match recent {
                Recent::Workspace(overview) => vec![overview.workspace.path.clone()],
                Recent::Unite(unite) => unite
                    .members()
                    .iter()
                    .map(|m| m.workspace.path.clone())
                    .collect(),
            })
            .unwrap_or_default();
        let last_projects = paths
            .first()
            .cloned()
            .map(|active| LastProjectSet { paths, active });
        let mut welcome = Self {
            items: Vec::new(),
            recent,
            last_projects: None,
            selected_index: 0,
            notice: None,
        };
        welcome.set_last_projects(last_projects);
        welcome
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    #[must_use]
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// History remains available to the project picker, without Welcome cards.
    #[must_use]
    pub fn all_recent(&self) -> &[Recent] {
        &self.recent
    }

    #[must_use]
    pub fn last_projects(&self) -> Option<&LastProjectSet> {
        self.last_projects.as_ref()
    }

    pub(crate) fn set_recent(&mut self, recent: Vec<Recent>) {
        *self = Self::new(recent);
    }

    pub fn set_last_projects(&mut self, projects: Option<LastProjectSet>) {
        self.last_projects = projects.filter(LastProjectSet::is_valid);
        self.items = Vec::new();
        if self.last_projects.is_some() {
            self.items.push(MenuItem {
                label: "Open last projects",
                key: 'r',
            });
        }
        self.items.extend([
            MenuItem {
                label: "Open / add projects",
                key: 'o',
            },
            MenuItem {
                label: "Clone repository",
                key: 'e',
            },
            MenuItem {
                label: "Config",
                key: 'c',
            },
            MenuItem {
                label: "Quit",
                key: 'q',
            },
        ]);
        self.selected_index = 0;
    }

    /// `workspace` と同じ path の単体 recent に touch 後の identity / timestamp を反映し、
    /// 最終利用時刻の降順へ戻す。overview の集計値は既存 model の値を保つ。
    pub(crate) fn record_opened(&mut self, workspace: &Workspace) {
        let Some(overview) = self.recent.iter_mut().find_map(|recent| match recent {
            Recent::Workspace(overview) if overview.workspace.path == workspace.path => {
                Some(overview)
            }
            Recent::Workspace(_) | Recent::Unite(_) => None,
        }) else {
            return;
        };
        overview.workspace = workspace.clone();
        self.recent
            .sort_by_key(|recent| std::cmp::Reverse(recent.updated_at()));
    }

    /// Remove unregistered workspace paths from the in-memory Recent
    /// projection. Unite entries retain their surviving members and disappear
    /// only when no registered member remains.
    pub(crate) fn remove_paths(&mut self, paths: &[std::path::PathBuf]) {
        crate::presentation::prune_recent_paths(&mut self.recent, paths);
        if let Some(mut last) = self.last_projects.clone() {
            last.retain_paths(|path| !paths.iter().any(|removed| removed == path));
            self.set_last_projects(Some(last));
        }
    }

    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected_index
    }
    #[must_use]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }
    pub fn select_next(&mut self) {
        self.selected_index = (self.selected_index + 1) % self.items.len();
        self.notice = None;
    }
    pub fn select_prev(&mut self) {
        self.selected_index = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.items.len() - 1);
        self.notice = None;
    }
    #[must_use]
    pub fn selected_action(&self) -> MenuAction {
        activate(self.items[self.selected_index].key)
    }
    #[must_use]
    pub fn action_for(&self, key: char) -> Option<MenuAction> {
        self.items
            .iter()
            .find(|item| item.key == key)
            .map(|item| activate(item.key))
    }

    fn project_name(&self, path: &std::path::Path) -> String {
        self.recent
            .iter()
            .flat_map(|recent| match recent {
                Recent::Workspace(overview) => std::slice::from_ref(overview),
                Recent::Unite(unite) => unite.members(),
            })
            .find(|overview| overview.workspace.path == path)
            .map_or_else(
                || {
                    path.file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                },
                |overview| overview.workspace.name.clone(),
            )
    }
}

impl Default for Welcome {
    fn default() -> Self {
        Self::empty()
    }
}

/// Only action labels take the focus colour; shortcuts stay quiet.
fn menu_text(label: &str, selected: bool) -> String {
    let style = if selected {
        Role::Accent.style().bold()
    } else {
        Style::new().fg(Color::White)
    };
    style.paint(label)
}

fn shortcut(key: char) -> String {
    Style::new().fg(Color::White).dim().paint(&key.to_string())
}

fn menu_row(item: &MenuItem, selected: bool, width: usize) -> String {
    let label = widgets::pad_to_width(item.label, width.saturating_sub(6));
    widgets::clip_to_width(
        &format!("  {} {}  ", menu_text(&label, selected), shortcut(item.key)),
        width,
    )
}

fn utility_row(welcome: &Welcome) -> String {
    welcome
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item.key, 'c' | 'q'))
        .map(|(index, item)| {
            format!(
                "{}  {}",
                menu_text(item.label, index == welcome.selected_index),
                shortcut(item.key),
            )
        })
        .collect::<Vec<_>>()
        .join("     ")
}

/// A quiet launcher grouped in the upper third, with only hints at the bottom.
#[must_use]
pub fn render(
    raw_height: usize,
    raw_width: usize,
    welcome: &Welcome,
    _now: DateTime<Utc>,
) -> Vec<String> {
    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let block = BLOCK_WIDTH.min(width);
    let indent = |line: &str| {
        format!(
            "{}{}",
            " ".repeat(widgets::centered_padding(width, block)),
            widgets::clip_to_width(line, block)
        )
    };
    let spacious = height >= 20;
    let mut content = Vec::new();
    if spacious {
        content.extend(widgets::icon::centered(width).iter().map(|line| {
            Role::Feature
                .style()
                .paint(&widgets::clip_to_width(line, width))
        }));
        content.push(String::new());
    }
    if height >= 8 {
        content.push(mascot_screen::centered_line(
            width,
            "USAGI",
            Role::Success.style().dim(),
        ));
        content.push(String::new());
    }
    for (index, item) in welcome
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| !matches!(item.key, 'c' | 'q'))
    {
        content.push(indent(&menu_row(
            item,
            index == welcome.selected_index,
            block,
        )));
        if height >= 10
            && item.key == 'r'
            && let Some(last) = welcome.last_projects()
        {
            let names = last
                .paths
                .iter()
                .map(|path| welcome.project_name(path))
                .collect::<Vec<_>>()
                .join(" · ");
            content.push(indent(
                &Style::new()
                    .fg(Color::White)
                    .dim()
                    .paint(&format!("  {names}")),
            ));
        }
        if spacious {
            content.push(String::new());
        }
    }
    // Reserve utilities and the footer before admitting optional notice rows.
    let budget = height.saturating_sub(2);
    content.truncate(budget);
    if let Some(notice) = welcome.notice() {
        let available = budget.saturating_sub(content.len());
        content.extend(
            widgets::wrap_to_width(notice, block)
                .iter()
                .take(available)
                .map(|line| mascot_screen::centered_line(width, line, Role::Warning.style())),
        );
    }
    if spacious && content.len() < budget {
        content.push(String::new());
    }
    if height >= 2 {
        content.push(mascot_screen::centered_line(
            width,
            &utility_row(welcome),
            Style::new(),
        ));
    }
    // Centre the whole group near the upper third, saturating on short screens.
    let top = (height / 3)
        .saturating_sub(content.len() / 2)
        .min(height.saturating_sub(content.len() + 1));
    let mut frame = vec![String::new(); top];
    frame.extend(content);
    frame.resize(height.saturating_sub(1), String::new());
    frame.push(mascot_screen::centered_line(
        width,
        FOOTER,
        Style::new().fg(Color::White).dim(),
    ));
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi_core::domain::recent::UniteOverview;
    use usagi_core::domain::workspace::WorkspaceOverview;

    fn recent(name: &str) -> Recent {
        Recent::Workspace(WorkspaceOverview::new(
            Workspace::new(name, format!("/tmp/{name}")),
            0,
            0,
            0,
        ))
    }

    #[test]
    fn first_launch_opens_picker_and_history_defaults_to_resume() {
        let empty = Welcome::default();
        assert_eq!(empty.items()[empty.selected_index()].key, 'o');
        assert_eq!(empty.selected_action(), MenuAction::Open);
        assert_eq!(empty.action_for('r'), None);
        let mut welcome = Welcome::new(vec![recent("alpha"), recent("beta")]);
        assert_eq!(welcome.selected_action(), MenuAction::Resume);
        for digit in ['0', '1', '2', '3', '4'] {
            assert_eq!(welcome.action_for(digit), None);
        }
        welcome.select_next();
        assert_eq!(welcome.selected_action(), MenuAction::Open);
        welcome.select_prev();
        welcome.select_prev();
        assert_eq!(welcome.selected_action(), MenuAction::Quit);
        welcome.select_next();
        assert_eq!(welcome.selected_action(), MenuAction::Resume);
    }

    #[test]
    fn unregister_prunes_unite_history_and_removes_empty_groups() {
        let members = ["alpha", "beta"]
            .into_iter()
            .map(|name| {
                WorkspaceOverview::new(Workspace::new(name, format!("/tmp/{name}")), 0, 0, 0)
            })
            .collect();
        let mut welcome = Welcome::new(vec![
            Recent::Unite(UniteOverview::new(members)),
            Recent::Unite(UniteOverview::new(Vec::new())),
        ]);
        welcome.remove_paths(&["/tmp/alpha".into()]);
        assert_eq!(welcome.all_recent().len(), 1);
        assert!(matches!(&welcome.all_recent()[0], Recent::Unite(group)
            if group.members().len() == 1 && group.primary_name() == "beta"));
        welcome.remove_paths(&["/tmp/beta".into()]);
        assert!(welcome.all_recent().is_empty());
        assert!(welcome.last_projects().is_none());
    }

    #[test]
    fn persisted_deck_is_independent_of_recent_and_prunes_removed_projects() {
        let mut welcome = Welcome::new(vec![recent("alpha"), recent("beta")]);
        welcome.set_last_projects(Some(LastProjectSet {
            paths: vec!["/tmp/beta".into(), "/tmp/alpha".into()],
            active: "/tmp/alpha".into(),
        }));
        welcome.record_opened(&Workspace::new("beta", "/tmp/beta"));
        assert_eq!(
            welcome.last_projects().unwrap().paths[0],
            std::path::Path::new("/tmp/beta")
        );
        welcome.remove_paths(&["/tmp/alpha".into()]);
        assert_eq!(
            welcome.last_projects().unwrap().active,
            std::path::Path::new("/tmp/beta")
        );
        welcome.remove_paths(&["/tmp/beta".into()]);
        assert_eq!(welcome.selected_action(), MenuAction::Open);
    }

    #[test]
    fn layout_has_no_recent_cards_and_keeps_utilities_visible() {
        let mut welcome = Welcome::new(vec![recent("alpha"), recent("hidden")]);
        welcome.set_notice(Some("Could not open project".into()));
        for (height, width) in [(24, 80), (16, 40), (10, 32), (40, 120)] {
            let frame = render(height, width, &welcome, Utc::now());
            assert_eq!(frame.len(), height);
            assert!(
                frame
                    .iter()
                    .all(|line| widgets::display_width(line) <= width)
            );
            let text = frame.join("\n");
            assert!(!text.contains("Recent"));
            assert!(!text.contains("hidden"));
            assert!(text.contains("alpha"));
            assert!(text.contains("Config"));
            assert!(text.contains("Quit"));
            assert!(text.contains("Could not open project"));
        }
    }

    #[test]
    fn launcher_groups_actions_near_the_upper_third() {
        let mut welcome = Welcome::new(vec![recent("usagi"), recent("monica")]);
        welcome.set_last_projects(Some(LastProjectSet {
            paths: vec!["/tmp/usagi".into(), "/tmp/monica".into()],
            active: "/tmp/monica".into(),
        }));
        for height in [24, 40, 80] {
            let frame = render(height, 80, &welcome, Utc::now());
            let plain: Vec<_> = frame.iter().map(|line| widgets::strip_ansi(line)).collect();
            let first = plain.iter().position(|line| !line.is_empty()).unwrap();
            let utilities = plain
                .iter()
                .position(|line| line.contains("Config"))
                .unwrap();
            let clone = plain
                .iter()
                .position(|line| line.contains("Clone repository"))
                .unwrap();
            assert!(usize::midpoint(first, utilities).abs_diff(height / 3) <= 1);
            assert_eq!(utilities - clone, 3);
            assert!(utilities < height - 2);
            assert!(plain.last().unwrap().contains("Enter open"));
            assert_eq!(
                plain
                    .iter()
                    .filter(|line| line.contains("usagi · monica"))
                    .count(),
                1
            );
            assert!(!plain.join("\n").contains("active:"));
            assert!(!plain.join("\n").contains("2 projects"));
        }
    }

    #[test]
    fn compact_frames_preserve_actions_and_bound_long_content() {
        let mut welcome = Welcome::new(vec![recent(
            "日本語の長いプロジェクト名".repeat(8).as_str(),
        )]);
        welcome.set_notice(Some("Could not open project. ".repeat(50)));
        for height in 1..=24 {
            for width in [1, 3, 20, 40, 80] {
                let frame = render(height, width, &welcome, Utc::now());
                assert_eq!(frame.len(), height);
                assert!(
                    frame
                        .iter()
                        .all(|line| widgets::display_width(line) <= width)
                );
                if height >= 5 && width >= 40 {
                    let text = frame.join("\n");
                    for label in [
                        "Open last projects",
                        "Open / add projects",
                        "Clone repository",
                        "Config",
                        "Quit",
                    ] {
                        assert!(text.contains(label), "{height}x{width}: missing {label}");
                    }
                }
            }
        }
        assert_eq!(render(0, 0, &welcome, Utc::now()).len(), 24);
    }

    #[test]
    fn shortcuts_stay_dim_when_the_label_is_selected() {
        let welcome = Welcome::empty();
        let item = &welcome.items()[0];
        let focused = menu_row(item, true, 48);
        let idle = menu_row(item, false, 48);
        assert!(focused.contains("\u{1b}[1;36mOpen / add projects"));
        assert!(focused.contains(&shortcut('o')));
        assert!(idle.contains(&shortcut('o')));
        let mut selected = welcome.clone();
        selected.select_prev();
        assert!(utility_row(&selected).contains(&menu_text("Quit", true)));
        assert!(utility_row(&selected).contains(&shortcut('q')));
    }

    #[test]
    fn changing_selection_only_changes_style_and_keeps_all_labels_in_place() {
        for recent in [Vec::new(), vec![self::recent("alpha")]] {
            for (height, width) in [(24, 80), (16, 40), (10, 32), (40, 120), (4, 3)] {
                let mut welcome = Welcome::new(recent.clone());
                let plain = |frame: Vec<String>| {
                    frame
                        .iter()
                        .map(|line| widgets::strip_ansi(line))
                        .collect::<Vec<_>>()
                };
                let original = plain(render(height, width, &welcome, Utc::now()));
                assert!(original.iter().all(|line| !line.contains(['[', ']'])));
                for _ in 0..welcome.items().len() {
                    welcome.select_next();
                    assert_eq!(plain(render(height, width, &welcome, Utc::now())), original);
                }
            }
        }
        let welcome = Welcome::empty();
        let item = &welcome.items()[0];
        assert!(menu_row(item, true, 48).contains("\u{1b}[1;36mOpen"));
        assert!(menu_row(item, false, 48).contains("\u{1b}[37mOpen"));
        assert!(widgets::strip_ansi(&menu_row(item, false, 48)).ends_with("o  "));
    }
}
