use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Execute one setup command with the platform shell.
///
/// The usecase-owned setup port is adapted to this IO primitive by the
/// composition code. Keeping the primitive trait-free prevents infrastructure
/// from knowing session orchestration.
pub fn run(cwd: &Path, command: &str) -> Result<()> {
    #[cfg(windows)]
    let output_result = Command::new("cmd")
        .args(["/C", command])
        .current_dir(cwd)
        .output();
    #[cfg(not(windows))]
    let output_result = Command::new("sh")
        .args(["-lc", command])
        .current_dir(cwd)
        .output();
    let output =
        output_result.with_context(|| format!("failed to run setup command `{command}`"))?;

    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!(
        "setup command `{command}` exited with {}{}{}",
        output.status,
        if stdout.trim().is_empty() {
            String::new()
        } else {
            format!("\nstdout:\n{}", stdout.trim_end())
        },
        if stderr.trim().is_empty() {
            String::new()
        } else {
            format!("\nstderr:\n{}", stderr.trim_end())
        }
    );
}
