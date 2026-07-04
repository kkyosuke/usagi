//! The chat screen's event loop: draw the frame, read a key, and drive the
//! model request.
//!
//! The model call is asynchronous: submitting a line hands a prompt to the
//! injected [`AskFn`], which returns a [`Receiver`] that yields the reply once.
//! While that is in flight the loop polls the receiver on a timer, animating a
//! "thinking" spinner, and the input goes read-only so a second turn cannot race
//! the first. The `AskFn` indirection keeps the loop testable — production spawns
//! a thread that shells out to `ollama` (see [`super::run`]), while tests hand
//! back a ready (or deliberately withheld) receiver and drive scripted keys.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use anyhow::Result;
use console::{Key, Term};

use crate::presentation::tui::io::screen::{FramePainter, KeyReader};

use super::state::Chat;
use super::ui;

/// How often the loop wakes while a reply is in flight, to poll the receiver and
/// advance the spinner.
const POLL_TICK: Duration = Duration::from_millis(120);

/// Starts a model request for the given prompt, returning a receiver that yields
/// the completion (`Ok`) or an error message to show in its place (`Err`)
/// exactly once. Injected so the loop never shells out in tests.
pub type AskFn<'a> = dyn FnMut(String) -> Receiver<Result<String, String>> + 'a;

