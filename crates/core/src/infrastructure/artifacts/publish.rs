//! Publish only explicitly selected work files. All writes are relative to
//! pinned directory descriptors, including the final atomic index replacement.

use super::{
    ARTIFACT_INDEX, ARTIFACTS_DIR, Entry, Index, MAX_ENTRIES, MAX_INDEX_BYTES, Review,
    directory_files, displayable_path, invalid, legacy_deliverable, read_index, valid_path,
    valid_purpose,
};
use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Entry point staged in .work/ and its explicitly selected relative dependencies.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    source: PathBuf,
    purpose: String,
    #[serde(default)]
    assets: Vec<PathBuf>,
}

const MAX_BYTES: usize = 64 * 1024 * 1024;
const READ: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);
const DIRECTORY: OFlags = READ.union(OFlags::DIRECTORY);

fn subdir(parent: &File, path: &Path, create: bool) -> io::Result<File> {
    let mut dir = parent.try_clone()?;
    for part in path {
        if create {
            match mkdirat(&dir, part, Mode::RWXU) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        dir = openat(&dir, part, DIRECTORY, Mode::empty())?.into();
    }
    Ok(dir)
}

fn input(dir: &File, path: &Path) -> io::Result<File> {
    let parent = subdir(dir, path.parent().ok_or_else(invalid)?, false)?;
    let file = File::from(openat(
        &parent,
        path.file_name().ok_or_else(invalid)?,
        READ,
        Mode::empty(),
    )?);
    if !file.metadata()?.is_file() {
        return Err(invalid());
    }
    Ok(file)
}

fn write_atomic(parent: &File, name: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = format!(".publish-{}", uuid::Uuid::new_v4());
    let flags = OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC;
    let mut file = File::from(openat(
        parent,
        temp.as_str(),
        flags,
        Mode::RUSR | Mode::WUSR,
    )?);
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| Ok(renameat(parent, temp.as_str(), parent, name)?));
    if written.is_err() {
        let _ = unlinkat(parent, temp.as_str(), AtFlags::empty());
    }
    written
}

