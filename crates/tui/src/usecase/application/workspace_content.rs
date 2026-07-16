//! Application ports for workspace-scoped documents and editable content.
//!
//! The controller owns selection and overlay state; implementations of these
//! ports live at the composition boundary and translate to daemon IPC or the
//! settings owner.  No presentation view model or terminal type crosses this
//! boundary.

use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::pullrequest::PrLink;

use super::controller::{EnvironmentEntry, SafeError, Target};

/// Presentation-safe document returned for a non-terminal pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneDocument {
    /// Already-redacted, display-ready logical rows. ANSI styling remains a
    /// presentation concern and is deliberately absent.
    pub lines: Vec<String>,
}

/// Read-only workspace content boundary.
pub trait WorkspaceReadPort {
    /// Load the selected target's scratchpad.
    ///
    /// # Errors
    ///
    /// Returns only a presentation-safe storage or daemon failure.
    fn load_notes(&mut self, target: Target) -> Result<Scratchpad, SafeError>;
    /// Load the selected target's environment entries.
    ///
    /// # Errors
    ///
    /// Returns only a presentation-safe settings or daemon failure.
    fn load_environment(&mut self, target: Target) -> Result<Vec<EnvironmentEntry>, SafeError>;
    /// Load a display-safe diff document for the target.
    ///
    /// # Errors
    ///
    /// Returns only a presentation-safe content lookup failure.
    fn load_diff(&mut self, target: Target) -> Result<PaneDocument, SafeError>;
    /// Load visible pull requests for the target.
    ///
    /// # Errors
    ///
    /// Returns only a presentation-safe content lookup failure.
    fn load_pull_requests(&mut self, target: Target) -> Result<Vec<PrLink>, SafeError>;
}

/// Mutable workspace content boundary.
pub trait WorkspaceWritePort {
    /// Persist a complete scratchpad for one stable target.
    ///
    /// # Errors
    ///
    /// Returns only a presentation-safe storage or daemon failure.
    fn save_notes(&mut self, target: Target, scratchpad: Scratchpad) -> Result<(), SafeError>;
    /// Persist the complete target environment projection.
    ///
    /// # Errors
    ///
    /// Returns only a presentation-safe settings or daemon failure.
    fn save_environment(
        &mut self,
        target: Target,
        entries: Vec<EnvironmentEntry>,
    ) -> Result<(), SafeError>;
}
