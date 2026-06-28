use console::style;

use crate::presentation::tui::welcome;
use crate::presentation::tui::widgets;

use crate::domain::settings::SkillFeature;

use super::state::{Config, Field, InstallModal, LocalField, ModelModal};

/// The label of the Save button row.
const SAVE_LABEL: &str = "[ Save ]";

/// Inner width of the local-LLM install / progress modal box. Wide enough for
/// the longest body line ("ローカル LLM (ollama) をインストールします" = 42
/// columns) so it fits without truncation; `render_modal` still clamps this to
/// the terminal and clips any line that overruns it on a narrow screen.
const MODAL_INNER_WIDTH: usize = 42;

/// Builds the centred mascot, title, and subtitle block.
///
/// The title and subtitle reflect the screen's scope (global vs. workspace), so
/// they are passed in. Vertical placement is handled by [`render_frame`], so
/// this adds no leading padding.
fn header_lines(width: usize, title: &str, subtitle: &str) -> Vec<String> {
    widgets::header_lines(width, title, Some(subtitle))
}

/// The width of the label column: the widest field label across both the
/// global and project-local fields, so the value column aligns whether or not
/// the local override rows are shown.
fn label_width() -> usize {
    let global = Field::ALL.iter().map(|f| f.label().chars().count());
    let local = LocalField::ALL.iter().map(|f| f.label().chars().count());
    // The shipped-skill feature rows appear in both scopes, so their labels
    // factor into the shared column width too.
    let skills = SkillFeature::ALL.iter().map(|f| f.label().chars().count());
    global.chain(local).chain(skills).max().unwrap_or(0)
}

/// Builds one setting row. Two single-column gutters sit left of the label: the
/// `>` cursor for the selected row, then a `●` flag for a field carrying an
/// unsaved edit. The label follows in a fixed-width column, then the already
/// styled `< value >` chooser.
///
/// `value` arrives pre-styled from [`widgets::chooser`]; keeping the cursor and
/// change flag in their own gutters keeps the label and value columns aligned.
fn setting_row(
    block_pad: &str,
    label: &str,
    label_width: usize,
    value: &str,
    selected: bool,
    changed: bool,
) -> String {
    let cursor = widgets::cursor_marker(selected);

    // A dot to the left of the label flags an edit that has not been saved yet.
    let mark = if changed {
        style("●").yellow().bold().to_string()
    } else {
        " ".to_string()
    };

    let padded = format!("{label:<label_width$}");
    let label = if selected {
        style(padded).cyan().bold().to_string()
    } else {
        style(padded).cyan().to_string()
    };

    format!("{block_pad}{cursor} {mark} {label}  {value}")
}

/// Renders an action row's value (e.g. the Local LLM "Install" prompt) as a
/// plain green label — no chevrons, since it triggers an action rather than
/// cycling through choices. It brightens (bold) when focused.
fn action_label(text: &str, selected: bool) -> String {
    let styled = style(text.to_string());
    if selected {
        styled.green().bold()
    } else {
        styled.green()
    }
    .to_string()
}

/// Builds the settings list: one row per editable field (global, then any
/// project-local overrides). Most rows are a `< value >` chooser that lights up
/// when focused and turns yellow when edited; an action row (the Local LLM
/// "Install" prompt) is a plain label instead.
fn settings_lines(block_pad: &str, config: &Config) -> Vec<String> {
    let label_width = label_width();

    config
        .rows()
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let selected = i == config.selected_index();
            let value = if row.disabled {
                // Inert rows (e.g. the model row before the runtime is present)
                // render as a dim plain label with no chevrons.
                style(row.value.clone()).dim().to_string()
            } else if row.action {
                action_label(&row.value, selected)
            } else {
                widgets::chooser(&row.value, selected, row.changed)
            };
            setting_row(
                block_pad,
                row.label,
                label_width,
                &value,
                selected,
                row.changed,
            )
        })
        .collect()
}

