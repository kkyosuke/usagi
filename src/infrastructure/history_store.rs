//! Persistence for a single repository's command history.
//!
//! Every command run in the workspace screen is appended to
//! `<repo>/.usagi/history.jsonl`, next to the `state.json` that describes the
//! same repository. The file is **append-only JSONL**: each line is one
//! [`HistoryEntry`] serialized as JSON, written with
//! `OpenOptions::append`. A read-modify-write of a single JSON document would
//! lose entries when two writers (two TUI panes, or the TUI plus a command run)
//! both read N entries and each write back N+1; appending one line per entry
//! lets concurrent writers each add their own line without clobbering the
//! other. A POSIX append write of a short line is atomic, so the file never
//! interleaves two entries, and the reader tolerates a torn trailing line just
//! in case.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::domain::history::HistoryEntry;
use crate::infrastructure::repo_paths::STATE_DIR;

const HISTORY_FILE: &str = "history.jsonl";

/// The most entries [`HistoryStore::load`] returns — the newest ones. The file is
/// append-only and grows over a repository's whole lifetime; loading only the tail
/// bounds the startup parse cost and the in-memory buffer that seeds the screen's
/// command recall, however large the file has become.
const MAX_LOADED_ENTRIES: usize = 1_000;

/// The most bytes [`HistoryStore::load`] reads off the end of the history file.
/// The file is append-only and unbounded, so reading it whole would grow the
/// startup read with the repository's entire command history. Only the tail is
/// needed (the last [`MAX_LOADED_ENTRIES`] entries), so a larger file is read from
/// this offset before its end instead of in full. Sized well above
/// [`MAX_LOADED_ENTRIES`] typical JSONL entries (a command string + an RFC3339
/// timestamp, ~100–300 bytes each) so the tail almost always still holds the full
/// [`MAX_LOADED_ENTRIES`]; only pathologically long commands would yield fewer.
const TAIL_READ_BYTES: u64 = 1 << 20;

/// File-based persistence for a repository's command history, rooted at its
/// `.usagi/` directory.
pub struct HistoryStore {
    dir: PathBuf,
}

