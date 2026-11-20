// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context fill gauge for the TUI side panel.
//!
//! Renders a single-line inline bar `context  [████████░░]  Nk / Nk · N%`
//! using block characters in the `info` palette color (via `theme.code_inline`).
//! When `context_max_tokens == 0` (pre-init or unknown provider window), the label
//! renders `Nk / —` and the bar stays empty — no divide-by-zero.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

/// Render the context fill gauge into `area`.
///
/// Shows a single row: `context  [████████░░]  Nk / Nk · N%`.
/// Bar color: `theme.code_inline` fg (maps to palette `info` = `#6FDCD2`).
/// Hidden when `area` has zero height.
pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.height == 0 {
        return;
    }

    let max = metrics.context_max_tokens;
    let used = metrics.context_tokens;

    let (ratio, label) = if max == 0 {
        (0.0_f64, format!("{}k / —", used / 1000))
    } else {
        let clamped = used.min(max);
        #[expect(
            clippy::cast_precision_loss,
            reason = "r uses approximate floating-point values for ranking, display, or telemetry, where integer precision is not part of the contract"
        )]
        let r = clamped as f64 / max as f64;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "pct narrows a value whose domain-specific range is bounded at this conversion site; converts a value that is nonnegative by the domain invariant at this site"
        )]
        let pct = (r * 100.0) as u64;
        (r, format!("{}k / {}k · {pct}%", used / 1000, max / 1000))
    };

    let bar = build_bar(ratio, 10);
    let bar_style =
        Style::default().fg(theme.code_inline.fg.unwrap_or(ratatui::style::Color::Cyan));

    let line = Line::from(vec![
        Span::styled("context  ", theme.system_message),
        Span::styled(bar, bar_style),
        Span::styled(format!("  {label}"), theme.status_bar),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

/// Build a block-character bar string of the given `width` (number of cells).
fn build_bar(ratio: f64, width: usize) -> String {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "filled narrows a value whose domain-specific range is bounded at this conversion site; converts a value that is nonnegative by the domain invariant at this site"
    )]
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricsSnapshot;

    fn make_metrics(context_tokens: u64, context_max_tokens: u64) -> MetricsSnapshot {
        MetricsSnapshot {
            context_tokens,
            context_max_tokens,
            ..MetricsSnapshot::default()
        }
    }

    #[test]
    fn ratio_zero_when_max_is_zero() {
        let m = make_metrics(1000, 0);
        let max = m.context_max_tokens;
        let used = m.context_tokens;
        let ratio: f64 = if max == 0 {
            0.0
        } else {
            #[expect(
                clippy::cast_precision_loss,
                reason = "r uses approximate floating-point values for ranking, display, or telemetry, where integer precision is not part of the contract"
            )]
            let r = used.min(max) as f64 / max as f64;
            r
        };
        assert!(
            ratio.abs() < f64::EPSILON,
            "ratio must be exactly 0.0 when max is 0"
        );
    }

    #[test]
    fn ratio_clamped_when_used_exceeds_max() {
        let m = make_metrics(200_000, 128_000);
        let max = m.context_max_tokens;
        let used = m.context_tokens;
        #[expect(
            clippy::cast_precision_loss,
            reason = "ratio uses approximate floating-point values for ranking, display, or telemetry, where integer precision is not part of the contract"
        )]
        let ratio = used.min(max) as f64 / max as f64;
        assert!((ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn build_bar_empty_at_zero() {
        assert_eq!(build_bar(0.0, 10), "[░░░░░░░░░░]");
    }

    #[test]
    fn build_bar_full_at_one() {
        assert_eq!(build_bar(1.0, 10), "[██████████]");
    }

    #[test]
    fn build_bar_half() {
        assert_eq!(build_bar(0.5, 10), "[█████░░░░░]");
    }
}