/// Builds the Save button row, sat in the value column below the fields.
///
/// The button is enabled (green) only when there are unsaved changes; with
/// nothing to save it is dimmed, so its state mirrors whether pressing it would
/// do anything. The `>` cursor marks it when focused, like any other row.
fn save_button_line(block_pad: &str, dirty: bool, selected: bool) -> String {
    let marker = widgets::cursor_marker(selected);

    let button = if dirty {
        style(SAVE_LABEL).green().bold().to_string()
    } else {
        style(SAVE_LABEL).dim().to_string()
    };

    // Align the button under the value column: the cursor gutter, the (empty)
    // change-flag gutter, then the label-width gutter, matching `setting_row`.
    let gutter = " ".repeat(label_width());
    format!("{block_pad}{marker}   {gutter}  {button}")
}

/// Builds the transient notice line below the settings.
///
/// Always returns two lines — a blank separator plus the notice slot (blank
/// when absent) — so showing or clearing a notice never shifts the layout.
fn notice_lines(block_pad: &str, notice: Option<&str>) -> Vec<String> {
    let slot = match notice {
        Some(notice) => format!("{block_pad}{}", style(notice).yellow()),
        None => String::new(),
    };
    vec![String::new(), slot]
}

/// Builds the footer help line.
///
/// Returns the footer text only; [`render_frame`] pins it to the bottom edge.
fn footer_lines(width: usize) -> Vec<String> {
    // `Space` is surfaced because it is the only key that opens the runtime
    // install / model-picker modals — without it those actions are
    // undiscoverable. `Enter` both saves (on the Save button) and confirms a
    // field, so it is labelled generically rather than "save".
    vec![widgets::dim_line(
        width,
        "↑↓ move · ←→ change · Space select · Enter confirm · Esc back",
    )]
}

/// Builds the `ollama` runtime install confirmation modal: it explains the
/// action and masks the sudo password as it is typed. The model is chosen
/// separately afterwards via the picker, so it is not mentioned here.
fn install_modal_frame(raw_height: usize, raw_width: usize, modal: &InstallModal) -> Vec<String> {
    let body = vec![
        "ローカル LLM ランタイムをインストール".to_string(),
        String::new(),
        "導入には sudo 権限が必要です".to_string(),
        "（モデルは導入後に選んで取得します）".to_string(),
        String::new(),
        format!("sudo パスワード: {}", modal.masked()),
        String::new(),
        style("Enter: 開始   Esc: キャンセル").dim().to_string(),
    ];
    widgets::render_modal(raw_height, raw_width, "Local LLM", MODAL_INNER_WIDTH, &body)
}

/// Builds the model-selection modal: each offered model with an install marker
/// (✓ pulled, ⬇ not yet), the cursor row highlighted. Picking an unpulled model
/// starts a background pull; picking a pulled one adopts it.
fn model_modal_frame(raw_height: usize, raw_width: usize, modal: &ModelModal) -> Vec<String> {
    let mut body = vec!["使用するモデルを選択".to_string(), String::new()];
    for row in modal.rows() {
        let marker = if row.installed {
            "✓ 導入済"
        } else {
            "⬇ 未導入"
        };
        let cursor = if row.selected { ">" } else { " " };
        let line = format!("{cursor} {:<20} {marker}", row.model);
        body.push(if row.selected {
            style(line).cyan().bold().to_string()
        } else {
            style(line).dim().to_string()
        });
    }
    body.push(String::new());
    body.push(style("↑↓ 選択  Enter 決定  Esc 取消").dim().to_string());
    widgets::render_modal(
        raw_height,
        raw_width,
        "Local LLM Model",
        MODAL_INNER_WIDTH,
        &body,
    )
}

