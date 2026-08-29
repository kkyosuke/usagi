//! Shipping `usagi claude-sandbox` Agent filesystem boundary tests.
//!
//! A missing backend is also fail-closed, so denied effects remain testable on every Unix CI host.
//! The positive own-worktree case runs when the platform backend is operational.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const ORIGINAL: &[u8] = b"authority-sentinel\n";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("claude-sandbox-session-scope")
}

fn run_in_session(own: &Path, script: &str, arguments: &[&Path]) -> Output {
    run_in_session_with_roots(&fixture_root(), own, &[], script, arguments)
}

fn run_in_session_with_roots(
    protected: &Path,
    own: &Path,
    additional_writable_roots: &[&Path],
    script: &str,
    arguments: &[&Path],
) -> Output {
    let protected = protected.canonicalize().expect("canonical protected root");
    let own = own.canonicalize().expect("canonical writable root");
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        .args(["claude-sandbox", "--mode", "session", "--protected-root"])
        .arg(protected)
        .arg("--writable-root")
        .arg(&own);
    for root in additional_writable_roots {
        command
            .arg("--writable-root")
            .arg(root.canonicalize().expect("canonical writable root"));
    }
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

#[test]
fn session_scope_can_commit_its_linked_worktree_without_writing_sibling_content() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("agent-session-git-scope");
    let _ = fs::remove_dir_all(&fixture);
    let repository = fixture.join("repository");
    let own = fixture.join("sessions/a");
    let sibling = fixture.join("sessions/b/sentinel");
    fs::create_dir_all(&repository).unwrap();
    fs::create_dir_all(sibling.parent().unwrap()).unwrap();
    fs::write(&sibling, ORIGINAL).unwrap();

    let git = |cwd: &Path, arguments: &[&str]| {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&repository, &["init", "--quiet"]);
    git(&repository, &["config", "user.name", "fixture"]);
    git(
        &repository,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(repository.join("tracked"), "base\n").unwrap();
    git(&repository, &["add", "tracked"]);
    git(&repository, &["commit", "--quiet", "-m", "base"]);
    git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "usagi/a",
            own.to_str().unwrap(),
        ],
    );
    let common = repository.join(".git");
    let output = run_in_session_with_roots(
        &fixture,
        &own,
        &[&common],
        "printf change >> tracked && git add tracked && git -c user.name=fixture -c user.email=fixture@example.invalid commit --quiet -m change",
        &[],
    );
    if !output.status.success() && sandbox_backend_unavailable(&output) {
        let _ = fs::remove_dir_all(&fixture);
        return;
    }
    assert!(
        output.status.success(),
        "linked-worktree commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_unchanged(&sibling);
    let count = Command::new("git")
        .args(["rev-list", "--count", "usagi/a"])
        .current_dir(&repository)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(count.stdout).unwrap().trim(), "2");

    let _ = fs::remove_dir_all(&fixture);
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

fn run_usagi_in_root(root: &Path, data: &Path, arguments: &[&str]) -> Output {
    let protected = root.canonicalize().expect("canonical protected root");
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_usagi"));
    let mut command = Command::new(&binary);
    command
        .args(["claude-sandbox", "--mode", "root", "--protected-root"])
        .arg(&protected);
    #[cfg(target_os = "macos")]
    command.arg("--backend").arg("/usr/bin/sandbox-exec");
    command
        .arg("--")
        .arg(&binary)
        .args(arguments)
        .current_dir(&protected)
        .env("USAGI_HOME", data)
        .env("USAGI_RUNTIME_MODE", "production")
        .env("USAGI_WORKSPACE_ROOT", &protected)
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

fn bootstrap_broker_sockets(data: &Path) -> Vec<PathBuf> {
    let daemon = data.join("daemon");
    let mut sockets = fs::read_dir(daemon)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("bootstrap-broker-") && name.ends_with(".sock")
            })
        })
        .collect::<Vec<_>>();
    sockets.sort();
    sockets
}

