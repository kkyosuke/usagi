//! Overview の `daemon` command が開く status / lifecycle control modal。
//!
//! daemon metrics、診断 health、daemon-authoritative session projection を一つの
//! surface にまとめる。診断値は表示専用であり、lifecycle 操作は controller の typed
//! effect だけを通る。

use usagi_core::domain::agent::AgentRuntimeInventoryState;
use usagi_core::usecase::client::DaemonMetrics;
use usagi_core::usecase::daemon_health::{DaemonHealth, HealthReason};
use usagi_core::usecase::session_state::SessionStateCounts;

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::{self, modal};
use crate::usecase::application::controller::{DaemonAction, DaemonControlState};

const INNER_WIDTH: usize = 60;
const MAX_BODY_HEIGHT: usize = 30;
const FIXED_RUNTIME_BODY_ROWS: usize = 15;
const MEBIBYTE: u64 = 1_048_576;

/// Presentation-safe row derived from the daemon-authoritative Agent inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentRuntimeRow {
    pub(crate) scope: String,
    pub(crate) runtime_id: String,
    pub(crate) state: AgentRuntimeInventoryState,
}

#[derive(Clone, Copy)]
pub(crate) struct DaemonProjection<'a> {
    pub(crate) metrics: Option<&'a DaemonMetrics>,
    pub(crate) health: DaemonHealth,
    pub(crate) sessions: SessionStateCounts,
    pub(crate) session_total: usize,
    pub(crate) runtimes: Option<&'a [AgentRuntimeRow]>,
    pub(crate) control: &'a DaemonControlState,
}

/// Home frame の上へ daemon status を合成する。
#[must_use]
pub(crate) fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    projection: DaemonProjection<'_>,
) -> Vec<String> {
    let (height, _) = widgets::normalize_size(raw_height, raw_width);
    // Four modal rows belong to border + internal vertical padding. Keep one
    // Home row visible above and below on short terminals as the shared modal
    // overlay contract requires.
    let body_height = height.saturating_sub(6).min(MAX_BODY_HEIGHT);
    modal::render_over(
        raw_height,
        raw_width,
        base,
        "Daemon",
        INNER_WIDTH,
        &body(
            projection.metrics,
            projection.health,
            projection.sessions,
            projection.session_total,
            projection.runtimes,
            projection.control,
            body_height,
        ),
    )
}

fn body(
    metrics: Option<&DaemonMetrics>,
    health: DaemonHealth,
    sessions: SessionStateCounts,
    session_total: usize,
    runtimes: Option<&[AgentRuntimeRow]>,
    control: &DaemonControlState,
    body_height: usize,
) -> Vec<String> {
    let mut lines = vec![modal::heading("Status")];
    lines.push(status_line(metrics, health));
    lines.push(metric_line(metrics));
    lines.push(session_line(sessions, session_total));
    lines.push(String::new());
    lines.push(modal::caption("Lifecycle actions (non-force)"));
    lines.push(action_line(control));
    lines.push(action_result_line(control));
    lines.push(String::new());
    lines.push(modal::caption("Agent capacity"));
    lines.push(agent_capacity_line(metrics));
    lines.push(String::new());
    lines.push(modal::caption("Agent runtimes"));
    lines.extend(runtime_lines(
        runtimes,
        body_height.saturating_sub(FIXED_RUNTIME_BODY_ROWS),
    ));
    lines.push(String::new());
    lines.push(modal::footer("↑/↓: select  Enter: run  Esc: close"));
    modal::fixed_body(lines, body_height)
}

fn action_line(control: &DaemonControlState) -> String {
    let actions = DaemonAction::ALL.map(|action| {
        let label = action.label();
        if action == control.selected() {
            Role::Accent
                .style()
                .bold()
                .paint(&format!("{} {label}", modal::selection_marker(true)))
        } else {
            Style::new().dim().paint(&format!("  {label}"))
        }
    });
    modal::content_line(&actions.join("   "), INNER_WIDTH)
}

fn action_result_line(control: &DaemonControlState) -> String {
    if let Some(action) = control.pending() {
        return modal::content_line(
            &Role::Accent
                .style()
                .paint(&format!("… {} in progress", action.label())),
            INNER_WIDTH,
        );
    }
    match control.result() {
        Some(Ok(notice)) => {
            modal::content_line(&Role::Success.style().paint(&notice.message), INNER_WIDTH)
        }
        Some(Err(error)) => modal::error_line(error.message.as_str(), INNER_WIDTH),
        None => modal::content_line(
            &Style::new().dim().paint("force actions remain CLI-only"),
            INNER_WIDTH,
        ),
    }
}

