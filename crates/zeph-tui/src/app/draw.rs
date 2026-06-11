// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::layout::AppLayout;
use crate::widgets;

use super::{App, Panel};

impl App {
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let collapsed = self.effective_collapsed();
        let layout = AppLayout::compute(
            frame.area(),
            self.show_side_panels,
            self.desired_input_height(),
            collapsed,
        );

        self.draw_header(frame, layout.header);
        if self.sessions.current().show_splash {
            widgets::splash::render(frame, layout.chat, self.effective_color_mode());
        } else {
            let mut cache = std::mem::take(&mut self.sessions.current_mut().render_cache);
            let max_scroll = widgets::chat::render(self, frame, layout.chat, &mut cache);
            self.sessions.current_mut().render_cache = cache;
            self.sessions.current_mut().scroll_offset =
                self.sessions.current().scroll_offset.min(max_scroll);
        }
        self.draw_separator(frame, layout.separator);
        self.draw_side_panel(frame, &layout, collapsed);
        let spinner_idx = self.throbber_state().index().cast_unsigned();
        let busy = self.is_agent_busy();
        let activity_label = self.status_label().map(str::to_owned);
        let supervisor_label = self.supervisor_activity_label();
        let effective_label = activity_label.or(supervisor_label);
        let wave_state = self.wave_state();
        let wave_tick = self.wave_tick();
        let motion = self.motion();
        // Take the reuse buffer out of self before the shared &App borrow starts so that
        // build_full_busy_sep can write into it without conflicting with &self below.
        let mut wave_buf = std::mem::take(&mut self.wave_buf);
        widgets::input::render(
            self,
            frame,
            layout.input,
            busy,
            effective_label.as_deref(),
            spinner_idx,
            wave_state,
            wave_tick,
            motion,
            &mut wave_buf,
        );
        self.wave_buf = wave_buf;
        widgets::status::render(self, &self.metrics, frame, layout.status);

        if let Some(state) = &self.file_picker_state {
            widgets::file_picker::render(state, frame, layout.input, &self.theme);
        }

        if let Some(state) = &self.slash_autocomplete {
            widgets::slash_autocomplete::render(state, frame, layout.input, &self.theme);
        }

        if let Some(state) = &self.reverse_search {
            let history = self.sessions.current().input_history.clone();
            widgets::reverse_search::render(state, &history, frame, layout.input, &self.theme);
        }

        if let Some(state) = &self.confirm_state {
            widgets::confirm::render(&state.prompt, frame, frame.area(), &self.theme);
        }

        if let Some(state) = &self.elicitation_state {
            widgets::elicitation::render(&state.dialog, frame, frame.area(), &self.theme);
        }

        if let Some(palette) = &self.command_palette {
            widgets::command_palette::render(palette, frame, frame.area(), &self.theme);
        }

