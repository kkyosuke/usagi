//! Terminal-independent live-pane input handling.
//!
//! [`LiveInput`] retains the distinction between semantic key events, UTF-8 text,
//! paste, and already-decoded terminal bytes. [`LiveInputClassifier`] is the only
//! place that reserves live-pane shortcuts; application-controller [`AppKey`]
//! values remain the vocabulary for management screens.

use std::time::Duration;

/// The longest interval in which a `Ctrl-O` leader accepts its follow-up.
pub const LEADER_TIMEOUT: Duration = Duration::from_secs(1);

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
    /// Open or reattach the agent pane.
    Agent,
    /// Close the active tab.
    CloseTab,
    /// Open quit confirmation.
    QuitConfirmation,
    /// Select the previous session, if one exists.
    PreviousSession,
    /// Cancel the topmost management state without forwarding ESC to the PTY.
    Escape,
}

/// A classifier result that an adapter can dispatch without daemon wire types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveInputOutput {
    /// Send these bytes to the daemon-owned terminal, exactly once.
    Passthrough(Vec<u8>),
    /// Perform a TUI-local management operation.
    Action(LiveTerminalAction),
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
            LiveInput::Text(text) => self.forward_non_key(text.into_bytes()),
            LiveInput::Paste(bytes) | LiveInput::Raw(bytes) => self.forward_non_key(bytes),
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

    fn classify_key(
        &mut self,
        now: Duration,
        leader_alive: bool,
        key: &KeyEvent,
    ) -> LiveInputOutput {
        if key.kind == KeyEventKind::Release {
            return LiveInputOutput::Swallowed;
        }
        // Escape cancels a pending leader and belongs to the TUI escape stack.
        // It must never be forwarded as a terminal byte.
        if matches!(key.code, KeyCode::Escape) {
            self.leader_at = None;
            return LiveInputOutput::Action(LiveTerminalAction::Escape);
        }
        if leader_alive {
            self.leader_at = None;
            return prefix_action(key).map_or(LiveInputOutput::Swallowed, LiveInputOutput::Action);
        }
        if is_ctrl_o(key) {
            self.leader_at = Some(now);
            return LiveInputOutput::Swallowed;
        }
        if is_ctrl_caret(key) {
            return LiveInputOutput::Action(LiveTerminalAction::PreviousSession);
        }
        LiveInputOutput::Passthrough(encode_key(key))
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

fn is_ctrl_caret(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{1e}'))
        || (matches!(key.code, KeyCode::Char('^')) && is_only_control(key.modifiers))
}

fn prefix_action(key: &KeyEvent) -> Option<LiveTerminalAction> {
    let plain = Modifiers::default();
    if key.modifiers != plain && !is_ctrl_o(key) {
        return None;
    }
    match key.code {
        KeyCode::Char('o' | '\u{0f}') => Some(LiveTerminalAction::Switch),
        KeyCode::Char('a') => Some(LiveTerminalAction::OpenCloseupModal),
        KeyCode::Char('n') | KeyCode::Right => Some(LiveTerminalAction::NextTab),
        KeyCode::Char('p') | KeyCode::Left => Some(LiveTerminalAction::PreviousTab),
        KeyCode::Char('g') => Some(LiveTerminalAction::Agent),
        KeyCode::Char('x') => Some(LiveTerminalAction::CloseTab),
        KeyCode::Char('q') => Some(LiveTerminalAction::QuitConfirmation),
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

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::ZERO;

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
                name: "ctrl c",
                input: ctrl('c'),
                expected: vec![3],
            },
            Case {
                name: "ctrl q",
                input: ctrl('q'),
                expected: vec![17],
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
    fn input_two_acceptance_table_reserves_all_prefix_actions() {
        struct Case {
            follow_up: LiveInput,
            action: LiveTerminalAction,
        }
        let cases = [
            Case {
                follow_up: key(KeyCode::Char('o')),
                action: LiveTerminalAction::Switch,
            },
            Case {
                follow_up: ctrl('o'),
                action: LiveTerminalAction::Switch,
            },
            Case {
                follow_up: key(KeyCode::Char('a')),
                action: LiveTerminalAction::OpenCloseupModal,
            },
            Case {
                follow_up: key(KeyCode::Char('n')),
                action: LiveTerminalAction::NextTab,
            },
            Case {
                follow_up: key(KeyCode::Right),
                action: LiveTerminalAction::NextTab,
            },
            Case {
                follow_up: key(KeyCode::Char('p')),
                action: LiveTerminalAction::PreviousTab,
            },
            Case {
                follow_up: key(KeyCode::Left),
                action: LiveTerminalAction::PreviousTab,
            },
            Case {
                follow_up: key(KeyCode::Char('g')),
                action: LiveTerminalAction::Agent,
            },
            Case {
                follow_up: key(KeyCode::Char('x')),
                action: LiveTerminalAction::CloseTab,
            },
            Case {
                follow_up: key(KeyCode::Char('q')),
                action: LiveTerminalAction::QuitConfirmation,
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
    fn escape_cancels_a_leader_without_becoming_a_terminal_byte() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('o')),
            LiveInputOutput::Swallowed
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(1), key(KeyCode::Escape)),
            LiveInputOutput::Action(LiveTerminalAction::Escape)
        );
        assert!(!classifier.leader_pending(Duration::from_millis(1)));
        assert_eq!(
            classifier.classify(Duration::from_millis(2), key(KeyCode::Char('n'))),
            LiveInputOutput::Passthrough(b"n".to_vec())
        );
    }

    #[test]
    fn ctrl_caret_is_reserved_once_and_not_a_prefix_follow_up() {
        let mut classifier = LiveInputClassifier::default();
        assert_eq!(
            classifier.classify(T0, ctrl('^')),
            LiveInputOutput::Action(LiveTerminalAction::PreviousSession)
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(1), ctrl('^')),
            LiveInputOutput::Action(LiveTerminalAction::PreviousSession)
        );
        assert_eq!(
            classifier.classify(Duration::from_millis(2), key(KeyCode::Char('\u{1e}'))),
            LiveInputOutput::Action(LiveTerminalAction::PreviousSession)
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
}
