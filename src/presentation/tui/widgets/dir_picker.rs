//! A modal directory browser: type to filter, arrow to move, browse in and out
//! of folders, and pick the directory you land in.
//!
//! The browsing logic ([`DirPicker`]) is pure and terminal-independent — it
//! reads the filesystem only through the injected [`DirSource`] port, so it is
//! driven by a real filesystem in production ([`FsDirSource`]) and a fake in
//! tests. [`event_loop`] wires a [`DirPicker`] to a terminal and key source,
//! and [`render`] draws it as a centred modal over the calling screen.

use std::path::{Path, PathBuf};

use anyhow::Result;
use console::{style, Key, Term};

use crate::presentation::tui::io::screen::{FramePainter, KeyReader};

use super::picker::Picker;

/// Inner width of the modal box, in columns.
const INNER_WIDTH: usize = 56;

/// Most directory rows shown at once; longer lists scroll under the cursor.
const MAX_ROWS: usize = 10;

/// Rows the list area always occupies: up to [`MAX_ROWS`] entries plus one row
/// kept for the "… N more" overflow hint. Every listing state — error, empty,
/// short, or scrolled — is padded to this height so the modal box never resizes
/// and never jumps as you filter (no layout shift / CLS).
const LIST_HEIGHT: usize = MAX_ROWS + 1;

/// Lists the immediate subdirectories of a directory.
///
/// Abstracting the filesystem behind this port keeps [`DirPicker`] testable: a
/// fake source feeds it canned listings, while production uses [`FsDirSource`].
pub trait DirSource {
    /// The names of the directories directly inside `dir`, or a message
    /// describing why they could not be listed.
    fn entries(&self, dir: &Path) -> std::result::Result<Vec<String>, String>;
}

/// A [`DirSource`] backed by the real filesystem.
pub struct FsDirSource;

impl DirSource for FsDirSource {
    fn entries(&self, dir: &Path) -> std::result::Result<Vec<String>, String> {
        let read = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
        let mut names: Vec<String> = read
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        Ok(names)
    }
}

/// What the user did in the directory modal.
#[derive(Debug)]
pub enum Choice {
    /// The user picked this directory.
    Selected(PathBuf),
    /// The user dismissed the modal without picking.
    Cancelled,
    /// The user asked to quit the application entirely.
    Quit,
}

/// A directory browser: the directory currently being viewed, a filtered list
/// of its subdirectories, and any error from the last listing.
#[derive(Debug, Clone)]
pub struct DirPicker {
    /// The directory currently being browsed (and the one [`Choice::Selected`]
    /// returns).
    current: PathBuf,
    /// The subdirectories of `current`, filtered by the search query.
    picker: Picker,
    /// The error from the last listing, if it failed (e.g. permission denied).
    error: Option<String>,
}

impl DirPicker {
    /// Opens the browser on `start`, listing its subdirectories via `source`.
    pub fn open(source: &dyn DirSource, start: &Path) -> Self {
        let mut dir = Self {
            current: start.to_path_buf(),
            picker: Picker::new(Vec::new()),
            error: None,
        };
        dir.refresh(source);
        dir
    }

    /// The directory currently being browsed.
    pub fn current(&self) -> &Path {
        &self.current
    }

    /// The error from the last listing, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The current search query.
    pub fn query(&self) -> &str {
        self.picker.query()
    }

    /// The subdirectory names matching the query, in order.
    pub fn matches(&self) -> Vec<&str> {
        self.picker.matches()
    }

    /// The cursor position within the matches.
    pub fn cursor(&self) -> usize {
        self.picker.cursor()
    }

    /// Append a character to the search query.
    pub fn insert_char(&mut self, c: char) {
        self.picker.insert_char(c);
    }

    /// Delete the last character of the search query.
    pub fn backspace(&mut self) {
        self.picker.backspace();
    }

    /// Move the cursor up one subdirectory.
    pub fn move_up(&mut self) {
        self.picker.move_up();
    }

    /// Move the cursor down one subdirectory.
    pub fn move_down(&mut self) {
        self.picker.move_down();
    }

