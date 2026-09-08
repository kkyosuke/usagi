//! Safe file discovery and text loading for the Home Preview overlay.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};
use std::time::Duration;

use usagi_core::domain::presentation_text::presentation_character_is_safe;
use usagi_core::infrastructure::bounded_process::{
    ChildOutputObservation, ChildPolicy, observe_command_output,
};
use usagi_core::infrastructure::git::confined_git_command;

/// Maximum number of repository paths offered to the fuzzy finder.
pub(crate) const MAX_PREVIEW_FILES: usize = 20_000;
/// Maximum bytes read from one previewed file.
pub(crate) const MAX_PREVIEW_BYTES: usize = 512 * 1024;
/// Maximum bytes retained from `git ls-files` before the child is terminated.
pub(crate) const MAX_PREVIEW_LIST_BYTES: usize = 8 * 1024 * 1024;

const PREVIEW_LIST_POLICY: ChildPolicy = ChildPolicy {
    timeout: Duration::from_secs(2),
    terminate_grace: Duration::from_millis(100),
    output_limit: MAX_PREVIEW_LIST_BYTES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilePreviewError {
    FilesUnavailable,
    FileUnavailable,
    OutsideRoot,
    NotRegular,
    TooLarge,
    Binary,
    NotUtf8,
}

impl FilePreviewError {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::FilesUnavailable => "Files are unavailable.",
            Self::FileUnavailable => "This file is unavailable.",
            Self::OutsideRoot => "This file is outside the preview root.",
            Self::NotRegular => "Only regular files can be previewed.",
            Self::TooLarge => "Files larger than 512 KiB cannot be previewed.",
            Self::Binary => "Binary files cannot be previewed.",
            Self::NotUtf8 => "Only UTF-8 text files can be previewed.",
        }
    }

    pub(crate) const fn error_id(self) -> &'static str {
        match self {
            Self::FilesUnavailable => "preview-files",
            Self::FileUnavailable => "preview-file",
            Self::OutsideRoot => "preview-outside-root",
            Self::NotRegular => "preview-not-regular",
            Self::TooLarge => "preview-too-large",
            Self::Binary => "preview-binary",
            Self::NotUtf8 => "preview-not-utf8",
        }
    }
}

/// Return tracked and untracked, non-ignored repository files in stable order.
pub(crate) fn list_files(root: &Path) -> Result<Vec<String>, FilePreviewError> {
    let mut command = confined_git_command(root);
    command.args([
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]);
    listed_files(observe_command_output(command, PREVIEW_LIST_POLICY))
}

fn listed_files(observation: ChildOutputObservation) -> Result<Vec<String>, FilePreviewError> {
    let ChildOutputObservation::Success { stdout, .. } = observation else {
        return Err(FilePreviewError::FilesUnavailable);
    };
    let mut files = stdout
        .split(|byte| *byte == 0)
        .filter_map(|path| std::str::from_utf8(path).ok())
        .filter(|path| valid_relative_path(path))
        .filter(|path| path.chars().all(presentation_character_is_safe))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files.truncate(MAX_PREVIEW_FILES);
    Ok(files)
}

/// Load one finder listing or one selected document for the background preview
/// lane. Exactly one side of the tuple is populated.
pub(crate) fn load_preview(
    root: &Path,
    path: Option<&str>,
) -> Result<(Vec<String>, Vec<String>), FilePreviewError> {
    match path {
        Some(path) => read_file(root, path).map(|lines| (Vec::new(), lines)),
        None => list_files(root).map(|files| (files, Vec::new())),
    }
}

