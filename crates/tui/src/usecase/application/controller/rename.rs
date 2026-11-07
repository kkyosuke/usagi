//! Switch-mode display-name editing, fenced to the selected session incarnation.

use super::{AppKey, AppState, Effect, Notice, Overlay, Selection, Target};
use unicode_segmentation::UnicodeSegmentation;
use usagi_core::domain::id::{RequestId, SessionId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameSessionForm {
    pub session: SessionId,
    pub name: String,
    pub error: Option<Notice>,
    pub saving: bool,
    request_id: RequestId,
}

pub(super) fn open(state: &mut AppState) -> Vec<Effect> {
    let Selection::Target(Target::Session(session)) = state.selected else {
        return Vec::new();
    };
    if !state.sessions.contains(&session) || !state.session_can_use(session) {
        return Vec::new();
    }
    let Some(name) = state.session_labels.get(&session).cloned() else {
        return Vec::new();
    };
    state.rename_session = Some(RenameSessionForm {
        session,
        name,
        error: None,
        saving: false,
        request_id: RequestId::new(),
    });
    state.overlay = Some(Overlay::RenameSession);
    Vec::new()
}

pub(super) fn key(state: &mut AppState, key: &AppKey) -> Vec<Effect> {
    let Some(form) = state.rename_session.as_mut() else {
        state.overlay = None;
        return Vec::new();
    };
    if form.saving {
        return Vec::new();
    }
    match key {
        AppKey::Escape => {
            state.rename_session = None;
            state.overlay = None;
        }
        AppKey::Backspace => {
            let end = form
                .name
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(index, _)| index);
            form.name.truncate(end);
            form.error = None;
        }
        AppKey::Paste(text) => {
            form.name.extend(text.chars().filter(|character| {
                usagi_core::domain::presentation_text::presentation_character_is_safe(*character)
            }));
            form.error = None;
        }
        AppKey::Char(character)
            if usagi_core::domain::presentation_text::presentation_character_is_safe(
                *character,
            ) =>
        {
            form.name.push(*character);
            form.error = None;
        }
        AppKey::Enter => {
            let name = match crate::usecase::session_rename::validate(&form.name) {
                Ok(name) => name.to_owned(),
                Err(message) => {
                    form.error = Some(Notice::new(message));
                    return Vec::new();
                }
            };
            if !state.sessions.contains(&form.session) {
                form.error = Some(Notice::new("This session is no longer available."));
                return Vec::new();
            }
            form.saving = true;
            form.error = None;
            form.request_id = RequestId::new();
            return vec![Effect::RenameSession {
                session: form.session,
                request_id: form.request_id,
                name,
            }];
        }
        _ => {}
    }
    Vec::new()
}

