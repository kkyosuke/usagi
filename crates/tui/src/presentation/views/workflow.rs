//! Fixed progress header and instruction composer around a scrollable history.

use crate::presentation::theme::{Color, Role, Style};
use crate::presentation::widgets::{clip_to_width, pad_to_width};
use crate::usecase::application::workflow::{WorkflowFreshness, WorkflowPanel};
use usagi_core::domain::workflow::{Delivery, Outcome};

/// Column the agent selectors open in, so `Planner` and `Implementer` put their
/// `< value >` under each other instead of stepping right with the label.
const AGENT_LABEL_WIDTH: usize = "Implementer:".len();

/// Secondary text: white *and* dim, never a bare `dim()`.
///
/// `Style::new().dim()` emits SGR 2 over whatever foreground the emulator
/// happens to default to, which is exactly the combination that renders as
/// near-invisible grey on the palettes people actually use. Naming the base
/// colour is the same discipline
/// [`session_tab`](crate::presentation::widgets::session_tab) already applies.
///
/// It is for captions and key hints only. Anything that reports what the run is
/// doing stays at full brightness — a progress line the reader has to lean in
/// for is the bug this pane was reported with.
fn muted() -> Style {
    Style::new().fg(Color::White).dim()
}

/// How this pane names a provider. `agy` is the odd one out: its selector is the
/// CLI's name, which says nothing about the model behind it.
fn provider_name(provider: usagi_core::domain::settings::DefaultModel) -> &'static str {
    if provider == usagi_core::domain::settings::DefaultModel::Agy {
        "Gemini (agy)"
    } else {
        provider.selector()
    }
}

/// Wall time of a history entry in the reader's own zone.
///
/// Records written before the history carried time have none; they draw a
/// placeholder of the same width so the column does not jump.
fn clock(at: Option<chrono::DateTime<chrono::Utc>>) -> String {
    at.map_or_else(
        || "--:--".to_owned(),
        |at| at.with_timezone(&chrono::Local).format("%H:%M").to_string(),
    )
}

