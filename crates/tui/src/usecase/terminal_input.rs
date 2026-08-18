//! Terminal-independent live-pane input handling.
//!
//! [`LiveInput`] retains the distinction between semantic key events, UTF-8 text,
//! paste, and already-decoded terminal bytes. [`LiveInputClassifier`] is the only
//! place that reserves live-pane shortcuts; application-controller [`AppKey`]
//! values remain the vocabulary for management screens.

use std::time::Duration;

use usagi_core::usecase::vt_screen::MouseProtocolEncoding;

/// The longest interval in which a `Ctrl-O` leader accepts its follow-up.
pub const LEADER_TIMEOUT: Duration = Duration::from_secs(1);
/// Arrow presses emitted for one alternate-screen wheel notch.
pub const WHEEL_LINES: usize = 3;

/// A terminal key code, independent of any terminal-event library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    /// A Unicode scalar value.
    Char(char),
    /// Return / Enter.
    Enter,
    /// Backspace.
    Backspace,
    /// Tab and reverse Tab.
    Tab,
    /// Shift-Tab.
    BackTab,
    /// Escape.
    Escape,
    /// Cursor keys.
    Up,
    /// Cursor keys.
    Down,
    /// Cursor keys.
    Left,
    /// Cursor keys.
    Right,
    /// Navigation keys.
    Home,
    /// Navigation keys.
    End,
    /// Navigation keys.
    PageUp,
    /// Navigation keys.
    PageDown,
    /// Editing keys.
    Insert,
    /// Editing keys.
    Delete,
    /// A function key.
    Function(u8),
    /// A terminal-specific key that has no portable encoding.
    Unknown,
}

/// Modifier state reported with a key event.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Modifier bits are independently reported by terminals.
pub struct Modifiers {
    /// Shift modifier.
    pub shift: bool,
    /// Control modifier.
    pub control: bool,
    /// Alt / Meta modifier.
    pub alt: bool,
    /// Super / Command modifier.
    pub super_: bool,
    /// Hyper modifier.
    pub hyper: bool,
    /// Meta modifier.
    pub meta: bool,
}

/// The phase of a physical key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind {
    /// A key was pressed.
    Press,
    /// An auto-repeat was reported.
    Repeat,
    /// A key was released.
    Release,
}

/// A semantic key event and its optional original terminal encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// Terminal-independent key identity.
    pub code: KeyCode,
    /// Modifier state at the event.
    pub modifiers: Modifiers,
    /// Press, repeat, or release.
    pub kind: KeyEventKind,
    /// Original bytes when the terminal backend exposes them. They take priority
    /// over the portable encoder so no terminal-specific sequence is lost.
    pub raw_bytes: Vec<u8>,
}

impl KeyEvent {
    /// Creates a key event that uses the portable encoder.
    #[must_use]
    pub fn new(code: KeyCode, modifiers: Modifiers, kind: KeyEventKind) -> Self {
        Self {
            code,
            modifiers,
            kind,
            raw_bytes: Vec::new(),
        }
    }
}

/// Input received while a daemon-owned terminal pane is live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveInput {
    /// A semantic keyboard event.
    Key(KeyEvent),
    /// UTF-8 text delivered independently of a physical key.
    Text(String),
    /// Paste payload; it must remain one ordered payload.
    Paste(Vec<u8>),
    /// Bytes supplied by a terminal backend without a semantic key event.
    Raw(Vec<u8>),
    /// A left-button press at a 0-based terminal cell. Mouse input is not
    /// forwarded to a daemon-owned terminal; the presentation layer owns its
    /// sidebar hit testing.
    Mouse { column: u16, row: u16 },
    /// Pointer wheel moved toward older terminal output.
    WheelUp { column: u16, row: u16 },
    /// Pointer wheel moved toward newer terminal output.
    WheelDown { column: u16, row: u16 },
    /// Pointer lifecycle for terminal-output click/selection. It never reaches
    /// the PTY.
    Pointer(PointerEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerEvent {
    pub kind: PointerKind,
    pub column: u16,
    pub row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerKind {
    Down,
    Drag,
    Up,
}

/// terminal、backend、timer を controller へ渡す統一 runtime stream。
///
/// `B` は daemon wire 型ではなく、adapter が投影した TUI-local backend event にする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent<B> {
    /// semantic key、text、または paste payload。
    Input(LiveInput),
    /// terminal geometry。width（columns）を先に持つ。
    Resize { width: u16, height: u16 },
    /// 定期的な runtime tick。
    Tick,
    /// backend receiver から届いた TUI-local event。
    Backend(B),
}

/// A TUI-local action reserved from the live terminal stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveTerminalAction {
    /// Return to Switch mode.
    Switch,
    /// Open the active target's Closeup modal.
    OpenCloseupModal,
    /// Select the next tab.
    NextTab,
    /// Select the previous tab.
    PreviousTab,
    /// Move the selected tab one slot toward the next tab.
    MoveTabNext,
    /// Move the selected tab one slot toward the previous tab.
    MoveTabPrevious,
    /// Open or reattach the agent pane.
    Agent,
    /// Toggle the Home Director mode drawer (`Ctrl-O Ctrl-G`). It is the frontmost
    /// Home surface: opening it never mutates the background route, selection, or
    /// active managed session, and re-issuing it closes the drawer.
    Director,
    /// Open the Home Director mode drawer and its explicit New CLI picker
    /// (`Ctrl-O n`). Plain `n` is intentionally distinct from `Ctrl-N`, which
    /// remains [`LiveTerminalAction::NextTab`].
    DirectorNew,
    /// Close the active tab.
    CloseTab,
    /// Explicitly resume the selected interrupted Agent tab (#510). Nothing else
    /// starts a provider resume.
    ResumeTab,
    /// Open quit confirmation.
    QuitConfirmation,
    /// Scroll the focused terminal pane one line toward older output.
    ScrollUp,
    /// Scroll the focused terminal pane one line toward the live bottom.
    ScrollDown,
    /// Return the focused terminal pane to the live bottom in one step, so it
    /// follows new output again. A scrolled viewport holds its rows against
    /// everything a live Agent appends, which is what makes reading history
    /// possible and what would otherwise leave the reader thousands of rows of
    /// `ScrollDown` away from the newest output.
    ScrollBottom,
    /// A physical wheel notch, routed after consulting the program's DEC modes.
    Wheel { up: bool, column: u16, row: u16 },
}

