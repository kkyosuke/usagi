use super::*;
use std::fs;
use usagi_core::domain::id::{SessionId, WorkspaceId};
use usagi_core::usecase::terminal_search::WorkspaceOutputSearch;
use usagi_tui::usecase::application::unified_search::{CurrentSession, Project};

#[derive(Default)]
struct Terminals {
    calls: Vec<(WorkspaceId, String, Option<SessionId>)>,
    mismatch: bool,
    cancel: Option<Arc<AtomicBool>>,
}
impl GardenInventoryPort for Terminals {
    fn inventory(
        &mut self,
        _: WorkspaceId,
    ) -> Result<usagi_core::domain::agent::AgentWorkspaceObservation, String> {
        unreachable!("search must not observe Agent inventory")
    }
    fn search_terminal_output(
        &mut self,
        workspace: WorkspaceId,
        query: &str,
        session: Option<SessionId>,
    ) -> Result<WorkspaceOutputSearch, String> {
        self.calls.push((workspace, query.into(), session));
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Release);
        }
        Ok(WorkspaceOutputSearch {
            workspace,
            query: query.into(),
            session: if self.mismatch { None } else { session },
            terminals: vec![],
            omitted_terminals: 0,
        })
    }
}
fn query(root: &Path) -> Query {
    Query {
        text: "report".into(),
        scope: Scope::All,
        projects: vec![Project {
            workspace: WorkspaceId::new(),
            root: root.into(),
        }],
        current: None,
    }
}
fn file(root: &Path, path: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "body").unwrap();
}

#[test]
fn unified_search_combines_sources_and_isolates_current_session_before_project_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    file(root, ".usagi/sessions/one/artifacts/report.txt");
    file(root, ".usagi/sessions/two/artifacts/report.txt");
    file(
        root,
        ".usagi/artifacts/one/00000000-0000-0000-0000-000000000001/report.txt",
    );
    file(root, "report-secret.txt");
    let mut port = UnifiedSearch::new(root, Box::<Terminals>::default());
    let mut query = query(root);
    let result = port.search(&query, Arc::default());
    assert_eq!(result.artifacts.len(), 3);
    assert_eq!(result.outputs.len(), 1);
    assert!(result.errors.is_empty());
    assert!(
        result
            .artifacts
            .iter()
            .all(|file| file.target == Target::Root(query.projects[0].workspace))
    );
    query.scope = Scope::Terminal;
    assert!(port.search(&query, Arc::default()).artifacts.is_empty());
    query.scope = Scope::Artifact;
    assert!(port.search(&query, Arc::default()).outputs.is_empty());
    query.text = "missing".into();
    assert!(port.search(&query, Arc::default()).artifacts.is_empty());
    query.text = "report".into();
    query.scope = Scope::CurrentSession;
    assert_eq!(port.search(&query, Arc::default()).errors.len(), 1);
    let current = CurrentSession {
        workspace: query.projects[0].workspace,
        session: SessionId::new(),
        root: root.join(".usagi/sessions/one"),
    };
    query.current = Some(current.clone());
    for _ in 0..=PROJECT_LIMIT {
        query.projects.insert(
            0,
            Project {
                workspace: WorkspaceId::new(),
                root: root.into(),
            },
        );
    }
    let result = port.search(&query, Arc::default());
    assert_eq!(result.omitted, 0);
    assert_eq!(result.artifacts.len(), 1);
    assert_eq!(result.artifacts[0].path, "artifacts/report.txt");
    assert_eq!(result.artifacts[0].target, Target::Session(current.session));
    assert_eq!(
        result.outputs[0].1.as_ref().unwrap().session,
        Some(current.session)
    );
    port.terminals = Box::new(Terminals {
        mismatch: true,
        ..Terminals::default()
    });
    assert!(port.search(&query, Arc::default()).outputs[0].1.is_err());
    query.scope = Scope::Terminal;
    assert_eq!(port.search(&query, Arc::default()).omitted, 2);
    assert!(port.read("../secret").is_err());
}

#[test]
fn unified_search_bounds_artifacts_reports_errors_and_cancels_between_sources() {
    let tmp = tempfile::tempdir().unwrap();
    for index in 0..RESULT_LIMIT + 2 {
        file(
            tmp.path(),
            &format!(".usagi/sessions/s/artifacts/report-{index}.txt"),
        );
    }
    let mut port = UnifiedSearch::new(tmp.path(), Box::<Terminals>::default());
    let mut query = query(tmp.path());
    query.scope = Scope::Artifact;
    query.text.clear();
    let result = port.search(&query, Arc::default());
    assert_eq!(result.artifacts.len(), RESULT_LIMIT);
    assert_eq!(result.omitted, 2);
    assert_eq!(
        port.search(&query, Arc::new(AtomicBool::new(true))),
        Results::default()
    );
    std::os::unix::fs::symlink(
        tmp.path().join("secret"),
        tmp.path().join(".usagi/sessions/s/artifacts/link"),
    )
    .unwrap();
    assert_eq!(port.search(&query, Arc::default()).errors.len(), 1);
    query.scope = Scope::All;
    query.text = "report".into();
    let cancelled = Arc::new(AtomicBool::new(false));
    port.terminals = Box::new(Terminals {
        cancel: Some(cancelled.clone()),
        ..Terminals::default()
    });
    assert_eq!(port.search(&query, cancelled), Results::default());
}
