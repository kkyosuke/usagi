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
///
/// `TMPDIR` is inherited so a child confined by the OS sandbox launcher
/// (`usagi claude-sandbox`) can still write into its own temporary area: the
/// launcher turns `$TMPDIR` into a writable root, and the product needs the same
/// value to actually use it.
///
/// `USER` is the one name here whose value is not simply inherited: it is
/// resolved from the daemon's own effective UID by
/// [`public_terminal_environment`], because a child that authenticates against
/// an OS keychain is indexed by the user it actually runs as.
pub const TERMINAL_ENVIRONMENT_VARIABLES: [&str; 17] = [
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
    "TMPDIR",
    "USER",
];

/// The name whose value the daemon resolves instead of inheriting.
pub const USER_ENVIRONMENT_VARIABLE: &str = "USER";

/// Composes the public terminal environment that every daemon-owned PTY child
/// shares: the generic `login-shell` profile and the Agent PTY's public base.
///
/// `inherited` reads one name from the daemon's own environment.
/// `resolved_user` is the OS user name for the daemon's effective UID, resolved
/// once per process rather than per launch.
///
/// The resolved name wins over an inherited `USER`, which is only a string the
/// launching environment chose and can name a different account than the one the
/// daemon actually runs as. When the platform cannot answer, a usable inherited
/// value is kept, and when neither is usable the variable is simply absent: the
/// same fail-safe the configured environment uses for a binding it cannot
/// resolve, so a pane still opens.
pub fn public_terminal_environment(
    inherited: impl Fn(&str) -> Option<String>,
    resolved_user: Option<&str>,
) -> BTreeMap<String, String> {
    let mut environment = TERMINAL_ENVIRONMENT_VARIABLES
        .into_iter()
        .filter(|name| *name != USER_ENVIRONMENT_VARIABLE)
        .filter_map(|name| inherited(name).map(|value| (name.to_owned(), value)))
        .collect::<BTreeMap<_, _>>();
    if let Some(user) = usable(resolved_user.map(str::to_owned))
        .or_else(|| usable(inherited(USER_ENVIRONMENT_VARIABLE)))
    {
        environment.insert(USER_ENVIRONMENT_VARIABLE.to_owned(), user);
    }
    environment
}

/// Whether a candidate user name can be handed to a PTY child at all. An empty
/// value is indistinguishable from an unset one to the shell, and a NUL cannot
/// cross the spawn boundary.
fn usable(candidate: Option<String>) -> Option<String> {
    candidate.filter(|value| !value.is_empty() && !value.contains('\0'))
}

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
    use super::{LoginShellProfile, public_terminal_environment};
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
                ("USER".into(), "resolved-user".into()),
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
        assert_eq!(resolved.environment.len(), 9);
        assert_eq!(
            resolved
                .environment
                .iter()
                .find(|(name, _)| name.as_str() == "USER")
                .map(|(_, value)| value.as_str()),
            Some("resolved-user")
        );
        assert!(
            resolved
                .snapshot
                .environment_allowlist
                .iter()
                .any(|name| name.as_str() == "USER"),
            "the durable allowlist must admit the name the launch boundary validates"
        );
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

    /// The daemon's own environment, including values a PTY child must never
    /// receive. The reader answers any name, exactly as a full copy of the
    /// parent environment would, so the allowlist is what keeps the secrets out.
    fn daemon_environment(user: Option<&str>) -> BTreeMap<String, String> {
        let mut environment = BTreeMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("GH_TOKEN".to_owned(), "must-not-leak".to_owned()),
            (
                "OP_SERVICE_ACCOUNT_TOKEN".to_owned(),
                "must-not-leak".to_owned(),
            ),
        ]);
        if let Some(user) = user {
            environment.insert("USER".to_owned(), user.to_owned());
        }
        environment
    }

    #[test]
    fn the_resolved_user_wins_over_an_inherited_one_and_no_secret_is_copied() {
        let inherited = daemon_environment(Some("inherited-user"));
        let environment = public_terminal_environment(
            |name| inherited.get(name).cloned(),
            Some("uid-resolved-user"),
        );
        assert_eq!(environment["USER"], "uid-resolved-user");
        assert_eq!(environment["TERM"], "xterm-256color");
        assert!(!environment.contains_key("GH_TOKEN"));
        assert!(!environment.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));
    }

    #[test]
    fn an_unresolvable_user_falls_back_to_a_usable_inherited_value() {
        let inherited = daemon_environment(Some("inherited-user"));
        let environment = public_terminal_environment(|name| inherited.get(name).cloned(), None);
        assert_eq!(environment["USER"], "inherited-user");
    }

    #[test]
    fn an_unusable_resolved_user_falls_back_to_the_inherited_value() {
        let inherited = daemon_environment(Some("inherited-user"));
        for unusable in ["", "with\0nul"] {
            let environment =
                public_terminal_environment(|name| inherited.get(name).cloned(), Some(unusable));
            assert_eq!(environment["USER"], "inherited-user");
        }
    }

    #[test]
    fn no_usable_user_leaves_the_variable_absent_without_failing_the_launch() {
        for inherited_user in [None, Some(""), Some("with\0nul")] {
            let inherited = daemon_environment(inherited_user);
            let environment =
                public_terminal_environment(|name| inherited.get(name).cloned(), None);
            assert!(!environment.contains_key("USER"));
            assert_eq!(environment["TERM"], "xterm-256color");
        }
    }

    #[test]
    fn rejects_a_profile_without_a_working_directory() {
        let profile = LoginShellProfile::new(BTreeMap::new(), PathBuf::new());
        assert!(profile.resolve(&request("login-shell")).is_err());
    }
}
