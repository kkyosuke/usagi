//! Pure state for the local-LLM chat screen.
//!
//! The screen lets the user converse with the workspace's configured local LLM
//! (served through Ollama) without leaving usagi. This module owns everything
//! that can be reasoned about without a terminal or a model runtime — the
//! transcript, the line being composed, the in-flight flag, and the scrollback
//! offset — so the whole conversation surface is unit-tested. The event loop
//! ([`super::event`]) drives the model call and the keyboard; the renderer
//! ([`super::ui`]) turns this state into frames.

use crate::presentation::tui::widgets::text_input::TextInput;

/// The light system instruction prepended to every prompt so the local model
/// answers as a concise coding assistant rather than continuing the transcript
/// verbatim.
const SYSTEM_PREAMBLE: &str =
    "You are usagi's local coding assistant. Answer the user concisely and helpfully.";

/// Shown in place of an empty completion so a blank reply never looks like a
/// silent failure.
const EMPTY_REPLY: &str = "(no response)";

/// Who authored a message in the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A message the user sent.
    User,
    /// A reply from the local model (or an error surfaced in its place).
    Assistant,
}

/// One turn in the chat transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

/// The chat screen's state: which model it talks to, the transcript so far, the
/// input line being composed, whether a reply is in flight, and how far the
/// transcript is scrolled back from the newest message.
#[derive(Debug, Clone)]
pub struct Chat {
    /// The Ollama model name the conversation runs against (shown in the header).
    model: String,
    /// The conversation so far, oldest first.
    messages: Vec<Message>,
    /// The line the user is composing (sent on `Enter`).
    input: TextInput,
    /// Whether a model reply is currently being awaited — the input is read-only
    /// until it lands, so a second turn cannot race the first.
    pending: bool,
    /// How many lines the transcript is scrolled up from the newest; `0` pins the
    /// view to the latest message. Clamped by the renderer against the real line
    /// count, so the state need not know the viewport height.
    scroll: usize,
}

