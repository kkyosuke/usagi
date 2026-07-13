//! Persistent error logging with daily rotation and retention.
//!
//! Runtime errors that bubble up to `main` would otherwise only reach stderr
//! and vanish. This module appends them to a per-day file under
//! `<data dir>/logs/` (`error-YYYY-MM-DD.log`) and prunes files older than
//! [`RETENTION_DAYS`], so roughly a month of failures stays inspectable
//! without the directory growing without bound.
//!
//! The markdown stores route their "skipping a corrupt sibling" notices here via
//! [`ErrorLog::record`] so a lenient scan leaves an inspectable trail.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate};

use crate::infrastructure::paths::data_dir;

/// Whether this process has already pruned old log files. Pruning scans the
/// whole `logs/` directory, so [`ErrorLog::record`] does it once per process
/// rather than on every error — an error burst would otherwise re-scan the
/// directory on each line.
static PRUNED: AtomicBool = AtomicBool::new(false);

/// Subdirectory of the data directory that holds the daily log files.
const LOGS_DIR_NAME: &str = "logs";
const FILE_PREFIX: &str = "error-";
const FILE_SUFFIX: &str = ".log";
const DATE_FORMAT: &str = "%Y-%m-%d";

/// How many days of daily log files are kept; older files are pruned.
pub const RETENTION_DAYS: i64 = 30;

/// Append-only error log rooted at a `logs/` directory.
pub struct ErrorLog {
    dir: PathBuf,
}

