//! Linux systemd **user** unit provisioning for the daemon composition root.
//!
//! This is the Linux counterpart of [`super::launchd`] and keeps the same shape:
//! systemd only supervises the foreground `daemon serve` process, the daemon
//! lock remains the single-instance authority, and this module never reads or
//! interprets managed-session state.
//!
//! A **user** unit (not a system unit) is what matches usagi's model: the daemon
//! owns PTYs and Agent children for one user's session and resolves its data home
//! below that user's home directory. A system unit would run as root and resolve a
//! different data home entirely.
//!
//! Like the plist, the unit carries the [`DataHome`] pair and nothing else.
//! systemd starts the service from its own environment rather than from the shell
//! that installed it, so without the pair the supervised daemon would re-resolve
//! its data home from an empty environment and land somewhere the installing
//! process never chose. No token or session state is written here.
//!
//! # Difference from launchd
//!
//! The `LaunchAgent` uses unconditional `KeepAlive`, which restarts the daemon even
//! after a deliberate `usagi daemon stop`. This unit uses `Restart=on-failure`
//! instead, so a graceful stop stays stopped while a crash is still recovered.
//! `usagi daemon stop` is the documented way to stop the daemon, and supervision
//! must not undo it.

// The planning and rendering below are pure and tested on every host, but the
// real IO that consumes them exists only on Linux. On other hosts they are
// reached from tests alone, so `dead_code` would fire on a non-test build.
//
// The allowance is file-scoped rather than per-item because rustc's liveness
// propagates: marking only the entry points still leaves everything they call
// unreachable from a live root. The cost is that genuinely dead code added to
// this module is not reported on hosts where the allowance is active — for this
// file that means a macOS build, so a Linux build is what catches it.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

use usagi_core::infrastructure::paths::{DATA_DIR_ENV, DataHome, RUNTIME_MODE_ENV};

/// The unit's file name. `systemctl --user` addresses the service by this name.
const UNIT: &str = "usagi-daemon.service";

// The real IO exists only where the supervisor does. The pure planning and
// rendering below stay cross-platform so their tests run on every host.
#[cfg(target_os = "linux")]
pub(crate) use real_io::{install, uninstall};

#[cfg(target_os = "linux")]
mod real_io {
    #![coverage(off)]

    use std::process::Command;

    use super::{Path, PathBuf, install_with, uninstall_with};

    pub(crate) fn install(
        executable: &Path,
        data_home: &super::DataHome,
        workspace: &Path,
    ) -> std::io::Result<PathBuf> {
        let path = unit_path()?;
        let mut create_dir_all = create_dir_all;
        let mut write = write_file;
        let mut run = systemctl;
        install_with(
            executable,
            data_home,
            workspace,
            path,
            &mut create_dir_all,
            &mut write,
            &mut run,
        )
    }

    pub(crate) fn uninstall() -> std::io::Result<PathBuf> {
        let path = unit_path()?;
        let mut run = systemctl;
        let mut remove_file = remove_file;
        uninstall_with(path, &Path::exists, &mut run, &mut remove_file)
    }

