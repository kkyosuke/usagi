//! Application boundary for daemon-owned terminal requests.
//!
//! The port deliberately contains no JSON value, protocol action discriminator,
//! or negotiated snapshot representation. Presentation adapters decode and
//! validate the wire request before entering this boundary, then shape the typed
//! result for the negotiated protocol revision.

use usagi_core::{
    domain::{
        id::{ClientId, ConnectionId, OperationId, RequestId, TerminalRef},
        terminal_launch::{TerminalInventoryEntry, TerminalLaunchScope},
        terminal_visibility::{CompletedTerminalEntry, TerminalVisibility},
    },
    infrastructure::ipc::ProtocolError,
    usecase::client::TerminalRequest,
};

use super::terminal::{Attached, InputAck, Output, Snapshot};

/// Connection and request identities attached to one decoded terminal command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalRequestContext {
    pub connection: ConnectionId,
    pub client: ClientId,
    pub request: RequestId,
}

/// Typed application result. Wire-specific JSON and snapshot selection are
/// intentionally absent and belong to the presentation adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalResponse {
    Launch {
        terminal: TerminalRef,
        launch_operation: OperationId,
        replayed: bool,
    },
    Inventory(Vec<TerminalInventoryEntry>),
    Attached(Attached),
    Resumed {
        output: Vec<Output>,
        exited: bool,
    },
    Snapshot(Snapshot),
    Detached,
    Input(InputAck),
    InputOutcome(Option<InputAck>),
    CompletedInventory(Vec<CompletedTerminalEntry>),
    Visibility {
        visibility: TerminalVisibility,
        applied: bool,
        conflict: bool,
    },
}

/// Daemon-owned terminal application input port.
pub trait TerminalOwner {
    /// Executes one already decoded and presentation-validated request.
    ///
    /// # Errors
    /// Returns a safe protocol failure when the requested application effect
    /// cannot be completed under the current ownership and fencing state.
    fn handle(
        &mut self,
        context: TerminalRequestContext,
        request: TerminalRequest,
    ) -> Result<TerminalResponse, ProtocolError>;

    fn inventory(&self, _scope: &TerminalLaunchScope) -> Vec<TerminalInventoryEntry> {
        Vec::new()
    }

    fn completed_inventory(&self, _scope: &TerminalLaunchScope) -> Vec<CompletedTerminalEntry> {
        Vec::new()
    }

    fn disconnect(&mut self, connection: ConnectionId);
}

#[cfg(test)]
pub(crate) fn response_json(
    response: TerminalResponse,
    wire: super::terminal::SnapshotWire,
) -> serde_json::Value {
    use serde_json::json;

    match response {
        TerminalResponse::Launch {
            terminal,
            launch_operation,
            replayed,
        } => {
            json!({"terminal": terminal, "launch_operation": launch_operation, "replayed": replayed})
        }
        TerminalResponse::Inventory(terminals) => json!({"terminals": terminals}),
        TerminalResponse::Attached(attached) => json!(attached.into_frame(wire)),
        TerminalResponse::Resumed { output, exited } => json!({"output": output, "exited": exited}),
        TerminalResponse::Snapshot(snapshot) => json!(snapshot.into_frame(wire)),
        TerminalResponse::Detached => json!({}),
        TerminalResponse::Input(ack) => json!({"ack": ack}),
        TerminalResponse::InputOutcome(Some(ack)) => json!({"outcome": "final", "ack": ack}),
        TerminalResponse::InputOutcome(None) => json!({"outcome": "unknown"}),
        TerminalResponse::CompletedInventory(entries) => json!({"entries": entries}),
        TerminalResponse::Visibility {
            visibility,
            applied,
            conflict,
        } => json!({"visibility": visibility, "applied": applied, "conflict": conflict}),
    }
}

/// Compatibility surface for pre-boundary unit tests. New tests should call
/// [`TerminalOwner`] with typed requests directly.
#[cfg(test)]
pub(crate) trait JsonTerminalOwner {
    fn request(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: usagi_core::usecase::client::TerminalAction,
        payload: serde_json::Value,
        wire: super::terminal::SnapshotWire,
    ) -> Result<serde_json::Value, ProtocolError>;

    fn inventory(&self, scope: &TerminalLaunchScope) -> Vec<TerminalInventoryEntry>;
    fn completed_inventory(&self, scope: &TerminalLaunchScope) -> Vec<CompletedTerminalEntry>;
    fn disconnect(&mut self, connection: ConnectionId);
}

#[cfg(test)]
impl<T: TerminalOwner> JsonTerminalOwner for T {
    fn request(
        &mut self,
        connection: ConnectionId,
        client: ClientId,
        request_id: RequestId,
        action: usagi_core::usecase::client::TerminalAction,
        payload: serde_json::Value,
        wire: super::terminal::SnapshotWire,
    ) -> Result<serde_json::Value, ProtocolError> {
        use usagi_core::usecase::client::{TerminalAction, TerminalRequest};
        let request: TerminalRequest = serde_json::from_value(payload).map_err(|_| {
            ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::InvalidArgument,
                "invalid terminal request vocabulary",
            )
        })?;
        let matching = matches!(
            (&action, &request),
            (TerminalAction::Launch, TerminalRequest::Launch { .. })
                | (TerminalAction::Inventory, TerminalRequest::Inventory { .. })
                | (TerminalAction::Attach, TerminalRequest::Attach { .. })
                | (TerminalAction::Resume, TerminalRequest::Resume { .. })
                | (TerminalAction::Resync, TerminalRequest::Resync { .. })
                | (TerminalAction::Input, TerminalRequest::Input { .. })
                | (
                    TerminalAction::InputOutcome,
                    TerminalRequest::InputOutcome { .. }
                )
                | (TerminalAction::Resize, TerminalRequest::Resize { .. })
                | (TerminalAction::Detach, TerminalRequest::Detach { .. })
                | (
                    TerminalAction::CompletedInventory,
                    TerminalRequest::CompletedInventory { .. }
                )
                | (TerminalAction::Observe, TerminalRequest::Observe { .. })
                | (TerminalAction::Dismiss, TerminalRequest::Dismiss { .. })
        );
        if !matching {
            return Err(ProtocolError::new(
                usagi_core::infrastructure::ipc::ErrorCode::InvalidArgument,
                "terminal action does not match its payload",
            ));
        }
        TerminalOwner::handle(
            self,
            TerminalRequestContext {
                connection,
                client,
                request: request_id,
            },
            request,
        )
        .map(|response| response_json(response, wire))
    }

    fn inventory(&self, scope: &TerminalLaunchScope) -> Vec<TerminalInventoryEntry> {
        TerminalOwner::inventory(self, scope)
    }

    fn completed_inventory(&self, scope: &TerminalLaunchScope) -> Vec<CompletedTerminalEntry> {
        TerminalOwner::completed_inventory(self, scope)
    }

    fn disconnect(&mut self, connection: ConnectionId) {
        TerminalOwner::disconnect(self, connection);
    }
}
