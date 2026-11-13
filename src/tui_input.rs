//! crossterm を TUI の統一 runtime input stream へ接続する合成ルート adapter。
//!
//! crossterm 依存はこの binary crate に閉じ、TUI crate へ渡す値は terminal 非依存の
//! [`LiveInput`] と [`RuntimeEvent`] にする。`EventPump::next` は terminal、backend、tick
//! をこの順に観測するため、同じ poll cycle で同時に ready だった event の順序も決定的である。

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode as CrosstermKeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use usagi_tui::usecase::terminal_input::{
    KeyCode, KeyEvent as InputKeyEvent, KeyEventKind as InputKeyEventKind, LiveInput, Modifiers,
    PointerEvent, PointerKind, RuntimeEvent,
};

mod sgr_mouse;

/// Maximum ready wheel events folded into one frame. The bound lets a sustained
/// input stream redraw periodically instead of draining forever.
const WHEEL_COALESCE_LIMIT: usize = 32;

/// Bound Escape lookahead to two interactive frames. Crossterm can emit an
/// isolated Escape when a tty read ends immediately before an SGR mouse tail.
const MOUSE_REPORT_GRACE: Duration = Duration::from_millis(32);
/// Three bounded decimal fields, their separators, and the SGR introducer/end.
const MOUSE_REPORT_MAX_CHARS: usize = 20;

/// poll/read を差し替えられる crossterm event source。
pub trait CrosstermEventSource {
    /// Monotonic clock used to bound Escape lookahead; fakes can fix or advance it.
    fn now(&mut self) -> Instant {
        Instant::now()
    }
    /// `timeout` まで terminal event を待てるか調べる。
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    /// `poll` が ready を返した後の event を読む。
    fn read(&mut self) -> io::Result<Event>;
}

/// 実端末の crossterm source。
#[derive(Default)]
pub struct CrosstermSource {
    #[cfg(test)]
    scripted: bool,
    #[cfg(test)]
    events: VecDeque<Event>,
    #[cfg(test)]
    delayed: bool,
}

#[cfg(test)]
impl CrosstermSource {
    fn scripted(events: impl IntoIterator<Item = Event>) -> Self {
        Self {
            scripted: true,
            events: events.into_iter().collect(),
            delayed: false,
        }
    }

    fn delayed(event: Event) -> Self {
        Self {
            scripted: true,
            events: VecDeque::from([event]),
            delayed: true,
        }
    }
}

#[coverage(off)] // coverage: reason=real_io owner=tui expires=2027-01-31 tests=real_pty_entry_resize_quit_and_reattach_restore_terminal
impl CrosstermEventSource for CrosstermSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        #[cfg(test)]
        if self.scripted {
            if self.delayed && timeout.is_zero() {
                return Ok(false);
            }
            return Ok(!self.events.is_empty());
        }
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        #[cfg(test)]
        if self.scripted {
            return self
                .events
                .pop_front()
                .ok_or_else(|| io::Error::other("scripted crossterm source is empty"));
        }
        event::read()
    }
}

/// non-blocking backend receiver の最小 seam。
pub trait BackendReceiver {
    /// 次の event を FIFO 順で返す。空なら `None`。
    fn try_recv(&mut self) -> Option<Self::Event>;

    /// backend event の型。
    type Event;
}

/// backend をまだ接続しない runtime 用 receiver。
#[derive(Debug, Default)]
pub struct NoBackend<B>(std::marker::PhantomData<B>);

impl<B> BackendReceiver for NoBackend<B> {
    type Event = B;

    // The production `NoBackend<()>` monomorphization only returns this constant;
    // the EventPump tests cover the same no-backend behavior with their test event
    // type. Exclude the duplicate runtime-only instantiation from LLVM coverage.
    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=multiplexes_terminal_backend_and_tick_in_a_deterministic_order
    fn try_recv(&mut self) -> Option<Self::Event> {
        None
    }
}

/// crossterm、backend、tick を単一 stream に多重化する poll pump。
pub struct EventPump<S, R> {
    source: S,
    backend: R,
    tick_interval: Duration,
    next_tick_at: Duration,
    pending_terminal: VecDeque<Event>,
    /// A second Escape encountered during lookahead has not been decoded yet.
    pending_escape: Option<Event>,
    pending_mouse: Option<MouseReport>,
    legacy_unix_control_aliases: bool,
}

