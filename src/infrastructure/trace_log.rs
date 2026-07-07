//! Persistent operation trace logging with daily rotation and retention.
//!
//! Where [`crate::infrastructure::error_log`] records only failures, this module
//! records a structured **trace** of operations — CLI commands, TUI key presses,
//! session create / remove, MCP tool calls — so a whole session's activity can be
//! analysed after the fact. Each [`TraceEvent`] is appended as one JSON line to a
//! per-day file under `<data dir>/logs/` (`trace-YYYY-MM-DD.jsonl`), next to the
//! daily error log, and files older than [`RETENTION_DAYS`] are pruned.
//!
//! **Append-only JSONL**, exactly like the command history
//! ([`crate::infrastructure::history_store`]): each line is one event written
//! with `O_APPEND`, so concurrent writers (TUI panes, an `usagi mcp` process)
//! each add their own line without a read-modify-write that could drop another's.
//!
//! **Opt-in.** Tracing sits on hot paths (every key press, every MCP call), so it
//! is off unless [`is_enabled`] returns true — the `USAGI_TRACE` environment
//! variable is set to a non-empty value other than `0`. While disabled,
//! [`TraceLog::record`] returns after a single env lookup and touches no disk;
//! a hot caller that would build an expensive event uses
//! [`TraceLog::record_with`], which defers constructing it until past that gate.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate};

use crate::domain::trace::TraceEvent;
use crate::infrastructure::storage::data_dir;

/// Environment variable that turns operation tracing on. Set it to a non-empty
/// value other than `0` to enable; unset / empty / `0` keeps tracing off.
pub const TRACE_ENV: &str = "USAGI_TRACE";

/// Whether this process has already pruned old trace files. Pruning scans the
/// whole `logs/` directory, so [`TraceLog::record`] does it once per process
/// rather than on every event — a busy TUI would otherwise re-scan the directory
/// on every key press.
static PRUNED: AtomicBool = AtomicBool::new(false);

/// Subdirectory of the data directory that holds the daily trace files.
const LOGS_DIR_NAME: &str = "logs";
const FILE_PREFIX: &str = "trace-";
const FILE_SUFFIX: &str = ".jsonl";
const DATE_FORMAT: &str = "%Y-%m-%d";

/// How many days of daily trace files are kept; older files are pruned.
pub const RETENTION_DAYS: i64 = 30;

/// Whether operation tracing is enabled, read from [`TRACE_ENV`]. Enabled when
/// the variable is set to a non-empty value other than `0`.
pub fn is_enabled() -> bool {
    match std::env::var_os(TRACE_ENV) {
        Some(value) => !value.is_empty() && value != OsStr::new("0"),
        None => false,
    }
}

/// Append-only trace log rooted at a `logs/` directory.
pub struct TraceLog {
    dir: PathBuf,
}

impl TraceLog {
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

    /// Best-effort: record `event` to today's trace file under the default data
    /// directory and, once per process, prune files older than [`RETENTION_DAYS`].
    /// A no-op when tracing is disabled ([`is_enabled`]). Every failure — including
    /// not finding the data directory — is swallowed, so tracing never disrupts
    /// the operation it is recording.
    ///
    /// This is the single entry point every traced surface calls (the CLI
    /// dispatcher in `main`, the home screen's event loop, session create /
    /// remove, the MCP tool dispatcher), mirroring
    /// [`crate::infrastructure::error_log::ErrorLog::record`] so no caller has to
    /// thread a logger through its signature.
    pub fn record(event: TraceEvent) {
        if !is_enabled() {
            return;
        }
        Self::emit(&event);
    }

    /// Like [`record`](Self::record), but the event is built by `build` only
    /// *after* the [`is_enabled`] gate passes. On a hot path that constructs an
    /// expensive event — the home screen traces every key press with a
    /// `format!`ed `{mode} {key}` detail — this keeps the timestamp, the
    /// allocation, and the `format!` from running at all while tracing is off (the
    /// default), so the "costs nothing unless enabled" promise holds at the call
    /// site rather than only inside this function. Cold callers whose event is
    /// cheap to build unconditionally (the CLI / session / MCP paths) stay on
    /// [`record`].
    pub fn record_with(build: impl FnOnce() -> TraceEvent) {
        if !is_enabled() {
            return;
        }
        Self::emit(&build());
    }

