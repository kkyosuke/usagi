//! macOS `LaunchAgent` provisioning for the daemon composition root.
//!
//! launchd only supervises the foreground `daemon serve` process.  The daemon
//! lock remains the single-instance authority, and this module never reads or
//! interprets managed-session state.

use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

const LABEL: &str = "com.usagi.daemon";

#[coverage(off)] // Real per-user LaunchAgent filesystem and launchctl boundary.
pub(crate) fn install(executable: &Path, data_dir: &Path) -> std::io::Result<PathBuf> {
    let path = plist_path()?;
    let log = data_dir.join("logs").join("launchd-daemon.stderr.log");
    let plist = render(executable, &log)?;
    std::fs::create_dir_all(path.parent().expect("LaunchAgents has a parent"))?;
    std::fs::create_dir_all(log.parent().expect("log path has a parent"))?;
    std::fs::write(&path, plist)?;
    launchctl("bootstrap", &path)?;
    Ok(path)
}

#[coverage(off)] // Real per-user LaunchAgent filesystem and launchctl boundary.
pub(crate) fn uninstall() -> std::io::Result<PathBuf> {
    let path = plist_path()?;
    if path.exists() {
        // `bootout` may report an unloaded service after a reboot.  The plist
        // still must be removed to stop future RunAtLoad supervision.
        let _ = launchctl("bootout", &path);
        std::fs::remove_file(&path)?;
    }
    Ok(path)
}

#[coverage(off)] // Reads the real user's home directory.
fn plist_path() -> std::io::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| std::io::Error::other("could not determine the home directory"))?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[coverage(off)] // Executes the platform service manager.
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
    use super::render;
    use std::path::Path;

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
}
