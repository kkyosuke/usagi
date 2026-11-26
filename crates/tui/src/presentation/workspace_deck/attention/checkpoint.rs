//! Per-workspace comparison with the last explicitly visited Changes view.
use super::WorkspaceDeck;
use std::{collections::BTreeMap, path::PathBuf};
use usagi_core::domain::attention::{AttentionItem, AttentionTarget, WorkspaceAttention};
use usagi_core::infrastructure::persistence::json_file;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(in crate::presentation::workspace_deck) struct Checkpoint(BTreeMap<String, String>);

impl Checkpoint {
    pub(super) fn contains(&self, item: &AttentionItem) -> bool {
        self.0.get(&item.key) == Some(&item.reason)
    }
    fn capture(snapshot: &WorkspaceAttention) -> Self {
        Self(
            snapshot
                .items
                .iter()
                .filter(|i| {
                    i.kind.needs_action()
                        || matches!(i.target, Some(AttentionTarget::Result { .. }))
                })
                .map(|i| (i.key.clone(), i.reason.clone()))
                .collect(),
        )
    }
}

impl WorkspaceDeck {
    pub(crate) fn set_attention_cache(&mut self, path: Option<PathBuf>) {
        self.attention_cache = path;
    }
    pub(in crate::presentation::workspace_deck) fn begin_attention_check(&mut self) {
        let Some(dir) = &self.attention_cache else {
            return;
        };
        for slot in &mut self.slots {
            let path = dir.join(format!("{}.json", slot.workspace_id));
            let Ok(last) = json_file::read_bounded::<Checkpoint>(&path, 2 * 1024 * 1024) else {
                self.notice = Some("Could not read the previous check.".into());
                continue;
            };
            slot.checked_attention = last.unwrap_or_default();
            let Some(snapshot) = slot.attention.as_ref().filter(|_| !slot.attention_stale) else {
                continue;
            };
            if json_file::write_atomic_cache(dir, &path, &Checkpoint::capture(snapshot)).is_err() {
                self.notice = Some("Could not save this check.".into());
            }
        }
        if let Some(super::DeckOverlay::Attention(list)) = &mut self.overlay {
            list.select_first(&self.slots);
        }
    }
}
