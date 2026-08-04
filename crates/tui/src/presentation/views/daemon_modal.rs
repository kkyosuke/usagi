//! Overview の `daemon` command が開く読み取り専用 status modal。
//!
//! daemon metrics、診断 health、daemon-authoritative session projection を一つの
//! surface にまとめる。値は表示専用であり、launch admission や ownership の判断には使わない。

use usagi_core::usecase::client::DaemonMetrics;
use usagi_core::usecase::daemon_health::{DaemonHealth, HealthReason};
use usagi_core::usecase::session_state::SessionStateCounts;

use crate::presentation::theme::{Role, Style};
use crate::presentation::widgets::modal;

const INNER_WIDTH: usize = 60;
const BODY_HEIGHT: usize = 14;
const MEBIBYTE: u64 = 1_048_576;

/// Home frame の上へ daemon status を合成する。
#[must_use]
pub fn render_over(
    raw_height: usize,
    raw_width: usize,
    base: &[String],
    metrics: Option<&DaemonMetrics>,
    health: DaemonHealth,
    sessions: SessionStateCounts,
    session_total: usize,
) -> Vec<String> {
    modal::render_over(
        raw_height,
        raw_width,
        base,
        "Daemon",
        INNER_WIDTH,
        &body(metrics, health, sessions, session_total),
    )
}

fn body(
    metrics: Option<&DaemonMetrics>,
    health: DaemonHealth,
    sessions: SessionStateCounts,
    session_total: usize,
) -> Vec<String> {
    let mut lines = vec![modal::heading("Status")];
    lines.push(status_line(metrics, health));
    lines.push(metric_line(metrics));
    lines.push(session_line(sessions, session_total));
    lines.push(String::new());
    lines.push(modal::caption("Agent capacity"));
    lines.push(agent_capacity_line(metrics));
    lines.push(modal::content_line(
        "16/16 means Agent runtimes, not managed sessions.",
        INNER_WIDTH,
    ));
    lines.push(modal::content_line(
        "Exit a live Agent with Ctrl-D to release its slot.",
        INNER_WIDTH,
    ));
    lines.push(modal::content_line(
        "Manage worktrees with: session remove -s",
        INNER_WIDTH,
    ));
    lines.push(String::new());
    lines.push(modal::footer("Esc: close"));
    modal::fixed_body(lines, BODY_HEIGHT)
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
    }
}

#[cfg(test)]
mod tests {
    use super::render_over;
    use crate::presentation::widgets::display_width;
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
    fn saturated_daemon_explains_the_slots_and_session_cleanup_boundary() {
        let base = vec!["background".to_owned(); 24];
        let frame = render_over(
            24,
            100,
            &base,
            Some(&metrics(16)),
            DaemonHealth::Ok,
            SessionStateCounts {
                running: 3,
                waiting: 1,
                failed: 2,
            },
            8,
        )
        .join("\n");
        let plain = strip(&frame);
        assert!(plain.contains("Daemon"));
        assert!(plain.contains("16/16  saturated"));
        assert!(plain.contains("Agent runtimes, not managed sessions"));
        assert!(plain.contains("Sessions 8   running 3   waiting 1   failed 2"));
        assert!(plain.contains("session remove -s"));
    }

    #[test]
    fn unavailable_and_unhealthy_observations_are_explicit_and_tiny_safe() {
        let base = vec!["background".to_owned(); 4];
        let unavailable = render_over(
            24,
            80,
            &vec!["background".to_owned(); 24],
            None,
            DaemonHealth::Ok,
            SessionStateCounts::default(),
            0,
        )
        .join("\n");
        assert!(unavailable.contains("waiting for daemon observation"));
        assert!(unavailable.contains("unreported"));

        let tiny = render_over(
            4,
            5,
            &base,
            Some(&metrics(1)),
            DaemonHealth::Danger(HealthReason::DaemonUnresponsive),
            SessionStateCounts::default(),
            0,
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
        ] {
            let frame = strip(
                &render_over(
                    24,
                    100,
                    &base,
                    Some(&metrics(1)),
                    DaemonHealth::Warning(reason),
                    SessionStateCounts::default(),
                    0,
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
                Some(&metrics(12)),
                DaemonHealth::Ok,
                SessionStateCounts::default(),
                0,
            )
            .join("\n"),
        );
        assert!(busy.contains("12/16"));
        assert!(!busy.contains("saturated"));
    }
}