fn load_files(work: &File, input_spec: &Publication) -> io::Result<BTreeMap<PathBuf, Vec<u8>>> {
    let source = input_spec
        .source
        .strip_prefix(".work")
        .map_err(|_| invalid())?;
    if !valid_path(source) || !valid_purpose(&input_spec.purpose) || input_spec.assets.len() > 256 {
        return Err(invalid());
    }
    let folder = subdir(work, source.parent().ok_or_else(invalid)?, false)?;
    let entry = Path::new(source.file_name().ok_or_else(invalid)?);
    let mut files = BTreeMap::new();
    let mut remaining = MAX_BYTES;
    for path in std::iter::once(entry).chain(input_spec.assets.iter().map(PathBuf::as_path)) {
        if !valid_path(path) || path.iter().count() > super::MAX_DEPTH || files.contains_key(path) {
            return Err(invalid());
        }
        let file = input(&folder, path)?;
        let meta = file.metadata()?;
        if meta.nlink() != 1 || meta.len() > remaining as u64 {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.take(remaining as u64 + 1).read_to_end(&mut bytes)?;
        remaining = remaining.checked_sub(bytes.len()).ok_or_else(invalid)?;
        files.insert(path.to_owned(), bytes);
    }
    Ok(files)
}

fn bundle(files: &BTreeMap<PathBuf, Vec<u8>>, source: &Path) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(source.as_os_str().as_encoded_bytes());
    digest.update([0]);
    for (path, bytes) in files {
        digest.update(path.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    let mut name = String::from("review-");
    for byte in digest.finalize() {
        write!(name, "{byte:02x}").expect("writing to a String");
    }
    PathBuf::from(name)
}

fn index_bytes(index: &Index) -> io::Result<Vec<u8>> {
    let bytes = serde_json::to_vec_pretty(&index.values().collect::<Vec<_>>())?;
    if bytes.len() as u64 > MAX_INDEX_BYTES || index.len() > MAX_ENTRIES {
        return Err(invalid());
    }
    Ok(bytes)
}

/// Publish a review to an authenticated, daemon-resolved session worktree.
/// Each version has immutable relative asset paths. Repeating the same content
/// updates its purpose in place and retains all other registered entries.
///
/// # Errors
/// Rejects paths outside the current worktree, invalid metadata, conflicting
/// entries, excessive inputs and I/O failures.
pub fn publish(workspace: &Path, session: &Path, spec: Publication) -> io::Result<PathBuf> {
    let relative = session.strip_prefix(workspace).map_err(|_| invalid())?;
    if !valid_path(relative) {
        return Err(invalid());
    }
    let pinned = super::open_relative(workspace, relative)?;
    let work = subdir(&pinned, Path::new(".work"), false)?;
    let files = load_files(&work, &spec)?;
    let output = subdir(&pinned, Path::new(ARTIFACTS_DIR), false)?;
    fs2::FileExt::try_lock_exclusive(&output)?;
    let mut index: Index = match input(&output, Path::new(ARTIFACT_INDEX)) {
        Ok(mut file) => read_index(&mut file)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let existing = directory_files(&output)?
                .into_iter()
                .filter(|path| displayable_path(path) && legacy_deliverable(path))
                .map(|path| (path.clone(), Entry::Path(path)))
                .collect();
            // Freeze the legacy listing before generating files under this root.
            // Readers must never see an unfinished review through the fallback.
            write_atomic(&output, Path::new(ARTIFACT_INDEX), &index_bytes(&existing)?)?;
            existing
        }
        Err(error) => return Err(error),
    };
    let dirname = bundle(&files, &spec.source);
    let entry = dirname.join(spec.source.file_name().ok_or_else(invalid)?);
    index.retain(
        |_, item| !matches!(item, Entry::Review(r) if r.source.as_ref() == Some(&spec.source)),
    );
    index.insert(
        entry.clone(),
        Entry::Review(Review {
            path: entry.clone(),
            source: Some(spec.source),
            purpose: spec.purpose,
            assets: spec.assets,
        }),
    );
    let bytes = index_bytes(&index)?;
    let directory = subdir(&output, &dirname, true)?;
    for (path, content) in files {
        let parent = subdir(&directory, path.parent().ok_or_else(invalid)?, true)?;
        write_atomic(
            &parent,
            Path::new(path.file_name().ok_or_else(invalid)?),
            &content,
        )?;
        parent.sync_all()?;
    }
    directory.sync_all()?;
    write_atomic(&output, Path::new(ARTIFACT_INDEX), &bytes)?;
    output.sync_all()?;
    Ok(session.join(ARTIFACTS_DIR).join(entry))
}

#[cfg(test)]
mod tests {
    use super::super::{archive, list_reviews, prepare_session, review_assets};
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn spec(assets: &[&str]) -> Publication {
        Publication {
            source: ".work/spec.md".into(),
            purpose: "Confirm the API".into(),
            assets: assets.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn publishes_only_explicit_files_and_replays_without_duplicate_entries() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join(".usagi/sessions/s");
        fs::create_dir_all(&session).unwrap();
        prepare_session(&session).unwrap();
        assert!(archive(&session).unwrap().is_none());
        let output = session.join(ARTIFACTS_DIR);
        let work = session.join(".work");
        fs::create_dir(work.join("assets")).unwrap();
        fs::write(work.join("assets/chart.png"), "chart").unwrap();
        fs::write(work.join("debug.log"), "log").unwrap();
        fs::write(work.join("spec.md"), "![chart](assets/chart.png)").unwrap();
        fs::write(output.join("stray.txt"), "hidden").unwrap();
        assert!(list_reviews(&session, false).unwrap().is_empty());
        // Also exercise publication to an existing legacy session.
        fs::remove_file(output.join(ARTIFACT_INDEX)).unwrap();
        let path = publish(root.path(), &session, spec(&["assets/chart.png"])).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "![chart](assets/chart.png)"
        );
        assert!(path.with_file_name("assets/chart.png").is_file());
        assert!(!path.with_file_name("debug.log").exists());
        let mut retry = spec(&["assets/chart.png"]);
        retry.purpose = "Review the chart".into();
        assert_eq!(publish(root.path(), &session, retry).unwrap(), path);
        let listed = list_reviews(&session, false).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed[path.strip_prefix(&session).unwrap().to_str().unwrap()].as_deref(),
            Some("Review the chart")
        );
        let rel = path.strip_prefix(&output).unwrap();
        let snapshot = archive(&session).unwrap().unwrap();
        assert_eq!(
            review_assets(&snapshot, rel).unwrap().unwrap(),
            [rel.with_file_name("assets/chart.png")]
        );
        assert!(!snapshot.join("debug.log").exists());
        fs::write(work.join("spec.md"), "revised spec").unwrap();
        let revision = publish(root.path(), &session, spec(&[])).unwrap();
        assert_ne!(revision, path);
        assert_eq!(list_reviews(&session, false).unwrap().len(), 2);
        assert!(path.is_file());
    }

    #[test]
    fn refuses_out_of_scope_sources_and_preserves_previous_entries_on_write_errors() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session");
        fs::create_dir(&session).unwrap();
        prepare_session(&session).unwrap();
        let work = session.join(".work");
        fs::write(work.join("spec.md"), "proposal").unwrap();
        let mut bad = spec(&[]);
        bad.source = "other.md".into();
        assert!(publish(root.path(), &session, bad).is_err());
        assert!(publish(&session, root.path(), spec(&[])).is_err());
        let path = publish(root.path(), &session, spec(&[])).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(publish(root.path(), &session, spec(&[])).is_err());
        let output = session.join(ARTIFACTS_DIR);
        let full = (0..MAX_ENTRIES)
            .map(|i| format!("review-{i}"))
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&full).unwrap();
        fs::write(output.join(ARTIFACT_INDEX), &bytes).unwrap();
        assert!(publish(root.path(), &session, spec(&[])).is_err());
        assert_eq!(fs::read(output.join(ARTIFACT_INDEX)).unwrap(), bytes);
        assert!(review_assets(&output, Path::new("old")).unwrap().is_none());
    }

    #[test]
    fn rejects_invalid_or_oversized_inputs_and_preserves_the_index_on_failure() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session");
        fs::create_dir(&session).unwrap();
        prepare_session(&session).unwrap();
        let work = session.join(".work");
        fs::write(work.join("spec.md"), "document").unwrap();
        let huge = File::create(work.join("big")).unwrap();
        huge.set_len(MAX_BYTES as u64 + 1).unwrap();
        symlink(".", work.join("link")).unwrap();
        for request in [
            spec(&["spec.md"]),
            spec(&["big"]),
            spec(&["link/file"]),
            spec(&["../other"]),
            spec(&["missing"]),
        ] {
            assert!(publish(root.path(), &session, request).is_err());
        }
        let mut request = spec(&[]);
        request.purpose.clear();
        assert!(publish(root.path(), &session, request).is_err());
        fs::hard_link(work.join("spec.md"), work.join("alias")).unwrap();
        assert!(publish(root.path(), &session, spec(&[])).is_err());
        fs::remove_file(work.join("alias")).unwrap();
        assert!(publish(root.path(), root.path(), spec(&[])).is_err());
        let output = session.join(ARTIFACTS_DIR);
        assert_eq!(
            fs::read_to_string(output.join(ARTIFACT_INDEX)).unwrap(),
            "[]\n"
        );
        let output_fd = File::open(&output).unwrap();
        fs::create_dir(output.join("occupied")).unwrap();
        assert!(write_atomic(&output_fd, Path::new("occupied"), b"data").is_err());
        let regular = File::open(work.join("spec.md")).unwrap();
        assert!(subdir(&regular, Path::new("child"), true).is_err());
        fs::write(output.join(ARTIFACT_INDEX), "bad index").unwrap();
        assert!(publish(root.path(), &session, spec(&[])).is_err());
        fs::remove_file(output.join(ARTIFACT_INDEX)).unwrap();
        fs::create_dir(output.join(ARTIFACT_INDEX)).unwrap();
        assert!(publish(root.path(), &session, spec(&[])).is_err());
    }
}