impl<S, R> EventPump<S, R>
where
    S: CrosstermEventSource,
    R: BackendReceiver,
{
    /// `now` を基準に tick schedule を開始する。
    pub fn new(source: S, backend: R, tick_interval: Duration, now: Duration) -> Self {
        assert!(!tick_interval.is_zero(), "tick interval must not be zero");
        Self {
            source,
            backend,
            tick_interval,
            next_tick_at: now.saturating_add(tick_interval),
            pending_terminal: VecDeque::new(),
            pending_escape: None,
            pending_mouse: None,
            legacy_unix_control_aliases: false,
        }
    }

    /// Restores the original bytes behind crossterm's Unix `Ctrl+4..7`
    /// aliases when the terminal does not support enhanced keyboard events.
    #[must_use]
    pub fn with_legacy_unix_control_aliases(mut self, enabled: bool) -> Self {
        self.legacy_unix_control_aliases = enabled;
        self
    }

    /// `now` 時点で次の runtime event を返す。
    ///
    /// 先に ready 済みの terminal event を drain せず 1 件だけ返し、その後 backend、期限に
    /// 達した tick を観測する。いずれも ready でなければ、次の tick まで terminal を poll
    /// して terminal event のみを返す。backend はこの待機後の次 cycle で観測する。
    #[coverage(off)] // coverage: reason=generic_monomorphization owner=tui expires=2027-01-31 tests=source_poll_and_read_errors_are_projected_from_each_pump_phase
    pub fn next(&mut self, now: Duration) -> io::Result<RuntimeEvent<R::Event>> {
        while let Some(event) = self.poll_terminal(Duration::ZERO)? {
            if let Some(event) = adapt_event_with_legacy_unix_control_aliases(
                event,
                self.legacy_unix_control_aliases,
            ) {
                return self.coalesce_pointer(event);
            }
        }
        if let Some(event) = self.backend.try_recv() {
            return Ok(RuntimeEvent::Backend(event));
        }
        if now >= self.next_tick_at {
            advance_tick(&mut self.next_tick_at, self.tick_interval, now);
            return Ok(RuntimeEvent::Tick);
        }

        let timeout = self.next_tick_at.saturating_sub(now);
        loop {
            if let Some(event) = self.poll_terminal(timeout)? {
                if let Some(event) = adapt_event_with_legacy_unix_control_aliases(
                    event,
                    self.legacy_unix_control_aliases,
                ) {
                    return self.coalesce_pointer(event);
                }
                continue;
            }
            advance_tick(
                &mut self.next_tick_at,
                self.tick_interval,
                now.saturating_add(timeout),
            );
            return Ok(RuntimeEvent::Tick);
        }
    }

    fn poll_terminal(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        if let Some(event) = self.pending_terminal.pop_front() {
            return Ok(Some(event));
        }
        if self.pending_mouse.is_some() {
            return recover_mouse_report(
                &mut self.source,
                &mut self.pending_terminal,
                &mut self.pending_escape,
                &mut self.pending_mouse,
                timeout,
            );
        }
        let poll_deadline = self.source.now() + timeout;
        let event = if let Some(escape) = self.pending_escape.take() {
            escape
        } else {
            let Some(event) = poll_source(&mut self.source, timeout)? else {
                return Ok(None);
            };
            event
        };
        if event == Event::Key(KeyEvent::new(CrosstermKeyCode::Esc, KeyModifiers::NONE)) {
            self.pending_mouse = Some(MouseReport {
                escape: Some(event),
                deadline: self.source.now() + MOUSE_REPORT_GRACE,
                buffered: VecDeque::new(),
                report: String::new(),
            });
            let remaining = poll_deadline.saturating_duration_since(self.source.now());
            return recover_mouse_report(
                &mut self.source,
                &mut self.pending_terminal,
                &mut self.pending_escape,
                &mut self.pending_mouse,
                remaining,
            );
        }
        Ok(Some(event))
    }

    /// Fold an immediately-ready wheel burst or passive motion to its latest cell. The first
    /// different event is retained verbatim for the next call, preserving input
    /// order across keys, resize, pointer phases, and direction changes.
    fn coalesce_pointer(
        &mut self,
        mut event: RuntimeEvent<R::Event>,
    ) -> io::Result<RuntimeEvent<R::Event>> {
        if !matches!(
            event,
            RuntimeEvent::Input(
                LiveInput::WheelUp { .. }
                    | LiveInput::WheelDown { .. }
                    | LiveInput::Pointer(PointerEvent {
                        kind: PointerKind::Move,
                        ..
                    })
            )
        ) {
            return Ok(event);
        }

        for _ in 1..WHEEL_COALESCE_LIMIT {
            let Some(raw) = self.poll_terminal(Duration::ZERO)? else {
                break;
            };
            let Some(next) = adapt_event_with_legacy_unix_control_aliases::<R::Event>(
                raw.clone(),
                self.legacy_unix_control_aliases,
            ) else {
                continue;
            };
            match (&mut event, next) {
                (
                    RuntimeEvent::Input(LiveInput::Pointer(pointer)),
                    RuntimeEvent::Input(LiveInput::Pointer(next)),
                ) if pointer.kind == PointerKind::Move && next.kind == PointerKind::Move => {
                    *pointer = next;
                }

                (
                    RuntimeEvent::Input(LiveInput::WheelUp {
                        column,
                        row,
                        notches,
                    }),
                    RuntimeEvent::Input(LiveInput::WheelUp {
                        column: next_column,
                        row: next_row,
                        notches: next_notches,
                    }),
                )
                | (
                    RuntimeEvent::Input(LiveInput::WheelDown {
                        column,
                        row,
                        notches,
                    }),
                    RuntimeEvent::Input(LiveInput::WheelDown {
                        column: next_column,
                        row: next_row,
                        notches: next_notches,
                    }),
                ) if *column == next_column && *row == next_row => {
                    *notches = notches.saturating_add(next_notches);
                }
                _ => {
                    self.pending_terminal.push_front(raw);
                    break;
                }
            }
        }
        Ok(event)
    }
}

/// A candidate survives poll/frame boundaries. Only a lone Escape is released
/// after the grace period; its next event can still start a delayed mouse tail.
struct MouseReport {
    escape: Option<Event>,
    deadline: Instant,
    buffered: VecDeque<Event>,
    report: String,
}

/// Recover only a complete SGR mouse report. Time passing is not evidence that
/// a partial control sequence is text. Replay only when input disproves it.
fn recover_mouse_report(
    source: &mut dyn CrosstermEventSource,
    pending_terminal: &mut VecDeque<Event>,
    pending_escape: &mut Option<Event>,
    pending_mouse: &mut Option<MouseReport>,
    timeout: Duration,
) -> io::Result<Option<Event>> {
    let mut candidate = pending_mouse.take().expect("mouse candidate exists");
    let poll_deadline = source.now() + timeout;
    loop {
        debug_assert!(candidate.report.len() < MOUSE_REPORT_MAX_CHARS);
        let now = source.now();
        let grace = candidate.deadline.saturating_duration_since(now);
        // Consuming a partial prefix is not a completed poll. Continue waiting
        // for the remainder of this call's budget so `next` cannot emit early
        // ticks and move its schedule arbitrarily far into the future.
        let wait = poll_deadline.saturating_duration_since(now);
        let timeout = if candidate.report.is_empty() && !grace.is_zero() {
            grace
        } else {
            wait.max(grace)
        };
        let event = match poll_source(source, timeout) {
            Ok(Some(event)) => event,
            Ok(None) => {
                // Keep a bounded prefix while letting backend events and ticks
                // run. A late tail must never be replayed as ordinary typing.
                let escape = if candidate.report.is_empty() {
                    candidate.escape.take()
                } else {
                    None
                };
                candidate.deadline = source.now();
                *pending_mouse = Some(candidate);
                return Ok(escape);
            }
            Err(error) => {
                pending_terminal.extend(candidate.escape);
                pending_terminal.extend(candidate.buffered);
                return Err(error);
            }
        };
        if event == Event::Key(KeyEvent::new(CrosstermKeyCode::Esc, KeyModifiers::NONE)) {
            if candidate.escape.is_none() && candidate.buffered.is_empty() {
                candidate.escape = Some(event);
                candidate.deadline = source.now() + MOUSE_REPORT_GRACE;
                continue;
            }
            // Replay the first Escape and its text before trying this new
            // Escape. Its mouse tail remains in the source for decoding.
            *pending_escape = Some(event);
            break;
        }
        candidate.buffered.push_back(event.clone());
        let Event::Key(KeyEvent {
            code: CrosstermKeyCode::Char(character),
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) = event
        else {
            break;
        };
        // Legacy terminal parsers set Shift on uppercase M, while a split
        // report's payload has no keyboard modifier identity of its own.
        if !(modifiers.is_empty() || character == 'M' && modifiers == KeyModifiers::SHIFT) {
            break;
        }
        candidate.report.push(character);
        match sgr_mouse::parse_report(&candidate.report) {
            Ok(Some(mouse)) => return Ok(Some(Event::Mouse(mouse))),
            Ok(None) => {}
            Err(()) => break,
        }
    }
    pending_terminal.extend(candidate.escape);
    pending_terminal.extend(candidate.buffered);
    Ok(pending_terminal.pop_front())
}

fn poll_source(
    source: &mut dyn CrosstermEventSource,
    timeout: Duration,
) -> io::Result<Option<Event>> {
    if source.poll(timeout)? {
        source.read().map(Some)
    } else {
        Ok(None)
    }
}

fn advance_tick(next_tick_at: &mut Duration, tick_interval: Duration, now: Duration) {
    while *next_tick_at <= now {
        *next_tick_at = next_tick_at.saturating_add(tick_interval);
    }
}

/// crossterm event を、保持可能な TUI runtime 語彙へ変換する。
#[cfg(test)]
#[must_use]
pub fn adapt_event<B>(event: Event) -> Option<RuntimeEvent<B>> {
    adapt_event_with_legacy_unix_control_aliases(event, false)
}

fn adapt_event_with_legacy_unix_control_aliases<B>(
    event: Event,
    legacy_unix_control_aliases: bool,
) -> Option<RuntimeEvent<B>> {
    match event {
        Event::Key(key) => Some(RuntimeEvent::Input(LiveInput::Key(
            adapt_key_with_legacy_unix_control_aliases(key, legacy_unix_control_aliases),
        ))),
        Event::Paste(text) => Some(RuntimeEvent::Input(LiveInput::Paste(text.into_bytes()))),
        Event::Resize(width, height) => Some(RuntimeEvent::Resize { width, height }),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Moved => Some(RuntimeEvent::Input(LiveInput::Pointer(PointerEvent {
                kind: PointerKind::Move,
                column: mouse.column,
                row: mouse.row,
            }))),
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                Some(RuntimeEvent::Input(LiveInput::Mouse {
                    column: mouse.column,
                    row: mouse.row,
                }))
            }
            MouseEventKind::ScrollUp => Some(RuntimeEvent::Input(LiveInput::WheelUp {
                column: mouse.column,
                row: mouse.row,
                notches: 1,
            })),
            MouseEventKind::ScrollDown => Some(RuntimeEvent::Input(LiveInput::WheelDown {
                column: mouse.column,
                row: mouse.row,
                notches: 1,
            })),
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                Some(RuntimeEvent::Input(LiveInput::Pointer(PointerEvent {
                    kind: PointerKind::Drag,
                    column: mouse.column,
                    row: mouse.row,
                })))
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                Some(RuntimeEvent::Input(LiveInput::Pointer(PointerEvent {
                    kind: PointerKind::Up,
                    column: mouse.column,
                    row: mouse.row,
                })))
            }
            _ => None,
        },
        Event::FocusGained | Event::FocusLost => None,
    }
}