/// The broker sockets under `data`, once at least one has been bound.
fn wait_for_broker_socket(data: &Path) -> Vec<PathBuf> {
    for _ in 0..200 {
        let sockets = bootstrap_broker_sockets(data);
        if !sockets.is_empty() {
            return sockets;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the daemon never published a bootstrap broker endpoint");
}

fn retire_bootstrap_brokers(repo: &Path, data: &Path, fixture: &Path) {
    let _ = fs::remove_dir_all(repo);
    let sockets = bootstrap_broker_sockets(data);
    for socket in &sockets {
        if let Ok(mut broker) = std::os::unix::net::UnixStream::connect(socket) {
            let _ = broker.write_all(b"P");
        }
    }
    for _ in 0..40 {
        if sockets.iter().all(|socket| !socket.exists()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        sockets.iter().all(|socket| !socket.exists()),
        "bootstrap broker did not retire"
    );
    fs::remove_dir_all(fixture).unwrap();
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
fn claude_global_config_atomic_save_preserves_other_existing_home_entries() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("claude-global-config-prefix");
    let _ = fs::remove_dir_all(&fixture);
    let repo = fixture.join("repo");
    let home = fixture.join("home");
    let bin = fixture.join("bin");
    for path in [&repo, &home.join(".claude"), &bin] {
        fs::create_dir_all(path).unwrap();
    }
    let config = home.join(".claude.json");
    let guard = home.join("existing-guard");
    fs::write(&config, "old\n").unwrap();
    fs::write(&guard, ORIGINAL).unwrap();
    let program = bin.join("claude");
    fs::write(
        &program,
        "#!/bin/sh\ncp \"$1/.claude.json\" \"$1/.claude.json.backup.1\" || exit 1\n: > \"$1/.claude.json.lock\" || exit 1\nprintf 'trusted\\n' > \"$1/.claude.json.tmp.$$\" || exit 1\nmv \"$1/.claude.json.tmp.$$\" \"$1/.claude.json\" || exit 1\nrm \"$1/.claude.json.lock\"\n",
    )
    .unwrap();
    fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();

    let saved = run_agent_in_root(&repo, &home, &program);
    if !saved.status.success() && sandbox_backend_unavailable(&saved) {
        let _ = fs::remove_dir_all(&fixture);
        return;
    }
    assert!(
        saved.status.success(),
        "atomic config save must succeed: {}",
        String::from_utf8_lossy(&saved.stderr)
    );
    assert_eq!(fs::read(&config).unwrap(), b"trusted\n");
    assert_eq!(
        fs::read(home.join(".claude.json.backup.1")).unwrap(),
        b"old\n"
    );
    assert!(!home.join(".claude.json.lock").exists());

    fs::write(
        &program,
        "#!/bin/sh\nprintf attack > \"$1/existing-guard\"\n",
    )
    .unwrap();
    let refused = run_agent_in_root(&repo, &home, &program);
    assert!(!refused.status.success());
    assert_unchanged(&guard);

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

#[test]
fn root_scope_cold_starts_through_the_out_of_sandbox_broker() {
    // Keep the Unix socket path below `SUN_LEN`. The sandbox always grants
    // `/tmp` and `/var/tmp`, so putting the protected repository below either
    // directory would make a universal writable root its ancestor.
    let fixture = PathBuf::from(std::env::var_os("HOME").expect("test HOME"))
        .join(".codex")
        .join(format!("ub{}", std::process::id()));
    let _ = fs::remove_dir_all(&fixture);
    let repo = fixture.join("repo");
    let data = fixture.join("data");
    fs::create_dir_all(&repo).unwrap();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_usagi"));
    let run = |arguments: &[&str]| {
        Command::new(&binary)
            .args(arguments)
            .current_dir(&repo)
            .env("USAGI_HOME", &data)
            .env("USAGI_RUNTIME_MODE", "production")
            .env("USAGI_WORKSPACE_ROOT", &repo)
            .output()
            .unwrap()
    };
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(&repo)
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
    fs::write(repo.join("tracked"), "base\n").unwrap();
    git(&["add", "tracked"]);
    git(&["commit", "--quiet", "-m", "base"]);

    let started = run(&["daemon", "start"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );

    // `daemon start` returns once the daemon has registered itself, which is
    // before the broker it spawns has finished binding.
    let sockets = wait_for_broker_socket(&data);
    assert_eq!(sockets.len(), 1, "one broker for this workspace and binary");

    // The daemon goes away without anyone running `usagi daemon stop`: a
    // supervisor stop, a signal, a crash. This is the case the broker exists
    // for, so it must still be there afterwards — a sandboxed root client
    // cannot spawn a replacement for it.
    signal_daemon_away(&data);
    assert!(
        sockets[0].exists(),
        "the broker did not outlive the daemon that started it"
    );

    // One client that connects without sending a request must time out instead
    // of monopolising the broker's single accept loop forever.
    let idle = std::os::unix::net::UnixStream::connect(&sockets[0]).unwrap();
    let mut broker = std::os::unix::net::UnixStream::connect(&sockets[0]).unwrap();
    broker.write_all(b"S").unwrap();
    let mut reply = [0_u8; 1];
    broker.read_exact(&mut reply).unwrap();
    assert_eq!(reply, [b'O']);
    drop(idle);

    // Leave the workspace daemonless again so the root-scope client below has to
    // reach the broker rather than an endpoint that is already up.
    signal_daemon_away(&data);
    assert!(sockets[0].exists());

    let created = run_usagi_in_root(&repo, &data, &["session", "create", "brokered"]);
    if !created.status.success() && sandbox_backend_unavailable(&created) {
        let _ = run(&["daemon", "stop", "--force"]);
        retire_bootstrap_brokers(&repo, &data, &fixture);
        return;
    }
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(repo.join(".usagi/sessions/brokered/.git").is_file());

    let removed = run(&["session", "remove", "brokered"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let stopped = run(&["daemon", "stop"]);
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    // An explicit stop is the other end of the broker's life: nothing usagi owns
    // for this workspace is left running, so the operator's `stop` really stops.
    for socket in bootstrap_broker_sockets(&data) {
        for _ in 0..40 {
            if !socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !socket.exists(),
            "`daemon stop` left a broker running: {}",
            socket.display()
        );
    }
    retire_bootstrap_brokers(&repo, &data, &fixture);
}

/// End the running daemon the way anything other than `usagi daemon stop` does:
/// by signalling the process named in its record and waiting for it to retire.
///
/// `daemon stop` deliberately also retires the bootstrap broker, so it cannot be
/// used to reach the state this exercises — a workspace whose daemon is gone and
/// whose broker is still there to start a replacement.
fn signal_daemon_away(data: &Path) {
    let record = data.join("daemon/daemon.json");
    // A daemon the broker started registers itself a moment after the broker
    // reports success, so the record is waited for rather than assumed.
    let mut text = String::new();
    for _ in 0..200 {
        if let Ok(contents) = fs::read_to_string(&record)
            && contents.contains("\"pid\"")
        {
            text = contents;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!text.is_empty(), "no daemon recorded itself to signal");
    let pid = text
        .split("\"pid\"")
        .nth(1)
        .and_then(|tail| {
            tail.split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
        })
        .and_then(|digits| digits.parse::<i32>().ok())
        .expect("the daemon record names a pid");
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    for _ in 0..100 {
        if !record.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the signalled daemon never cleared its record");
}