    fn create_dir_all(path: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn write_file(path: &Path, contents: String) -> std::io::Result<()> {
        std::fs::write(path, contents)
    }

    fn remove_file(path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }

    fn unit_path() -> std::io::Result<PathBuf> {
        super::unit_path_from_config_dir(dirs::config_dir())
    }

    fn systemctl(args: &[&str]) -> std::io::Result<()> {
        let status = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .status()?;
        if status.success() {
            return Ok(());
        }
        Err(std::io::Error::other(format!(
            "systemctl --user {} failed",
            args.join(" ")
        )))
    }
}

/// Resolve `<config dir>/systemd/user/usagi-daemon.service`.
///
/// The config directory is injected so the caller decides how it is discovered;
/// production passes `dirs::config_dir()`, which honours `$XDG_CONFIG_HOME` and
/// otherwise uses `~/.config`.
fn unit_path_from_config_dir(config_dir: Option<PathBuf>) -> std::io::Result<PathBuf> {
    let config_dir = config_dir
        .ok_or_else(|| std::io::Error::other("could not determine the config directory"))?;
    Ok(config_dir.join("systemd/user").join(UNIT))
}

fn install_with(
    executable: &Path,
    data_home: &DataHome,
    workspace: &Path,
    path: PathBuf,
    create_dir_all: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    write: &mut dyn FnMut(&Path, String) -> std::io::Result<()>,
    run: &mut dyn FnMut(&[&str]) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    // The log lives in the selected directory and the unit announces the pair
    // that resolves back to it, so the supervised daemon and its log cannot end
    // up in different modes.
    let log = data_home
        .selected()
        .join("logs")
        .join("systemd-daemon.stderr.log");
    let unit = render(executable, &log, data_home, workspace)?;
    create_dir_all(path.parent().expect("the unit directory has a parent"))?;
    create_dir_all(log.parent().expect("log path has a parent"))?;
    write(&path, unit)?;
    // A freshly written unit is invisible until systemd re-reads its directories.
    run(&["daemon-reload"])?;
    run(&["enable", "--now", UNIT])?;
    Ok(path)
}

fn uninstall_with(
    path: PathBuf,
    exists: &dyn Fn(&Path) -> bool,
    run: &mut dyn FnMut(&[&str]) -> std::io::Result<()>,
    remove_file: &mut dyn FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    if exists(&path) {
        // `disable --now` may report an already-inactive unit. The file still
        // must be removed to stop future activation, exactly as the LaunchAgent
        // path tolerates a failed `bootout`.
        let _ = run(&["disable", "--now", UNIT]);
        remove_file(&path)?;
        // Let systemd forget the removed unit; a stale entry would otherwise
        // linger until the next reload.
        let _ = run(&["daemon-reload"]);
    }
    Ok(path)
}

fn render(
    executable: &Path,
    stderr_log: &Path,
    data_home: &DataHome,
    workspace: &Path,
) -> std::io::Result<String> {
    let executable = utf8(executable, "non-UTF-8 executable path")?;
    let stderr_log = utf8(stderr_log, "non-UTF-8 log path")?;
    // A base that cannot be spelled as UTF-8 is refused rather than lossily
    // converted: a unit holding a corrupted base would send the supervised
    // daemon to a directory nobody chose.
    let base = utf8(data_home.base(), "non-UTF-8 data home base path")?;
    // The daemon binds the workspace its startup directory names. systemd's
    // default for a user unit is the user's home, which is not a workspace
    // anyone chose — and under the default `~/.usagi` data home it collides the
    // workspace fence with the single-instance lock.
    let workspace = utf8(workspace, "non-UTF-8 workspace root")?;
    Ok(format!(
        "[Unit]\n\
         Description=usagi daemon\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={}\n\
         ExecStart={} daemon serve\n\
         Restart=on-failure\n\
         RestartSec=1\n\
         Environment={}\n\
         Environment={}\n\
         StandardError=append:{}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        specifier_safe(workspace),
        quoted(executable),
        assignment(DATA_DIR_ENV, base),
        assignment(RUNTIME_MODE_ENV, data_home.mode().as_env_value()),
        specifier_safe(stderr_log)
    ))
}

fn utf8<'a>(path: &'a Path, message: &str) -> std::io::Result<&'a str> {
    path.to_str()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, message.to_owned()))
}

/// Escape a value systemd reads literally to end of line
/// (`WorkingDirectory=`, `StandardError=append:`).
///
/// `%` starts a unit specifier, so a literal one has to be doubled or systemd
/// expands it into something else (or fails to parse the unit). No quoting is
/// applied: these fields take the rest of the line as the value, so a space is
/// already safe and a quote would become part of the path.
fn specifier_safe(value: &str) -> String {
    value.replace('%', "%%")
}

/// Render one `Environment=` assignment, quoting the **whole** `NAME=value`.
///
/// `systemd.exec(5)` documents the quoting this way — `Environment="VAR=word1
/// word2"` — rather than quoting the value alone. Following the documented form
/// keeps the unit valid on every systemd instead of relying on the parser
/// accepting a quote that starts mid-word.
fn assignment(name: &str, value: &str) -> String {
    quoted(&format!("{name}={value}"))
}

/// Quote a value that systemd splits like a shell word: `ExecStart` and an
/// `Environment=` assignment.
///
/// Without quoting, a path containing a space would be read as two arguments —
/// which is how a service silently starts with the wrong `argv`. Inside double
/// quotes systemd still processes `\` escapes, so both `\` and `"` are escaped
/// before the specifier pass.
///
/// **Not every field takes quotes.** `WorkingDirectory=` and
/// `StandardError=append:` are read literally to end of line, so a quote there
/// becomes part of the path: systemd rejects `WorkingDirectory="/tmp/x"` with
/// "path is not absolute". Those use [`specifier_safe`] instead. The distinction
/// is only observable against a real systemd, which is why
/// `systemd_accepts_the_rendered_unit` exists.
fn quoted(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", specifier_safe(&escaped))
}

