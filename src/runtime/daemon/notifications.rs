//! Desktop notification IO. No TUI, PTY, runtime lock, or Git subprocess is needed.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use usagi_core::{
    domain::session_lifecycle::SessionLifecycle,
    infrastructure::{
        persistence::json_file,
        store::{
            lifecycle::DaemonLifecycleStore, pr_inventory::PrInventoryStore,
            user_decision::UserDecisionStore,
        },
        workspace_state,
    },
};
use usagi_daemon::usecase::{
    desktop_notifications::{
        DesktopNotifier, Ledger, MAX_SUBJECTS, Notification, NotificationStore, Observation,
        deliver,
    },
    shutdown::{BackgroundWorker, ShutdownRequest},
};

use super::{
    AdmissionGate, ChildObservation, ChildPolicy, ErrorLog, active_cleanup_lease, observe_until,
};

const LEDGER_BYTES: usize = 2 * 1024 * 1024;
const TICK: Duration = Duration::from_secs(5);

struct FileLedger(PathBuf);

impl NotificationStore for FileLedger {
    fn load(&self) -> Result<Ledger> {
        Ok(
            json_file::read_bounded(&self.0.join("desktop-notifications.json"), LEDGER_BYTES)?
                .unwrap_or_default(),
        )
    }
    fn save(&self, ledger: &Ledger) -> Result<()> {
        // write_atomic writes pretty JSON followed by a newline. Bound those
        // exact bytes so a successful checkpoint always remains readable.
        if serde_json::to_vec_pretty(ledger)?.len() >= LEDGER_BYTES {
            bail!("notification checkpoint capacity exceeded");
        }
        json_file::write_atomic(&self.0, &self.0.join("desktop-notifications.json"), ledger)
    }
}

/// Read durable workspaces, including retired tenants whose PRs still refresh.
/// Any read failure leaves the previous checkpoint intact, never a partial view.
fn snapshot(dir: &Path, now: DateTime<Utc>) -> Result<Vec<Observation>> {
    let prs = PrInventoryStore::new(dir).load()?;
    let decisions = UserDecisionStore::new(dir);
    let mut observations = BTreeMap::new();
    for workspace in workspace_state::adopted(dir)? {
        let Some(lifecycle) = DaemonLifecycleStore::new(workspace.dir()).load()? else {
            continue;
        };
        let project = workspace
            .root()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let sessions: BTreeMap<_, _> = lifecycle
            .sessions
            .iter()
            .filter(|session| session.lifecycle == SessionLifecycle::Available)
            .map(|session| (session.session_id, session.name.as_str()))
            .collect();
        for decision in decisions.pending(lifecycle.workspace_id)? {
            if decision.expires_at.is_some_and(|expires| expires <= now) {
                continue;
            }
            let name = match decision.owner.session_id {
                Some(id) => {
                    let Some(name) = sessions.get(&id) else {
                        continue;
                    };
                    *name
                }
                None => "Workspace",
            };
            let item = Observation::decision(
                lifecycle.workspace_id,
                decision.decision_id,
                &format!("{project} / {name}"),
            );
            observations.insert(item.key.clone(), item);
        }
        let mut candidates = BTreeMap::new();
        for (id, name) in sessions {
            if let Some(inventory) = prs.sessions.get(&id) {
                for pr in inventory.entries.values() {
                    if pr.is_dismissed() {
                        continue;
                    }
                    // Dismissal and pinning belong to individual sessions.
                    // Prefer an automatically refreshed, known copy so an old
                    // user-owned state cannot hide another session's live PR.
                    let priority = (pr.pinned, pr.head_oid.is_none());
                    let candidate = candidates
                        .entry(&pr.identity)
                        .or_insert((priority, pr, name));
                    if priority < candidate.0 {
                        *candidate = (priority, pr, name);
                    }
                }
            }
        }
        for (_, pr, name) in candidates.into_values() {
            let item = Observation::pull_request(
                lifecycle.workspace_id,
                pr,
                &format!("{project} / {name}"),
            );
            observations.insert(item.key.clone(), item);
        }
        if observations.len() > MAX_SUBJECTS {
            bail!("notification snapshot capacity exceeded");
        }
    }
    Ok(observations.into_values().collect())
}

