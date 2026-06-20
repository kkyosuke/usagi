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

use std::io::ErrorKind;
use std::path::{Component, Path};

use anyhow::{bail, Context, Result};

/// File extensions treated as Markdown when matching a bare name and when
/// deciding whether a target already names a file directly.
const MARKDOWN_EXTENSIONS: [&str; 2] = ["md", "markdown"];

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
        match std::fs::read_to_string(&path) {
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
    fn a_non_markdown_extension_target_is_tried_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "plain").unwrap();
        // `notes.txt` has a non-Markdown extension, so the `.md` / `.markdown`
        // spellings are tried first (and skipped as missing) before the verbatim
        // name resolves.
        let (title, content) = read_under(dir.path(), "notes.txt").unwrap();
        assert_eq!(title, "notes.txt");
        assert_eq!(content, "plain");
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
