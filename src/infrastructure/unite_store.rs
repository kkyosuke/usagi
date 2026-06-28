//! Global storage of the most recent 統合(unite) selection — the set of
//! workspaces the user last opened together on the Open screen.
//!
//! Where [`super::resume_focus_store`] records *where in one workspace* the user
//! was, this records *which workspaces* they viewed together, so the next visit
//! to the Open screen can pre-check them (`Space`-toggled) and the user can
//! re-open the same union with one `Enter`. It is a single machine-wide fact, not
//! per-workspace, so it lives in one flat file at `<data-dir>/unite-set.json`
//! rather than the hashed per-workspace addressing the other stores use.
//!
//! The set is stored by workspace *name* (the stable identifier the Open list and
//! `workspaces.json` key on); names no longer registered are simply skipped when
//! the list pre-checks them, so a stale entry never errors.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::infrastructure::json_file;
use crate::infrastructure::storage;

/// File under the data dir holding the last unite selection.
const UNITE_FILE: &str = "unite-set.json";

/// The last set of workspaces opened together, by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UniteSet {
    /// Workspace names, in the order they were opened.
    pub workspaces: Vec<String>,
}

/// Persist the unite selection (workspace names), replacing any previous one. A
/// single workspace (the ordinary single-workspace open) is recorded just the
/// same, so re-opening it pre-checks nothing extra.
pub fn save(names: &[String]) -> Result<()> {
    let dir = storage::data_dir()?;
    let path = dir.join(UNITE_FILE);
    json_file::write_atomic(
        &dir,
        &path,
        &UniteSet {
            workspaces: names.to_vec(),
        },
    )
}

/// Read the last unite selection (workspace names), or an empty list when none is
/// stored or the file is unreadable/corrupt — the screen pre-checks nothing then.
pub fn load() -> Vec<String> {
    storage::data_dir()
        .ok()
        .and_then(|dir| {
            json_file::read::<UniteSet>(&dir.join(UNITE_FILE))
                .ok()
                .flatten()
        })
        .map(|set| set.workspaces)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `$USAGI_HOME` at a throwaway directory for the duration of a test,
    /// serialized against other env-mutating tests, and run `body`.
    fn with_data_dir(body: impl FnOnce()) {
        let _guard = crate::test_support::process_env_guard();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(storage::DATA_DIR_ENV, dir.path());
        body();
        std::env::remove_var(storage::DATA_DIR_ENV);
    }

    #[test]
    fn load_is_empty_when_nothing_is_stored() {
        with_data_dir(|| {
            assert!(load().is_empty());
        });
    }

    #[test]
    fn save_then_load_round_trips_the_names_in_order() {
        with_data_dir(|| {
            save(&["beta".to_string(), "alpha".to_string()]).unwrap();
            assert_eq!(load(), vec!["beta".to_string(), "alpha".to_string()]);
        });
    }

    #[test]
    fn save_replaces_the_previous_selection() {
        with_data_dir(|| {
            save(&["a".to_string(), "b".to_string()]).unwrap();
            save(&["c".to_string()]).unwrap();
            assert_eq!(load(), vec!["c".to_string()]);
        });
    }

    #[test]
    fn a_corrupt_file_reads_as_an_empty_selection() {
        with_data_dir(|| {
            let path = storage::data_dir().unwrap().join(UNITE_FILE);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "not json").unwrap();
            assert!(load().is_empty());
        });
    }
}
