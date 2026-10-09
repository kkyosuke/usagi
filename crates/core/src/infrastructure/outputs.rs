//! Session-generated files and immutable, workspace-local output archives.
//!
//! Archives are independent of session worktrees and never removed by session
//! teardown. Failed snapshots leave the original outputs intact.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use crate::domain::presentation_text::presentation_character_is_safe;
use crate::domain::workspace_layout::{SESSIONS_DIR, STATE_DIR};

/// Reserved session output directory, also used below `.usagi` for archives.
pub const OUTPUTS_DIR: &str = "outputs";
const MAX_ENTRIES: usize = 20_000;
const MAX_DEPTH: usize = 64;

fn invalid() -> io::Error {
    io::Error::other(
        "outputs contain an unsafe path, unsupported entry, or exceed traversal limits",
    )
}

/// Create a real directory, refusing links and other occupied entries.
fn directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(path)?.is_dir() {
                Ok(())
            } else {
                Err(invalid())
            }
        }
        Err(error) => Err(error),
    }
}

/// Prepare the session's ignored output directory without replacing user files.
///
/// # Errors
/// Returns an error if the directory cannot be created safely or written.
pub fn prepare(session: &Path) -> io::Result<()> {
    let root = session.join(OUTPUTS_DIR);
    directory(&root)?;
    let ignore = root.join(".gitignore");
    match OpenOptions::new().write(true).create_new(true).open(ignore) {
        Ok(mut file) => {
            file.write_all(b"# Generated artifacts are retained separately from Git.\n*\n")
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Walk bounded, regular output files only. Symlinks and special files are
/// refused instead of dereferenced or silently lost during archiving.
fn walk(
    root: &Path,
    relative: &Path,
    files: &mut Vec<PathBuf>,
    visited: &mut usize,
) -> io::Result<()> {
    if relative.components().count() > MAX_DEPTH {
        return Err(invalid());
    }
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.is_dir() {
        let mut entries = fs::read_dir(path)?
            .take(MAX_ENTRIES + 1)
            .collect::<io::Result<Vec<_>>>()?;
        if entries.len() > MAX_ENTRIES {
            return Err(invalid());
        }
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            *visited += 1;
            if *visited > MAX_ENTRIES {
                return Err(invalid());
            }
            let name = entry.file_name();
            if relative.as_os_str().is_empty() && name == ".gitignore" {
                continue;
            }
            walk(root, &relative.join(name), files, visited)?;
        }
    } else if metadata.is_file() {
        files.push(relative.to_path_buf());
    } else {
        return Err(invalid());
    }
    Ok(())
}

fn files(root: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(metadata) if !metadata.is_dir() => return Err(invalid()),
        Ok(_) => {}
    }
    let mut result = Vec::new();
    walk(root, Path::new(""), &mut result, &mut 0)?;
    Ok(result)
}

// Open through directory descriptors so swapped intermediate symlinks cannot
// redirect an archive read out of the session while teardown is in progress.
fn open_relative(root: &Path, relative: &Path) -> io::Result<File> {
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(invalid());
        };
        let name = CString::new(name.as_bytes()).map_err(|_| invalid())?;
        let flags = libc::O_RDONLY
            | libc::O_NONBLOCK
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if components.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: the directory descriptor is live and name is NUL terminated.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ownership of the newly opened descriptor transfers once.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

/// Snapshot all output files before removing a session. Published snapshots
/// have unique names, so retries and reused session names never overwrite data.
/// `session` has already passed the daemon's canonical teardown fence.
///
/// # Errors
/// Refuses unsafe entries, invalid session layouts, and incomplete copies.
pub fn archive(session: &Path) -> io::Result<Option<PathBuf>> {
    let source = session.join(OUTPUTS_DIR);
    let entries = files(&source)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let container = session.parent().ok_or_else(invalid)?;
    let state = container.parent().ok_or_else(invalid)?;
    if container.file_name() != Some(SESSIONS_DIR.as_ref())
        || state.file_name() != Some(STATE_DIR.as_ref())
    {
        return Err(invalid());
    }
    let name = session.file_name().ok_or_else(invalid)?;
    let archives = state.join(OUTPUTS_DIR);
    directory(&archives)?;
    prepare(state)?;
    File::open(state)?.sync_all()?;
    let owner = archives.join(name);
    directory(&owner)?;
    File::open(&archives)?.sync_all()?;
    let id = uuid::Uuid::new_v4().to_string();
    let staging = owner.join(format!(".partial-{id}"));
    fs::create_dir(&staging)?;
    let published = owner.join(id);
    let result = (|| {
        for relative in entries {
            let destination = staging.join(&relative);
            fs::create_dir_all(destination.parent().ok_or_else(invalid)?)?;
            let input = open_relative(session, &Path::new(OUTPUTS_DIR).join(&relative))?;
            copy_regular(input, &destination)?;
        }
        sync_directories(&staging)?;
        fs::rename(&staging, &published)?;
        File::open(&owner)?.sync_all()?;
        Ok(Some(published))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(staging);
    }
    result
}

fn copy_regular(mut input: File, destination: &Path) -> io::Result<()> {
    if !input.metadata()?.is_file() {
        return Err(invalid());
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()
}

fn sync_directories(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_directories(&entry.path())?;
        }
    }
    File::open(root)?.sync_all()
}

/// Paths offered by the output finder, relative to its session/workspace root.
/// Discovery deliberately does not use Git, so ignored generated files appear.
///
/// # Errors
/// Returns an error for unsafe entries, traversal limits, or inaccessible files.
pub fn list(root: &Path, all: bool) -> io::Result<Vec<String>> {
    let mut paths = Vec::new();
    if all {
        let state = root.join(STATE_DIR);
        match fs::symlink_metadata(&state) {
            Ok(metadata) if !metadata.is_dir() => return Err(invalid()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(paths),
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        for base in [state.join(SESSIONS_DIR), state.join(OUTPUTS_DIR)] {
            if let Ok(metadata) = fs::symlink_metadata(&base)
                && !metadata.is_dir()
            {
                return Err(invalid());
            }
            let entries = match fs::read_dir(&base) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                if base.ends_with(SESSIONS_DIR) {
                    append(root, &entry.path().join(OUTPUTS_DIR), &mut paths)?;
                } else {
                    for snapshot in fs::read_dir(entry.path())? {
                        let snapshot = snapshot?;
                        if snapshot.file_type()?.is_dir()
                            && uuid::Uuid::parse_str(&snapshot.file_name().to_string_lossy())
                                .is_ok()
                        {
                            append(root, &snapshot.path(), &mut paths)?;
                        }
                    }
                }
            }
        }
    } else {
        append(root, &root.join(OUTPUTS_DIR), &mut paths)?;
    }
    paths.sort();
    Ok(paths)
}

fn append(root: &Path, directory: &Path, paths: &mut Vec<String>) -> io::Result<()> {
    for relative in files(directory)? {
        let path = directory.join(relative);
        let relative = path.strip_prefix(root).map_err(|_| invalid())?;
        if let Some(text) = relative.to_str()
            && text.chars().all(presentation_character_is_safe)
        {
            paths.push(text.to_owned());
        }
        if paths.len() > MAX_ENTRIES {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Restrict file reads and external-open requests to the selected output scope.
#[must_use]
pub fn contains(relative: &str, all: bool) -> bool {
    let parts = Path::new(relative)
        .components()
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    let Some(parts) = parts else {
        return false;
    };
    if !relative.chars().all(presentation_character_is_safe) {
        return false;
    }
    if all {
        matches!(
            parts.as_slice(),
            [STATE_DIR, SESSIONS_DIR, _, OUTPUTS_DIR, _, ..]
        ) || matches!(parts.as_slice(), [STATE_DIR, OUTPUTS_DIR, _, snapshot, _, ..] if uuid::Uuid::parse_str(snapshot).is_ok())
    } else {
        matches!(parts.as_slice(), [OUTPUTS_DIR, _, ..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn session(root: &Path, name: &str) -> PathBuf {
        let session = root.join(STATE_DIR).join(SESSIONS_DIR).join(name);
        fs::create_dir_all(&session).unwrap();
        prepare(&session).unwrap();
        session
    }

    #[test]
    fn outputs_survive_removal_and_same_named_sessions_never_overwrite_archives() {
        let temp = tempfile::tempdir().unwrap();
        let first = session(temp.path(), "first");
        let second = session(temp.path(), "second");
        assert_eq!(archive(&first).unwrap(), None);
        prepare(&first).unwrap();
        fs::create_dir(first.join("outputs/nested")).unwrap();
        fs::write(first.join("outputs/nested/report.md"), "first result").unwrap();
        fs::write(second.join("outputs/chart.png"), [1, 2, 3]).unwrap();
        assert_eq!(list(&first, false).unwrap(), ["outputs/nested/report.md"]);
        assert_eq!(list(temp.path(), true).unwrap().len(), 2);
        let archived = archive(&first).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(archived.join("nested/report.md")).unwrap(),
            "first result"
        );
        assert!(first.join("outputs/nested/report.md").exists());
        fs::remove_dir_all(&first).unwrap();
        let recreated = session(temp.path(), "first");
        fs::write(recreated.join("outputs/report.md"), "new result").unwrap();
        let newer = archive(&recreated).unwrap().unwrap();
        assert_ne!(archived, newer);
        assert_eq!(
            fs::read_to_string(archived.join("nested/report.md")).unwrap(),
            "first result"
        );
        let listed = list(temp.path(), true).unwrap();
        assert_eq!(listed.len(), 4);
        assert!(listed.iter().all(|path| contains(path, true)));
        fs::create_dir(newer.parent().unwrap().join(".partial-interrupted")).unwrap();
        fs::write(
            newer
                .parent()
                .unwrap()
                .join(".partial-interrupted/hidden.txt"),
            "partial",
        )
        .unwrap();
        assert_eq!(list(temp.path(), true).unwrap(), listed);
    }

    #[test]
    fn missing_outputs_are_empty_and_non_directories_or_links_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        assert!(list(temp.path(), false).unwrap().is_empty());
        assert!(list(temp.path(), true).unwrap().is_empty());
        assert!(archive(temp.path()).unwrap().is_none());
        fs::write(temp.path().join("outputs"), "occupied").unwrap();
        assert!(prepare(temp.path()).is_err());
        assert!(list(temp.path(), false).is_err());
        fs::remove_file(temp.path().join("outputs")).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, temp.path().join("outputs")).unwrap();
        assert!(prepare(temp.path()).is_err());
        assert!(list(temp.path(), false).is_err());
        symlink(&outside, temp.path().join(STATE_DIR)).unwrap();
        assert!(list(temp.path(), true).is_err());
        fs::remove_file(temp.path().join(STATE_DIR)).unwrap();
        fs::create_dir(temp.path().join(STATE_DIR)).unwrap();
        symlink(&outside, temp.path().join(STATE_DIR).join(SESSIONS_DIR)).unwrap();
        assert!(list(temp.path(), true).is_err());
    }

    #[test]
    fn unsafe_entry_prevents_publication_and_keeps_original_files() {
        let temp = tempfile::tempdir().unwrap();
        let session = session(temp.path(), "safe");
        fs::write(session.join("outputs/report.txt"), "keep").unwrap();
        symlink(temp.path(), session.join("outputs/link")).unwrap();
        assert!(archive(&session).is_err());
        assert!(session.join("outputs/report.txt").exists());
        assert!(!temp.path().join(".usagi/outputs").exists());
        assert!(open_relative(&session, Path::new("outputs/link/secret")).is_err());
        fs::remove_file(session.join("outputs/link")).unwrap();
        fs::write(session.join("outputs/escape\u{1b}.txt"), "not displayed").unwrap();
        assert_eq!(list(&session, false).unwrap(), ["outputs/report.txt"]);
        assert!(
            archive(&session)
                .unwrap()
                .unwrap()
                .join("escape\u{1b}.txt")
                .exists()
        );
        assert!(open_relative(&session, Path::new("../secret")).is_err());
        assert!(open_relative(&session, Path::new("outputs/a\0b")).is_err());
        assert!(
            open_relative(&session, Path::new("outputs/report.txt"))
                .unwrap()
                .metadata()
                .unwrap()
                .is_file()
        );
    }

    #[test]
    fn archive_requires_managed_container_and_refuses_occupied_archive_roots() {
        let temp = tempfile::tempdir().unwrap();
        prepare(temp.path()).unwrap();
        fs::write(temp.path().join("outputs/result.txt"), "result").unwrap();
        assert!(archive(temp.path()).is_err());
        let session = session(temp.path(), "one");
        fs::write(session.join("outputs/result.txt"), "result").unwrap();
        fs::write(temp.path().join(".usagi/outputs"), "occupied").unwrap();
        assert!(archive(&session).is_err());
        assert!(session.join("outputs/result.txt").exists());
    }

    #[test]
    fn output_scope_rejects_traversal_and_non_output_files() {
        for path in [
            "",
            "/outputs/a",
            "outputs/../secret",
            "src/main.rs",
            "outputs/a\n.txt",
        ] {
            assert!(!contains(path, false), "{path}");
            assert!(!contains(path, true), "{path}");
        }
        assert!(contains("outputs/nested/a.txt", false));
        assert!(!contains("outputs/nested/a.txt", true));
        assert!(contains(".usagi/sessions/one/outputs/a.txt", true));
        assert!(!contains(".usagi/sessions/one/src/a.txt", true));
        assert!(!contains(".usagi/outputs/one/.partial-123/a.txt", true));
        assert!(contains(
            ".usagi/outputs/one/00000000-0000-4000-8000-000000000000/a.txt",
            true
        ));
    }

    #[test]
    fn traversal_limits_fail_without_publishing_a_partial_inventory() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "a").unwrap();
        assert!(
            walk(temp.path(), Path::new(""), &mut Vec::new(), &mut {
                MAX_ENTRIES
            })
            .is_err()
        );
        let deep = PathBuf::from(vec!["a"; MAX_DEPTH + 1].join("/"));
        assert!(walk(temp.path(), &deep, &mut Vec::new(), &mut 0).is_err());
        let mut existing = vec![String::new(); MAX_ENTRIES];
        assert!(append(temp.path(), temp.path(), &mut existing).is_err());
    }
    #[test]
    fn filesystem_errors_leave_sources_and_refuse_incomplete_inventories() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("blocked");
        fs::write(&blocked, "file").unwrap();
        assert!(directory(&blocked.join("child")).is_err());
        assert!(files(&blocked.join("child")).is_err());
        assert!(list(&blocked.join("child"), true).is_err());
        assert!(copy_regular(File::open(temp.path()).unwrap(), &temp.path().join("copy")).is_err());
        fs::write(temp.path().join("other"), "other").unwrap();
        assert!(append(&blocked, temp.path(), &mut Vec::new()).is_err());
        let many = temp.path().join("many");
        fs::create_dir(&many).unwrap();
        for index in 0..=MAX_ENTRIES {
            fs::write(many.join(index.to_string()), "").unwrap();
        }
        assert!(files(&many).is_err());
    }

    #[test]
    fn permission_errors_abort_archive_and_remove_partial_snapshot() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let session = session(temp.path(), "one");
        let unreadable = session.join("outputs/unreadable.txt");
        fs::write(&unreadable, "keep").unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let result = archive(&session);
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(unreadable).unwrap(), "keep");
        assert_eq!(
            fs::read_dir(temp.path().join(".usagi/outputs/one"))
                .unwrap()
                .count(),
            0
        );
        let output = session.join("outputs");
        fs::remove_file(output.join(".gitignore")).unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o500)).unwrap();
        let result = prepare(&session);
        fs::set_permissions(&output, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        let container = temp.path().join(".usagi/sessions");
        fs::set_permissions(&container, fs::Permissions::from_mode(0o000)).unwrap();
        let result = list(temp.path(), true);
        fs::set_permissions(&container, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
    }
}
