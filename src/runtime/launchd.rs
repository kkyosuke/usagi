//! macOS `LaunchAgent` provisioning for the daemon composition root.
//!
//! launchd only supervises the foreground `daemon serve` process.  The daemon
//! lock remains the single-instance authority, and this module never reads or
//! interprets managed-session state.

use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

const LABEL: &str = "com.usagi.daemon";

pub(crate) use real_io::{install, uninstall};

mod real_io {
    #![coverage(off)]

    #[cfg(target_os = "macos")]
    use super::Command;
    use super::{Path, PathBuf, install_with, uninstall_with};

    pub(crate) fn install(executable: &Path, data_dir: &Path) -> std::io::Result<PathBuf> {
        let path = plist_path()?;
        let mut create_dir_all = create_dir_all;
        let mut write = write_file;
        let mut launch = launchctl;
        install_with(
            executable,
            data_dir,
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
        #[cfg(target_os = "macos")]
        {
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
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (verb, plist);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "launchd supervision is only supported on macOS",
            ))
        }
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
    data_dir: &Path,
    path: PathBuf,
    create_dir_all: &mut dyn FnMut(&Path) -> std::io::Result<()>,
    write: &mut dyn FnMut(&Path, String) -> std::io::Result<()>,
    launch: &mut dyn FnMut(&str, &Path) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    let log = data_dir.join("logs").join("launchd-daemon.stderr.log");
    let plist = render(executable, &log)?;
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

fn render(executable: &Path, stderr_log: &Path) -> std::io::Result<String> {
    let executable = executable.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "non-UTF-8 executable path",
        )
    })?;
    let stderr_log = stderr_log.to_str().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF-8 log path")
    })?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>daemon</string><string>serve</string></array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>StandardErrorPath</key><string>{}</string>\n</dict></plist>\n",
        xml_escape(executable),
        xml_escape(stderr_log)
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
    use super::{install_with, plist_path_from_home, render, uninstall_with};
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    #[test]
    fn rendered_agent_supervises_foreground_serve_without_environment() {
        let plist = render(
            Path::new("/Applications/usagi&bin"),
            Path::new("/tmp/daemon.log"),
        )
        .unwrap();
        assert!(plist.contains("<string>/Applications/usagi&amp;bin</string><string>daemon</string><string>serve</string>"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(!plist.contains("EnvironmentVariables"));
    }

    #[cfg(unix)]
    #[test]
    fn rendered_agent_rejects_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;

        let invalid = Path::new(std::ffi::OsStr::from_bytes(&[0xff]));
        assert_eq!(
            render(invalid, Path::new("/tmp/log")).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            render(Path::new("/opt/usagi"), invalid).unwrap_err().kind(),
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
            Path::new("/data"),
            path.clone(),
            &mut create,
            &mut write,
            &mut launch,
        )
        .unwrap();
        assert_eq!(result, path);
        assert_eq!(
            created.into_inner(),
            [
                PathBuf::from("/home/usagi/Library/LaunchAgents"),
                PathBuf::from("/data/logs")
            ]
        );
        let (destination, contents) = written.into_inner().unwrap();
        assert_eq!(destination, path);
        assert!(contents.contains("/opt/usagi&amp;friends/usagi"));
        assert_eq!(launched.into_inner(), [("bootstrap".into(), path.clone())]);

        let mut create = |_: &Path| Ok(());
        let mut write = |_: &Path, _: String| Ok(());
        let mut launch = |_: &str, _: &Path| Err(std::io::Error::other("launchctl failed"));
        let error = install_with(
            Path::new("/opt/usagi"),
            Path::new("/data"),
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
