//! Safe file discovery and text loading for the Home Preview overlay.

use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

use usagi_core::domain::presentation_text::presentation_character_is_safe;
use usagi_core::infrastructure::git::GitRunner;

/// Maximum number of repository paths offered to the fuzzy finder.
pub(crate) const MAX_PREVIEW_FILES: usize = 20_000;
/// Maximum bytes read from one previewed file.
pub(crate) const MAX_PREVIEW_BYTES: usize = 512 * 1024;

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
pub(crate) fn list_files(
    git: &dyn GitRunner,
    root: &Path,
) -> Result<Vec<String>, FilePreviewError> {
    let output = git
        .run(
            root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )
        .map_err(|_| FilePreviewError::FilesUnavailable)?;
    if !output.success {
        return Err(FilePreviewError::FilesUnavailable);
    }

    let mut files = output
        .stdout
        .split('\0')
        .filter(|path| valid_relative_path(path))
        .filter(|path| path.chars().all(presentation_character_is_safe))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files.truncate(MAX_PREVIEW_FILES);
    Ok(files)
}

/// Read one UTF-8 regular file without allowing the requested path to escape
/// the target workspace or session worktree.
pub(crate) fn read_file(root: &Path, relative: &str) -> Result<Vec<String>, FilePreviewError> {
    if !valid_relative_path(relative) {
        return Err(FilePreviewError::OutsideRoot);
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|_| FilePreviewError::FileUnavailable)?;
    let candidate = canonical_root.join(relative);
    let canonical_file = candidate
        .canonicalize()
        .map_err(|_| FilePreviewError::FileUnavailable)?;
    if !canonical_file.starts_with(&canonical_root) {
        return Err(FilePreviewError::OutsideRoot);
    }
    let metadata = canonical_file
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
    File::open(&canonical_file)
        .and_then(|file| {
            file.take((MAX_PREVIEW_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|_| FilePreviewError::FileUnavailable)?;
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

    use anyhow::Result;
    use tempfile::tempdir;
    use usagi_core::infrastructure::git::{GitOutput, GitRunner};

    use super::*;

    struct FakeGit(Option<GitOutput>);

    impl GitRunner for FakeGit {
        fn run(&self, _: &Path, _: &[&str]) -> Result<GitOutput> {
            self.0
                .clone()
                .ok_or_else(|| anyhow::anyhow!("secret failure"))
        }
    }

    fn output(success: bool, stdout: &str) -> GitOutput {
        GitOutput {
            success,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    #[test]
    fn listing_sorts_deduplicates_and_rejects_unsafe_paths() {
        let git = FakeGit(Some(output(
            true,
            "src/z.rs\0README.md\0src/z.rs\0../outside\0/a/./b\0bad\nname\0\0",
        )));
        assert_eq!(
            list_files(&git, Path::new("/repo")).unwrap(),
            vec!["README.md", "src/z.rs"]
        );
    }

    #[test]
    fn listing_maps_spawn_and_exit_failures_to_a_safe_error() {
        let spawn = FakeGit(None);
        assert_eq!(
            list_files(&spawn, Path::new("/repo")),
            Err(FilePreviewError::FilesUnavailable)
        );
        let exit = FakeGit(Some(output(false, "")));
        assert_eq!(
            list_files(&exit, Path::new("/repo")),
            Err(FilePreviewError::FilesUnavailable)
        );
    }

    #[test]
    fn listing_is_bounded() {
        let stdout = (0..=MAX_PREVIEW_FILES).fold(String::new(), |mut output, index| {
            write!(&mut output, "{index:05}.txt\0").unwrap();
            output
        });
        let files = list_files(&FakeGit(Some(output(true, &stdout))), Path::new("/repo")).unwrap();
        assert_eq!(files.len(), MAX_PREVIEW_FILES);
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
