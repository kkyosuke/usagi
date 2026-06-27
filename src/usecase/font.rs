//! Nerd Font provisioning for `usagi doctor --fix`.
//!
//! usagi's TUI paints git lifecycle and issue-graph glyphs with a Nerd Font (it
//! falls back to colored words when one is absent, so the font is *optional*).
//! This module decides whether a Nerd Font is already installed and, on request,
//! downloads one into the user's font directory.
//!
//! Like the local-LLM provisioning, the remedy is not a package-manager install:
//! it downloads a release archive with `curl` and unpacks it with `unzip`, both
//! driven through the shared [`CommandRunner`] abstraction so the logic is tested
//! with a fake runner and never shells out. The filesystem work (locating font
//! directories, scanning them, creating the destination) is exercised against
//! temporary directories.

use std::path::{Path, PathBuf};

use crate::usecase::doctor::CommandRunner;

/// The Nerd Font usagi installs: JetBrainsMono, a popular monospaced font.
const FONT_NAME: &str = "JetBrainsMono";

/// Where the font archive is downloaded from. The `latest` redirect tracks the
/// newest Nerd Fonts release, so no version needs to be pinned here.
const DOWNLOAD_URL: &str =
    "https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip";

/// Filename of the downloaded archive in the system temp directory.
const ARCHIVE_NAME: &str = "usagi-JetBrainsMono-nerd-font.zip";

/// The URL the font archive is downloaded from.
pub fn download_url() -> &'static str {
    DOWNLOAD_URL
}

/// Manual install guidance, surfaced whenever the automatic flow cannot run.
pub fn font_manual() -> String {
    "install a Nerd Font manually from https://github.com/ryanoasis/nerd-fonts/releases".to_string()
}

/// The font directories for `os`, with `home` as the user's home directory.
///
/// The first entry is also the install destination. Returns an empty list on
/// platforms with no known user font directory (e.g. Windows), which the caller
/// treats as "no automatic install path".
pub fn font_dirs(os: &str, home: &Path) -> Vec<PathBuf> {
    match os {
        "macos" => vec![home.join("Library").join("Fonts")],
        "linux" => vec![
            home.join(".local").join("share").join("fonts"),
            home.join(".fonts"),
        ],
        _ => Vec::new(),
    }
}

/// Whether any Nerd Font is already installed in `dirs`.
pub fn nerd_font_installed(dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|dir| dir_has_nerd_font(dir))
}

/// Whether `dir` directly contains a Nerd Font file. A missing or unreadable
/// directory simply counts as "no font" rather than an error.
fn dir_has_nerd_font(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| is_nerd_font_file(&entry.file_name().to_string_lossy()))
}

/// Whether `name` looks like a Nerd Font file: a `.ttf`/`.otf` whose name
/// carries the Nerd Font marker (e.g. `JetBrainsMonoNerdFont-Regular.ttf`).
fn is_nerd_font_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    (lower.ends_with(".ttf") || lower.ends_with(".otf"))
        && (lower.contains("nerdfont") || lower.contains("nerd font"))
}

/// One thing [`ensure`] did (or found already done) while provisioning a font.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontStep {
    /// A Nerd Font was already installed; nothing was downloaded.
    AlreadyPresent,
    /// The font was downloaded and installed into `dir` during this run.
    Installed { font: &'static str, dir: String },
}

/// Why [`ensure`] could not install a font.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontError {
    /// No known user font directory on this OS; only manual steps are offered.
    Unsupported { manual: String },
    /// A tool needed to download or unpack the font is missing.
    ToolMissing { tool: &'static str, manual: String },
    /// The destination font directory could not be created.
    DirCreateFailed { dir: String, manual: String },
    /// Downloading the font archive failed.
    DownloadFailed { manual: String },
    /// Unpacking the font archive failed.
    ExtractFailed { manual: String },
}

