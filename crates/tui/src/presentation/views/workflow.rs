//! Fixed progress header and instruction composer around a scrollable history.

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{clip_to_width, pad_to_width};
use crate::usecase::application::workflow::{WorkflowFreshness, WorkflowPanel};
use usagi_core::domain::workflow::{Delivery, Outcome};

/// Column the agent selectors open in, so `Planner` and `Implementer` put their
/// `< value >` under each other instead of stepping right with the label.
const AGENT_LABEL_WIDTH: usize = "Implementer:".len();

/// Render within the pane's content rectangle, never over its tab strip.
#[must_use]
pub fn render(height: usize, width: usize, panel: &WorkflowPanel) -> Vec<String> {
    if height == 0 || width == 0 {
        return vec![String::new(); height];
    }
    let header = header(panel);
    let composer = composer(panel, width);

    // Even a short terminal keeps one status row and the end of the composer.
    let composer_height = composer.len().min(height.saturating_sub(1));
    let header_height = header.len().min(height - composer_height);
    let history_height = height - composer_height - header_height;
    // `header` and the history rows below already sanitized every untrusted
    // fragment before painting it, so nothing re-sanitizes them here: that pass
    // would strip the SGR this pane is drawn with.
    let mut rows = header.into_iter().take(header_height).collect::<Vec<_>>();
    let history = history(panel);
    let end = history.len().saturating_sub(panel.history_offset);
    let start = end.saturating_sub(history_height);
    rows.extend(history[start..end].iter().cloned());
    rows.resize(header_height + history_height, String::new());
    rows.extend(
        composer
            .into_iter()
            .rev()
            .take(composer_height)
            .collect::<Vec<_>>()
            .into_iter()
            .rev(),
    );
    rows.into_iter()
        .map(|line| pad_to_width(&clip_to_width(&line, width), width))
        .collect()
}

/// The fixed bottom block: what the draft is addressed to, the draft itself,
/// and the one line that says what a key does right now.
fn composer(panel: &WorkflowPanel, width: usize) -> Vec<String> {
    let mut composer = vec![Role::Accent.style().bold().paint(&if panel.run.is_some() {
        format!("Instruction to: {}", panel.recipient_label())
    } else {
        "Goal".to_owned()
    })];
    composer.extend(input_rows(panel, width));
    composer.push(if panel.submitting {
        Role::Warning
            .style()
            .paint("Submitting... (draft retained)")
    } else if panel.pending.is_some() {
        Role::Warning
            .style()
            .paint("Ctrl+S: retry previous request (same operation ID)")
    } else if panel.run.is_some() {
        Style::new()
            .dim()
            .paint("Enter: newline | Tab: recipient | Ctrl+S: submit")
    } else {
        Style::new()
            .dim()
            .paint("Tab: goal/agents | Left/Right: choose | Ctrl+S: start")
    });
    composer
}

/// Ended runs come first: they are the oldest thing that happened here, and a
/// restarted session should still show what it already tried.
fn history(panel: &WorkflowPanel) -> Vec<String> {
    let mut history = panel
        .finished
        .iter()
        .map(|ended| {
            let tag = format!("[{}]", ended.outcome.label());
            format!(
                "{} {} ({})",
                if ended.outcome == Outcome::Completed {
                    Role::Success.style().paint(&tag)
                } else {
                    Style::new().dim().paint(&tag)
                },
                safe_line(&ended.goal.replace('\n', " / ")),
                Style::new().dim().paint(ended.phase.label())
            )
        })
        .collect::<Vec<_>>();
    history.extend(panel.run.as_ref().map_or_else(Vec::new, |run| {
        let mut rows = run
            .history
            .iter()
            .map(|entry| {
                format!(
                    "{}: {}",
                    Role::Accent.style().paint(&safe_line(&entry.actor)),
                    safe_line(&entry.body.replace('\n', " / "))
                )
            })
            .collect::<Vec<_>>();
        rows.extend(run.instructions.iter().map(|instruction| {
            let delivery = match instruction.delivery {
                Delivery::Queued => "queued",
                Delivery::Notified => "notified",
                Delivery::Acknowledged => "processed",
                Delivery::Unconfirmed => "delivery unconfirmed",
            };
            let tag = format!("[{delivery}]");
            format!(
                "{} {}",
                if instruction.delivery == Delivery::Unconfirmed {
                    Role::Warning.style().paint(&tag)
                } else {
                    Style::new().dim().paint(&tag)
                },
                safe_line(&instruction.body.replace('\n', " / "))
            )
        }));
        rows
    }));
    history
}

