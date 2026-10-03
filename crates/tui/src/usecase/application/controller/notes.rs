//! Session memo editing, with request fences and explicit unsaved-close choices.

use super::{
    AppKey, AppState, Effect, HomeMode, Notice, Overlay, Route, SafeError, Selection, Target,
};
use crate::usecase::application::environment_source::EnvironmentSourceEditor;
use usagi_core::domain::id::{RequestId, SessionId};
use usagi_core::domain::note::Scratchpad;
use usagi_core::domain::presentation_text::presentation_character_is_safe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSection {
    Note,
    Todos,
    Decisions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteCloseChoice {
    Save,
    Discard,
    KeepEditing,
}

impl NoteCloseChoice {
    fn shifted(self, forward: bool) -> Self {
        match (self, forward) {
            (Self::Save, true) | (Self::KeepEditing, false) => Self::Discard,
            (Self::Discard, true) | (Self::Save, false) => Self::KeepEditing,
            _ => Self::Save,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NotePhase {
    Loading,
    Ready,
    ReadFailed,
    Saving,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteEditor {
    target: Target,
    request_id: RequestId,
    pub(super) scratchpad: Scratchpad,
    baseline: Scratchpad,
    pub(super) source: EnvironmentSourceEditor,
    pub(super) section: NoteSection,
    error: Option<SafeError>,
    pub(super) phase: NotePhase,
    confirmation: bool,
    choice: NoteCloseChoice,
    committed: bool,
}

impl NoteEditor {
    pub(super) fn loading(target: Target) -> Self {
        Self {
            target,
            request_id: RequestId::new(),
            scratchpad: Scratchpad::default(),
            baseline: Scratchpad::default(),
            source: EnvironmentSourceEditor::default(),
            section: NoteSection::Note,
            error: None,
            phase: NotePhase::Loading,
            confirmation: false,
            choice: NoteCloseChoice::KeepEditing,
            committed: false,
        }
    }
    #[must_use]
    pub const fn target(&self) -> Target {
        self.target
    }
    #[must_use]
    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }
    #[must_use]
    pub fn scratchpad(&self) -> &Scratchpad {
        &self.scratchpad
    }
    #[must_use]
    pub const fn section(&self) -> NoteSection {
        self.section
    }
    #[must_use]
    pub fn draft(&self) -> &str {
        self.source.value()
    }
    #[must_use]
    pub const fn source(&self) -> &EnvironmentSourceEditor {
        &self.source
    }
    #[must_use]
    pub fn error(&self) -> Option<&SafeError> {
        self.error.as_ref()
    }
    #[must_use]
    pub fn loading_state(&self) -> bool {
        self.phase == NotePhase::Loading
    }
    #[must_use]
    pub fn read_failed(&self) -> bool {
        self.phase == NotePhase::ReadFailed
    }
    #[must_use]
    pub fn saving(&self) -> bool {
        self.phase == NotePhase::Saving
    }
    #[must_use]
    pub const fn confirmation(&self) -> bool {
        self.confirmation
    }
    #[must_use]
    pub const fn close_choice(&self) -> NoteCloseChoice {
        self.choice
    }
    #[must_use]
    pub fn modified(&self) -> bool {
        self.scratchpad != self.baseline
            || (!self.committed
                && self.section == NoteSection::Note
                && self.draft() != self.baseline.note.as_deref().unwrap_or(""))
    }
}

fn safe_text(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| matches!(*c, '\n' | '\t') || presentation_character_is_safe(*c))
        .collect()
}

pub(super) fn open(state: &mut AppState) -> Vec<Effect> {
    let target = match state.route {
        Route::Home(HomeMode::Switch) => match state.selected {
            Selection::Target(Target::Session(id))
                if state.sessions.contains(&id) && state.session_can_use(id) =>
            {
                Target::Session(id)
            }
            _ => return Vec::new(),
        },
        Route::Home(HomeMode::Closeup) => match state.active_target() {
            Some(target)
                if target
                    .session_id()
                    .is_none_or(|id| state.sessions.contains(&id) && state.session_can_use(id)) =>
            {
                target
            }
            _ => return Vec::new(),
        },
    };
    let editor = NoteEditor::loading(target);
    let request_id = editor.request_id;
    state.note_editor = Some(editor);
    state.environment_editor = None;
    state.overlay = Some(Overlay::Notes);
    vec![Effect::LoadNotes { target, request_id }]
}

fn close(state: &mut AppState) {
    state.note_editor = None;
    state.overlay = None;
}

pub(super) fn loaded(
    state: &mut AppState,
    request_id: RequestId,
    target: Target,
    pad: &Scratchpad,
) {
    if let Some(editor) = state.note_editor.as_mut().filter(|e| {
        e.target == target && e.request_id == request_id && e.phase == NotePhase::Loading
    }) {
        editor
            .source
            .replace(safe_text(pad.note.as_deref().unwrap_or("")));
        editor.scratchpad.clone_from(pad);
        editor.baseline.clone_from(pad);
        editor.phase = NotePhase::Ready;
        editor.error = None;
    }
}

pub(super) fn saved(
    state: &mut AppState,
    request_id: RequestId,
    target: Target,
    pad: &Scratchpad,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
) {
    if state.note_editor.as_ref().is_some_and(|e| {
        e.target == target && e.request_id == request_id && e.phase == NotePhase::Saving
    }) {
        if let Target::Session(id) = target {
            state.saved_notes = Some((id, pad.clone()));
            state.saved_note_at = updated_at;
            state.note_revision = state.note_revision.saturating_add(1);
        }
        state.notice = Some(Notice::new("Memo saved"));
        close(state);
    }
}

pub(super) fn observed(
    state: &mut AppState,
    id: SessionId,
    note: Option<&str>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
) {
    if state.saved_notes.as_ref().is_some_and(|(saved_id, pad)| {
        *saved_id == id
            && (pad.note.as_deref() == note
                || state
                    .saved_note_at
                    .zip(updated_at)
                    .is_some_and(|(saved, observed)| observed >= saved))
    }) {
        state.saved_notes = None;
        state.saved_note_at = None;
        state.note_revision = state.note_revision.saturating_add(1);
    }
}

pub(super) fn failed(
    state: &mut AppState,
    request_id: RequestId,
    target: Target,
    error: &SafeError,
) {
    if let Some(editor) = state
        .note_editor
        .as_mut()
        .filter(|e| e.target == target && e.request_id == request_id)
    {
        editor.phase = if editor.phase == NotePhase::Loading {
            NotePhase::ReadFailed
        } else {
            NotePhase::Ready
        };
        editor.error = Some(error.clone());
    }
}

fn save(state: &mut AppState) -> Vec<Effect> {
    let available = state.note_editor.as_ref().is_some_and(|e| {
        e.target
            .session_id()
            .is_none_or(|id| state.sessions.contains(&id) && state.session_can_use(id))
    });
    let Some(editor) = state
        .note_editor
        .as_mut()
        .filter(|e| e.phase == NotePhase::Ready)
    else {
        return Vec::new();
    };
    if !available {
        editor.error = Some(SafeError {
            message: super::SafeMessage::new("This session is no longer available."),
            error_id: "note-target-unavailable".to_owned(),
        });
        return Vec::new();
    }
    let mut scratchpad = editor.scratchpad.clone();
    if editor.section == NoteSection::Note && !editor.committed {
        scratchpad.note = (!editor.draft().is_empty()).then(|| editor.draft().to_owned());
    }
    editor.phase = NotePhase::Saving;
    editor.confirmation = false;
    editor.error = None;
    editor.request_id = RequestId::new();
    vec![Effect::SaveNotes {
        target: editor.target,
        request_id: editor.request_id,
        scratchpad,
    }]
}

pub(super) fn key(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    if state.overlay != Some(Overlay::Notes) {
        return Vec::new();
    }
    if matches!(
        key,
        AppKey::SaveNotes | AppKey::SaveRoles | AppKey::Char('\u{13}')
    ) {
        return save(state);
    }
    let Some(editor) = state.note_editor.as_mut() else {
        return Vec::new();
    };
    if editor.phase == NotePhase::Saving {
        return Vec::new();
    }
    if editor.confirmation {
        match key {
            AppKey::Escape | AppKey::Char('c') => editor.confirmation = false,
            AppKey::Left | AppKey::Right | AppKey::Tab => {
                editor.choice = editor.choice.shifted(!matches!(key, AppKey::Left));
            }
            AppKey::Char('s') => return save(state),
            AppKey::Char('d') => close(state),
            AppKey::Enter => match editor.choice {
                NoteCloseChoice::Save => return save(state),
                NoteCloseChoice::Discard => close(state),
                NoteCloseChoice::KeepEditing => editor.confirmation = false,
            },
            _ => {}
        }
        return Vec::new();
    }
    if matches!(key, AppKey::Escape) {
        if editor.modified() {
            editor.confirmation = true;
            editor.choice = NoteCloseChoice::KeepEditing;
        } else {
            close(state);
        }
        return Vec::new();
    }
    if editor.phase != NotePhase::Ready {
        if editor.phase == NotePhase::ReadFailed && matches!(key, AppKey::Enter) {
            editor.phase = NotePhase::Loading;
            editor.error = None;
            editor.request_id = RequestId::new();
            return vec![Effect::LoadNotes {
                target: editor.target,
                request_id: editor.request_id,
            }];
        }
        return Vec::new();
    }
    if apply_edit(editor, key) {
        editor.error = None;
    }
    Vec::new()
}

fn apply_edit(editor: &mut NoteEditor, key: &AppKey) -> bool {
    match key {
        AppKey::SelectNoteSection(section) => {
            editor.section = *section;
            editor.source.replace(if *section == NoteSection::Note {
                editor.scratchpad.note.as_deref().unwrap_or("")
            } else {
                ""
            });
            editor.committed = false;
        }
        AppKey::SetNoteDraft(text) => {
            editor.source.replace(safe_text(text));
            editor.committed = false;
        }
        AppKey::Char(c) if presentation_character_is_safe(*c) => {
            editor.source.insert(&c.to_string());
            editor.committed = false;
        }
        AppKey::Paste(text) => {
            editor.source.paste(&safe_text(text));
            editor.committed = false;
        }
        AppKey::Enter => {
            editor.source.newline();
            editor.committed = false;
        }
        AppKey::Backspace => {
            editor.source.backspace();
            editor.committed = false;
        }
        AppKey::Left | AppKey::Right => editor.source.move_cursor(matches!(key, AppKey::Right)),
        AppKey::Up | AppKey::Down => editor.source.move_vertical(matches!(key, AppKey::Down)),
        AppKey::CtrlA | AppKey::Home => editor.source.move_edge(false),
        AppKey::ToggleTodo(index) => {
            if let Some(todo) = editor.scratchpad.todos.get_mut(*index) {
                todo.done = !todo.done;
            }
        }
        AppKey::CommitNoteDraft => {
            let draft = editor.draft().trim().to_owned();
            match editor.section {
                NoteSection::Note => editor.scratchpad.note = (!draft.is_empty()).then_some(draft),
                NoteSection::Todos if !draft.is_empty() => editor
                    .scratchpad
                    .todos
                    .push(usagi_core::domain::note::SessionTodo::new(draft)),
                NoteSection::Decisions if !draft.is_empty() => editor.scratchpad.decisions.push(
                    usagi_core::domain::note::SessionDecision::new(chrono::Utc::now(), draft),
                ),
                _ => {}
            }
            editor.source.replace("");
            editor.committed = true;
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::{AppEvent, BackendEvent, SafeMessage, update};
    use super::*;
    use usagi_core::domain::id::{SessionId, WorkspaceId};
    use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};

    fn state() -> (AppState, Target) {
        let first = SessionId::new();
        let second = SessionId::new();
        let mut state = AppState::home(WorkspaceId::new(), vec![first, second]);
        state.active = Some(first);
        state.selected = Selection::Target(Target::Session(second));
        (state, Target::Session(second))
    }
    fn error() -> SafeError {
        SafeError {
            message: SafeMessage::new("Could not save memo"),
            error_id: "memo-io".into(),
        }
    }
    fn read(state: &mut AppState, target: Target, text: &str) {
        let _ = update(state, AppEvent::Key(AppKey::Char('n')));
        let request_id = state.note_editor().unwrap().request_id();
        let _ = update(
            state,
            AppEvent::Backend(BackendEvent::NotesLoaded {
                target,
                request_id,
                scratchpad: Scratchpad {
                    note: Some(text.into()),
                    ..Default::default()
                },
            }),
        );
    }
    fn write(state: &mut AppState) -> (RequestId, Scratchpad) {
        let effects = update(state, AppEvent::Key(AppKey::SaveRoles));
        let editor = state.note_editor().unwrap();
        let request_id = editor.request_id();
        let pad = Scratchpad {
            note: (!editor.draft().is_empty()).then(|| editor.draft().to_owned()),
            todos: editor.scratchpad().todos.clone(),
            decisions: editor.scratchpad().decisions.clone(),
        };
        assert_eq!(
            effects,
            [Effect::SaveNotes {
                target: editor.target(),
                request_id,
                scratchpad: pad.clone(),
            }]
        );
        (request_id, pad)
    }

    #[test]
    fn opening_and_saving_a_mcp_note_preserves_tabs_and_pasted_tabs() {
        let (mut state, target) = state();
        read(&mut state, target, "\t日本語\n\t次の作業");
        assert!(!state.note_editor().unwrap().modified());
        let _ = key(&mut state, &AppKey::Paste("\t続き".into()));
        let (_, pad) = write(&mut state);
        assert_eq!(pad.note.as_deref(), Some("\t日本語\n\t次の作業\t続き"));
    }

    #[test]
    fn fresh_mcp_observation_and_session_removal_release_the_saved_preview() {
        let (mut state, target) = state();
        let id = target.session_id().unwrap();
        read(&mut state, target, "saved by TUI");
        let (request_id, pad) = write(&mut state);
        let saved_at = chrono::Utc::now();
        saved(&mut state, request_id, target, &pad, Some(saved_at));
        let other = state.active.unwrap();
        observed(&mut state, other, pad.note.as_deref(), Some(saved_at));
        assert!(state.saved_notes().is_some());
        observed(
            &mut state,
            id,
            Some("older snapshot"),
            Some(saved_at - chrono::Duration::seconds(1)),
        );
        assert!(state.saved_notes().is_some());
        observed(&mut state, id, Some("MCP update"), Some(saved_at));
        assert!(state.saved_notes().is_none());
        assert_eq!(state.note_revision(), 2);

        read(&mut state, target, "another TUI save");
        let (request_id, pad) = write(&mut state);
        saved(&mut state, request_id, target, &pad, Some(saved_at));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(vec![other])),
        );
        assert!(state.saved_notes().is_none());
        assert_eq!(state.note_revision(), 4);
    }

    #[test]
    fn edits_selected_session_and_only_closes_after_matching_save_success() {
        let (mut state, target) = state();
        let active = state.active;
        read(&mut state, target, "次にやること");
        let original_request = state.note_editor().unwrap().request_id();
        for key in [
            AppKey::Enter,
            AppKey::Paste("テスト\r\nを書く".into()),
            AppKey::Left,
            AppKey::Right,
            AppKey::Up,
            AppKey::Down,
        ] {
            let _ = update(&mut state, AppEvent::Key(key));
        }
        assert_eq!(
            state.note_editor().unwrap().draft(),
            "次にやること\nテスト\nを書く"
        );
        let (request_id, pad) = write(&mut state);
        assert_eq!(pad.note.as_deref(), Some("次にやること\nテスト\nを書く"));
        assert!(state.note_editor().unwrap().saving());
        for key in [AppKey::SaveNotes, AppKey::Escape, AppKey::Char('x')] {
            assert!(update(&mut state, AppEvent::Key(key)).is_empty());
        }
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::NotesLoaded {
                target,
                request_id: original_request,
                scratchpad: Scratchpad::default(),
            }),
        );
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::NotesSaved {
                updated_at: None,
                target,
                request_id: original_request,
                scratchpad: pad.clone(),
            }),
        );
        assert!(state.note_editor().unwrap().saving());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::NotesError {
                target,
                request_id,
                error: error(),
            }),
        );
        assert_eq!(
            state.note_editor().unwrap().draft(),
            pad.note.as_deref().unwrap()
        );
        assert!(state.note_editor().unwrap().error().is_some());
        let (retry, pad) = write(&mut state);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::NotesSaved {
                updated_at: None,
                target,
                request_id: retry,
                scratchpad: pad.clone(),
            }),
        );
        assert!(state.note_editor().is_none());
        assert_eq!(state.overlay(), None);
        assert_eq!(state.selected, Selection::Target(target));
        assert_eq!(state.active, active);
        assert_eq!(state.saved_notes().unwrap().1, pad);
        assert_eq!(state.note_revision(), 1);
        let sessions = state.sessions.clone();
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::Sessions(sessions)),
        );
        observed(
            &mut state,
            target.session_id().unwrap(),
            Some("old snapshot"),
            None,
        );
        assert!(state.saved_notes().is_some());
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::SessionNoteObserved {
                updated_at: None,
                session: target.session_id().unwrap(),
                note: pad.note,
            }),
        );
        assert!(state.saved_notes().is_none());
        assert_eq!(state.note_revision(), 2);
    }

    #[test]
    fn unsaved_close_allows_continue_discard_and_save() {
        let (mut state, target) = state();
        read(&mut state, target, "before");
        let _ = key(&mut state, &AppKey::Escape);
        assert!(state.note_editor().is_none());
        read(&mut state, target, "before");
        let _ = key(&mut state, &AppKey::Char('x'));
        for resume in [AppKey::Enter, AppKey::Escape, AppKey::Char('c')] {
            let _ = key(&mut state, &AppKey::Escape);
            assert!(state.note_editor().unwrap().confirmation());
            let _ = key(&mut state, &AppKey::Char('z'));
            let _ = key(&mut state, &resume);
            assert!(!state.note_editor().unwrap().confirmation());
        }
        let _ = key(&mut state, &AppKey::Escape);
        for input in [
            AppKey::Right,
            AppKey::Tab,
            AppKey::Left,
            AppKey::Left,
            AppKey::Right,
            AppKey::Right,
            AppKey::Right,
        ] {
            let _ = key(&mut state, &input);
        }
        let _ = key(&mut state, &AppKey::Char('d'));
        assert!(state.note_editor().is_none());
        for save_key in [AppKey::Enter, AppKey::Char('s')] {
            read(&mut state, target, "before");
            let _ = key(&mut state, &AppKey::Char('x'));
            let _ = key(&mut state, &AppKey::Escape);
            let _ = key(&mut state, &AppKey::Right);
            assert!(matches!(
                key(&mut state, &save_key).as_slice(),
                [Effect::SaveNotes { .. }]
            ));
            let request_id = state.note_editor().unwrap().request_id();
            failed(&mut state, request_id, target, &error());
            let _ = key(&mut state, &AppKey::Escape);
            let _ = key(&mut state, &AppKey::Left);
            assert_eq!(
                state.note_editor().unwrap().close_choice(),
                NoteCloseChoice::Discard
            );
            let _ = key(&mut state, &AppKey::Enter);
            assert!(state.note_editor().is_none());
        }
    }

    #[test]
    fn failed_or_stale_reads_cannot_overwrite_notes_and_failed_read_can_retry() {
        let (mut state, target) = state();
        let _ = open(&mut state);
        let old = state.note_editor().unwrap().request_id();
        assert!(key(&mut state, &AppKey::SaveRoles).is_empty());
        let _ = key(&mut state, &AppKey::Char('x'));
        assert_eq!(state.note_editor().unwrap().draft(), "");
        let _ = key(&mut state, &AppKey::Escape);
        let _ = open(&mut state);
        loaded(
            &mut state,
            old,
            target,
            &Scratchpad {
                note: Some("stale".into()),
                ..Default::default()
            },
        );
        assert!(state.note_editor().unwrap().loading_state());
        let request_id = state.note_editor().unwrap().request_id();
        failed(&mut state, request_id, target, &error());
        assert!(state.note_editor().unwrap().read_failed());
        assert!(key(&mut state, &AppKey::SaveNotes).is_empty());
        assert!(key(&mut state, &AppKey::Paste("x".into())).is_empty());
        assert!(matches!(
            key(&mut state, &AppKey::Enter).as_slice(),
            [Effect::LoadNotes { .. }]
        ));
        let request_id = state.note_editor().unwrap().request_id();
        loaded(&mut state, request_id, target, &Scratchpad::default());
        let _ = key(&mut state, &AppKey::Paste("draft".into()));
        state.sessions.clear();
        assert!(key(&mut state, &AppKey::SaveNotes).is_empty());
        assert_eq!(state.note_editor().unwrap().draft(), "draft");
        assert!(state.note_editor().unwrap().error().is_some());
    }

    #[test]
    fn editing_filters_terminal_controls_and_supports_clear_and_legacy_sections() {
        let (mut state, target) = state();
        read(&mut state, target, "あいう");
        let _ = key(&mut state, &AppKey::Backspace);
        let _ = key(&mut state, &AppKey::Home);
        let _ = key(&mut state, &AppKey::Char('先'));
        let _ = key(&mut state, &AppKey::CtrlA);
        let _ = key(&mut state, &AppKey::Paste("\0\u{1b}\r\n".into()));
        assert_eq!(state.note_editor().unwrap().draft(), "\n先あい");
        assert!(key(&mut state, &AppKey::Char('\0')).is_empty());
        let _ = key(&mut state, &AppKey::SetNoteDraft(String::new()));
        let (_, pad) = write(&mut state);
        assert_eq!(pad.note, None);
        let request_id = state.note_editor().unwrap().request_id();
        failed(&mut state, request_id, target, &error());
        for section in [
            NoteSection::Todos,
            NoteSection::Decisions,
            NoteSection::Note,
        ] {
            let _ = key(&mut state, &AppKey::SelectNoteSection(section));
            let _ = key(&mut state, &AppKey::SetNoteDraft("entry".into()));
            let _ = key(&mut state, &AppKey::CommitNoteDraft);
        }
        let _ = key(&mut state, &AppKey::ToggleTodo(0));
        let _ = key(&mut state, &AppKey::ToggleTodo(42));
        assert!(state.note_editor().unwrap().scratchpad().todos[0].done);
        assert_eq!(state.note_editor().unwrap().scratchpad().decisions.len(), 1);
    }

    #[test]
    fn only_usable_cursor_sessions_open_in_switch_and_closeup_uses_active_session() {
        let (mut state, target) = state();
        state.selected = Selection::NewSession;
        assert!(open(&mut state).is_empty());
        state.selected = Selection::Target(target);
        let id = target.session_id().unwrap();
        state.session_lifecycles.insert(
            id,
            SessionLifecycleProjection {
                lifecycle: SessionLifecycle::Failed,
                failure_stage: None,
                failure_summary: None,
            },
        );
        assert!(open(&mut state).is_empty());
        state.route = Route::Home(HomeMode::Closeup);
        assert!(matches!(
            open(&mut state).as_slice(),
            [Effect::LoadNotes {
                target: Target::Session(_),
                ..
            }]
        ));
        close(&mut state);
        state.active = None;
        assert!(open(&mut state).is_empty());
        assert!(key(&mut state, &AppKey::Enter).is_empty());
        state.overlay = Some(Overlay::Notes);
        assert!(key(&mut state, &AppKey::Enter).is_empty());
        assert!(key(&mut state, &AppKey::SaveNotes).is_empty());
    }
}
