//! macOS clipboard adapter for the TUI composition root.
//!
//! `pbcopy` is intentionally kept here: the TUI crate depends only on its
//! `ClipboardPort`, so selection tests never execute a process or touch a user
//! clipboard.

use std::io::Write;
use std::process::{Command, Stdio};

use usagi_tui::usecase::application::terminal_selection::ClipboardPort;

pub(crate) struct MacosClipboard;

impl ClipboardPort for MacosClipboard {
    #[coverage(off)]
    fn write_text(&mut self, text: &str) -> Result<(), String> {
        let mut child = Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|error| format!("clipboard is unavailable: {error}"))?;
        child
            .stdin
            .take()
            .ok_or_else(|| "clipboard is unavailable".to_owned())?
            .write_all(text.as_bytes())
            .map_err(|error| format!("clipboard write failed: {error}"))?;
        child
            .wait()
            .map_err(|error| format!("clipboard is unavailable: {error}"))?
            .success()
            .then_some(())
            .ok_or_else(|| "clipboard command failed".to_owned())
    }
}
