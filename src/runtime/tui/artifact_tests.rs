//! Regression paths through the production overlay ports, workers and Home view.
#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;
use std::fs;
use usagi_core::domain::workspace_state::WorkspaceState;
use usagi_tui::presentation::views::workspace::{HomeProjection, ProjectedSession, render_home};
use usagi_tui::usecase::application::controller::{AppState, Effect, update};

fn refresh_artifact_counts(port: &mut DaemonMetricsPort) -> Vec<MetricsUpdate> {
    port.last_git_refresh = None;
    let mut updates = port.poll_updates(None);
    let deadline = Instant::now() + Duration::from_secs(5);
    while port.git_receiver.is_some() {
        assert!(
            Instant::now() < deadline,
            "sidebar observation worker timed out"
        );
        thread::sleep(Duration::from_millis(5));
        updates.extend(port.poll_updates(None));
    }
    updates
}

#[test]
fn sidebar_artifact_counts_follow_files_without_requiring_git_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let mut port = DaemonMetricsPort {
        metrics: RefreshPump::spawn(metrics_cadence(), || Ok(None)),
        latest: None,
        git_diffs: BTreeMap::new(),
        artifact_counts: BTreeMap::new(),
        git_sessions: vec![(session, tmp.path().to_path_buf())],
        git_receiver: None,
        last_git_refresh: None,
    };
    let updates = refresh_artifact_counts(&mut port);
    assert!(
        updates.contains(&MetricsUpdate::ArtifactCounts(BTreeMap::from([(
            session, 0
        )])))
    );

    usagi_core::infrastructure::artifacts::prepare(tmp.path()).unwrap();
    fs::create_dir(tmp.path().join("artifacts/empty")).unwrap();
    assert!(refresh_artifact_counts(&mut port).is_empty());

    // The same worker also retains Git summaries when they are available.
    for args in [
        vec!["init", "--quiet", "--initial-branch=main", "."],
        vec![
            "-c",
            "user.name=Artifact test",
            "-c",
            "user.email=artifact@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "base",
        ],
        vec!["checkout", "--quiet", "-b", "artifact-test"],
    ] {
        let output = Command::new("git")
            .current_dir(tmp.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(refresh_artifact_counts(&mut port).iter().any(|update| {
        matches!(update, MetricsUpdate::GitDiffs(diffs) if diffs.contains_key(&session))
    }));

    fs::write(tmp.path().join("artifacts/empty/report.txt"), "report").unwrap();
    assert!(
        refresh_artifact_counts(&mut port).contains(&MetricsUpdate::ArtifactCounts(
            BTreeMap::from([(session, 1)])
        ))
    );

    fs::remove_file(tmp.path().join("artifacts/empty/report.txt")).unwrap();
    assert!(
        refresh_artifact_counts(&mut port).contains(&MetricsUpdate::ArtifactCounts(
            BTreeMap::from([(session, 0)])
        ))
    );

    // A late result for a removed session cannot restore its badge.
    let (sender, receiver) = mpsc::channel();
    sender
        .send(SessionSidebarObservation {
            session,
            git_diff: None,
            artifact_count: 99,
        })
        .unwrap();
    drop(sender);
    port.git_receiver = Some(receiver);
    assert!(
        port.poll_updates(Some(&[]))
            .contains(&MetricsUpdate::ArtifactCounts(BTreeMap::new()))
    );
    assert!(port.artifact_counts.is_empty());
}

struct Browser {
    opened: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl crate::runtime::artifact_opener::ArtifactViewer for Browser {
    fn open(
        &mut self,
        path: &str,
    ) -> Result<usagi_tui::usecase::application::controller::ArtifactOpenOutcome, String> {
        self.opened.lock().unwrap().push(fs::read(path).unwrap());
        Err("test opener unavailable".into())
    }
}

fn record(root: &Path, name: &str) -> SessionRecord {
    SessionRecord {
        name: name.into(),
        display_name: None,
        origin: SessionOrigin::Human,
        started_from: None,
        root: root.join(".usagi/sessions").join(name),
        created_at: Utc::now(),
        last_active: None,
        notes: Scratchpad::default(),
        prs: vec![],
    }
}

fn access(
    root: &Path,
    workspace: WorkspaceId,
    roots: preview_access::PreviewRoots,
) -> Box<dyn crate::runtime::preview_authority::PreviewAuthority> {
    let observed = Arc::clone(&roots);
    Box::new(preview_access::PreviewAccess::with_observer(
        workspace,
        root.to_path_buf(),
        roots,
        Box::new(move || {
            let sessions = preview_access::lock_roots(&observed)
                .iter()
                .map(|(id, path)| {
                    let mut session = ManagedSession::adopt_available(
                        path.file_name().unwrap().to_string_lossy().into_owned(),
                        Utc::now(),
                    );
                    session.session_id = *id;
                    session
                })
                .collect();
            Ok(LifecycleSnapshot {
                workspace_id: workspace,
                root_worktree_id: usagi_core::domain::id::WorktreeId::new(),
                revision: 1,
                sessions,
                agent_resumes: BTreeMap::new(),
                session_roles: BTreeMap::new(),
            })
        }),
    ))
}

fn port(
    root: &Path,
    workspace: WorkspaceId,
    records: &[(SessionId, SessionRecord)],
) -> (ProductionOverlayPort, Arc<Mutex<Vec<Vec<u8>>>>) {
    let snapshot = WorkspaceSnapshot::with_runtime_ids(
        Workspace::new("artifacts-test", root),
        WorkspaceState {
            sessions: records.iter().map(|(_, row)| row.clone()).collect(),
            ..Default::default()
        },
        workspace,
        records.iter().map(|(id, _)| *id).collect(),
    );
    let opened = Arc::new(Mutex::new(Vec::new()));
    let roots = Arc::new(Mutex::new(project_backend_session_roots(&snapshot)));
    let port = ProductionOverlayPort {
        root: root.to_path_buf(),
        sessions: Arc::clone(&roots),
        pr_sessions: Arc::new(Mutex::new(snapshot.session_ids)),
        pr_pump: RefreshPump::spawn(
            RefreshCadence::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            || Ok(Vec::new()),
        ),
        preview_pump: PreviewPump::spawn(load_preview, access(root, workspace, Arc::clone(&roots))),
        artifact_opener: crate::runtime::artifact_opener::ArtifactOpener::spawn(
            Box::new(Browser {
                opened: Arc::clone(&opened),
            }),
            access(root, workspace, roots),
        ),
        browser: PlatformBrowserOpener {
            reaper: PlatformChildReaper::default(),
        },
        clipboard: PlatformClipboard,
    };
    (port, opened)
}

fn load(port: &mut ProductionOverlayPort, target: Target, filter: PreviewFileFilter) -> AppEvent {
    let (admission, immediate) = Completions::channel();
    port.load_preview(target, RequestId::new(), None, filter, admission);
    if let Ok(event) = immediate.try_recv() {
        return event;
    }
    let (completions, events) = Completions::channel();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        port.poll(&completions);
        for event in events.try_iter() {
            if matches!(
                event,
                AppEvent::Backend(
                    BackendEvent::PreviewLoaded { .. } | BackendEvent::PreviewError { .. }
                )
            ) {
                return event;
            }
        }
        assert!(Instant::now() < deadline, "preview completion missing");
        thread::yield_now();
    }
}

fn native(port: &mut ProductionOverlayPort, target: Target, path: &str, id: RequestId) -> AppEvent {
    let (completions, events) = Completions::channel();
    port.open_artifact(
        target,
        id,
        path.into(),
        PreviewFileFilter::Artifacts,
        completions,
    );
    events.recv_timeout(Duration::from_secs(5)).unwrap()
}

fn wait_preview(port: &mut ProductionOverlayPort) -> AppEvent {
    let (completion, events) = Completions::channel();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        port.poll(&completion);
        if let Some(event) = events.try_iter().find(|event| {
            matches!(
                event,
                AppEvent::Backend(
                    BackendEvent::PreviewLoaded { .. } | BackendEvent::PreviewError { .. }
                )
            )
        }) {
            return event;
        }
        assert!(Instant::now() < deadline, "preview completion missing");
        thread::yield_now();
    }
}