    /// Append `event` to today's trace file and prune old files once per process.
    /// Callers reach this only past the [`is_enabled`] gate.
    fn emit(event: &TraceEvent) {
        // `if let` (not `let … else { return }`) so the data-dir-not-found case is
        // just "skip the block": there is no way to make `open_default` fail under
        // test, and an unreachable early return would leave a line uncovered.
        if let Ok(log) = Self::open_default() {
            let now = Local::now();
            log.prune_once(now.date_naive());
            let _ = log.append(now, event);
        }
    }

    /// Prune old trace files at most once per process (see [`PRUNED`]).
    fn prune_once(&self, today: NaiveDate) {
        if !PRUNED.swap(true, Ordering::Relaxed) {
            let _ = self.prune(today, RETENTION_DAYS);
        }
    }

    fn file_for(&self, date: NaiveDate) -> PathBuf {
        self.dir.join(format!(
            "{FILE_PREFIX}{}{FILE_SUFFIX}",
            date.format(DATE_FORMAT)
        ))
    }

    /// Append `event` as one JSON line to the day's trace file (dated by `now`),
    /// creating the directory and file if needed. The whole line — JSON plus its
    /// terminating newline — is written in a single `O_APPEND` call, so the file
    /// never interleaves two events.
    pub fn append(&self, now: DateTime<Local>, event: &TraceEvent) -> Result<()> {
        fs::create_dir_all(&self.dir).context(format!(
            "failed to create log directory {}",
            self.dir.display()
        ))?;
        let path = self.file_for(now.date_naive());
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context(format!("failed to open trace file {}", path.display()))?;
        file.write_all(line.as_bytes())
            .context(format!("failed to write trace file {}", path.display()))?;
        Ok(())
    }

    /// Delete daily trace files whose date is older than `retention_days` before
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

/// Parse the date out of a `trace-YYYY-MM-DD.jsonl` file name, ignoring any other
/// file the directory may contain (e.g. the sibling `error-*.log`).
fn parse_date(name: &str) -> Option<NaiveDate> {
    let core = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    NaiveDate::parse_from_str(core, DATE_FORMAT).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::trace::TraceCategory;
    use chrono::TimeZone;
    use std::fs;

    fn at(year: i32, month: u32, day: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, 10, 30, 0)
            .single()
            .expect("valid local timestamp")
    }

