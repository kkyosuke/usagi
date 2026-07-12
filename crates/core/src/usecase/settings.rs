//! Settings read/write boundary shared by settings clients and their backend.

use std::io;

use crate::domain::settings::Settings;

/// The persistence scope selected in the Config screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    /// Per-user settings shared by every workspace.
    Global,
    /// Settings local to the current workspace.
    Workspace,
}

/// Read and write settings without coupling clients to a storage backend.
///
/// Implementations own scope-to-storage resolution. Callers retain their draft
/// when [`save`](Self::save) fails so an error remains safe to retry.
pub trait SettingsPort {
    /// Load the saved settings for `scope`.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot read the selected scope.
    fn read(&mut self, scope: SettingsScope) -> io::Result<Settings>;

    /// Persist `settings` in `scope`.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot save the selected scope.
    fn save(&mut self, scope: SettingsScope, settings: &Settings) -> io::Result<()>;
}