#[cfg(test)]
mod tests {
    use super::{
        DataHome, UNIT, install_with, quoted, render, specifier_safe, uninstall_with,
        unit_path_from_config_dir,
    };
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use usagi_core::infrastructure::paths::RuntimeMode;

    fn home() -> DataHome {
        DataHome::new("/home/usagi/.usagi", RuntimeMode::Production)
    }

    const WORKSPACE: &str = "/home/usagi/project";

    #[test]
    fn rendered_unit_supervises_serve_and_announces_the_data_home() {
        let unit = render(
            Path::new("/home/usagi/.usagi/bin/usagi"),
            Path::new("/home/usagi/.usagi/logs/systemd-daemon.stderr.log"),
            &home(),
            Path::new(WORKSPACE),
        )
        .unwrap();
        assert!(unit.contains("ExecStart=\"/home/usagi/.usagi/bin/usagi\" daemon serve"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("Type=simple"));
        // The base and the mode spelling travel together, exactly as the plist
        // carries them, so the supervised daemon resolves the installing
        // process's data home instead of one from an empty environment.
        assert!(unit.contains("Environment=\"USAGI_HOME=/home/usagi/.usagi\""));
        assert!(unit.contains("Environment=\"USAGI_RUNTIME_MODE=production\""));
        assert!(
            unit.contains("StandardError=append:/home/usagi/.usagi/logs/systemd-daemon.stderr.log")
        );
    }

    #[test]
    fn the_unit_pins_the_workspace_the_installer_resolved() {
        // systemd starts a user unit in the user's home. The daemon binds the
        // workspace its startup directory names, so without this pin the
        // supervised daemon would own a workspace nobody chose — and under the
        // default `~/.usagi` data home the workspace fence and the
        // single-instance lock become the same file, which the daemon reports as
        // "already running" and never recovers from.
        let unit = render(
            Path::new("/opt/usagi"),
            Path::new("/tmp/log"),
            &home(),
            Path::new(WORKSPACE),
        )
        .unwrap();
        assert!(unit.contains(&format!("WorkingDirectory={WORKSPACE}\n")));
    }

    #[test]
    fn the_pinned_workspace_is_quoted_and_escaped_and_must_be_spellable() {
        let unit = render(
            Path::new("/opt/usagi"),
            Path::new("/tmp/log"),
            &home(),
            Path::new("/home/usagi/my project/100%"),
        )
        .unwrap();
        assert!(unit.contains("WorkingDirectory=/home/usagi/my project/100%%\n"));

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let invalid = Path::new(std::ffi::OsStr::from_bytes(&[0xff]));
            assert_eq!(
                render(
                    Path::new("/opt/usagi"),
                    Path::new("/tmp/log"),
                    &home(),
                    invalid
                )
                .unwrap_err()
                .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
    }

    /// Ask the real systemd whether the rendered unit is valid.
    ///
    /// Every other test here compares the output against strings this module
    /// itself produced, so a directive that systemd rejects — a quoting form it
    /// does not accept, a key it does not know — would pass all of them. This is
    /// the only check that can fail for that reason. Paths that exist are used
    /// because `systemd-analyze verify` also resolves `ExecStart` and
    /// `WorkingDirectory`.
    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_accepts_the_rendered_unit() {
        use std::process::Command;

        // Deliberately not skipped when the tool is missing. A silent skip would
        // read as a pass on exactly the hosts that cannot check the one thing
        // this test exists for, and the run that matters — CI on Linux — always
        // has systemd. A missing tool is reported as a failure with what to
        // install.
        let dir = tempfile::tempdir().expect("temp dir");
        let unit_path = dir.path().join(UNIT);
        let unit = render(
            Path::new("/bin/sh"),
            &dir.path().join("daemon.log"),
            &DataHome::new(dir.path(), RuntimeMode::Local),
            dir.path(),
        )
        .unwrap();
        std::fs::write(&unit_path, &unit).expect("write unit");

        let output = Command::new("systemd-analyze")
            .arg("verify")
            .arg(&unit_path)
            .output()
            .expect("systemd-analyze is required on Linux to validate the unit");
        // Both streams are decoded before the assertion so the failure message
        // costs no branch of its own: an argument evaluated only on failure is a
        // line no passing run executes.
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "systemd rejected the unit:\n{unit}\nstdout: {stdout}\nstderr: {stderr}"
        );
    }

    #[test]
    fn a_graceful_stop_is_not_undone_by_supervision() {
        // `Restart=always` would restart the daemon after `usagi daemon stop`,
        // which is the documented way to stop it. Only failures are recovered.
        let unit = render(
            Path::new("/opt/usagi"),
            Path::new("/tmp/log"),
            &home(),
            Path::new(WORKSPACE),
        )
        .unwrap();
        assert!(unit.contains("Restart=on-failure"));
        assert!(!unit.contains("Restart=always"));
    }

    #[test]
    fn rendered_unit_announces_each_mode_and_its_selected_log() {
        for (mode, spelling) in [
            (RuntimeMode::Production, "production"),
            (RuntimeMode::Development, "development"),
            (RuntimeMode::Local, "local"),
        ] {
            let unit = render(
                Path::new("/opt/usagi"),
                Path::new("/tmp/log"),
                &DataHome::new("/data", mode),
                Path::new(WORKSPACE),
            )
            .unwrap();
            assert!(
                unit.contains(&format!("Environment=\"USAGI_RUNTIME_MODE={spelling}\"")),
                "{spelling}"
            );
            // The base stays the base for every mode, including production
            // where the selected directory *is* the base.
            assert!(
                unit.contains("Environment=\"USAGI_HOME=/data\""),
                "{spelling}"
            );
        }
    }

    #[test]
    fn shell_split_values_are_quoted_and_specifiers_are_escaped() {
        // A space would otherwise split ExecStart into two arguments, and a bare
        // `%` would be expanded by systemd as a specifier.
        assert_eq!(quoted("/opt/my usagi/bin"), "\"/opt/my usagi/bin\"");
        assert_eq!(quoted("/opt/100%/usagi"), "\"/opt/100%%/usagi\"");
        assert_eq!(quoted("/opt/a\\b\"c"), "\"/opt/a\\\\b\\\"c\"");
        assert_eq!(specifier_safe("/var/log/50%"), "/var/log/50%%");
        assert_eq!(specifier_safe("/var/log/plain"), "/var/log/plain");

        let unit = render(
            Path::new("/opt/my usagi/usagi"),
            Path::new("/var/log/50%/daemon.log"),
            &DataHome::new("/data dir/100%", RuntimeMode::Local),
            Path::new(WORKSPACE),
        )
        .unwrap();
        assert!(unit.contains("ExecStart=\"/opt/my usagi/usagi\" daemon serve"));
        assert!(unit.contains("Environment=\"USAGI_HOME=/data dir/100%%\""));
        assert!(unit.contains("StandardError=append:/var/log/50%%/daemon.log"));
    }

    #[cfg(unix)]
    #[test]
    fn rendered_unit_rejects_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;

        let invalid = Path::new(std::ffi::OsStr::from_bytes(&[0xff]));
        assert_eq!(
            render(
                invalid,
                Path::new("/tmp/log"),
                &home(),
                Path::new(WORKSPACE)
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            render(
                Path::new("/opt/usagi"),
                invalid,
                &home(),
                Path::new(WORKSPACE)
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            render(
                Path::new("/opt/usagi"),
                Path::new("/tmp/log"),
                &DataHome::new(invalid, RuntimeMode::Local),
                Path::new(WORKSPACE)
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn unit_path_is_derived_from_an_injected_config_dir() {
        assert_eq!(
            unit_path_from_config_dir(Some(PathBuf::from("/home/usagi/.config"))).unwrap(),
            PathBuf::from("/home/usagi/.config/systemd/user/usagi-daemon.service")
        );
        assert_eq!(
            unit_path_from_config_dir(None).unwrap_err().kind(),
            std::io::ErrorKind::Other
        );
    }

    #[test]
    fn install_plan_writes_the_unit_then_reloads_and_enables_it() {
        let created = RefCell::new(Vec::new());
        let written = RefCell::new(None);
        let ran = RefCell::new(Vec::new());
        let path = PathBuf::from("/home/usagi/.config/systemd/user/usagi-daemon.service");
        let mut create = |directory: &Path| {
            created.borrow_mut().push(directory.to_owned());
            Ok(())
        };
        let mut write = |destination: &Path, contents: String| {
            *written.borrow_mut() = Some((destination.to_owned(), contents));
            Ok(())
        };
        let mut run = |args: &[&str]| {
            ran.borrow_mut().push(args.join(" "));
            Ok(())
        };
        let result = install_with(
            Path::new("/opt/usagi/usagi"),
            &DataHome::new("/data", RuntimeMode::Local),
            Path::new(WORKSPACE),
            path.clone(),
            &mut create,
            &mut write,
            &mut run,
        )
        .unwrap();
        assert_eq!(result, path);
        // The log directory is the *selected* directory, so local mode puts it
        // below `local/` while the unit announces the base plus that mode.
        assert_eq!(
            created.into_inner(),
            [
                PathBuf::from("/home/usagi/.config/systemd/user"),
                PathBuf::from("/data/local/logs")
            ]
        );
        let (destination, contents) = written.into_inner().unwrap();
        assert_eq!(destination, path);
        assert!(contents.contains("append:/data/local/logs/systemd-daemon.stderr.log"));
        assert!(contents.contains("Environment=\"USAGI_HOME=/data\""));
        assert!(contents.contains("Environment=\"USAGI_RUNTIME_MODE=local\""));
        // Reload must precede enable, or systemd enables a unit it has not read.
        assert_eq!(
            ran.into_inner(),
            ["daemon-reload".to_owned(), format!("enable --now {UNIT}")]
        );
    }

    #[test]
    fn install_plan_propagates_every_step_failure() {
        // One set of seams, switched per scenario, rather than a fresh set for
        // each. A per-scenario set leaves the seams *after* the failing step
        // never invoked, and an uninvoked closure is an uncovered function.
        let path = PathBuf::from("/home/usagi/.config/systemd/user/usagi-daemon.service");
        let failing_step = std::cell::Cell::new("");
        let fail_if = |step: &str| {
            if failing_step.get() == step {
                Err(std::io::Error::other(format!("{step} failed")))
            } else {
                Ok(())
            }
        };
        let mut create = |_: &Path| fail_if("mkdir");
        let mut write = |_: &Path, _: String| fail_if("write");
        let mut run = |_: &[&str]| fail_if("systemctl");

        for step in ["mkdir", "write", "systemctl"] {
            failing_step.set(step);
            assert_eq!(
                install_with(
                    Path::new("/opt/usagi"),
                    &DataHome::new("/data", RuntimeMode::Local),
                    Path::new(WORKSPACE),
                    path.clone(),
                    &mut create,
                    &mut write,
                    &mut run,
                )
                .unwrap_err()
                .kind(),
                std::io::ErrorKind::Other,
                "{step}"
            );
        }

        // With no step failing, the same seams complete the plan — which is what
        // proves the loop above failed for the injected reason and not because
        // the seams were broken.
        failing_step.set("");
        assert_eq!(
            install_with(
                Path::new("/opt/usagi"),
                &DataHome::new("/data", RuntimeMode::Local),
                Path::new(WORKSPACE),
                path.clone(),
                &mut create,
                &mut write,
                &mut run,
            )
            .unwrap(),
            path
        );
    }

    #[test]
    fn uninstall_plan_skips_an_absent_unit_and_removes_a_present_one() {
        let path = PathBuf::from("/home/usagi/.config/systemd/user/usagi-daemon.service");
        let calls = RefCell::new(Vec::new());
        let present = std::cell::Cell::new(false);
        let exists = |_: &Path| present.get();
        let mut run = |args: &[&str]| {
            calls.borrow_mut().push(args.join(" "));
            Err(std::io::Error::other("already inactive"))
        };
        let mut remove = |target: &Path| {
            calls
                .borrow_mut()
                .push(format!("remove {}", target.display()));
            Ok(())
        };
        assert_eq!(
            uninstall_with(path.clone(), &exists, &mut run, &mut remove).unwrap(),
            path
        );
        assert!(calls.borrow().is_empty());

        // A failing `disable` is tolerated, but the file removal is not: without
        // it the unit would activate again at the next login.
        present.set(true);
        let result = uninstall_with(path.clone(), &exists, &mut run, &mut remove).unwrap();
        assert_eq!(result, path);
        assert_eq!(
            calls.into_inner(),
            [
                format!("disable --now {UNIT}"),
                format!("remove {}", path.display()),
                "daemon-reload".to_owned()
            ]
        );
    }

    #[test]
    fn uninstall_plan_propagates_a_removal_failure() {
        let path = PathBuf::from("/home/usagi/.config/systemd/user/usagi-daemon.service");
        let exists = |_: &Path| true;
        let mut run = |_: &[&str]| Ok(());
        let mut remove = |_: &Path| Err(std::io::Error::other("permission denied"));
        assert_eq!(
            uninstall_with(path, &exists, &mut run, &mut remove)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Other
        );
    }
}
