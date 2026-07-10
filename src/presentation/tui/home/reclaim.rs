//! Eligibility and grace tracking for reclaiming finished session panes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::domain::workspace_state::{BranchStatus, PrState, WorktreeState};

pub(super) fn safe_to_reclaim(
    worktree: &WorktreeState,
    running: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
) -> bool {
    worktree.status != BranchStatus::Dirty
        && !running.contains(&worktree.path)
        && !waiting.contains(&worktree.path)
}

pub(super) fn has_merged_pr(worktree: &WorktreeState) -> bool {
    worktree.pr.iter().any(|pr| pr.state == PrState::Merged)
}

pub(super) fn finished_reclaim_paths(
    worktrees: &[WorktreeState],
    running: &HashSet<PathBuf>,
    waiting: &HashSet<PathBuf>,
    done: &HashSet<PathBuf>,
) -> Vec<PathBuf> {
    worktrees
        .iter()
        .filter(|worktree| {
            safe_to_reclaim(worktree, running, waiting)
                && (has_merged_pr(worktree) || done.contains(&worktree.path))
        })
        .map(|worktree| worktree.path.clone())
        .collect()
}

#[derive(Default)]
pub(super) struct ReclaimTracker {
    detected: HashMap<PathBuf, Instant>,
}

impl ReclaimTracker {
    pub(super) fn due(
        &mut self,
        worktrees: &[WorktreeState],
        running: &HashSet<PathBuf>,
        waiting: &HashSet<PathBuf>,
        live: &HashSet<PathBuf>,
        grace: Duration,
        now: Instant,
    ) -> Vec<PathBuf> {
        let eligible: HashSet<&Path> = worktrees
            .iter()
            .filter(|worktree| {
                live.contains(&worktree.path)
                    && has_merged_pr(worktree)
                    && safe_to_reclaim(worktree, running, waiting)
            })
            .map(|worktree| worktree.path.as_path())
            .collect();
        self.detected
            .retain(|path, _| eligible.contains(path.as_path()));
        let mut due = Vec::new();
        for path in eligible {
            let detected = self.detected.entry(path.to_path_buf()).or_insert(now);
            if now.saturating_duration_since(*detected) >= grace {
                due.push(path.to_path_buf());
            }
        }
        for path in &due {
            self.detected.remove(path);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::workspace_state::PrLink;
    use chrono::Utc;

    fn worktree(status: BranchStatus) -> WorktreeState {
        let mut pr = PrLink::new(173, "https://github.com/o/r/pull/173");
        pr.state = PrState::Merged;
        WorktreeState {
            branch: Some("usagi/issue-173".into()),
            path: PathBuf::from("/session"),
            head: "abc".into(),
            primary: false,
            upstream: None,
            status,
            diff: None,
            ahead_behind: None,
            pr: vec![pr],
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn safety_excludes_dirty_running_and_waiting() {
        let clean = worktree(BranchStatus::Synced);
        assert!(safe_to_reclaim(&clean, &HashSet::new(), &HashSet::new()));
        assert!(!safe_to_reclaim(
            &worktree(BranchStatus::Dirty),
            &HashSet::new(),
            &HashSet::new()
        ));
        assert!(!safe_to_reclaim(
            &clean,
            &HashSet::from([clean.path.clone()]),
            &HashSet::new()
        ));
        assert!(!safe_to_reclaim(
            &clean,
            &HashSet::new(),
            &HashSet::from([clean.path.clone()])
        ));
    }

    #[test]
    fn tracker_waits_for_grace_and_resets_when_unsafe() {
        let wt = worktree(BranchStatus::Synced);
        let live = HashSet::from([wt.path.clone()]);
        let start = Instant::now();
        let mut tracker = ReclaimTracker::default();
        assert!(tracker
            .due(
                std::slice::from_ref(&wt),
                &HashSet::new(),
                &HashSet::new(),
                &live,
                Duration::from_secs(60),
                start
            )
            .is_empty());
        let waiting = HashSet::from([wt.path.clone()]);
        assert!(tracker
            .due(
                std::slice::from_ref(&wt),
                &HashSet::new(),
                &waiting,
                &live,
                Duration::from_secs(60),
                start + Duration::from_secs(61)
            )
            .is_empty());
        assert_eq!(
            tracker.due(
                std::slice::from_ref(&wt),
                &HashSet::new(),
                &HashSet::new(),
                &live,
                Duration::ZERO,
                start + Duration::from_secs(62)
            ),
            vec![wt.path]
        );
    }

    #[test]
    fn tracker_requires_live_merged_session() {
        let mut open = worktree(BranchStatus::Synced);
        open.pr[0].state = PrState::Open;
        assert!(!has_merged_pr(&open));

        let merged = worktree(BranchStatus::Synced);
        assert!(has_merged_pr(&merged));
        let start = Instant::now();
        let mut tracker = ReclaimTracker::default();
        assert!(tracker
            .due(
                std::slice::from_ref(&merged),
                &HashSet::new(),
                &HashSet::new(),
                &HashSet::new(),
                Duration::ZERO,
                start
            )
            .is_empty());
    }

    #[test]
    fn finished_reclaim_paths_accepts_done_or_merged_sessions() {
        let merged = worktree(BranchStatus::Synced);
        let mut done = worktree(BranchStatus::Synced);
        done.path = PathBuf::from("/done");
        done.pr[0].state = PrState::Open;
        let mut active = worktree(BranchStatus::Synced);
        active.path = PathBuf::from("/active");
        active.pr[0].state = PrState::Open;

        assert_eq!(
            finished_reclaim_paths(
                &[merged.clone(), done.clone(), active.clone()],
                &HashSet::from([active.path.clone()]),
                &HashSet::new(),
                &HashSet::from([done.path.clone()])
            ),
            vec![merged.path, done.path]
        );
    }
}