#[test]
fn queued_and_in_flight_text_previews_reject_reused_session_paths() {
    for queued in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let workspace = WorkspaceId::new();
        let old = SessionId::new();
        let row = record(root.path(), "same");
        fs::create_dir_all(row.root.join("artifacts")).unwrap();
        let path = row.root.join("artifacts/report.md");
        fs::write(&path, "OLD").unwrap();
        let (mut port, _) = port(root.path(), workspace, &[(old, row.clone())]);
        let (started, waiting) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let checks = access(root.path(), workspace, Arc::clone(&port.sessions));
        port.preview_pump = PreviewPump::spawn(
            move |root, path, filter| {
                started.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                load_preview(root, path, filter)
            },
            checks,
        );
        let (completion, immediate) = Completions::channel();
        let request_id = RequestId::new();
        if queued {
            let (first_completion, _) = Completions::channel();
            port.load_preview(
                Target::Root(workspace),
                RequestId::new(),
                None,
                PreviewFileFilter::AllArtifacts,
                first_completion,
            );
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        port.load_preview(
            Target::Session(old),
            request_id,
            Some("artifacts/report.md".into()),
            PreviewFileFilter::Artifacts,
            completion,
        );
        if !queued {
            waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        fs::remove_dir_all(&row.root).unwrap();
        fs::create_dir_all(row.root.join("artifacts")).unwrap();
        fs::write(&path, "NEW").unwrap();
        port.sync_session_roots(vec![(SessionId::new(), row.root)]);
        release.send(()).unwrap();
        assert!(matches!(wait_preview(&mut port),
            AppEvent::Backend(BackendEvent::PreviewError { target: Target::Session(id), request_id: received, error, .. })
                if id == old && received == request_id && error.error_id == "preview-scope"
        ));
        assert!(immediate.try_recv().is_err());
        assert!(
            waiting.try_recv().is_err(),
            "the queued stale job must never read the replacement"
        );
    }
}

struct BlockingBrowser {
    opened: mpsc::Sender<Vec<u8>>,
    release: mpsc::Receiver<()>,
}

impl crate::runtime::artifact_opener::ArtifactViewer for BlockingBrowser {
    fn open(
        &mut self,
        path: &str,
    ) -> Result<usagi_tui::usecase::application::controller::ArtifactOpenOutcome, String> {
        self.opened.send(fs::read(path).unwrap()).unwrap();
        self.release.recv_timeout(Duration::from_secs(5)).unwrap();
        Err("test helper failure".into())
    }
}

#[test]
fn queued_native_requests_never_dispatch_a_replacement_session() {
    let root = tempfile::tempdir().unwrap();
    let workspace = WorkspaceId::new();
    let old = SessionId::new();
    let row = record(root.path(), "same");
    for scope in [root.path(), &row.root] {
        fs::create_dir_all(scope.join("artifacts")).unwrap();
        fs::write(scope.join("artifacts/report.md"), "OLD").unwrap();
    }
    let (mut port, _) = port(root.path(), workspace, &[(old, row.clone())]);
    let (opened, received) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    port.artifact_opener = crate::runtime::artifact_opener::ArtifactOpener::spawn(
        Box::new(BlockingBrowser {
            opened,
            release: gate,
        }),
        access(root.path(), workspace, Arc::clone(&port.sessions)),
    );
    let (first_completion, first_events) = Completions::channel();
    port.open_artifact(
        Target::Root(workspace),
        RequestId::new(),
        "artifacts/report.md".into(),
        PreviewFileFilter::Artifacts,
        first_completion,
    );
    assert_eq!(
        received.recv_timeout(Duration::from_secs(5)).unwrap(),
        b"OLD"
    );
    let request_id = RequestId::new();
    let (completion, events) = Completions::channel();
    port.open_artifact(
        Target::Session(old),
        request_id,
        "artifacts/report.md".into(),
        PreviewFileFilter::Artifacts,
        completion,
    );
    fs::remove_dir_all(&row.root).unwrap();
    fs::create_dir_all(row.root.join("artifacts")).unwrap();
    fs::write(row.root.join("artifacts/report.md"), "NEW").unwrap();
    port.sync_session_roots(vec![(SessionId::new(), row.root)]);
    release.send(()).unwrap();
    let _first = first_events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(events.recv_timeout(Duration::from_secs(5)).unwrap(),
            AppEvent::Backend(BackendEvent::ArtifactOpenFinished { target: Target::Session(id), request_id: received, result: Err(error), .. })
                if id == old && received == request_id && error.error_id == "preview-scope"
        )
    );
    assert!(
        received.try_recv().is_err(),
        "the stale request must not reach an external app"
    );
}

