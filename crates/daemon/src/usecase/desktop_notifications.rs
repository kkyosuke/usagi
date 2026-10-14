//! Daemon-owned notification transitions, independent of any connected TUI.
//!
//! Commit attempts before invoking the OS: a restart must not replay a desktop
//! notification. Delivery is best effort, not an exactly-once OS transaction.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use usagi_core::domain::{
    id::{UserDecisionId, WorkspaceId},
    pr_inventory::{PrChecksState, PrEntry, PrRefreshState, PrReviewDecision, PrState},
    presentation_text::sanitize_presentation_line,
};

pub const MAX_SUBJECTS: usize = 4096;
pub const MAX_DELIVERIES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: &'static str,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum State {
    Quiet,
    Decision,
    CiFailed(String),
    ReviewReady(String),
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub key: String,
    state: Option<State>,
    notification: Option<Notification>,
}

fn safe_label(label: &str) -> String {
    sanitize_presentation_line(label)
        .chars()
        .take(160)
        .collect()
}

impl Observation {
    #[must_use]
    pub fn decision(workspace: WorkspaceId, decision: UserDecisionId, label: &str) -> Self {
        Self {
            key: format!("{workspace}:decision:{decision}"),
            state: Some(State::Decision),
            notification: Some(Notification {
                title: "usagi: decision needed",
                body: safe_label(label),
            }),
        }
    }

    #[must_use]
    pub fn pull_request(workspace: WorkspaceId, pr: &PrEntry, label: &str) -> Self {
        let state = pr_state(pr);
        let title = match &state {
            Some(State::CiFailed(_)) => Some("usagi: CI failed"),
            Some(State::ReviewReady(_)) => Some("usagi: review ready"),
            _ => None,
        };
        Self {
            key: format!("{workspace}:pr:{}", pr.url()),
            state,
            notification: title.map(|title| Notification {
                title,
                body: format!("{} · PR #{}", safe_label(label), pr.number()),
            }),
        }
    }
}

fn pr_state(pr: &PrEntry) -> Option<State> {
    if pr.state != PrState::Open {
        return Some(State::Quiet);
    }
    let head = pr.head_oid.as_ref()?;
    if pr.refresh != PrRefreshState::Idle || head.is_empty() || head.len() > 128 {
        return None;
    }
    if pr.checks == Some(PrChecksState::Failing) {
        return Some(State::CiFailed(head.clone()));
    }
    // A pending refresh/check is not evidence of recovery. Keep the previous
    // state so polling and rerunning the same failing checks cannot spam.
    if pr.checks == Some(PrChecksState::Pending) {
        return None;
    }
    if pr.draft || pr.review == Some(PrReviewDecision::ChangesRequested) {
        return Some(State::Quiet);
    }
    Some(State::ReviewReady(head.clone()))
}

/// Only keys and states are durable; no question text, answer, or PR title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    version: u8,
    subjects: BTreeMap<String, State>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: 1,
            subjects: BTreeMap::new(),
        }
    }
}

pub trait NotificationStore {
    /// # Errors
    /// Fails on unreadable or unsupported durable state.
    fn load(&self) -> Result<Ledger>;
    /// # Errors
    /// Fails if an atomic attempt checkpoint cannot be committed.
    fn save(&self, ledger: &Ledger) -> Result<()>;
}

pub trait DesktopNotifier {
    /// Returns whether the OS helper accepted the request. The daemon must
    /// continue even if the platform or desktop service is unavailable.
    fn notify(&mut self, notification: &Notification) -> bool;
}