/// A control chord reserved globally when no live-terminal leader is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalControlChord {
    /// Interrupt / quit (`Ctrl-C`).
    CtrlC,
    /// Open workspace quit confirmation (`Ctrl-Q`).
    CtrlQ,
    /// Unregister the selected workspace (`Ctrl-D`).
    CtrlD,
}

/// A classifier result that an adapter can dispatch without daemon wire types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveInputOutput {
    /// Send these bytes to the daemon-owned terminal, exactly once.
    Passthrough(Vec<u8>),
    /// Perform a TUI-local management operation.
    Action(LiveTerminalAction),
    /// Dispatch a global control chord after leader precedence has been resolved.
    GlobalControl(GlobalControlChord),
    /// Consume input without forwarding it (leader, unknown follow-up, release).
    Swallowed,
}

/// Pure state machine for the default `Ctrl-O` live-terminal prefix scheme.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LiveInputClassifier {
    leader_at: Option<Duration>,
}

impl LiveInputClassifier {
    /// Classifies one input at an injected monotonic timestamp.
    ///
    /// `now` is deliberately supplied by the caller: tests and future event
    /// loops can drive timeout behaviour without reading a clock here.
    #[must_use]
    pub fn classify(&mut self, now: Duration, input: LiveInput) -> LiveInputOutput {
        let leader_alive = self
            .leader_at
            .is_some_and(|started| now.saturating_sub(started) < LEADER_TIMEOUT);
        if !leader_alive {
            self.leader_at = None;
        }

        match input {
            LiveInput::Key(key) => self.classify_key(now, leader_alive, &key),
            LiveInput::WheelUp { column, row } => {
                self.leader_at = None;
                LiveInputOutput::Action(LiveTerminalAction::Wheel {
                    up: true,
                    column,
                    row,
                })
            }
            LiveInput::WheelDown { column, row } => {
                self.leader_at = None;
                LiveInputOutput::Action(LiveTerminalAction::Wheel {
                    up: false,
                    column,
                    row,
                })
            }
            LiveInput::Text(text) => self.classify_bytes(leader_alive, text.into_bytes()),
            LiveInput::Raw(bytes) => self.classify_bytes(leader_alive, bytes),
            LiveInput::Paste(bytes) => self.forward_non_key(bytes),
            LiveInput::Mouse { .. } | LiveInput::Pointer(_) => {
                self.leader_at = None;
                if leader_alive {
                    LiveInputOutput::Swallowed
                } else {
                    LiveInputOutput::Passthrough(Vec::new())
                }
            }
        }
    }

    /// Returns whether a leader is still waiting at `now`.
    #[must_use]
    pub fn leader_pending(&self, now: Duration) -> bool {
        self.leader_at
            .is_some_and(|started| now.saturating_sub(started) < LEADER_TIMEOUT)
    }

    fn forward_non_key(&mut self, bytes: Vec<u8>) -> LiveInputOutput {
        self.leader_at = None;
        LiveInputOutput::Passthrough(bytes)
    }

