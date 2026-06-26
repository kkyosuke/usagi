//! Background provisioning for the config screen's Local LLM rows.
//!
//! The Local LLM install action and the model picker launch their work on a
//! background thread and return immediately, so the user can keep using usagi
//! (and leave the config screen) while it proceeds. Progress is recorded in the
//! global [`install_task`] so every screen surfaces the loading rabbit until the
//! work finishes. This module owns that off-thread orchestration; the config
//! launcher ([`super`]) only calls into it.

use anyhow::Result;

use crate::presentation::tui::install_task;
use crate::usecase::doctor::SystemRunner;
use crate::usecase::local_llm;

/// Starts installing the `ollama` runtime on a background thread, recording its
/// progress in the global [`install_task`] so every screen can show the loading
/// rabbit and the completion message. Returns as soon as the worker is launched;
/// the sudo password is forwarded to [`local_llm::ensure_runtime`] so the
/// installer can elevate unattended, and it runs `quiet` so its raw output never
/// paints over the TUI. Errors if an install is already in flight.
pub(super) fn start_install_runtime(password: &str) -> Result<()> {
    let handle = install_task::handle();
    if !handle.begin("ollama") {
        return Err(anyhow::anyhow!("インストールは既に実行中です"));
    }
    let password_owned = password.to_string();
    std::thread::spawn(move || {
        let result = local_llm::ensure_runtime(
            std::env::consts::OS,
            &SystemRunner,
            Some(&password_owned),
            true,
        );
        let (ok, message) = match result {
            Ok(_) => (true, "ollama を導入しました 🐰".to_string()),
            Err(e) => (false, e.user_message()),
        };
        handle.finish(ok, message);
    });
    Ok(())
}

/// Starts pulling `model` into the installed runtime on a background thread,
/// recording progress in the global [`install_task`] like
/// [`start_install_runtime`]. `ollama pull` needs no sudo; it runs `quiet` so
/// its `pulling manifest …` output never paints over the TUI. Errors if an
/// install is already in flight.
pub(super) fn start_pull_model(model: &str) -> Result<()> {
    let handle = install_task::handle();
    if !handle.begin(model) {
        return Err(anyhow::anyhow!("インストールは既に実行中です"));
    }
    let model_owned = model.to_string();
    std::thread::spawn(move || {
        let result = local_llm::ensure_model(&SystemRunner, &model_owned, true);
        let (ok, message) = match result {
            Ok(_) => (true, format!("{model_owned} を導入しました 🐰")),
            Err(e) => (false, e.user_message()),
        };
        handle.finish(ok, message);
    });
    Ok(())
}
