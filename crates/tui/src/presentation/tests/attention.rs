use super::*;
use crate::presentation::WorkspaceStep;
use usagi_core::domain::attention::{AttentionItem, AttentionKind, WorkspaceAttention};

struct AttentionFactory {
    inner: CountingBackendFactory,
    attention: WorkspaceAttention,
    pr_requests: Arc<Mutex<Vec<Target>>>,
}

struct AttentionPort(WorkspaceAttention);
impl GardenInventoryPort for AttentionPort {
    fn inventory(&mut self, _: WorkspaceId) -> Result<AgentWorkspaceObservation, String> {
        Err("unused".into())
    }
    fn attention(&mut self, _: WorkspaceId) -> Result<WorkspaceAttention, String> {
        Ok(self.0.clone())
    }
}

impl crate::presentation::ControllerBackendFactory for AttentionFactory {
    fn create(
        &mut self,
        snapshot: &WorkspaceSnapshot,
        host: ControllerHost,
    ) -> crate::presentation::ControllerBackendComposition {
        let mut composition = self.inner.create(snapshot, host);
        composition.garden_inventory = Box::new(AttentionPort(self.attention.clone()));
        composition.backend = composition
            .backend
            .with_overlay(Box::new(AttentionPrRequests(Arc::clone(&self.pr_requests))));
        composition
    }
}

#[test]
fn attention_shortcut_and_enter_open_the_selected_session_decisions() {
    let snapshot = snapshot("attention-project");
    let mut deck = WorkspaceDeck::new(&snapshot);
    let attention = WorkspaceAttention {
        workspace: snapshot.workspace_id,
        items: vec![AttentionItem {
            key: "decision:one".into(),
            session: Some(snapshot.session_ids[0]),
            label: "selected-session".into(),
            kind: AttentionKind::Decision,
            reason: "Choose deployment target".into(),
        }],
    };
    deck.apply_attention(snapshot.workspace_id, Ok(attention.clone()));
    let mut factory = AttentionFactory {
        inner: CountingBackendFactory::new(),
        attention,
        pr_requests: Arc::default(),
    };
    let mut term = FakeTerminal::with_keys(&[
        Key::Live(LiveTerminalAction::OpenAttention),
        Key::Enter,
        Key::Escape,
        Key::CtrlQ,
        Key::Char('y'),
    ]);
    term.size = Some((30, 140));
    let result = crate::presentation::frame_loop::drive_workspace_controller(
        &mut term,
        snapshot,
        &mut deck,
        &mut SessionCommandLane::new(),
        &[],
        None,
        &mut factory,
        usagi_core::domain::settings::ModalSelectionMode::Action,
        usagi_core::domain::settings::PrAutoOpen::default(),
        crate::presentation::WorkspaceEntryPolicy::default(),
        None,
    )
    .unwrap();
    assert!(matches!(result, WorkspaceStep::Quit));
    let frames = term.frames.iter().map(|f| f.join("\n")).collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .any(|f| f.contains("All projects") && f.contains("Choose deployment target")),
        "{frames:#?}"
    );
    assert!(frames.iter().any(|f| f.contains("Pending decisions")));
}

struct AttentionPrRequests(Arc<Mutex<Vec<Target>>>);
impl crate::presentation::BackendOverlayPort for AttentionPrRequests {
    fn load_pull_requests(&mut self, target: Target, _: Completions) {
        self.0.lock().unwrap().push(target);
    }
    fn load_preview(
        &mut self,
        _: Target,
        _: RequestId,
        _: Option<String>,
        _: PreviewFileFilter,
        _: Completions,
    ) {
    }
    fn open_pull_request(&mut self, _: String, _: Completions) {}
}

#[test]
fn attention_pr_visit_never_reuses_the_previous_active_session_when_target_is_unusable() {
    use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};
    for lifecycle in [
        SessionLifecycle::Available,
        SessionLifecycle::Creating,
        SessionLifecycle::Failed,
    ] {
        let mut snapshot = snapshot_with_sessions("attention-pr", &["active", "target"]);
        let target = snapshot.session_ids[1];
        snapshot.session_lifecycles.insert(
            target,
            SessionLifecycleProjection {
                lifecycle,
                failure_stage: None,
                failure_summary: None,
            },
        );
        let mut deck = WorkspaceDeck::new(&snapshot);
        let attention = WorkspaceAttention {
            workspace: snapshot.workspace_id,
            items: vec![AttentionItem {
                key: "pr:target".into(),
                session: Some(target),
                label: "target".into(),
                kind: AttentionKind::Review,
                reason: "Review target PR".into(),
            }],
        };
        deck.apply_attention(snapshot.workspace_id, Ok(attention.clone()));
        let pr_requests = Arc::default();
        let mut factory = AttentionFactory {
            inner: CountingBackendFactory::new(),
            attention,
            pr_requests: Arc::clone(&pr_requests),
        };
        let mut term = FakeTerminal::with_keys(&[
            Key::Enter,
            Key::Live(LiveTerminalAction::OpenAttention),
            Key::Enter,
            Key::CtrlQ,
            Key::Char('y'),
        ]);
        term.size = Some((30, 140));
        let result = crate::presentation::frame_loop::drive_workspace_controller(
            &mut term,
            snapshot,
            &mut deck,
            &mut SessionCommandLane::new(),
            &[],
            None,
            &mut factory,
            usagi_core::domain::settings::ModalSelectionMode::Action,
            usagi_core::domain::settings::PrAutoOpen::default(),
            crate::presentation::WorkspaceEntryPolicy::default(),
            None,
        )
        .unwrap();
        assert!(matches!(result, WorkspaceStep::Quit));
        let requested = pr_requests.lock().unwrap().clone();
        assert_eq!(
            requested,
            if lifecycle == SessionLifecycle::Available {
                vec![Target::Session(target)]
            } else {
                vec![]
            },
            "{lifecycle:?}"
        );
    }
}