/// crossterm の key kind、modifier、code を TUI の terminal 非依存語彙へ写す。
#[cfg(test)]
#[must_use]
pub fn adapt_key(key: KeyEvent) -> InputKeyEvent {
    adapt_key_with_legacy_unix_control_aliases(key, false)
}

fn adapt_key_with_legacy_unix_control_aliases(
    key: KeyEvent,
    legacy_unix_control_aliases: bool,
) -> InputKeyEvent {
    let mut input = InputKeyEvent::new(
        match key.code {
            CrosstermKeyCode::Char(character) => {
                KeyCode::Char(adapt_character(character, key.modifiers))
            }
            CrosstermKeyCode::Enter => KeyCode::Enter,
            CrosstermKeyCode::Backspace => KeyCode::Backspace,
            CrosstermKeyCode::Tab => KeyCode::Tab,
            CrosstermKeyCode::BackTab => KeyCode::BackTab,
            CrosstermKeyCode::Esc => KeyCode::Escape,
            CrosstermKeyCode::Up => KeyCode::Up,
            CrosstermKeyCode::Down => KeyCode::Down,
            CrosstermKeyCode::Left => KeyCode::Left,
            CrosstermKeyCode::Right => KeyCode::Right,
            CrosstermKeyCode::Home => KeyCode::Home,
            CrosstermKeyCode::End => KeyCode::End,
            CrosstermKeyCode::PageUp => KeyCode::PageUp,
            CrosstermKeyCode::PageDown => KeyCode::PageDown,
            CrosstermKeyCode::Insert => KeyCode::Insert,
            CrosstermKeyCode::Delete => KeyCode::Delete,
            CrosstermKeyCode::F(number) => KeyCode::Function(number),
            _ => KeyCode::Unknown,
        },
        Modifiers {
            shift: key.modifiers.contains(KeyModifiers::SHIFT),
            control: key.modifiers.contains(KeyModifiers::CONTROL),
            alt: key.modifiers.contains(KeyModifiers::ALT),
            super_: key.modifiers.contains(KeyModifiers::SUPER),
            hyper: key.modifiers.contains(KeyModifiers::HYPER),
            meta: key.modifiers.contains(KeyModifiers::META),
        },
        match key.kind {
            KeyEventKind::Press => InputKeyEventKind::Press,
            KeyEventKind::Repeat => InputKeyEventKind::Repeat,
            KeyEventKind::Release => InputKeyEventKind::Release,
        },
    );
    if legacy_unix_control_aliases {
        input.raw_bytes = legacy_unix_control_byte(&key).into_iter().collect();
    }
    input
}

/// Crossterm's Unix legacy parser projects bytes `0x1c..=0x1f` as
/// `Ctrl+4..=Ctrl+7`. Reconstruct that byte only when the runtime has already
/// established that enhanced, unambiguous keyboard events are unavailable.
fn legacy_unix_control_byte(key: &KeyEvent) -> Option<u8> {
    if key.modifiers != KeyModifiers::CONTROL {
        return None;
    }
    match key.code {
        CrosstermKeyCode::Char('4') => Some(0x1c),
        CrosstermKeyCode::Char('5') => Some(0x1d),
        CrosstermKeyCode::Char('6') => Some(0x1e),
        CrosstermKeyCode::Char('7') => Some(0x1f),
        _ => None,
    }
}

/// Canonicalize printable ASCII letters across terminal keyboard protocols.
///
/// Legacy input commonly reports Shift+x as `Char('X')`, while enhanced
/// keyboard protocols may report the base key `Char('x')` plus the Shift bit.
/// The TUI's character vocabulary represents the resulting text scalar, so both
/// forms must become `Char('X')` before shortcut and text-input classification.
fn adapt_character(character: char, modifiers: KeyModifiers) -> char {
    if modifiers.contains(KeyModifiers::SHIFT) {
        character.to_ascii_uppercase()
    } else {
        character
    }
}

#[cfg(test)]
mod tests {
    #![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract
    use std::collections::VecDeque;

    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::*;

    struct FakeSource {
        events: VecDeque<Event>,
        timeouts: Vec<Duration>,
        now: Instant,
    }

    impl Default for FakeSource {
        fn default() -> Self {
            Self::with([])
        }
    }

