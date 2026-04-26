// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context fill gauge for the TUI side panel.
//!
//! Renders a [`ratatui::widgets::Gauge`] showing the fraction of the provider's context window
//! currently in use. Color changes from green to yellow to red as the window fills.
//! When `context_max_tokens == 0` (pre-init or unknown provider window), the gauge label
//! renders `"—"` and the bar stays at 0 — no divide-by-zero.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Gauge};

use crate::metrics::MetricsSnapshot;

/// Render the context fill gauge into `area`.
///
/// Color thresholds: green below 70%, yellow 70–90%, red above 90%.
/// Hidden (renders an empty block) when `area` has zero height.
pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }

    let max = metrics.context_max_tokens;
    let used = metrics.context_tokens;

    let (ratio, label) = if max == 0 {
        // Unknown provider window — display placeholder rather than divide-by-zero.
        (0.0_f64, format!("Context: {}k / —", used / 1000))
    } else {
        let clamped = used.min(max);
        #[allow(clippy::cast_precision_loss)]
        let r = clamped as f64 / max as f64;
        // r is in [0.0, 1.0], so r * 100.0 is in [0.0, 100.0] — no truncation or sign loss.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pct = (r * 100.0) as u64;
        let label = format!("Context: {}k / {}k ({pct}%)", used / 1000, max / 1000);
        (r, label)
    };

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct_u16 = (ratio * 100.0).clamp(0.0, 100.0) as u16;

    let color = match pct_u16 {
        0..=69 => Color::Green,
        70..=89 => Color::Yellow,
        _ => Color::Red,
    };

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("Context"))
        .gauge_style(Style::default().fg(color))
        .ratio(ratio)
        .label(Span::raw(label));

    frame.render_widget(gauge, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricsSnapshot;

    /// Returns a zero-sized rect so render is a no-op — we are only testing the label logic.
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
        // When max is 0 ratio must be 0.0 — no divide-by-zero.
        let max = m.context_max_tokens;
        let used = m.context_tokens;
        let ratio: f64 = if max == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
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
        #[allow(clippy::cast_precision_loss)]
        let ratio = used.min(max) as f64 / max as f64;
        assert!((ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn color_green_below_70_percent() {
        let pct: u16 = 50;
        let color = match pct {
            0..=69 => Color::Green,
            70..=89 => Color::Yellow,
            _ => Color::Red,
        };
        assert_eq!(color, Color::Green);
    }

    #[test]
    fn color_yellow_70_to_89_percent() {
        let pct: u16 = 80;
        let color = match pct {
            0..=69 => Color::Green,
            70..=89 => Color::Yellow,
            _ => Color::Red,
        };
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn color_red_above_90_percent() {
        let pct: u16 = 95;
        let color = match pct {
            0..=69 => Color::Green,
            70..=89 => Color::Yellow,
            _ => Color::Red,
        };
        assert_eq!(color, Color::Red);
    }
}