    /// Browse into the highlighted subdirectory. A no-op when nothing matches.
    pub fn descend(&mut self, source: &dyn DirSource) {
        if let Some(name) = self.picker.selected().map(str::to_owned) {
            self.current.push(name);
            self.refresh(source);
        }
    }

    /// Browse up to the parent directory. A no-op at the filesystem root.
    pub fn ascend(&mut self, source: &dyn DirSource) {
        if let Some(parent) = self.current.parent().map(Path::to_path_buf) {
            self.current = parent;
            self.refresh(source);
        }
    }

    /// The directory the user would pick right now (the one being browsed).
    pub fn chosen(&self) -> PathBuf {
        self.current.clone()
    }

    /// Re-list `current` through `source`, replacing the entries and recording
    /// any error so [`render`] can surface it.
    fn refresh(&mut self, source: &dyn DirSource) {
        match source.entries(&self.current) {
            Ok(entries) => {
                self.picker.set_entries(entries);
                self.error = None;
            }
            Err(message) => {
                self.picker.set_entries(Vec::new());
                self.error = Some(message);
            }
        }
    }
}

/// Builds the directory list area, padded to a fixed [`LIST_HEIGHT`].
///
/// Padding every state to the same height keeps the modal box one size, so it
/// never jumps as the listing changes while you filter (no layout shift / CLS).
fn list_lines(dir: &DirPicker) -> Vec<String> {
    let mut lines = list_content(dir);
    lines.resize(LIST_HEIGHT, String::new());
    lines
}

/// The varying content of the list area: a listing error, an empty-state hint,
/// or the (possibly scrolled) window of matching subdirectories with the cursor
/// marked. [`list_lines`] pads the result to a fixed height.
fn list_content(dir: &DirPicker) -> Vec<String> {
    if let Some(error) = dir.error() {
        return vec![style(format!("⚠ {error}")).red().to_string()];
    }
    let matches = dir.matches();
    if matches.is_empty() {
        return vec![style("(no matching directories)")
            .dim()
            .italic()
            .to_string()];
    }

    // Scroll the window so the cursor stays visible in long listings.
    let cursor = dir.cursor();
    let start = if cursor >= MAX_ROWS {
        cursor + 1 - MAX_ROWS
    } else {
        0
    };
    let end = (start + MAX_ROWS).min(matches.len());

    let mut lines: Vec<String> = matches[start..end]
        .iter()
        .enumerate()
        .map(|(offset, name)| {
            let row = format!("{name}/");
            if start + offset == cursor {
                format!("{} {}", style(">").red().bold(), style(row).cyan().bold())
            } else {
                format!("  {row}")
            }
        })
        .collect();

    // Note how many matches are scrolled out of view.
    let hidden = matches.len() - (end - start);
    if hidden > 0 {
        lines.push(style(format!("  … {hidden} more")).dim().to_string());
    }
    lines
}

/// Renders the directory browser as a centred modal for a raw terminal size.
pub fn render(raw_height: usize, raw_width: usize, dir: &DirPicker) -> Vec<String> {
    let mut body = vec![
        style(dir.current().display().to_string())
            .cyan()
            .bold()
            .to_string(),
        format!(
            "{} {}{}",
            style("Search:").dim(),
            dir.query(),
            style("▏").cyan()
        ),
        String::new(),
    ];
    body.extend(list_lines(dir));
    body.push(String::new());
    body.push(
        style("↑↓/Tab move · → open · ← up · Enter select · Esc cancel")
            .dim()
            .to_string(),
    );
    super::render_modal(
        raw_height,
        raw_width,
        "Select directory",
        INNER_WIDTH,
        &body,
    )
}