/// A goal is free text the person typed; it is shown on one row.
fn one_line(text: &str) -> String {
    safe_line(&text.replace('\n', " / "))
}

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
    // Clamp to the viewport, not just to the row count. `rows - 1` alone leaves
    // every offset past `rows - history_height` drawing a part-empty window, so
    // one page back from the top would show a handful of rows over blank lines —
    // the same near-empty pane the bound exists to prevent.
    let end = history.len().saturating_sub(
        panel
            .history_offset
            .min(history.len().saturating_sub(history_height)),
    );
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
        muted().paint(
            "Enter: newline | Tab: recipient | Ctrl+S: submit | PgUp/PgDn+Shift+End: history",
        )
    } else {
        muted().paint(
            "Tab: goal/agents | Left/Right: choose | Ctrl+S: start | PgUp/PgDn+Shift+End: history",
        )
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
                    Role::Warning.style().paint(&tag)
                },
                one_line(&ended.goal),
                muted().paint(ended.phase.label()),
            ) + &ended.pr_url.as_deref().map_or_else(String::new, |url| {
                format!(" {}", Role::Info.style().paint(&safe_line(url)))
            })
        })
        .collect::<Vec<_>>();
    history.extend(panel.run.as_ref().map_or_else(Vec::new, |run| {
        let mut rows = run
            .history
            .iter()
            .map(|entry| {
                format!(
                    "{} {} {}: {}",
                    if entry.advanced {
                        Role::Success.style().paint("|>")
                    } else {
                        muted().paint(" ·")
                    },
                    muted().paint(&clock(entry.at)),
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
                match instruction.delivery {
                    Delivery::Unconfirmed => Role::Warning.style().paint(&tag),
                    Delivery::Acknowledged => Role::Success.style().paint(&tag),
                    Delivery::Queued | Delivery::Notified => muted().paint(&tag),
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
        let name = provider_name(provider);
        let value = if focused {
            Role::Accent.style().bold()
        } else {
            Role::Accent.style()
        };
        let arrow = if focused {
            Role::Accent.style().bold()
        } else {
            muted()
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

/// What a started run is doing, who is on it, and what it is waiting on.
///
/// Rows are ordered by what a short pane must keep. A goal and the participants
/// are what makes the run identifiable at all, so they sit above the pipeline
/// and the reference detail; the reason the run stopped moving is handled by
/// [`header`] and outranks all of it.
fn run_rows(run: &usagi_core::domain::workflow::WorkflowRun) -> Vec<String> {
    let owner = match run.phase {
        usagi_core::domain::workflow::Phase::Reviewing => provider_name(run.agents.reviewer),
        usagi_core::domain::workflow::Phase::Waiting => "Human decision",
        usagi_core::domain::workflow::Phase::Ready => "None (complete)",
        _ => provider_name(run.agents.implementer),
    };
    let mut rows = vec![
        // Without this the pane stops saying what it was asked to do the moment
        // the run starts: the goal was only ever drawn for *ended* runs.
        format!("Goal: {}", one_line(&run.goal)),
        format!(
            "Current owner: {} {}",
            Role::Accent.style().paint(owner),
            muted().paint(&format!(
                "| plan {} · impl {} · review {}",
                provider_name(run.agents.planner),
                provider_name(run.agents.implementer),
                provider_name(run.agents.reviewer),
            )),
        ),
        format!(
            "{} | revisions {}/{}",
            muted().paint("Implement -> Review -> PR ready"),
            run.revisions,
            run.revision_limit
        ),
    ];
    if let Some(issue) = run.issue {
        rows.push(format!(
            "Issue: {} (PR must mark it done)",
            Role::Info.style().paint(&format!("#{issue}"))
        ));
    }
    // The run exists to produce this. It was stored but never drawn, so a run
    // that reached `PR ready` gave the person no way to reach its PR.
    if let Some(url) = &run.pr_url {
        rows.push(format!("PR: {}", Role::Info.style().paint(&safe_line(url))));
    }
    if let Some(review) = &run.review {
        rows.push(format!(
            "Review HEAD: {}",
            muted().paint(&safe_line(&review.target.head_sha))
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
    // Why the run stopped moving outranks everything describing the run: on a
    // short pane these are the rows that must survive.
    if let Some(reason) = panel
        .run
        .as_ref()
        .and_then(|run| run.waiting_reason.as_ref())
    {
        header.push(Role::Warning.style().paint(&safe_line(reason)));
    }
    if let Some(error) = &panel.error {
        header.push(
            Role::Danger
                .style()
                .paint(&format!("Error: {}", safe_line(error))),
        );
    }
    if let Some(run) = &panel.run {
        header.extend(run_rows(run));
    } else {
        header.extend(agent_rows(panel));
    }
    // Offered in every phase, not only the two that are already the person's
    // turn: the run that most needs ending is the one still insisting it is
    // working. A start the daemon refused needs it just as badly — every
    // Ctrl+S there resends the same rejected request, so without this line the
    // pane tells the person to retry and nothing else. It goes last because a
    // short pane keeps the header's first rows, and a standing hint is worth
    // less than an error or a waiting reason.
    if panel.run.is_some() {
        header.push(muted().paint("Closeup `workflow finish` ends this run"));
    } else if panel.error.is_some()
        && matches!(
            panel.pending,
            Some((
                _,
                usagi_core::domain::workflow::WorkflowCommand::Start { .. }
            ))
        )
    {
        header.push(muted().paint("Closeup `workflow finish` abandons this start"));
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
            let marker = muted().paint(">");
            if index != current {
                return format!("{marker} {safe}");
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
                    &Style::new()
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
        // `expect` rather than `unwrap_or_else(|| panic!(..))`: a panic closure
        // that never runs is an uncovered function, and this crate's gate is
        // 100%.
        let row = |needle: &str| {
            rows.iter()
                .find(|row| row.contains(needle))
                .expect("the agent row is drawn")
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
        // `render` paints these rows and no longer sanitizes them afterwards,
        // so every fragment has to be sanitized where it is built. Poison all
        // of them, not just the error: this test is the backstop that keeps the
        // per-fragment discipline honest. `clip_to_width` drops the bidi and
        // control characters on its own but deliberately passes ESC through as
        // styling, so ESC is the class that rests entirely on `safe_line`.
        let poison = "\u{1b}[2Junsafe\u{202e}";
        let mut run = crate::usecase::application::workflow::fixture_run(SessionId::new());
        run.waiting_reason = Some(format!("Check results pending {poison}"));
        run.review = Some(Review {
            request: OperationId::new(),
            target: usagi_core::domain::agent_message::ReviewTarget {
                base_sha: "a".repeat(40),
                head_sha: format!("{poison}{}", "b".repeat(40)),
            },
            approved: true,
        });
        run.history.push(WorkflowHistoryEntry {
            id: OperationId::new(),
            actor: format!("Claude{poison}"),
            body: format!("Review completed {poison}"),
            at: None,
            kind: usagi_core::domain::agent_message::MessageKind::Message,
            advanced: false,
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
                body: format!("Check\nerrors {poison}"),
                delivery,
            });
        }
        let mut panel = WorkflowPanel {
            run: Some(run),
            error: Some(poison.to_owned()),
            finished: vec![usagi_core::domain::workflow::FinishedRun {
                id: OperationId::new(),
                outcome: usagi_core::domain::workflow::Outcome::Completed,
                goal: format!("Ship login {poison}"),
                phase: Phase::Ready,
                issue: None,
                pr_url: None,
            }],
            ..WorkflowPanel::default()
        };
        // The draft is the last untrusted path: the daemon's saved start goal is
        // pasted straight into it, and the editor does not sanitize on the way
        // in, so `input_rows` is the only thing standing between it and the
        // screen.
        let poisoned_draft = format!(
            "line one {poison}\nline two {poison}\nline three {poison}\nline four {poison}"
        );
        panel.draft.replace(&poisoned_draft);
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
            // Assert each poisoned row is actually on screen. Without this the
            // ESC assertion below goes quietly vacuous the day the header grows
            // past the pane and pushes a row out of the window.
            assert!(view.contains("Review completed"));
            assert!(view.contains("delivery unconfirmed"));
            assert!(view.contains("Ship login"));
            assert!(view.contains("Review HEAD"));
            assert!(view.contains("Check results pending"));
            assert!(view.contains("line four"));
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

    /// Every faint run in this pane names its own foreground.
    ///
    /// A bare `dim()` is `ESC[2m`, which leaves the base colour to the emulator
    /// and is what made the whole pane unreadable. White + dim is `ESC[2;37m`,
    /// so the bare form is detectable in the painted output and this test is the
    /// backstop that keeps it out.
    const BARE_DIM: &str = "\u{1b}[2m";

    #[test]
    fn nothing_is_painted_with_an_uncoloured_dim() {
        use usagi_core::domain::id::{OperationId, SessionId};
        use usagi_core::domain::workflow::{
            FinishedRun, Instruction, Outcome, Phase, Recipient, Review,
        };

        // Exercise every branch that used to carry a bare dim: the two composer
        // hints, both standing hints, ended runs, all four delivery states, the
        // unfocused selector arrows, the progress line, the review SHA and the
        // draft's own gutter marker.
        let mut panel = WorkflowPanel::default();
        panel.draft.replace("Ship login\nwith tests");
        assert!(!render(24, 100, &panel).join("\n").contains(BARE_DIM));

        panel.agent_field = Some(1);
        assert!(!render(24, 100, &panel).join("\n").contains(BARE_DIM));

        let mut run = crate::usecase::application::workflow::fixture_run(SessionId::new());
        run.review = Some(Review {
            request: OperationId::new(),
            target: usagi_core::domain::agent_message::ReviewTarget {
                base_sha: "a".repeat(40),
                head_sha: "b".repeat(40),
            },
            approved: true,
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
                body: "Check the error path".into(),
                delivery,
            });
        }
        panel.run = Some(run);
        panel.finished = vec![
            FinishedRun {
                id: OperationId::new(),
                outcome: Outcome::Completed,
                goal: "Ship login".into(),
                phase: Phase::Ready,
                issue: None,
                pr_url: None,
            },
            FinishedRun {
                id: OperationId::new(),
                outcome: Outcome::Stopped,
                goal: "Rewrite the parser".into(),
                phase: Phase::Revising,
                issue: None,
                pr_url: None,
            },
        ];
        let rendered = render(24, 100, &panel).join("\n");
        assert!(!rendered.contains(BARE_DIM));

        // A refused start reaches the other standing hint.
        panel.run = None;
        panel.error = Some("stop the session's existing Agent".into());
        panel.pending = Some((
            OperationId::new(),
            usagi_core::domain::workflow::WorkflowCommand::Start {
                goal: "task".into(),
                agents: usagi_core::domain::workflow::WorkflowAgents::default(),
            },
        ));
        assert!(!render(24, 100, &panel).join("\n").contains(BARE_DIM));
    }

    #[test]
    fn the_live_revision_count_is_not_dimmed_away() {
        let mut run = crate::usecase::application::workflow::fixture_run(
            usagi_core::domain::id::SessionId::new(),
        );
        run.revisions = 2;
        let panel = WorkflowPanel {
            run: Some(run),
            ..WorkflowPanel::default()
        };
        let rows = render(20, 100, &panel);
        let progress = rows
            .iter()
            .find(|row| strip(row).contains("revisions 2/3"))
            .expect("the progress row is drawn");
        // The static pipeline may be quiet; the count the person is watching is
        // outside every faint run on the line.
        let (_, after_count) = progress
            .split_once("revisions")
            .expect("the count is on this row");
        assert!(!after_count.contains(BARE_DIM));
        assert!(!after_count.contains("\u{1b}[2;37m"));
    }

    #[test]
    fn a_running_run_names_its_goal_participants_and_pr() {
        use usagi_core::domain::settings::DefaultModel;
        use usagi_core::domain::workflow::{Phase, WorkflowAgents};

        let mut run = crate::usecase::application::workflow::fixture_run(
            usagi_core::domain::id::SessionId::new(),
        );
        run.goal = "Implement login\nwith tests".into();
        run.agents = WorkflowAgents {
            planner: DefaultModel::Agy,
            implementer: DefaultModel::Claude,
            reviewer: DefaultModel::OpenAi,
        };
        let panel = WorkflowPanel {
            run: Some(run),
            ..WorkflowPanel::default()
        };
        let rendered = plain(&render(24, 120, &panel));
        // The goal used to be drawn for ended runs only, so a started run stopped
        // saying what it had been asked to do. A multi-line goal stays on one row.
        assert!(rendered.contains("Goal: Implement login / with tests"));
        // All three participants are named while the run is going, not just the
        // one whose turn it is.
        assert!(rendered.contains("Current owner: claude"));
        assert!(rendered.contains("plan Gemini (agy) · impl claude · review codex"));

        // The PR is the thing the run exists to produce; it was stored but never
        // drawn, so `PR ready` gave the person no way to reach it.
        let mut panel = panel;
        let run = panel.run.as_mut().expect("the run is set");
        run.phase = Phase::Ready;
        run.pr_url = Some("https://github.com/o/r/pull/7".into());
        assert!(plain(&render(24, 120, &panel)).contains("PR: https://github.com/o/r/pull/7"));
    }

    #[test]
    fn an_ended_run_keeps_the_pr_it_produced() {
        use usagi_core::domain::workflow::{FinishedRun, Outcome, Phase};
        let panel = WorkflowPanel {
            finished: vec![FinishedRun {
                id: usagi_core::domain::id::OperationId::new(),
                outcome: Outcome::Completed,
                goal: "Ship login".into(),
                phase: Phase::Ready,
                issue: None,
                pr_url: Some("https://github.com/o/r/pull/9".into()),
            }],
            ..WorkflowPanel::default()
        };
        let rendered = plain(&render(20, 120, &panel));
        assert!(
            rendered.contains("[completed] Ship login (PR ready) https://github.com/o/r/pull/9")
        );
    }

    #[test]
    fn a_short_pane_keeps_the_reason_the_run_stopped_moving() {
        use usagi_core::domain::workflow::Phase;
        let mut run = crate::usecase::application::workflow::fixture_run(
            usagi_core::domain::id::SessionId::new(),
        );
        run.phase = Phase::Waiting;
        run.waiting_reason = Some("Revision limit reached".into());
        run.goal = "Implement login".into();
        let panel = WorkflowPanel {
            run: Some(run),
            ..WorkflowPanel::default()
        };
        // Describing the run costs rows; the reason it is stuck must not be the
        // thing those rows push off a short pane.
        for height in 5..12 {
            assert!(
                plain(&render(height, 100, &panel)).contains("Revision limit reached"),
                "height {height} keeps the waiting reason"
            );
        }
    }

    #[test]
    fn the_scroll_bound_counts_the_rows_that_are_actually_drawn() {
        use usagi_core::domain::id::{OperationId, SessionId};
        use usagi_core::domain::workflow::{
            FinishedRun, Instruction, Outcome, Phase, Recipient, WorkflowHistoryEntry,
        };
        let mut run = crate::usecase::application::workflow::fixture_run(SessionId::new());
        for index in 0..4 {
            run.history.push(WorkflowHistoryEntry {
                id: OperationId::new(),
                actor: "claude".into(),
                body: format!("step {index}"),
                at: None,
                kind: usagi_core::domain::agent_message::MessageKind::Message,
                advanced: false,
            });
            run.instructions.push(Instruction {
                id: OperationId::new(),
                requested_recipient: Recipient::Automatic,
                recipient: run.implementer,
                body: format!("instruction {index}"),
                delivery: Delivery::Queued,
            });
        }
        let panel = WorkflowPanel {
            run: Some(run),
            finished: vec![FinishedRun {
                id: OperationId::new(),
                outcome: Outcome::Stopped,
                goal: "Earlier attempt".into(),
                phase: Phase::Revising,
                issue: None,
                pr_url: None,
            }],
            ..WorkflowPanel::default()
        };
        // `WorkflowPanel::history_rows` bounds the scroll; if it drifts from the
        // rows this view draws, the bound stops matching the window it bounds.
        assert_eq!(history(&panel).len(), panel.history_rows());
    }

    #[test]
    fn paging_back_keeps_the_history_window_full() {
        use usagi_core::domain::id::OperationId;
        use usagi_core::domain::workflow::{FinishedRun, Outcome, Phase};
        let panel = WorkflowPanel {
            finished: (0..12)
                .map(|index| FinishedRun {
                    id: OperationId::new(),
                    outcome: Outcome::Stopped,
                    goal: format!("attempt {index}"),
                    phase: Phase::Revising,
                    issue: None,
                    pr_url: None,
                })
                .collect(),
            ..WorkflowPanel::default()
        };
        // 4 header rows + 10 history rows + 3 composer rows.
        let height = 17;
        let history_rows = |panel: &WorkflowPanel| {
            render(height, 100, panel)[4..14]
                .iter()
                .map(|row| strip(row).trim_end().to_owned())
                .collect::<Vec<_>>()
        };
        assert!(history_rows(&panel).iter().all(|row| !row.is_empty()));

        // Bounding the offset by the row count alone leaves every offset past
        // `rows - height` drawing a part-empty window; one page back would show
        // 7 rows over 3 blank lines. The window has to stay full.
        let mut scrolled = panel.clone();
        for _ in 0..5 {
            scrolled.scroll_history(true);
            let rows = history_rows(&scrolled);
            assert!(
                rows.iter().all(|row| !row.is_empty()),
                "offset {} drew a part-empty window: {rows:?}",
                scrolled.history_offset
            );
        }
        // The oldest row is reachable, and paging back further stays there.
        assert!(history_rows(&scrolled)[0].contains("attempt 0"));
    }

    #[test]
    fn history_rows_carry_their_time_and_mark_what_moved_the_run() {
        use usagi_core::domain::agent_message::MessageKind;
        use usagi_core::domain::id::{OperationId, SessionId};
        use usagi_core::domain::workflow::WorkflowHistoryEntry;

        let mut run = crate::usecase::application::workflow::fixture_run(SessionId::new());
        run.history.push(WorkflowHistoryEntry {
            id: OperationId::new(),
            actor: "codex".into(),
            body: "Plan ready".into(),
            at: chrono::DateTime::from_timestamp(1_700_000_000, 0),
            kind: MessageKind::Message,
            advanced: false,
        });
        run.history.push(WorkflowHistoryEntry {
            id: OperationId::new(),
            actor: "claude".into(),
            body: "Approved".into(),
            at: chrono::DateTime::from_timestamp(1_700_003_600, 0),
            kind: MessageKind::Approved,
            advanced: true,
        });
        // A record from before the history carried time keeps its column width.
        run.history.push(WorkflowHistoryEntry {
            id: OperationId::new(),
            actor: "claude".into(),
            body: "Older entry".into(),
            at: None,
            kind: MessageKind::Message,
            advanced: false,
        });
        let panel = WorkflowPanel {
            run: Some(run),
            ..WorkflowPanel::default()
        };
        let rows = render(24, 120, &panel)
            .into_iter()
            .map(|row| strip(&row))
            .collect::<Vec<_>>();
        let row = |needle: &str| {
            rows.iter()
                .find(|row| row.contains(needle))
                .expect("the history row is drawn")
                .clone()
        };
        // The clock is rendered in the reader's own zone, so assert its shape
        // rather than an hour that depends on where the test runs.
        let stamp = |row: &str| {
            row.split_whitespace()
                .find(|word| {
                    word.len() == 5
                        && word.as_bytes()[2] == b':'
                        && word.bytes().filter(u8::is_ascii_digit).count() == 4
                })
                .map(str::to_owned)
        };
        assert!(stamp(&row("Plan ready")).is_some());
        assert!(stamp(&row("Approved")).is_some());
        // The message that moved the run is marked; the ones that did not are not.
        assert!(row("Approved").trim_start().starts_with("|>"));
        assert!(!row("Plan ready").contains("|>"));
        // A timeless record still lines the column up.
        assert!(row("Older entry").contains("--:--"));
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