/// Reconcile one complete snapshot and deliver a bounded batch. The caller
/// holds the active-generation lease until this pass finishes.
///
/// # Errors
/// Returns an error without delivering when observation or persistence is invalid.
pub fn deliver(
    observations: &[Observation],
    store: &dyn NotificationStore,
    notifier: &mut dyn DesktopNotifier,
) -> Result<usize> {
    let previous = store.load()?;
    if previous.version != 1 || previous.subjects.len() > MAX_SUBJECTS {
        bail!("unsupported notification ledger");
    }
    let keys: BTreeSet<_> = observations.iter().map(|item| item.key.as_str()).collect();
    if observations.len() > MAX_SUBJECTS || keys.len() != observations.len() {
        bail!("invalid notification snapshot");
    }
    let mut next = previous.clone();
    next.subjects.retain(|key, _| keys.contains(key.as_str()));
    let mut notifications = Vec::new();
    for item in observations {
        let Some(state) = &item.state else { continue };
        if next.subjects.get(&item.key) == Some(state) {
            continue;
        }
        if let Some(notification) = &item.notification {
            if notifications.len() == MAX_DELIVERIES {
                continue;
            }
            notifications.push(notification);
        }
        next.subjects.insert(item.key.clone(), state.clone());
    }
    if next != previous {
        store.save(&next)?;
    }
    let mut failed = 0;
    for notification in notifications {
        failed += usize::from(!notifier.notify(notification));
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use usagi_core::domain::pr_inventory::canonicalize;

    #[derive(Default)]
    struct Store {
        ledger: RefCell<Ledger>,
        writes: Cell<usize>,
        fail_load: bool,
        fail_save: bool,
    }
    impl NotificationStore for Store {
        fn load(&self) -> Result<Ledger> {
            if self.fail_load {
                bail!("read failed");
            }
            Ok(self.ledger.borrow().clone())
        }
        fn save(&self, ledger: &Ledger) -> Result<()> {
            if self.fail_save {
                bail!("write failed");
            }
            self.ledger.replace(ledger.clone());
            self.writes.set(self.writes.get() + 1);
            Ok(())
        }
    }
    #[derive(Default)]
    struct Notifier {
        sent: Vec<Notification>,
        fail: bool,
    }
    impl DesktopNotifier for Notifier {
        fn notify(&mut self, notification: &Notification) -> bool {
            self.sent.push(notification.clone());
            !self.fail
        }
    }
    fn pr() -> PrEntry {
        let mut pr = PrEntry::new(canonicalize("https://github.com/acme/app/pull/1").unwrap());
        pr.head_oid = Some("a".repeat(40));
        pr
    }

    #[test]
    fn decision_reconnect_restart_and_delivery_failure_do_not_replay() {
        let store = Store::default();
        let item = Observation::decision(
            WorkspaceId::new(),
            UserDecisionId::new(),
            "project\n/ session",
        );
        let mut notifier = Notifier {
            fail: true,
            ..Notifier::default()
        };
        assert_eq!(
            deliver(std::slice::from_ref(&item), &store, &mut notifier).unwrap(),
            1
        );
        assert_eq!(notifier.sent.len(), 1);
        assert!(!notifier.sent[0].body.contains('\n'));
        assert_eq!(
            deliver(std::slice::from_ref(&item), &store, &mut notifier).unwrap(),
            0
        );
        assert_eq!(store.writes.get(), 1);
        let restarted = Store {
            ledger: RefCell::new(
                serde_json::from_str(&serde_json::to_string(&*store.ledger.borrow()).unwrap())
                    .unwrap(),
            ),
            ..Store::default()
        };
        deliver(&[item], &restarted, &mut notifier).unwrap();
        assert_eq!(notifier.sent.len(), 1);
        deliver(&[], &restarted, &mut notifier).unwrap();
        assert!(restarted.ledger.borrow().subjects.is_empty());
    }

    #[test]
    fn pr_changes_are_notified_but_refresh_and_pending_checks_do_not_reset_state() {
        let workspace = WorkspaceId::new();
        let store = Store::default();
        let mut notifier = Notifier::default();
        let mut pr = pr();
        pr.checks = Some(PrChecksState::Failing);
        let mut send = |pr: &PrEntry| {
            deliver(
                &[Observation::pull_request(
                    workspace,
                    pr,
                    "project / session",
                )],
                &store,
                &mut notifier,
            )
            .unwrap();
        };
        send(&pr);
        pr.refresh = PrRefreshState::BackingOff;
        send(&pr);
        pr.refresh = PrRefreshState::Idle;
        pr.checks = Some(PrChecksState::Pending);
        send(&pr);
        pr.checks = Some(PrChecksState::Failing);
        send(&pr);
        pr.checks = Some(PrChecksState::Passing);
        send(&pr);
        send(&pr);
        pr.head_oid = Some("b".repeat(40));
        send(&pr);
        pr.draft = true;
        send(&pr);
        pr.draft = false;
        send(&pr);
        pr.state = PrState::Merged;
        send(&pr);
        assert_eq!(
            notifier.sent.iter().map(|n| n.title).collect::<Vec<_>>(),
            [
                "usagi: CI failed",
                "usagi: review ready",
                "usagi: review ready",
                "usagi: review ready"
            ]
        );
        assert!(
            notifier
                .sent
                .iter()
                .all(|n| n.body == "project / session · PR #1")
        );
    }

    #[test]
    fn unknown_prs_drafts_review_changes_and_closed_prs_are_not_ready() {
        let mut pr = pr();
        pr.head_oid = None;
        assert_eq!(pr_state(&pr), None);
        pr.head_oid = Some(String::new());
        assert_eq!(pr_state(&pr), None);
        pr.head_oid = Some("x".repeat(129));
        assert_eq!(pr_state(&pr), None);
        pr.head_oid = Some("a".repeat(40));
        pr.draft = true;
        assert_eq!(pr_state(&pr), Some(State::Quiet));
        pr.draft = false;
        pr.review = Some(PrReviewDecision::ChangesRequested);
        assert_eq!(pr_state(&pr), Some(State::Quiet));
        pr.checks = Some(PrChecksState::Failing);
        assert!(matches!(pr_state(&pr), Some(State::CiFailed(_))));
        for state in [PrState::Dismissed, PrState::Closed, PrState::Merged] {
            pr.state = state;
            assert_eq!(pr_state(&pr), Some(State::Quiet));
        }
    }

    #[test]
    fn a_bounded_batch_preserves_pending_deliveries_and_separates_workspaces() {
        let store = Store::default();
        let mut notifier = Notifier::default();
        let id = UserDecisionId::new();
        let items: Vec<_> = (0..MAX_DELIVERIES + 2)
            .map(|_| Observation::decision(WorkspaceId::new(), id, &"x".repeat(200)))
            .collect();
        deliver(&items, &store, &mut notifier).unwrap();
        assert_eq!(notifier.sent.len(), MAX_DELIVERIES);
        assert_eq!(notifier.sent[0].body.len(), 160);
        deliver(&items, &store, &mut notifier).unwrap();
        assert_eq!(notifier.sent.len(), items.len());
        deliver(&items, &store, &mut notifier).unwrap();
        assert_eq!(store.writes.get(), 2);
    }

    #[test]
    fn invalid_or_uncommitted_state_never_delivers() {
        let item = Observation::decision(WorkspaceId::new(), UserDecisionId::new(), "project");
        let mut notifier = Notifier::default();
        for store in [
            Store {
                fail_load: true,
                ..Store::default()
            },
            Store {
                fail_save: true,
                ..Store::default()
            },
            Store {
                ledger: RefCell::new(Ledger {
                    version: 2,
                    ..Ledger::default()
                }),
                ..Store::default()
            },
        ] {
            assert!(deliver(std::slice::from_ref(&item), &store, &mut notifier).is_err());
        }
        let store = Store::default();
        assert!(deliver(&[item.clone(), item.clone()], &store, &mut notifier).is_err());
        assert!(deliver(&vec![item; MAX_SUBJECTS + 1], &store, &mut notifier).is_err());
        store.ledger.borrow_mut().subjects = (0..=MAX_SUBJECTS)
            .map(|n| (n.to_string(), State::Quiet))
            .collect();
        assert!(deliver(&[], &store, &mut notifier).is_err());
        assert!(notifier.sent.is_empty());
        assert_eq!(store.writes.get(), 0);
    }

    #[test]
    fn checkpoint_precedes_the_external_effect() {
        struct CheckingNotifier<'a>(&'a Store);
        impl DesktopNotifier for CheckingNotifier<'_> {
            fn notify(&mut self, _: &Notification) -> bool {
                assert_eq!(self.0.ledger.borrow().subjects.len(), 1);
                true
            }
        }
        let store = Store::default();
        let item = Observation::decision(WorkspaceId::new(), UserDecisionId::new(), "project");
        deliver(&[item], &store, &mut CheckingNotifier(&store)).unwrap();
    }
}
