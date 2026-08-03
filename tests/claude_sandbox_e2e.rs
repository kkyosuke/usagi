//! Shipping `usagi claude-sandbox` session-scope filesystem boundary tests.
//!
//! A missing backend is also fail-closed, so denied effects remain testable on every Unix CI host.
//! The positive own-worktree case runs when the platform backend is operational.

#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ORIGINAL: &[u8] = b"authority-sentinel\n";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("claude-sandbox-session-scope")
}

fn run_in_session(own: &Path, script: &str, arguments: &[&Path]) -> Output {
    let protected = fixture_root()
        .canonicalize()
        .expect("canonical protected root");
    let own = own.canonicalize().expect("canonical writable root");
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        .args(["claude-sandbox", "--mode", "session", "--protected-root"])
        .arg(protected)
        .arg("--writable-root")
        .arg(&own);
    #[cfg(target_os = "macos")]
    command.arg("--backend").arg(
        PathBuf::from("/usr/bin/sandbox-exec")
            .canonicalize()
            .expect("shipping sandbox backend"),
    );
    command
        .arg("--")
        .args(["/bin/sh", "-c", script, "sandbox-test"])
        .args(arguments)
        .current_dir(own)
        .output()
        .expect("shipping launcher starts")
}

fn assert_unchanged(path: &Path) {
    assert_eq!(
        fs::read(path).unwrap(),
        ORIGINAL,
        "{} changed",
        path.display()
    );
}

fn sandbox_backend_unavailable(output: &Output) -> bool {
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    diagnostic.contains("sandbox_apply: Operation not permitted")
        || diagnostic.contains("sandbox backend")
        || diagnostic.contains("OS sandbox backend")
}

#[test]
fn session_scope_preserves_sibling_issue_and_daemon_authority_for_path_alias_matrix() {
    let fixture = fixture_root();
    let _ = fs::remove_dir_all(&fixture);
    let own = fixture.join("sessions/a");
    let sibling = fixture.join("sessions/b/sentinel");
    let issue = fixture.join("issues/630.md");
    let daemon = fixture.join("daemon/state.json");
    for path in [
        &own,
        sibling.parent().unwrap(),
        issue.parent().unwrap(),
        daemon.parent().unwrap(),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    for path in [&sibling, &issue, &daemon] {
        fs::write(path, ORIGINAL).unwrap();
    }

    let absolute = run_in_session(&own, "printf attack > \"$1\"", &[&sibling]);
    assert!(!absolute.status.success());
    assert_unchanged(&sibling);

    let dotdot = run_in_session(&own, "printf attack > ../b/sentinel", &[]);
    assert!(!dotdot.status.success());
    assert_unchanged(&sibling);

    let alias = own.join("sibling-alias");
    std::os::unix::fs::symlink(sibling.parent().unwrap(), &alias).unwrap();
    let symlink = run_in_session(&own, "printf attack > sibling-alias/sentinel", &[]);
    assert!(!symlink.status.success());
    assert_unchanged(&sibling);

    let hardlink = own.join("authority-hardlink");
    let link = run_in_session(&own, "ln \"$1\" \"$2\"", &[&issue, &hardlink]);
    assert!(!link.status.success());
    assert_unchanged(&issue);
    assert!(!hardlink.exists());

    let remove = run_in_session(&own, "rm \"$1\"", &[&issue]);
    assert!(!remove.status.success());
    assert_unchanged(&issue);

    let moved = own.join("stolen-daemon-state");
    let rename = run_in_session(&own, "mv \"$1\" \"$2\"", &[&daemon, &moved]);
    assert!(!rename.status.success());
    assert_unchanged(&daemon);
    assert!(!moved.exists());

    let own_file = own.join("allowed");
    let allowed = run_in_session(&own, "printf own > \"$1\"", &[&own_file]);
    if allowed.status.success() {
        assert_eq!(fs::read(own_file).unwrap(), b"own");
    } else {
        assert!(
            sandbox_backend_unavailable(&allowed),
            "an operational backend must allow own-worktree writes: {}",
            String::from_utf8_lossy(&allowed.stderr)
        );
    }

    let _ = fs::remove_dir_all(&fixture);
}