impl HistoryStore {
    /// Open the store for the repository whose primary worktree is `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            dir: repo_root.as_ref().join(STATE_DIR),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn history_path(&self) -> PathBuf {
        self.dir.join(HISTORY_FILE)
    }

    /// Load the recorded history, oldest first. Returns an empty vector if the
    /// file has never been written.
    ///
    /// Each line is one JSON-encoded [`HistoryEntry`]. A blank line is skipped,
    /// and a trailing line without a terminating newline is tolerated as a
    /// possibly-incomplete write and dropped rather than treated as corruption,
    /// so a concurrent append in flight never makes a read fail. Any *complete*
    /// line that is not valid JSON is a real corruption and surfaces as an
    /// error.
    pub fn load(&self) -> Result<Vec<HistoryEntry>> {
        let path = self.history_path();
        let text = match self.read_tail(&path) {
            Ok(Some(text)) => text,
            Ok(None) => return Ok(Vec::new()),
            Err(e) => return Err(e).context(format!("failed to read {}", path.display())),
        };

        // A trailing line not terminated by '\n' may be a half-written append
        // still in flight; drop it instead of failing the whole read.
        let ends_with_newline = text.ends_with('\n');
        let mut lines: Vec<&str> = text.lines().collect();
        if !ends_with_newline {
            lines.pop();
        }
        // Keep only the most recent entries: an append-only history grows without
        // bound on disk, but the screen only needs the tail for recall, and a
        // complete line earlier in the file is never re-validated here.
        let skip = lines.len().saturating_sub(MAX_LOADED_ENTRIES);
        if skip > 0 {
            lines.drain(..skip);
        }

        let mut entries = Vec::with_capacity(lines.len());
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let entry: HistoryEntry = serde_json::from_str(line).context(format!(
                "failed to parse a history entry in {}",
                path.display()
            ))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Read the history file's text for [`load`](Self::load), bounded to its last
    /// [`TAIL_READ_BYTES`]. Returns `None` when the file has never been written.
    ///
    /// A file within the cap is read whole (the common case). A larger one is read
    /// from [`TAIL_READ_BYTES`] before its end, and everything up to and including
    /// the first newline is dropped: that leading line began mid-record (the window
    /// opened partway through it), and dropping it also lands the remaining bytes on
    /// a line boundary — so the tail is valid UTF-8 (each complete line is valid
    /// JSON). `load` still trims to the last [`MAX_LOADED_ENTRIES`], so reading a
    /// generous byte window and then bounding by entry count both hold. This keeps
    /// the startup read bounded no matter how large the append-only file has grown.
    fn read_tail(&self, path: &Path) -> std::io::Result<Option<String>> {
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let len = file.metadata()?.len();
        if len <= TAIL_READ_BYTES {
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            return Ok(Some(text));
        }
        file.seek(SeekFrom::Start(len - TAIL_READ_BYTES))?;
        let mut bytes = Vec::with_capacity(TAIL_READ_BYTES as usize);
        file.read_to_end(&mut bytes)?;
        // Drop the partial first line (and any torn leading UTF-8 with it).
        let start = bytes
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |nl| nl + 1);
        Ok(Some(String::from_utf8_lossy(&bytes[start..]).into_owned()))
    }

    /// Append a single executed `command` to the history, stamped with the
    /// current time. Writes one JSON line with `O_APPEND`, so concurrent
    /// appends each add their own line without a full-file read-modify-write
    /// and never lose each other's entry.
    pub fn append(&self, command: impl Into<String>) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .context(format!("failed to create {}", self.dir.display()))?;
        let path = self.history_path();
        let mut line = serde_json::to_string(&HistoryEntry::now(command))?;
        line.push('\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context(format!("failed to open {}", path.display()))?;
        file.write_all(line.as_bytes())
            .context(format!("failed to append to {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn load_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn append_accumulates_entries_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());

        store.append("man").unwrap();
        store.append("doctor").unwrap();

        let entries = store.load().unwrap();
        let commands: Vec<&str> = entries.iter().map(|e| e.command.as_str()).collect();
        assert_eq!(commands, vec!["man", "doctor"]);
        assert!(store.history_path().exists());
    }

    #[test]
    fn saved_file_is_one_json_line_per_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        store.append("man").unwrap();
        store.append("doctor").unwrap();

        let text = fs::read_to_string(store.history_path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line round-trips on its own.
        for line in lines {
            let entry: HistoryEntry = serde_json::from_str(line).unwrap();
            assert!(!entry.command.is_empty());
        }
    }

    #[test]
    fn concurrent_appends_do_not_lose_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HistoryStore::new(dir.path()));
        // Create the directory up front so both threads only race on the file.
        fs::create_dir_all(store.dir()).unwrap();

        let per_thread = 50;
        let mut handles = Vec::new();
        for t in 0..2 {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                for i in 0..per_thread {
                    store.append(format!("cmd-{t}-{i}")).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let entries = store.load().unwrap();
        // No entry from either thread is silently lost.
        assert_eq!(entries.len(), 2 * per_thread);
        for t in 0..2 {
            for i in 0..per_thread {
                let want = format!("cmd-{t}-{i}");
                assert!(
                    entries.iter().any(|e| e.command == want),
                    "missing entry {want}"
                );
            }
        }
    }

    #[test]
    fn load_keeps_only_the_most_recent_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        // Write more lines than the cap directly, so the file is "large".
        let total = MAX_LOADED_ENTRIES + 5;
        let mut text = String::new();
        for i in 0..total {
            let line = serde_json::to_string(&HistoryEntry::now(format!("cmd-{i}"))).unwrap();
            text.push_str(&line);
            text.push('\n');
        }
        fs::write(store.history_path(), text).unwrap();

        let entries = store.load().unwrap();
        // Only the tail is loaded, newest preserved and in order.
        assert_eq!(entries.len(), MAX_LOADED_ENTRIES);
        assert_eq!(entries.first().unwrap().command, "cmd-5");
        assert_eq!(
            entries.last().unwrap().command,
            format!("cmd-{}", total - 1)
        );
    }

    #[test]
    fn load_reads_only_the_tail_of_a_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        // Build a file well over TAIL_READ_BYTES with padded commands, so the byte
        // window — not the entry cap — is what truncates the read.
        let total = 700usize;
        let mut text = String::new();
        for i in 0..total {
            let command = format!("cmd-{i:04}-{}", "x".repeat(2000));
            text.push_str(&serde_json::to_string(&HistoryEntry::now(command)).unwrap());
            text.push('\n');
        }
        assert!(
            text.len() as u64 > TAIL_READ_BYTES,
            "the test file must exceed the tail window"
        );
        fs::write(store.history_path(), &text).unwrap();

        let entries = store.load().unwrap();
        // The window kept only the newest run — contiguous, ending at the last
        // appended command, with the oldest entries left unread.
        let index = |e: &HistoryEntry| {
            e.command
                .split('-')
                .nth(1)
                .unwrap()
                .parse::<usize>()
                .unwrap()
        };
        assert!(!entries.is_empty());
        assert!(
            entries.len() < total,
            "entries older than the byte window are dropped"
        );
        assert_eq!(index(entries.last().unwrap()), total - 1);
        assert!(
            index(entries.first().unwrap()) > 0,
            "the oldest entries are not read"
        );
        for pair in entries.windows(2) {
            assert_eq!(
                index(&pair[1]),
                index(&pair[0]) + 1,
                "entries stay contiguous"
            );
        }
    }

    #[test]
    fn load_of_a_huge_unterminated_line_yields_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        // A single blob larger than the tail window with no newline: the tail read
        // finds no line boundary, so nothing complete is parsed (rather than a torn
        // fragment being mis-parsed).
        let blob = "x".repeat(TAIL_READ_BYTES as usize + 4096);
        fs::write(store.history_path(), blob).unwrap();
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn dir_points_at_the_usagi_subdirectory() {
        let store = HistoryStore::new("/repo");
        assert_eq!(store.dir(), Path::new("/repo/.usagi"));
        assert_eq!(
            store.history_path(),
            PathBuf::from("/repo/.usagi/history.jsonl")
        );
    }

    #[test]
    fn load_skips_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        let entry = serde_json::to_string(&HistoryEntry::now("man")).unwrap();
        // Blank lines around a real entry are ignored.
        fs::write(store.history_path(), format!("\n{entry}\n\n")).unwrap();

        let entries = store.load().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "man");
    }

    #[test]
    fn load_tolerates_a_torn_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        let good = serde_json::to_string(&HistoryEntry::now("man")).unwrap();
        // A complete first line, then a half-written second line with no
        // trailing newline (an append caught mid-flight). The partial line is
        // dropped rather than failing the read.
        fs::write(store.history_path(), format!("{good}\n{{\"command\":\"do")).unwrap();

        let entries = store.load().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "man");
    }

    #[test]
    fn load_errors_on_a_corrupt_complete_line() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        fs::create_dir_all(store.dir()).unwrap();
        // A complete line (newline-terminated) that is not valid JSON is real
        // corruption, not a torn write.
        fs::write(store.history_path(), "{ not json\n").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn load_errors_when_history_path_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        // Make history.jsonl a directory so reading it fails with a non-NotFound error.
        fs::create_dir_all(store.history_path()).unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn load_errors_when_the_history_path_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        // A regular file where the `.usagi/` directory should be makes opening
        // `.usagi/history.jsonl` fail with a non-NotFound error (ENOTDIR), which
        // must surface rather than read as "no history yet".
        fs::write(store.dir(), "not a directory").unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn append_errors_when_the_directory_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the `.usagi` directory should be makes create_dir_all fail.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let store = HistoryStore::new(&blocker);
        assert!(store.append("man").is_err());
    }

    #[test]
    fn append_errors_when_the_history_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::new(dir.path());
        // A directory at history.jsonl makes the append open() fail.
        fs::create_dir_all(store.history_path()).unwrap();
        assert!(store.append("man").is_err());
    }
}
