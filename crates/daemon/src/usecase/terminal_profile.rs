//! Pure resolution of the built-in interactive login-shell profile.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use usagi_core::domain::{
    agent::EnvironmentVariableName,
    terminal_launch::{
        DurableTerminalLaunchSnapshot, ResolvedTerminalLaunch, TerminalLaunchRequest,
        TerminalLaunchValidationError, TerminalProfileId,
    },
};

/// Public, non-secret terminal and shell-configuration inputs inherited by an
/// interactive login shell. Values are used only for the live PTY and never
/// written into the durable terminal record.
pub const TERMINAL_ENVIRONMENT_VARIABLES: [&str; 15] = [
    "SHELL",
    "TERM",
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "COLORTERM",
    "COLORFGBG",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TERM_SESSION_ID",
    "NO_COLOR",
    "ZDOTDIR",
    "XDG_CONFIG_HOME",
];

/// Resolves a trusted shell and its public terminal characteristics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginShellProfile {
    environment: BTreeMap<String, String>,
    working_directory: PathBuf,
}

impl LoginShellProfile {
    #[must_use]
    pub fn new(environment: BTreeMap<String, String>, working_directory: PathBuf) -> Self {
        Self {
            environment,
            working_directory,
        }
    }

    /// Produces a login and interactive shell launch without storing values in
    /// the durable terminal record.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an unknown profile or an invalid launch
    /// boundary.
    ///
    /// # Panics
    ///
    /// Panics only if the static `login-shell` profile ID stops being valid.
    pub fn resolve(
        &self,
        request: &TerminalLaunchRequest,
    ) -> Result<ResolvedTerminalLaunch, TerminalLaunchValidationError> {
        let login_shell = TerminalProfileId::new("login-shell").expect("static profile is valid");
        if request.profile_id != login_shell {
            return Err(TerminalLaunchValidationError::UnknownProfile {
                profile_id: request.profile_id.clone(),
            });
        }
        let environment = self.preserved_environment();
        let allowlist = environment.keys().cloned().collect::<BTreeSet<_>>();
        ResolvedTerminalLaunch::new(
            DurableTerminalLaunchSnapshot::new(
                request.clone(),
                2,
                self.shell_program(),
                vec!["-l".to_owned(), "-i".to_owned()],
                self.working_directory.clone(),
                allowlist,
            )?,
            environment,
        )
    }

    fn shell_program(&self) -> String {
        self.environment
            .get("SHELL")
            .filter(|shell| shell.starts_with('/') && !shell.contains('\0'))
            .cloned()
            .unwrap_or_else(|| "/bin/sh".to_owned())
    }

    fn preserved_environment(&self) -> BTreeMap<EnvironmentVariableName, String> {
        let mut environment = TERMINAL_ENVIRONMENT_VARIABLES
            .into_iter()
            .filter(|name| *name != "TERM_SESSION_ID")
            .filter_map(|name| {
                self.environment
                    .get(name)
                    .filter(|value| !value.is_empty() && !value.contains('\0'))
                    .map(|value| {
                        (
                            EnvironmentVariableName::new(name).expect("static name is valid"),
                            value.clone(),
                        )
                    })
            })
            .collect::<BTreeMap<_, _>>();
        // Terminal.app's `/etc/zshrc_Apple_Terminal` treats this as a request
        // to restore and persist its own window session. The daemon-owned PTY
        // is not that window, but it still needs the rest of the Terminal.app
        // prompt configuration. An empty value prevents that session hook
        // while overriding an inherited host value.
        environment.insert(
            EnvironmentVariableName::new("TERM_SESSION_ID").expect("static name is valid"),
            String::new(),
        );
        environment
    }
}

#[cfg(test)]
mod tests {
    use super::LoginShellProfile;
    use std::{collections::BTreeMap, path::PathBuf};
    use usagi_core::domain::{
        id::{SessionId, WorkspaceId, WorktreeId},
        terminal_launch::{TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId},
    };

    fn request(profile: &str) -> TerminalLaunchRequest {
        TerminalLaunchRequest {
            profile_id: TerminalProfileId::new(profile).unwrap(),
            scope: TerminalLaunchScope {
                workspace_id: WorkspaceId::new(),
                session_id: Some(SessionId::new()),
                worktree_id: WorktreeId::new(),
            },
        }
    }

    #[test]
    fn resolves_login_interactive_shell_and_preserves_terminal_environment() {
        let profile = LoginShellProfile::new(
            BTreeMap::from([
                ("SHELL".into(), "/bin/zsh".into()),
                ("TERM".into(), "xterm-256color".into()),
                ("PATH".into(), "/opt/homebrew/bin:/usr/bin".into()),
                ("HOME".into(), "/Users/example".into()),
                ("LANG".into(), "ja_JP.UTF-8".into()),
                ("COLORTERM".into(), "truecolor".into()),
                ("TERM_PROGRAM".into(), "Apple_Terminal".into()),
                ("TERM_SESSION_ID".into(), "host-window-1".into()),
                ("SECRET".into(), "do-not-copy".into()),
            ]),
            PathBuf::from("/workspace"),
        );
        let resolved = profile.resolve(&request("login-shell")).unwrap();
        assert_eq!(resolved.snapshot.program, "/bin/zsh");
        assert_eq!(resolved.snapshot.arguments, ["-l", "-i"]);
        assert_eq!(
            resolved.snapshot.working_directory,
            PathBuf::from("/workspace")
        );
        assert_eq!(resolved.environment.len(), 8);
        assert_eq!(
            resolved
                .environment
                .iter()
                .find(|(name, _)| name.as_str() == "COLORTERM")
                .map(|(_, value)| value.as_str()),
            Some("truecolor")
        );
        assert_eq!(
            resolved
                .environment
                .iter()
                .find(|(name, _)| name.as_str() == "TERM_SESSION_ID")
                .map(|(_, value)| value.as_str()),
            Some("")
        );
        assert!(
            !resolved
                .environment
                .values()
                .any(|value| value == "do-not-copy")
        );
        assert!(
            !serde_json::to_string(&resolved.snapshot)
                .unwrap()
                .contains("xterm-256color")
        );
    }

    #[test]
    fn falls_back_to_sh_and_rejects_unknown_profile() {
        let profile = LoginShellProfile::new(
            BTreeMap::from([("SHELL".into(), "zsh".into())]),
            PathBuf::from("."),
        );
        assert_eq!(
            profile
                .resolve(&request("login-shell"))
                .unwrap()
                .snapshot
                .program,
            "/bin/sh"
        );
        assert!(profile.resolve(&request("other")).is_err());
    }

    #[test]
    fn rejects_a_profile_without_a_working_directory() {
        let profile = LoginShellProfile::new(BTreeMap::new(), PathBuf::new());
        assert!(profile.resolve(&request("login-shell")).is_err());
    }
}
