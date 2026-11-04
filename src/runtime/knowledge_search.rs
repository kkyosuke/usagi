//! Workspace-scoped adapters over the existing Issue and Memory searches.
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use usagi_core::infrastructure::store::{issue::IssueStore, memory::MemoryStore};
use usagi_core::usecase::{issue, memory};
use usagi_tui::usecase::application::knowledge_search::{
    Document, Kind, KnowledgeSearchPort, RESULT_LIMIT, Results,
};

pub(crate) struct KnowledgeSearch {
    root: PathBuf,
}

impl KnowledgeSearch {
    pub(crate) fn new(root: &Path) -> Self {
        Self { root: root.into() }
    }
}

impl KnowledgeSearchPort for KnowledgeSearch {
    fn search(&mut self, query: &str, cancelled: Arc<AtomicBool>) -> Results {
        let mut result = Results::default();
        if cancelled.load(Ordering::Acquire) {
            return result;
        }
        match issue::search(
            &IssueStore::new(&self.root).with_lock_cancellation(Arc::clone(&cancelled)),
            query,
            &issue::IssueFilter::default(),
        ) {
            Ok(mut issues) => {
                issues.sort_by(|a, b| {
                    b.summary
                        .updated_at
                        .cmp(&a.summary.updated_at)
                        .then(a.summary.file.cmp(&b.summary.file))
                });
                result.omitted += issues.len().saturating_sub(RESULT_LIMIT);
                result
                    .documents
                    .extend(issues.into_iter().take(RESULT_LIMIT).map(|item| {
                        let readiness = if item.ambiguous {
                            "ambiguous"
                        } else if item.is_ready() {
                            "ready"
                        } else {
                            "not ready"
                        };
                        Document {
                            kind: Kind::Issue,
                            path: format!(".usagi/issues/{}", item.summary.file),
                            title: format!("#{} {}", item.summary.number, item.summary.title),
                            detail: format!(
                                "{} · {} · {readiness}",
                                item.summary.status.as_str(),
                                item.summary.priority.as_str()
                            ),
                        }
                    }));
            }
            Err(_) => result
                .errors
                .push("Issue store is unavailable; refresh to retry".into()),
        }
        if cancelled.load(Ordering::Acquire) {
            return Results::default();
        }
        match memory::search(
            &MemoryStore::new(&self.root).with_lock_cancellation(Arc::clone(&cancelled)),
            query,
            &memory::MemoryFilter::default(),
        ) {
            Ok(memories) => {
                result.omitted += memories.len().saturating_sub(RESULT_LIMIT);
                result
                    .documents
                    .extend(
                        memories
                            .into_iter()
                            .take(RESULT_LIMIT)
                            .map(|item| Document {
                                kind: Kind::Memory,
                                path: format!(".usagi/memory/{}", item.file),
                                title: format!("{} · {}", item.name, item.title),
                                detail: item.kind.as_str().into(),
                            }),
                    );
            }
            Err(_) => result
                .errors
                .push("Memory store is unavailable; refresh to retry".into()),
        }
        if cancelled.load(Ordering::Acquire) {
            return Results::default();
        }
        result
    }

    fn read(&mut self, path: &str) -> Result<Vec<String>, String> {
        // Only exact immediate source filenames are admitted; the common
        // preview reader rejects symlinks, traversal, binary and oversized files.
        path.strip_prefix(".usagi/issues/")
            .or_else(|| path.strip_prefix(".usagi/memory/"))
            .filter(|file| {
                !file.is_empty()
                    && !file.contains('/')
                    && Path::new(file)
                        .extension()
                        .is_some_and(|extension| extension == "md")
            })
            .ok_or_else(|| "Invalid Issue / Memory source".to_owned())?;
        super::file_preview::read_file(&self.root, path).map_err(|error| error.message().to_owned())
    }
}

#[cfg(test)]
mod tests;