    fn classify_bytes(&mut self, leader_alive: bool, bytes: Vec<u8>) -> LiveInputOutput {
        self.leader_at = None;
        if leader_alive {
            if bytes == [7] {
                return LiveInputOutput::Action(LiveTerminalAction::Director);
            }
            return LiveInputOutput::Swallowed;
        }
        global_control_bytes(&bytes).map_or(
            LiveInputOutput::Passthrough(bytes),
            LiveInputOutput::GlobalControl,
        )
    }

    fn classify_key(
        &mut self,
        now: Duration,
        leader_alive: bool,
        key: &KeyEvent,
    ) -> LiveInputOutput {
        if key.kind == KeyEventKind::Release {
            self.leader_at = None;
            return LiveInputOutput::Swallowed;
        }
        if leader_alive {
            self.leader_at = None;
            if matches!(key.code, KeyCode::Char('g')) && key.modifiers == Modifiers::default() {
                return LiveInputOutput::Passthrough(encode_key(key));
            }
            return prefix_action(key).map_or(LiveInputOutput::Swallowed, LiveInputOutput::Action);
        }
        if is_ctrl_o(key) {
            self.leader_at = Some(now);
            return LiveInputOutput::Swallowed;
        }
        if let Some(control) = global_control_key(key) {
            return LiveInputOutput::GlobalControl(control);
        }
        LiveInputOutput::Passthrough(encode_key(key))
    }
}

fn global_control_key(key: &KeyEvent) -> Option<GlobalControlChord> {
    match key.code {
        KeyCode::Char('\u{3}') => Some(GlobalControlChord::CtrlC),
        KeyCode::Char('\u{11}') => Some(GlobalControlChord::CtrlQ),
        KeyCode::Char('\u{4}') => Some(GlobalControlChord::CtrlD),
        KeyCode::Char('c') if is_only_control(key.modifiers) => Some(GlobalControlChord::CtrlC),
        KeyCode::Char('q') if is_only_control(key.modifiers) => Some(GlobalControlChord::CtrlQ),
        KeyCode::Char('d') if is_only_control(key.modifiers) => Some(GlobalControlChord::CtrlD),
        _ => None,
    }
}

fn global_control_bytes(bytes: &[u8]) -> Option<GlobalControlChord> {
    match bytes {
        [3] => Some(GlobalControlChord::CtrlC),
        [17] => Some(GlobalControlChord::CtrlQ),
        [4] => Some(GlobalControlChord::CtrlD),
        _ => None,
    }
}

fn is_only_control(modifiers: Modifiers) -> bool {
    modifiers.control
        && !modifiers.shift
        && !modifiers.alt
        && !modifiers.super_
        && !modifiers.hyper
        && !modifiers.meta
}

fn is_ctrl_o(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{0f}'))
        || (matches!(key.code, KeyCode::Char('o')) && is_only_control(key.modifiers))
}

fn is_ctrl_a(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{1}'))
        || (matches!(key.code, KeyCode::Char('a')) && is_only_control(key.modifiers))
}

fn is_ctrl_n(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{e}'))
        || (matches!(key.code, KeyCode::Char('n')) && is_only_control(key.modifiers))
}

fn is_ctrl_p(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{10}'))
        || (matches!(key.code, KeyCode::Char('p')) && is_only_control(key.modifiers))
}

fn is_ctrl_x(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{18}'))
        || (matches!(key.code, KeyCode::Char('x')) && is_only_control(key.modifiers))
}

fn is_ctrl_g(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{7}'))
        || (matches!(key.code, KeyCode::Char('g')) && is_only_control(key.modifiers))
}

fn prefix_action(key: &KeyEvent) -> Option<LiveTerminalAction> {
    if is_ctrl_o(key) {
        return Some(LiveTerminalAction::Switch);
    }
    if is_ctrl_a(key) {
        return Some(LiveTerminalAction::OpenCloseupModal);
    }
    if is_ctrl_n(key) {
        return Some(LiveTerminalAction::NextTab);
    }
    if is_ctrl_p(key) {
        return Some(LiveTerminalAction::PreviousTab);
    }
    if is_ctrl_x(key) {
        return Some(LiveTerminalAction::CloseTab);
    }
    if is_ctrl_g(key) {
        return Some(LiveTerminalAction::Director);
    }
    // Plain follow-ups for the live-terminal view controls the Home reducer does
    // not own: scroll the PTY output and close the focused tab. A
    // modified variant (other than the control chords above) is not a prefix
    // action and falls through to the PTY.
    if key.modifiers != Modifiers::default() {
        return None;
    }
    match key.code {
        KeyCode::Char('n') => Some(LiveTerminalAction::DirectorNew),
        KeyCode::Char('x') => Some(LiveTerminalAction::CloseTab),
        KeyCode::Char('r') => Some(LiveTerminalAction::ResumeTab),
        KeyCode::Char(']') => Some(LiveTerminalAction::MoveTabNext),
        KeyCode::Char('[') => Some(LiveTerminalAction::MoveTabPrevious),
        KeyCode::Char('u') | KeyCode::Up => Some(LiveTerminalAction::ScrollUp),
        KeyCode::Char('d') | KeyCode::Down => Some(LiveTerminalAction::ScrollDown),
        KeyCode::Char('b') | KeyCode::End => Some(LiveTerminalAction::ScrollBottom),
        _ => None,
    }
}