/// Runs the directory modal against the given terminal and key source until the
/// user picks a directory, cancels, or quits. Lists directories through
/// `source` and starts browsing at `start`.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    source: &dyn DirSource,
    start: &Path,
) -> Result<Choice> {
    let mut dir = DirPicker::open(source, start);
    let mut painter = FramePainter::new();

    loop {
        let (height, width) = term.size();
        let frame = render(height as usize, width as usize, &dir);
        painter.paint(term, frame)?;

        let key = match reader.read_key() {
            Ok(key) => key,
            // An interrupted read (e.g. a delivered signal) means quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Choice::Quit),
            Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
        };

        match key {
            Key::Escape => return Ok(Choice::Cancelled),
            // `Ctrl-C` / `Ctrl-Q` (the bare `0x11`) quit; matched before the
            // `Key::Char(c)` filter arm below that would otherwise capture Ctrl-Q.
            Key::CtrlC | Key::Char('\u{0011}') => return Ok(Choice::Quit),
            Key::Enter => return Ok(Choice::Selected(dir.chosen())),
            // Tab mirrors the new-session form, where it steps to the next
            // field: here it steps to the next/previous matching directory.
            Key::ArrowUp | Key::BackTab => dir.move_up(),
            Key::ArrowDown | Key::Tab => dir.move_down(),
            Key::ArrowRight => dir.descend(source),
            Key::ArrowLeft => dir.ascend(source),
            Key::Backspace => dir.backspace(),
            Key::Char(c) => dir.insert_char(c),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::io;

    /// A [`DirSource`] driven by a fixed map of directory → child names. Paths
    /// not in the map list as empty; the special path `/denied` errors.
    struct FakeSource {
        tree: HashMap<PathBuf, Vec<String>>,
    }

    impl FakeSource {
        fn new(entries: &[(&str, &[&str])]) -> Self {
            let tree = entries
                .iter()
                .map(|(dir, kids)| {
                    (
                        PathBuf::from(dir),
                        kids.iter().map(|k| k.to_string()).collect(),
                    )
                })
                .collect();
            Self { tree }
        }
    }

    impl DirSource for FakeSource {
        fn entries(&self, dir: &Path) -> std::result::Result<Vec<String>, String> {
            if dir == Path::new("/denied") {
                return Err("permission denied".to_string());
            }
            Ok(self.tree.get(dir).cloned().unwrap_or_default())
        }
    }

    /// A key source that replays a scripted sequence of results.
    struct ScriptedReader {
        keys: VecDeque<io::Result<Key>>,
    }

    impl ScriptedReader {
        fn new(keys: Vec<io::Result<Key>>) -> Self {
            Self { keys: keys.into() }
        }
    }

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> io::Result<Key> {
            // Default to Escape so a test can never spin forever.
            self.keys.pop_front().unwrap_or(Ok(Key::Escape))
        }
    }

    fn run(keys: Vec<io::Result<Key>>, source: &dyn DirSource, start: &str) -> Choice {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(keys);
        event_loop(&term, &mut reader, source, Path::new(start)).unwrap()
    }

    #[test]
    fn open_lists_the_starting_directory() {
        let source = FakeSource::new(&[("/home", &["alpha", "beta"])]);
        let dir = DirPicker::open(&source, Path::new("/home"));
        assert_eq!(dir.current(), Path::new("/home"));
        assert_eq!(dir.matches(), vec!["alpha", "beta"]);
        assert!(dir.error().is_none());
    }

    #[test]
    fn descending_and_ascending_walks_the_tree() {
        let source = FakeSource::new(&[
            ("/home", &["projects"]),
            ("/home/projects", &["app", "lib"]),
        ]);
        let mut dir = DirPicker::open(&source, Path::new("/home"));
        // Into /home/projects via the highlighted "projects".
        dir.descend(&source);
        assert_eq!(dir.current(), Path::new("/home/projects"));
        assert_eq!(dir.matches(), vec!["app", "lib"]);
        // Browsing resets the query, so back up to /home shows its children.
        dir.ascend(&source);
        assert_eq!(dir.current(), Path::new("/home"));
        assert_eq!(dir.matches(), vec!["projects"]);
    }

    #[test]
    fn descending_with_no_match_is_a_noop() {
        let source = FakeSource::new(&[("/home", &[])]);
        let mut dir = DirPicker::open(&source, Path::new("/home"));
        dir.descend(&source);
        // Nothing to descend into: still at /home.
        assert_eq!(dir.current(), Path::new("/home"));
    }

    #[test]
    fn ascending_at_the_root_is_a_noop() {
        let source = FakeSource::new(&[("/", &["home"])]);
        let mut dir = DirPicker::open(&source, Path::new("/"));
        dir.ascend(&source);
        assert_eq!(dir.current(), Path::new("/"));
    }

    #[test]
    fn a_listing_error_is_recorded_and_clears_the_entries() {
        let source = FakeSource::new(&[]);
        let dir = DirPicker::open(&source, Path::new("/denied"));
        assert_eq!(dir.error(), Some("permission denied"));
        assert!(dir.matches().is_empty());
    }

    #[test]
    fn typing_filters_and_movement_tracks_the_cursor() {
        let source = FakeSource::new(&[("/home", &["app", "apex", "lib"])]);
        let mut dir = DirPicker::open(&source, Path::new("/home"));
        dir.insert_char('a');
        dir.insert_char('p');
        assert_eq!(dir.matches(), vec!["app", "apex"]);
        dir.move_down();
        assert_eq!(dir.cursor(), 1);
        dir.backspace();
        dir.backspace();
        assert_eq!(dir.matches(), vec!["app", "apex", "lib"]);
    }

    #[test]
    fn render_shows_the_path_search_and_entries_with_a_cursor() {
        let source = FakeSource::new(&[("/home", &["alpha", "beta"])]);
        let dir = DirPicker::open(&source, Path::new("/home"));
        let frame = render(24, 80, &dir).join("\n");
        assert!(frame.contains("Select directory"));
        assert!(frame.contains("/home"));
        assert!(frame.contains("alpha/"));
        assert!(frame.contains("beta/"));
        assert!(frame.contains('>'));
    }

    #[test]
    fn render_surfaces_a_listing_error() {
        let source = FakeSource::new(&[]);
        let dir = DirPicker::open(&source, Path::new("/denied"));
        let frame = render(24, 80, &dir).join("\n");
        assert!(frame.contains("permission denied"));
    }

    #[test]
    fn render_shows_an_empty_state_when_nothing_matches() {
        let source = FakeSource::new(&[("/home", &["alpha"])]);
        let mut dir = DirPicker::open(&source, Path::new("/home"));
        dir.insert_char('z');
        let frame = render(24, 80, &dir).join("\n");
        assert!(frame.contains("no matching directories"));
    }

    #[test]
    fn render_scrolls_and_counts_hidden_entries_in_a_long_list() {
        let kids: Vec<String> = (0..20).map(|i| format!("dir{i:02}")).collect();
        let kid_refs: Vec<&str> = kids.iter().map(String::as_str).collect();
        let source = FakeSource::new(&[("/home", &kid_refs)]);
        let mut dir = DirPicker::open(&source, Path::new("/home"));
        // Move past the first window so it scrolls (cursor >= MAX_ROWS).
        for _ in 0..MAX_ROWS {
            dir.move_down();
        }
        let frame = render(40, 80, &dir).join("\n");
        // The first entry has scrolled out of view; later ones are visible.
        assert!(!frame.contains("dir00/"));
        assert!(frame.contains("dir10/"));
        assert!(frame.contains("more"));
    }

    #[test]
    fn the_modal_box_keeps_its_size_across_listing_states() {
        // The list area is padded to a fixed height, so the box — and thus the
        // whole modal — stays the same size whatever the listing shows: a short
        // list, an empty result, an error, or a scrolled long list all render
        // an identically tall box, so it never jumps as you filter (no CLS).
        let many: Vec<String> = (0..20).map(|i| format!("d{i:02}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let source = FakeSource::new(&[("/home", &["one"]), ("/big", &refs)]);

        // Count the rows belonging to the box (its borders and content rows).
        let box_height = |dir: &DirPicker| {
            render(40, 80, dir)
                .iter()
                .filter(|l| l.contains('│') || l.contains('┌') || l.contains('└'))
                .count()
        };

        let short = DirPicker::open(&source, Path::new("/home"));
        let mut empty = short.clone();
        empty.insert_char('z'); // filters everything out
        let error = DirPicker::open(&source, Path::new("/denied"));
        let mut scrolled = DirPicker::open(&source, Path::new("/big"));
        for _ in 0..MAX_ROWS {
            scrolled.move_down(); // past the first window, so it scrolls
        }

        let baseline = box_height(&short);
        assert_eq!(box_height(&empty), baseline);
        assert_eq!(box_height(&error), baseline);
        assert_eq!(box_height(&scrolled), baseline);
    }

    #[test]
    fn enter_selects_the_current_directory() {
        let source = FakeSource::new(&[("/home", &["projects"])]);
        // Descend into projects, then Enter picks it.
        let choice = run(vec![Ok(Key::ArrowRight), Ok(Key::Enter)], &source, "/home");
        assert!(matches!(choice, Choice::Selected(p) if p == Path::new("/home/projects")));
    }

    #[test]
    fn escape_cancels() {
        let source = FakeSource::new(&[("/home", &["a"])]);
        assert!(matches!(
            run(vec![Ok(Key::Escape)], &source, "/home"),
            Choice::Cancelled
        ));
    }

    #[test]
    fn ctrl_c_quits() {
        let source = FakeSource::new(&[("/home", &["a"])]);
        assert!(matches!(
            run(vec![Ok(Key::CtrlC)], &source, "/home"),
            Choice::Quit
        ));
    }

    #[test]
    fn ctrl_q_quits() {
        // `Ctrl-Q` (the bare `0x11`) quits, matched before the `Char(c)` filter arm
        // so it never lands in the filter as a literal control character.
        let source = FakeSource::new(&[("/home", &["a"])]);
        assert!(matches!(
            run(vec![Ok(Key::Char('\u{0011}'))], &source, "/home"),
            Choice::Quit
        ));
    }

    #[test]
    fn navigation_and_editing_keys_are_handled_then_select() {
        // Exercises every interactive arm: move down/up, ascend/descend, type,
        // backspace, and an ignored key, before Enter selects.
        let source = FakeSource::new(&[("/home", &["projects"]), ("/home/projects", &["app"])]);
        let keys = vec![
            Ok(Key::ArrowDown),
            Ok(Key::ArrowUp),
            Ok(Key::Tab),        // step down (alias of ArrowDown)
            Ok(Key::BackTab),    // step up (alias of ArrowUp)
            Ok(Key::ArrowRight), // into /home/projects
            Ok(Key::ArrowLeft),  // back to /home
            Ok(Key::Char('p')),  // filter
            Ok(Key::Backspace),  // clear filter
            Ok(Key::Home),       // ignored (the `_` arm)
            Ok(Key::Enter),
        ];
        assert!(matches!(
            run(keys, &source, "/home"),
            Choice::Selected(p) if p == Path::new("/home")
        ));
    }

    #[test]
    fn interrupted_read_quits() {
        let source = FakeSource::new(&[("/home", &["a"])]);
        let choice = run(
            vec![Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "interrupted",
            ))],
            &source,
            "/home",
        );
        assert!(matches!(choice, Choice::Quit));
    }

    #[test]
    fn unexpected_read_error_is_propagated() {
        let source = FakeSource::new(&[("/home", &["a"])]);
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let err = event_loop(&term, &mut reader, &source, Path::new("/home")).unwrap_err();
        assert!(err.to_string().contains("Failed to read key"));
    }

    #[test]
    fn fs_dir_source_lists_only_subdirectories_sorted() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("zeta")).unwrap();
        std::fs::create_dir(root.path().join("alpha")).unwrap();
        std::fs::write(root.path().join("file.txt"), b"x").unwrap();

        let names = FsDirSource.entries(root.path()).unwrap();
        // Files are excluded; directories come back sorted.
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
    }

    #[test]
    fn fs_dir_source_reports_an_error_for_a_missing_directory() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("does-not-exist");
        assert!(FsDirSource.entries(&missing).is_err());
    }
}