/// Builds the full configuration frame for a raw terminal size.
pub fn render_frame(
    raw_height: usize,
    raw_width: usize,
    config: &Config,
    notice: Option<&str>,
) -> Vec<String> {
    // When a modal is open it overlays the whole screen: the runtime-install
    // password prompt, or the model picker.
    if let Some(modal) = config.install_modal() {
        return install_modal_frame(raw_height, raw_width, modal);
    }
    if let Some(modal) = config.model_modal() {
        return model_modal_frame(raw_height, raw_width, modal);
    }

    let (height, width) = widgets::normalize_size(raw_height, raw_width);
    let block_pad = " ".repeat(widgets::centered_padding(width, widgets::BLOCK_WIDTH));

    // The body (mascot, title, settings and notice slot) is centred vertically;
    // the footer is pinned to the bottom edge of the frame.
    let mut body = header_lines(width, config.title(), config.subtitle());
    body.push(String::new());
    body.extend(settings_lines(&block_pad, config));
    // A blank line sets the Save button apart from the fields above it.
    body.push(String::new());
    body.push(save_button_line(
        &block_pad,
        config.is_dirty(),
        config.is_save_selected(),
    ));
    body.extend(notice_lines(&block_pad, notice));
    let footer = footer_lines(width);

    let mut lines = Vec::with_capacity(height);

    // Pin the mascot to the shared row (clamped so tall settings never overrun the
    // footer) so it never jumps from the welcome / Open / New screens.
    let available = height.saturating_sub(body.len() + footer.len());
    let top_padding = welcome::mascot_top_padding(height).min(available);
    for _ in 0..top_padding {
        lines.push(String::new());
    }
    lines.extend(body);

    // Push the footer down to the bottom row of the frame.
    let bottom_padding = height.saturating_sub(lines.len() + footer.len());
    for _ in 0..bottom_padding {
        lines.push(String::new());
    }
    lines.extend(footer);

    // Clamp every row to the terminal width so a long value, notice, or footer on
    // a narrow terminal is clipped (with an ellipsis) rather than wrapping and
    // breaking the centred layout. Rows already built to width are unaffected.
    for line in &mut lines {
        *line = widgets::clip_to_width(line, width);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::{Settings, Theme};

    /// A row carries the cursor when its first non-space glyph is `>`. Chevrons
    /// from the chooser also contain `>`, so position — not mere presence — is
    /// what distinguishes the selected row.
    fn has_cursor(line: &str) -> bool {
        console::strip_ansi_codes(line)
            .trim_start()
            .starts_with('>')
    }

    fn sample_config() -> Config {
        Config::new(
            Settings {
                theme: Theme::Dark,
                default_workspace: Some("alpha".to_string()),
                ..Default::default()
            },
            vec!["alpha".to_string()],
        )
    }

    #[test]
    fn header_lines_render_mascot_title_and_subtitle() {
        let lines = header_lines(80, "Config", "Adjust your global preferences");
        assert!(!lines[0].is_empty());
        let joined = lines.join("\n");
        assert!(joined.contains("Config"));
        assert!(joined.contains("Adjust your global preferences"));
    }

    #[test]
    fn setting_row_marks_only_the_selected_entry() {
        let selected = setting_row("", "Theme", 17, "Dark", true, false);
        assert!(selected.contains('>'));
        assert!(selected.contains("Theme"));
        assert!(selected.contains("Dark"));
        let unselected = setting_row("", "Theme", 17, "Dark", false, false);
        assert!(!unselected.contains('>'));
    }

    #[test]
    fn setting_row_flags_changed_fields_with_a_dot() {
        let changed = setting_row("", "Theme", 17, "Dark", false, true);
        assert!(changed.contains('●'));
        let unchanged = setting_row("", "Theme", 17, "Dark", false, false);
        assert!(!unchanged.contains('●'));
    }

    #[test]
    fn settings_lines_render_one_row_per_field() {
        let config = sample_config();
        let lines = settings_lines("", &config);
        // One row per fixed field, then one per shipped-skill feature.
        assert_eq!(lines.len(), Field::ALL.len() + SkillFeature::ALL.len());
        assert!(lines[0].contains("Theme"));
        assert!(lines[0].contains("Dark"));
        assert!(lines[1].contains("Default Workspace"));
        assert!(lines[1].contains("alpha"));
        assert!(lines[2].contains("Notifications"));
        assert!(lines[2].contains("On"));
        assert!(lines[3].contains("Restore Panes"));
        assert!(lines[3].contains("On"));
        assert!(lines[4].contains("Agent CLI"));
        // Each field shows its single current value via the chooser.
        assert!(lines[4].contains("Claude"));
        assert!(lines[5].contains("Session Action UI"));
        assert!(lines[5].contains("Menu"));
        // The 没入 key-scheme row is a chooser like the others.
        assert!(lines[6].contains("Terminal Keys"));
        assert!(lines[6].contains("Ctrl-O prefix"));
        assert!(lines[7].contains("Mascot Animation"));
        assert!(lines[7].contains("On"));
        // The Local LLM row (index 8) is an action button: plain "Install",
        // with no chevrons.
        assert!(lines[8].contains("Local LLM"));
        assert!(lines[8].contains("Install"));
        assert!(!lines[8].contains('<'));
        // The model row (index 9) is inert until the runtime is installed: a
        // plain "—" with no chevrons.
        assert!(lines[9].contains("Local LLM Model"));
        assert!(lines[9].contains('—'));
        assert!(!lines[9].contains('<'));
        // The shipped-skill feature rows follow the fixed fields: a chooser
        // showing the feature's on/off state (on by default).
        assert!(lines[10].contains("PR Skills"));
        assert!(lines[10].contains("On"));
        // Every other field is a chooser, so chevrons appear on those rows...
        assert!(lines
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 8 && *i != 9)
            .all(|(_, l)| l.contains('<') && l.contains('>')));
        // ...but only the focused (first) row carries the cursor.
        assert!(has_cursor(&lines[0]));
        assert_eq!(lines.iter().filter(|l| has_cursor(l)).count(), 1);
        // Nothing is edited in a fresh config, so no row is flagged changed.
        assert!(lines.iter().all(|l| !l.contains('●')));
    }

    #[test]
    fn settings_lines_flag_an_edited_field() {
        let mut config = sample_config();
        config.cycle_selected(true); // edit the focused Theme field
        let lines = settings_lines("", &config);
        assert!(lines[0].contains('●'));
        assert!(lines[1..].iter().all(|l| !l.contains('●')));
    }

    #[test]
    fn notice_lines_reserve_a_slot_when_absent() {
        let lines = notice_lines("", None);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.is_empty()));
    }

    #[test]
    fn notice_lines_render_text_when_present() {
        let lines = notice_lines("", Some("Saved 🐰"));
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("Saved"));
    }

    #[test]
    fn footer_lines_include_help_text() {
        let lines = footer_lines(80);
        assert!(lines.iter().any(|l| l.contains("Esc")));
        // `Space` is surfaced so the install / model-picker modals are findable.
        assert!(lines.iter().any(|l| l.contains("Space")));
        assert!(lines.iter().any(|l| l.contains("Enter")));
    }

    #[test]
    fn render_frame_clamps_rows_to_a_narrow_terminal() {
        // A long notice on a narrow terminal must be clipped, not wrapped: every
        // rendered line stays within the terminal width.
        let config = sample_config();
        let width = 30;
        let frame = render_frame(
            24,
            width,
            &config,
            Some("とても長い通知メッセージ".repeat(5).as_str()),
        );
        for line in &frame {
            assert!(
                console::measure_text_width(line) <= width,
                "line exceeds width {width}: {line:?}"
            );
        }
    }

    #[test]
    fn save_button_line_renders_the_button_and_cursor() {
        // Dirty + selected: shows the label and carries the cursor.
        let selected = save_button_line("", true, true);
        assert!(selected.contains("Save"));
        assert!(selected.contains('>'));
        // Clean + unselected: still shows the label, no cursor.
        let idle = save_button_line("", false, false);
        assert!(idle.contains("Save"));
        assert!(!idle.contains('>'));
    }

    #[test]
    fn render_frame_combines_all_sections() {
        let config = sample_config();
        let frame = render_frame(0, 0, &config, Some("Saved 🐰"));
        let joined = frame.join("\n");
        assert!(joined.contains("Config"));
        assert!(joined.contains("Theme"));
        assert!(joined.contains("Dark"));
        // The Save button is part of every frame.
        assert!(joined.contains("Save"));
        assert!(joined.contains("Saved"));
        assert!(joined.contains("Esc"));
    }

    #[test]
    fn render_frame_marks_the_save_button_when_focused() {
        let mut config = sample_config();
        // Move the cursor down onto the Save button (past every field, including
        // the shipped-skill feature rows).
        for _ in 0..config.save_index() {
            config.move_down();
        }
        assert!(config.is_save_selected());
        let frame = render_frame(24, 80, &config, None);
        // No field row carries the cursor; the Save row does.
        let cursor_rows: Vec<&String> = frame.iter().filter(|l| has_cursor(l)).collect();
        assert_eq!(cursor_rows.len(), 1);
        assert!(cursor_rows[0].contains("Save"));
    }

    #[test]
    fn render_frame_centers_body_and_pins_footer() {
        let config = sample_config();
        let height = 40;
        let frame = render_frame(height, 80, &config, None);

        assert_eq!(frame.len(), height);
        assert!(frame.last().unwrap().contains("Esc"));
        let top_padding = frame.iter().take_while(|l| l.is_empty()).count();
        assert!(top_padding > 0);
        assert!(!frame[top_padding].is_empty());
    }

    #[test]
    fn mascot_anchors_to_the_shared_welcome_row_so_it_never_jumps() {
        // The mascot sits on exactly the row the welcome screen places it, so the
        // rabbit does not shift (no CLS) when moving between the screens. Given the
        // terminal room to hold the anchor (a short terminal pulls it up), the
        // settings screen lines its rabbit up with the rest.
        let config = sample_config();
        for height in [40usize, 50, 60] {
            let frame = render_frame(height, 80, &config, None);
            let row = welcome::mascot_top_padding(height);
            assert!(console::strip_ansi_codes(&frame[row]).contains("(\\(\\"));
        }
    }

    #[test]
    fn render_frame_does_not_overflow_a_short_terminal() {
        let config = sample_config();
        let frame = render_frame(3, 80, &config, None);
        assert!(!frame[0].is_empty());
        assert!(frame.last().unwrap().contains("Esc"));
    }

    #[test]
    fn notice_slot_keeps_layout_stable_across_toggling() {
        let config = sample_config();
        let without = render_frame(24, 80, &config, None);
        let with = render_frame(24, 80, &config, Some("Saved 🐰"));
        assert_eq!(without.len(), with.len());
    }

    #[test]
    fn action_label_is_a_plain_button_without_chevrons() {
        let focused = action_label("Install", true);
        let idle = action_label("Install", false);
        assert!(focused.contains("Install"));
        assert!(idle.contains("Install"));
        // No chevrons: it reads as an action, not a left/right chooser.
        assert!(!focused.contains('<') && !focused.contains('>'));
    }

    /// A config focused on the (uninstalled) Local LLM row with the install
    /// modal open and `password` typed in.
    fn config_with_open_modal(password: &str) -> Config {
        let mut config = sample_config();
        while config.selected_field() != Some(Field::LocalLlm) {
            config.move_down();
        }
        config.open_install_modal();
        for c in password.chars() {
            config.install_modal_push(c);
        }
        config
    }

    #[test]
    fn render_frame_overlays_the_install_modal_and_masks_the_password() {
        let config = config_with_open_modal("ab");
        let joined = render_frame(24, 80, &config, None).join("\n");
        assert!(joined.contains("Local LLM"));
        assert!(joined.contains("sudo"));
        assert!(joined.contains("インストール"));
        // The password shows only as bullets, never in the clear.
        assert!(joined.contains("••"));
        assert!(!joined.contains("ab"));
        // The modal replaces the settings list (no Save button behind it).
        assert!(!joined.contains("Save"));
    }

    /// A config with the runtime installed, focused on the model row, with the
    /// model picker open and `installed` models flagged as pulled.
    fn config_with_open_model_modal(installed: &[&str]) -> Config {
        let mut config = sample_config();
        config.set_ollama_installed(true);
        config.set_installed_models(installed.iter().map(|m| m.to_string()).collect());
        while config.selected_field() != Some(Field::LocalLlmModel) {
            config.move_down();
        }
        config.open_model_modal();
        config
    }

    #[test]
    fn render_frame_overlays_the_model_picker_with_install_markers() {
        // The default model is pulled; the others are not, so both the ✓ and ⬇
        // markers (and the selected vs. dim row styles) are exercised.
        let config = config_with_open_model_modal(&["qwen2.5-coder:7b"]);
        let joined = render_frame(24, 80, &config, None).join("\n");
        assert!(joined.contains("Local LLM Model"));
        // Every offered model is listed.
        for model in crate::domain::settings::LOCAL_LLM_MODELS {
            assert!(joined.contains(model));
        }
        // Both install states render their marker.
        assert!(joined.contains("導入済"));
        assert!(joined.contains("未導入"));
        // The picker replaces the settings list.
        assert!(!joined.contains("Save"));
    }
}