    fn temp_log() -> (tempfile::TempDir, TraceLog) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let log = TraceLog::new(dir.path().join("logs"));
        (dir, log)
    }

    fn event(action: &str) -> TraceEvent {
        TraceEvent::now(TraceCategory::Cli, action)
    }

    #[test]
    fn append_writes_one_json_line_to_the_dated_file() {
        let (_dir, log) = temp_log();
        log.append(at(2026, 6, 16), &event("doctor")).unwrap();

        let path = log.dir().join("trace-2026-06-16.jsonl");
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        // The single line round-trips back into the same event.
        let parsed: TraceEvent = serde_json::from_str(contents.trim_end()).unwrap();
        assert_eq!(parsed.action, "doctor");
        assert_eq!(parsed.category, TraceCategory::Cli);
    }

    #[test]
    fn append_accumulates_entries_in_the_same_day() {
        let (_dir, log) = temp_log();
        log.append(at(2026, 6, 16), &event("first")).unwrap();
        log.append(at(2026, 6, 16), &event("second")).unwrap();

        let contents = fs::read_to_string(log.dir().join("trace-2026-06-16.jsonl")).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }

    #[test]
    fn append_reports_an_error_when_the_directory_cannot_be_created() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let log = TraceLog::new(blocker.join("logs"));
        assert!(log.append(at(2026, 6, 16), &event("boom")).is_err());
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
        log.append(at(2026, 4, 1), &event("old")).unwrap();
        log.append(at(2026, 5, 17), &event("edge")).unwrap();
        log.append(today, &event("fresh")).unwrap();
        // The sibling error log and an unrelated file are ignored, not removed.
        fs::write(log.dir().join("error-2026-04-01.log"), "x").unwrap();
        fs::write(log.dir().join("notes.txt"), "keep me").unwrap();

        let removed = log.prune(today.date_naive(), RETENTION_DAYS).unwrap();
        assert_eq!(removed, 1);
        assert!(!log.dir().join("trace-2026-04-01.jsonl").exists());
        assert!(log.dir().join("trace-2026-05-17.jsonl").exists());
        assert!(log.dir().join("trace-2026-06-16.jsonl").exists());
        assert!(log.dir().join("error-2026-04-01.log").exists());
        assert!(log.dir().join("notes.txt").exists());
    }

    #[test]
    fn prune_reports_an_error_when_the_log_path_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let path = dir.path().join("logs");
        fs::write(&path, "i am a file").unwrap();
        let log = TraceLog::new(&path);
        assert!(log
            .prune(at(2026, 6, 16).date_naive(), RETENTION_DAYS)
            .is_err());
    }

    #[test]
    fn parse_date_only_accepts_well_formed_trace_names() {
        assert_eq!(
            parse_date("trace-2026-06-16.jsonl"),
            Some(NaiveDate::from_ymd_opt(2026, 6, 16).unwrap())
        );
        assert_eq!(parse_date("trace-not-a-date.jsonl"), None);
        // The sibling error log is a different prefix/suffix and is not a trace file.
        assert_eq!(parse_date("error-2026-06-16.log"), None);
        assert_eq!(parse_date("trace-2026-06-16.log"), None);
    }

    #[test]
    fn is_enabled_reflects_the_env_var() {
        let _guard = crate::test_support::process_env_guard();

        std::env::remove_var(TRACE_ENV);
        assert!(!is_enabled());
        std::env::set_var(TRACE_ENV, "");
        assert!(!is_enabled());
        std::env::set_var(TRACE_ENV, "0");
        assert!(!is_enabled());
        std::env::set_var(TRACE_ENV, "1");
        assert!(is_enabled());

        std::env::remove_var(TRACE_ENV);
    }

    #[test]
    fn record_is_a_noop_while_tracing_is_disabled() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().expect("failed to create temp dir");
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
        std::env::remove_var(TRACE_ENV);

        TraceLog::record(TraceEvent::now(TraceCategory::Cli, "doctor"));

        // Nothing is written: the disabled guard never touches the logs directory.
        assert!(!home.path().join("logs").exists());

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn record_writes_an_event_under_the_data_directory_when_enabled() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().expect("failed to create temp dir");
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
        std::env::set_var(TRACE_ENV, "1");

        TraceLog::record(TraceEvent::now(TraceCategory::Mcp, "issue_create").with_detail("ok"));

        let logs = home.path().join("logs");
        let entry = fs::read_dir(&logs)
            .expect("logs dir exists")
            .find_map(|e| {
                let path = e.expect("readable entry").path();
                (path.extension().and_then(|x| x.to_str()) == Some("jsonl")).then_some(path)
            })
            .expect("a trace file was written");
        let contents = fs::read_to_string(entry).unwrap();
        assert!(contents.contains("issue_create"), "{contents}");
        assert!(contents.contains("\"mcp\""), "{contents}");

        std::env::remove_var(TRACE_ENV);
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    /// The event the home screen's hot key-press path builds, extracted so both
    /// `record_with` tests pass the *same* builder. The enabled test invokes it
    /// (so its body is covered); the disabled test passes it but — proving the
    /// point of `record_with` — never runs it, so it must not be an inline closure
    /// whose body would then read as uncovered.
    fn key_event() -> TraceEvent {
        TraceEvent::now(TraceCategory::Tui, "key").with_detail("Overview Enter")
    }

    #[test]
    fn record_with_is_a_noop_while_tracing_is_disabled() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().expect("failed to create temp dir");
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
        std::env::remove_var(TRACE_ENV);

        // Tracing off: `record_with` returns before invoking the builder, so the
        // event is never constructed and nothing is written — the logs directory
        // is not even created.
        TraceLog::record_with(key_event);
        assert!(!home.path().join("logs").exists());

        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn record_with_builds_and_writes_when_enabled() {
        let _guard = crate::test_support::process_env_guard();
        let home = tempfile::tempdir().expect("failed to create temp dir");
        std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
        std::env::set_var(TRACE_ENV, "1");

        TraceLog::record_with(key_event);

        let logs = home.path().join("logs");
        let entry = fs::read_dir(&logs)
            .expect("logs dir exists")
            .find_map(|e| {
                let path = e.expect("readable entry").path();
                (path.extension().and_then(|x| x.to_str()) == Some("jsonl")).then_some(path)
            })
            .expect("a trace file was written");
        let contents = fs::read_to_string(entry).unwrap();
        assert!(contents.contains("Overview Enter"), "{contents}");
        assert!(contents.contains("\"tui\""), "{contents}");

        std::env::remove_var(TRACE_ENV);
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }

    #[test]
    fn open_default_roots_the_log_under_the_data_directory() {
        let _guard = crate::test_support::process_env_guard();
        std::env::set_var(
            crate::infrastructure::storage::DATA_DIR_ENV,
            "/tmp/usagi-trace-home",
        );
        let log = TraceLog::open_default().unwrap();
        assert_eq!(log.dir(), Path::new("/tmp/usagi-trace-home/logs"));
        std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
    }
}
