//! Automatic retention of explicitly selected session build/cache directories.

use std::io;
use std::path::{Path, PathBuf};

use usagi_core::domain::session_cache::SessionCachePolicy;

/// Retain directories while a runtime is live, reserved, or ownership is unclear.
#[must_use]
pub const fn runtime_blocks_cleanup(state: super::terminal::TerminalRuntimeState) -> bool {
    use super::terminal::TerminalRuntimeState;
    !matches!(
        state,
        TerminalRuntimeState::Exited
            | TerminalRuntimeState::Reclaimed
            | TerminalRuntimeState::SpawnFailed
            | TerminalRuntimeState::Sleeping
            | TerminalRuntimeState::Interrupted
    )
}

/// A complete, no-follow measurement. The identity is rechecked at detachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheUsage {
    pub bytes: u64,
    pub modified: u64,
    pub identity: (u64, u64),
}

/// An exact session identity plus the selected relative directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheCandidate {
    pub session: usagi_core::domain::id::SessionId,
    pub session_name: String,
    pub path: PathBuf,
    pub usage: CacheUsage,
    pub protected: bool,
}

/// IO boundaries keep measurements off runtime locks and slow removal off both
/// runtime locks and IPC connections. Detachment must revalidate ownership and
/// process absence while excluding concurrent managed launches.
pub trait SessionCacheIo {
    /// # Errors
    /// Returns incomplete or unavailable discovery rather than authorizing cleanup.
    fn candidates(&mut self, paths: &[PathBuf]) -> io::Result<Vec<CacheCandidate>>;
    /// # Errors
    /// Returns ownership, confinement, or filesystem errors without discarding data.
    fn detach(&mut self, candidate: &CacheCandidate) -> io::Result<Option<PathBuf>>;
    /// # Errors
    /// Returns removal failures; the detached tree must remain for a later retry.
    fn remove(&mut self, detached: &Path) -> io::Result<()>;
}

/// Summary of one maintenance pass. Failed/blocked entries remain reclaimable
/// on a later pass; the size threshold is therefore a soft limit.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed_bytes: u64,
    pub removed: usize,
    pub deferred: usize,
}

/// Choose expired directories, then oldest directories until usage is below
/// the workspace-wide size budget. Stable path ordering resolves equal ages.
#[must_use]
pub fn plan(policy: &SessionCachePolicy, candidates: &[CacheCandidate], now: u64) -> Vec<usize> {
    let mut ordered = (0..candidates.len()).collect::<Vec<_>>();
    ordered.sort_by_key(|&index| {
        (
            candidates[index].usage.modified,
            candidates[index].session,
            &candidates[index].path,
        )
    });
    let mut remaining = total_bytes(candidates);
    let mut selected = Vec::new();
    for index in ordered {
        if candidates[index].protected {
            continue;
        }
        let usage = candidates[index].usage;
        let expired = policy.max_age_hours.is_some_and(|hours| {
            now.saturating_sub(usage.modified) >= hours.saturating_mul(3600)
                && usage.modified <= now
        });
        if expired || policy.max_bytes.is_some_and(|limit| remaining > limit) {
            remaining = remaining.saturating_sub(usage.bytes);
            selected.push(index);
        }
    }
    selected
}

/// Allocated bytes, including protected caches, with overflow-safe aggregation.
#[must_use]
pub fn total_bytes(candidates: &[CacheCandidate]) -> u64 {
    candidates.iter().fold(0u64, |sum, candidate| {
        sum.saturating_add(candidate.usage.bytes)
    })
}

/// Run one pass against injected daemon-owned IO.
///
/// # Errors
/// Returns configuration or discovery errors before applying any cleanup.
pub fn clean(
    io: &mut impl SessionCacheIo,
    policy: &SessionCachePolicy,
    now: u64,
) -> io::Result<CleanupReport> {
    policy.validate().map_err(io::Error::other)?;
    if policy.paths.is_empty() {
        return Ok(CleanupReport::default());
    }
    let candidates = io.candidates(&policy.paths)?;
    let mut report = CleanupReport::default();
    for index in plan(policy, &candidates, now) {
        let candidate = &candidates[index];
        match io.detach(candidate) {
            Ok(Some(detached)) if io.remove(&detached).is_ok() => {
                report.removed += 1;
                report.removed_bytes = report.removed_bytes.saturating_add(candidate.usage.bytes);
            }
            _ => report.deferred += 1,
        }
    }
    Ok(report)
}

