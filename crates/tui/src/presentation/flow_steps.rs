//! Welcome / New / Open / Config の起動フロー各 step。

use std::io;

use crate::presentation::views::config;
#[cfg(test)]
use crate::usecase::application::controller::{SafeError, SafeMessage};

use super::{
    AppEvent, AppKey, BackendEvent, Completions, Config, ConfigStep, Field, Key, MenuAction, New,
    NewStep, Notice, Open, OpenStep, SettingsPort, Terminal, Welcome, WorkspaceConfigStep,
    play_config_save_wave, run_workspace_loading, run_workspace_loading_with,
    step_setup_commands_editor,
};

pub(super) fn unavailable_completion(completions: &Completions, message: &str) {
    completions.emit(AppEvent::Backend(BackendEvent::Notice(Notice::new(
        message,
    ))));
}

/// welcome 画面のキー処理結果。
pub(super) enum WelcomeStep {
    Stay,
    Quit,
    OpenList,
    /// Recent の単体 workspace を開く。
    OpenRecent(usize),
    /// New（新規 workspace 作成フォーム）へ進む。
    NewForm,
    /// Config（設定画面）へ進む。
    ConfigScreen,
}

pub(super) fn save_config_responsive(
    term: &mut dyn Terminal,
    form: &mut Config,
    settings: &mut dyn SettingsPort,
    base: Option<&[String]>,
) -> io::Result<bool> {
    if !settings.background_operations() {
        play_config_save_wave(term, form, base)?;
        return Ok(form.commit_save(settings));
    }
    let Some((scope, draft)) = form.save_request() else {
        return Ok(false);
    };
    let result = if let Some(base) = base {
        run_config_save_loading(term, form, base, || settings.save(scope, &draft))
    } else {
        run_workspace_loading(term, "Saving settings…", false, || {
            settings.save(scope, &draft)
        })
    };
    Ok(form.finish_save(draft, result))
}

/// Persist a workspace Config draft without replacing its Home-owned modal.
///
/// The generic workspace loading surface clears the complete frame. Using it
/// here made Config disappear during the write, then reappear for `done` before
/// closing. Keep painting the pending Config form over the same Home snapshot
/// so one modal remains visible throughout the save lifecycle.
pub(super) fn run_config_save_loading<T: Send>(
    term: &mut dyn Terminal,
    form: &mut Config,
    base: &[String],
    operation: impl FnOnce() -> io::Result<T> + Send,
) -> io::Result<T> {
    run_workspace_loading_with(
        term,
        "Saving settings…",
        false,
        false,
        operation,
        |height, width, _frame, _status, _show_progress| {
            let lines = config::render_over(height, width, base, form);
            form.advance_save_animation();
            lines
        },
    )
}

pub(super) fn save_environment_responsive(
    term: &mut dyn Terminal,
    form: &mut Config,
    settings: &mut dyn SettingsPort,
) -> bool {
    let Some((scope, bindings)) = form.environment_save_request() else {
        return false;
    };
    let result = if settings.background_operations() {
        run_workspace_loading(term, "Saving environment…", false, || {
            settings.save_environment(scope, &bindings)
        })
    } else {
        settings.save_environment(scope, &bindings)
    };
    form.finish_environment_save(bindings, result)
}

#[cfg(test)]
pub(super) fn unavailable_environment_error() -> SafeError {
    SafeError {
        message: SafeMessage::new("Environment is unavailable."),
        error_id: "environment-unavailable".to_owned(),
    }
}

/// welcome のメニュー操作を画面遷移へ写す。
pub(super) fn welcome_action(action: MenuAction) -> WelcomeStep {
    match action {
        MenuAction::Quit => WelcomeStep::Quit,
        MenuAction::Open => WelcomeStep::OpenList,
        MenuAction::OpenRecent(index) => WelcomeStep::OpenRecent(index),
        MenuAction::New => WelcomeStep::NewForm,
        MenuAction::Config => WelcomeStep::ConfigScreen,
    }
}

