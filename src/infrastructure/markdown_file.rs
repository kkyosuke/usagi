//! Reading a Markdown file for the workspace screen's right-pane preview.
//!
//! The `preview` command names a Markdown file by path (`preview docs/x.md`) or
//! by bare name (`preview README`, which resolves to `README.md`). This module
//! resolves that name to a file **inside the workspace root**, reads it, and
//! returns its display path and contents. Rendering the Markdown to styled lines
//! is a separate, pure concern in [`crate::presentation::tui::markdown`]; here we
//! only do the filesystem IO.
//!
//! Resolution stays within the workspace: an absolute path or one that climbs out
//! with `..` is refused, so `preview` never reads files outside the project the
//! user opened.

use std::io::{ErrorKind, Read};
use std::path::{Component, Path};

use anyhow::{bail, Context, Result};

/// File extensions treated as Markdown when matching a bare name and when
/// deciding whether a target already names a file directly.
const MARKDOWN_EXTENSIONS: [&str; 2] = ["md", "markdown"];

/// The most bytes [`read_under`] loads from a previewed file. The preview is a
/// scrollable terminal pane, not an editor: a multi-megabyte file would be read
/// wholesale into memory and synchronously rendered + syntax-highlighted on the
/// event-loop thread, freezing the TUI. Reading at most this much (and marking
/// the cut) bounds both the memory and the render work to something the pane can
/// show, whatever the file's true size.
const MAX_PREVIEW_BYTES: u64 = 512 * 1024;

/// Resolve `target` to a Markdown file under `root`, read it, and return its
/// workspace-relative display path and contents.
///
/// `target` may be a path (`docs/guide.md`) or a bare name (`README`), in which
/// case the Markdown extensions are tried in turn (`README.md`, `README.markdown`).
/// The target must stay within `root`: an absolute path or one containing a `..`
/// component is refused.
pub fn read_under(root: &Path, target: &str) -> Result<(String, String)> {
    let target = target.trim();
    if target.is_empty() {
        bail!("no file given to preview");
    }

    let rel = Path::new(target);
    // Keep the read inside the workspace: reject absolute paths and any parent
    // traversal so `preview ../../etc/passwd` cannot escape the project.
    for component in rel.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("preview path must stay within the workspace: \"{target}\"");
            }
            _ => {}
        }
    }

    for candidate in candidates(target) {
        let path = root.join(&candidate);
        match read_capped(&path) {
            Ok(content) => return Ok((candidate, content)),
            // The candidate does not exist — try the next spelling.
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            // It exists but cannot be read (e.g. it is a directory, or permission
            // denied): report that rather than misleadingly claiming none matched.
            Err(e) => return Err(e).with_context(|| format!("reading \"{candidate}\"")),
        }
    }

    bail!("no Markdown file found for \"{target}\"");
}

/// Read `path` to a `String`, but never more than [`MAX_PREVIEW_BYTES`]. A file
/// larger than the cap is truncated to it and a marker line is appended, so an
/// enormous file still opens promptly instead of stalling the UI. Invalid UTF-8
/// (or a multi-byte char split by the cut) is replaced lossily — acceptable for a
/// read-only preview. Missing / unreadable paths surface as the matching
/// [`std::io::Error`], so the caller can tell "not found" from "exists but failed".
fn read_capped(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    // Read one byte past the cap so an exactly-cap-sized file is not falsely
    // marked truncated.
    file.by_ref()
        .take(MAX_PREVIEW_BYTES + 1)
        .read_to_end(&mut buf)?;
    let truncated = buf.len() as u64 > MAX_PREVIEW_BYTES;
    if truncated {
        buf.truncate(MAX_PREVIEW_BYTES as usize);
    }
    let mut content = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        content.push_str("\n\n… (preview truncated at 512 KiB)\n");
    }
    Ok(content)
}

