#![coverage(off)]

//! Platform clipboard adapter for the TUI composition root.
//!
//! The TUI depends only on `ClipboardPort`; this module selects the platform's
//! conventional command at runtime.  That keeps the binary buildable on every
//! Rust target without linking a window-system SDK, while supporting macOS,
//! Windows, Wayland, and X11 when their clipboard service is available. The
//! module is an OS process adapter; its behaviour is covered through the pure
//! `ClipboardPort` boundary in `usagi-tui` rather than LLVM line coverage.

use std::io::Write;
use std::process::{Command, Stdio};

use usagi_tui::usecase::application::terminal_selection::ClipboardPort;

/// The real OS clipboard adapter used by the crossterm composition root.
pub(crate) struct PlatformClipboard;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipboardCommand {
    program: &'static str,
    arguments: &'static [&'static str],
}

impl ClipboardPort for PlatformClipboard {
    #[coverage(off)]
    fn write_text(&mut self, text: &str) -> Result<(), String> {
        let mut failures = Vec::new();
        for command in clipboard_commands() {
            match write_with(&command, text) {
                Ok(()) => return Ok(()),
                Err(error) => failures.push(error),
            }
        }
        Err(format!(
            "clipboard is unavailable ({})",
            failures.join("; ")
        ))
    }
}

fn clipboard_commands() -> Vec<ClipboardCommand> {
    commands_for(
        current_platform(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Macos,
    Windows,
    Unix,
}

#[coverage(off)]
fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Macos
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

fn commands_for(platform: Platform, wayland: bool, x11: bool) -> Vec<ClipboardCommand> {
    if platform == Platform::Macos {
        return vec![ClipboardCommand {
            program: "pbcopy",
            arguments: &[],
        }];
    }
    if platform == Platform::Windows {
        return vec![ClipboardCommand {
            program: "clip.exe",
            arguments: &[],
        }];
    }

    // Linux and the other Unix targets may expose either protocol. Prefer the
    // current session's native protocol, then try the other one so remote and
    // nested desktop sessions remain usable.
    let mut commands = Vec::new();
    if wayland || !x11 {
        commands.push(ClipboardCommand {
            program: "wl-copy",
            arguments: &[],
        });
    }
    if x11 || !wayland {
        commands.extend([
            ClipboardCommand {
                program: "xclip",
                arguments: &["-selection", "clipboard"],
            },
            ClipboardCommand {
                program: "xsel",
                arguments: &["--clipboard", "--input"],
            },
        ]);
    }
    commands
}

fn write_with(command: &ClipboardCommand, text: &str) -> Result<(), String> {
    let mut child = Command::new(command.program)
        .args(command.arguments)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{}: {error}", command.program))?;
    child
        .stdin
        .take()
        .ok_or_else(|| format!("{}: stdin is unavailable", command.program))?
        .write_all(text.as_bytes())
        .map_err(|error| format!("{}: {error}", command.program))?;
    child
        .wait()
        .map_err(|error| format!("{}: {error}", command.program))?
        .success()
        .then_some(())
        .ok_or_else(|| format!("{}: command failed", command.program))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_native_macos_or_windows_command() {
        assert_eq!(
            commands_for(Platform::Macos, false, false),
            vec![ClipboardCommand {
                program: "pbcopy",
                arguments: &[]
            }]
        );
        assert_eq!(
            commands_for(Platform::Windows, false, false),
            vec![ClipboardCommand {
                program: "clip.exe",
                arguments: &[]
            }]
        );
    }

    #[test]
    fn unix_fallback_commands_preserve_the_clipboard_selection() {
        let commands = commands_for(Platform::Unix, false, true);
        assert_eq!(
            commands[0],
            ClipboardCommand {
                program: "xclip",
                arguments: &["-selection", "clipboard"]
            }
        );
        assert_eq!(
            commands[1],
            ClipboardCommand {
                program: "xsel",
                arguments: &["--clipboard", "--input"]
            }
        );
        assert_eq!(
            commands_for(Platform::Unix, true, false)[0].program,
            "wl-copy"
        );
    }
}
