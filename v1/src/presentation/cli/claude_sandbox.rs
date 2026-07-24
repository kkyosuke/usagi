//! Fail-closed OS sandbox launcher used by every Claude invocation.
//!
//! The Claude adapter invokes this hidden subcommand instead of invoking
//! `claude` directly.  The launcher canonicalizes every writable root before
//! constructing the platform sandbox, then runs the requested command only
//! after the sandbox process has started successfully.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{bail, Context, Result};

/// Whether the launch cwd itself is writable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Session,
    Root,
}

impl Mode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "session" => Ok(Self::Session),
            "root" => Ok(Self::Root),
            _ => bail!("invalid Claude sandbox mode: {value}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SandboxCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

/// Run `command` inside the platform sandbox, returning an error instead of
/// falling back to an unprotected Claude process.
pub fn run(
    mode: Mode,
    mut extra_writable_roots: Vec<PathBuf>,
    command: Vec<OsString>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve Claude sandbox cwd")?;
    // Claude persists credentials and conversations here, while hooks and MCP
    // write usagi's own state. The ordinary OS temporary directories are also
    // writable: command-line tools commonly rely on them for sockets, locks,
    // downloads, and atomic replacements.
    let home = dirs::home_dir().context("Claude OS sandbox cannot resolve the home directory")?;
    let claude_state = home.join(".claude");
    let usagi_state = home.join(".usagi");
    std::fs::create_dir_all(&claude_state)
        .context("failed to prepare Claude's sandbox state directory")?;
    extra_writable_roots.push(claude_state);
    extra_writable_roots.push(usagi_state);
    extra_writable_roots.extend(writable_temp_roots());
    #[cfg(target_os = "macos")]
    extra_writable_roots.extend(macos_keychain_roots(&home)?);

    // `sandbox-exec` cannot create a second Seatbelt profile.  This is normal
    // when usagi itself is launched by a sandboxed host (for example Codex).
    // In that case the child inherits the host's already-active OS boundary,
    // so starting Claude directly preserves the boundary rather than dropping
    // it.  Do not use this as a general fallback: every other sandbox failure
    // still follows the fail-closed path below.
    #[cfg(target_os = "macos")]
    if inherits_macos_sandbox()? {
        return run_inherited_sandbox(&command);
    }

    run_in(
        &cwd,
        mode,
        extra_writable_roots,
        command,
        |program, args| {
            Command::new(program)
                .args(args)
                .status()
                .with_context(|| format!("failed to start OS sandbox {}", program.display()))
        },
    )
}

/// Existing conventional temporary roots that command-line tools may write.
///
/// `temp_dir()` preserves the user's platform-specific `$TMPDIR` (for example
/// macOS' per-user `/var/folders/.../T`). Unix also exposes shared `/tmp` and
/// `/var/tmp`; missing roots are skipped so an unusual host does not prevent
/// Claude from starting.
fn writable_temp_roots() -> Vec<PathBuf> {
    let mut roots = vec![std::env::temp_dir()];
    #[cfg(unix)]
    roots.extend(["/tmp", "/var/tmp"].into_iter().map(PathBuf::from));
    roots.retain(|root| root.is_dir());
    roots
}

/// Writable roots that keep the macOS Keychain usable inside the sandbox.
///
/// Claude stores its OAuth credentials in the login Keychain. Reading them
/// needs the Module Directory Service to maintain its per-user cache under
/// `$DARWIN_USER_CACHE_DIR`, and a token refresh rewrites the keychain
/// database under `~/Library/Keychains`. If either write is denied, Claude
/// cannot reach the Keychain and falls back to a possibly stale
/// `~/.claude/.credentials.json`, exiting with an authentication error.
#[cfg(target_os = "macos")]
fn macos_keychain_roots(home: &Path) -> Result<Vec<PathBuf>> {
    Ok(vec![
        home.join("Library/Keychains"),
        darwin_user_cache_dir()?,
    ])
}

/// The per-user Darwin cache directory
/// (`confstr(_CS_DARWIN_USER_CACHE_DIR)`, e.g. `/var/folders/<xx>/<id>/C/`).
#[cfg(target_os = "macos")]
fn darwin_user_cache_dir() -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: the buffer outlives the call and its capacity is passed with it.
    let len = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_CACHE_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if len == 0 || len > buf.len() {
        bail!("Claude sandbox cannot resolve the Darwin user cache directory");
    }
    // `len` counts the trailing NUL, which PathBuf must not carry.
    buf.truncate(len - 1);
    Ok(PathBuf::from(OsString::from_vec(buf)))
}

/// Whether the current process already has a macOS Seatbelt profile that
/// rejects nested `sandbox-exec` invocations.
#[cfg(target_os = "macos")]
fn inherits_macos_sandbox() -> Result<bool> {
    let output = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .output()
        .context("failed to probe the macOS Claude sandbox")?;
    Ok(!output.status.success() && is_nested_sandbox_rejection(&output.stderr))
}

