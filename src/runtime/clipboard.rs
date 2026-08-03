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

struct ClipboardCommand {
    program: &'static str,
    arguments: &'static [&'static str],
}

mod real_io {
    #![coverage(off)]

    use super::{
        ClipboardCommand, ClipboardPort, Command, PlatformClipboard, Stdio, Write, commands_for,
        current_platform, write_with_fallbacks,
    };

    impl ClipboardPort for PlatformClipboard {
        fn write_text(&mut self, text: &str) -> Result<(), String> {
            let mut write = write_with;
            write_with_fallbacks(&clipboard_commands(), text, &mut write)
        }
    }

    fn clipboard_commands() -> Vec<ClipboardCommand> {
        commands_for(
            &current_platform(),
            std::env::var_os("WAYLAND_DISPLAY").is_some(),
            std::env::var_os("DISPLAY").is_some(),
        )
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
}

#[allow(dead_code)] // Non-host variants are selected by the platform-independent fallback tests.
enum Platform {
    Macos,
    Windows,
    Unix,
}

fn current_platform() -> Platform {
    #[cfg(target_os = "macos")]
    return Platform::Macos;
    #[cfg(target_os = "windows")]
    return Platform::Windows;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return Platform::Unix;
}

fn write_with_fallbacks(
    commands: &[ClipboardCommand],
    text: &str,
    write: &mut dyn FnMut(&ClipboardCommand, &str) -> Result<(), String>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    for command in commands {
        match write(command, text) {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(error),
        }
    }
    Err(format!(
        "clipboard is unavailable ({})",
        failures.join("; ")
    ))
}

fn commands_for(platform: &Platform, wayland: bool, x11: bool) -> Vec<ClipboardCommand> {
    match platform {
        Platform::Macos => {
            return vec![ClipboardCommand {
                program: "pbcopy",
                arguments: &[],
            }];
        }
        Platform::Windows => {
            return vec![ClipboardCommand {
                program: "clip.exe",
                arguments: &[],
            }];
        }
        Platform::Unix => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_native_macos_or_windows_command() {
        let macos = commands_for(&Platform::Macos, false, false);
        assert_eq!(macos.len(), 1);
        assert_eq!(macos[0].program, "pbcopy");
        assert!(macos[0].arguments.is_empty());
        let windows = commands_for(&Platform::Windows, false, false);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].program, "clip.exe");
        assert!(windows[0].arguments.is_empty());
    }

    #[test]
    fn unix_fallback_commands_preserve_the_clipboard_selection() {
        let commands = commands_for(&Platform::Unix, false, true);
        assert_eq!(commands[0].program, "xclip");
        assert_eq!(commands[0].arguments, ["-selection", "clipboard"]);
        assert_eq!(commands[1].program, "xsel");
        assert_eq!(commands[1].arguments, ["--clipboard", "--input"]);
        assert_eq!(
            commands_for(&Platform::Unix, true, false)[0].program,
            "wl-copy"
        );
        assert_eq!(
            commands_for(&Platform::Unix, true, true)
                .iter()
                .map(|command| command.program)
                .collect::<Vec<_>>(),
            ["wl-copy", "xclip", "xsel"]
        );
        assert_eq!(
            commands_for(&Platform::Unix, false, false)
                .iter()
                .map(|command| command.program)
                .collect::<Vec<_>>(),
            ["wl-copy", "xclip", "xsel"]
        );
    }

    #[test]
    fn current_platform_matches_the_compilation_target() {
        #[cfg(target_os = "macos")]
        let expected = Platform::Macos;
        #[cfg(target_os = "windows")]
        let expected = Platform::Windows;
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let expected = Platform::Unix;
        assert_eq!(
            std::mem::discriminant(&current_platform()),
            std::mem::discriminant(&expected)
        );
    }

    #[test]
    fn fallback_stops_after_success_and_preserves_partial_failures() {
        let commands = commands_for(&Platform::Unix, true, true);
        let mut attempted = Vec::new();
        let mut write = |command: &ClipboardCommand, text: &str| {
            attempted.push((command.program, text.to_owned()));
            if command.program == "xclip" {
                Ok(())
            } else {
                Err(format!("{} failed", command.program))
            }
        };
        let result = write_with_fallbacks(&commands, "copy me", &mut write);
        assert_eq!(result, Ok(()));
        assert_eq!(
            attempted,
            [("wl-copy", "copy me".into()), ("xclip", "copy me".into())]
        );
    }

    #[test]
    fn all_backend_failures_are_reported_in_attempt_order() {
        let commands = commands_for(&Platform::Unix, true, true);
        let mut fail =
            |command: &ClipboardCommand, _: &str| Err(format!("{} failed", command.program));
        let error = write_with_fallbacks(&commands, "text", &mut fail).unwrap_err();
        assert_eq!(
            error,
            "clipboard is unavailable (wl-copy failed; xclip failed; xsel failed)"
        );
    }
}