/// Config 画面のキー処理。Save は dirty な Save 行でのみ有効で、Enter は save フローを
/// 開始（loading）する。保存中の再入力は `begin_save` が弾く。
#[allow(clippy::needless_pass_by_value)]
pub(super) fn step_config(
    config: &mut Config,
    key: Key,
    settings: &mut dyn SettingsPort,
) -> ConfigStep {
    if config.is_selecting_team() {
        match key {
            Key::Left | Key::Char('h') => config.cycle_team_card(false),
            Key::Right | Key::Char('l') => config.cycle_team_card(true),
            Key::Up | Key::Char('k') => config.move_team_picker_vertical(false),
            Key::Down | Key::Char('j') => config.move_team_picker_vertical(true),
            Key::Enter => config.apply_team_picker(),
            Key::Escape => config.cancel_team_picker(),
            _ => {}
        }
        return ConfigStep::Stay;
    }
    if config.is_editing_environment() {
        match key {
            Key::Management {
                action: AppKey::SaveRoles,
                ..
            } if config.scope() == usagi_core::usecase::settings::SettingsScope::Global => {
                if settings.background_operations() {
                    return ConfigStep::SaveSource;
                }
                config.save_environment(settings);
            }
            Key::Enter if config.is_environment_save_focused() => {
                if settings.background_operations() {
                    return ConfigStep::SaveSource;
                }
                config.save_environment(settings);
            }
            Key::Enter => config.newline_environment(),
            Key::Tab => config.toggle_environment_focus(),
            Key::Backspace => config.backspace_environment(),
            Key::Delete => config.delete_environment(),
            Key::Left => config.move_environment(false),
            Key::Right => config.move_environment(true),
            Key::Up => config.move_environment_vertical(false),
            Key::Down => config.move_environment_vertical(true),
            Key::Home | Key::LineStart => config.move_environment_edge(false),
            Key::End | Key::LineEnd => config.move_environment_edge(true),
            Key::Char(character) if !character.is_control() => {
                config.type_environment(&character.to_string());
            }
            Key::Paste(text) => config.paste_environment(&text),
            Key::Escape => config.cancel_environment(),
            _ => {}
        }
        return ConfigStep::Stay;
    }
    if config.is_editing_setup_commands() {
        return step_setup_commands_editor(config, key, settings);
    }
    match key {
        Key::Up | Key::Char('k') => {
            config.previous_field();
            ConfigStep::Stay
        }
        Key::Down | Key::Char('j') => {
            config.next_field();
            ConfigStep::Stay
        }
        Key::Left | Key::Char('h') => {
            config.cycle_selected(false);
            ConfigStep::Stay
        }
        Key::Right | Key::Char('l') => {
            config.cycle_selected(true);
            ConfigStep::Stay
        }
        // Enter begins the save flow (loading). `begin_save` is a no-op unless a
        // dirty Save row is focused with no save already in flight, so a rapid
        // second Enter cannot start a second save.
        Key::Enter if config.open_environment(settings) => ConfigStep::Stay,
        Key::Enter if config.open_setup_commands(settings) => ConfigStep::Stay,
        Key::Enter if config.open_team_picker() => ConfigStep::Stay,
        Key::Enter if config.begin_save() => ConfigStep::Save,
        Key::Escape => ConfigStep::Back,
        Key::Quit | Key::CtrlQ => ConfigStep::Quit,
        _ => ConfigStep::Stay,
    }
}

/// Workspace Config is an overlay owned by Home, so global quit chords must not
/// escape to the enclosing workspace loop while it has input focus. The full
/// screen Config keeps its existing quit contract through [`step_config`].
pub(super) fn step_workspace_config(
    config: &mut Config,
    key: Key,
    settings: &mut dyn SettingsPort,
) -> WorkspaceConfigStep {
    match step_config(config, key, settings) {
        ConfigStep::Stay | ConfigStep::Quit => WorkspaceConfigStep::Stay,
        ConfigStep::Back => WorkspaceConfigStep::Back,
        ConfigStep::Save => WorkspaceConfigStep::Save,
        ConfigStep::SaveSource => WorkspaceConfigStep::SaveSource,
    }
}

/// welcome 画面のキー処理。最上位画面なので Esc も終了として扱う。
#[allow(clippy::needless_pass_by_value)]
pub(super) fn step_welcome(welcome: &mut Welcome, key: Key) -> WelcomeStep {
    match key {
        Key::Up | Key::Char('k') => {
            welcome.select_prev();
            WelcomeStep::Stay
        }
        Key::Down | Key::Char('j') => {
            welcome.select_next();
            WelcomeStep::Stay
        }
        Key::Escape | Key::Quit | Key::CtrlQ => WelcomeStep::Quit,
        Key::Enter => welcome_action(welcome.selected_action()),
        Key::Char(ch) => welcome
            .action_for(ch)
            .map_or(WelcomeStep::Stay, welcome_action),
        Key::Left
        | Key::Right
        | Key::PageUp
        | Key::PageDown
        | Key::Home
        | Key::End
        | Key::Delete
        | Key::LineStart
        | Key::LineEnd
        | Key::SelectLeft
        | Key::SelectRight
        | Key::SelectHome
        | Key::SelectEnd
        | Key::Backspace
        | Key::Tab
        | Key::CtrlD
        | Key::CtrlX
        | Key::Help
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Management { .. }
        | Key::Paste(_)
        | Key::TerminalCopy { .. }
        | Key::Resize
        | Key::Other => WelcomeStep::Stay,
    }
}

