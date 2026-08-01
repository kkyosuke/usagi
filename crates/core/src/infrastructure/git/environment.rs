//! The environment a `git` subprocess is allowed to inherit.
//!
//! Pointing git at a repository with `-C <repo>` is not enough to scope it. Git
//! resolves its repository, index, object database, hooks and configuration from
//! the `GIT_*` namespace **in preference to** the directory it was pointed at, so
//! an inherited `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` /
//! `GIT_OBJECT_DIRECTORY`, or a `GIT_CONFIG_COUNT` injection, silently redirects
//! the command to another repository. The daemon inherits whatever environment
//! started it and lives as long as the machine does, so every invocation has to
//! re-establish that scope itself.
//!
//! This module is the single place that decides it. The decision is a pure
//! function of the inherited variable *names*, and the subprocess boundaries (the
//! daemon's `GitRunner`, the issue-number sequence resolver) only build a
//! [`Command`] through [`confined_git_command`].

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

/// The `GIT_*` variables a confined git subprocess keeps from its parent.
///
/// These pick the program git uses to reach a *remote*, which a clone of a
/// private repository needs and which cannot re-point the command at another
/// local repository. Every other `GIT_*` variable is dropped, including the ones
/// that only look harmless: `GIT_EXEC_PATH` and `GIT_ASKPASS` name programs git
/// runs, and `GIT_TRACE*` names a file git writes.
pub const INHERITED_GIT_VARIABLES: &[&str] = &["GIT_SSH", "GIT_SSH_COMMAND", "GIT_SSH_VARIANT"];

/// The values every confined git subprocess is given, whatever the parent held.
///
/// `LC_ALL` pins git's messages to the C locale because callers branch on them
/// (`git worktree remove` reporting "is not a working tree" is a no-op, not a
/// failure). `GIT_TERMINAL_PROMPT` refuses credential prompts: a daemon has no
/// terminal to answer one on, so a prompt would hang a session operation instead
/// of failing it. Configured credential helpers still run.
pub const CONFINED_GIT_VALUES: &[(&str, &str)] = &[("GIT_TERMINAL_PROMPT", "0"), ("LC_ALL", "C")];

/// Whether an inherited variable named `name` must be dropped before running
/// git: everything in the `GIT_*` namespace except [`INHERITED_GIT_VARIABLES`].
///
/// Deny-by-default is what makes this safe to leave alone as git grows new
/// variables — a new way to select a repository, an index or a config source is
/// confined without this list being updated.
fn is_confined_variable(name: &str) -> bool {
    name.starts_with("GIT_") && !INHERITED_GIT_VARIABLES.contains(&name)
}

/// Drop the confined variables of `inherited` from `command` and set the values
/// every git subprocess gets.
///
/// The injected values are applied last, so a value in `inherited` can never win
/// over them.
fn confine(command: &mut Command, inherited: &[OsString]) {
    for name in inherited
        .iter()
        .filter(|name| is_confined_variable(&name.to_string_lossy()))
    {
        command.env_remove(name);
    }
    for (name, value) in CONFINED_GIT_VALUES {
        command.env(name, value);
    }
}

/// A `git` command scoped to `repo`, with the environment confined to it.
///
/// This is the only way this workspace builds a `git` command: `-C repo` names
/// the repository and the confinement keeps the inherited environment from
/// naming a different one.
#[must_use]
pub fn confined_git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    let inherited: Vec<OsString> = std::env::vars_os().map(|(name, _)| name).collect();
    confine(&mut command, &inherited);
    command
}

#[cfg(test)]
mod tests {
    use super::{
        CONFINED_GIT_VALUES, INHERITED_GIT_VARIABLES, confine, confined_git_command,
        is_confined_variable,
    };
    use std::ffi::{OsStr, OsString};
    use std::path::Path;
    use std::process::Command;

    /// The (name, value) pairs `command` carries, with `None` for a removal.
    fn envs(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn every_git_variable_but_the_transport_ones_is_confined() {
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_PREFIX",
            "GIT_CEILING_DIRECTORIES",
            "GIT_DISCOVERY_ACROSS_FILESYSTEM",
            "GIT_CONFIG",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_PARAMETERS",
            "GIT_EXEC_PATH",
            "GIT_ASKPASS",
            "GIT_TRACE",
            "GIT_TERMINAL_PROMPT",
        ] {
            assert!(is_confined_variable(name), "{name} must be confined");
        }
        for name in INHERITED_GIT_VARIABLES {
            assert!(!is_confined_variable(name), "{name} must be inherited");
        }
        // Nothing outside the namespace is touched: the locale, the command
        // search path and the home directory keep working as the user set them.
        for name in ["PATH", "HOME", "LANG", "LC_ALL", "SSH_AUTH_SOCK", "GITHUB"] {
            assert!(!is_confined_variable(name), "{name} must be inherited");
        }
    }

    #[test]
    fn an_injected_value_is_never_also_inherited() {
        // Otherwise the allowlist would readmit a value the injection replaces.
        for (name, _) in CONFINED_GIT_VALUES {
            assert!(
                !INHERITED_GIT_VARIABLES.contains(name),
                "{name} is both injected and inherited"
            );
        }
    }

    #[test]
    fn confining_removes_the_inherited_scope_and_injects_the_fixed_values() {
        let inherited: Vec<OsString> = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_CONFIG_COUNT",
            "GIT_SSH_COMMAND",
            "PATH",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let mut command = Command::new("git");
        confine(&mut command, &inherited);

        let envs = envs(&command);
        for name in ["GIT_DIR", "GIT_WORK_TREE", "GIT_CONFIG_COUNT"] {
            assert!(
                envs.contains(&(name.to_owned(), None)),
                "{name} was not removed: {envs:?}"
            );
        }
        // The inherited transport variable and everything outside the namespace
        // are left to the parent environment, so they carry no entry at all.
        for name in ["GIT_SSH_COMMAND", "PATH"] {
            assert!(
                !envs.iter().any(|(entry, _)| entry == name),
                "{name} was overridden: {envs:?}"
            );
        }
        for (name, value) in CONFINED_GIT_VALUES {
            assert!(
                envs.contains(&((*name).to_owned(), Some((*value).to_owned()))),
                "{name} was not injected: {envs:?}"
            );
        }
    }

    #[test]
    fn a_non_utf8_variable_name_is_confined_by_its_prefix() {
        // A lossy name still starts with the prefix, so an unrepresentable name
        // cannot smuggle a repository override past the filter.
        #[cfg(unix)]
        let hostile = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(b"GIT_DIR\xff".to_vec())
        };
        #[cfg(not(unix))]
        let hostile = OsString::from("GIT_DIR\u{fffd}");

        let mut command = Command::new("git");
        confine(&mut command, std::slice::from_ref(&hostile));
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == hostile.as_os_str() && value.is_none())
        );
    }

    #[test]
    fn a_confined_command_names_the_repository_it_is_scoped_to() {
        let command = confined_git_command(Path::new("/repo"));
        assert_eq!(command.get_program(), OsStr::new("git"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("-C"), OsStr::new("/repo")]
        );
        // The real inherited environment goes through the same policy: whatever
        // this process holds, the fixed values are on the command.
        let envs = envs(&command);
        for (name, value) in CONFINED_GIT_VALUES {
            assert!(
                envs.contains(&((*name).to_owned(), Some((*value).to_owned()))),
                "{name} was not injected: {envs:?}"
            );
        }
    }
}