pub(super) fn complete(
    state: &mut AppState,
    session: SessionId,
    request_id: RequestId,
    result: Result<(), String>,
) -> Vec<Effect> {
    let Some(form) = state
        .rename_session
        .as_mut()
        .filter(|form| form.session == session && form.request_id == request_id && form.saving)
    else {
        return Vec::new();
    };
    match result {
        Ok(()) => {
            state.rename_session = None;
            if state.overlay == Some(Overlay::RenameSession) {
                state.overlay = None;
            }
            state.notice = Some(Notice::new("Session renamed."));
        }
        Err(message) => {
            form.saving = false;
            form.error = Some(Notice::new(message));
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecase::application::controller::{
        AppEvent, BackendEvent, HomeMode, Route, update,
    };
    use std::collections::BTreeMap;
    use usagi_core::domain::id::WorkspaceId;
    use usagi_core::domain::session_lifecycle::{SessionLifecycle, SessionLifecycleProjection};

    fn home() -> (AppState, SessionId, SessionId) {
        let active = SessionId::new();
        let selected = SessionId::new();
        let mut state = AppState::home(WorkspaceId::new(), vec![active, selected]);
        state.active = Some(active);
        state.selected = Selection::Target(Target::Session(selected));
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::SessionLabels(BTreeMap::from([
                (active, "active".into()),
                (selected, "old".into()),
            ]))),
        );
        (state, active, selected)
    }

    fn press(state: &mut AppState, key: AppKey) -> Vec<Effect> {
        update(state, AppEvent::Key(key))
    }

    #[test]
    fn rename_targets_switch_cursor_preserves_active_and_retries_failed_save() {
        let (mut state, active, selected) = home();
        assert_eq!(press(&mut state, AppKey::Char('r')), Vec::<Effect>::new());
        assert_eq!(state.overlay(), Some(Overlay::RenameSession));
        assert_eq!(state.rename_session_form().unwrap().name, "old");
        for _ in 0..3 {
            press(&mut state, AppKey::Backspace);
        }
        press(&mut state, AppKey::Paste(" 新しい名前 👩‍💻 ".into()));
        let effects = press(&mut state, AppKey::Enter);
        let [
            Effect::RenameSession {
                session,
                request_id,
                name,
            },
        ] = effects.as_slice()
        else {
            panic!("rename effect");
        };
        assert_eq!(*session, selected);
        assert_eq!(name, "新しい名前 👩‍💻");
        let old_request = *request_id;
        assert!(state.rename_session_form().unwrap().saving);
        for key in [
            AppKey::Enter,
            AppKey::Escape,
            AppKey::Char('f'),
            AppKey::CtrlX,
        ] {
            assert_eq!(press(&mut state, key), Vec::<Effect>::new());
        }
        complete(&mut state, selected, RequestId::new(), Ok(()));
        complete(&mut state, active, old_request, Ok(()));
        assert!(state.rename_session_form().unwrap().saving);
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::SessionRenamed {
                session: selected,
                request_id: old_request,
                result: Err("write failed".into()),
            }),
        );
        assert!(!state.rename_session_form().unwrap().saving);
        assert_eq!(state.rename_session_form().unwrap().name, " 新しい名前 👩‍💻 ");
        assert!(state.rename_session_form().unwrap().error.is_some());
        let effects = press(&mut state, AppKey::Enter);
        let [Effect::RenameSession { request_id, .. }] = effects.as_slice() else {
            panic!("retry effect");
        };
        assert_ne!(*request_id, old_request);
        complete(&mut state, selected, old_request, Ok(()));
        assert!(state.rename_session_form().unwrap().saving);
        complete(&mut state, selected, *request_id, Ok(()));
        assert!(state.rename_session_form().is_none());
        assert_eq!(state.overlay(), None);
        assert_eq!(state.active, Some(active));
        assert_eq!(state.selected, Selection::Target(Target::Session(selected)));
        assert_eq!(state.route(), Route::Home(HomeMode::Switch));
        complete(&mut state, selected, *request_id, Err("late".into()));
        assert_eq!(state.notice().unwrap().message, "Session renamed.");
    }

    #[test]
    fn rename_is_scoped_to_usable_switch_rows_and_escape_cancels() {
        let (mut state, _, selected) = home();
        for selection in [
            Selection::Idle,
            Selection::NewSession,
            Selection::Target(Target::Root(state.workspace)),
            Selection::Target(Target::Session(SessionId::new())),
        ] {
            state.selected = selection;
            press(&mut state, AppKey::Char('r'));
            assert!(state.rename_session_form().is_none());
        }
        state.selected = Selection::Target(Target::Session(selected));
        for lifecycle in [
            SessionLifecycle::Creating,
            SessionLifecycle::Deleting,
            SessionLifecycle::Failed,
        ] {
            state.session_lifecycles.insert(
                selected,
                SessionLifecycleProjection {
                    lifecycle,
                    failure_stage: None,
                    failure_summary: None,
                },
            );
            press(&mut state, AppKey::Char('r'));
            assert!(state.rename_session_form().is_none());
        }
        state.session_lifecycles.clear();
        state.route = Route::Home(HomeMode::Closeup);
        press(&mut state, AppKey::Char('r'));
        assert!(state.rename_session_form().is_none());
        state.route = Route::Home(HomeMode::Switch);
        state.overlay = Some(Overlay::QuitConfirmation);
        press(&mut state, AppKey::Char('r'));
        assert!(state.rename_session_form().is_none());
        state.overlay = None;
        state.session_labels.clear();
        press(&mut state, AppKey::Char('r'));
        assert!(state.rename_session_form().is_none());
        state.session_labels.insert(selected, "old".into());
        press(&mut state, AppKey::Char('r'));
        press(&mut state, AppKey::Char('!'));
        assert_eq!(state.rename_session_form().unwrap().name, "old!");
        assert_eq!(press(&mut state, AppKey::Escape), Vec::<Effect>::new());
        assert!(state.rename_session_form().is_none());
        assert_eq!(state.session_labels()[&selected], "old");
        state.overlay = Some(Overlay::RenameSession);
        key(&mut state, &AppKey::Enter);
        assert_eq!(state.overlay(), None);
    }

    #[test]
    fn invalid_input_and_removed_target_keep_the_draft_without_effects() {
        let (mut state, _, selected) = home();
        press(&mut state, AppKey::Char('r'));
        for _ in 0..4 {
            press(&mut state, AppKey::Backspace);
        }
        assert_eq!(press(&mut state, AppKey::Enter), Vec::<Effect>::new());
        assert!(state.rename_session_form().unwrap().error.is_some());
        press(&mut state, AppKey::Paste("あ👩‍💻\n\u{1b}\u{202e}".into()));
        assert_eq!(state.rename_session_form().unwrap().name, "あ👩‍💻");
        press(&mut state, AppKey::Backspace);
        assert_eq!(state.rename_session_form().unwrap().name, "あ");
        press(&mut state, AppKey::Char('\u{1b}'));
        press(&mut state, AppKey::Up);
        assert_eq!(state.rename_session_form().unwrap().name, "あ");
        state.rename_session.as_mut().unwrap().name = "a".repeat(65);
        assert_eq!(press(&mut state, AppKey::Enter), Vec::<Effect>::new());
        assert_eq!(
            state
                .rename_session_form()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .message,
            "Use at most 64 characters."
        );
        state.rename_session.as_mut().unwrap().name = "retained".into();
        state.sessions.retain(|session| *session != selected);
        assert_eq!(press(&mut state, AppKey::Enter), Vec::<Effect>::new());
        assert_eq!(state.rename_session_form().unwrap().name, "retained");
        assert!(
            state
                .rename_session_form()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .message
                .contains("no longer")
        );
    }
}
