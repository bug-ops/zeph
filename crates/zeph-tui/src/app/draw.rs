// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::layout::AppLayout;
use crate::theme::Theme;
use crate::widgets;

use super::{App, Panel};

impl App {
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let layout = AppLayout::compute(
            frame.area(),
            self.show_side_panels,
            self.desired_input_height(),
        );

        self.draw_header(frame, layout.header);
        if self.sessions.current().show_splash {
            widgets::splash::render(frame, layout.chat);
        } else {
            let mut cache = std::mem::take(&mut self.sessions.current_mut().render_cache);
            let max_scroll = widgets::chat::render(self, frame, layout.chat, &mut cache);
            self.sessions.current_mut().render_cache = cache;
            self.sessions.current_mut().scroll_offset =
                self.sessions.current().scroll_offset.min(max_scroll);
        }
        self.draw_side_panel(frame, &layout);
        let spinner_idx = self.throbber_state().index().cast_unsigned();
        let busy = self.is_agent_busy();
        let activity_label = self.status_label().map(str::to_owned);
        let supervisor_label = self.supervisor_activity_label();
        let effective_label = activity_label.or(supervisor_label);
        widgets::input::render(
            self,
            frame,
            layout.input,
            busy,
            effective_label.as_deref(),
            spinner_idx,
        );
        widgets::status::render(self, &self.metrics, frame, layout.status);

        if let Some(state) = &self.file_picker_state {
            widgets::file_picker::render(state, frame, layout.input);
        }

        if let Some(state) = &self.slash_autocomplete {
            widgets::slash_autocomplete::render(state, frame, layout.input);
        }

        if let Some(state) = &self.confirm_state {
            widgets::confirm::render(&state.prompt, frame, frame.area());
        }

        if let Some(state) = &self.elicitation_state {
            widgets::elicitation::render(&state.dialog, frame, frame.area());
        }

        if let Some(palette) = &self.command_palette {
            widgets::command_palette::render(palette, frame, frame.area());
        }

        if self.show_help {
            widgets::help::render(frame, frame.area());
        }
    }

    pub(super) fn draw_header(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;

        let theme = Theme::default();

        let provider = if self.metrics.provider_name.is_empty() {
            "---"
        } else {
            &self.metrics.provider_name
        };
        let model = if self.metrics.model_name.is_empty() {
            "---"
        } else {
            &self.metrics.model_name
        };

        let ctx_badge = if self.metrics.extended_context {
            " [1M CTX]"
        } else {
            ""
        };
        let text = format!(
            " Zeph v{} | Provider: {provider} | Model: {model}{ctx_badge}",
            env!("CARGO_PKG_VERSION")
        );

        let line = Line::from(Span::styled(text, theme.header));
        let paragraph = Paragraph::new(line).style(theme.header);
        frame.render_widget(paragraph, area);
    }

    fn draw_side_panel(&mut self, frame: &mut ratatui::Frame, layout: &AppLayout) {
        widgets::skills::render(&self.metrics, frame, layout.skills);
        widgets::memory::render(&self.metrics, frame, layout.memory);

        // Split the resources area into: context gauge (3 rows), compaction badge (3 rows),
        // and the remaining resources panel. Each gauge/badge needs at least its border rows.
        {
            use ratatui::layout::{Constraint, Direction, Layout};
            let context_split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(0),
                ])
                .split(layout.resources);
            widgets::context_gauge::render(&self.metrics, frame, context_split[0]);
            widgets::compaction_badge::render(&self.metrics, frame, context_split[1]);
            widgets::resources::render(&self.metrics, frame, context_split[2]);
        }

        let tick = self.throbber_state.index().cast_unsigned();
        let has_graph = self.metrics.orchestration_graph.as_ref().is_some_and(|s| {
            // Use is_stale() to check if snapshot is too old to show (IC4).
            !s.is_stale()
        });
        let panel_focused = self.active_panel == Panel::SubAgents;

        // When SubAgents panel is focused (`a` key), always show the interactive sidebar.
        // Otherwise: auto-show plan when graph active, security events, or subagents list.
        if panel_focused {
            widgets::subagents::render_interactive(
                &self.metrics,
                &mut self.subagent_sidebar,
                frame,
                layout.subagents,
                tick,
            );
        } else if has_graph && !self.sessions.current().plan_view_active {
            widgets::plan_view::render(&self.metrics, frame, layout.subagents, tick);
        } else if self.has_recent_security_events() {
            widgets::security::render(&self.metrics, frame, layout.subagents);
        } else {
            widgets::subagents::render(&self.metrics, frame, layout.subagents);
        }

        // Overlay task registry over the subagents slot when `/tasks` is toggled.
        if self.show_task_panel {
            if self.task_supervisor.is_some() {
                widgets::task_registry::render(
                    &self.cached_task_snapshots,
                    tick,
                    layout.subagents,
                    frame,
                );
            } else {
                use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
                let theme = Theme::default();
                let block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.panel_border)
                    .title(" Tasks ");
                let paragraph = Paragraph::new(" Task supervisor not available.")
                    .block(block)
                    .wrap(Wrap { trim: true });
                frame.render_widget(paragraph, layout.subagents);
            }
        }
    }
}