/// Only the primitive filesystem calls are supplied by the real adapter.
pub trait CacheTreeIo {
    /// # Errors
    /// Returns errors observing metadata without following a symlink.
    fn node(&self, path: &Path) -> io::Result<CacheNode>;
    /// # Errors
    /// Returns errors listing the immediate directory contents.
    fn children(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
}

#[derive(Debug, Clone, Copy)]
pub struct CacheNode {
    pub directory: bool,
    pub symlink: bool,
    pub usage: CacheUsage,
}

/// Measure allocated bytes and the newest modification without following links.
///
/// # Errors
/// A partial measurement is never usable for cleanup: traversal errors retain
/// the complete directory until a later successful scan.
pub fn measure(io: &impl CacheTreeIo, root: &Path) -> io::Result<CacheUsage> {
    let initial = io.node(root)?;
    if !initial.directory || initial.symlink {
        return Err(io::Error::other("cache must be a real directory"));
    }
    let mut usage = CacheUsage {
        bytes: 0,
        ..initial.usage
    };
    let mut pending = vec![root.to_path_buf()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(path) = pending.pop() {
        let node = io.node(&path)?;
        if seen.insert(node.usage.identity) {
            usage.bytes = usage.bytes.saturating_add(node.usage.bytes);
        }
        usage.modified = usage.modified.max(node.usage.modified);
        if node.directory && !node.symlink {
            pending.extend(io.children(&path)?);
        }
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn candidate(path: &str, bytes: u64, modified: u64) -> CacheCandidate {
        CacheCandidate {
            session: usagi_core::domain::id::SessionId::new(),
            session_name: "fixture".into(),
            path: path.into(),
            usage: CacheUsage {
                bytes,
                modified,
                identity: (1, 2),
            },
            protected: false,
        }
    }

    #[test]
    fn retention_orders_age_and_size_without_expiring_future_timestamps() {
        let candidates = vec![
            candidate("new", 60, 7200),
            candidate("old", 50, 1),
            candidate("future", 10, 20000),
        ];
        let mut policy = SessionCachePolicy {
            paths: vec!["target".into()],
            max_age_hours: Some(2),
            max_bytes: None,
        };
        assert_eq!(plan(&policy, &candidates, 7201), vec![1]);
        policy.max_age_hours = None;
        policy.max_bytes = Some(80);
        assert_eq!(plan(&policy, &candidates, 7201), vec![1]);
        policy.max_bytes = Some(10);
        assert_eq!(plan(&policy, &candidates, 7201), vec![1, 0]);
        policy.max_bytes = Some(0);
        assert_eq!(plan(&policy, &candidates, 7201), vec![1, 0, 2]);
        assert!(plan(&policy, &[], 7201).is_empty());
        policy.max_bytes = None;
        policy.max_age_hours = Some(u64::MAX);
        assert!(plan(&policy, &candidates, 7201).is_empty());
        let same_session = candidates[0].session;
        let tied = vec![
            CacheCandidate {
                session: same_session,
                ..candidate("b", 1, 1)
            },
            CacheCandidate {
                session: same_session,
                ..candidate("a", 1, 1)
            },
        ];
        policy.max_bytes = Some(1);
        assert_eq!(plan(&policy, &tied, 1), vec![1]);
        let protected = vec![
            CacheCandidate {
                protected: true,
                ..candidate("busy", 100, 0)
            },
            candidate("idle", 2, 1),
        ];
        assert_eq!(plan(&policy, &protected, 1), vec![1]);
    }

    #[derive(Default)]
    struct FakeCleanup {
        entries: Vec<CacheCandidate>,
        mode: u8,
        removed: Vec<PathBuf>,
    }
    impl SessionCacheIo for FakeCleanup {
        fn candidates(&mut self, _: &[PathBuf]) -> io::Result<Vec<CacheCandidate>> {
            if self.mode == 4 {
                return Err(io::Error::other("discovery failed"));
            }
            Ok(self.entries.clone())
        }
        fn detach(&mut self, candidate: &CacheCandidate) -> io::Result<Option<PathBuf>> {
            match self.mode {
                1 => Ok(None),
                2 => Err(io::Error::other("became busy")),
                _ => Ok(Some(candidate.path.clone())),
            }
        }
        fn remove(&mut self, path: &Path) -> io::Result<()> {
            if self.mode == 3 {
                return Err(io::Error::other("retry trash next pass"));
            }
            self.removed.push(path.into());
            Ok(())
        }
    }

    #[test]
    fn cleanup_revalidates_candidates_and_defers_effect_failures() {
        let policy = SessionCachePolicy {
            paths: vec!["target".into()],
            max_age_hours: None,
            max_bytes: Some(0),
        };
        let mut io = FakeCleanup {
            entries: vec![candidate("target", 10, 1)],
            ..FakeCleanup::default()
        };
        assert_eq!(
            clean(&mut io, &SessionCachePolicy::default(), 5).unwrap(),
            CleanupReport::default()
        );
        assert_eq!(
            clean(&mut io, &policy, 5).unwrap(),
            CleanupReport {
                removed: 1,
                removed_bytes: 10,
                deferred: 0
            }
        );
        assert_eq!(io.removed, [PathBuf::from("target")]);
        for mode in 1..=3 {
            io.mode = mode;
            assert_eq!(clean(&mut io, &policy, 5).unwrap().deferred, 1);
        }
        io.mode = 4;
        assert!(clean(&mut io, &policy, 5).is_err());
        let invalid = SessionCachePolicy {
            paths: vec!["..".into()],
            ..policy
        };
        assert!(clean(&mut io, &invalid, 5).is_err());
    }

    struct Tree(BTreeMap<PathBuf, CacheNode>, bool);
    impl CacheTreeIo for Tree {
        fn node(&self, path: &Path) -> io::Result<CacheNode> {
            self.0
                .get(path)
                .copied()
                .ok_or_else(|| io::Error::other("missing"))
        }
        fn children(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            if self.1 {
                return Err(io::Error::other("unreadable"));
            }
            Ok(self
                .0
                .keys()
                .filter(|key| key.parent() == Some(path))
                .cloned()
                .collect())
        }
    }

    #[test]
    fn cache_measurement_does_not_follow_links_or_accept_partial_scans() {
        let usage = CacheUsage {
            bytes: 10,
            modified: 1,
            identity: (1, 2),
        };
        let dir = CacheNode {
            directory: true,
            symlink: false,
            usage,
        };
        let file = CacheNode {
            directory: false,
            symlink: false,
            usage: CacheUsage {
                modified: 2,
                identity: (1, 3),
                ..usage
            },
        };
        let link = CacheNode {
            directory: true,
            symlink: true,
            usage: CacheUsage {
                identity: (1, 4),
                ..usage
            },
        };
        let mut tree = Tree(
            [
                (PathBuf::from("target"), dir),
                ("target/file".into(), file),
                ("target/hardlink".into(), file),
                ("target/link".into(), link),
                ("target/link/untouched".into(), file),
            ]
            .into(),
            false,
        );
        assert_eq!(
            measure(&tree, Path::new("target")).unwrap(),
            CacheUsage {
                bytes: 30,
                modified: 2,
                ..usage
            }
        );
        assert!(measure(&tree, Path::new("target/file")).is_err());
        assert!(measure(&tree, Path::new("target/link")).is_err());
        assert!(measure(&tree, Path::new("absent")).is_err());
        tree.1 = true;
        assert!(measure(&tree, Path::new("target")).is_err());
    }

    #[test]
    fn only_proved_stopped_runtimes_allow_cache_cleanup() {
        use super::super::terminal::{TerminalReconcileState, TerminalRuntimeState as State};
        for state in [
            State::Exited,
            State::Reclaimed,
            State::SpawnFailed,
            State::Sleeping,
            State::Interrupted,
        ] {
            assert!(!runtime_blocks_cleanup(state));
        }
        for state in [
            State::Reserved,
            State::Running,
            State::ReconcileRequired(TerminalReconcileState::IdentityUnknown),
        ] {
            assert!(runtime_blocks_cleanup(state));
        }
    }
}
