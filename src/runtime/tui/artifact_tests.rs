//! Regression paths through the production overlay ports, workers and Home view.
#![coverage(off)] // coverage: reason=composition owner=tui expires=2027-01-31 tests=module_unit_contract

use super::*;
use std::fs;
use usagi_core::domain::workspace_state::WorkspaceState;
use usagi_tui::presentation::views::workspace::{HomeProjection, ProjectedSession, render_home};
use usagi_tui::usecase::application::controller::{AppState, Effect, update};

struct Browser {
    opened: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl BrowserOpener for Browser {
    fn open(&mut self, path: &str) -> Result<(), String> {
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
    let port = ProductionOverlayPort {
        root: root.to_path_buf(),
        sessions: project_backend_session_roots(&snapshot),
        pr_sessions: Arc::new(Mutex::new(snapshot.session_ids)),
        pr_pump: RefreshPump::spawn(
            RefreshCadence::new(
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            || Ok(Vec::new()),
        ),
        preview_pump: PreviewPump::spawn(load_preview),
        artifact_opener: crate::runtime::artifact_opener::ArtifactOpener::spawn(Box::new(
            Browser {
                opened: Arc::clone(&opened),
            },
        )),
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
