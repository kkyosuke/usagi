//! Shipping `usagi claude-sandbox` Agent filesystem boundary tests.
//!
//! A missing backend is also fail-closed, so denied effects remain testable on every Unix CI host.
//! The positive own-worktree case runs when the platform backend is operational.

#![cfg(unix)]

use std::collections::BTreeMap;
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

fn run_in_root(root: &Path, script: &str) -> Output {
    let protected = root.canonicalize().expect("canonical protected root");
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        .args(["claude-sandbox", "--mode", "root", "--protected-root"])
        .arg(&protected);
    #[cfg(target_os = "macos")]
    command.arg("--backend").arg(
        PathBuf::from("/usr/bin/sandbox-exec")
            .canonicalize()
            .expect("shipping sandbox backend"),
    );
    command
        .args(["--", "/bin/sh", "-c", script])
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "5")
        .env("GIT_CONFIG_KEY_0", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_1", "/dev/null")
        .env("GIT_CONFIG_KEY_2", "submodule.recurse")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env("GIT_CONFIG_KEY_3", "status.submoduleSummary")
        .env("GIT_CONFIG_VALUE_3", "false")
        .env("GIT_CONFIG_KEY_4", "diff.ignoreSubmodules")
        .env("GIT_CONFIG_VALUE_4", "all")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "")
        .env("GIT_EXTERNAL_DIFF", "")
        .output()
        .expect("shipping launcher starts")
}

/// Runs `program` (a fixture executable whose *name* selects the provider) as a
/// root coordinator, with `home` as the launcher's `$HOME` policy input.
fn run_agent_in_root(root: &Path, home: &Path, program: &Path) -> Output {
    let protected = root.canonicalize().expect("canonical protected root");
    let home = home.canonicalize().expect("canonical home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        .args(["claude-sandbox", "--mode", "root", "--protected-root"])
        .arg(&protected)
        .arg("--home")
        .arg(&home);
    #[cfg(target_os = "macos")]
    command.arg("--backend").arg(
        PathBuf::from("/usr/bin/sandbox-exec")
            .canonicalize()
            .expect("shipping sandbox backend"),
    );
    command
        .arg("--")
        .arg(program)
        .arg(&home)
        .current_dir(&protected)
        .output()
        .expect("shipping launcher starts")
}

fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            let relative = entry.strip_prefix(root).unwrap().to_path_buf();
            let metadata = fs::symlink_metadata(&entry).unwrap();
            if metadata.file_type().is_symlink() {
                snapshot.insert(
                    relative,
                    fs::read_link(entry)
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes(),
                );
            } else if metadata.is_dir() {
                visit(root, &entry, snapshot);
            } else {
                snapshot.insert(relative, fs::read(entry).unwrap());
            }
        }
    }
    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
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

/// A root coordinator must be able to write the state its own CLI needs — Codex
/// keeps `state_5.sqlite` under `~/.codex`, and a launcher that granted only
/// `~/.claude` made `usagi` unable to start Codex at the workspace root at all
/// ("attempt to write a readonly database"). The grant follows the launched
/// program, so it never widens to another provider's state.
#[test]
fn root_scope_grants_only_the_state_directory_of_the_agent_it_launches() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("agent-root-state-scope");
    let _ = fs::remove_dir_all(&fixture);
    let repo = fixture.join("repo");
    let home = fixture.join("home");
    let bin = fixture.join("bin");
    for path in [&repo, &bin, &home.join(".codex"), &home.join(".claude")] {
        fs::create_dir_all(path).unwrap();
    }
    // The fixture programs differ only in name: each writes both state probes.
    for program in ["codex", "claude"] {
        let path = bin.join(program);
        fs::write(
            &path,
            "#!/bin/sh\ntouch \"$1/.codex/probe\"\ntouch \"$1/.claude/probe\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    }

    for (program, granted, denied) in [
        ("codex", ".codex", ".claude"),
        ("claude", ".claude", ".codex"),
    ] {
        let output = run_agent_in_root(&repo, &home, &bin.join(program));
        if !output.status.success() && sandbox_backend_unavailable(&output) {
            continue;
        }
        let granted = home.join(granted).join("probe");
        let denied = home.join(denied).join("probe");
        assert!(
            granted.exists(),
            "{program} must write its own state: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !denied.exists(),
            "{program} must not write another provider's state"
        );
        fs::remove_file(granted).unwrap();
    }

    let _ = fs::remove_dir_all(&fixture);
}

#[test]
fn root_scope_keeps_checkout_and_git_common_dir_byte_identical() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("agent-root-read-only");
    let _ = fs::remove_dir_all(&fixture);
    fs::create_dir_all(&fixture).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&fixture)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&["config", "user.name", "fixture"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    let pwned = fixture.join("PWNED");
    git(&[
        "config",
        "diff.pwn.command",
        &format!("touch {}", pwned.display()),
    ]);
    fs::write(fixture.join(".gitattributes"), "tracked diff=pwn\n").unwrap();
    fs::write(fixture.join("tracked"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "base"]);
    git(&[
        "config",
        "core.fsmonitor",
        &format!("touch {}", pwned.display()),
    ]);
    fs::write(fixture.join("tracked"), "changed\n").unwrap();
    let before = tree_bytes(&fixture);

    for script in [
        "printf attack > tracked",
        "touch created",
        "ln -s /tmp escaped-link",
        "git add tracked",
        "git commit -am attack",
        "git -c diff.external=touch diff --ext-diff -- tracked",
        "git diff --ext-diff -- tracked",
    ] {
        let output = run_in_root(&fixture, script);
        assert!(
            !output.status.success(),
            "mutation unexpectedly succeeded: {script}"
        );
        assert_eq!(
            tree_bytes(&fixture),
            before,
            "repository changed after {script}"
        );
        assert!(!pwned.exists());
    }

    for script in [
        "git --no-pager --no-optional-locks status --short",
        "git --no-pager --no-optional-locks log --no-ext-diff --no-textconv -1",
        "git --no-pager --no-optional-locks diff --no-ext-diff --no-textconv -- tracked",
    ] {
        let output = run_in_root(&fixture, script);
        if !output.status.success() {
            assert!(
                sandbox_backend_unavailable(&output),
                "read-only command failed: {script}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(
            tree_bytes(&fixture),
            before,
            "repository changed after {script}"
        );
        assert!(!pwned.exists());
    }

    let _ = fs::remove_dir_all(&fixture);
}