/// New 画面のキー処理（純粋）。矢印キーでフィールドを移り、←→ でモード切替（モード選択時）または
/// キャレット移動、文字入力・Backspace で編集、Esc で welcome へ戻り、`Ctrl-C` で終了する。
/// フォームの確定（作成）は作成処理が入るまで留まる。
#[allow(clippy::needless_pass_by_value)]
pub(super) fn step_new(form: &mut New, key: Key) -> NewStep {
    if form.is_creating() {
        return match key {
            Key::Escape => NewStep::Back,
            Key::Quit | Key::CtrlQ => NewStep::Quit,
            Key::Other | Key::Resize => {
                form.advance_create_animation();
                NewStep::Stay
            }
            _ => NewStep::Stay,
        };
    }
    match key {
        Key::Up => {
            form.focus_prev();
            NewStep::Stay
        }
        Key::Down => {
            form.focus_next();
            NewStep::Stay
        }
        Key::Left => {
            step_new_horizontal(form, false);
            NewStep::Stay
        }
        Key::Right => {
            step_new_horizontal(form, true);
            NewStep::Stay
        }
        // Home/End と emacs 行頭/行末（Ctrl-A/Ctrl-E）はフォーカス中フィールドの
        // キャレット移動。テキスト入力にフォーカスがあるので new-session ではなく caret。
        Key::Home | Key::LineStart => {
            form.cursor_home();
            NewStep::Stay
        }
        Key::End | Key::LineEnd => {
            form.cursor_end();
            NewStep::Stay
        }
        Key::SelectLeft => {
            form.select_left();
            NewStep::Stay
        }
        Key::SelectRight => {
            form.select_right();
            NewStep::Stay
        }
        Key::SelectHome => {
            form.select_home();
            NewStep::Stay
        }
        Key::SelectEnd => {
            form.select_end();
            NewStep::Stay
        }
        Key::Backspace => {
            form.backspace();
            NewStep::Stay
        }
        Key::Delete => {
            form.delete_forward();
            NewStep::Stay
        }
        Key::Char(ch) => {
            form.insert_char(ch);
            NewStep::Stay
        }
        // A bracketed paste inserts its text into the focused field verbatim, so
        // a repository URL or path pastes as one block.
        Key::Paste(text) => {
            for ch in text.chars() {
                form.insert_char(ch);
            }
            NewStep::Stay
        }
        Key::Escape => NewStep::Back,
        Key::Quit | Key::CtrlQ => NewStep::Quit,
        Key::Tab => form
            .begin_directory_completion()
            .map_or(NewStep::Stay, NewStep::CompleteDirectory),
        // Enter は入力を検証して作成へ進む。必須項目が欠けていれば安全なメッセージを
        // notice に出し、同画面に留まって draft を保つ。
        Key::Enter => match form.to_request() {
            Ok(request) => NewStep::Create(request),
            Err(error) => {
                form.set_notice(Some(error.message().to_owned()));
                NewStep::Stay
            }
        },
        Key::CtrlD
        | Key::CtrlX
        | Key::Help
        | Key::PageUp
        | Key::PageDown
        | Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Management { .. }
        | Key::TerminalCopy { .. }
        | Key::Resize
        | Key::Other => NewStep::Stay,
    }
}

/// 作成失敗の io error を、New フォームの 1 行 notice slot に収まる安全なメッセージへ縮める。
/// git の stderr は複数行になりうるので先頭行だけを取り、長すぎる場合は切り詰める。
pub(super) fn new_project_notice(error: &io::Error) -> String {
    const MAX: usize = 72;
    let message = error.to_string();
    let first = message.lines().next().unwrap_or("").trim();
    let detail = if first.is_empty() {
        "could not create the project"
    } else {
        first
    };
    if detail.chars().count() > MAX {
        let truncated: String = detail.chars().take(MAX - 1).collect();
        format!("{truncated}…")
    } else {
        detail.to_owned()
    }
}

