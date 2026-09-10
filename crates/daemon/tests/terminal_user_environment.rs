//! Real-PTY regression for the `USER` binding every daemon-owned child needs.
//!
//! macOS keychain clients — Claude Code among them — index a stored credential
//! by `$USER`. A child launched without it authenticates as a different account
//! than the same product does in the developer's own terminal, so the daemon has
//! to supply the name, and supply the one it actually runs as.
//!
//! The tests drive a **real** PTY child through the two boundaries that child
//! can arrive from — the generic `login-shell` profile and the Agent spawn
//! provision — and read the environment the child itself reports. Both are fed
//! by one composition, [`public_terminal_environment`], whose reader answers any
//! name the way a wholesale copy of the parent environment would: what keeps
//! `GH_TOKEN` out of the child is the allowlist, and these tests assert that in
//! the same run.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use usagi_core::domain::agent::EnvironmentVariableName;
use usagi_core::domain::id::{SessionId, WorkspaceId, WorktreeId};
use usagi_core::domain::terminal_launch::{
    TerminalLaunchRequest, TerminalLaunchScope, TerminalProfileId,
};
use usagi_daemon::infrastructure::{os_user::effective_user_name, pty::PtyTerminal};
use usagi_daemon::usecase::runtime::SpawnProvision;
use usagi_daemon::usecase::terminal::Geometry;
use usagi_daemon::usecase::terminal_profile::{LoginShellProfile, public_terminal_environment};

/// The daemon's own environment: public terminal characteristics, an inherited
/// `USER` naming a different account, and secrets that must never reach a child.
fn daemon_environment() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("SHELL".to_owned(), "/bin/sh".to_owned()),
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ("USER".to_owned(), "inherited-user".to_owned()),
        ("GH_TOKEN".to_owned(), "must-not-leak".to_owned()),
        (
            "OP_SERVICE_ACCOUNT_TOKEN".to_owned(),
            "must-not-leak".to_owned(),
        ),
    ])
}

/// The environment a real PTY child reports for itself.
fn child_environment(environment: &[(String, String)]) -> BTreeMap<String, String> {
    let terminal = PtyTerminal::spawn_with(
        "/bin/sh",
        &["-c".to_owned(), "env".to_owned()],
        environment,
        Path::new("/"),
        Geometry { cols: 80, rows: 24 },
    )
    .expect("the fixture child starts under a real PTY");
    let mut output = String::new();
    terminal
        .reader()
        .expect("the PTY master is readable")
        .read_to_string(&mut output)
        .expect("the child's own environment is readable");
    assert_eq!(terminal.wait().expect("the child is reaped"), 0);
    output
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn request(session_id: Option<SessionId>) -> TerminalLaunchRequest {
    TerminalLaunchRequest {
        profile_id: TerminalProfileId::new("login-shell").expect("the static profile is valid"),
        scope: TerminalLaunchScope {
            workspace_id: WorkspaceId::new(),
            session_id,
            worktree_id: WorktreeId::new(),
        },
    }
}

fn assert_no_secret_reached(child: &BTreeMap<String, String>) {
    assert!(!child.contains_key("GH_TOKEN"));
    assert!(!child.contains_key("OP_SERVICE_ACCOUNT_TOKEN"));
    assert!(!child.values().any(|value| value == "must-not-leak"));
}

#[test]
fn a_generic_terminal_child_runs_with_the_resolved_user_in_every_scope() {
    let inherited = daemon_environment();
    let profile = LoginShellProfile::new(
        public_terminal_environment(
            |name| inherited.get(name).cloned(),
            Some("uid-resolved-user"),
        ),
        Path::new("/").to_path_buf(),
    );
    // A managed session and the workspace root reach the same profile, so both
    // scopes are held to the same environment.
    for session_id in [Some(SessionId::new()), None] {
        let resolved = profile
            .resolve(&request(session_id))
            .expect("the login-shell profile resolves");
        let environment = resolved
            .environment
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect::<Vec<_>>();
        let child = child_environment(&environment);
        assert_eq!(
            child.get("USER").map(String::as_str),
            Some("uid-resolved-user")
        );
        assert_eq!(
            child.get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_no_secret_reached(&child);
    }
}

#[test]
fn an_agent_child_runs_with_the_resolved_user_unless_the_configured_env_replaces_it() {
    let inherited = daemon_environment();
    let public = public_terminal_environment(
        |name| inherited.get(name).cloned(),
        Some("uid-resolved-user"),
    );

    let agent = SpawnProvision::new(Vec::new(), Vec::new()).compose_environment(&public);
    let child = child_environment(&agent.into_iter().collect::<Vec<_>>());
    assert_eq!(
        child.get("USER").map(String::as_str),
        Some("uid-resolved-user")
    );
    assert_no_secret_reached(&child);

    // The configured environment is layered over the terminal characteristics,
    // which is what lets an operator name the account themselves.
    let configured = SpawnProvision::new(
        [(
            EnvironmentVariableName::new("USER").expect("the literal name is valid"),
            "configured-user".to_owned(),
        )],
        Vec::new(),
    )
    .compose_environment(&public);
    let child = child_environment(&configured.into_iter().collect::<Vec<_>>());
    assert_eq!(
        child.get("USER").map(String::as_str),
        Some("configured-user")
    );
    assert_no_secret_reached(&child);
}

#[test]
fn the_resolved_name_is_the_account_the_process_actually_runs_as() {
    let resolved = effective_user_name();
    let reported = std::process::Command::new("/usr/bin/id")
        .arg("-un")
        .output()
        .expect("the platform reports the current user");
    assert!(reported.status.success());
    let reported = String::from_utf8(reported.stdout)
        .expect("the reported user name is UTF-8")
        .trim()
        .to_owned();
    assert_eq!(resolved, Some(reported));
}