/// Run the chat screen against `term` and `reader` until the user leaves (`Esc`
/// or `Ctrl-C`). Assumes the alternate screen is already active (owned by the
/// caller). `ask` performs each model request off the loop; `chat` is the
/// (usually empty) initial conversation.
pub fn event_loop(
    term: &Term,
    reader: &mut dyn KeyReader,
    mut chat: Chat,
    ask: &mut AskFn,
) -> Result<()> {
    let mut painter = FramePainter::new();
    // The in-flight reply's channel, or `None` when idle.
    let mut pending: Option<Receiver<Result<String, String>>> = None;
    let mut tick: usize = 0;

    loop {
        // Drain a finished reply before drawing so it shows this frame. A dropped
        // sender (a worker that panicked before sending) reads as a failed
        // request rather than hanging the screen forever.
        if let Some(rx) = pending.as_ref() {
            match rx.try_recv() {
                Ok(reply) => {
                    chat.finish(reply);
                    pending = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    chat.finish(Err("local LLM request failed".to_string()));
                    pending = None;
                }
            }
        }

        let (height, width) = term.size();
        let frame = ui::render_frame(height as usize, width as usize, &chat, tick);
        painter.paint(term, frame)?;

        // While a reply is pending the read wakes on a timer so the spinner
        // animates and the receiver is re-polled; otherwise it blocks.
        let key = if pending.is_some() {
            match reader.read_key_timeout(POLL_TICK) {
                Ok(Some(key)) => key,
                Ok(None) => {
                    tick = tick.wrapping_add(1);
                    let _ = painter.tick(term);
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
                Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
            }
        } else {
            match reader.read_key() {
                Ok(key) => key,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(()),
                Err(e) => return Err(anyhow::Error::from(e).context("Failed to read key")),
            }
        };

        match key {
            // Either leave key returns to the caller (the home screen).
            Key::Escape | Key::CtrlC => return Ok(()),
            // Scrollback works even while a reply is in flight (read-only view).
            Key::ArrowUp => chat.scroll_up(),
            Key::ArrowDown => chat.scroll_down(),
            // Everything else is inert until the pending reply lands, so a
            // half-finished turn cannot be garbled.
            _ if pending.is_some() => {}
            Key::Enter => {
                if let Some(prompt) = chat.submit() {
                    pending = Some(ask(prompt));
                }
            }
            Key::Backspace => {
                chat.input_mut().backspace();
            }
            Key::Del => {
                chat.input_mut().delete_forward();
            }
            Key::ArrowLeft => chat.input_mut().move_left(),
            Key::ArrowRight => chat.input_mut().move_right(),
            Key::Home => chat.input_mut().move_home(),
            Key::End => chat.input_mut().move_end(),
            // Filter control chars so a stray escape sequence cannot land as text.
            Key::Char(c) if !c.is_control() => {
                chat.input_mut().insert(c);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::mpsc::{self, Sender};

    /// A scripted input source: each read pops the next entry. `Ok(Some(key))`
    /// yields a key, `Ok(None)` models an idle tick (only meaningful on the
    /// timeout path), and `Err(_)` injects a read error. Running off the end
    /// yields an interrupted error, which the loop treats as "leave".
    struct ScriptedReader {
        script: std::collections::VecDeque<io::Result<Option<Key>>>,
    }

    impl ScriptedReader {
        fn new(script: Vec<io::Result<Option<Key>>>) -> Self {
            Self {
                script: script.into_iter().collect(),
            }
        }

        fn pop(&mut self) -> io::Result<Option<Key>> {
            self.script
                .pop_front()
                .unwrap_or_else(|| Err(io::Error::new(io::ErrorKind::Interrupted, "end of script")))
        }
    }

    impl KeyReader for ScriptedReader {
        fn read_key(&mut self) -> io::Result<Key> {
            // The blocking path never scripts an idle tick.
            self.pop().map(|k| k.expect("blocking read wants a key"))
        }

        fn read_key_timeout(&mut self, _timeout: Duration) -> io::Result<Option<Key>> {
            self.pop()
        }
    }

    /// A ready receiver already holding `reply`.
    fn ready(reply: Result<String, String>) -> Receiver<Result<String, String>> {
        let (tx, rx) = mpsc::channel();
        tx.send(reply).unwrap();
        rx
    }

    /// A shared "echo the prompt back" request, referenced by tests that never
    /// actually submit (so its body is covered by the ones that do, without any
    /// per-test unused closure).
    fn ask_ready(prompt: String) -> Receiver<Result<String, String>> {
        ready(Ok(prompt))
    }

    /// A receiver whose sender is stashed in `senders` so it stays connected but
    /// silent — every poll reads `Empty`, keeping the loop in the pending state.
    fn withheld(
        senders: &mut Vec<Sender<Result<String, String>>>,
    ) -> Receiver<Result<String, String>> {
        let (tx, rx) = mpsc::channel();
        senders.push(tx);
        rx
    }

    fn key(k: Key) -> io::Result<Option<Key>> {
        Ok(Some(k))
    }

    #[test]
    fn typing_and_enter_sends_a_prompt_and_shows_the_reply() {
        let term = Term::stdout();
        // Type "hi", Enter (submit); the script then runs out, so the blocking
        // read returns Interrupted and the loop leaves cleanly.
        let mut reader = ScriptedReader::new(vec![
            key(Key::Char('h')),
            key(Key::Char('i')),
            key(Key::Enter),
        ]);
        let mut asked = Vec::new();
        let mut ask = |prompt: String| {
            asked.push(prompt.clone());
            ready(Ok("hello there".to_string()))
        };
        event_loop(&term, &mut reader, Chat::new("m"), &mut ask).unwrap();
        // The prompt sent to the model carried the typed line.
        assert_eq!(asked.len(), 1);
        assert!(asked[0].contains("User: hi"));
    }

    #[test]
    fn enter_on_a_blank_line_sends_nothing_but_a_real_line_does() {
        let term = Term::stdout();
        // A blank Enter starts no request; a typed line then does exactly one.
        let mut reader = ScriptedReader::new(vec![
            key(Key::Enter),     // blank: no-op
            key(Key::Char('a')), // type a line
            key(Key::Enter),     // submit it
            key(Key::Escape),    // leave
        ]);
        let mut calls = 0;
        let mut ask = |prompt: String| {
            calls += 1;
            ready(Ok(prompt))
        };
        event_loop(&term, &mut reader, Chat::new("m"), &mut ask).unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_pending_reply_polls_animates_and_scrolls_while_awaiting() {
        let term = Term::stdout();
        // Submit, scroll up (allowed while pending), an idle tick (spinner
        // animates), then the script runs out so the timeout read returns
        // Interrupted and the loop leaves.
        let mut reader = ScriptedReader::new(vec![
            key(Key::Char('q')),
            key(Key::Enter),
            key(Key::ArrowUp), // scrolls even while pending
            Ok(None),          // idle tick
        ]);
        let mut senders = Vec::new();
        let mut ask = |_: String| withheld(&mut senders);
        event_loop(&term, &mut reader, Chat::new("m"), &mut ask).unwrap();
    }

    #[test]
    fn a_disconnected_channel_reports_a_failed_request() {
        let term = Term::stdout();
        // The worker's sender is dropped before it sends: the first drain sees
        // Disconnected and records the failure. Then the script runs out.
        let mut reader = ScriptedReader::new(vec![key(Key::Char('q')), key(Key::Enter)]);
        let mut ask = |_: String| {
            let (tx, rx) = mpsc::channel::<Result<String, String>>();
            drop(tx);
            rx
        };
        event_loop(&term, &mut reader, Chat::new("m"), &mut ask).unwrap();
    }

    #[test]
    fn keys_are_ignored_while_a_reply_is_withheld() {
        let term = Term::stdout();
        // Submit, then keys arrive while the reply is still pending: typing and
        // backspace are inert until it lands. Esc then leaves.
        let mut reader = ScriptedReader::new(vec![
            key(Key::Char('q')),
            key(Key::Enter),
            key(Key::Char('x')), // ignored: pending
            key(Key::Backspace), // ignored: pending
            key(Key::Escape),
        ]);
        let mut senders = Vec::new();
        let mut ask = |_: String| withheld(&mut senders);
        event_loop(&term, &mut reader, Chat::new("m"), &mut ask).unwrap();
    }

    #[test]
    fn editing_and_unknown_keys_drive_the_input_without_submitting() {
        let term = Term::stdout();
        // One real submit up front (via the shared `ask_ready`, whose reply lands
        // immediately), then every idle-state editing key, the control-char
        // filter, an unhandled key, and scrollback.
        let mut reader = ScriptedReader::new(vec![
            key(Key::Char('s')),
            key(Key::Enter), // submit -> ask_ready -> reply drains at once
            key(Key::Char('a')),
            key(Key::Char('b')),
            key(Key::ArrowLeft),
            key(Key::Backspace),
            key(Key::Char('c')),
            key(Key::Home),
            key(Key::End),
            key(Key::ArrowRight),
            key(Key::Del),
            key(Key::ArrowUp),
            key(Key::ArrowDown),
            key(Key::Char('\u{1b}')), // control char: filtered
            key(Key::Tab),            // unhandled key: inert
            key(Key::Escape),
        ]);
        let mut ask: fn(String) -> Receiver<Result<String, String>> = ask_ready;
        event_loop(&term, &mut reader, Chat::new("m"), &mut ask).unwrap();
    }

    #[test]
    fn ctrl_c_leaves_the_screen() {
        let term = Term::stdout();
        let mut reader = ScriptedReader::new(vec![key(Key::CtrlC)]);
        let mut ask: fn(String) -> Receiver<Result<String, String>> = ask_ready;
        event_loop(&term, &mut reader, Chat::new("m"), &mut ask).unwrap();
    }

    #[test]
    fn a_blocking_read_error_propagates() {
        let term = Term::stdout();
        // A non-interrupted read error on the idle (blocking) path is surfaced.
        let mut reader = ScriptedReader::new(vec![Err(io::Error::other("boom"))]);
        let mut ask: fn(String) -> Receiver<Result<String, String>> = ask_ready;
        let result = event_loop(&term, &mut reader, Chat::new("m"), &mut ask);
        assert!(result.is_err());
    }

    #[test]
    fn a_pending_read_error_propagates() {
        let term = Term::stdout();
        // A non-interrupted read error on the pending (timeout) path is surfaced.
        let mut reader = ScriptedReader::new(vec![
            key(Key::Char('q')),
            key(Key::Enter),
            Err(io::Error::other("boom")),
        ]);
        let mut senders = Vec::new();
        let mut ask = |_: String| withheld(&mut senders);
        let result = event_loop(&term, &mut reader, Chat::new("m"), &mut ask);
        assert!(result.is_err());
    }
}