/// The three provider choices, each opening its `< value >` in the same column
/// so the longest label cannot push its own value out of line.
fn agent_rows(panel: &WorkflowPanel) -> Vec<String> {
    [
        (0, "Planner", panel.agents.planner),
        (1, "Implementer", panel.agents.implementer),
        (2, "Reviewer", panel.agents.reviewer),
    ]
    .into_iter()
    .map(|(index, label, provider)| {
        let focused = panel.agent_field == Some(index);
        let marker = if focused {
            Role::Danger.style().bold().paint(">")
        } else {
            " ".to_owned()
        };
        let name = if provider == usagi_core::domain::settings::DefaultModel::Agy {
            "Gemini (agy)"
        } else {
            provider.selector()
        };
        let value = if focused {
            Role::Accent.style().bold()
        } else {
            Role::Accent.style()
        };
        let arrow = if focused {
            Role::Accent.style().bold()
        } else {
            Style::new().dim()
        };
        format!(
            "{marker} {:<width$} {} {} {}",
            format!("{label}:"),
            arrow.paint("<"),
            value.paint(name),
            arrow.paint(">"),
            width = AGENT_LABEL_WIDTH,
        )
    })
    .collect()
}

/// What a started run is doing, and what it is waiting on.
fn run_rows(run: &usagi_core::domain::workflow::WorkflowRun) -> Vec<String> {
    let owner = match run.phase {
        usagi_core::domain::workflow::Phase::Reviewing => run.agents.reviewer.selector(),
        usagi_core::domain::workflow::Phase::Waiting => "Human decision",
        usagi_core::domain::workflow::Phase::Ready => "None (complete)",
        _ => run.agents.implementer.selector(),
    };
    let mut rows = vec![
        format!("Current owner: {}", Role::Accent.style().paint(owner)),
        Style::new().dim().paint(&format!(
            "Implement -> Review -> PR ready | revisions {}/{}",
            run.revisions, run.revision_limit
        )),
    ];
    if let Some(issue) = run.issue {
        rows.push(format!(
            "Issue: {} (PR must mark it done)",
            Role::Info.style().paint(&format!("#{issue}"))
        ));
    }
    if let Some(reason) = &run.waiting_reason {
        rows.push(Role::Warning.style().paint(&safe_line(reason)));
    }
    if let Some(review) = &run.review {
        rows.push(format!(
            "Review HEAD: {}",
            Style::new()
                .dim()
                .paint(&safe_line(&review.target.head_sha))
        ));
    }
    rows
}

fn header(panel: &WorkflowPanel) -> Vec<String> {
    // Only the very first read announces itself. The pane re-reads the daemon
    // on a steady cadence, and replacing the status it just fetched with
    // "Loading" on every one of those made the header flicker between two
    // strings for as long as the tab stayed open.
    let status = if panel.loading && panel.freshness == WorkflowFreshness::Pending {
        Role::Warning.style().bold().paint("Loading workflow")
    } else {
        Role::Accent
            .style()
            .bold()
            .paint(&panel.run.as_ref().map_or_else(
                || "Implementation + Review / Not started".to_owned(),
                |run| format!("Implementation + Review / {}", run.phase.label()),
            ))
    };
    let mut header = vec![status];
    if let Some(run) = &panel.run {
        header.extend(run_rows(run));
    } else {
        header.extend(agent_rows(panel));
    }
    if let Some(error) = &panel.error {
        header.push(
            Role::Danger
                .style()
                .paint(&format!("Error: {}", safe_line(error))),
        );
    }
    // Offered in every phase, not only the two that are already the person's
    // turn: the run that most needs ending is the one still insisting it is
    // working. A start the daemon refused needs it just as badly — every
    // Ctrl+S there resends the same rejected request, so without this line the
    // pane tells the person to retry and nothing else. It goes last because a
    // short pane keeps the header's first rows, and a standing hint is worth
    // less than an error or a waiting reason.
    if panel.run.is_some() {
        header.push(
            Style::new()
                .dim()
                .paint("Closeup `workflow finish` ends this run"),
        );
    } else if panel.pending.is_some() && panel.error.is_some() {
        header.push(
            Style::new()
                .dim()
                .paint("Closeup `workflow finish` abandons this start"),
        );
    }
    header
}