/// Ensure a Nerd Font is installed, downloading one into the first of `dirs` if
/// none is present. Idempotent: a font already in any of `dirs` reports
/// [`FontStep::AlreadyPresent`] without downloading.
///
/// The download (`curl`) and extraction (`unzip`) run through `runner`; on Linux
/// the font cache is refreshed with `fc-cache` afterwards (best-effort). `os`
/// selects that platform-specific step.
pub fn ensure(
    os: &str,
    runner: &dyn CommandRunner,
    dirs: &[PathBuf],
) -> Result<FontStep, FontError> {
    if nerd_font_installed(dirs) {
        return Ok(FontStep::AlreadyPresent);
    }
    let Some(dest) = dirs.first() else {
        return Err(FontError::Unsupported {
            manual: font_manual(),
        });
    };
    // Both the downloader and the extractor must be present before we start.
    for tool in ["curl", "unzip"] {
        if !runner.available(tool) {
            return Err(FontError::ToolMissing {
                tool,
                manual: font_manual(),
            });
        }
    }
    let dest_str = dest.to_string_lossy().into_owned();
    if std::fs::create_dir_all(dest).is_err() {
        return Err(FontError::DirCreateFailed {
            dir: dest_str,
            manual: font_manual(),
        });
    }
    let archive = std::env::temp_dir().join(ARCHIVE_NAME);
    let archive_str = archive.to_string_lossy().into_owned();
    // Download the archive. `-fsSL` fails on HTTP errors, stays quiet, and
    // follows the `latest` redirect.
    let downloaded = runner.run("curl", &["-fsSL", DOWNLOAD_URL, "-o", &archive_str]);
    if !matches!(downloaded, Ok(true)) {
        let _ = std::fs::remove_file(&archive);
        return Err(FontError::DownloadFailed {
            manual: font_manual(),
        });
    }
    // Extract only the font files (ignore the bundled README/LICENSE).
    let extracted = runner.run(
        "unzip",
        &["-o", &archive_str, "*.ttf", "*.otf", "-d", &dest_str],
    );
    // The archive is no longer needed regardless of how extraction went.
    let _ = std::fs::remove_file(&archive);
    if !matches!(extracted, Ok(true)) {
        return Err(FontError::ExtractFailed {
            manual: font_manual(),
        });
    }
    // Linux looks fonts up through fontconfig's cache, so refresh it; the step
    // is best-effort (a missing/failing `fc-cache` does not fail the install).
    if os == "linux" {
        let _ = runner.run("fc-cache", &["-f", &dest_str]);
    }
    Ok(FontStep::Installed {
        font: FONT_NAME,
        dir: dest_str,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A configurable [`CommandRunner`] recording the commands it ran, so a test
    /// can assert both the outcome and that the right commands fired. Programs in
    /// `fail` exit non-zero; every other program succeeds — letting a test fail
    /// just the download or just the extraction.
    struct FakeRunner {
        available: Vec<&'static str>,
        fail: Vec<&'static str>,
        ran: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn new(available: Vec<&'static str>) -> Self {
            Self {
                available,
                fail: Vec::new(),
                ran: RefCell::new(Vec::new()),
            }
        }

        fn failing(available: Vec<&'static str>, fail: Vec<&'static str>) -> Self {
            Self {
                available,
                fail,
                ran: RefCell::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn available(&self, program: &str) -> bool {
            self.available.contains(&program)
        }
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<bool> {
            self.ran
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            Ok(!self.fail.contains(&program))
        }
        fn check(&self, _program: &str, _args: &[&str]) -> bool {
            true
        }
        fn spawn(&self, _program: &str, _args: &[&str]) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Drop a fake Nerd Font file into `dir`.
    fn write_nerd_font(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("JetBrainsMonoNerdFont-Regular.ttf"), b"x").unwrap();
    }

    #[test]
    fn fake_runner_check_and_spawn_are_inert() {
        // `ensure` never probes (`check`) or spawns a daemon, so those trait
        // methods — required by `CommandRunner` — are exercised directly here.
        let runner = FakeRunner::new(vec![]);
        assert!(runner.check("curl", &["--version"]));
        assert!(runner.spawn("curl", &[]).is_ok());
    }

    #[test]
    fn download_url_and_manual_point_at_nerd_fonts() {
        assert!(download_url().contains("ryanoasis/nerd-fonts"));
        assert!(font_manual().contains("ryanoasis/nerd-fonts"));
    }

    #[test]
    fn font_dirs_per_platform() {
        let home = Path::new("/home/u");
        assert_eq!(font_dirs("macos", home), vec![home.join("Library/Fonts")]);
        assert_eq!(
            font_dirs("linux", home),
            vec![home.join(".local/share/fonts"), home.join(".fonts")]
        );
        assert!(font_dirs("windows", home).is_empty());
    }

    #[test]
    fn is_nerd_font_file_matches_only_font_files_with_the_marker() {
        assert!(is_nerd_font_file("JetBrainsMonoNerdFont-Regular.ttf"));
        assert!(is_nerd_font_file("Hack Nerd Font Mono.otf"));
        // Right extension, no marker.
        assert!(!is_nerd_font_file("Arial.ttf"));
        // Marker, wrong extension.
        assert!(!is_nerd_font_file("readme-nerdfont.txt"));
    }

    #[test]
    fn nerd_font_installed_scans_the_given_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let with_font = tmp.path().join("fonts");
        write_nerd_font(&with_font);
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        assert!(nerd_font_installed(std::slice::from_ref(&with_font)));
        // An empty directory and a non-existent one both count as "no font".
        assert!(!nerd_font_installed(&[empty, tmp.path().join("missing")]));
    }

    #[test]
    fn ensure_is_a_no_op_when_a_font_is_already_present() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fonts");
        write_nerd_font(&dir);
        let runner = FakeRunner::new(vec![]);
        assert_eq!(
            ensure("macos", &runner, &[dir]),
            Ok(FontStep::AlreadyPresent)
        );
        // Nothing was downloaded.
        assert!(runner.ran.borrow().is_empty());
    }

    #[test]
    fn ensure_reports_unsupported_without_a_font_directory() {
        let runner = FakeRunner::new(vec!["curl", "unzip"]);
        assert_eq!(
            ensure("windows", &runner, &[]),
            Err(FontError::Unsupported {
                manual: font_manual()
            })
        );
    }

    #[test]
    fn ensure_reports_a_missing_download_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fonts");
        // `unzip` present but `curl` missing.
        let runner = FakeRunner::new(vec!["unzip"]);
        assert_eq!(
            ensure("macos", &runner, &[dir]),
            Err(FontError::ToolMissing {
                tool: "curl",
                manual: font_manual()
            })
        );
    }

    #[test]
    fn ensure_reports_a_failed_directory_creation() {
        let tmp = tempfile::tempdir().unwrap();
        // A regular file where the font directory should be: create_dir_all fails.
        let path = tmp.path().join("fonts");
        std::fs::write(&path, b"not a dir").unwrap();
        let runner = FakeRunner::new(vec!["curl", "unzip"]);
        assert_eq!(
            ensure("macos", &runner, std::slice::from_ref(&path)),
            Err(FontError::DirCreateFailed {
                dir: path.to_string_lossy().into_owned(),
                manual: font_manual()
            })
        );
    }

    #[test]
    fn ensure_reports_a_failed_download() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fonts");
        // Tools present, but `curl` exits non-zero.
        let runner = FakeRunner::failing(vec!["curl", "unzip"], vec!["curl"]);
        assert_eq!(
            ensure("macos", &runner, &[dir]),
            Err(FontError::DownloadFailed {
                manual: font_manual()
            })
        );
    }

    #[test]
    fn ensure_reports_a_failed_extraction() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fonts");
        // `curl` succeeds but `unzip` exits non-zero.
        let runner = FakeRunner::failing(vec!["curl", "unzip"], vec!["unzip"]);
        assert_eq!(
            ensure("macos", &runner, &[dir]),
            Err(FontError::ExtractFailed {
                manual: font_manual()
            })
        );
    }

    #[test]
    fn ensure_downloads_extracts_and_refreshes_the_cache_on_linux() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fonts");
        let runner = FakeRunner::new(vec!["curl", "unzip"]);
        let dir_str = dir.to_string_lossy().into_owned();
        assert_eq!(
            ensure("linux", &runner, std::slice::from_ref(&dir)),
            Ok(FontStep::Installed {
                font: FONT_NAME,
                dir: dir_str.clone(),
            })
        );
        let archive = std::env::temp_dir().join(ARCHIVE_NAME);
        let archive_str = archive.to_string_lossy().into_owned();
        assert_eq!(
            *runner.ran.borrow(),
            vec![
                format!("curl -fsSL {DOWNLOAD_URL} -o {archive_str}"),
                format!("unzip -o {archive_str} *.ttf *.otf -d {dir_str}"),
                format!("fc-cache -f {dir_str}"),
            ]
        );
        // The destination directory was created.
        assert!(dir.is_dir());
    }

    #[test]
    fn ensure_skips_the_font_cache_refresh_off_linux() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fonts");
        let runner = FakeRunner::new(vec!["curl", "unzip"]);
        ensure("macos", &runner, &[dir]).unwrap();
        // No `fc-cache` on macOS.
        assert!(!runner
            .ran
            .borrow()
            .iter()
            .any(|c| c.starts_with("fc-cache")));
    }
}
