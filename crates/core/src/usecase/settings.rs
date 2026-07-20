//! Settings read/write boundary shared by settings clients and their backend.

use std::{io, path::Path};

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
    /// Select the workspace identity used for subsequent workspace-scope reads
    /// and writes. Stateless embedders may keep the default no-op.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot resolve the workspace scope.
    fn select_workspace(&mut self, _workspace_root: &Path) -> io::Result<()> {
        Ok(())
    }

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

/// Resolve settings for a Home entry without allowing a damaged preference
/// file to prevent the workspace from opening. Workspace resolution wins;
/// failures fall back to the readable global value, then to domain defaults.
pub fn read_for_workspace_entry(port: &mut dyn SettingsPort) -> Settings {
    port.read(SettingsScope::Workspace)
        .or_else(|_| port.read(SettingsScope::Global))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::{ModalSelectionMode, Theme};

    struct FakePort {
        workspace: io::Result<Settings>,
        global: io::Result<Settings>,
    }

    impl SettingsPort for FakePort {
        fn read(&mut self, scope: SettingsScope) -> io::Result<Settings> {
            let result = match scope {
                SettingsScope::Global => &self.global,
                SettingsScope::Workspace => &self.workspace,
            };
            result
                .as_ref()
                .cloned()
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }

        fn save(&mut self, _: SettingsScope, _: &Settings) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn workspace_entry_prefers_effective_then_global_then_defaults() {
        let effective = Settings {
            modal_selection_mode: ModalSelectionMode::Prompt,
            ..Settings::default()
        };
        let global = Settings {
            theme: Theme::Dark,
            ..Settings::default()
        };
        let mut readable = FakePort {
            workspace: Ok(effective.clone()),
            global: Ok(global.clone()),
        };
        assert_eq!(read_for_workspace_entry(&mut readable), effective);

        let mut broken_local = FakePort {
            workspace: Err(io::Error::other("corrupt local settings")),
            global: Ok(global.clone()),
        };
        assert_eq!(read_for_workspace_entry(&mut broken_local), global);

        let mut broken_both = FakePort {
            workspace: Err(io::Error::other("corrupt local settings")),
            global: Err(io::Error::other("corrupt global settings")),
        };
        assert_eq!(
            read_for_workspace_entry(&mut broken_both),
            Settings::default()
        );
    }
}
