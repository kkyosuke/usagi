//! Persistent error logging with daily rotation and retention.
//!
//! Runtime errors that bubble up to `main` would otherwise only reach stderr
//! and vanish. This module appends them to a per-day file under `<data
//! dir>/logs/` (`error-YYYY-MM-DD.log`) and prunes files older than
//! [`RETENTION_DAYS`], so roughly a month of failures stays inspectable
//! without the directory growing without bound.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate};

use crate::infrastructure::storage::data_dir;

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
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(data_dir()?.join(LOGS_DIR_NAME)))
    }

    /// Open the log rooted at an explicit directory (mainly for tests).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file_for(&self, date: NaiveDate) -> PathBuf {
        self.dir.join(format!(
            "{FILE_PREFIX}{}{FILE_SUFFIX}",
            date.format(DATE_FORMAT)
        ))
    }

    /// Append an error entry timestamped `now` to that day's log file,
    /// creating the directory and file if needed. Multi-line messages are
    /// indented so each entry stays visually grouped under its timestamp.
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
        writeln!(file, "[{}] {body}", now.format("%Y-%m-%d %H:%M:%S"))
            .context(format!("failed to write log file {}", path.display()))?;
        Ok(())
    }

    /// Delete daily log files whose date is older than `retention_days` before
    /// `today`. Returns how many files were removed; a missing directory is
    /// treated as "nothing to prune".
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
fn parse_date(name: &str) -> Option<NaiveDate> {
    let core = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    NaiveDate::parse_from_str(core, DATE_FORMAT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::fs;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, 10, 30, 0)
            .single()
            .expect("valid local timestamp")
    }

    fn temp_log() -> (tempfile::TempDir, ErrorLog) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let log = ErrorLog::new(dir.path().join("logs"));
        (dir, log)
    }

    #[test]
    fn append_creates_the_dated_file_and_records_the_message() {
        let (_dir, log) = temp_log();
        log.append(at(2026, 6, 16), "boom").unwrap();

        let path = log.dir().join("error-2026-06-16.log");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "[2026-06-16 10:30:00] boom\n");
    }

    #[test]
    fn append_groups_multi_line_messages_under_one_timestamp() {
        let (_dir, log) = temp_log();
        log.append(at(2026, 6, 16), "boom\ncaused by: io").unwrap();

        let contents = fs::read_to_string(log.dir().join("error-2026-06-16.log")).unwrap();
        assert_eq!(contents, "[2026-06-16 10:30:00] boom\n    caused by: io\n");
    }

    #[test]
    fn append_keeps_adding_entries_to_the_same_day() {
        let (_dir, log) = temp_log();
        log.append(at(2026, 6, 16), "first").unwrap();
        log.append(at(2026, 6, 16), "second").unwrap();

        let contents = fs::read_to_string(log.dir().join("error-2026-06-16.log")).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }

    #[test]
    fn append_reports_an_error_when_the_directory_cannot_be_created() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // A file where the logs directory should be makes create_dir_all fail.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let log = ErrorLog::new(blocker.join("logs"));
        assert!(log.append(at(2026, 6, 16), "boom").is_err());
    }

    #[test]
    fn prune_on_a_missing_directory_removes_nothing() {
        let (_dir, log) = temp_log();
        assert_eq!(
            log.prune(at(2026, 6, 16).date_naive(), RETENTION_DAYS)
                .unwrap(),
            0
        );
    }

    #[test]
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
    fn prune_reports_an_error_when_the_log_path_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        // A file at the logs path exists() but cannot be read as a directory.
        let path = dir.path().join("logs");
        fs::write(&path, "i am a file").unwrap();
        let log = ErrorLog::new(&path);
        assert!(log
            .prune(at(2026, 6, 16).date_naive(), RETENTION_DAYS)
            .is_err());
    }

    #[test]
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
    fn open_default_roots_the_log_under_the_data_directory() {
        let _guard = crate::test_support::process_env_guard();
        std::env::set_var(
            crate::infrastructure::storage::DATA_DIR_ENV,
            "/tmp/usagi-log-home",
        );
        let log = ErrorLog::open_default().unwrap();
        assert_eq!(log.dir(), Path::new("/tmp/usagi-log-home/logs"));
    }
}
