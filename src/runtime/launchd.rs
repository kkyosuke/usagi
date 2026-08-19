//! macOS `LaunchAgent` provisioning for the daemon composition root.
//!
//! launchd only supervises the foreground `daemon serve` process.  The daemon
//! lock remains the single-instance authority, and this module never reads or
//! interprets managed-session state.
//!
//! The plist carries exactly one thing beyond the program to run: the
//! [`DataHome`] pair. launchd starts the agent from its own environment, not
//! from the shell that installed the service, so a plist without that pair
//! makes the supervised daemon re-resolve its data home from an empty
//! environment — landing it on a different directory than the installing
//! process, while the plist's own stderr log path still points at the
//! installing process's selected directory. Forwarding `base` plus the mode
//! spelling is the same contract the daemon already uses for the Agent MCP
//! children it launches. No token or session state is written here.

// The planning and rendering below are pure and tested on every host, but the
// real IO that consumes them exists only on macOS. On other hosts they are
// reached from tests alone, so `dead_code` would fire on a non-test build.
//
// The allowance is file-scoped rather than per-item because rustc's liveness
// propagates: marking only the entry points still leaves everything they call
// unreachable from a live root. The cost is that genuinely dead code added to
// this module is not reported on hosts where the allowance is active — for this
// file that means a Linux build, so a macOS build is what catches it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::{Path, PathBuf};

use usagi_core::infrastructure::paths::{DATA_DIR_ENV, DataHome, RUNTIME_MODE_ENV};

const LABEL: &str = "com.usagi.daemon";

// The real IO exists only where the supervisor does. The pure planning and
// rendering below stay cross-platform so their tests run on every host.
#[cfg(target_os = "macos")]
pub(crate) use real_io::{install, uninstall};

#[cfg(target_os = "macos")]
mod real_io {
    #![coverage(off)]

    use std::process::Command;

    use super::{Path, PathBuf, install_with, uninstall_with};

    pub(crate) fn install(
        executable: &Path,
        data_home: &super::DataHome,
        workspace: &Path,
    ) -> std::io::Result<PathBuf> {
        let path = plist_path()?;
        let mut create_dir_all = create_dir_all;
        let mut write = write_file;
        let mut launch = launchctl;
        install_with(
            executable,
            data_home,
            workspace,
            path,
            &mut create_dir_all,
            &mut write,
            &mut launch,
        )
    }