/// New 画面の ←→ 操作。モード選択にフォーカスがあるときはモードを切り替え、テキスト欄では
/// キャレットを左右へ動かす（`right` が右方向）。
pub(super) fn step_new_horizontal(form: &mut New, right: bool) {
    if form.focus() == Field::Mode {
        form.toggle_mode();
    } else if right {
        form.cursor_right();
    } else {
        form.cursor_left();
    }
}

/// Open 画面のキー処理。Enter で選択 path を確定し、Esc で welcome へ戻る。
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub(super) fn step_open(open: &mut Open, key: Key) -> OpenStep {
    if open.unregistering_path().is_some() {
        return match key {
            Key::Left | Key::Right | Key::Tab => {
                open.toggle_unregister_choice();
                OpenStep::Stay
            }
            Key::Char('y' | 'Y') | Key::Enter => open
                .confirm_unregister()
                .map_or(OpenStep::Stay, OpenStep::ConfirmUnregister),
            Key::Char('n' | 'N') | Key::Escape => {
                open.cancel_unregister();
                OpenStep::Stay
            }
            Key::Quit | Key::CtrlQ => OpenStep::Quit,
            _ => OpenStep::Stay,
        };
    }
    if open.cleanup_confirming() {
        return match key {
            Key::Char('y') | Key::Enter => OpenStep::ConfirmCleanup,
            Key::Char('n') | Key::Escape => {
                open.cancel_cleanup();
                OpenStep::Stay
            }
            Key::Quit | Key::CtrlQ => OpenStep::Quit,
            _ => OpenStep::Stay,
        };
    }
    match key {
        Key::Up => {
            open.select_prev();
            OpenStep::Stay
        }
        Key::Down => {
            open.select_next();
            OpenStep::Stay
        }
        Key::Backspace => {
            open.pop_filter();
            OpenStep::Stay
        }
        Key::Left => {
            open.filter_left();
            OpenStep::Stay
        }
        Key::Right => {
            open.filter_right();
            OpenStep::Stay
        }
        Key::Home | Key::LineStart => {
            open.filter_home();
            OpenStep::Stay
        }
        Key::End | Key::LineEnd => {
            open.filter_end();
            OpenStep::Stay
        }
        Key::Delete => {
            open.filter_delete_forward();
            OpenStep::Stay
        }
        Key::SelectLeft => {
            open.filter_select_left();
            OpenStep::Stay
        }
        Key::SelectRight => {
            open.filter_select_right();
            OpenStep::Stay
        }
        Key::SelectHome => {
            open.filter_select_home();
            OpenStep::Stay
        }
        Key::SelectEnd => {
            open.filter_select_end();
            OpenStep::Stay
        }
        Key::Escape => OpenStep::Back,
        Key::Quit | Key::CtrlQ => OpenStep::Quit,
        Key::Enter => {
            let paths = if open.is_unite() {
                open.unite_paths()
            } else {
                open.selected()
                    .map(|workspace| vec![workspace.path.clone()])
                    .unwrap_or_default()
            };
            if paths.is_empty() {
                OpenStep::Stay
            } else {
                OpenStep::Choose(paths)
            }
        }
        Key::Tab => {
            open.toggle_unite();
            OpenStep::Stay
        }
        Key::Char(' ') if open.is_unite() => {
            open.toggle_unite_member();
            OpenStep::Stay
        }
        Key::Char('C') => {
            open.request_cleanup();
            OpenStep::Stay
        }
        Key::CtrlX => {
            open.request_unregister();
            OpenStep::Stay
        }
        Key::Char(ch) => {
            open.push_filter(ch);
            OpenStep::Stay
        }
        // A bracketed paste appends its text to the filter one character at a time.
        Key::Paste(text) => {
            for ch in text.chars() {
                open.push_filter(ch);
            }
            OpenStep::Stay
        }
        Key::Live(_)
        | Key::Click { .. }
        | Key::Pointer(_)
        | Key::Passthrough(_)
        | Key::Management { .. }
        | Key::TerminalCopy { .. }
        | Key::CtrlD
        | Key::Help
        | Key::PageUp
        | Key::PageDown
        | Key::Resize
        | Key::Other => OpenStep::Stay,
    }
}
