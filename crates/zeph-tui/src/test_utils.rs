// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

use crate::event::{AppEvent, EventSource};

pub struct MockEventSource {
    events: VecDeque<AppEvent>,
}

impl MockEventSource {
    #[must_use]
    pub fn new(events: Vec<AppEvent>) -> Self {
        Self {
            events: events.into(),
        }
    }
}

impl EventSource for MockEventSource {
    fn next_event(&mut self) -> Option<AppEvent> {
        self.events.pop_front()
    }
}

/// Create a terminal backed by `TestBackend` for widget rendering tests.
///
/// # Panics
///
/// Panics if `ratatui` fails to initialize the test terminal backend.
#[must_use]
pub fn test_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    let backend = TestBackend::new(width, height);
    Terminal::new(backend).unwrap()
}

/// Render a widget tree into a string buffer for snapshot and text assertions.
///
/// # Panics
///
/// Panics if drawing to the test terminal fails.
pub fn render_to_string<F>(width: u16, height: u16, render_fn: F) -> String
where
    F: FnOnce(&mut ratatui::Frame, Rect),
{
    let mut terminal = test_terminal(width, height);
    terminal
        .draw(|frame| {
            let area = frame.area();
            render_fn(frame, area);
        })
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    buffer_to_string(&buf)
}

fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
    let mut output = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            output.push_str(cell.symbol());
        }
        output.push('\n');
    }
    output
}