    pub(crate) fn uninstall() -> std::io::Result<PathBuf> {
        let path = plist_path()?;
        let mut launch = launchctl;
        let mut remove_file = remove_file;
        uninstall_with(path, &Path::exists, &mut launch, &mut remove_file)
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

    fn plist_path() -> std::io::Result<PathBuf> {
        super::plist_path_from_home(dirs::home_dir())
    }

    fn launchctl(verb: &str, plist: &Path) -> std::io::Result<()> {
        let domain = format!("gui/{}", unsafe { libc::geteuid() });
        let status = Command::new("/bin/launchctl")
            .arg(verb)
            .arg(domain)
            .arg(plist)
            .status()?;
        if status.success() {
            return Ok(());
        }
        Err(std::io::Error::other(format!("launchctl {verb} failed")))
    }
}

fn plist_path_from_home(home: Option<PathBuf>) -> std::io::Result<PathBuf> {
    let home =
        home.ok_or_else(|| std::io::Error::other("could not determine the home directory"))?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn install_with(
    executable: &Path,
    data_home: &DataHome,
    workspace: &Path,
    path: PathBuf,
    create_dir_all: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    write: &mut dyn FnMut(&Path, String) -> std::io::Result<()>,
    launch: &mut dyn FnMut(&str, &Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    // The log lives in the selected directory, and the plist announces the pair
    // that resolves back to it. Deriving both from one `DataHome` is what keeps
    // the supervised daemon and its log in the same mode.
    let log = data_home
        .selected()
        .join("logs")
        .join("launchd-daemon.stderr.log");
    let plist = render(executable, &log, data_home, workspace)?;
    create_dir_all(path.parent().expect("LaunchAgents has a parent"))?;
    create_dir_all(log.parent().expect("log path has a parent"))?;
    write(&path, plist)?;
    launch("bootstrap", &path)?;
    Ok(path)
}

fn uninstall_with(
    path: PathBuf,
    exists: &dyn Fn(&Path) -> bool,
    launch: &mut dyn FnMut(&str, &Path) -> std::io::Result<()>,
    remove_file: &mut dyn FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    if exists(&path) {
        // `bootout` may report an unloaded service after a reboot. The plist
        // still must be removed to stop future RunAtLoad supervision.
        let _ = launch("bootout", &path);
        remove_file(&path)?;
    }
    Ok(path)
}

fn render(
    executable: &Path,
    stderr_log: &Path,
    data_home: &DataHome,
    workspace: &Path,
) -> std::io::Result<String> {
    let executable = executable.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "non-UTF-8 executable path",
        )
    })?;
    let stderr_log = stderr_log.to_str().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF-8 log path")
    })?;
    // A base that cannot be spelled as UTF-8 is refused rather than lossily
    // converted: a plist holding a corrupted base would send the supervised
    // daemon to a directory nobody chose.
    let base = data_home.base().to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "non-UTF-8 data home base path",
        )
    })?;
    // launchd starts the agent from `/`, which is not a workspace anyone chose.
    // The daemon binds the workspace its startup directory names, so the
    // directory is pinned here rather than left to the supervisor.
    let workspace = workspace.to_str().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF-8 workspace root")
    })?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>daemon</string><string>serve</string></array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>StandardErrorPath</key><string>{}</string>\n<key>WorkingDirectory</key><string>{}</string>\n<key>EnvironmentVariables</key><dict><key>{DATA_DIR_ENV}</key><string>{}</string><key>{RUNTIME_MODE_ENV}</key><string>{}</string></dict>\n</dict></plist>\n",
        xml_escape(executable),
        xml_escape(stderr_log),
        xml_escape(workspace),
        xml_escape(base),
        data_home.mode().as_env_value()
    ))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::{DataHome, install_with, plist_path_from_home, render, uninstall_with};
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use usagi_core::infrastructure::paths::RuntimeMode;

    const WORKSPACE: &str = "/home/usagi/project";

    fn home() -> DataHome {
        DataHome::new("/home/usagi/.usagi", RuntimeMode::Local)
    }

    #[test]
    fn rendered_agent_supervises_foreground_serve_and_announces_the_data_home() {
        let plist = render(
            Path::new("/Applications/usagi&bin"),
            Path::new("/tmp/daemon.log"),
            &DataHome::new("/home/usagi/.usagi", RuntimeMode::Local),
            Path::new(WORKSPACE),
        )
        .unwrap();
        assert!(plist.contains("<string>/Applications/usagi&amp;bin</string><string>daemon</string><string>serve</string>"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        // The base and the mode spelling travel together. launchd would
        // otherwise start the daemon from an empty environment, resolving a
        // different data home than the log path in this same plist.
        assert!(plist.contains(
            "<key>EnvironmentVariables</key><dict><key>USAGI_HOME</key><string>/home/usagi/.usagi</string><key>USAGI_RUNTIME_MODE</key><string>local</string></dict>"
        ));
    }

    #[test]
    fn the_agent_pins_the_workspace_the_installer_resolved() {
        // launchd starts the agent from `/`. The daemon binds the workspace its
        // startup directory names, so the directory is pinned here; otherwise the
        // supervised daemon owns a workspace nobody chose.
        let plist = render(
            Path::new("/opt/usagi"),
            Path::new("/tmp/log"),
            &home(),
            Path::new(WORKSPACE),
        )
        .unwrap();
        assert!(plist.contains(&format!(
            "<key>WorkingDirectory</key><string>{WORKSPACE}</string>"
        )));

        // The pinned path is escaped like every other value, and a path that
        // cannot be spelled is refused rather than written lossily.
        let escaped = render(
            Path::new("/opt/usagi"),
            Path::new("/tmp/log"),
            &home(),
            Path::new("/home/usagi/a&b/<c>"),
        )
        .unwrap();
        assert!(
            escaped.contains(
                "<key>WorkingDirectory</key><string>/home/usagi/a&amp;b/&lt;c&gt;</string>"
            )
        );
    }

    #[test]
    fn rendered_agent_announces_the_base_itself_for_production() {
        // Production selects the base, so the announced base must stay the base
        // rather than climbing above it.
        let plist = render(
            Path::new("/opt/usagi"),
            Path::new("/home/usagi/.usagi/logs/launchd-daemon.stderr.log"),
            &DataHome::new("/home/usagi/.usagi", RuntimeMode::Production),
            Path::new(WORKSPACE),
        )
        .unwrap();
        assert!(plist.contains(
            "<key>USAGI_HOME</key><string>/home/usagi/.usagi</string><key>USAGI_RUNTIME_MODE</key><string>production</string>"
        ));
    }

    #[test]
    fn rendered_agent_escapes_the_announced_base() {
        let plist = render(
            Path::new("/opt/usagi"),
            Path::new("/tmp/log"),
            &DataHome::new("/home/usagi&co/<data>", RuntimeMode::Development),
            Path::new(WORKSPACE),
        )
        .unwrap();
        assert!(plist.contains(
            "<key>USAGI_HOME</key><string>/home/usagi&amp;co/&lt;data&gt;</string><key>USAGI_RUNTIME_MODE</key><string>development</string>"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rendered_agent_rejects_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;

        let invalid = Path::new(std::ffi::OsStr::from_bytes(&[0xff]));
        let home = DataHome::new("/home/usagi/.usagi", RuntimeMode::Local);
        assert_eq!(
            render(invalid, Path::new("/tmp/log"), &home, Path::new(WORKSPACE))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            render(
                Path::new("/opt/usagi"),
                invalid,
                &home,
                Path::new(WORKSPACE)
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
        // A base that cannot be spelled is refused rather than written lossily.
        assert_eq!(
            render(
                Path::new("/opt/usagi"),
                Path::new("/tmp/log"),
                &DataHome::new(invalid, RuntimeMode::Local),
                Path::new(WORKSPACE),
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
        // Likewise the pinned workspace: an unspellable directory would send the
        // supervised daemon somewhere nobody chose.
        assert_eq!(
            render(
                Path::new("/opt/usagi"),
                Path::new("/tmp/log"),
                &home,
                invalid
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn plist_path_is_derived_from_an_injected_home() {
        assert_eq!(
            plist_path_from_home(Some(PathBuf::from("/Users/usagi"))).unwrap(),
            PathBuf::from("/Users/usagi/Library/LaunchAgents/com.usagi.daemon.plist")
        );
        assert_eq!(
            plist_path_from_home(None).unwrap_err().kind(),
            std::io::ErrorKind::Other
        );
    }

    #[test]
    fn install_plan_creates_paths_writes_plist_and_propagates_launch_failure() {
        let created = RefCell::new(Vec::new());
        let written = RefCell::new(None);
        let launched = RefCell::new(Vec::new());
        let path = PathBuf::from("/home/usagi/Library/LaunchAgents/com.usagi.daemon.plist");
        let mut create = |directory: &Path| {
            created.borrow_mut().push(directory.to_owned());
            Ok(())
        };
        let mut write = |destination: &Path, contents: String| {
            *written.borrow_mut() = Some((destination.to_owned(), contents));
            Ok(())
        };
        let mut launch = |verb: &str, target: &Path| {
            launched
                .borrow_mut()
                .push((verb.to_owned(), target.to_owned()));
            Ok(())
        };
        let result = install_with(
            Path::new("/opt/usagi&friends/usagi"),
            &DataHome::new("/data", RuntimeMode::Local),
            Path::new(WORKSPACE),
            path.clone(),
            &mut create,
            &mut write,
            &mut launch,
        )
        .unwrap();
        assert_eq!(result, path);
        // The log directory is the *selected* directory, so local mode puts it
        // below `local/` while the plist announces the base plus that mode.
        assert_eq!(
            created.into_inner(),
            [
                PathBuf::from("/home/usagi/Library/LaunchAgents"),
                PathBuf::from("/data/local/logs")
            ]
        );
        let (destination, contents) = written.into_inner().unwrap();
        assert_eq!(destination, path);
        assert!(contents.contains("/opt/usagi&amp;friends/usagi"));
        assert!(contents.contains("<string>/data/local/logs/launchd-daemon.stderr.log</string>"));
        assert!(contents.contains(
            "<key>USAGI_HOME</key><string>/data</string><key>USAGI_RUNTIME_MODE</key><string>local</string>"
        ));
        assert_eq!(launched.into_inner(), [("bootstrap".into(), path.clone())]);

        let mut create = |_: &Path| Ok(());
        let mut write = |_: &Path, _: String| Ok(());
        let mut launch = |_: &str, _: &Path| Err(std::io::Error::other("launchctl failed"));
        let error = install_with(
            Path::new("/opt/usagi"),
            &DataHome::new("/data", RuntimeMode::Local),
            Path::new(WORKSPACE),
            path,
            &mut create,
            &mut write,
            &mut launch,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn uninstall_plan_skips_absent_plist_and_removes_present_plist() {
        let path = PathBuf::from("/home/usagi/Library/LaunchAgents/com.usagi.daemon.plist");
        let calls = RefCell::new(Vec::new());
        let present = std::cell::Cell::new(false);
        let exists = |_: &Path| present.get();
        let mut launch = |verb: &str, target: &Path| {
            calls
                .borrow_mut()
                .push((verb.to_owned(), target.to_owned()));
            Err(std::io::Error::other("already unloaded"))
        };
        let mut remove = |target: &Path| {
            calls
                .borrow_mut()
                .push(("remove".into(), target.to_owned()));
            Ok(())
        };
        assert_eq!(
            uninstall_with(path.clone(), &exists, &mut launch, &mut remove).unwrap(),
            path
        );
        assert!(calls.borrow().is_empty());

        present.set(true);
        let result = uninstall_with(path.clone(), &exists, &mut launch, &mut remove).unwrap();
        assert_eq!(result, path);
        assert_eq!(
            calls.into_inner(),
            [("bootout".into(), path.clone()), ("remove".into(), path)]
        );
    }
}
