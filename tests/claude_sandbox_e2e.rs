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

#[path = "support/daemon.rs"]
mod daemon_fixture;

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
    run_in_session_with_roots(&protected, own, &[], &[], script, arguments)
}

fn run_in_session_with_roots(
    protected: &Path,
    own: &Path,
    additional_roots: &[PathBuf],
    read_only_roots: &[PathBuf],
    script: &str,
    arguments: &[&Path],
) -> Output {
    let own = own.canonicalize().expect("canonical writable root");
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        .args(["claude-sandbox", "--mode", "session", "--protected-root"])
        .arg(protected)
        .arg("--writable-root")
        .arg(&own);
    for root in additional_roots {
        command.arg("--writable-root").arg(root);
    }
    for root in read_only_roots {
        command.arg("--read-only-root").arg(root);
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

fn git(repo: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
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
        .env("GIT_CONFIG_COUNT", "6")
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
        .env("GIT_CONFIG_KEY_5", "extensions.worktreeConfig")
        .env("GIT_CONFIG_VALUE_5", "false")
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
    run_agent_in_root_with_policy(root, home, program, &[], &[])
}

fn run_agent_in_root_with_policy(
    root: &Path,
    home: &Path,
    program: &Path,
    writable_roots: &[&Path],
    read_only_roots: &[&Path],
) -> Output {
    let protected = root.canonicalize().expect("canonical protected root");
    let home = home.canonicalize().expect("canonical home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_usagi"));
    command
        .args(["claude-sandbox", "--mode", "root", "--protected-root"])
        .arg(&protected)
        .arg("--home")
        .arg(&home);
    for writable_root in writable_roots {
        command.arg("--writable-root").arg(writable_root);
    }
    for read_only_root in read_only_roots {
        command.arg("--read-only-root").arg(read_only_root);
    }
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

fn assert_daemon_log_excludes(data: &Path, excluded: &str) {
    let entries = fs::read_dir(data.join("logs"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .collect::<String>();
    assert!(
        !entries.contains(excluded),
        "daemon log contains {excluded}: {entries}"
    );
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
fn session_scope_preserves_sibling_tracked_source_and_daemon_authority_for_path_alias_matrix() {
    let _guard = daemon_fixture::heavy_e2e_lock();
    let fixture = fixture_root();
    let _ = fs::remove_dir_all(&fixture);
    let own = fixture.join("sessions/a");
    let sibling = fixture.join("sessions/b/sentinel");
    let tracked = fixture.join("memory/630.md");
    let daemon = fixture.join("daemon/state.json");
    for path in [
        &own,
        sibling.parent().unwrap(),
        tracked.parent().unwrap(),
        daemon.parent().unwrap(),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    for path in [&sibling, &tracked, &daemon] {
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
    let link = run_in_session(&own, "ln \"$1\" \"$2\"", &[&tracked, &hardlink]);
    assert!(!link.status.success());
    assert_unchanged(&tracked);
    assert!(!hardlink.exists());

    let remove = run_in_session(&own, "rm \"$1\"", &[&tracked]);
    assert!(!remove.status.success());
    assert_unchanged(&tracked);

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

#[test]
fn session_scope_can_commit_without_granting_the_main_ref() {
    let _guard = daemon_fixture::heavy_e2e_lock();
    let protected = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("claude-sandbox-session-git");
    let _ = fs::remove_dir_all(&protected);
    fs::create_dir_all(protected.join(".usagi/sessions")).unwrap();
    git(&protected, &["init", "--quiet"]);
    git(&protected, &["config", "user.name", "fixture"]);
    git(
        &protected,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(protected.join("tracked"), "base\n").unwrap();
    git(&protected, &["add", "tracked"]);
    git(&protected, &["commit", "--quiet", "-m", "base"]);
    let main_before = git(&protected, &["rev-parse", "HEAD"]);
    let worktree = protected.join(".usagi/sessions/commit");
    git(
        &protected,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "usagi/sandbox-commit",
            worktree.to_str().unwrap(),
        ],
    );

    let git_dir = PathBuf::from(git(&worktree, &["rev-parse", "--git-dir"]))
        .canonicalize()
        .unwrap();
    let common = PathBuf::from(git(&worktree, &["rev-parse", "--git-common-dir"]))
        .canonicalize()
        .unwrap();
    let read_only_roots = [
        worktree.join(".git").canonicalize().unwrap(),
        git_dir.join("commondir").canonicalize().unwrap(),
        git_dir.join("gitdir").canonicalize().unwrap(),
    ];
    let roots = [
        git_dir,
        common.join("objects").canonicalize().unwrap(),
        common.join("refs/heads/usagi").canonicalize().unwrap(),
        common.join("logs/refs/heads/usagi").canonicalize().unwrap(),
    ];
    let protected = protected.canonicalize().unwrap();
    let tamper = run_in_session_with_roots(
        &protected,
        &worktree,
        &roots,
        &read_only_roots,
        "printf 'gitdir: /tmp\\n' > .git",
        &[],
    );
    assert!(!tamper.status.success(), ".git pointer must stay read-only");
    let output = run_in_session_with_roots(
        &protected,
        &worktree,
        &roots,
        &read_only_roots,
        "printf committed > tracked && git add tracked && git commit --quiet -m sandbox",
        &[],
    );
    if !output.status.success() && sandbox_backend_unavailable(&output) {
        let _ = fs::remove_dir_all(&protected);
        return;
    }
    assert!(
        output.status.success(),
        "sandboxed commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(git(&worktree, &["rev-parse", "HEAD"]), main_before);
    assert_eq!(git(&protected, &["rev-parse", "HEAD"]), main_before);

    let _ = fs::remove_dir_all(&protected);
}

/// A root coordinator must be able to write the state its own CLI needs. The
/// grant follows the launched program, so it never widens to another provider's
/// state directory. AGY receives only its conversation subtree, not all of
/// `.gemini`.
#[test]
fn root_scope_grants_only_the_state_directory_of_the_agent_it_launches() {
    let _guard = daemon_fixture::heavy_e2e_lock();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("agent-root-state-scope");
    let _ = fs::remove_dir_all(&fixture);
    let repo = fixture.join("repo");
    let home = fixture.join("home");
    let bin = fixture.join("bin");
    let state_directories = [".codex", ".claude", ".gemini/antigravity-cli/conversations"];
    for path in [
        &repo,
        &bin,
        &home.join(".codex"),
        &home.join(".claude"),
        &home.join(".gemini/antigravity-cli/conversations"),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    // The fixture programs differ only in name: each writes every state probe.
    for program in ["codex", "claude", "agy"] {
        let path = bin.join(program);
        fs::write(
            &path,
            "#!/bin/sh\ntouch \"$1/.codex/probe\"\ntouch \"$1/.claude/probe\"\ntouch \"$1/.gemini/antigravity-cli/conversations/probe\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    }

    for (program, granted) in [
        ("codex", ".codex"),
        ("claude", ".claude"),
        ("agy", ".gemini/antigravity-cli/conversations"),
    ] {
        let output = run_agent_in_root(&repo, &home, &bin.join(program));
        if !output.status.success() && sandbox_backend_unavailable(&output) {
            continue;
        }
        let granted_directory = granted;
        let granted = home.join(granted_directory).join("probe");
        assert!(
            granted.exists(),
            "{program} must write its own state: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        for denied in state_directories
            .iter()
            .copied()
            .filter(|directory| *directory != granted_directory)
        {
            assert!(
                !home.join(denied).join("probe").exists(),
                "{program} must not write another provider's state"
            );
        }
        fs::remove_file(granted).unwrap();
    }

    let _ = fs::remove_dir_all(&fixture);
}

#[test]
#[allow(clippy::too_many_lines)] // One sandbox run must show every granted AGY runtime path beside every denied customization path.
fn agy_grants_only_conversation_state_in_the_shipping_sandbox() {
    let _guard = daemon_fixture::heavy_e2e_lock();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("agy-global-config-read-only");
    let _ = fs::remove_dir_all(&fixture);
    let repo = fixture.join("repo");
    let home = fixture.join("home");
    let bin = fixture.join("bin");
    let integration = fixture.join("data/agent-integrations/workspace/agy/workspace");
    let integration_hooks = integration.join(".agents/hooks.json");
    let alias = repo.join(".usagi/workspace-agy-0123456789abcdef");
    let config = home.join(".gemini/config");
    let plugin = config.join("plugins/existing/plugin.json");
    let hooks = config.join("hooks.json");
    let mcp = config.join("mcp_config.json");
    let state = home.join(".gemini/antigravity-cli");
    let state_plugins = state.join("plugins");
    let state_plugin = state_plugins.join("existing/plugin.json");
    let state_skills = state.join("skills");
    let state_skill = state_skills.join("existing/SKILL.md");
    let settings = state.join("settings.json");
    let import_manifest = state.join("import_manifest.json");
    let statusline = state.join("statusline.sh");
    let title = state.join("title.sh");
    let global_context = home.join(".gemini/GEMINI.md");
    let conversations = state.join("conversations");
    let brain = state.join("brain");
    let presence = state.join("presence");
    let crashes = state.join("crashes");
    let summaries = state.join("conversation_summaries.db");
    let summaries_shm = state.join("conversation_summaries.db-shm");
    let summaries_wal = state.join("conversation_summaries.db-wal");
    let agentapi = state.join("bin/agentapi");
    for path in [
        &repo.join(".usagi"),
        &bin,
        plugin.parent().unwrap(),
        state_plugin.parent().unwrap(),
        state_skill.parent().unwrap(),
        integration_hooks.parent().unwrap(),
        agentapi.parent().unwrap(),
        &conversations,
        &brain,
        &presence,
        &crashes,
    ] {
        fs::create_dir_all(path).unwrap();
    }
    for path in [
        &plugin,
        &hooks,
        &mcp,
        &state_plugin,
        &state_skill,
        &settings,
        &import_manifest,
        &statusline,
        &title,
        &global_context,
        &integration_hooks,
        &summaries,
        &summaries_shm,
        &summaries_wal,
        &agentapi,
    ] {
        fs::write(path, ORIGINAL).unwrap();
    }
    // The daemon prepares the helper anchor executable (see
    // `prepare_agy_writable_paths`); AGY then rewrites it in place.
    fs::set_permissions(
        &agentapi,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let integration = integration.canonicalize().unwrap();
    std::os::unix::fs::symlink(&integration, &alias).unwrap();
    let program = bin.join("agy");
    fs::write(
        &program,
        format!(
            "#!/bin/sh\n\
             S=\"$1/.gemini/antigravity-cli\"\n\
             touch \"$S/conversations/state-probe\" || exit 10\n\
             printf summary > \"$S/conversation_summaries.db\" || exit 11\n\
             printf wal > \"$S/conversation_summaries.db-wal\" || exit 12\n\
             printf shm > \"$S/conversation_summaries.db-shm\" || exit 13\n\
             printf '#!/bin/sh\\nprintf helper-ran\\n' > \"$S/bin/agentapi\" || exit 14\n\
             mkdir \"$S/brain/conversation\" || exit 15\n\
             \"$S/bin/agentapi\" > \"$S/brain/conversation/task.md\" || exit 16\n\
             touch \"$S/presence/conversation.lock\" || exit 17\n\
             touch \"$S/crashes/crash.log\" || exit 18\n\
             printf attack > \"$1/.gemini/config/hooks.json\" && exit 20\n\
             printf attack > \"$1/.gemini/config/mcp_config.json\" && exit 21\n\
             mkdir -p \"$1/.gemini/config/plugins/injected\" && exit 22\n\
             mv \"$1/.gemini/config\" \"$1/.gemini/config-moved\" && exit 23\n\
             printf attack > \"$S/settings.json\" && exit 24\n\
             printf attack > \"$S/import_manifest.json\" && exit 25\n\
             mkdir -p \"$S/plugins/injected\" && exit 26\n\
             printf attack > \"$S/skills/existing/SKILL.md\" && exit 27\n\
             printf attack > \"$1/.gemini/GEMINI.md\" && exit 28\n\
             printf attack > \"$S/statusline.sh\" && exit 29\n\
             printf attack > \"$S/title.sh\" && exit 30\n\
             printf attack > \"$S/new-status-command.sh\" && exit 31\n\
             mv \"$S\" \"$1/.gemini/antigravity-cli-moved\" && exit 32\n\
             mv \"$1/.gemini\" \"$1/.gemini-moved\" && exit 33\n\
             printf attack > \"$S/bin/injected\" && exit 34\n\
             printf attack > '{hooks}' && exit 35\n\
             printf attack > '{alias}/.agents/hooks.json' && exit 36\n\
             rm '{alias}' && exit 37\n\
             ln -sfn \"$1\" '{alias}' && exit 38\n\
             exit 0\n",
            hooks = integration.join(".agents/hooks.json").display(),
            alias = alias.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();

    let output = run_agent_in_root_with_policy(
        &repo,
        &home,
        &program,
        &[
            &brain,
            &presence,
            &crashes,
            &summaries,
            &summaries_shm,
            &summaries_wal,
            &agentapi,
        ],
        &[&integration],
    );
    if !output.status.success() && sandbox_backend_unavailable(&output) {
        let _ = fs::remove_dir_all(&fixture);
        return;
    }
    assert!(
        output.status.success(),
        "AGY runtime state must remain writable while global customization is denied (exit {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(conversations.join("state-probe").is_file());
    assert_eq!(fs::read(&summaries).unwrap(), b"summary");
    assert_eq!(fs::read(&summaries_wal).unwrap(), b"wal");
    assert_eq!(fs::read(&summaries_shm).unwrap(), b"shm");
    assert_eq!(
        fs::read(brain.join("conversation/task.md")).unwrap(),
        b"helper-ran",
        "the rewritten run_command helper must stay executable"
    );
    assert!(presence.join("conversation.lock").is_file());
    assert!(crashes.join("crash.log").is_file());
    for path in [
        &plugin,
        &hooks,
        &mcp,
        &state_plugin,
        &state_skill,
        &settings,
        &import_manifest,
        &statusline,
        &title,
        &global_context,
        &integration.join(".agents/hooks.json"),
    ] {
        assert_unchanged(path);
    }
    assert_eq!(fs::read_link(&alias).unwrap(), integration);
    assert!(!config.join("plugins/injected").exists());
    assert!(!state.join("plugins/injected").exists());
    assert!(!state.join("new-status-command.sh").exists());
    assert!(!state.join("bin/injected").exists());
    assert!(config.is_dir());
    assert!(state.is_dir());
    assert!(!home.join(".gemini/config-moved").exists());
    assert!(!state.join("settings-moved.json").exists());
    assert!(!home.join(".gemini/antigravity-cli-moved").exists());
    assert!(!home.join(".gemini-moved").exists());

    let _ = fs::remove_dir_all(&fixture);
}

#[test]
fn claude_global_config_atomic_save_preserves_other_existing_home_entries() {
    let _guard = daemon_fixture::heavy_e2e_lock();
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
    let _guard = daemon_fixture::heavy_e2e_lock();
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
    let _guard = daemon_fixture::heavy_e2e_lock();
    // Keep the Unix socket path below `SUN_LEN`. The sandbox always grants
    // `/tmp` and `/var/tmp`, so putting the protected repository below either
    // directory would make a universal writable root its ancestor.
    let codex_dir = PathBuf::from(std::env::var_os("HOME").expect("test HOME")).join(".codex");
    let fixture = codex_dir.join(format!("ub{}", std::process::id()));
    let _ = fs::remove_dir_all(&fixture);
    let repo = fixture.join("repo");
    let data = fixture.join("data");
    fs::create_dir_all(&repo).unwrap();
    for dir in [&codex_dir, &fixture, &repo] {
        fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    }
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
    assert_daemon_log_excludes(&data, "peer process identity unavailable");
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
/// by signalling the exact process incarnation named in its record and waiting
/// for it to retire.
fn signal_daemon_away(data: &Path) {
    assert!(
        daemon_fixture::signal_exact_daemon(data, libc::SIGTERM),
        "the signalled daemon never cleared its record"
    );
}
