//! Reuse the bounded terminal, artifact and knowledge readers on one worker.
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use usagi_tui::usecase::application::{
    controller::Target,
    knowledge_search::KnowledgeSearchPort,
    runtime_ports::GardenInventoryPort,
    unified_search::{
        Artifact, PROJECT_LIMIT, Query, RESULT_LIMIT, Results, Scope, UnifiedSearchPort,
    },
};

pub(crate) struct UnifiedSearch {
    knowledge: super::knowledge_search::KnowledgeSearch,
    terminals: Box<dyn GardenInventoryPort>,
}

impl UnifiedSearch {
    pub(crate) fn new(root: &Path, terminals: Box<dyn GardenInventoryPort>) -> Self {
        Self {
            knowledge: super::knowledge_search::KnowledgeSearch::new(root),
            terminals,
        }
    }
}

impl UnifiedSearchPort for UnifiedSearch {
    fn search(&mut self, query: &Query, cancelled: Arc<AtomicBool>) -> Results {
        let mut results = Results::default();
        if query.scope == Scope::CurrentSession && query.current.is_none() {
            results
                .errors
                .push("No current session. Select a session and reopen search.".into());
            return results;
        }
        let current = (query.scope == Scope::CurrentSession)
            .then_some(query.current.as_ref())
            .flatten();
        let projects = query
            .projects
            .iter()
            .filter(|project| current.is_none_or(|current| current.workspace == project.workspace))
            .collect::<Vec<_>>();
        results.omitted = projects.len().saturating_sub(PROJECT_LIMIT);
        for project in projects.into_iter().take(PROJECT_LIMIT) {
            if cancelled.load(Ordering::Acquire) {
                return Results::default();
            }
            if query.scope != Scope::Artifact && !query.text.is_empty() {
                let session = current.map(|current| current.session);
                let response = self
                    .terminals
                    .search_terminal_output(project.workspace, &query.text, session)
                    .and_then(|response| {
                        if response.workspace == project.workspace
                            && response.query == query.text
                            && response.session == session
                        {
                            Ok(response)
                        } else {
                            Err(
                                "Terminal search scope changed or is unsupported; refresh to retry"
                                    .into(),
                            )
                        }
                    });
                results.outputs.push((project.workspace, response));
            }
            if cancelled.load(Ordering::Acquire) {
                return Results::default();
            }
            if query.scope != Scope::Terminal {
                let (root, target, all) = current.map_or(
                    (
                        project.root.as_path(),
                        Target::Root(project.workspace),
                        true,
                    ),
                    |current| {
                        (
                            current.root.as_path(),
                            Target::Session(current.session),
                            false,
                        )
                    },
                );
                match usagi_core::infrastructure::artifacts::list(root, all) {
                    Ok(paths) => {
                        let mut matches = paths
                            .into_iter()
                            .filter_map(|path| {
                                usagi_tui::usecase::application::unified_search::artifact_score(
                                    &path,
                                    &query.text,
                                )
                                .map(|score| (score, path))
                            })
                            .collect::<Vec<_>>();
                        matches.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
                        let remaining = RESULT_LIMIT.saturating_sub(results.artifacts.len());
                        results.omitted += matches.len().saturating_sub(remaining);
                        results
                            .artifacts
                            .extend(matches.into_iter().take(remaining).map(|(_, path)| {
                                Artifact {
                                    workspace: project.workspace,
                                    project_root: project.root.clone(),
                                    target,
                                    path,
                                }
                            }));
                    }
                    Err(_) => results.errors.push(format!(
                        "Artifacts unavailable in {}",
                        project.root.display()
                    )),
                }
            }
        }
        if query.scope == Scope::All && !cancelled.load(Ordering::Acquire) {
            results.knowledge = self.knowledge.search(&query.text, Arc::clone(&cancelled));
        }
        if cancelled.load(Ordering::Acquire) {
            return Results::default();
        }
        results
    }

    fn read(&mut self, path: &str) -> Result<Vec<String>, String> {
        self.knowledge.read(path)
    }
}

#[cfg(test)]
mod tests;