impl ErrorLog {
    /// Open the log under the default data directory (`<data dir>/logs/`).
    ///
    /// # Errors
    ///
    /// Returns an error when the data directory cannot be determined.
    #[coverage(off)]
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(data_dir()?.join(LOGS_DIR_NAME)))
    }

    /// Open the log rooted at an explicit directory (mainly for tests).
    #[must_use]
    #[coverage(off)]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    #[must_use]
    #[coverage(off)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Best-effort: append `message` to today's log file under the default data
    /// directory and, once per process, prune files older than [`RETENTION_DAYS`].
    /// Every failure — including not finding the data directory — is swallowed, so
    /// logging never masks the error it is recording.
    ///
    /// This is the entry point for code paths that cannot surface a `Result` to
    /// `main`, such as the markdown stores' lenient scan skipping a corrupt file:
    /// routing those here keeps every failure inspectable in `<data dir>/logs/`.
    #[coverage(off)]
    pub fn record(message: &str) {
        // `if let` (not `let … else { return }`) so the data-dir-not-found case
        // is just "skip the block" rather than its own statement: there is no
        // way to make `open_default` fail under test, and an unreachable early
        // return would leave a line uncovered.
        if let Ok(log) = Self::open_default() {
            let now = Local::now();
            log.prune_once(now.date_naive());
            let _ = log.append(now, message);
        }
    }

    /// Prune old log files at most once per process. Pruning scans the whole
    /// `logs/` directory, so an error burst would otherwise re-scan it on every
    /// line; the first call wins the swap and prunes, later calls skip.
    #[coverage(off)]
    fn prune_once(&self, today: NaiveDate) {
        if !PRUNED.swap(true, Ordering::Relaxed) {
            let _ = self.prune(today, RETENTION_DAYS);
        }
    }

    #[coverage(off)]
    fn file_for(&self, date: NaiveDate) -> PathBuf {
        self.dir.join(format!(
            "{FILE_PREFIX}{}{FILE_SUFFIX}",
            date.format(DATE_FORMAT)
        ))
    }

    /// Append an error entry timestamped `now` to that day's log file,
    /// creating the directory and file if needed. Multi-line messages are
    /// indented so each entry stays visually grouped under its timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when the log directory cannot be created or the day's log
    /// file cannot be opened or appended to.
    #[coverage(off)]
    pub fn append(&self, now: DateTime<Local>, message: &str) -> Result<()> {
        fs::create_dir_all(&self.dir).context(format!(
            "failed to create log directory {}",
            self.dir.display()
        ))?;
        let path = self.file_for(now.date_naive());
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context(format!("failed to open log file {}", path.display()))?;
        let body = message.replace('\n', "\n    ");
        // Build the whole entry (timestamp + body + newline) as one buffer and
        // emit it in a single `write_all`. writeln! on an unbuffered File issues a
        // separate write syscall per format fragment, and O_APPEND only makes each
        // individual write atomic — so a concurrent process appending to the same
        // daily log (the TUI, a `usagi mcp` server) could interleave its fragments
        // inside this entry's line. One write_all keeps each entry on its own line.
        let line = format!("[{}] {body}\n", now.format("%Y-%m-%d %H:%M:%S"));
        file.write_all(line.as_bytes())
            .context(format!("failed to write log file {}", path.display()))?;
        Ok(())
    }

    /// Delete daily log files whose date is older than `retention_days` before
    /// `today`. Returns how many files were removed; a missing directory is
    /// treated as "nothing to prune".
    ///
    /// # Errors
    ///
    /// Returns an error when the log directory exists but cannot be read, or a
    /// stale file cannot be removed.
    #[coverage(off)]
    pub fn prune(&self, today: NaiveDate, retention_days: i64) -> Result<usize> {
        if !self.dir.exists() {
            return Ok(0);
        }
        let cutoff = today - chrono::Duration::days(retention_days);
        let mut removed = 0;
        let entries = fs::read_dir(&self.dir).context(format!(
            "failed to read log directory {}",
            self.dir.display()
        ))?;
        for entry in entries {
            let entry =
                entry.context(format!("failed to read an entry in {}", self.dir.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(date) = parse_date(&name) else {
                continue;
            };
            if date < cutoff {
                fs::remove_file(entry.path())
                    .context(format!("failed to remove {}", entry.path().display()))?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Parse the date out of an `error-YYYY-MM-DD.log` file name, ignoring any
/// other file the directory may contain.
#[coverage(off)]
fn parse_date(name: &str) -> Option<NaiveDate> {
    let core = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    NaiveDate::parse_from_str(core, DATE_FORMAT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;

    #[coverage(off)]
    fn at(year: i32, month: u32, day: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, 10, 30, 0)
            .single()
            .expect("valid local timestamp")
    }

    #[coverage(off)]
    fn temp_log() -> (tempfile::TempDir, ErrorLog) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let log = ErrorLog::new(dir.path().join("logs"));
        (dir, log)
    }

    #[test]
    #[coverage(off)]
    fn append_creates_the_dated_file_and_records_the_message() {
        let (_dir, log) = temp_log();
        log.append(at(2026, 6, 16), "boom").unwrap();

        let path = log.dir().join("error-2026-06-16.log");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "[2026-06-16 10:30:00] boom\n");
    }

    #[test]
    #[coverage(off)]
    fn append_groups_multi_line_messages_under_one_timestamp() {
        let (_dir, log) = temp_log();
        log.append(at(2026, 6, 16), "boom\ncaused by: io").unwrap();

        let contents = fs::read_to_string(log.dir().join("error-2026-06-16.log")).unwrap();
        assert_eq!(contents, "[2026-06-16 10:30:00] boom\n    caused by: io\n");
    }

    #[test]
    #[coverage(off)]
    fn append_keeps_adding_entries_to_the_same_day() {
        let (_dir, log) = temp_log();
        log.append(at(2026, 6, 16), "first").unwrap();
        log.append(at(2026, 6, 16), "second").unwrap();

        let contents = fs::read_to_string(log.dir().join("error-2026-06-16.log")).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }

    #[test]
    #[coverage(off)]
    fn append_reports_an_error_when_the_directory_cannot_be_created() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // A file where the logs directory should be makes create_dir_all fail.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let log = ErrorLog::new(blocker.join("logs"));
        assert!(log.append(at(2026, 6, 16), "boom").is_err());
    }

    #[test]
    #[coverage(off)]
    fn prune_on_a_missing_directory_removes_nothing() {
        let (_dir, log) = temp_log();
        assert_eq!(
            log.prune(at(2026, 6, 16).date_naive(), RETENTION_DAYS)
                .unwrap(),
            0
        );
    }

    #[test]
    #[coverage(off)]
    fn prune_removes_only_files_older_than_the_retention_window() {
        let (_dir, log) = temp_log();
        let today = at(2026, 6, 16);
        // Old enough to drop, exactly on the cutoff (kept), and current.
        log.append(at(2026, 4, 1), "old").unwrap();
        log.append(at(2026, 5, 17), "edge").unwrap();
        log.append(today, "fresh").unwrap();
        // An unrelated file is ignored rather than removed.
        fs::write(log.dir().join("notes.txt"), "keep me").unwrap();

        let removed = log.prune(today.date_naive(), RETENTION_DAYS).unwrap();
        assert_eq!(removed, 1);
        assert!(!log.dir().join("error-2026-04-01.log").exists());
        assert!(log.dir().join("error-2026-05-17.log").exists());
        assert!(log.dir().join("error-2026-06-16.log").exists());
        assert!(log.dir().join("notes.txt").exists());
    }

    #[test]
    #[coverage(off)]
    fn prune_reports_an_error_when_the_log_path_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // A file at the logs path exists() but cannot be read as a directory.
        let path = dir.path().join("logs");
        fs::write(&path, "i am a file").unwrap();
        let log = ErrorLog::new(&path);
        assert!(
            log.prune(at(2026, 6, 16).date_naive(), RETENTION_DAYS)
                .is_err()
        );
    }

    #[test]
    #[coverage(off)]
    fn parse_date_only_accepts_well_formed_log_names() {
        assert_eq!(
            parse_date("error-2026-06-16.log"),
            Some(NaiveDate::from_ymd_opt(2026, 6, 16).unwrap())
        );
        assert_eq!(parse_date("error-not-a-date.log"), None);
        assert_eq!(parse_date("error-2026-06-16.txt"), None);
        assert_eq!(parse_date("workspaces.json"), None);
    }

    #[test]
    #[coverage(off)]
    fn record_writes_an_entry_under_the_data_directory() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().expect("failed to create temp dir");
        unsafe {
            std::env::set_var(crate::infrastructure::paths::DATA_DIR_ENV, home.path());
        }

        ErrorLog::record("session create \"c\" failed: boom");

        let logs = home.path().join("logs");
        let entry = fs::read_dir(&logs)
            .expect("logs dir exists")
            .next()
            .expect("a log file was written")
            .expect("readable entry");
        let contents = fs::read_to_string(entry.path()).unwrap();
        assert!(contents.contains("session create \"c\" failed: boom"));

        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }

    #[test]
    #[coverage(off)]
    fn open_default_roots_the_log_under_the_data_directory() {
        let _guard = crate::test_support::process_env_guard();
        unsafe {
            std::env::set_var(
                crate::infrastructure::paths::DATA_DIR_ENV,
                "/tmp/usagi-log-home",
            );
        }
        let log = ErrorLog::open_default().unwrap();
        assert_eq!(log.dir(), Path::new("/tmp/usagi-log-home/logs"));
        unsafe {
            std::env::remove_var(crate::infrastructure::paths::DATA_DIR_ENV);
        }
    }
}
