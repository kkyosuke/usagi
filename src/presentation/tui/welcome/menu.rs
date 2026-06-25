use console::Key;

use super::ui::MenuItem;

/// What the event loop should do after a key has been handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Stay on the screen and redraw.
    Continue,
    /// Open the project selection screen.
    OpenOpen,
    /// Open the New Project screen.
    OpenNew,
    /// Open the Config screen.
    OpenConfig,
    /// Leave the welcome screen.
    Quit,
}

/// Mutable state of the welcome-screen menu, independent of any terminal I/O.
pub struct Menu {
    items: Vec<MenuItem>,
    selected_index: usize,
    notice: Option<String>,
}

impl Menu {
    /// Builds the menu with its fixed set of entries.
    pub fn new() -> Self {
        Self {
            items: vec![
                MenuItem {
                    label: "Open",
                    key: 'o',
                },
                MenuItem {
                    label: "New",
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
            ],
            selected_index: 0,
            notice: None,
        }
    }

    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    pub fn selected_index(&self) -> usize {
        self.selected_index
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Applies a key press, mutating the menu and reporting whether to continue.
    pub fn handle_key(&mut self, key: Key) -> Action {
        match key {
            Key::ArrowUp | Key::Char('k') => {
                self.selected_index = self
                    .selected_index
                    .checked_sub(1)
                    .unwrap_or(self.items.len() - 1);
                self.notice = None;
                Action::Continue
            }
            Key::ArrowDown | Key::Char('j') => {
                self.selected_index = (self.selected_index + 1) % self.items.len();
                self.notice = None;
                Action::Continue
            }
            Key::Enter => self.activate(self.items[self.selected_index].key),
            // `Ctrl-Q` (the bare `0x11` `console` reports) is the global quit chord
            // alongside `q` / `Esc` / `Ctrl-C`.
            Key::Char('q') | Key::Escape | Key::CtrlC | Key::Char('\u{0011}') => Action::Quit,
            Key::Char(c) if self.items.iter().any(|item| item.key == c) => self.activate(c),
            _ => Action::Continue,
        }
    }

    /// Activates the menu item with the given shortcut key, shared by Enter and
    /// the direct shortcut keys.
    fn activate(&self, key: char) -> Action {
        match key {
            'o' => Action::OpenOpen,
            'e' => Action::OpenNew,
            'c' => Action::OpenConfig,
            'q' => Action::Quit,
            // Every real menu item maps above; an unknown key is a safe no-op.
            _ => Action::Continue,
        }
    }

    /// Replaces the transient notice, e.g. after returning from a sub-screen.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }
}

impl Default for Menu {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_menu_starts_at_first_item_without_notice() {
        let menu = Menu::new();
        assert_eq!(menu.selected_index(), 0);
        assert_eq!(menu.notice(), None);
        assert_eq!(menu.items().len(), 4);
    }

    #[test]
    fn default_matches_new() {
        let menu = Menu::default();
        assert_eq!(menu.selected_index(), 0);
    }

    #[test]
    fn arrow_down_advances_and_wraps() {
        let mut menu = Menu::new();
        assert_eq!(menu.handle_key(Key::ArrowDown), Action::Continue);
        assert_eq!(menu.selected_index(), 1);
        // 'j' is an alias for ArrowDown.
        menu.handle_key(Key::Char('j'));
        menu.handle_key(Key::Char('j'));
        assert_eq!(menu.selected_index(), 3);
        menu.handle_key(Key::ArrowDown);
        assert_eq!(menu.selected_index(), 0);
    }

    #[test]
    fn arrow_up_wraps_to_last_item() {
        let mut menu = Menu::new();
        assert_eq!(menu.handle_key(Key::ArrowUp), Action::Continue);
        assert_eq!(menu.selected_index(), 3);
        // 'k' is an alias for ArrowUp.
        menu.handle_key(Key::Char('k'));
        assert_eq!(menu.selected_index(), 2);
    }

    #[test]
    fn movement_clears_an_existing_notice() {
        let mut menu = Menu::new();
        // A notice left over from a returning sub-screen is cleared on movement.
        menu.set_notice(Some("Saved 🐰".to_string()));
        menu.handle_key(Key::ArrowDown);
        assert_eq!(menu.notice(), None);
        menu.set_notice(Some("Saved 🐰".to_string()));
        menu.handle_key(Key::ArrowUp);
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn enter_on_config_item_opens_config_screen() {
        let mut menu = Menu::new();
        menu.handle_key(Key::ArrowDown); // New
        menu.handle_key(Key::ArrowDown); // Config
        assert_eq!(menu.selected_index(), 2);
        assert_eq!(menu.handle_key(Key::Enter), Action::OpenConfig);
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn enter_on_open_item_opens_open_screen() {
        let mut menu = Menu::new();
        // "Open" is the first item.
        assert_eq!(menu.selected_index(), 0);
        assert_eq!(menu.handle_key(Key::Enter), Action::OpenOpen);
        // Opening a sub-screen does not leave a "coming soon" notice behind.
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn open_shortcut_opens_open_screen() {
        let mut menu = Menu::new();
        assert_eq!(menu.handle_key(Key::Char('o')), Action::OpenOpen);
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn enter_on_quit_item_quits() {
        let mut menu = Menu::new();
        menu.handle_key(Key::ArrowUp); // wrap to the last item, "Quit"
        assert_eq!(menu.selected_index(), 3);
        assert_eq!(menu.handle_key(Key::Enter), Action::Quit);
    }

    #[test]
    fn enter_on_new_item_opens_new_screen() {
        let mut menu = Menu::new();
        menu.handle_key(Key::ArrowDown); // move to "New"
        assert_eq!(menu.selected_index(), 1);
        assert_eq!(menu.handle_key(Key::Enter), Action::OpenNew);
        // Opening a sub-screen does not leave a "coming soon" notice behind.
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn new_shortcut_opens_new_screen() {
        let mut menu = Menu::new();
        assert_eq!(menu.handle_key(Key::Char('e')), Action::OpenNew);
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn set_notice_replaces_the_notice() {
        let mut menu = Menu::new();
        menu.set_notice(Some("done".to_string()));
        assert_eq!(menu.notice(), Some("done"));
        menu.set_notice(None);
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn config_shortcut_opens_config_screen() {
        let mut menu = Menu::new();
        assert_eq!(menu.handle_key(Key::Char('c')), Action::OpenConfig);
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn activate_ignores_an_unknown_key() {
        // Every menu item routes to a real action; a stray key is a no-op. This
        // arm is unreachable via handle_key (which only activates known item
        // keys), so it is exercised directly.
        let menu = Menu::new();
        assert_eq!(menu.activate('z'), Action::Continue);
    }

    #[test]
    fn unknown_character_is_ignored() {
        let mut menu = Menu::new();
        assert_eq!(menu.handle_key(Key::Char('z')), Action::Continue);
        assert_eq!(menu.notice(), None);
    }

    #[test]
    fn quit_keys_quit() {
        // `Ctrl-Q` (the bare `0x11`) joins `q` / `Esc` / `Ctrl-C` as a quit chord.
        for key in [
            Key::Char('q'),
            Key::Escape,
            Key::CtrlC,
            Key::Char('\u{0011}'),
        ] {
            assert_eq!(Menu::new().handle_key(key), Action::Quit);
        }
    }

    #[test]
    fn other_keys_continue_without_change() {
        let mut menu = Menu::new();
        assert_eq!(menu.handle_key(Key::Home), Action::Continue);
        assert_eq!(menu.selected_index(), 0);
        assert_eq!(menu.notice(), None);
    }
}