#[test]
fn artifacts_for_new_and_recreated_sessions_use_current_roots_without_reopening() {
    let root = tempfile::tempdir().unwrap();
    let workspace = WorkspaceId::new();
    let old = SessionId::new();
    let initial = record(root.path(), "initial");
    let (mut port, opened) = port(root.path(), workspace, &[(old, initial.clone())]);
    let late = SessionId::new();
    let late_record = record(root.path(), "late");
    fs::create_dir_all(late_record.root.join("artifacts")).unwrap();
    fs::write(
        late_record.root.join("artifacts/report.png"),
        "late artifact",
    )
    .unwrap();
    fs::write(late_record.root.join("source.txt"), "late source").unwrap();
    port.sync_session_roots(vec![(old, initial.root), (late, late_record.root.clone())]);
    assert!(
        matches!(load(&mut port, Target::Session(late), PreviewFileFilter::Artifacts), AppEvent::Backend(BackendEvent::PreviewLoaded { files, .. }) if files == ["artifacts/report.png"])
    );
    assert!(
        matches!(load(&mut port, Target::Root(workspace), PreviewFileFilter::AllArtifacts), AppEvent::Backend(BackendEvent::PreviewLoaded { files, .. }) if files == [".usagi/sessions/late/artifacts/report.png"])
    );

    let (completions, immediate) = Completions::channel();
    let id = RequestId::new();
    port.load_preview(
        Target::Session(late),
        id,
        Some("source.txt".into()),
        PreviewFileFilter::All,
        completions,
    );
    let (completions, events) = Completions::channel();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        port.poll(&completions);
        if let Some(event) = immediate.try_iter().chain(events.try_iter()).find(|event| matches!(event, AppEvent::Backend(BackendEvent::PreviewLoaded { request_id, .. }) if *request_id == id)) {
            assert!(matches!(event, AppEvent::Backend(BackendEvent::PreviewLoaded { lines, .. }) if lines == ["late source"]));
            break;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let id = RequestId::new();
    assert!(
        matches!(native(&mut port, Target::Session(late), "artifacts/report.png", id), AppEvent::Backend(BackendEvent::ArtifactOpenFinished { request_id, result: Err(error), .. }) if request_id == id && error.error_id == "artifact-viewer-open")
    );
    assert_eq!(*opened.lock().unwrap(), vec![b"late artifact".to_vec()]);

    let recreated = SessionId::new();
    fs::write(
        late_record.root.join("artifacts/report.png"),
        "recreated artifact",
    )
    .unwrap();
    port.sync_session_roots(vec![(recreated, late_record.root)]);
    assert!(
        matches!(load(&mut port, Target::Session(late), PreviewFileFilter::Artifacts), AppEvent::Backend(BackendEvent::PreviewError { error, .. }) if error.error_id == "preview-file")
    );
    let id = RequestId::new();
    assert!(
        matches!(native(&mut port, Target::Session(late), "artifacts/report.png", id), AppEvent::Backend(BackendEvent::ArtifactOpenFinished { request_id, result: Err(error), .. }) if request_id == id && error.error_id == "preview-file")
    );
    assert!(
        matches!(load(&mut port, Target::Session(recreated), PreviewFileFilter::Artifacts), AppEvent::Backend(BackendEvent::PreviewLoaded { files, .. }) if files == ["artifacts/report.png"])
    );
    let _ = native(
        &mut port,
        Target::Session(recreated),
        "artifacts/report.png",
        RequestId::new(),
    );
    assert_eq!(
        opened.lock().unwrap().last().unwrap(),
        b"recreated artifact"
    );
    port.sync_session_roots(Vec::new());
    assert!(port.target_root(Target::Session(recreated)).is_err());
}

#[test]
fn native_artifact_error_reaches_the_visible_home_and_text_preview_without_losing_the_list() {
    let root = tempfile::tempdir().unwrap();
    let workspace = WorkspaceId::new();
    let session = SessionId::new();
    let row = record(root.path(), "reports");
    fs::create_dir_all(row.root.join("artifacts")).unwrap();
    fs::write(row.root.join("artifacts/tests.log"), "all tests passed").unwrap();
    let (mut port, opened) = port(root.path(), workspace, &[(session, row.clone())]);
    let mut state = AppState::home(workspace, vec![session]);
    let _ = update(&mut state, AppEvent::Key(AppKey::OpenArtifacts));
    let request_id = state.preview_overlay().unwrap().request_id();
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PreviewLoaded {
            target: Target::Session(session),
            request_id,
            path: None,
            filter: PreviewFileFilter::Artifacts,
            files: vec!["artifacts/tests.log".into()],
            changed: vec![],
            lines: vec![],
        }),
    );
    let effects = update(&mut state, AppEvent::Key(AppKey::Enter));
    let id = match effects.as_slice() {
        [Effect::OpenArtifact { request_id, .. }] => *request_id,
        _ => panic!("native open missing"),
    };
    let event = native(
        &mut port,
        Target::Session(session),
        "artifacts/tests.log",
        id,
    );
    assert!(
        matches!(&event, AppEvent::Backend(BackendEvent::ArtifactOpenFinished { target: Target::Session(id), request_id, path, filter: PreviewFileFilter::Artifacts, result: Err(error) }) if *id == session && *request_id == state.preview_overlay().unwrap().artifact_open().unwrap().request_id() && path == "artifacts/tests.log" && error.error_id == "preview-native-format")
    );
    let _ = update(&mut state, event);
    let rows = vec![ProjectedSession::from_record(session, &row)];
    for (width, height) in [(50, 18), (100, 26), (160, 40)] {
        let frame = render_home(
            height,
            width,
            &HomeProjection::from_state(&state, "artifacts-test", &rows),
        )
        .join("\n");
        assert!(frame.contains("cannot be opened"), "{frame}");
        assert!(frame.contains("Tab preview"));
        if width >= 100 {
            assert!(frame.contains("Press Tab for a text preview."));
        }
    }
    assert_eq!(
        state.preview_overlay().unwrap().selected_file(),
        Some("artifacts/tests.log")
    );
    assert_eq!(state.preview_overlay().unwrap().visible_len(), 1);
    assert!(state.preview_overlay().unwrap().error().is_none());
    let effects = update(&mut state, AppEvent::Key(AppKey::Tab));
    let request_id = match effects.as_slice() {
        [Effect::LoadPreview { request_id, .. }] => *request_id,
        _ => panic!("text preview missing"),
    };
    let (_, _, lines) = load_preview(
        &row.root,
        Some("artifacts/tests.log"),
        PreviewFileFilter::Artifacts,
    )
    .unwrap();
    let _ = update(
        &mut state,
        AppEvent::Backend(BackendEvent::PreviewLoaded {
            target: Target::Session(session),
            request_id,
            path: Some("artifacts/tests.log".into()),
            filter: PreviewFileFilter::Artifacts,
            files: vec![],
            changed: vec![],
            lines,
        }),
    );
    let frame = render_home(
        26,
        100,
        &HomeProjection::from_state(&state, "artifacts-test", &rows),
    )
    .join("\n");
    assert!(frame.contains("all tests passed"));
    assert!(frame.contains("cannot be opened externally"));
    assert!(
        !frame.contains("Press Tab for a text preview."),
        "Tab in the document opens an app, not a text preview"
    );
    assert!(
        opened.lock().unwrap().is_empty(),
        "an unsupported file must never reach the browser"
    );
}