/// The display paths to try for `target`, in order. A target that already ends in
/// a Markdown extension is tried as-is; a bare name has each extension appended
/// (and is also tried verbatim, so an extensionless Markdown file still resolves).
fn candidates(target: &str) -> Vec<String> {
    if has_markdown_extension(target) {
        return vec![target.to_string()];
    }
    let mut out: Vec<String> = MARKDOWN_EXTENSIONS
        .iter()
        .map(|ext| format!("{target}.{ext}"))
        .collect();
    out.push(target.to_string());
    out
}

/// Whether `target`'s extension is one of the Markdown extensions (case-insensitive).
fn has_markdown_extension(target: &str) -> bool {
    Path::new(target)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            MARKDOWN_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn reads_a_named_markdown_file_directly() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("guide.md"), "# Guide\n").unwrap();

        let (title, content) = read_under(dir.path(), "guide.md").unwrap();
        assert_eq!(title, "guide.md");
        assert_eq!(content, "# Guide\n");
    }

    #[test]
    fn resolves_a_bare_name_to_the_md_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "hello").unwrap();

        let (title, content) = read_under(dir.path(), "README").unwrap();
        assert_eq!(title, "README.md");
        assert_eq!(content, "hello");
    }

    #[test]
    fn resolves_a_bare_name_to_the_markdown_extension() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("NOTES.markdown"), "notes").unwrap();

        let (title, _) = read_under(dir.path(), "NOTES").unwrap();
        assert_eq!(title, "NOTES.markdown");
    }

    #[test]
    fn resolves_a_bare_name_to_an_extensionless_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("LICENSE"), "license text").unwrap();

        let (title, content) = read_under(dir.path(), "LICENSE").unwrap();
        assert_eq!(title, "LICENSE");
        assert_eq!(content, "license text");
    }

    #[test]
    fn reads_a_file_in_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/x.md"), "x").unwrap();

        let (title, _) = read_under(dir.path(), "docs/x.md").unwrap();
        assert_eq!(title, "docs/x.md");
    }

    #[test]
    fn matches_the_extension_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("R.MD"), "x").unwrap();
        let (title, _) = read_under(dir.path(), "R.MD").unwrap();
        assert_eq!(title, "R.MD");
    }

    #[test]
    fn reads_a_small_file_verbatim_without_a_truncation_marker() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("small.md"), "a\nb\n").unwrap();
        let (_, content) = read_under(dir.path(), "small.md").unwrap();
        assert_eq!(content, "a\nb\n");
        assert!(!content.contains("truncated"));
    }

    #[test]
    fn caps_an_oversized_file_and_marks_it_truncated() {
        let dir = tempfile::tempdir().unwrap();
        // A file comfortably larger than the cap.
        let big = "x".repeat((MAX_PREVIEW_BYTES as usize) + 1024);
        fs::write(dir.path().join("big.md"), &big).unwrap();

        let (_, content) = read_under(dir.path(), "big.md").unwrap();
        // The body is cut to the cap and a marker is appended, so the result is
        // bounded regardless of the source size.
        assert!(content.len() < big.len());
        assert!(content.contains("preview truncated"));
        assert!(content.starts_with("xxxx"));
    }

    #[test]
    fn errors_on_an_empty_target() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_under(dir.path(), "   ").is_err());
    }

    #[test]
    fn errors_when_no_file_matches() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_under(dir.path(), "missing").unwrap_err().to_string();
        assert!(err.contains("no Markdown file"));
    }

    #[test]
    fn refuses_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_under(dir.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn refuses_a_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_under(dir.path(), "../secret.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("stay within the workspace"));
    }

    #[test]
    fn errors_when_the_target_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("docs.md")).unwrap();
        // `docs.md` exists but is a directory, so reading it fails with a
        // non-NotFound error that is surfaced rather than skipped.
        let err = read_under(dir.path(), "docs.md").unwrap_err().to_string();
        assert!(err.contains("reading \"docs.md\""));
    }
}