impl Chat {
    /// Open an empty chat bound to `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: Vec::new(),
            input: TextInput::new(),
            pending: false,
            scroll: 0,
        }
    }

    /// The model name the conversation runs against.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The transcript so far, oldest first.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The line being composed, for rendering its caret.
    pub fn input(&self) -> &TextInput {
        &self.input
    }

    /// The line being composed, for the event loop to route editing keys to.
    pub fn input_mut(&mut self) -> &mut TextInput {
        &mut self.input
    }

    /// Whether a model reply is currently being awaited.
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// How far the transcript is scrolled back from the newest message.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Scroll one line towards older messages (the renderer clamps the far end).
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_add(1);
    }

    /// Scroll one line back towards the newest message.
    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// Accept the composed line as a user message and mark a reply in flight,
    /// returning the full prompt to send to the model (the transcript so far,
    /// with a system preamble). Returns `None` — sending nothing — when a reply
    /// is already pending or the line is blank, so `Enter` is a safe no-op then.
    ///
    /// The view is re-pinned to the newest message so the just-sent turn is in
    /// sight regardless of any prior scrollback.
    pub fn submit(&mut self) -> Option<String> {
        if self.pending {
            return None;
        }
        let text = self.input.value().trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.input.clear();
        self.messages.push(Message {
            role: Role::User,
            text,
        });
        self.pending = true;
        self.scroll = 0;
        Some(self.build_prompt())
    }

    /// Record the model's reply (or an error message shown in its place),
    /// clearing the in-flight flag and re-pinning the view to the newest message.
    /// An all-whitespace completion is shown as [`EMPTY_REPLY`] so a blank answer
    /// is not mistaken for a hang.
    pub fn finish(&mut self, reply: Result<String, String>) {
        self.pending = false;
        self.scroll = 0;
        let text = match reply {
            Ok(text) if text.trim().is_empty() => EMPTY_REPLY.to_string(),
            Ok(text) => text.trim().to_string(),
            Err(error) => format!("⚠ {error}"),
        };
        self.messages.push(Message {
            role: Role::Assistant,
            text,
        });
    }

    /// Build the single prompt string sent to `ollama run`: the system preamble,
    /// then the whole transcript tagged by role, ending on an open `Assistant:`
    /// turn so the model continues as the assistant. `ollama run` is stateless
    /// per call, so replaying the transcript each turn is what keeps context.
    fn build_prompt(&self) -> String {
        let mut prompt = String::from(SYSTEM_PREAMBLE);
        for message in &self.messages {
            let tag = match message.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            prompt.push_str("\n\n");
            prompt.push_str(tag);
            prompt.push_str(": ");
            prompt.push_str(&message.text);
        }
        prompt.push_str("\n\nAssistant:");
        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_opens_an_empty_chat_bound_to_the_model() {
        let chat = Chat::new("qwen2.5-coder:7b");
        assert_eq!(chat.model(), "qwen2.5-coder:7b");
        assert!(chat.messages().is_empty());
        assert!(chat.input().is_empty());
        assert!(!chat.is_pending());
        assert_eq!(chat.scroll(), 0);
    }

    #[test]
    fn submit_records_the_user_turn_and_returns_the_prompt() {
        let mut chat = Chat::new("m");
        for c in "hello".chars() {
            chat.input_mut().insert(c);
        }
        let prompt = chat.submit().expect("a non-empty line is sent");
        // The composed line becomes a user message and the input is cleared.
        assert_eq!(chat.messages().len(), 1);
        assert_eq!(chat.messages()[0].role, Role::User);
        assert_eq!(chat.messages()[0].text, "hello");
        assert!(chat.input().is_empty());
        // A reply is now awaited.
        assert!(chat.is_pending());
        // The prompt carries the system preamble and the open assistant turn.
        assert!(prompt.starts_with(SYSTEM_PREAMBLE));
        assert!(prompt.contains("\n\nUser: hello"));
        assert!(prompt.ends_with("\n\nAssistant:"));
    }

    #[test]
    fn submit_trims_the_line_and_ignores_a_blank_one() {
        let mut chat = Chat::new("m");
        // A blank (all-whitespace) line sends nothing and starts no request.
        for c in "   ".chars() {
            chat.input_mut().insert(c);
        }
        assert!(chat.submit().is_none());
        assert!(chat.messages().is_empty());
        assert!(!chat.is_pending());
        // A padded line is trimmed before it is recorded.
        chat.input_mut().clear();
        for c in "  hi  ".chars() {
            chat.input_mut().insert(c);
        }
        chat.submit().unwrap();
        assert_eq!(chat.messages()[0].text, "hi");
    }

    #[test]
    fn submit_is_a_no_op_while_a_reply_is_pending() {
        let mut chat = Chat::new("m");
        chat.input_mut().insert('a');
        chat.submit().unwrap();
        // A second submit while awaiting the first reply sends nothing.
        chat.input_mut().insert('b');
        assert!(chat.submit().is_none());
        assert_eq!(chat.messages().len(), 1);
        // The second line is left intact in the input for when the reply lands.
        assert_eq!(chat.input().value(), "b");
    }

    #[test]
    fn finish_appends_the_reply_and_clears_pending() {
        let mut chat = Chat::new("m");
        chat.input_mut().insert('q');
        chat.submit().unwrap();
        chat.finish(Ok("  an answer  ".to_string()));
        assert!(!chat.is_pending());
        assert_eq!(chat.messages().len(), 2);
        assert_eq!(chat.messages()[1].role, Role::Assistant);
        // The reply is trimmed.
        assert_eq!(chat.messages()[1].text, "an answer");
    }

    #[test]
    fn finish_shows_a_placeholder_for_a_blank_reply() {
        let mut chat = Chat::new("m");
        chat.input_mut().insert('q');
        chat.submit().unwrap();
        chat.finish(Ok("   ".to_string()));
        assert_eq!(chat.messages()[1].text, EMPTY_REPLY);
    }

    #[test]
    fn finish_surfaces_an_error_in_the_transcript() {
        let mut chat = Chat::new("m");
        chat.input_mut().insert('q');
        chat.submit().unwrap();
        chat.finish(Err("model offline".to_string()));
        assert!(!chat.is_pending());
        assert_eq!(chat.messages()[1].role, Role::Assistant);
        assert_eq!(chat.messages()[1].text, "⚠ model offline");
    }

    #[test]
    fn build_prompt_replays_the_whole_transcript() {
        let mut chat = Chat::new("m");
        chat.input_mut().insert('a');
        chat.submit().unwrap();
        chat.finish(Ok("first".to_string()));
        for c in "b".chars() {
            chat.input_mut().insert(c);
        }
        let prompt = chat.submit().unwrap();
        // Both prior turns and the new user turn are present, in order.
        let user_a = prompt.find("User: a").unwrap();
        let assistant = prompt.find("Assistant: first").unwrap();
        let user_b = prompt.find("User: b").unwrap();
        assert!(user_a < assistant && assistant < user_b);
        assert!(prompt.ends_with("\n\nAssistant:"));
    }

    #[test]
    fn scrolling_moves_the_offset_and_saturates_at_zero() {
        let mut chat = Chat::new("m");
        assert_eq!(chat.scroll(), 0);
        // Down at the bottom stays put (no underflow).
        chat.scroll_down();
        assert_eq!(chat.scroll(), 0);
        chat.scroll_up();
        chat.scroll_up();
        assert_eq!(chat.scroll(), 2);
        chat.scroll_down();
        assert_eq!(chat.scroll(), 1);
    }

    #[test]
    fn submit_and_finish_repin_the_view_to_the_newest_message() {
        let mut chat = Chat::new("m");
        chat.scroll_up();
        chat.scroll_up();
        chat.input_mut().insert('a');
        // Sending re-pins to the bottom so the new turn is visible.
        chat.submit().unwrap();
        assert_eq!(chat.scroll(), 0);
        // Scroll back up, then a landing reply re-pins again.
        chat.scroll_up();
        chat.finish(Ok("reply".to_string()));
        assert_eq!(chat.scroll(), 0);
    }
}