/// Read one UTF-8 regular file without allowing the requested path to escape
/// the target workspace or session worktree.
pub(crate) fn read_file(root: &Path, relative: &str) -> Result<Vec<String>, FilePreviewError> {
    if !valid_relative_path(relative) {
        return Err(FilePreviewError::OutsideRoot);
    }
    let file = open_beneath(
        &root
            .canonicalize()
            .map_err(|_| FilePreviewError::FileUnavailable)?,
        relative,
    )?;
    let metadata = file
        .metadata()
        .map_err(|_| FilePreviewError::FileUnavailable)?;
    if !metadata.is_file() {
        return Err(FilePreviewError::NotRegular);
    }
    if metadata.len() > MAX_PREVIEW_BYTES as u64 {
        return Err(FilePreviewError::TooLarge);
    }

    let capacity = usize::try_from(metadata.len())
        .unwrap_or(MAX_PREVIEW_BYTES)
        .min(MAX_PREVIEW_BYTES);
    let mut bytes = Vec::with_capacity(capacity);
    file.take((MAX_PREVIEW_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| FilePreviewError::FileUnavailable)?;
    decode_file(bytes)
}

/// Open every component relative to an already-open directory descriptor.
/// `O_NOFOLLOW` on each hop makes the containment decision and the final read
/// one descriptor chain rather than a check-then-open path race.
fn open_beneath(root: &Path, relative: &str) -> Result<File, FilePreviewError> {
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)
        .map_err(|_| FilePreviewError::FileUnavailable)?;
    let mut components = Path::new(relative).components().peekable();
    while let Some(Component::Normal(component)) = components.next() {
        let name = CString::new(component.as_bytes()).map_err(|_| FilePreviewError::OutsideRoot)?;
        let directory_flag = if components.peek().is_some() {
            libc::O_DIRECTORY
        } else {
            0
        };
        // SAFETY: `directory` owns a live descriptor, `name` is NUL-terminated,
        // and a successful raw descriptor is moved into exactly one `File`.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | directory_flag,
            )
        };
        if descriptor < 0 {
            return Err(open_error(&std::io::Error::last_os_error()));
        }
        // SAFETY: this branch owns the newly returned descriptor exactly once.
        let opened = unsafe { File::from_raw_fd(descriptor) };
        if components.peek().is_none() {
            return Ok(opened);
        }
        directory = opened;
    }
    Err(FilePreviewError::OutsideRoot)
}

fn open_error(error: &std::io::Error) -> FilePreviewError {
    match error.raw_os_error() {
        Some(libc::ELOOP | libc::ENOTDIR) => FilePreviewError::OutsideRoot,
        _ => FilePreviewError::FileUnavailable,
    }
}

fn decode_file(bytes: Vec<u8>) -> Result<Vec<String>, FilePreviewError> {
    if bytes.len() > MAX_PREVIEW_BYTES {
        return Err(FilePreviewError::TooLarge);
    }
    if bytes.contains(&0) {
        return Err(FilePreviewError::Binary);
    }
    let text = String::from_utf8(bytes).map_err(|_| FilePreviewError::NotUtf8)?;
    Ok(text.lines().map(sanitize_line).collect())
}

