// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ephemeral toast overlay widget (#5104).
//!
//! Renders a stack of transient notifications anchored to the bottom of the
//! chat area (above the input line, below modal overlays).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::delights::{ToastKind, ToastQueue};
use crate::theme::Theme;

/// Render active toasts into the bottom of `chat_area`.
///
/// Toasts are stacked newest-at-bottom, max 3 visible. Each toast occupies
/// one row. The overlay lives entirely within `chat_area` so it never obscures
/// the input slot (which is a separate layout area below `chat_area`).
pub(crate) fn render(
    toasts: &ToastQueue,
    frame: &mut Frame,
    chat_area: Rect,
    theme: &Theme,
    now: u64,
) {
    let active: Vec<_> = toasts.active_items(now).collect();
    if active.is_empty() || chat_area.height == 0 || chat_area.width == 0 {
        return;
    }

    #[allow(clippy::cast_possible_truncation)]
    let n = active.len().min(3) as u16;
    let y_start = chat_area.y + chat_area.height.saturating_sub(n);

    for (i, toast) in active.iter().rev().take(3).enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let y = y_start + i as u16;
        if y >= chat_area.y + chat_area.height {
            break;
        }
        let toast_area = Rect {
            x: chat_area.x,
            y,
            width: chat_area.width,
            height: 1,
        };
        let style = toast_style(toast.kind, theme);
        let text = format!(" {} ", toast.text);
        let line = Line::from(Span::styled(text, style));
        frame.render_widget(Clear, toast_area);
        frame.render_widget(Paragraph::new(line), toast_area);
    }
}

fn toast_style(kind: ToastKind, theme: &Theme) -> Style {
    match kind {
        ToastKind::Info => theme.status_bar,
        ToastKind::Success => Style::default().fg(Color::Green),
        ToastKind::Warn => Style::default().fg(Color::Yellow),
    }
}
