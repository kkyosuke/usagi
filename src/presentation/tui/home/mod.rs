pub mod ui;

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::screen::AlternateScreenGuard;
use ui::MenuItem;

/// Displays the startup screen and waits for the user to quit.
///
/// Menu actions other than Quit are placeholders for now and show a
/// "coming soon" notice when selected.
pub fn run() -> Result<()> {
    let menu_items = [
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
    ];

    let term = Term::stdout();
    let mut guard = AlternateScreenGuard::new(term.clone())?;
    let mut selected_index = 0;
    let mut notice: Option<String> = None;

    loop {
        term.move_cursor_to(0, 0)?;
        term.clear_screen()?;
        ui::show_rabbit(&term);
        ui::render_side_menu(&term, &menu_items, selected_index);
        ui::render_notice(&term, notice.as_deref());
        ui::render_footer(&term);

        let key = match term.read_key() {
            Ok(key) => key,
            // Treat an interrupted read (e.g. Ctrl+C delivered as a signal) as quit.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
            Err(e) => {
                // Restore the terminal without the farewell on an unexpected error.
                guard.dismiss();
                return Err(anyhow::Error::from(e).context("Failed to read key"));
            }
        };

        match key {
            Key::ArrowUp | Key::Char('k') => {
                selected_index = selected_index
                    .checked_sub(1)
                    .unwrap_or(menu_items.len() - 1);
                notice = None;
            }
            Key::ArrowDown | Key::Char('j') => {
                selected_index = (selected_index + 1) % menu_items.len();
                notice = None;
            }
            Key::Enter => {
                let item = &menu_items[selected_index];
                if item.key == 'q' {
                    return Ok(());
                }
                notice = Some(format!("{} is coming soon 🐰", item.label));
            }
            Key::Char('q') | Key::Escape | Key::CtrlC => return Ok(()),
            Key::Char(c) => {
                if let Some(item) = menu_items.iter().find(|item| item.key == c) {
                    notice = Some(format!("{} is coming soon 🐰", item.label));
                }
            }
            _ => {}
        }
    }
}
