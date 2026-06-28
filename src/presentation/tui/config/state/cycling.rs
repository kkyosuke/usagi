//! Value-cycling logic for the configuration screen: advancing each field to
//! its next/previous choice, plus the small pure helpers that wrap individual
//! enums and string lists. Split from the parent module to keep each file
//! focused on one concern.

use std::collections::BTreeMap;

use super::*;

impl Config {
    /// Advance the selected field's value to its next (or previous) choice,
    /// wrapping. The edit is held in memory only — nothing is persisted until
    /// the user saves. Returns `true` when a value actually changed, and `false`
    /// when there was nothing to cycle (the Save button, or a default-workspace
    /// field with no registered workspaces).
    pub fn cycle_selected(&mut self, forward: bool) -> bool {
        if let Some(field) = self.selected_field() {
            return self.cycle_global(field, forward);
        }
        if let Some(field) = self.selected_local_field() {
            return self.cycle_local(field, forward);
        }
        if let Some(feature) = self.selected_skill_feature() {
            return self.cycle_skill_feature(feature, forward);
        }
        // The cursor is on the Save button: nothing to cycle.
        false
    }

    /// Toggle a shipped-skill feature row. In the global scope it flips the
    /// effective on/off, recording the value in the map only when it differs from
    /// the feature's default (so the map stays minimal and dirty-tracking stays
    /// consistent). In the local scope it cycles "follow global" → On → Off,
    /// storing or clearing the per-project override. Always reports a change.
    fn cycle_skill_feature(&mut self, feature: SkillFeature, forward: bool) -> bool {
        match self.scope {
            Scope::Global => {
                let next = !self.settings.skill_feature_enabled(feature);
                // Record the value only when it differs from the feature's
                // default; matching the default clears the entry so toggling back
                // leaves no stray key (keeps `is_dirty` honest, the map minimal).
                let stored = (next != feature.default_enabled()).then_some(next);
                set_skill_override(&mut self.settings.skill_features, feature, stored);
                true
            }
            Scope::Local => {
                let local = self.local_edit_mut();
                let current = local.settings.skill_feature_override(feature);
                let next = cycle_optional(current, &[true, false], forward);
                set_skill_override(&mut local.settings.skill_features, feature, next);
                true
            }
        }
    }

    /// Cycle a global field's value.
    fn cycle_global(&mut self, field: Field, forward: bool) -> bool {
        match field {
            Field::Theme => {
                self.settings.theme = cycle_theme(self.settings.theme, forward);
                true
            }
            Field::DefaultWorkspace => self.cycle_default_workspace(forward),
            Field::Notifications => {
                // A boolean toggle: direction is irrelevant, it always flips.
                self.settings.notifications_enabled = !self.settings.notifications_enabled;
                true
            }
            Field::RestorePanes => {
                // A boolean toggle: direction is irrelevant, it always flips.
                self.settings.restore_panes_enabled = !self.settings.restore_panes_enabled;
                true
            }
            Field::AgentCli => {
                // Only cycle through installed agents (the current value is always
                // kept selectable), so an uninstalled CLI is never offered.
                let choices = self.agent_cli_choices(Some(self.settings.agent_cli));
                self.settings.agent_cli = cycle_enum(self.settings.agent_cli, &choices, forward);
                true
            }
            Field::SessionActionUi => {
                self.settings.session_action_ui = cycle_enum(
                    self.settings.session_action_ui,
                    &SESSION_ACTION_UIS,
                    forward,
                );
                true
            }
            Field::KeyScheme => {
                self.settings.key_scheme =
                    cycle_enum(self.settings.key_scheme, &KEY_SCHEMES, forward);
                true
            }
            Field::MascotAnimation => {
                // A boolean toggle: direction is irrelevant, it always flips.
                self.settings.mascot_animation_enabled = !self.settings.mascot_animation_enabled;
                true
            }
            Field::LocalLlm => {
                // Only meaningful once installed: flip the on/off toggle. While
                // not installed the row is an install action handled by the
                // event layer, so there is nothing to cycle.
                if self.ollama_installed() {
                    self.settings.local_llm.enabled = !self.settings.local_llm.enabled;
                    true
                } else {
                    false
                }
            }
            // The model is no longer cycled with ←/→: it is chosen from the
            // picker modal (which also pulls an uninstalled choice), so arrows
            // are a no-op here and the event layer opens the modal instead.
            Field::LocalLlmModel => false,
        }
    }