fn command(
    platform: &str,
    uid: u32,
    notification: &Notification,
) -> Option<(&'static str, Vec<String>)> {
    match platform {
        "macos" => Some(("/bin/launchctl", vec![
            "asuser".into(), uid.to_string(), "/usr/bin/osascript".into(), "-e".into(),
            "on run argv\n display notification (item 2 of argv) with title (item 1 of argv)\nend run".into(),
            "--".into(), notification.title.into(), notification.body.clone(),
        ])),
        "linux" => Some(("notify-send", vec!["--app-name=usagi".into(), "--".into(), notification.title.into(), notification.body.clone()])),
        _ => None,
    }
}

struct PlatformNotifier(Arc<ShutdownRequest>);

#[coverage(off)] // coverage: reason=real_io owner=daemon expires=2027-01-31 tests=desktop_notification_commands_keep_text_in_arguments
impl DesktopNotifier for PlatformNotifier {
    fn notify(&mut self, notification: &Notification) -> bool {
        // SAFETY: geteuid only reads the current OS identity. asuser selects
        // the notification helper's user context without changing daemon custody.
        let uid = unsafe { libc::geteuid() };
        let Some((program, arguments)) = command(std::env::consts::OS, uid, notification) else {
            return false;
        };
        let arguments: Vec<_> = arguments.iter().map(String::as_str).collect();
        matches!(
            observe_until(
                program,
                &arguments,
                ChildPolicy {
                    timeout: Duration::from_secs(2),
                    terminate_grace: Duration::from_millis(100),
                    output_limit: 1024,
                },
                &self.0.flag()
            ),
            ChildObservation::Success(_) | ChildObservation::EmptyOutput
        )
    }
}

#[coverage(off)] // coverage: reason=composition owner=daemon expires=2027-01-31 tests=desktop_notifications_run_without_a_tui_and_join_on_shutdown
pub(super) fn start(
    dir: PathBuf,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let notifier = Box::new(PlatformNotifier(Arc::clone(&shutdown)));
    start_with(dir, gate, shutdown, notifier, TICK, ErrorLog::record)
}