#[cfg(target_os = "macos")]
fn is_nested_sandbox_rejection(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains("sandbox_apply: Operation not permitted")
}

/// Start Claude without a second profile only after [`inherits_macos_sandbox`]
/// established that the parent profile is inherited by this child process.
#[cfg(target_os = "macos")]
fn run_inherited_sandbox(command: &[OsString]) -> Result<()> {
    let (program, args) = command
        .split_first()
        .context("Claude sandbox refused an empty command")?;
    let status = Command::new(program)
        .args(args)
        .status()
        .context("failed to start Claude inside the inherited macOS sandbox")?;
    if !status.success() {
        bail!("Claude inside the inherited macOS sandbox exited with {status}");
    }
    Ok(())
}

fn run_in(
    cwd: &Path,
    _mode: Mode,
    mut extra_writable_roots: Vec<PathBuf>,
    command: Vec<OsString>,
    spawn: impl FnOnce(&Path, &[OsString]) -> Result<ExitStatus>,
) -> Result<()> {
    if command.is_empty() {
        bail!("Claude sandbox refused an empty command");
    }

    // The cwd is the current session worktree in session mode and the project
    // root in root mode. Both are intentional working areas.
    extra_writable_roots.push(cwd.to_path_buf());
    let writable_roots = canonical_roots(extra_writable_roots)?;
    let sandbox = platform_command(&writable_roots, &command)?;
    let status = spawn(&sandbox.program, &sandbox.args)?;
    if !status.success() {
        bail!("sandboxed Claude exited with {status}");
    }
    Ok(())
}

fn canonical_roots(roots: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut canonical = Vec::new();
    for root in roots {
        let resolved = std::fs::canonicalize(&root).with_context(|| {
            format!(
                "Claude sandbox writable root cannot be canonicalized: {}",
                root.display()
            )
        })?;
        if !canonical.contains(&resolved) {
            canonical.push(resolved);
        }
    }
    Ok(canonical)
}

#[cfg(target_os = "macos")]
fn platform_command(writable_roots: &[PathBuf], command: &[OsString]) -> Result<SandboxCommand> {
    let sandbox_exec = PathBuf::from("/usr/bin/sandbox-exec");
    if !sandbox_exec.is_file() {
        bail!("Claude OS sandbox is unavailable: /usr/bin/sandbox-exec was not found");
    }
    let mut allowed = String::new();
    for root in writable_roots {
        let path = root.to_string_lossy();
        let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
        allowed.push_str(&format!(" (subpath \"{escaped}\")"));
    }
    let deny = if allowed.is_empty() {
        "(deny file-write*)".to_string()
    } else {
        format!("(deny file-write* (require-not (require-any{allowed})))")
    };
    let profile = format!("(version 1)\n(allow default)\n{deny}\n");
    let mut args = vec![OsString::from("-p"), OsString::from(profile)];
    args.extend_from_slice(command);
    Ok(SandboxCommand {
        program: sandbox_exec,
        args,
    })
}