fn runtime_lines(runtimes: Option<&[AgentRuntimeRow]>, capacity: usize) -> Vec<String> {
    if capacity == 0 {
        return Vec::new();
    }
    let Some(runtimes) = runtimes else {
        return vec![modal::empty_notice("waiting for Agent inventory")];
    };
    if runtimes.is_empty() {
        return vec![modal::empty_notice("(none)")];
    }

    let visible = if runtimes.len() > capacity {
        capacity.saturating_sub(1)
    } else {
        runtimes.len()
    };
    let mut lines = runtimes
        .iter()
        .take(visible)
        .map(runtime_line)
        .collect::<Vec<_>>();
    if visible < runtimes.len() {
        lines.push(modal::scroll_below(runtimes.len() - visible));
    }
    lines
}

fn runtime_line(runtime: &AgentRuntimeRow) -> String {
    let state = match runtime.state {
        AgentRuntimeInventoryState::Reserved => "reserved",
        AgentRuntimeInventoryState::Live => "live",
        AgentRuntimeInventoryState::Sleeping => "sleeping",
        AgentRuntimeInventoryState::Interrupted => "interrupted",
        AgentRuntimeInventoryState::Exited => "exited",
        AgentRuntimeInventoryState::Reclaimed => "reclaimed",
        AgentRuntimeInventoryState::Unavailable => "unavailable",
    };
    modal::content_line(
        &format!("{}  {state}  #{}", runtime.scope, runtime.runtime_id),
        INNER_WIDTH,
    )
}

fn status_line(metrics: Option<&DaemonMetrics>, health: DaemonHealth) -> String {
    match health {
        DaemonHealth::Ok if metrics.is_some() => {
            modal::content_line(&Role::Success.style().paint("● healthy"), INNER_WIDTH)
        }
        DaemonHealth::Ok => modal::content_line(
            &Style::new().dim().paint("○ waiting for daemon observation"),
            INNER_WIDTH,
        ),
        DaemonHealth::Warning(reason) => modal::content_line(
            &Role::Warning
                .style()
                .paint(&format!("⚠ {}", health_reason(reason))),
            INNER_WIDTH,
        ),
        DaemonHealth::Danger(reason) => modal::content_line(
            &Role::Danger
                .style()
                .paint(&format!("⚠ {}", health_reason(reason))),
            INNER_WIDTH,
        ),
    }
}

fn metric_line(metrics: Option<&DaemonMetrics>) -> String {
    let text = metrics.map_or_else(
        || "CPU —   memory —   clients —".to_owned(),
        |metrics| {
            format!(
                "CPU {}%   memory {}MB   clients {}",
                metrics.cpu_percent_hundredths / 100,
                metrics.resident_memory_bytes / MEBIBYTE,
                metrics.active_subscribers
            )
        },
    );
    modal::content_line(&Style::new().dim().paint(&text), INNER_WIDTH)
}

fn session_line(sessions: SessionStateCounts, total: usize) -> String {
    modal::content_line(
        &format!(
            "Sessions {total}   running {}   waiting {}   failed {}",
            sessions.running, sessions.waiting, sessions.failed
        ),
        INNER_WIDTH,
    )
}

fn agent_capacity_line(metrics: Option<&DaemonMetrics>) -> String {
    let Some(concurrency) = metrics.and_then(|metrics| metrics.agent_concurrency) else {
        return modal::content_line(&Style::new().dim().paint("— unreported"), INNER_WIDTH);
    };
    let text = format!("{}/{}", concurrency.in_use, concurrency.limit);
    let styled = if concurrency.is_saturated() {
        Role::Danger.style().paint(&format!("{text}  saturated"))
    } else if concurrency.reaches_fraction(3, 4) {
        Role::Warning.style().paint(&text)
    } else {
        Style::new().dim().paint(&text)
    };
    modal::content_line(&styled, INNER_WIDTH)
}