fn start_with(
    dir: PathBuf,
    gate: AdmissionGate,
    shutdown: Arc<ShutdownRequest>,
    mut notifier: Box<dyn DesktopNotifier + Send>,
    tick: Duration,
    report: fn(&str),
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("usagi-desktop-notifications".into())
        .spawn(move || {
            let health = shutdown.monitor_background_worker(BackgroundWorker::DesktopNotifications);
            let mut unavailable = false;
            while !shutdown.is_requested() {
                if let Some(_lease) = active_cleanup_lease(&gate) {
                    let result = snapshot(&dir, Utc::now()).and_then(|items| {
                        deliver(&items, &FileLedger(dir.clone()), &mut *notifier)
                    });
                    let failed = !matches!(result, Ok(0));
                    if failed && !unavailable {
                        report("Desktop notifications unavailable; background work continues");
                    }
                    unavailable = failed;
                }
                if shutdown.wait_for_tick(tick) {
                    break;
                }
            }
            health.finish_planned();
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    static REPORTS: AtomicUsize = AtomicUsize::new(0);
    fn report(_: &str) {
        REPORTS.fetch_add(1, Ordering::SeqCst);
    }
    use usagi_core::domain::{
        agent::CallerRef,
        id::{AgentId, DaemonGeneration, OperationId, SessionId, UserDecisionId, WorkspaceId},
        pr_inventory::{PrChecksState, PrEntry, PrInventory, PrState, canonicalize},
        session_lifecycle::{ManagedSession, WorkspaceLifecycleState},
        user_decision::{
            UserDecision, UserDecisionOwner, UserDecisionSelectionMode, UserDecisionStatus,
        },
    };
    use usagi_daemon::usecase::generation::GenerationRole;

    fn workspace(dir: &Path, name: &str) -> (WorkspaceId, SessionId) {
        let root = dir.join(name);
        let state = workspace_state::resolve(dir, &root).unwrap();
        let mut lifecycle = WorkspaceLifecycleState::new(WorkspaceId::new(), Utc::now());
        let session = ManagedSession::adopt_available("build".into(), Utc::now());
        let id = session.session_id;
        lifecycle.sessions.push(session);
        DaemonLifecycleStore::new(state.dir())
            .initialize(&lifecycle, &root)
            .unwrap();
        (lifecycle.workspace_id, id)
    }

    fn question(
        dir: &Path,
        workspace: WorkspaceId,
        session: Option<SessionId>,
        expires: Option<DateTime<Utc>>,
    ) -> UserDecisionId {
        let decision = UserDecision {
            decision_id: UserDecisionId::new(),
            owner: UserDecisionOwner {
                workspace_id: workspace,
                session_id: session,
                caller: CallerRef {
                    session_id: session,
                    agent_id: AgentId::new(),
                },
                run_id: OperationId::new(),
            },
            title: "private question title".into(),
            prompt: "private body".into(),
            options: vec![],
            allow_freeform: true,
            allow_comment: false,
            require_confirmation: false,
            recommendation: None,
            selection_limits: None,
            selection_mode: UserDecisionSelectionMode::Single,
            context: vec![],
            expires_at: expires,
            idempotency_key: None,
            status: UserDecisionStatus::Pending,
            answer: None,
            created_at: Utc::now(),
            resolved_at: None,
        };
        UserDecisionStore::new(dir)
            .create(decision)
            .unwrap()
            .unwrap()
            .decision_id
    }

    struct Recording(mpsc::Sender<Notification>);
    impl DesktopNotifier for Recording {
        fn notify(&mut self, notification: &Notification) -> bool {
            self.0.send(notification.clone()).unwrap();
            true
        }
    }

    struct TestWorker {
        shutdown: Arc<ShutdownRequest>,
        handle: Option<std::thread::JoinHandle<()>>,
    }
    impl TestWorker {
        fn join(mut self) -> std::thread::Result<()> {
            self.shutdown.request();
            self.handle.take().unwrap().join()
        }
    }
    impl Drop for TestWorker {
        fn drop(&mut self) {
            self.shutdown.request();
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }
    fn start_test(
        dir: PathBuf,
        gate: AdmissionGate,
        shutdown: Arc<ShutdownRequest>,
        notifier: Box<dyn DesktopNotifier + Send>,
        tick: Duration,
        report: fn(&str),
    ) -> std::io::Result<TestWorker> {
        let handle = start_with(dir, gate, Arc::clone(&shutdown), notifier, tick, report)?;
        Ok(TestWorker {
            shutdown,
            handle: Some(handle),
        })
    }

    #[test]
    fn snapshot_covers_retired_workspaces_filters_dead_questions_and_keeps_text_private() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let (first, session) = workspace(dir, "first");
        let (second, _) = workspace(dir, "second");
        workspace_state::resolve(dir, &dir.join("not-initialized")).unwrap();
        question(dir, first, Some(session), None);
        question(dir, second, None, None);
        question(dir, first, Some(SessionId::new()), None);
        question(
            dir,
            first,
            None,
            Some(Utc::now() + chrono::Duration::minutes(1)),
        );
        let mut prs = PrInventoryStore::new(dir).load().unwrap();
        let mut entry = PrEntry::new(canonicalize("https://github.com/acme/app/pull/1").unwrap());
        entry.head_oid = Some("a".repeat(40));
        entry.checks = Some(PrChecksState::Failing);
        prs.sessions.insert(
            session,
            PrInventory {
                revision: 1,
                entries: [(entry.identity.clone(), entry)].into(),
            },
        );
        PrInventoryStore::new(dir).save(&prs).unwrap();
        let items = snapshot(dir, Utc::now() + chrono::Duration::minutes(2)).unwrap();
        assert_eq!(items.len(), 3);
        let (tx, rx) = mpsc::channel();
        let store = FileLedger(dir.into());
        deliver(&items, &store, &mut Recording(tx.clone())).unwrap();
        let sent: Vec<_> = rx.try_iter().collect();
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|item| !item.body.contains("private")));
        assert!(sent.iter().any(|item| item.body == "second / Workspace"));
        assert!(sent.iter().any(|item| item.title == "usagi: CI failed"));
        let persisted = std::fs::read_to_string(dir.join("desktop-notifications.json")).unwrap();
        assert!(!persisted.contains("private"));
        assert!(!persisted.contains("build"));
        deliver(&items, &FileLedger(dir.into()), &mut Recording(tx)).unwrap();
        assert_eq!(rx.try_iter().count(), 0);
        std::fs::write(dir.join("pr-inventory.json"), "broken").unwrap();
        assert!(snapshot(dir, Utc::now()).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("desktop-notifications.json")).unwrap(),
            persisted
        );
    }

    #[test]
    fn corrupt_or_unwritable_checkpoints_fail_without_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let store = FileLedger(temp.path().into());
        assert_eq!(store.load().unwrap(), Ledger::default());
        store.save(&Ledger::default()).unwrap();
        let original = std::fs::read(temp.path().join("desktop-notifications.json")).unwrap();
        let near_limit: Ledger = serde_json::from_value(serde_json::json!({
            "version": 1,
            "subjects": { "x".repeat(LEDGER_BYTES - 48): "Quiet" }
        }))
        .unwrap();
        assert!(serde_json::to_vec(&near_limit).unwrap().len() < LEDGER_BYTES);
        assert!(serde_json::to_vec_pretty(&near_limit).unwrap().len() >= LEDGER_BYTES);
        assert!(store.save(&near_limit).is_err());
        assert_eq!(
            std::fs::read(temp.path().join("desktop-notifications.json")).unwrap(),
            original
        );
        std::fs::write(temp.path().join("desktop-notifications.json"), "corrupt").unwrap();
        assert!(store.load().is_err());
        let file = temp.path().join("not-a-directory");
        std::fs::write(&file, "file").unwrap();
        assert!(FileLedger(file).save(&Ledger::default()).is_err());
        let oversized: Ledger = serde_json::from_value(serde_json::json!({
            "version": 1,
            "subjects": { "x".repeat(LEDGER_BYTES): "Quiet" }
        }))
        .unwrap();
        assert!(store.save(&oversized).is_err());
    }

    #[test]
    fn duplicate_prs_use_a_refreshed_session_instead_of_dismissed_or_pinned_copies() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        workspace(dir, "project");
        let states = workspace_state::adopted(dir).unwrap();
        let lifecycle_store = DaemonLifecycleStore::new(states[0].dir());
        let mut lifecycle = lifecycle_store.load().unwrap().unwrap();
        lifecycle
            .sessions
            .push(ManagedSession::adopt_available("other".into(), Utc::now()));
        lifecycle.sessions.sort_by_key(|session| session.session_id);
        lifecycle.sessions[0].name = "stale".into();
        lifecycle.sessions[1].name = "current".into();
        lifecycle_store
            .initialize(&lifecycle, states[0].root())
            .unwrap();
        let identity = canonicalize("https://github.com/acme/app/pull/1").unwrap();
        let mut current = PrEntry::new(identity.clone());
        current.head_oid = Some("b".repeat(40));
        current.checks = Some(PrChecksState::Passing);
        let store = PrInventoryStore::new(dir);
        let mut prs = store.load().unwrap();
        prs.sessions.insert(
            lifecycle.sessions[1].session_id,
            PrInventory {
                revision: 1,
                entries: [(identity.clone(), current.clone())].into(),
            },
        );
        let (tx, rx) = mpsc::channel();
        let mut notifier = Recording(tx);
        for (state, pinned, head) in [
            (PrState::Dismissed, false, Some("a".repeat(40))),
            (PrState::Closed, true, Some("a".repeat(40))),
            (PrState::Open, true, Some("a".repeat(40))),
            (PrState::Open, false, None),
        ] {
            let mut stale = current.clone();
            stale.state = state;
            stale.pinned = pinned;
            stale.head_oid = head;
            stale.checks = Some(PrChecksState::Failing);
            prs.sessions.insert(
                lifecycle.sessions[0].session_id,
                PrInventory {
                    revision: 1,
                    entries: [(identity.clone(), stale)].into(),
                },
            );
            store.save(&prs).unwrap();
            let items = snapshot(dir, Utc::now()).unwrap();
            assert_eq!(items.len(), 1);
            deliver(&items, &FileLedger(dir.into()), &mut notifier).unwrap();
        }
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [Notification {
                title: "usagi: review ready",
                body: "project / current · PR #1".into(),
            }]
        );
        let checkpoint = std::fs::read(dir.join("desktop-notifications.json")).unwrap();
        for inventory in prs.sessions.values_mut() {
            inventory.entries.get_mut(&identity).unwrap().head_oid = None;
        }
        store.save(&prs).unwrap();
        deliver(
            &snapshot(dir, Utc::now()).unwrap(),
            &FileLedger(dir.into()),
            &mut notifier,
        )
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(
            std::fs::read(dir.join("desktop-notifications.json")).unwrap(),
            checkpoint
        );
        for inventory in prs.sessions.values_mut() {
            inventory.entries.get_mut(&identity).unwrap().state = PrState::Dismissed;
        }
        store.save(&prs).unwrap();
        assert!(snapshot(dir, Utc::now()).unwrap().is_empty());
    }

    #[test]
    fn desktop_notification_commands_keep_text_in_arguments() {
        let notification = Notification {
            title: "usagi: decision needed",
            body: "--flag; $(touch file)\"\nbody".into(),
        };
        for (platform, executable) in [("macos", "/bin/launchctl"), ("linux", "notify-send")] {
            let (program, args) = command(platform, 501, &notification).unwrap();
            assert_eq!(program, executable);
            assert_eq!(args.last().unwrap(), &notification.body);
            assert_eq!(args[args.len() - 2], notification.title);
            assert_eq!(args[args.len() - 3], "--");
        }
        assert!(command("unsupported", 501, &notification).is_none());
        let (_, args) = command("macos", 501, &notification).unwrap();
        assert_eq!(&args[..3], ["asuser", "501", "/usr/bin/osascript"]);
    }

    #[test]
    fn desktop_notifications_run_without_a_tui_and_join_on_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, session) = workspace(temp.path(), "project");
        let gate = AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active);
        let shutdown = Arc::new(ShutdownRequest::new());
        let (tx, rx) = mpsc::channel();
        let handle = start_test(
            temp.path().into(),
            gate,
            Arc::clone(&shutdown),
            Box::new(Recording(tx)),
            Duration::from_millis(5),
            report,
        )
        .unwrap();
        question(temp.path(), workspace, Some(session), None);
        let notification = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(notification.body, "project / build");
        shutdown.request();
        drop(handle);
        assert!(rx.try_recv().is_err());
        assert_eq!(shutdown.background_worker_health().failed_count(), 0);
    }

    #[test]
    fn standby_and_draining_generations_do_not_claim_notifications() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, _) = workspace(temp.path(), "project");
        question(temp.path(), workspace, None, None);
        let gate = AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Standby);
        let shutdown = Arc::new(ShutdownRequest::new());
        let (tx, rx) = mpsc::channel();
        let handle = start_test(
            temp.path().into(),
            gate.clone(),
            Arc::clone(&shutdown),
            Box::new(Recording(tx)),
            Duration::from_millis(5),
            report,
        )
        .unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        gate.activate().unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        gate.close(super::super::LeaseClass::ActiveControl);
        gate.await_drain(super::super::LeaseClass::ActiveControl)
            .unwrap();
        gate.enter_draining().unwrap();
        question(temp.path(), workspace, None, None);
        assert!(rx.recv_timeout(Duration::from_millis(30)).is_err());
        shutdown.request();
        handle.join().unwrap();
    }

    #[test]
    fn observation_failure_is_retried_without_stopping_the_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let (workspace, _) = workspace(temp.path(), "project");
        question(temp.path(), workspace, None, None);
        let store = PrInventoryStore::new(temp.path());
        std::fs::write(store.path(), "invalid").unwrap();
        let gate = AdmissionGate::new(DaemonGeneration::new(), GenerationRole::Active);
        let shutdown = Arc::new(ShutdownRequest::new());
        let (tx, rx) = mpsc::channel();
        let worker = start_test(
            temp.path().into(),
            gate,
            Arc::clone(&shutdown),
            Box::new(Recording(tx)),
            Duration::from_millis(5),
            report,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while REPORTS.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!shutdown.is_requested());
        assert!(rx.try_recv().is_err());
        store
            .save(
                &usagi_core::infrastructure::store::pr_inventory::PrInventoryStoreSnapshot::default(
                ),
            )
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        shutdown.request();
        worker.join().unwrap();
        assert_eq!(REPORTS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn oversized_snapshot_preserves_the_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        workspace(dir, "project");
        let states = workspace_state::adopted(dir).unwrap();
        let lifecycle_store = DaemonLifecycleStore::new(states[0].dir());
        let mut lifecycle = lifecycle_store.load().unwrap().unwrap();
        let mut prs = PrInventoryStore::new(dir).load().unwrap();
        // Seventeen bounded session inventories can exceed the global ceiling.
        for group in 0..17 {
            let session = ManagedSession::adopt_available(format!("session-{group}"), Utc::now());
            let mut inventory = PrInventory::default();
            for index in 0..256 {
                let identity = canonicalize(&format!(
                    "https://github.com/acme/app/pull/{}",
                    group * 256 + index + 1
                ))
                .unwrap();
                inventory
                    .entries
                    .insert(identity.clone(), PrEntry::new(identity));
            }
            prs.sessions.insert(session.session_id, inventory);
            lifecycle.sessions.push(session);
        }
        lifecycle_store
            .initialize(&lifecycle, states[0].root())
            .unwrap();
        PrInventoryStore::new(dir).save(&prs).unwrap();
        assert!(snapshot(dir, Utc::now()).is_err());
        assert!(!dir.join("desktop-notifications.json").exists());
    }
}
