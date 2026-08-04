//! Settings read/write boundary shared by settings clients and their backend.

use std::{io, path::Path};

use crate::domain::settings::{EnvBindings, Settings};

/// The persistence target selected by the Config entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    /// Per-user Theme and modal settings plus defaults copied into new
    /// workspaces for Agent, Issue, and Memory.
    Global,
    /// Agent, Issue, and Memory settings local to the current workspace.
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

    /// Persist only the environment bindings owned by `scope`.
    ///
    /// Config uses this narrow operation for its environment editor so saving a
    /// Theme or Agent draft cannot roll back an environment change made after
    /// the Config screen opened. Backends with merge-on-save semantics should
    /// override this method and hold their scope lock across read-modify-write.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected scope cannot be read or saved.
    fn save_environment(
        &mut self,
        scope: SettingsScope,
        environment: &EnvBindings,
    ) -> io::Result<()> {
        let mut settings = self.read(scope)?;
        settings.env.clone_from(environment);
        self.save(scope, &settings)
    }
}

/// Resolve settings for a Home entry without allowing a damaged preference
/// file to prevent the workspace from opening. Workspace Agent, Issue, and
/// Memory values are applied over global Theme and modal settings; failures
/// fall back to the readable global value, then to domain defaults.
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

        fn save(&mut self, scope: SettingsScope, settings: &Settings) -> io::Result<()> {
            match scope {
                SettingsScope::Global => self.global = Ok(settings.clone()),
                SettingsScope::Workspace => self.workspace = Ok(settings.clone()),
            }
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
        let environment = [("RUST_LOG".to_owned(), "debug".to_owned())]
            .into_iter()
            .collect();
        readable
            .save_environment(SettingsScope::Global, &environment)
            .unwrap();
        assert_eq!(readable.global.as_ref().unwrap().env, environment);
        readable
            .save(SettingsScope::Global, &Settings::default())
            .unwrap();

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