    /// Cycle a local override field through "follow global" then each concrete
    /// value. Returns `true` when a value changed, and `false` when there was
    /// nothing to cycle (the Default Branch field with no branches to choose).
    fn cycle_local(&mut self, field: LocalField, forward: bool) -> bool {
        // The Default Branch cycles branch names, so it needs both the branch
        // list and the local edit; it is handled separately to keep the borrows
        // disjoint. Every other local field cycles a fixed set in place.
        match field {
            LocalField::DefaultBranch => self.cycle_default_branch(forward),
            LocalField::AgentCli => {
                // The override cycles "follow global" then each installed agent;
                // an already-set override is kept selectable even if uninstalled.
                let keep = self.local.as_ref().and_then(|l| l.settings.agent_cli);
                let choices = self.agent_cli_choices(keep);
                let local = self.local_edit_mut();
                local.settings.agent_cli =
                    cycle_optional(local.settings.agent_cli, &choices, forward);
                true
            }
            LocalField::Notifications => {
                let local = self.local_edit_mut();
                local.settings.notifications_enabled = cycle_optional(
                    local.settings.notifications_enabled,
                    &[true, false],
                    forward,
                );
                true
            }
            LocalField::RestorePanes => {
                let local = self.local_edit_mut();
                local.settings.restore_panes_enabled = cycle_optional(
                    local.settings.restore_panes_enabled,
                    &[true, false],
                    forward,
                );
                true
            }
            LocalField::BranchSource => {
                // Local-only setting: toggle between the two concrete sources,
                // treating an unset value as the default. It is always stored.
                let local = self.local_edit_mut();
                let current = local.settings.default_branch_source.unwrap_or_default();
                local.settings.default_branch_source =
                    Some(cycle_enum(current, &BRANCH_SOURCES, forward));
                true
            }
        }
    }

    /// The local edit being modified. Only called from a local-field cycle,
    /// which is reachable solely when a local context exists.
    fn local_edit_mut(&mut self) -> &mut LocalEdit {
        self.local
            .as_mut()
            .expect("a local field is only selectable with a local context")
    }

    /// Cycle the local Default Branch through "auto" (the detected default) then
    /// each of the repository's branches, wrapping. A no-op (returns `false`)
    /// when no branches are available to choose from.
    fn cycle_default_branch(&mut self, forward: bool) -> bool {
        if self.branches.is_empty() {
            return false;
        }
        let local = self
            .local
            .as_mut()
            .expect("a local field is only selectable with a local context");
        // The choices are "auto" (`None`, index 0) followed by each branch name.
        let len = self.branches.len() + 1;
        let current = match &local.settings.default_branch {
            None => 0,
            // A branch that is no longer present behaves like "auto".
            Some(name) => self
                .branches
                .iter()
                .position(|b| b == name)
                .map_or(0, |i| i + 1),
        };
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        local.settings.default_branch = if next == 0 {
            None
        } else {
            Some(self.branches[next - 1].clone())
        };
        true
    }

    /// Cycle the default workspace through `None` then each registered name.
    /// A no-op (returns `false`) when no workspaces are registered.
    fn cycle_default_workspace(&mut self, forward: bool) -> bool {
        if self.workspaces.is_empty() {
            return false;
        }
        // The choices are `None` (index 0) followed by each workspace name.
        let len = self.workspaces.len() + 1;
        let current = match &self.settings.default_workspace {
            None => 0,
            // An unknown name (e.g. a since-removed workspace) behaves like None.
            Some(name) => self
                .workspaces
                .iter()
                .position(|w| w == name)
                .map_or(0, |i| i + 1),
        };
        let next = if forward {
            (current + 1) % len
        } else {
            (current + len - 1) % len
        };
        self.settings.default_workspace = if next == 0 {
            None
        } else {
            Some(self.workspaces[next - 1].clone())
        };
        true
    }
}

/// The theme one step after `theme` in cycle order (or before, when `forward`
/// is false), wrapping at the ends.
fn cycle_theme(theme: Theme, forward: bool) -> Theme {
    let i = THEMES.iter().position(|&t| t == theme).unwrap_or(0);
    let len = THEMES.len();
    let next = if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    };
    THEMES[next]
}

/// The value one step after `current` in `choices` (or before, when `forward` is
/// false), wrapping at the ends. Used for a fixed, non-optional set of choices.
fn cycle_enum<T: Copy + PartialEq>(current: T, choices: &[T], forward: bool) -> T {
    let i = choices.iter().position(|&c| c == current).unwrap_or(0);
    let len = choices.len();
    let next = if forward {
        (i + 1) % len
    } else {
        (i + len - 1) % len
    };
    choices[next]
}

/// Apply a skill-feature override to `map`: store the boolean under the
/// feature's id, or remove the entry when `None` (defer to the default / global).
fn set_skill_override(
    map: &mut BTreeMap<String, bool>,
    feature: SkillFeature,
    value: Option<bool>,
) {
    match value {
        Some(enabled) => {
            map.insert(feature.id().to_string(), enabled);
        }
        None => {
            map.remove(feature.id());
        }
    }
}

/// Cycle an optional override through `None` (follow global) then each value in
/// `choices`, wrapping. Forward order is `None → choices[0] → … → None`.
fn cycle_optional<T: Copy + PartialEq>(
    current: Option<T>,
    choices: &[T],
    forward: bool,
) -> Option<T> {
    // Index 0 is `None`; indices 1.. map onto `choices`.
    let len = choices.len() + 1;
    let current_index = match current {
        None => 0,
        Some(value) => choices
            .iter()
            .position(|&c| c == value)
            .map_or(0, |i| i + 1),
    };
    let next = if forward {
        (current_index + 1) % len
    } else {
        (current_index + len - 1) % len
    };
    if next == 0 {
        None
    } else {
        Some(choices[next - 1])
    }
}