    impl FakeSource {
        fn with(events: impl IntoIterator<Item = Event>) -> Self {
            Self {
                events: events.into_iter().collect(),
                timeouts: Vec::new(),
                now: Instant::now(),
            }
        }
    }

    impl CrosstermEventSource for FakeSource {
        fn now(&mut self) -> Instant {
            self.now
        }

        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            self.timeouts.push(timeout);
            Ok(!self.events.is_empty())
        }

        fn read(&mut self) -> io::Result<Event> {
            Ok(self.events.pop_front().expect("read after ready poll"))
        }
    }

    struct DelayedSource(Option<Event>);

    impl CrosstermEventSource for DelayedSource {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            Ok(!timeout.is_zero() && self.0.is_some())
        }

        fn read(&mut self) -> io::Result<Event> {
            Ok(self.0.take().expect("read after ready poll"))
        }
    }

    enum ErrorSource {
        ImmediatePoll,
        DelayedPoll,
        Read,
    }

    impl CrosstermEventSource for ErrorSource {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            match self {
                Self::ImmediatePoll => Err(io::Error::other("immediate poll failed")),
                Self::DelayedPoll if timeout.is_zero() => Ok(false),
                Self::DelayedPoll => Err(io::Error::other("delayed poll failed")),
                Self::Read => Ok(true),
            }
        }

        fn read(&mut self) -> io::Result<Event> {
            Err(io::Error::other("read failed"))
        }
    }

    #[derive(Default)]
    struct FakeEventBackend(VecDeque<&'static str>);

    impl BackendReceiver for FakeEventBackend {
        type Event = &'static str;

        fn try_recv(&mut self) -> Option<Self::Event> {
            self.0.pop_front()
        }
    }

    const T0: Duration = Duration::from_secs(10);
    const TICK: Duration = Duration::from_millis(100);

    fn wheel(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn adapter_preserves_key_kind_modifiers_text_paste_and_resize() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('う'),
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            KeyEventKind::Repeat,
        );
        assert_eq!(
            adapt_event::<()>(Event::Key(key)),
            Some(RuntimeEvent::Input(LiveInput::Key(InputKeyEvent::new(
                usagi_tui::usecase::terminal_input::KeyCode::Char('う'),
                Modifiers {
                    shift: true,
                    control: true,
                    alt: true,
                    ..Modifiers::default()
                },
                InputKeyEventKind::Repeat,
            ))))
        );

        let navigation = KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL | KeyModifiers::ALT);
        assert_eq!(
            adapt_event::<()>(Event::Key(navigation)),
            Some(RuntimeEvent::Input(LiveInput::Key(InputKeyEvent::new(
                usagi_tui::usecase::terminal_input::KeyCode::Right,
                Modifiers {
                    control: true,
                    alt: true,
                    ..Modifiers::default()
                },
                InputKeyEventKind::Press,
            ))))
        );
        assert_eq!(
            adapt_event::<()>(Event::Paste("貼り付け\ntext".into())),
            Some(RuntimeEvent::Input(LiveInput::Paste(
                "貼り付け\ntext".as_bytes().to_vec()
            )))
        );
        assert_eq!(
            adapt_event::<()>(Event::Resize(120, 40)),
            Some(RuntimeEvent::Resize {
                width: 120,
                height: 40
            })
        );
    }

    #[test]
    fn adapter_preserves_left_click_coordinates_and_ignores_other_mouse_events() {
        let left = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 12,
            row: 7,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            adapt_event::<()>(Event::Mouse(left)),
            Some(RuntimeEvent::Input(LiveInput::Mouse { column: 12, row: 7 }))
        );
        assert_eq!(
            adapt_event::<()>(Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Right),
                ..left
            })),
            None
        );
        for (kind, expected) in [
            (
                MouseEventKind::Drag(MouseButton::Left),
                Some(RuntimeEvent::Input(LiveInput::Pointer(PointerEvent {
                    kind: PointerKind::Drag,
                    column: 12,
                    row: 7,
                }))),
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                Some(RuntimeEvent::Input(LiveInput::Pointer(PointerEvent {
                    kind: PointerKind::Up,
                    column: 12,
                    row: 7,
                }))),
            ),
        ] {
            assert_eq!(
                adapt_event::<()>(Event::Mouse(MouseEvent { kind, ..left })),
                expected
            );
        }
        assert_eq!(adapt_event::<()>(Event::FocusLost), None);
        assert_eq!(NoBackend::<()>::default().try_recv(), None);
    }

    #[test]
    fn adapter_routes_wheel_direction_to_live_pane_scroll_actions() {
        let mouse = |kind| MouseEvent {
            kind,
            column: 41,
            row: 12,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            adapt_event::<()>(Event::Mouse(mouse(MouseEventKind::ScrollUp))),
            Some(RuntimeEvent::Input(LiveInput::WheelUp {
                column: 41,
                row: 12,
                notches: 1,
            }))
        );
        assert_eq!(
            adapt_event::<()>(Event::Mouse(mouse(MouseEventKind::ScrollDown))),
            Some(RuntimeEvent::Input(LiveInput::WheelDown {
                column: 41,
                row: 12,
                notches: 1,
            }))
        );
    }

    #[test]
    fn split_mouse_report_never_reaches_the_terminal_as_text() {
        let events = std::iter::once(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .chain("[<65;42;13M".chars().map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            }))
            .chain([Event::Key(KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
            ))]);
        let mut pump = EventPump::new(
            FakeSource::with(events),
            FakeEventBackend::default(),
            TICK,
            T0,
        );

        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelDown {
                column: 41,
                row: 12,
                notches: 1,
            })
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            adapt_event(Event::Key(KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
            )))
            .unwrap()
        );
    }

    fn split_report(report: &str) -> Vec<Event> {
        std::iter::once(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .chain(report.chars().map(|character| {
                Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    if character == 'M' {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    },
                ))
            }))
            .collect()
    }

    #[test]
    fn recovered_mouse_reports_join_wheel_bursts_without_reordering_keys() {
        let mut events = vec![wheel(MouseEventKind::ScrollUp, 41, 12)];
        events.extend(split_report("[<64;42;13M"));
        events.extend(split_report("[<64;42;13M"));
        events.push(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        events.extend(split_report("[<65;44;15M"));
        let mut pump = EventPump::new(
            FakeSource::with(events),
            FakeEventBackend::default(),
            TICK,
            T0,
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelUp {
                column: 41,
                row: 12,
                notches: 3,
            })
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            adapt_event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            )))
            .unwrap()
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelDown {
                column: 43,
                row: 14,
                notches: 1,
            })
        );
    }

    #[test]
    fn ordinary_escape_and_incomplete_reports_replay_every_event_unchanged() {
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let mut cases = vec![
            vec![escape.clone()],
            split_report("[x"),
            split_report("[<64;42"),
            split_report("[<64;0;1M"),
            vec![escape.clone(), Event::Resize(80, 24)],
            vec![escape.clone(), Event::Paste("[<64;1;1M".into())],
            vec![
                escape.clone(),
                Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::CONTROL)),
            ],
            vec![
                escape,
                Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('['),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat,
                )),
            ],
        ];
        for events in &mut cases {
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
            )));
            let expected = events.iter().cloned().map(adapt_event).collect::<Vec<_>>();
            let mut pump = EventPump::new(
                FakeSource::with(events.clone()),
                FakeEventBackend::default(),
                TICK,
                T0,
            );
            for event in expected {
                assert_eq!(Some(pump.next(T0).unwrap()), event);
            }
            assert_eq!(pump.next(T0 + TICK).unwrap(), RuntimeEvent::Tick);
        }
    }

    #[test]
    fn mouse_lookahead_waits_for_a_delayed_tail_with_a_bounded_timeout() {
        struct FragmentedSource {
            events: VecDeque<Event>,
            now: Instant,
            timeouts: Vec<Duration>,
        }
        impl CrosstermEventSource for FragmentedSource {
            fn now(&mut self) -> Instant {
                self.now
            }

            fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
                self.timeouts.push(timeout);
                Ok(!self.events.is_empty() && (!timeout.is_zero() || self.events.len() == 12))
            }

            fn read(&mut self) -> io::Result<Event> {
                Ok(self.events.pop_front().unwrap())
            }
        }
        let mut pump = EventPump::new(
            FragmentedSource {
                events: split_report("[<65;42;13M").into(),
                now: Instant::now(),
                timeouts: Vec::new(),
            },
            FakeEventBackend::default(),
            TICK,
            T0,
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelDown {
                column: 41,
                row: 12,
                notches: 1,
            })
        );
        assert_eq!(pump.source.timeouts[0], Duration::ZERO);
        assert_eq!(&pump.source.timeouts[1..12], &[MOUSE_REPORT_GRACE; 11]);
    }

    #[test]
    fn delayed_mouse_fragments_survive_every_split_and_keep_backend_ticks_live() {
        for report in ["[<65;60;48M", "[<64;42;13M", "[<00065;65535;65535M"] {
            let events = split_report(report);
            for split in 1..events.len() {
                let mut pump = EventPump::new(
                    FakeSource::with(events[..split].iter().cloned()),
                    FakeEventBackend::default(),
                    TICK,
                    T0,
                );
                // A lone Escape remains responsive. Even after releasing it,
                // remember that the next input may be its delayed mouse tail.
                if split == 1 {
                    assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(events[0].clone()));
                }
                for frame in 1..=4 {
                    pump.source.now += MOUSE_REPORT_GRACE * 10;
                    pump.backend.0.push_back("backend remains live");
                    assert_eq!(
                        pump.next(T0 + TICK * frame).unwrap(),
                        RuntimeEvent::Backend("backend remains live"),
                        "{report:?} at {split}"
                    );
                    assert_eq!(pump.next(T0 + TICK * frame).unwrap(), RuntimeEvent::Tick);
                }
                // The blocking phase must still poll with a timeout after the
                // grace period, rather than spin on an unfinished candidate.
                assert_eq!(pump.next(T0 + TICK * 4).unwrap(), RuntimeEvent::Tick);
                assert_eq!(pump.source.timeouts.last(), Some(&TICK));
                pump.source.events.extend(events[split..].iter().cloned());
                assert_eq!(
                    Some(pump.next(T0 + TICK * 4).unwrap()),
                    adapt_event(Event::Mouse(
                        sgr_mouse::parse_report(report).unwrap().unwrap()
                    )),
                    "{report:?} at {split}"
                );
                assert!(pump.pending_mouse.is_none());
                let typed = Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
                pump.source.events.push_back(typed.clone());
                assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(typed));
            }
        }
    }

    #[test]
    fn a_partial_tail_during_a_blocking_poll_waits_out_the_frame_budget() {
        struct TimedSource {
            now: Instant,
            events: VecDeque<(Instant, Event)>,
        }
        impl CrosstermEventSource for TimedSource {
            fn now(&mut self) -> Instant {
                self.now
            }
            fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
                if let Some((at, _)) = self.events.front()
                    && *at <= self.now + timeout
                {
                    self.now = self.now.max(*at);
                    return Ok(true);
                }
                self.now += timeout;
                Ok(false)
            }
            fn read(&mut self) -> io::Result<Event> {
                Ok(self.events.pop_front().unwrap().1)
            }
        }
        let start = Instant::now();
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let mut pump = EventPump::new(
            TimedSource {
                now: start,
                events: VecDeque::from([(start, escape.clone())]),
            },
            FakeEventBackend::default(),
            TICK,
            T0,
        );
        assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(escape));
        assert_eq!(pump.source.now - start, MOUSE_REPORT_GRACE);
        pump.source.events.push_back((
            pump.source.now + Duration::from_millis(5),
            Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
        ));
        assert_eq!(
            pump.next(T0 + MOUSE_REPORT_GRACE).unwrap(),
            RuntimeEvent::Tick
        );
        assert_eq!(pump.source.now - start, TICK);
        assert_eq!(pump.pending_mouse.as_ref().unwrap().report, "[");

        // A new Escape and prefix can also arrive during the blocking phase.
        // Its grace deadline must not produce a tick before the frame deadline,
        // and the initial wait for Escape counts toward the same frame budget.
        let mut pump = EventPump::new(
            TimedSource {
                now: start,
                events: VecDeque::from([
                    (
                        start + Duration::from_millis(5),
                        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                    ),
                    (
                        start + Duration::from_millis(6),
                        Event::Key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE)),
                    ),
                ]),
            },
            FakeEventBackend::default(),
            TICK,
            T0,
        );
        assert_eq!(pump.next(T0).unwrap(), RuntimeEvent::Tick);
        assert_eq!(pump.source.now - start, TICK);
        assert_eq!(pump.pending_mouse.as_ref().unwrap().report, "[");
    }

    #[test]
    fn mouse_report_can_arrive_one_character_per_frame() {
        let mut pump = EventPump::new(FakeSource::default(), FakeEventBackend::default(), TICK, T0);
        for _ in 0..2 {
            let events = split_report("[<65;60;48M");
            for (index, event) in events.iter().enumerate() {
                pump.source.now += MOUSE_REPORT_GRACE * 10;
                pump.source.events.push_back(event.clone());
                let input = pump.next(T0 + TICK * 100).unwrap();
                if index == 0 {
                    assert_eq!(Some(input), adapt_event(event.clone()));
                } else if index + 1 == events.len() {
                    assert_eq!(
                        input,
                        RuntimeEvent::Input(LiveInput::WheelDown {
                            column: 59,
                            row: 47,
                            notches: 1,
                        })
                    );
                } else {
                    assert_eq!(input, RuntimeEvent::Tick);
                }
            }
        }
    }

    #[test]
    fn literal_mouse_shaped_text_without_escape_is_preserved() {
        let events = split_report("[<65;60;48M")[1..].to_vec();
        let mut pump = EventPump::new(
            FakeSource::with(events.clone()),
            FakeEventBackend::default(),
            TICK,
            T0,
        );
        for event in events {
            assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(event));
        }
    }

    #[test]
    fn delayed_non_mouse_input_is_replayed_in_order_without_a_duplicate_escape() {
        for tail in [
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
            ))],
            split_report("[x")[1..].to_vec(),
            split_report("[<64;0;1M")[1..].to_vec(),
            vec![Event::Paste("[<65;60;48M".into())],
            vec![Event::Resize(80, 24)],
            split_report("[<65;60;48M"),
        ] {
            let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            let mut pump = EventPump::new(
                FakeSource::with([escape.clone()]),
                FakeEventBackend::default(),
                TICK,
                T0,
            );
            assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(escape.clone()));
            pump.source.now += MOUSE_REPORT_GRACE * 10;
            pump.source.events.extend(tail.clone());
            if tail[0] == escape {
                assert_eq!(
                    pump.next(T0).unwrap(),
                    RuntimeEvent::Input(LiveInput::WheelDown {
                        column: 59,
                        row: 47,
                        notches: 1,
                    })
                );
            } else {
                for event in tail {
                    assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(event));
                }
            }
            assert_eq!(pump.next(T0 + TICK).unwrap(), RuntimeEvent::Tick);
        }
    }

    #[cfg(unix)]
    fn drive_mouse_pty_child() {
        crossterm::terminal::enable_raw_mode().unwrap();
        let mut pump = EventPump::new(
            CrosstermSource::default(),
            NoBackend::<()>::default(),
            Duration::from_millis(50),
            Duration::ZERO,
        );
        let start = Instant::now();
        println!("mouse-test:ready");
        let mut wheels = 0;
        while wheels < 2 {
            match pump.next(start.elapsed()).unwrap() {
                RuntimeEvent::Input(LiveInput::WheelDown {
                    column: 59,
                    row: 47,
                    notches: 1,
                }) => {
                    wheels += 1;
                    println!("mouse-test:wheel");
                }
                RuntimeEvent::Input(LiveInput::Key(key))
                    if key.code == usagi_tui::usecase::terminal_input::KeyCode::Escape =>
                {
                    println!("mouse-test:escape");
                }
                RuntimeEvent::Tick => {
                    let prefix = pump
                        .pending_mouse
                        .as_ref()
                        .map_or("", |candidate| candidate.report.as_str());
                    println!("mouse-test:tick:{prefix}");
                }
                event => println!("mouse-test:input:{event:?}"),
            }
        }
        crossterm::terminal::disable_raw_mode().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_delayed_mouse_fragments_do_not_become_keys() {
        use std::fs::File;
        use std::io::{BufRead, Write};
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::process::{Command, Stdio};
        use std::sync::mpsc;

        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        const CHILD: &str = "USAGI_TEST_DELAYED_MOUSE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            drive_mouse_pty_child();
            return;
        }

        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes the two descriptors; optional terminal
        // settings are null. Each successful descriptor is owned exactly once.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // SAFETY: both File values own live descriptors. The parent master must
        // not leak into the child test process.
        for file in [&master, &slave] {
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tui_input::tests::real_pty_delayed_mouse_fragments_do_not_become_keys",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .stdin(slave)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = child.0.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // libtest can print the test name before our first line.
                if let Some(index) = line.find("mouse-test:")
                    && sender.send(line[index..].to_owned()).is_err()
                {
                    break;
                }
            }
        });
        let wait_for = |expected: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let line = receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("child input acknowledgement");
                if line == expected {
                    break;
                }
                assert!(
                    !line.starts_with("mouse-test:input:")
                        && line != "mouse-test:wheel"
                        && line != "mouse-test:escape",
                    "unexpected input: {line}"
                );
            }
        };
        wait_for("mouse-test:ready");
        // Wait for a tick acknowledging each prefix before writing the next
        // byte. This forces real tty reads apart and exceeds the grace period
        // without a scheduler-dependent sleep or a synthetic event source.
        for _ in 0..2 {
            master.write_all(b"\x1b").unwrap();
            wait_for("mouse-test:escape");
            let report = "[<65;60;48M";
            for (index, byte) in report.bytes().enumerate() {
                master.write_all(&[byte]).unwrap();
                if index + 1 == report.len() {
                    wait_for("mouse-test:wheel");
                } else {
                    wait_for(&format!("mouse-test:tick:{}", &report[..=index]));
                }
            }
        }
        assert!(child.0.wait().unwrap().success());
        reader.join().unwrap();
    }

    #[test]
    fn a_second_escape_still_recovers_its_mouse_tail_after_replaying_the_first() {
        for prefix in ["", "[<64;66"] {
            let preceding = split_report(prefix);
            let mut events = preceding.clone();
            events.extend(split_report("[<65;66;28M"));
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
            )));
            let mut pump = EventPump::new(
                FakeSource::with(events),
                FakeEventBackend::default(),
                TICK,
                T0,
            );
            for event in preceding {
                assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(event));
            }
            assert_eq!(
                pump.next(T0).unwrap(),
                RuntimeEvent::Input(LiveInput::WheelDown {
                    column: 65,
                    row: 27,
                    notches: 1,
                })
            );
            assert_eq!(
                pump.next(T0).unwrap(),
                adapt_event(Event::Key(KeyEvent::new(
                    KeyCode::Char('z'),
                    KeyModifiers::NONE
                )))
                .unwrap()
            );
        }

        // After a lone Escape has already been released, a subsequent Escape
        // must not return an empty terminal poll and let backend/tick preempt
        // ready terminal input.
        let escape = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let mut pump = EventPump::new(
            FakeSource::with([escape.clone()]),
            FakeEventBackend::default(),
            TICK,
            T0,
        );
        assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(escape.clone()));
        pump.source.now += MOUSE_REPORT_GRACE * 2;
        pump.source.events.extend(split_report("[<65;66;28M"));
        pump.backend.0.push_back("after-terminal");
        assert_eq!(
            pump.next(T0 + TICK).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelDown {
                column: 65,
                row: 27,
                notches: 1,
            })
        );
        assert_eq!(
            pump.next(T0 + TICK).unwrap(),
            RuntimeEvent::Backend("after-terminal")
        );
    }

    #[test]
    fn mouse_lookahead_errors_preserve_the_escape_and_buffered_text() {
        struct FailingSource {
            events: VecDeque<Event>,
            fail_poll: bool,
            failed: bool,
        }
        impl CrosstermEventSource for FailingSource {
            fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
                if self.events.is_empty() && self.fail_poll && !self.failed {
                    self.failed = true;
                    return Err(io::Error::other("lookahead poll failed"));
                }
                Ok(!self.events.is_empty() || !self.failed)
            }

            fn read(&mut self) -> io::Result<Event> {
                if let Some(event) = self.events.pop_front() {
                    return Ok(event);
                }
                self.failed = true;
                Err(io::Error::other("lookahead read failed"))
            }
        }
        for fail_poll in [false, true] {
            let events = split_report("[<64");
            let mut pump = EventPump::new(
                FailingSource {
                    events: events.clone().into(),
                    fail_poll,
                    failed: false,
                },
                FakeEventBackend::default(),
                TICK,
                T0,
            );
            assert!(pump.next(T0).is_err());
            for event in events {
                assert_eq!(Some(pump.next(T0).unwrap()), adapt_event(event));
            }
            assert_eq!(pump.next(T0 + TICK).unwrap(), RuntimeEvent::Tick);
        }
    }

    #[test]
    fn fake_crossterm_sequence_keeps_each_relevant_event_in_order() {
        let source = FakeSource::with([
            Event::FocusGained,
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Paste("paste".into()),
        ]);
        let mut pump = EventPump::new(source, FakeEventBackend::default(), TICK, T0);

        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::Key(InputKeyEvent::new(
                usagi_tui::usecase::terminal_input::KeyCode::Char('x'),
                Modifiers::default(),
                InputKeyEventKind::Press,
            )))
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Resize {
                width: 80,
                height: 24
            }
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::Paste(b"paste".to_vec()))
        );
    }

    #[test]
    fn legacy_unix_control_aliases_reach_the_prefix_classifier_without_losing_digits() {
        use usagi_tui::usecase::terminal_input::{
            LiveInputClassifier, LiveInputOutput, LiveTerminalAction,
        };

        for (character, expected) in [
            ('4', LiveTerminalAction::ActivateWorkspace(4)),
            ('5', LiveTerminalAction::NextTab),
            ('6', LiveTerminalAction::ActivateWorkspace(6)),
            ('7', LiveTerminalAction::KeyboardHelp),
        ] {
            let source = FakeSource::with([
                Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
                // Crossterm's Unix legacy parser projects raw 0x1c..=0x1f
                // as Ctrl-4..=Ctrl-7. Reserved legacy bytes win, while bytes
                // without a reserved meaning retain their digit shortcut.
                Event::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::CONTROL,
                )),
            ]);
            let mut pump = EventPump::new(source, FakeEventBackend::default(), TICK, T0)
                .with_legacy_unix_control_aliases(true);
            let mut classifier = LiveInputClassifier::default();
            let RuntimeEvent::Input(leader) = pump.next(T0).unwrap() else {
                panic!("leader must remain a live input");
            };
            assert_eq!(
                classifier.classify(Duration::ZERO, leader),
                LiveInputOutput::Swallowed
            );
            let RuntimeEvent::Input(follow_up) = pump.next(T0).unwrap() else {
                panic!("follow-up must remain a live input");
            };
            assert_eq!(
                classifier.classify(Duration::from_millis(1), follow_up),
                LiveInputOutput::Action(expected),
                "legacy Ctrl-{character}"
            );
        }
    }

    #[test]
    fn enhanced_control_digit_remains_distinct_from_control_bracket() {
        use usagi_tui::usecase::terminal_input::{
            LiveInputClassifier, LiveInputOutput, LiveTerminalAction,
        };

        for (character, expected) in [
            ('5', LiveTerminalAction::ActivateWorkspace(5)),
            (']', LiveTerminalAction::NextTab),
        ] {
            let mut classifier = LiveInputClassifier::default();
            assert_eq!(
                classifier.classify(
                    Duration::ZERO,
                    LiveInput::Key(adapt_key(KeyEvent::new(
                        KeyCode::Char('o'),
                        KeyModifiers::CONTROL,
                    ))),
                ),
                LiveInputOutput::Swallowed
            );
            assert_eq!(
                classifier.classify(
                    Duration::from_millis(1),
                    LiveInput::Key(adapt_key(KeyEvent::new(
                        KeyCode::Char(character),
                        KeyModifiers::CONTROL,
                    ))),
                ),
                LiveInputOutput::Action(expected)
            );
        }
    }

    #[test]
    fn garden_hover_bursts_keep_the_latest_cell_before_the_next_click() {
        let source = FakeSource::with([
            wheel(MouseEventKind::Moved, 2, 3),
            wheel(MouseEventKind::Moved, 9, 8),
            wheel(MouseEventKind::Down(MouseButton::Left), 9, 8),
        ]);
        let mut pump = EventPump::new(source, FakeEventBackend::default(), TICK, T0);
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::Pointer(PointerEvent {
                kind: PointerKind::Move,
                column: 9,
                row: 8,
            }))
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::Mouse { column: 9, row: 8 })
        );
    }

    #[test]
    fn production_pointer_pump_preserves_hover_then_click() {
        let mut pump = EventPump::new(
            CrosstermSource::scripted([
                wheel(MouseEventKind::Moved, 2, 3),
                wheel(MouseEventKind::Moved, 9, 8),
                wheel(MouseEventKind::Down(MouseButton::Left), 9, 8),
            ]),
            NoBackend::<()>::default(),
            TICK,
            T0,
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::Pointer(PointerEvent {
                kind: PointerKind::Move,
                column: 9,
                row: 8,
            }))
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::Mouse { column: 9, row: 8 })
        );
    }

    #[test]
    fn same_direction_wheel_burst_is_coalesced_before_the_next_frame() {
        let source = FakeSource::with([
            wheel(MouseEventKind::ScrollUp, 6, 9),
            wheel(MouseEventKind::ScrollUp, 6, 9),
            // An ignored focus notification neither splits nor reorders the
            // surrounding wheel burst.
            Event::FocusGained,
            wheel(MouseEventKind::ScrollUp, 6, 9),
            wheel(MouseEventKind::ScrollUp, 10, 20),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        let mut pump = EventPump::new(source, FakeEventBackend::default(), TICK, T0);

        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelUp {
                column: 6,
                row: 9,
                notches: 3,
            })
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelUp {
                column: 10,
                row: 20,
                notches: 1,
            })
        );
        assert!(matches!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::Key(_))
        ));
    }

    #[test]
    fn wheel_coalescing_preserves_direction_changes_and_has_a_redraw_bound() {
        let mut events = (0..=WHEEL_COALESCE_LIMIT)
            .map(|_| wheel(MouseEventKind::ScrollDown, 1, 2))
            .collect::<Vec<_>>();
        events.push(wheel(MouseEventKind::ScrollUp, 3, 4));
        let mut pump = EventPump::new(
            FakeSource::with(events),
            FakeEventBackend::default(),
            TICK,
            T0,
        );

        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelDown {
                column: 1,
                row: 2,
                notches: WHEEL_COALESCE_LIMIT,
            })
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelDown {
                column: 1,
                row: 2,
                notches: 1,
            })
        );
        assert_eq!(
            pump.next(T0).unwrap(),
            RuntimeEvent::Input(LiveInput::WheelUp {
                column: 3,
                row: 4,
                notches: 1,
            })
        );
    }

    #[test]
    fn multiplexes_terminal_backend_and_tick_in_a_deterministic_order() {
        let source = FakeSource::with([Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))]);
        let backend = FakeEventBackend(VecDeque::from(["snapshot"]));
        let mut pump = EventPump::new(source, backend, TICK, T0);

        assert!(matches!(pump.next(T0).unwrap(), RuntimeEvent::Input(_)));
        assert_eq!(pump.next(T0).unwrap(), RuntimeEvent::Backend("snapshot"));
        assert_eq!(pump.next(T0 + TICK).unwrap(), RuntimeEvent::Tick);
    }

    #[test]
    fn waits_only_until_the_next_tick_when_no_source_is_ready() {
        let mut pump = EventPump::new(FakeSource::default(), FakeEventBackend::default(), TICK, T0);

        assert_eq!(pump.next(T0).unwrap(), RuntimeEvent::Tick);
        assert_eq!(pump.source.timeouts, vec![Duration::ZERO, TICK]);
    }

    #[test]
    fn source_poll_and_read_errors_are_projected_from_each_pump_phase() {
        for source in [ErrorSource::ImmediatePoll, ErrorSource::Read] {
            let mut pump = EventPump::new(source, FakeEventBackend::default(), TICK, T0);
            assert!(pump.next(T0).is_err());
        }

        let mut delayed = EventPump::new(
            ErrorSource::DelayedPoll,
            FakeEventBackend::default(),
            TICK,
            T0,
        );
        assert!(delayed.next(T0).is_err());
    }

    #[test]
    fn production_crossterm_pump_reaches_the_same_tick_state_machine() {
        let mut pump = EventPump::new(
            CrosstermSource::scripted(Vec::new()),
            NoBackend::<()>::default(),
            TICK,
            T0,
        );
        assert_eq!(pump.next(T0).unwrap(), RuntimeEvent::Tick);

        let mut immediate = EventPump::new(
            CrosstermSource::scripted([Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))]),
            NoBackend::<()>::default(),
            TICK,
            T0,
        );
        assert!(matches!(
            immediate.next(T0).unwrap(),
            RuntimeEvent::Input(_)
        ));
        let mut delayed = EventPump::new(
            CrosstermSource::delayed(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            ))),
            NoBackend::<()>::default(),
            TICK,
            T0,
        );
        assert!(matches!(delayed.next(T0).unwrap(), RuntimeEvent::Input(_)));
    }

    #[test]
    fn ignores_non_input_events_received_while_waiting_for_a_tick() {
        let source = DelayedSource(Some(Event::FocusLost));
        let mut pump = EventPump::new(source, FakeEventBackend::default(), TICK, T0);

        assert_eq!(pump.next(T0).unwrap(), RuntimeEvent::Tick);

        let source = DelayedSource(Some(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))));
        let mut pump = EventPump::new(source, FakeEventBackend::default(), TICK, T0);
        assert!(matches!(pump.next(T0).unwrap(), RuntimeEvent::Input(_)));
    }

    #[test]
    fn adapter_maps_every_portable_crossterm_key_code_and_kind() {
        let cases = [
            (
                KeyCode::Backspace,
                usagi_tui::usecase::terminal_input::KeyCode::Backspace,
            ),
            (
                KeyCode::Enter,
                usagi_tui::usecase::terminal_input::KeyCode::Enter,
            ),
            (
                KeyCode::Tab,
                usagi_tui::usecase::terminal_input::KeyCode::Tab,
            ),
            (
                KeyCode::BackTab,
                usagi_tui::usecase::terminal_input::KeyCode::BackTab,
            ),
            (
                KeyCode::Esc,
                usagi_tui::usecase::terminal_input::KeyCode::Escape,
            ),
            (KeyCode::Up, usagi_tui::usecase::terminal_input::KeyCode::Up),
            (
                KeyCode::Down,
                usagi_tui::usecase::terminal_input::KeyCode::Down,
            ),
            (
                KeyCode::Left,
                usagi_tui::usecase::terminal_input::KeyCode::Left,
            ),
            (
                KeyCode::Right,
                usagi_tui::usecase::terminal_input::KeyCode::Right,
            ),
            (
                KeyCode::Home,
                usagi_tui::usecase::terminal_input::KeyCode::Home,
            ),
            (
                KeyCode::End,
                usagi_tui::usecase::terminal_input::KeyCode::End,
            ),
            (
                KeyCode::PageUp,
                usagi_tui::usecase::terminal_input::KeyCode::PageUp,
            ),
            (
                KeyCode::PageDown,
                usagi_tui::usecase::terminal_input::KeyCode::PageDown,
            ),
            (
                KeyCode::Insert,
                usagi_tui::usecase::terminal_input::KeyCode::Insert,
            ),
            (
                KeyCode::Delete,
                usagi_tui::usecase::terminal_input::KeyCode::Delete,
            ),
            (
                KeyCode::F(8),
                usagi_tui::usecase::terminal_input::KeyCode::Function(8),
            ),
            (
                KeyCode::Null,
                usagi_tui::usecase::terminal_input::KeyCode::Unknown,
            ),
        ];

        for (code, expected) in cases {
            assert_eq!(
                adapt_key(KeyEvent::new(code, KeyModifiers::NONE)).code,
                expected
            );
        }
        assert_eq!(
            adapt_key(KeyEvent::new_with_kind(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ))
            .kind,
            InputKeyEventKind::Release
        );
    }

    #[test]
    fn adapter_canonicalizes_both_shifted_letter_encodings() {
        for character in ['x', 'X'] {
            assert_eq!(
                adapt_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::SHIFT,)).code,
                usagi_tui::usecase::terminal_input::KeyCode::Char('X')
            );
        }
        assert_eq!(
            adapt_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)).code,
            usagi_tui::usecase::terminal_input::KeyCode::Char('x')
        );
    }

    #[test]
    fn legacy_unix_control_alias_reconstruction_is_exact_and_opt_in() {
        for (character, byte) in [('4', 28), ('5', 29), ('6', 30), ('7', 31)] {
            assert_eq!(
                adapt_key_with_legacy_unix_control_aliases(
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL),
                    true,
                )
                .raw_bytes,
                vec![byte]
            );
        }
        assert_eq!(
            adapt_key_with_legacy_unix_control_aliases(
                KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL),
                false,
            )
            .raw_bytes,
            [] as [u8; 0]
        );
        for event in [
            KeyEvent::new(
                KeyCode::Char('5'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(
                adapt_key_with_legacy_unix_control_aliases(event, true).raw_bytes,
                [] as [u8; 0]
            );
        }
    }
}
