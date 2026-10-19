use super::*;
use std::sync::mpsc;
use usagi_core::domain::agent::AgentWorkspaceObservation;
use usagi_core::usecase::terminal_search::WorkspaceOutputSearch;

struct Port {
    entered: Sender<WorkspaceId>,
    release: mpsc::Receiver<bool>,
}
impl GardenInventoryPort for Port {
    fn inventory(&mut self, _: WorkspaceId) -> Result<AgentWorkspaceObservation, String> {
        Err("unused".into())
    }
    fn search_terminal_output(
        &mut self,
        workspace: WorkspaceId,
        query: &str,
    ) -> Result<WorkspaceOutputSearch, String> {
        self.entered.send(workspace).unwrap();
        assert!(
            !self.release.recv().unwrap(),
            "simulated unavailable reader"
        );
        Ok(WorkspaceOutputSearch {
            workspace,
            query: query.into(),
            terminals: vec![],
            omitted_terminals: 0,
        })
    }
}

#[test]
fn output_search_worker_is_nonblocking_cancels_between_workspaces_and_returns_port() {
    let (entered, calls) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (sender, completions) = mpsc::channel();
    let first = WorkspaceId::new();
    let job = spawn(
        Box::new(Port {
            entered,
            release: gate,
        }),
        (1, 1),
        "error".into(),
        vec![first, WorkspaceId::new()],
        sender.clone(),
    );
    assert_eq!(
        calls
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        first
    );
    job.cancel();
    release.send(false).unwrap();
    let completion = completions
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    drop(job);
    assert_eq!(completion.token, (1, 1));
    assert_eq!(completion.responses.len(), 1);
    assert_eq!(completion.responses[0].1.as_ref().unwrap().query, "error");
    let job = spawn(completion.port, (1, 2), "next".into(), vec![first], sender);
    assert_eq!(
        calls
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        first
    );
    release.send(true).unwrap();
    let completion = completions
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    drop(job);
    assert!(completion.responses[0].1.is_err());
}