        if self.show_help {
            widgets::help::render(frame, frame.area(), &self.theme);
        }
    }

    pub(super) fn draw_header(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        use ratatui::style::Modifier;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;

        let theme = &self.theme;

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
            "  1M CTX"
        } else {
            ""
        };

        // Brand name rendered bold, metadata in muted style — no solid background.
        let brand_style = theme.panel_title.add_modifier(Modifier::BOLD);
        let meta_style = theme.system_message;

        let meta = format!(
            "  {provider}  {model}  v{}{}",
            env!("CARGO_PKG_VERSION"),
            ctx_badge,
        );

        let line = Line::from(vec![
            Span::styled("⬡ zeph", brand_style),
            Span::styled(meta, meta_style),
        ]);

        // Transparent background: no .style() wrapper that would paint the row.
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_separator(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        use ratatui::text::Line;
        use ratatui::widgets::Paragraph;

        if area.width == 0 || area.height == 0 {
            return;
        }
        // Fill each row of the separator column with the vertical bar glyph.
        let rows: Vec<Line<'_>> = (0..area.height)
            .map(|_| Line::from(ratatui::text::Span::styled("│", self.theme.panel_border)))
            .collect();
        frame.render_widget(Paragraph::new(rows), area);
    }

    fn draw_side_panel(
        &mut self,
        frame: &mut ratatui::Frame,
        layout: &AppLayout,
        effective: [bool; 4],
    ) {
        use ratatui::layout::{Constraint, Direction, Layout};

        let focused_panel = self.active_panel;

        if effective[0] {
            self.render_collapsed_summary(
                frame,
                layout.skills,
                "skills",
                focused_panel == super::Panel::Skills,
            );
        } else if focused_panel == super::Panel::Skills {
            self.render_section_header(frame, layout.skills, "skills");
            let inner = shrink_top(layout.skills, 1);
            widgets::skills::render(&self.metrics, frame, inner, &self.theme);
        } else {
            widgets::skills::render(&self.metrics, frame, layout.skills, &self.theme);
        }

        if effective[1] {
            self.render_collapsed_summary(
                frame,
                layout.memory,
                "memory",
                focused_panel == super::Panel::Memory,
            );
        } else if focused_panel == super::Panel::Memory {
            self.render_section_header(frame, layout.memory, "memory");
            let inner = shrink_top(layout.memory, 1);
            widgets::memory::render(&self.metrics, frame, inner, &self.theme);
        } else {
            widgets::memory::render(&self.metrics, frame, layout.memory, &self.theme);
        }

        if effective[2] {
            self.render_collapsed_summary(
                frame,
                layout.resources,
                "resources",
                focused_panel == super::Panel::Resources,
            );
        } else {
            let resources_area = if focused_panel == super::Panel::Resources {
                self.render_section_header(frame, layout.resources, "resources");
                shrink_top(layout.resources, 1)
            } else {
                layout.resources
            };
            let splits = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Min(0),
                ])
                .split(resources_area);
            widgets::context_gauge::render(&self.metrics, frame, splits[0]);
            widgets::compaction_badge::render(&self.metrics, frame, splits[1]);
            widgets::resources::render(&self.metrics, frame, splits[2], &self.theme);
        }

        let tick = self.throbber_state.index().cast_unsigned();
        let ascii = self.is_ascii_only();
        let has_graph = self.metrics.orchestration_graph.as_ref().is_some_and(|s| {
            // Use is_stale() to check if snapshot is too old to show (IC4).
            !s.is_stale()
        });
        let panel_focused = self.active_panel == Panel::SubAgents;

        if effective[3] {
            self.render_collapsed_summary(
                frame,
                layout.subagents,
                "agents",
                focused_panel == Panel::SubAgents,
            );
        } else {
            self.render_subagents_slot(
                frame,
                layout.subagents,
                tick,
                ascii,
                panel_focused,
                has_graph,
            );
        }
    }

    fn render_subagents_slot(
        &mut self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        tick: u8,
        ascii: bool,
        panel_focused: bool,
        has_graph: bool,
    ) {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;

        // When SubAgents panel is focused (`a` key), always show the interactive sidebar.
        // Otherwise: auto-show plan when graph active, security events, or subagents list.
        if panel_focused {
            widgets::subagents::render_interactive(
                &self.metrics,
                &mut self.subagent_sidebar,
                frame,
                area,
                tick,
                &self.theme,
                ascii,
            );
        } else if has_graph && !self.sessions.current().plan_view_active {
            widgets::plan_view::render(&self.metrics, frame, area, tick, ascii, &self.theme);
        } else if self.has_recent_security_events() {
            widgets::security::render(&self.metrics, frame, area, &self.theme);
        } else {
            widgets::subagents::render(&self.metrics, frame, area, &self.theme);
        }

        // Overlay fleet panel over the subagents slot when `f` key is active (#3884).
        if self.active_panel == Panel::Fleet {
            widgets::fleet::render(
                &self.fleet_snapshot,
                frame,
                area,
                &mut self.fleet_list_state,
                &self.theme,
            );
        }

        // Overlay durable panel over the subagents slot when `D` key is active (spec-064, #4949).
        if self.active_panel == Panel::Durable {
            widgets::durable::render(
                &self.durable_snapshot,
                frame,
                area,
                &mut self.durable_list_state,
                &self.theme,
            );
        }

        // Overlay task registry over the subagents slot when `/tasks` is toggled.
        if self.show_task_panel {
            if self.task_supervisor.is_some() {
                widgets::task_registry::render(
                    &self.cached_task_snapshots,
                    tick,
                    area,
                    frame,
                    &self.theme,
                    ascii,
                );
            } else {
                let theme = &self.theme;
                let header = Line::from(vec![
                    Span::styled("⬡ ", theme.highlight),
                    Span::styled("tasks  supervisor not available", theme.system_message),
                ]);
                frame.render_widget(Paragraph::new(header), area);
            }
        }
    }

    /// Render a single-row collapsed summary bar for the given panel label.
    ///
    /// When `focused` is true the brand glyph prefix and accent color replace the muted style.
    fn render_collapsed_summary(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        label: &str,
        focused: bool,
    ) {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;

        if area.height == 0 || area.width == 0 {
            return;
        }
        let line = if focused {
            Line::from(vec![
                Span::styled("⬡ ", self.theme.highlight),
                Span::styled(label, self.theme.highlight),
            ])
        } else {
            Line::from(vec![
                Span::styled("▸ ", self.theme.panel_border),
                Span::styled(label, self.theme.panel_title),
            ])
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Render a single-row focused section header (brand glyph + accent color).
    fn render_section_header(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        label: &str,
    ) {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;

        if area.height == 0 || area.width == 0 {
            return;
        }
        let line = Line::from(vec![
            Span::styled("⬡ ", self.theme.highlight),
            Span::styled(label, self.theme.highlight),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}

/// Return `area` with the top `n` rows removed.
fn shrink_top(area: ratatui::layout::Rect, n: u16) -> ratatui::layout::Rect {
    if n >= area.height {
        return ratatui::layout::Rect {
            x: area.x,
            y: area.y + area.height,
            width: area.width,
            height: 0,
        };
    }
    ratatui::layout::Rect {
        x: area.x,
        y: area.y + n,
        width: area.width,
        height: area.height - n,
    }
}