fn valid_relative_path(raw: &str) -> bool {
    !raw.is_empty()
        && Path::new(raw)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn sanitize_line(line: &str) -> String {
    line.chars()
        .map(|character| {
            if character == '\t' {
                ' '
            } else if presentation_character_is_safe(character) {
                character
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

    fn output(stdout: &str) -> ChildOutputObservation {
        ChildOutputObservation::Success {
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn listing_sorts_deduplicates_and_rejects_unsafe_paths() {
        assert_eq!(
            listed_files(output(
                "src/z.rs\0README.md\0src/z.rs\0../outside\0/a/./b\0bad\nname\0\0",
            ))
            .unwrap(),
            vec!["README.md", "src/z.rs"]
        );
    }

    #[test]
    fn listing_maps_spawn_and_exit_failures_to_a_safe_error() {
        for failure in [
            ChildOutputObservation::SpawnFailed,
            ChildOutputObservation::ExitFailure,
            ChildOutputObservation::TimedOut,
            ChildOutputObservation::OutputTooLarge,
            ChildOutputObservation::ObservationFailed,
        ] {
            assert_eq!(
                listed_files(failure),
                Err(FilePreviewError::FilesUnavailable)
            );
        }
    }

    #[test]
    fn listing_is_bounded() {
        let stdout = (0..=MAX_PREVIEW_FILES).fold(String::new(), |mut output, index| {
            write!(&mut output, "{index:05}.txt\0").unwrap();
            output
        });
        let files = listed_files(output(&stdout)).unwrap();
        assert_eq!(files.len(), MAX_PREVIEW_FILES);
    }

    #[test]
    fn listing_runs_git_with_a_bounded_machine_output_contract() {
        let root = tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.path().join("b.txt"), "b").unwrap();
        fs::write(root.path().join("a.txt"), "a").unwrap();
        assert_eq!(list_files(root.path()).unwrap(), vec!["a.txt", "b.txt"]);
        assert_eq!(
            list_files(&root.path().join("missing")),
            Err(FilePreviewError::FilesUnavailable)
        );
    }

    #[test]
    fn text_reading_is_bounded_utf8_and_terminal_safe() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("safe.txt"), "one\ttwo\n\u{1b}[31mred").unwrap();
        assert_eq!(
            read_file(root.path(), "safe.txt").unwrap(),
            vec!["one two", "�[31mred"]
        );

        fs::write(root.path().join("binary"), b"a\0b").unwrap();
        assert_eq!(
            read_file(root.path(), "binary"),
            Err(FilePreviewError::Binary)
        );
        fs::write(root.path().join("not-utf8"), [0xff]).unwrap();
        assert_eq!(
            read_file(root.path(), "not-utf8"),
            Err(FilePreviewError::NotUtf8)
        );
        fs::write(root.path().join("large"), vec![b'x'; MAX_PREVIEW_BYTES + 1]).unwrap();
        assert_eq!(
            read_file(root.path(), "large"),
            Err(FilePreviewError::TooLarge)
        );
        assert_eq!(
            decode_file(vec![b'x'; MAX_PREVIEW_BYTES + 1]),
            Err(FilePreviewError::TooLarge)
        );
    }

    #[test]
    fn background_loader_reads_a_nested_document_and_rejects_an_empty_path() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("nested/file.txt"), "one\ntwo").unwrap();

        assert_eq!(
            load_preview(root.path(), Some("nested/file.txt")).unwrap(),
            (Vec::new(), vec!["one".to_owned(), "two".to_owned()])
        );
        assert!(matches!(
            open_beneath(root.path(), ""),
            Err(FilePreviewError::OutsideRoot)
        ));
    }

    #[test]
    fn reading_rejects_invalid_missing_and_non_file_targets() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        assert_eq!(
            read_file(root.path(), "../outside"),
            Err(FilePreviewError::OutsideRoot)
        );
        assert_eq!(
            read_file(root.path(), "missing"),
            Err(FilePreviewError::FileUnavailable)
        );
        assert_eq!(
            read_file(root.path(), "directory"),
            Err(FilePreviewError::NotRegular)
        );
        assert_eq!(
            read_file(&root.path().join("missing-root"), "file"),
            Err(FilePreviewError::FileUnavailable)
        );
    }

    #[cfg(unix)]
    #[test]
    fn reading_rejects_a_symlink_that_escapes_the_root() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        symlink(outside.path().join("secret"), root.path().join("link")).unwrap();
        assert_eq!(
            read_file(root.path(), "link"),
            Err(FilePreviewError::OutsideRoot)
        );
    }

    #[cfg(unix)]
    #[test]
    fn reading_rejects_symlinks_at_every_descriptor_hop() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("real")).unwrap();
        fs::write(root.path().join("real/file"), "safe").unwrap();
        symlink(
            root.path().join("real"),
            root.path().join("linked-directory"),
        )
        .unwrap();
        symlink(
            root.path().join("real/file"),
            root.path().join("linked-file"),
        )
        .unwrap();

        assert_eq!(
            read_file(root.path(), "linked-directory/file"),
            Err(FilePreviewError::OutsideRoot)
        );
        assert_eq!(
            read_file(root.path(), "linked-file"),
            Err(FilePreviewError::OutsideRoot)
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_open_descriptor_cannot_be_redirected_by_a_later_path_swap() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(root.path().join("file"), "safe").unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        let mut opened = open_beneath(root.path(), "file").unwrap();

        fs::rename(root.path().join("file"), root.path().join("old")).unwrap();
        symlink(outside.path().join("secret"), root.path().join("file")).unwrap();
        let mut contents = String::new();
        opened.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "safe");
    }

    #[test]
    fn open_errors_distinguish_symlink_or_non_directory_escape() {
        assert_eq!(
            open_error(&std::io::Error::from_raw_os_error(libc::ELOOP)),
            FilePreviewError::OutsideRoot
        );
        assert_eq!(
            open_error(&std::io::Error::from_raw_os_error(libc::ENOTDIR)),
            FilePreviewError::OutsideRoot
        );
        assert_eq!(
            open_error(&std::io::Error::from_raw_os_error(libc::ENOENT)),
            FilePreviewError::FileUnavailable
        );
    }

    #[test]
    fn every_error_has_a_safe_message_and_stable_id() {
        for error in [
            FilePreviewError::FilesUnavailable,
            FilePreviewError::FileUnavailable,
            FilePreviewError::OutsideRoot,
            FilePreviewError::NotRegular,
            FilePreviewError::TooLarge,
            FilePreviewError::Binary,
            FilePreviewError::NotUtf8,
        ] {
            assert!(!error.message().is_empty());
            assert!(error.error_id().starts_with("preview-"));
        }
    }
}