/// Encodes a press or repeat in the portable terminal form.
///
/// Original bytes attached to [`KeyEvent`] are returned unchanged. Releases and
/// unknown semantic keys without original bytes have no terminal encoding.
#[must_use]
pub fn encode_key(key: &KeyEvent) -> Vec<u8> {
    if key.kind == KeyEventKind::Release {
        return Vec::new();
    }
    if !key.raw_bytes.is_empty() {
        return key.raw_bytes.clone();
    }
    let mut bytes = match key.code {
        KeyCode::Char(character) if key.modifiers.control => {
            vec![(character.to_ascii_uppercase() as u8) & 0x1f]
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Escape => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Function(number) => function_key_bytes(number),
        KeyCode::Unknown => Vec::new(),
    };
    if key.modifiers.alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

fn function_key_bytes(number: u8) -> Vec<u8> {
    match number {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[18~".to_vec(),
        8 => b"\x1b[19~".to_vec(),
        9 => b"\x1b[20~".to_vec(),
        10 => b"\x1b[21~".to_vec(),
        11 => b"\x1b[23~".to_vec(),
        12 => b"\x1b[24~".to_vec(),
        _ => Vec::new(),
    }
}

/// Encodes one wheel notch for a program that enabled DEC mouse reporting.
/// `column` and `row` are zero-based terminal-viewport cells.
#[must_use]
pub fn encode_mouse_wheel(
    up: bool,
    column: usize,
    row: usize,
    encoding: MouseProtocolEncoding,
) -> Vec<u8> {
    let button = if up { 64_u32 } else { 65_u32 };
    let column = u32::try_from(column).unwrap_or(u32::MAX).saturating_add(1);
    let row = u32::try_from(row).unwrap_or(u32::MAX).saturating_add(1);
    match encoding {
        MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{column};{row}M").into_bytes(),
        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            let mut bytes = b"\x1b[M".to_vec();
            for field in [button, column, row] {
                let value = field.saturating_add(32);
                if let (MouseProtocolEncoding::Utf8, Some(character)) =
                    (encoding, char::from_u32(value))
                {
                    let mut encoded = [0_u8; 4];
                    bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                    continue;
                }
                bytes.push(u8::try_from(value).unwrap_or(u8::MAX));
            }
            bytes
        }
    }
}

/// Emulates a terminal's alternate-scroll mode for a full-screen program that
/// did not enable mouse reporting.
#[must_use]
pub fn encode_wheel_arrows(up: bool, application_cursor: bool) -> Vec<u8> {
    let arrow = match (up, application_cursor) {
        (true, true) => b"\x1bOA".as_slice(),
        (true, false) => b"\x1b[A".as_slice(),
        (false, true) => b"\x1bOB".as_slice(),
        (false, false) => b"\x1b[B".as_slice(),
    };
    arrow.repeat(WHEEL_LINES)
}

/// Bracketed-paste start marker (DECSET 2004). A program that requested the mode
/// treats everything up to [`PASTE_END`] as one paste.
const PASTE_START: &str = "\x1b[200~";
/// Bracketed-paste end marker (DECSET 2004).
const PASTE_END: &str = "\x1b[201~";

/// Wrap a paste payload in bracketed-paste markers so a program that enabled
/// bracketed paste (agent CLIs such as `claude` / `codex`) inserts the
/// multi-line text as one block instead of submitting on every embedded newline.
///
/// Any [`PASTE_END`] marker inside `text` is removed first: leaving it in would
/// let pasted content close the paste early and have its tail run as live
/// keystrokes (paste injection), so — like real terminals — the embedded
/// terminator is neutralised.
#[must_use]
pub fn encode_bracketed_paste(text: &str) -> Vec<u8> {
    let body = text.replace(PASTE_END, "");
    let mut out = Vec::with_capacity(PASTE_START.len() + body.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_START.as_bytes());
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(PASTE_END.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;

    #[test]
    fn bracketed_paste_wraps_a_multi_line_payload_in_markers() {
        assert_eq!(
            encode_bracketed_paste("line1\nline2"),
            b"\x1b[200~line1\nline2\x1b[201~".to_vec()
        );
    }

    #[test]
    fn bracketed_paste_strips_embedded_end_markers_to_block_injection() {
        assert_eq!(
            encode_bracketed_paste("safe\x1b[201~rm -rf /\r"),
            b"\x1b[200~saferm -rf /\r\x1b[201~".to_vec()
        );
    }

    fn key(code: KeyCode) -> LiveInput {
        LiveInput::Key(KeyEvent::new(
            code,
            Modifiers::default(),
            KeyEventKind::Press,
        ))
    }

    fn ctrl(character: char) -> LiveInput {
        LiveInput::Key(KeyEvent::new(
            KeyCode::Char(character),
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ))
    }

    #[test]
    fn platform_copy_shortcuts_reach_the_terminal() {
        let command_c = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('c'),
            Modifiers {
                super_: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(
            LiveInputClassifier::default().classify(T0, command_c),
            LiveInputOutput::Passthrough(b"c".to_vec())
        );
    }

    #[test]
    fn input_one_acceptance_table_preserves_live_terminal_bytes() {
        struct Case {
            name: &'static str,
            input: LiveInput,
            expected: Vec<u8>,
        }
        let cases = [
            Case {
                name: "plain q",
                input: key(KeyCode::Char('q')),
                expected: b"q".to_vec(),
            },
            Case {
                name: "bare n",
                input: key(KeyCode::Char('n')),
                expected: b"n".to_vec(),
            },
            Case {
                name: "escape",
                input: key(KeyCode::Escape),
                expected: vec![0x1b],
            },
            Case {
                name: "cjk utf8",
                input: LiveInput::Text("うさぎ".into()),
                expected: "うさぎ".as_bytes().to_vec(),
            },
            Case {
                name: "up",
                input: key(KeyCode::Up),
                expected: b"\x1b[A".to_vec(),
            },
            Case {
                name: "home",
                input: key(KeyCode::Home),
                expected: b"\x1b[H".to_vec(),
            },
            Case {
                name: "end",
                input: key(KeyCode::End),
                expected: b"\x1b[F".to_vec(),
            },
            Case {
                name: "page up",
                input: key(KeyCode::PageUp),
                expected: b"\x1b[5~".to_vec(),
            },
            Case {
                name: "page down",
                input: key(KeyCode::PageDown),
                expected: b"\x1b[6~".to_vec(),
            },
            Case {
                name: "alt chord",
                input: LiveInput::Key(KeyEvent::new(
                    KeyCode::Char('f'),
                    Modifiers {
                        alt: true,
                        ..Modifiers::default()
                    },
                    KeyEventKind::Press,
                )),
                expected: b"\x1bf".to_vec(),
            },
            Case {
                name: "paste",
                input: LiveInput::Paste(vec![0xe3, 0x81, 0x86, b'\n']),
                expected: vec![0xe3, 0x81, 0x86, b'\n'],
            },
            Case {
                name: "raw",
                input: LiveInput::Raw(vec![0x1b, b'[', b'9', b'9', b'~']),
                expected: vec![0x1b, b'[', b'9', b'9', b'~'],
            },
        ];
        for case in cases {
            let output = LiveInputClassifier::default().classify(T0, case.input);
            assert_eq!(
                output,
                LiveInputOutput::Passthrough(case.expected),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn press_and_repeat_forward_once_but_release_is_swallowed() {
        for kind in [KeyEventKind::Press, KeyEventKind::Repeat] {
            let output = LiveInputClassifier::default().classify(
                T0,
                LiveInput::Key(KeyEvent::new(
                    KeyCode::Char('z'),
                    Modifiers::default(),
                    kind,
                )),
            );
            assert_eq!(output, LiveInputOutput::Passthrough(b"z".to_vec()));
        }
        let output = LiveInputClassifier::default().classify(
            T0,
            LiveInput::Key(KeyEvent::new(
                KeyCode::Char('z'),
                Modifiers::default(),
                KeyEventKind::Release,
            )),
        );
        assert_eq!(output, LiveInputOutput::Swallowed);
    }

    #[test]
    fn raw_key_bytes_win_over_portable_encoding() {
        let key = KeyEvent {
            code: KeyCode::Up,
            modifiers: Modifiers::default(),
            kind: KeyEventKind::Press,
            raw_bytes: vec![1, 2, 3],
        };
        assert_eq!(encode_key(&key), vec![1, 2, 3]);
    }

    #[test]
    fn input_two_acceptance_table_reserves_only_documented_prefix_actions() {
        struct Case {
            follow_up: LiveInput,
            action: LiveTerminalAction,
        }
        let cases = [
            Case {
                follow_up: ctrl('o'),
                action: LiveTerminalAction::Switch,
            },
            Case {
                follow_up: ctrl('a'),
                action: LiveTerminalAction::OpenCloseupModal,
            },
            Case {
                follow_up: ctrl('n'),
                action: LiveTerminalAction::NextTab,
            },
            Case {
                follow_up: key(KeyCode::Char('n')),
                action: LiveTerminalAction::DirectorNew,
            },
            Case {
                follow_up: ctrl('p'),
                action: LiveTerminalAction::PreviousTab,
            },
            Case {
                follow_up: ctrl('g'),
                action: LiveTerminalAction::Director,
            },
            Case {
                follow_up: key(KeyCode::Char('\u{7}')),
                action: LiveTerminalAction::Director,
            },
            // View controls the reducer does not own: tab close and scroll.
            Case {
                follow_up: key(KeyCode::Char('x')),
                action: LiveTerminalAction::CloseTab,
            },
            Case {
                follow_up: ctrl('x'),
                action: LiveTerminalAction::CloseTab,
            },
            Case {
                follow_up: key(KeyCode::Char('\u{18}')),
                action: LiveTerminalAction::CloseTab,
            },
            // The only explicit per-tab provider resume (#510).
            Case {
                follow_up: key(KeyCode::Char('r')),
                action: LiveTerminalAction::ResumeTab,
            },
            Case {
                follow_up: key(KeyCode::Char(']')),
                action: LiveTerminalAction::MoveTabNext,
            },
            Case {
                follow_up: key(KeyCode::Char('[')),
                action: LiveTerminalAction::MoveTabPrevious,
            },
            Case {
                follow_up: key(KeyCode::Char('u')),
                action: LiveTerminalAction::ScrollUp,
            },
            Case {
                follow_up: key(KeyCode::Up),
                action: LiveTerminalAction::ScrollUp,
            },
            Case {
                follow_up: key(KeyCode::Char('d')),
                action: LiveTerminalAction::ScrollDown,
            },
            Case {
                follow_up: key(KeyCode::Down),
                action: LiveTerminalAction::ScrollDown,
            },
            Case {
                follow_up: key(KeyCode::Char('b')),
                action: LiveTerminalAction::ScrollBottom,
            },
            Case {
                follow_up: key(KeyCode::End),
                action: LiveTerminalAction::ScrollBottom,
            },
        ];
        for case in cases {
            let mut classifier = LiveInputClassifier::default();
            assert_eq!(
                classifier.classify(T0, ctrl('o')),
                LiveInputOutput::Swallowed
            );
            assert_eq!(
                classifier.classify(Duration::from_millis(1), case.follow_up),
                LiveInputOutput::Action(case.action)
            );
        }
    }

    #[test]
    fn director_chord_distinguishes_ctrl_g_from_plain_g() {
        for follow_up in [
            ctrl('g'),
            key(KeyCode::Char('\u{7}')),
            LiveInput::Raw(vec![7]),
        ] {
            let mut classifier = LiveInputClassifier::default();
            assert_eq!(
                classifier.classify(T0, ctrl('o')),
                LiveInputOutput::Swallowed
            );
            assert_eq!(
                classifier.classify(Duration::from_millis(1), follow_up),
                LiveInputOutput::Action(LiveTerminalAction::Director)
            );
        }

        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('o')),
            LiveInputOutput::Swallowed
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(1), key(KeyCode::Char('g'))),
            LiveInputOutput::Passthrough(b"g".to_vec())
        );
    }

    #[test]
    fn plain_view_control_keys_reach_the_pty_without_a_leader() {
        // The restored follow-ups are reserved only after a Ctrl-O leader; a bare
        // press still types into the terminal.
        for character in ['c', 'x', 'u', 'd'] {
            assert_eq!(
                LiveInputClassifier::default().classify(T0, key(KeyCode::Char(character))),
                LiveInputOutput::Passthrough(character.to_string().into_bytes())
            );
        }
        // Ctrl-X is reserved only as a leader follow-up. Both common decoder
        // forms remain a single PTY control byte when there is no Ctrl-O leader.
        for input in [ctrl('x'), key(KeyCode::Char('\u{18}'))] {
            assert_eq!(
                LiveInputClassifier::default().classify(T0, input),
                LiveInputOutput::Passthrough(vec![0x18])
            );
        }
    }

    #[test]
    fn global_control_table_matches_semantic_and_raw_forms_without_a_leader() {
        let cases = [
            (ctrl('c'), GlobalControlChord::CtrlC),
            (key(KeyCode::Char('\u{3}')), GlobalControlChord::CtrlC),
            (LiveInput::Raw(vec![3]), GlobalControlChord::CtrlC),
            (ctrl('q'), GlobalControlChord::CtrlQ),
            (key(KeyCode::Char('\u{11}')), GlobalControlChord::CtrlQ),
            (LiveInput::Raw(vec![17]), GlobalControlChord::CtrlQ),
            (ctrl('d'), GlobalControlChord::CtrlD),
            (key(KeyCode::Char('\u{4}')), GlobalControlChord::CtrlD),
            (LiveInput::Raw(vec![4]), GlobalControlChord::CtrlD),
        ];
        for (input, expected) in cases {
            assert_eq!(
                LiveInputClassifier::default().classify(T0, input),
                LiveInputOutput::GlobalControl(expected)
            );
        }
    }

    #[test]
    fn pending_leader_consumes_every_global_control_form_and_resets() {
        let follow_ups = [
            ctrl('c'),
            key(KeyCode::Char('\u{3}')),
            LiveInput::Raw(vec![3]),
            ctrl('q'),
            key(KeyCode::Char('\u{11}')),
            LiveInput::Raw(vec![17]),
            ctrl('d'),
            key(KeyCode::Char('\u{4}')),
            LiveInput::Raw(vec![4]),
        ];
        for follow_up in follow_ups {
            let mut classifier = LiveInputClassifier::default();
            assert_eq!(
                classifier.classify(T0, ctrl('o')),
                LiveInputOutput::Swallowed
            );
            assert_eq!(
                classifier.classify(Duration::from_millis(1), follow_up),
                LiveInputOutput::Swallowed
            );
            assert_eq!(
                classifier.classify(Duration::from_millis(2), key(KeyCode::Char('z'))),
                LiveInputOutput::Passthrough(b"z".to_vec())
            );
        }
    }

    #[test]
    fn every_non_leader_key_is_forwarded_to_the_pane() {
        let cases = [
            (ctrl('r'), vec![0x12]),
            (ctrl('^'), vec![0x1e]),
            (
                LiveInput::Key(KeyEvent::new(
                    KeyCode::Char('f'),
                    Modifiers {
                        alt: true,
                        ..Modifiers::default()
                    },
                    KeyEventKind::Press,
                )),
                b"\x1bf".to_vec(),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                LiveInputClassifier::default().classify(T0, input),
                LiveInputOutput::Passthrough(expected)
            );
        }
    }

    #[test]
    fn timeout_makes_the_next_input_fresh_passthrough() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('o')),
            LiveInputOutput::Swallowed
        );
        assert!(classifier.leader_pending(Duration::from_millis(999)));
        assert_eq!(
            classifier.classify(LEADER_TIMEOUT, key(KeyCode::Char('q'))),
            LiveInputOutput::Passthrough(b"q".to_vec())
        );
        assert!(!classifier.leader_pending(LEADER_TIMEOUT));
    }

    #[test]
    fn director_follow_up_is_plain_terminal_input_after_leader_timeout() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('o')),
            LiveInputOutput::Swallowed
        );
        assert_eq!(
            classifier.classify(LEADER_TIMEOUT, key(KeyCode::Char('g'))),
            LiveInputOutput::Passthrough(b"g".to_vec())
        );
    }

    #[test]
    fn timeout_boundary_restores_global_control_semantics() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('o')),
            LiveInputOutput::Swallowed
        );
        assert_eq!(
            classifier.classify(LEADER_TIMEOUT, LiveInput::Raw(vec![3])),
            LiveInputOutput::GlobalControl(GlobalControlChord::CtrlC)
        );
        assert!(!classifier.leader_pending(LEADER_TIMEOUT));
    }

    #[test]
    fn release_and_repeat_follow_ups_consume_and_reset_a_pending_leader() {
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            let mut classifier = LiveInputClassifier::default();
            assert_eq!(
                classifier.classify(T0, ctrl('o')),
                LiveInputOutput::Swallowed
            );
            let follow_up = LiveInput::Key(KeyEvent::new(
                KeyCode::Char('c'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
                kind,
            ));
            assert_eq!(
                classifier.classify(Duration::from_millis(1), follow_up),
                LiveInputOutput::Swallowed
            );
            assert_eq!(
                classifier.classify(Duration::from_millis(2), ctrl('q')),
                LiveInputOutput::GlobalControl(GlobalControlChord::CtrlQ)
            );
        }
    }

    #[test]
    fn wheel_events_keep_the_pointer_cell_for_mode_aware_routing() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, LiveInput::WheelUp { column: 4, row: 9 }),
            LiveInputOutput::Action(LiveTerminalAction::Wheel {
                up: true,
                column: 4,
                row: 9,
            })
        );
        assert_eq!(
            classifier.classify(T0, LiveInput::WheelDown { column: 2, row: 7 }),
            LiveInputOutput::Action(LiveTerminalAction::Wheel {
                up: false,
                column: 2,
                row: 7,
            })
        );
    }

    #[test]
    fn unknown_leader_follow_up_is_swallowed_once() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('o')),
            LiveInputOutput::Swallowed
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(1), key(KeyCode::Char('z'))),
            LiveInputOutput::Swallowed
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(2), key(KeyCode::Char('z'))),
            LiveInputOutput::Passthrough(b"z".to_vec())
        );
    }

    #[test]
    fn paste_clears_a_pending_leader_without_losing_order() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('o')),
            LiveInputOutput::Swallowed
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(1), LiveInput::Paste(b"abc".to_vec())),
            LiveInputOutput::Passthrough(b"abc".to_vec())
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(2), key(KeyCode::Char('x'))),
            LiveInputOutput::Passthrough(b"x".to_vec())
        );
    }

    #[test]
    fn mouse_input_clears_a_pending_leader_without_reaching_the_terminal() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('o')),
            LiveInputOutput::Swallowed
        );
        assert_eq!(
            classifier.classify(
                Duration::from_millis(1),
                LiveInput::Mouse { column: 4, row: 9 },
            ),
            LiveInputOutput::Swallowed
        );
        assert!(!classifier.leader_pending(Duration::from_millis(1)));
    }

    #[test]
    fn pointer_inputs_without_a_leader_are_left_for_the_terminal_adapter() {
        for input in [
            LiveInput::Mouse { column: 4, row: 9 },
            LiveInput::Pointer(PointerEvent {
                kind: PointerKind::Drag,
                column: 4,
                row: 9,
            }),
        ] {
            assert_eq!(
                LiveInputClassifier::default().classify(T0, input),
                LiveInputOutput::Passthrough(Vec::new())
            );
        }
    }

    #[test]
    fn encoder_covers_remaining_portable_key_variants() {
        let cases = [
            (KeyCode::Backspace, vec![0x7f]),
            (KeyCode::Tab, vec![b'\t']),
            (KeyCode::BackTab, b"\x1b[Z".to_vec()),
            (KeyCode::Down, b"\x1b[B".to_vec()),
            (KeyCode::Left, b"\x1b[D".to_vec()),
            (KeyCode::Right, b"\x1b[C".to_vec()),
            (KeyCode::Insert, b"\x1b[2~".to_vec()),
            (KeyCode::Delete, b"\x1b[3~".to_vec()),
            (KeyCode::Function(1), b"\x1bOP".to_vec()),
            (KeyCode::Function(2), b"\x1bOQ".to_vec()),
            (KeyCode::Function(3), b"\x1bOR".to_vec()),
            (KeyCode::Function(4), b"\x1bOS".to_vec()),
            (KeyCode::Function(5), b"\x1b[15~".to_vec()),
            (KeyCode::Function(6), b"\x1b[17~".to_vec()),
            (KeyCode::Function(7), b"\x1b[18~".to_vec()),
            (KeyCode::Function(8), b"\x1b[19~".to_vec()),
            (KeyCode::Function(9), b"\x1b[20~".to_vec()),
            (KeyCode::Function(10), b"\x1b[21~".to_vec()),
            (KeyCode::Function(11), b"\x1b[23~".to_vec()),
            (KeyCode::Function(12), b"\x1b[24~".to_vec()),
            (KeyCode::Function(13), Vec::new()),
            (KeyCode::Unknown, Vec::new()),
        ];
        for (code, expected) in cases {
            assert_eq!(
                encode_key(&KeyEvent::new(
                    code,
                    Modifiers::default(),
                    KeyEventKind::Press
                )),
                expected
            );
        }
        let alt_enter = KeyEvent::new(
            KeyCode::Enter,
            Modifiers {
                alt: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        );
        assert_eq!(encode_key(&alt_enter), b"\x1b\r".to_vec());
        let release = KeyEvent::new(
            KeyCode::Function(2),
            Modifiers::default(),
            KeyEventKind::Release,
        );
        assert!(encode_key(&release).is_empty());
    }

    #[test]
    fn modifier_distinctions_do_not_steal_non_default_chords() {
        let mut classifier = LiveInputClassifier::default();
        let shifted = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('O'),
            Modifiers {
                control: true,
                shift: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(
            classifier.classify(T0, shifted),
            LiveInputOutput::Passthrough(vec![15])
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(1), ctrl('o')),
            LiveInputOutput::Swallowed
        );
        let alt_follow_up = LiveInput::Key(KeyEvent::new(
            KeyCode::Char('q'),
            Modifiers {
                alt: true,
                ..Modifiers::default()
            },
            KeyEventKind::Press,
        ));
        assert_eq!(
            classifier.classify(Duration::from_millis(2), alt_follow_up),
            LiveInputOutput::Swallowed
        );
    }

    #[test]
    fn mouse_wheel_encoding_matches_sgr_and_legacy_terminal_protocols() {
        assert_eq!(
            encode_mouse_wheel(true, 4, 9, MouseProtocolEncoding::Sgr),
            b"\x1b[<64;5;10M"
        );
        assert_eq!(
            encode_mouse_wheel(false, 0, 0, MouseProtocolEncoding::Default),
            vec![0x1b, b'[', b'M', 97, 33, 33]
        );
    }

    #[test]
    fn alternate_wheel_uses_the_program_cursor_key_mode() {
        assert_eq!(encode_wheel_arrows(true, false), b"\x1b[A".repeat(3));
        assert_eq!(encode_wheel_arrows(false, true), b"\x1bOB".repeat(3));
    }
}
