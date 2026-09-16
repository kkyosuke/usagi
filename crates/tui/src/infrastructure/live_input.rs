//! 実端末から届いた live 入力を TUI の `Key` 語彙へ分類する。
//!
//! 端末バックエンド（crossterm）そのものは合成ルートが持ち、ここは既に
//! backend 非依存な `LiveInput` だけを見る。

use std::time::Duration;

use crate::usecase::application::Key;
use crate::usecase::application::controller::{AppKey, classify_management_input};
use crate::usecase::terminal_input::{
    GlobalControlChord, KeyCode, KeyEventKind, LiveInput, LiveInputClassifier, LiveInputOutput,
    Modifiers,
};

/// Apply the process-wide input ordering policy before projecting terminal input
/// into the management [`Key`] vocabulary. `LiveInputClassifier` is the sole
/// owner of leader precedence for every workspace surface; this adapter only
/// translates its resolved output.
#[must_use]
pub fn classify_terminal_input(
    classifier: &mut LiveInputClassifier,
    now: Duration,
    input: &LiveInput,
) -> Option<Key> {
    match classifier.classify(now, input.clone()) {
        LiveInputOutput::Action(action) => Some(Key::Live(action)),
        LiveInputOutput::GlobalControl(control) => Some(match control {
            GlobalControlChord::CtrlC => terminal_copy_key(input).unwrap_or(Key::Quit),
            GlobalControlChord::CtrlQ => Key::CtrlQ,
            GlobalControlChord::CtrlD => Key::CtrlD,
            GlobalControlChord::CtrlX => Key::CtrlX,
            GlobalControlChord::Help => Key::Help,
        }),
        LiveInputOutput::Swallowed => None,
        LiveInputOutput::Passthrough(bytes) => match input {
            LiveInput::Pointer(pointer) => Some(Key::Pointer(*pointer)),
            LiveInput::Mouse { column, row } => Some(Key::Click {
                column: *column,
                row: *row,
            }),
            _ => terminal_copy_key(input).or_else(|| Some(passthrough_key(input, bytes))),
        },
    }
}

/// Maps each supported platform's terminal copy chord to a selection-aware
/// request. Windows retains Ctrl-C as a PTY SIGINT when there is no selection.
#[must_use]
pub fn terminal_copy_key(input: &LiveInput) -> Option<Key> {
    let LiveInput::Key(key) = input else {
        return None;
    };
    let only = |control, shift, super_| {
        key.modifiers.control == control
            && key.modifiers.shift == shift
            && key.modifiers.super_ == super_
            && !key.modifiers.alt
            && !key.modifiers.hyper
            && !key.modifiers.meta
    };
    // `adapt_key` canonicalizes a shifted ASCII letter to uppercase. Linux's
    // native copy chord includes Shift, while terminals that already resolve
    // the character can still supply lowercase. Keep the character spelling
    // protocol-independent without relaxing the platform modifier contract.
    let copy_character = matches!(key.code, KeyCode::Char('c' | 'C'));
    #[cfg(target_os = "macos")]
    let matches_copy = copy_character && only(false, false, true);
    #[cfg(target_os = "windows")]
    let matches_copy = copy_character && only(true, false, false);
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let matches_copy = copy_character && only(true, true, false);

    #[cfg(target_os = "windows")]
    let fallback = vec![3];
    #[cfg(not(target_os = "windows"))]
    let fallback = Vec::new();

    matches_copy.then_some(Key::TerminalCopy { fallback })
}

/// Map a non-prefix terminal input to the management `Key` vocabulary. The
/// process-wide classifier has already reserved the `Ctrl-O` prefix, so this
/// preserves the prior mapping for every other key and text/paste payload.
#[must_use]
#[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=production_input_classifier_contract
pub fn passthrough_key(input: &LiveInput, bytes: Vec<u8>) -> Key {
    let key = match input {
        LiveInput::Key(key) => key,
        // Some terminal decoders preserve Return as its original byte instead
        // of emitting a semantic key event. Management modals must accept both
        // forms, otherwise Closeup actions appear to ignore Enter.
        LiveInput::Raw(bytes) if bytes.as_slice() == b"\r" || bytes.as_slice() == b"\n" => {
            return Key::Enter;
        }
        LiveInput::Text(text) if text == "\r" || text == "\n" => {
            return Key::Enter;
        }
        // A bracketed paste is delivered as one block. Carry the text so the
        // focused live pane wraps it in bracketed-paste markers before it reaches
        // the PTY (agents insert it instead of submitting each embedded newline)
        // and a management text input inserts it verbatim.
        LiveInput::Paste(_) => {
            return Key::Paste(String::from_utf8_lossy(&bytes).into_owned());
        }
        LiveInput::Raw(_) | LiveInput::Text(_) => {
            return Key::Passthrough(bytes);
        }
        LiveInput::Mouse { .. }
        | LiveInput::WheelUp { .. }
        | LiveInput::WheelDown { .. }
        | LiveInput::Pointer(_) => return Key::Other,
    };
    // Some terminal backends report an auto-repeat as the first observable
    // key event.  Treat it like a press so management controls (notably
    // Closeup's Enter action) are never dropped; only releases are inert.
    if matches!(key.kind, KeyEventKind::Release) {
        return Key::Other;
    }
    // Ctrl-A / Ctrl-E become semantic caret keys. A focused text field reads
    // them as emacs line-start / line-end; the reducer's navigation branch maps
    // `LineStart` back to the reserved `+ new session` action (IME-safe),
    // and `key_to_terminal_bytes` still forwards U+0001 / U+0005 to a focused
    // shell. `Home` / `End` carry the same split without the control modifier.
    if (key.modifiers.control && key.code == KeyCode::Char('a'))
        || key.code == KeyCode::Char('\u{1}')
    {
        return Key::LineStart;
    }
    if (key.modifiers.control && key.code == KeyCode::Char('e'))
        || key.code == KeyCode::Char('\u{5}')
    {
        return Key::LineEnd;
    }
    // Shift+motion extends a selection in the focused input; a live shell still
    // receives movement via `key_to_terminal_bytes`. Handle these before the
    // generic modified-chord passthrough below swallows the Shift.
    match key.code {
        KeyCode::Left if key.modifiers.shift => return Key::SelectLeft,
        KeyCode::Right if key.modifiers.shift => return Key::SelectRight,
        KeyCode::Home if key.modifiers.shift => return Key::SelectHome,
        KeyCode::End if key.modifiers.shift => return Key::SelectEnd,
        _ => {}
    }
    // The live classifier has already encoded the original terminal input.
    // Keep modified chords opaque so this management-key adapter cannot drop
    // their Ctrl/Alt bytes before Closeup forwards them to the focused pane.
    // Crossterm reports Shift even though `Char` already carries the resulting
    // uppercase (or shifted-symbol) Unicode scalar.  It is text input, not an
    // opaque terminal chord, so pass it to management forms normally.
    let shift_only = key.modifiers.shift
        && !key.modifiers.control
        && !key.modifiers.alt
        && !key.modifiers.super_
        && !key.modifiers.hyper
        && !key.modifiers.meta;
    if key.modifiers != Modifiers::default()
        && !(shift_only && matches!(key.code, KeyCode::Char(_)))
    {
        if let Some(action @ AppKey::SaveRoles) = classify_management_input(input.clone()) {
            return Key::Management {
                action,
                passthrough: bytes,
            };
        }
        return Key::Passthrough(bytes);
    }
    match key.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Delete => Key::Delete,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Escape => Key::Escape,
        KeyCode::Char(ch) => Key::Char(ch),
        _ => Key::Other,
    }
}
