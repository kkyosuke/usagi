//! crossterm を TUI の統一 runtime input stream へ接続する合成ルート adapter。
//!
//! crossterm 依存はこの binary crate に閉じ、TUI crate へ渡す値は terminal 非依存の
//! [`LiveInput`] と [`RuntimeEvent`] にする。`EventPump::next` は terminal、backend、tick
//! をこの順に観測するため、同じ poll cycle で同時に ready だった event の順序も決定的である。

use std::io;
use std::time::Duration;

use crossterm::event::{
    self, Event, KeyCode as CrosstermKeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use usagi_tui::usecase::terminal_input::{
    KeyCode, KeyEvent as InputKeyEvent, KeyEventKind as InputKeyEventKind, LiveInput, Modifiers,
    RuntimeEvent,
};

/// poll/read を差し替えられる crossterm event source。
pub trait CrosstermEventSource {
    /// `timeout` まで terminal event を待てるか調べる。
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    /// `poll` が ready を返した後の event を読む。
    fn read(&mut self) -> io::Result<Event>;
}

/// 実端末の crossterm source。
pub struct CrosstermSource;

impl CrosstermEventSource for CrosstermSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
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
    #[coverage(off)]
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
        }
    }

    /// `now` 時点で次の runtime event を返す。
    ///
    /// 先に ready 済みの terminal event を drain せず 1 件だけ返し、その後 backend、期限に
    /// 達した tick を観測する。いずれも ready でなければ、次の tick まで terminal を poll
    /// して terminal event のみを返す。backend はこの待機後の次 cycle で観測する。
    #[coverage(off)]
    pub fn next(&mut self, now: Duration) -> io::Result<RuntimeEvent<R::Event>> {
        while self.source.poll(Duration::ZERO)? {
            if let Some(event) = adapt_event(self.source.read()?) {
                return Ok(event);
            }
        }
        if let Some(event) = self.backend.try_recv() {
            return Ok(RuntimeEvent::Backend(event));
        }
        if now >= self.next_tick_at {
            self.advance_tick(now);
            return Ok(RuntimeEvent::Tick);
        }

        let timeout = self.next_tick_at.saturating_sub(now);
        loop {
            if self.source.poll(timeout)? {
                if let Some(event) = adapt_event(self.source.read()?) {
                    return Ok(event);
                }
                continue;
            }
            self.advance_tick(now.saturating_add(timeout));
            return Ok(RuntimeEvent::Tick);
        }
    }

    fn advance_tick(&mut self, now: Duration) {
        while self.next_tick_at <= now {
            self.next_tick_at = self.next_tick_at.saturating_add(self.tick_interval);
        }
    }
}

/// crossterm event を、保持可能な TUI runtime 語彙へ変換する。
#[must_use]
pub fn adapt_event<B>(event: Event) -> Option<RuntimeEvent<B>> {
    match event {
        Event::Key(key) => Some(RuntimeEvent::Input(LiveInput::Key(adapt_key(key)))),
        Event::Paste(text) => Some(RuntimeEvent::Input(LiveInput::Paste(text.into_bytes()))),
        Event::Resize(width, height) => Some(RuntimeEvent::Resize { width, height }),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                Some(RuntimeEvent::Input(LiveInput::Mouse {
                    column: mouse.column,
                    row: mouse.row,
                }))
            }
            MouseEventKind::ScrollUp => Some(RuntimeEvent::Input(LiveInput::WheelUp)),
            MouseEventKind::ScrollDown => Some(RuntimeEvent::Input(LiveInput::WheelDown)),
            _ => None,
        },
        Event::FocusGained | Event::FocusLost => None,
    }
}

/// crossterm の key kind、modifier、code を TUI の terminal 非依存語彙へ写す。
#[must_use]
pub fn adapt_key(key: KeyEvent) -> InputKeyEvent {
    InputKeyEvent::new(
        match key.code {
            CrosstermKeyCode::Char(character) => KeyCode::Char(character),
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
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    use super::*;

    #[derive(Default)]
    struct FakeSource {
        events: VecDeque<Event>,
        timeouts: Vec<Duration>,
    }

    impl FakeSource {
        fn with(events: impl IntoIterator<Item = Event>) -> Self {
            Self {
                events: events.into_iter().collect(),
                timeouts: Vec::new(),
            }
        }
    }

    impl CrosstermEventSource for FakeSource {
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

    #[derive(Default)]
    struct FakeBackend(VecDeque<&'static str>);

    impl BackendReceiver for FakeBackend {
        type Event = &'static str;

        fn try_recv(&mut self) -> Option<Self::Event> {
            self.0.pop_front()
        }
    }

    const T0: Duration = Duration::from_secs(10);
    const TICK: Duration = Duration::from_millis(100);

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
            Some(RuntimeEvent::Input(LiveInput::WheelUp))
        );
        assert_eq!(
            adapt_event::<()>(Event::Mouse(mouse(MouseEventKind::ScrollDown))),
            Some(RuntimeEvent::Input(LiveInput::WheelDown))
        );
    }

    #[test]
    fn fake_crossterm_sequence_keeps_each_relevant_event_in_order() {
        let source = FakeSource::with([
            Event::FocusGained,
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Event::Resize(80, 24),
            Event::Paste("paste".into()),
        ]);
        let mut pump = EventPump::new(source, FakeBackend::default(), TICK, T0);

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
    fn multiplexes_terminal_backend_and_tick_in_a_deterministic_order() {
        let source = FakeSource::with([Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))]);
        let backend = FakeBackend(VecDeque::from(["snapshot"]));
        let mut pump = EventPump::new(source, backend, TICK, T0);

        assert!(matches!(pump.next(T0).unwrap(), RuntimeEvent::Input(_)));
        assert_eq!(pump.next(T0).unwrap(), RuntimeEvent::Backend("snapshot"));
        assert_eq!(pump.next(T0 + TICK).unwrap(), RuntimeEvent::Tick);
    }

    #[test]
    fn waits_only_until_the_next_tick_when_no_source_is_ready() {
        let mut pump = EventPump::new(FakeSource::default(), FakeBackend::default(), TICK, T0);

        assert_eq!(pump.next(T0).unwrap(), RuntimeEvent::Tick);
        assert_eq!(pump.source.timeouts, vec![Duration::ZERO, TICK]);
    }

    #[test]
    fn ignores_non_input_events_received_while_waiting_for_a_tick() {
        let source = DelayedSource(Some(Event::FocusLost));
        let mut pump = EventPump::new(source, FakeBackend::default(), TICK, T0);

        assert_eq!(pump.next(T0).unwrap(), RuntimeEvent::Tick);
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
}