#[cfg(target_os = "linux")]
fn platform_command(writable_roots: &[PathBuf], command: &[OsString]) -> Result<SandboxCommand> {
    let mut args: Vec<OsString> = [
        "--die-with-parent",
        "--new-session",
        "--ro-bind",
        "/",
        "/",
        "--dev-bind",
        "/dev",
        "/dev",
        "--proc",
        "/proc",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    for root in writable_roots {
        args.push(OsString::from("--bind"));
        args.push(root.as_os_str().to_owned());
        args.push(root.as_os_str().to_owned());
    }
    args.push(OsString::from("--"));
    args.extend_from_slice(command);
    Ok(SandboxCommand {
        program: PathBuf::from("bwrap"),
        args,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_command(_writable_roots: &[PathBuf], _command: &[OsString]) -> Result<SandboxCommand> {
    bail!("Claude OS sandbox is unsupported on this platform; refusing unprotected launch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn mode_parser_is_fail_closed() {
        assert_eq!(Mode::parse("session").unwrap(), Mode::Session);
        assert_eq!(Mode::parse("root").unwrap(), Mode::Root);
        assert!(Mode::parse("unknown").is_err());
    }

    #[test]
    fn canonical_roots_resolve_symlinks_and_reject_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = temp.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real, &link).unwrap();

        assert_eq!(
            canonical_roots(vec![link]).unwrap(),
            vec![std::fs::canonicalize(real).unwrap()]
        );
        assert!(canonical_roots(vec![temp.path().join("missing")]).is_err());
    }

    #[test]
    fn temporary_roots_include_the_platform_temp_directory() {
        let roots = writable_temp_roots();
        assert!(roots.contains(&std::env::temp_dir()));
        #[cfg(unix)]
        for root in [Path::new("/tmp"), Path::new("/var/tmp")] {
            if root.is_dir() {
                assert!(roots.iter().any(|candidate| candidate == root));
            }
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn root_mode_makes_the_project_cwd_writable() {
        let project = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(project.path()).unwrap();
        let command = vec![OsString::from("claude")];
        run_in(project.path(), Mode::Root, vec![], command, |_, args| {
            assert!(args.iter().any(|arg| arg
                .to_string_lossy()
                .contains(&canonical.to_string_lossy().to_string())));
            Command::new("/usr/bin/true")
                .status()
                .map_err(anyhow::Error::from)
        })
        .unwrap();
    }

    #[test]
    fn empty_command_never_reaches_the_spawner() {
        let temp = tempfile::tempdir().unwrap();
        let called = std::cell::Cell::new(false);
        let result = run_in(temp.path(), Mode::Root, vec![], vec![], |_, _| {
            called.set(true);
            unreachable!()
        });
        assert!(result.is_err());
        assert!(!called.get());
    }

    #[test]
    fn sandbox_spawn_failure_never_falls_back_to_the_raw_command() {
        let temp = tempfile::tempdir().unwrap();
        let error = run_in(
            temp.path(),
            Mode::Session,
            vec![],
            vec![OsString::from("claude")],
            |_, _| Err(anyhow::anyhow!("sandbox unavailable")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("sandbox unavailable"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nested_sandbox_probe_accepts_only_the_known_seatbelt_rejection() {
        // This test documents the exact macOS error that enables the inherited
        // sandbox path. Other failures remain fail-closed in `run_in`.
        assert!(is_nested_sandbox_rejection(
            b"sandbox-exec: sandbox_apply: Operation not permitted\n"
        ));
        assert!(!is_nested_sandbox_rejection(
            b"sandbox-exec: profile invalid\n"
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_roots_cover_the_keychain_db_and_the_mds_cache() {
        let temp = tempfile::tempdir().unwrap();
        let roots = macos_keychain_roots(temp.path()).unwrap();
        assert_eq!(roots[0], temp.path().join("Library/Keychains"));
        // The MDS cache root is the caller's real per-user cache directory: an
        // absolute path that exists, so `canonical_roots` accepts it.
        let cache = &roots[1];
        assert!(cache.is_absolute());
        assert!(cache.is_dir());
        assert!(!cache.as_os_str().to_string_lossy().ends_with('\0'));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn real_sandbox_allows_in_root_write_and_blocks_symlink_escape() {
        #[cfg(target_os = "linux")]
        if Command::new("bwrap").arg("--version").output().is_err() {
            // Production takes the same fail-closed spawn-error path; builders
            // without bubblewrap cannot exercise the kernel boundary itself.
            return;
        }

        #[cfg(target_os = "macos")]
        if !Command::new("/usr/bin/sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .status()
            .is_ok_and(|status| status.success())
        {
            // A parent Seatbelt profile may prohibit creating a nested sandbox.
            // Production fails closed there; an unsandboxed host exercises the
            // sentinel assertions below.
            return;
        }

        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let allowed = temp.path().join("allowed");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, "safe").unwrap();
        let escape = allowed.join("escape");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();

        let allowed_file = allowed.join("created");
        let write_allowed = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(format!("printf ok > '{}'", allowed_file.display())),
        ];
        let plan = platform_command(std::slice::from_ref(&allowed), &write_allowed).unwrap();
        let status = Command::new(plan.program).args(plan.args).status().unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(allowed_file).unwrap(), "ok");

        let escape_command = vec![
            OsString::from("/bin/sh"),
            OsString::from("-c"),
            OsString::from(format!("printf changed > '{}/sentinel'", escape.display())),
        ];
        let plan = platform_command(&[allowed], &escape_command).unwrap();
        let status = Command::new(plan.program).args(plan.args).status().unwrap();
        assert!(!status.success());
        assert_eq!(fs::read_to_string(sentinel).unwrap(), "safe");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_allows_only_canonical_write_roots() {
        let temp = tempfile::tempdir().unwrap();
        let command = platform_command(
            &[temp.path().to_path_buf()],
            &[OsString::from("/usr/bin/true")],
        )
        .unwrap();
        assert_eq!(command.program, Path::new("/usr/bin/sandbox-exec"));
        let profile = command.args[1].to_string_lossy();
        assert!(profile.contains("(deny file-write* (require-not (require-any"));
        assert!(profile.contains(&temp.path().to_string_lossy().to_string()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_plan_mounts_the_host_read_only_then_binds_allow_roots() {
        let root = PathBuf::from("/tmp/allowed");
        let command =
            platform_command(std::slice::from_ref(&root), &[OsString::from("true")]).unwrap();
        assert_eq!(command.program, Path::new("bwrap"));
        assert!(command.args.windows(3).any(|args| {
            args == [
                OsString::from("--bind"),
                root.clone().into(),
                root.clone().into(),
            ]
        }));
        assert!(command.args.windows(3).any(|args| {
            args == [
                OsString::from("--ro-bind"),
                OsString::from("/"),
                OsString::from("/"),
            ]
        }));
    }
}
