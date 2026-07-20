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
    // write usagi's own state. These are explicit application roots, not a
    // broad home-directory exception.
    let home = dirs::home_dir().context("Claude OS sandbox cannot resolve the home directory")?;
    let claude_state = home.join(".claude");
    let usagi_state = home.join(".usagi");
    let sandbox_tmp = usagi_state.join("tmp/claude");
    std::fs::create_dir_all(&claude_state)
        .context("failed to prepare Claude's sandbox state directory")?;
    std::fs::create_dir_all(&sandbox_tmp)
        .context("failed to prepare Claude's sandbox temporary directory")?;
    extra_writable_roots.push(claude_state);
    extra_writable_roots.push(usagi_state);
    run_in(
        &cwd,
        mode,
        extra_writable_roots,
        command,
        |program, args| {
            Command::new(program)
                .args(args)
                .env("TMPDIR", &sandbox_tmp)
                .status()
                .with_context(|| format!("failed to start OS sandbox {}", program.display()))
        },
    )
}

fn run_in(
    cwd: &Path,
    mode: Mode,
    mut extra_writable_roots: Vec<PathBuf>,
    command: Vec<OsString>,
    spawn: impl FnOnce(&Path, &[OsString]) -> Result<ExitStatus>,
) -> Result<()> {
    if command.is_empty() {
        bail!("Claude sandbox refused an empty command");
    }

    if mode == Mode::Session {
        extra_writable_roots.push(cwd.to_path_buf());
    }
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
        let command = platform_command(&[root.clone()], &[OsString::from("true")]).unwrap();
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