const fn health_reason(reason: HealthReason) -> &'static str {
    match reason {
        HealthReason::DaemonUnresponsive => "daemon unresponsive",
        HealthReason::MetricsStalled => "metrics stalled",
        HealthReason::TerminalOutputDropped => "terminal output dropping",
        HealthReason::TerminalBackpressure => "terminal backpressure",
        HealthReason::PrScanIncomplete => "PR scan incomplete",
        HealthReason::MetricsUpdatesDropped => "metrics updates dropping",
        HealthReason::BackgroundWorkerStopped => "background worker stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentRuntimeRow, DaemonProjection, action_result_line, render_over, runtime_lines,
    };
    use crate::presentation::widgets::display_width;
    use crate::usecase::application::controller::{
        AppEvent, AppKey, AppState, BackendEvent, DaemonAction, DaemonControlState, Effect, Notice,
        SafeError, SafeMessage, update,
    };
    use usagi_core::domain::agent::AgentRuntimeInventoryState;
    use usagi_core::domain::id::WorkspaceId;
    use usagi_core::usecase::client::{AgentConcurrency, DaemonMetrics};
    use usagi_core::usecase::daemon_health::{DaemonHealth, HealthReason};
    use usagi_core::usecase::session_state::SessionStateCounts;

    fn metrics(in_use: u32) -> DaemonMetrics {
        DaemonMetrics {
            schema_version: 3,
            sampled_at_ms: 1,
            cpu_percent_hundredths: 250,
            resident_memory_bytes: 64 * 1_048_576,
            active_subscribers: 2,
            dropped_updates: 0,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            agent_concurrency: Some(AgentConcurrency { in_use, limit: 16 }),
            failed_background_workers: 0,
        }
    }

    fn projection<'a>(
        metrics: Option<&'a DaemonMetrics>,
        health: DaemonHealth,
        sessions: SessionStateCounts,
        session_total: usize,
        runtimes: Option<&'a [AgentRuntimeRow]>,
        control: &'a DaemonControlState,
    ) -> DaemonProjection<'a> {
        DaemonProjection {
            metrics,
            health,
            sessions,
            session_total,
            runtimes,
            control,
        }
    }

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

    #[test]
    fn lifecycle_feedback_renders_pending_success_and_safe_failure() {
        let workspace = WorkspaceId::new();
        let mut state = AppState::home(workspace, Vec::new());
        let _ = update(&mut state, AppEvent::Key(AppKey::OpenOverview));
        let _ = update(
            &mut state,
            AppEvent::Key(AppKey::SubmitOverview("daemon".to_owned())),
        );
        let effects = update(&mut state, AppEvent::Key(AppKey::Char('r')));
        let [Effect::DaemonControl { action, token, .. }] = effects.as_slice() else {
            panic!("daemon restart did not produce its typed effect");
        };
        assert!(strip(&action_result_line(state.daemon_control())).contains("in progress"));

        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::DaemonControlFinished {
                workspace,
                action: *action,
                token: *token,
                result: Ok(Notice::new("restart complete")),
            }),
        );
        assert!(strip(&action_result_line(state.daemon_control())).contains("restart complete"));

        let effects = update(&mut state, AppEvent::Key(AppKey::Char('x')));
        let [Effect::DaemonControl { token, .. }] = effects.as_slice() else {
            panic!("daemon stop did not produce its typed effect");
        };
        let _ = update(
            &mut state,
            AppEvent::Backend(BackendEvent::DaemonControlFinished {
                workspace,
                action: DaemonAction::Stop,
                token: *token,
                result: Err(SafeError {
                    message: SafeMessage::new("stop refused"),
                    error_id: "daemon-stop-refused".to_owned(),
                }),
            }),
        );
        assert!(strip(&action_result_line(state.daemon_control())).contains("stop refused"));
    }

    #[test]
    fn saturated_daemon_lists_authoritative_agent_runtimes() {
        let base = vec!["background".to_owned(); 24];
        let runtimes = vec![
            AgentRuntimeRow {
                scope: "root".to_owned(),
                runtime_id: "12345678".to_owned(),
                state: AgentRuntimeInventoryState::Live,
            },
            AgentRuntimeRow {
                scope: "review-fix".to_owned(),
                runtime_id: "abcdef01".to_owned(),
                state: AgentRuntimeInventoryState::Interrupted,
            },
        ];
        let frame = render_over(
            24,
            100,
            &base,
            projection(
                Some(&metrics(16)),
                DaemonHealth::Ok,
                SessionStateCounts {
                    running: 3,
                    waiting: 1,
                    failed: 2,
                },
                8,
                Some(&runtimes),
                &DaemonControlState::default(),
            ),
        )
        .join("\n");
        let plain = strip(&frame);
        assert!(plain.contains("Daemon"));
        assert!(plain.contains("Lifecycle actions (non-force)"));
        assert!(plain.contains("Start"));
        assert!(plain.contains("Restart"));
        assert!(plain.contains("Stop"));
        assert!(plain.contains("16/16  saturated"));
        assert!(plain.contains("root  live  #12345678"));
        assert!(plain.contains("review-fix  interrupted  #abcdef01"));
        assert!(!plain.contains("means Agent runtimes"));
        assert!(plain.contains("Sessions 8   running 3   waiting 1   failed 2"));
    }

    #[test]
    fn unavailable_and_unhealthy_observations_are_explicit_and_tiny_safe() {
        let base = vec!["background".to_owned(); 4];
        let unavailable = render_over(
            24,
            80,
            &vec!["background".to_owned(); 24],
            projection(
                None,
                DaemonHealth::Ok,
                SessionStateCounts::default(),
                0,
                None,
                &DaemonControlState::default(),
            ),
        )
        .join("\n");
        assert!(unavailable.contains("waiting for daemon observation"));
        assert!(unavailable.contains("unreported"));
        assert!(unavailable.contains("waiting for Agent inventory"));

        let tiny = render_over(
            4,
            5,
            &base,
            projection(
                Some(&metrics(1)),
                DaemonHealth::Danger(HealthReason::DaemonUnresponsive),
                SessionStateCounts::default(),
                0,
                None,
                &DaemonControlState::default(),
            ),
        );
        assert_eq!(tiny.len(), 4);
        assert!(tiny.iter().all(|line| display_width(line) <= 5));
    }

    #[test]
    fn warning_reasons_and_non_saturated_capacity_levels_are_rendered() {
        let base = vec!["background".to_owned(); 24];
        for (reason, label) in [
            (HealthReason::MetricsStalled, "metrics stalled"),
            (
                HealthReason::TerminalOutputDropped,
                "terminal output dropping",
            ),
            (HealthReason::TerminalBackpressure, "terminal backpressure"),
            (HealthReason::PrScanIncomplete, "PR scan incomplete"),
            (
                HealthReason::MetricsUpdatesDropped,
                "metrics updates dropping",
            ),
            (
                HealthReason::BackgroundWorkerStopped,
                "background worker stopped",
            ),
        ] {
            let frame = strip(
                &render_over(
                    24,
                    100,
                    &base,
                    projection(
                        Some(&metrics(1)),
                        DaemonHealth::Warning(reason),
                        SessionStateCounts::default(),
                        0,
                        None,
                        &DaemonControlState::default(),
                    ),
                )
                .join("\n"),
            );
            assert!(frame.contains(label));
            assert!(frame.contains("1/16"));
        }

        let busy = strip(
            &render_over(
                24,
                100,
                &base,
                projection(
                    Some(&metrics(12)),
                    DaemonHealth::Ok,
                    SessionStateCounts::default(),
                    0,
                    Some(&[]),
                    &DaemonControlState::default(),
                ),
            )
            .join("\n"),
        );
        assert!(busy.contains("12/16"));
        assert!(!busy.contains("saturated"));
    }

    #[test]
    fn runtime_rows_cover_every_state_and_bound_the_visible_inventory() {
        let rows = [
            (AgentRuntimeInventoryState::Reserved, "reserved"),
            (AgentRuntimeInventoryState::Live, "live"),
            (AgentRuntimeInventoryState::Sleeping, "sleeping"),
            (AgentRuntimeInventoryState::Interrupted, "interrupted"),
            (AgentRuntimeInventoryState::Exited, "exited"),
            (AgentRuntimeInventoryState::Reclaimed, "reclaimed"),
            (AgentRuntimeInventoryState::Unavailable, "unavailable"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (state, _))| AgentRuntimeRow {
            scope: format!("scope-{index}"),
            runtime_id: format!("runtime-{index}"),
            state,
        })
        .collect::<Vec<_>>();

        let all = strip(&runtime_lines(Some(&rows), rows.len()).join("\n"));
        for (_, label) in [
            (AgentRuntimeInventoryState::Reserved, "reserved"),
            (AgentRuntimeInventoryState::Live, "live"),
            (AgentRuntimeInventoryState::Sleeping, "sleeping"),
            (AgentRuntimeInventoryState::Interrupted, "interrupted"),
            (AgentRuntimeInventoryState::Exited, "exited"),
            (AgentRuntimeInventoryState::Reclaimed, "reclaimed"),
            (AgentRuntimeInventoryState::Unavailable, "unavailable"),
        ] {
            assert!(all.contains(label));
        }

        let bounded = strip(&runtime_lines(Some(&rows), 3).join("\n"));
        assert!(bounded.contains("scope-0"));
        assert!(bounded.contains("scope-1"));
        assert!(bounded.contains("↓ 5 more"));
        assert!(!bounded.contains("scope-2"));
        assert!(runtime_lines(Some(&rows), 0).is_empty());
    }
}
