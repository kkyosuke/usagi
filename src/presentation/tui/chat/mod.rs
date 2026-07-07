//! Local-LLM chat.
//!
//! A chat with the workspace's configured local LLM (served through Ollama),
//! shown in 集中's **right pane** — the same rectangle the embedded terminal /
//! agent use — so it reads like the other per-session surfaces. The conversation
//! state ([`state`]) and rendering ([`ui`]) are pure and unit-tested; the home
//! event loop owns the overlay, keyboard, and reply polling.
//!
//! This module also carries the one impure piece: [`spawn_request`], which runs
//! `ollama run` on a detached thread and hands back a receiver the loop polls.
//! Because it shells out, this file is excluded from coverage (like the other
//! screen composition roots); the logic that can be tested lives in [`state`] /
//! [`ui`] and in the home event loop's chat handling.

pub mod state;
pub mod ui;

use std::sync::mpsc::{self, Receiver};

use crate::usecase::doctor::SystemRunner;
use crate::usecase::local_llm;

/// Start a model request for `prompt` against Ollama `model`, returning a
/// receiver that yields the completion (`Ok`) or an error message to show in its
/// place (`Err`) exactly once. The request runs on a detached thread so a slow
/// generation never blocks the UI; the home loop polls the receiver each tick and
/// drops it (abandoning the thread harmlessly) if the chat is closed first.
pub fn spawn_request(model: String, prompt: String) -> Receiver<Result<String, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ollama_ask(&model, &prompt));
    });
    rx
}

/// Run one completion against `ollama run <model>`, feeding `prompt` as its
/// argument and returning the trimmed stdout. The Ollama server is brought up
/// first (a Homebrew install starts none on its own), and a non-zero exit is
/// surfaced as an error string the chat shows in the transcript.
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