fn safe_line(line: &str) -> String {
    let line = line.replace(
        [
            crate::presentation::frame::INPUT_CURSOR_MARKER,
            crate::presentation::frame::TERMINAL_CURSOR_MARKER,
        ],
        "",
    );
    usagi_core::domain::presentation_text::sanitize_presentation_line(&line)
}

fn input_rows(panel: &WorkflowPanel, width: usize) -> Vec<String> {
    let value = panel.draft.value();
    let cursor = panel.draft.cursor();
    let current = value[..cursor]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let line_start = value[..cursor].rfind('\n').map_or(0, |index| index + 1);
    value
        .split('\n')
        .enumerate()
        .skip(current.saturating_sub(2))
        .take(3)
        .map(|(index, line)| {
            let safe = safe_line(line);
            let marker = Style::new().dim().paint(">");
            if index != current {
                return format!("{marker} {}", Role::Accent.style().paint(&safe));
            }
            let caret = safe_line(&line[..cursor - line_start]).len();
            let mut start = 0;
            while start < caret
                && crate::presentation::widgets::display_width(&safe[start..caret])
                    >= width.saturating_sub(3)
            {
                start += safe[start..].chars().next().map_or(0, char::len_utf8);
            }
            format!(
                "{marker} {}",
                crate::presentation::widgets::block_caret(
                    &safe[start..],
                    caret - start,
                    &Role::Accent.style()
                )
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pane is drawn with SGR, so assertions read the text a person sees.
    fn strip(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) && c != '[' {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    fn plain(rows: &[String]) -> String {
        rows.iter()
            .map(|row| strip(row))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_run_started_from_an_issue_names_it() {
        let mut run = crate::usecase::application::workflow::fixture_run(
            usagi_core::domain::id::SessionId::new(),
        );
        run.issue = Some(742);
        let panel = WorkflowPanel {
            run: Some(run),
            ..WorkflowPanel::default()
        };
        assert!(plain(&render(20, 100, &panel)).contains("Issue: #742"));
    }

    #[test]
    fn ended_runs_stay_visible_and_every_phase_offers_the_way_out() {
        use usagi_core::domain::workflow::{FinishedRun, Outcome, Phase};
        let run = crate::usecase::application::workflow::fixture_run(
            usagi_core::domain::id::SessionId::new(),
        );
        let ended = |outcome, phase, goal: &str| FinishedRun {
            id: usagi_core::domain::id::OperationId::new(),
            outcome,
            goal: goal.to_owned(),
            phase,
            issue: None,
            pr_url: None,
        };
        let mut panel = WorkflowPanel {
            finished: vec![
                ended(Outcome::Completed, Phase::Ready, "Ship login"),
                ended(Outcome::Stopped, Phase::Revising, "Rewrite\nthe parser"),
            ],
            ..WorkflowPanel::default()
        };

        // Ended runs are readable before anything new starts, and a multi-line
        // goal stays on one row.
        let rendered = plain(&render(20, 100, &panel));
        assert!(rendered.contains("[completed] Ship login (PR ready)"));
        assert!(rendered.contains("[stopped] Rewrite / the parser (Revising)"));

        // A run the person wants to abandon is usually one that still claims to
        // be working, so the way out is offered in every phase.
        panel.run = Some(run);
        for phase in [
            Phase::Implementing,
            Phase::Reviewing,
            Phase::Ready,
            Phase::Waiting,
        ] {
            panel.run.as_mut().unwrap().phase = phase;
            assert!(
                plain(&render(20, 100, &panel)).contains("Closeup `workflow finish` ends this run"),
                "{phase:?} still offers the way out"
            );
        }
        // The archive survives alongside a new run.
        assert!(plain(&render(20, 100, &panel)).contains("[completed] Ship login"));

        // A short pane keeps the header's first rows, so the standing hint has to
        // sit after everything it must not displace. Assert the order itself,
        // which holds at any size, and then the one height where the two
        // actually compete.
        let run = panel.run.as_mut().unwrap();
        run.phase = Phase::Waiting;
        run.waiting_reason = Some("Revision limit reached".into());
        let position =
            |rows: &[String], needle: &str| rows.iter().position(|row| row.contains(needle));
        let full = render(20, 100, &panel);
        assert!(position(&full, "Revision limit reached") < position(&full, "workflow finish"));
        let short = render(7, 100, &panel);
        assert!(position(&short, "Revision limit reached").is_some());
        assert!(position(&short, "workflow finish").is_none());
    }

    #[test]
    fn workflow_renders_selected_providers_and_focus() {
        use usagi_core::domain::settings::DefaultModel;
        let mut panel = WorkflowPanel::default();
        panel.agents.planner = DefaultModel::Agy;
        panel.agents.implementer = DefaultModel::Claude;
        panel.agent_field = Some(1);
        panel.agents.reviewer = DefaultModel::OpenAi;
        let rows = render(20, 100, &panel)
            .into_iter()
            .map(|row| strip(&row))
            .collect::<Vec<_>>();
        let row = |needle: &str| {
            rows.iter()
                .find(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle} is drawn"))
                .clone()
        };
        let planner = row("Planner:");
        let implementer = row("Implementer:");
        let reviewer = row("Reviewer:");
        assert!(planner.contains("< Gemini (agy) >"));
        assert!(implementer.contains("< claude >"));
        assert!(reviewer.contains("< codex >"));
        // The three selectors open in the same column, so the longest label
        // cannot push its own value out of the column the others use.
        let opens_at = |row: &str| row.find('<').expect("the selector opens");
        assert_eq!(opens_at(&planner), opens_at(&implementer));
        assert_eq!(opens_at(&planner), opens_at(&reviewer));
        // Only the focused row carries the cursor.
        assert!(implementer.starts_with('>'));
        assert!(planner.starts_with(' ') && reviewer.starts_with(' '));

        let mut run = crate::usecase::application::workflow::fixture_run(
            usagi_core::domain::id::SessionId::new(),
        );
        run.agents = panel.agents;
        panel.run = Some(run);
        assert!(plain(&render(20, 100, &panel)).contains("Current owner: claude"));
        panel.run.as_mut().unwrap().phase = usagi_core::domain::workflow::Phase::Reviewing;
        assert!(plain(&render(20, 100, &panel)).contains("Current owner: codex"));
    }

    #[test]
    fn a_refresh_after_the_first_snapshot_keeps_the_status_it_fetched() {
        let mut panel = WorkflowPanel {
            loading: true,
            ..WorkflowPanel::default()
        };
        // The first read is the only one worth announcing.
        assert!(plain(&render(20, 90, &panel)).contains("Loading workflow"));
        panel.freshness = WorkflowFreshness::Observed;
        let steady = plain(&render(20, 90, &panel));
        assert!(!steady.contains("Loading workflow"));
        assert!(steady.contains("Implementation + Review / Not started"));
    }

    #[test]
    fn a_refused_start_is_told_how_to_be_abandoned() {
        use usagi_core::domain::workflow::WorkflowCommand;
        let mut panel = WorkflowPanel {
            pending: Some((
                usagi_core::domain::id::OperationId::new(),
                WorkflowCommand::Start {
                    goal: "task".into(),
                    agents: usagi_core::domain::workflow::WorkflowAgents::default(),
                },
            )),
            ..WorkflowPanel::default()
        };
        // A start still waiting on its own answer is not stuck, so nothing is
        // offered yet.
        assert!(!plain(&render(20, 90, &panel)).contains("workflow finish"));
        // Once the daemon has refused it, every Ctrl+S resends the same
        // rejected request: the way out has to be on screen.
        panel.error = Some("stop the session's existing Agent".into());
        let refused = plain(&render(20, 90, &panel));
        assert!(refused.contains("Closeup `workflow finish` abandons this start"));
        // It stays the lowest-priority row, below the error it explains.
        let position = |needle: &str| refused.lines().position(|row| row.contains(needle));
        assert!(position("Error:") < position("abandons this start"));
    }

    #[test]
    fn renders_real_progress_history_delivery_and_unconfirmed_controls_safely() {
        use usagi_core::domain::id::{OperationId, SessionId};
        use usagi_core::domain::workflow::{
            Instruction, Phase, Recipient, Review, WorkflowCommand, WorkflowHistoryEntry,
        };
        let mut run = crate::usecase::application::workflow::fixture_run(SessionId::new());
        run.waiting_reason = Some("Check results pending".into());
        run.review = Some(Review {
            request: OperationId::new(),
            target: usagi_core::domain::agent_message::ReviewTarget {
                base_sha: "a".repeat(40),
                head_sha: "b".repeat(40),
            },
            approved: true,
        });
        run.history.push(WorkflowHistoryEntry {
            id: OperationId::new(),
            actor: "Claude".into(),
            body: "Review completed".into(),
        });
        for delivery in [
            Delivery::Queued,
            Delivery::Notified,
            Delivery::Acknowledged,
            Delivery::Unconfirmed,
        ] {
            run.instructions.push(Instruction {
                id: OperationId::new(),
                requested_recipient: Recipient::Automatic,
                recipient: run.implementer,
                body: "Check\nerrors".into(),
                delivery,
            });
        }
        let mut panel = WorkflowPanel {
            run: Some(run),
            error: Some("\u{1b}[2Junsafe\u{202e}".into()),
            ..WorkflowPanel::default()
        };
        panel
            .draft
            .replace("line one\nline two\nline three\nline four");
        for phase in [
            Phase::Starting,
            Phase::Implementing,
            Phase::Reviewing,
            Phase::Revising,
            Phase::Verifying,
            Phase::Waiting,
            Phase::Ready,
        ] {
            panel.run.as_mut().unwrap().phase = phase;
            let view = render(25, 90, &panel).join("\n");
            assert!(view.contains(phase.label()));
            assert!(view.contains("Review completed"));
            assert!(view.contains("delivery unconfirmed"));
            assert!(!view.contains("\u{1b}[2J"));
            assert!(!view.contains('\u{202e}'));
        }
        panel.submitting = true;
        assert!(render(20, 90, &panel).join("\n").contains("Submitting"));
        panel.submitting = false;
        panel.pending = Some((
            OperationId::new(),
            WorkflowCommand::Start {
                goal: "task".into(),
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            },
        ));
        assert!(render(20, 90, &panel).join("\n").contains("retry previous"));
        panel.run = None;
        panel.loading = true;
        assert!(
            render(20, 90, &panel)
                .join("\n")
                .contains("Loading workflow")
        );
        panel.draft.move_edge(false);
        assert_eq!(input_rows(&panel, 10).len(), 3);
        panel.draft.replace("日本語の非常に長い入力を確認する");
        assert!(
            input_rows(&panel, 12)[0].contains(crate::presentation::frame::INPUT_CURSOR_MARKER)
        );
    }

    #[test]
    fn narrow_and_short_views_keep_bounds_and_the_composer() {
        let mut panel = WorkflowPanel::default();
        panel
            .draft
            .replace("Check authentication\nInclude error cases");
        for height in 0..16 {
            for width in [0, 1, 12, 80] {
                let rows = render(height, width, &panel);
                assert_eq!(rows.len(), height);
                assert!(
                    rows.iter()
                        .all(|row| crate::presentation::widgets::display_width(row) <= width)
                );
            }
        }
        let rows = render(16, 80, &panel).join("\n");
        assert!(rows.contains("Not started"));
        assert!(rows.contains("Check authentication"));
        assert!(rows.contains("Ctrl+S"));
    }
}
