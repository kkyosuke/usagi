//! One bounded read-only worker, shared with the Garden observation port.
pub(super) use super::background_job::SearchJob as Job;
use super::workspace_deck::output_search::{SearchResponses, SearchToken};
use crate::usecase::application::runtime_ports::GardenInventoryPort;
use std::sync::{atomic::Ordering, mpsc::Sender};
use usagi_core::domain::id::WorkspaceId;

pub(super) struct Completion {
    pub port: Box<dyn GardenInventoryPort>,
    pub token: SearchToken,
    pub responses: SearchResponses,
}

pub(super) fn spawn(
    mut port: Box<dyn GardenInventoryPort>,
    token: SearchToken,
    query: String,
    workspaces: Vec<WorkspaceId>,
    sender: Sender<Completion>,
) -> Job {
    Job::spawn(token, move |cancel| {
        let mut responses = Vec::new();
        for workspace in workspaces.into_iter().take(super::MAX_OBSERVED_PROJECTS) {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                port.search_terminal_output(workspace, &query, None)
            }))
            .unwrap_or_else(|_| Err("Terminal output search is unavailable".into()));
            responses.push((workspace, result));
        }
        let _ = sender.send(Completion {
            port,
            token,
            responses,
        });
    })
}

/// Consume the queued hit once the target tab has been focused and attached.
pub(super) fn focus_match(
    deck: &mut super::WorkspaceDeck,
    ui: &super::WorkspaceIoRuntime,
    controls: &mut super::LiveTerminalControls,
    focused: Option<&usagi_core::domain::id::TerminalRef>,
    viewport: usize,
) -> Option<&'static str> {
    if deck.overlay_open() {
        return None;
    }
    let hit = deck.pending_output_hit.take()?;
    if focused != Some(&hit.source.terminal) {
        return Some("Search target is no longer available; search again.");
    }
    let Some(session) = ui
        .terminals
        .iter()
        .find(|session| session.terminal() == &hit.source.terminal)
    else {
        return Some("Search target is unavailable; search again.");
    };
    let Some(selection) = session.search_selection(&hit.source, &hit.found, &hit.query) else {
        return Some("Output changed since this search; refresh to locate the match.");
    };
    controls.show_search_selection(
        selection,
        session.display_buffer(),
        session.display_row_origin(),
        session.display_row_count(),
        viewport,
    );
    None
}

#[cfg(test)]
mod tests;
