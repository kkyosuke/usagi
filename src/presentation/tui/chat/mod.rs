//! Local-LLM chat screen.
//!
//! A dedicated screen for conversing with the workspace's configured local LLM
//! (served through Ollama), reached from 在席 (Focus) — the `chat` command / menu
//! row. Unlike `agent` / `ai`, which launch an external agent CLI in the
//! worktree, this talks directly to the local model, so a quick question never
//! spends cloud-agent tokens.
//!
//! This module is the thin composition root: it wires the real terminal reader
//! and the `ollama`-backed request into the pure event loop ([`event`]). The
//! model call shells out to `ollama run`, so — like the other screen entry
//! points ([`super::config`] / [`super::home`]) — this file is excluded from
//! coverage; the conversation state ([`state`]), rendering ([`ui`]), and the loop
//! itself ([`event`]) are all unit-tested.

mod event;
mod state;
mod ui;

use std::sync::mpsc::{self, Receiver};

use anyhow::Result;
use console::Term;

use crate::presentation::tui::io::term_reader::TermKeyReader;
use crate::usecase::doctor::SystemRunner;
use crate::usecase::local_llm;

use state::Chat;

/// Run the chat screen against `term`, talking to Ollama `model`, until the user
/// leaves it. Assumes the alternate screen is already active (owned by the
/// caller, which repaints over this screen on return).
pub fn run(term: &Term, model: &str) -> Result<()> {
    let mut reader = TermKeyReader::new(term.clone());
    let request_model = model.to_string();
    // Each submitted turn runs the model on its own thread so a slow generation
    // never blocks the screen; the loop polls the returned receiver and keeps the
    // spinner moving. The thread is detached — leaving the screen mid-generation
    // simply drops the receiver, and the orphaned `ollama` call finishes on its
    // own without holding anything up.
    let mut ask = move |prompt: String| -> Receiver<Result<String, String>> {
        let (tx, rx) = mpsc::channel();
        let model = request_model.clone();
        std::thread::spawn(move || {
            let _ = tx.send(ollama_ask(&model, &prompt));
        });
        rx
    };
    event::event_loop(term, &mut reader, Chat::new(model), &mut ask)
}

/// Run one completion against `ollama run <model>`, feeding `prompt` as its
/// argument and returning the trimmed stdout. The Ollama server is brought up
/// first (a Homebrew install starts none on its own), and a non-zero exit is
/// surfaced as an error string the screen shows in the transcript.
fn ollama_ask(model: &str, prompt: &str) -> Result<String, String> {
    local_llm::ensure_server_started(&SystemRunner)?;
    let output = std::process::Command::new("ollama")
        .arg("run")
        .arg(model)
        .arg(prompt)
        .output()
        .map_err(|e| format!("failed to start ollama: {e}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("ollama exited with {}: {detail}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
