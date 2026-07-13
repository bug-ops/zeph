// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::layout::Rect;

use crate::layout::AppLayout;
use crate::widgets;
use crate::widgets::wave::EqualizerWidget;

use super::{App, Panel};

impl App {
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        // Height of the equalizer slot carved from the bottom of the subagents panel.
        const EQ_PANEL_H: u16 = 4;

        let collapsed = self.effective_collapsed();
        let mut layout = AppLayout::compute(
            frame.area(),
            self.show_side_panels,
            self.desired_input_height(),
            collapsed,
        );

        // Micro-delight state is advanced in tick_delights() (called on AppEvent::Tick).
        // draw() only reads the current state for rendering.
        let now = self.anim_tick();
        let cur_show_splash = self.sessions.current().show_splash;
        let shimmer_enabled =
            self.motion != zeph_config::Motion::Off && self.delights.splash_shimmer;

        self.draw_header(frame, layout.header);
        if cur_show_splash {
            let shimmer_phase = if shimmer_enabled {
                self.splash_shimmer.phase(now)
            } else {
                None
            };
            widgets::splash::render(
                frame,
                layout.chat,
                self.effective_color_mode(),
                shimmer_phase,
            );
        } else {
            let mut cache = std::mem::take(&mut self.sessions.current_mut().render_cache);
            let max_scroll = widgets::chat::render(self, frame, layout.chat, &mut cache);
            self.sessions.current_mut().render_cache = cache;
            self.sessions.current_mut().scroll_offset =
                self.sessions.current().scroll_offset.min(max_scroll);
        }
        self.draw_separator(frame, layout.separator);

        // Carve the equalizer slot from the bottom of the subagents area. The slot
        // appears while the agent is busy OR background/external requests are inflight
        // (so concurrent background work is visible), unless the user has hidden it.
        let wave_state = self.wave_state();
        let wave_tick = self.wave_tick();
        let wave_active = self.is_agent_busy() || self.background_inflight() > 0;
        let eq_area =
            if self.show_equalizer && wave_active && layout.subagents.height > EQ_PANEL_H + 2 {
                let sub_h = layout.subagents.height - EQ_PANEL_H;
                let eq = Rect {
                    y: layout.subagents.y + sub_h,
                    height: EQ_PANEL_H,
                    ..layout.subagents
                };
                layout.subagents = Rect {
                    height: sub_h,
                    ..layout.subagents
                };
                eq
            } else {
                Rect::default()
            };

        self.draw_side_panel(frame, &layout, collapsed);

        if eq_area.height > 0 {
            frame.render_widget(
                EqualizerWidget {
                    state: wave_state,
                    tick: wave_tick,
                    theme: &self.theme,
                    color_mode: self.effective_color_mode(),
                    ascii_only: self.is_ascii_only(),
                },
                eq_area,
            );
        }

        let spinner_idx = self.throbber_state().index().cast_unsigned();
        let busy = self.is_agent_busy();
        let motion = self.motion();
        widgets::input::render(self, frame, layout.input, busy, spinner_idx, motion);
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

        if let Some(state) = &self.transcript_search {
            widgets::transcript_search::render(state, frame, layout.input, &self.theme);
        }

        // Render toasts above the input, below modal overlays.
        if self.motion != zeph_config::Motion::Off && self.delights.toasts {
            widgets::toast::render(&self.toasts, frame, layout.chat, &self.theme, now);
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

        self.last_layout = Some(layout);
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
            Span::styled("≈ ", theme.user_message),
            Span::styled("zeph", brand_style),
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
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .split(resources_area);
            widgets::context_gauge::render(&self.metrics, frame, splits[0], &self.theme);
            widgets::compaction_badge::render(&self.metrics, frame, splits[1], &self.theme);
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
        use ratatui::widgets::{Clear, Paragraph};

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

        // Overlay the read-only settings view over the subagents slot when `S` is
        // active (issue #6024), mirroring the Fleet/Durable overlay precedent.
        if self.active_panel == Panel::Settings {
            widgets::settings::render(&self.metrics, &mut self.settings, frame, area, &self.theme);
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
                    Span::styled("≈ ", theme.highlight),
                    Span::styled("tasks  supervisor not available", theme.system_message),
                ]);
                frame.render_widget(Clear, area);
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
                Span::styled("≈ ", self.theme.highlight),
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
            Span::styled("≈ ", self.theme.highlight),
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    use super::App;

    fn make_app() -> App {
        let (user_tx, _) = mpsc::channel(1);
        let (_, agent_rx) = mpsc::channel(1);
        App::new(user_tx, agent_rx)
    }

    /// Fills the whole area with a sentinel glyph before calling `render_subagents_slot`, in
    /// the same frame — mirroring the real bug shape (#6061): the task-panel
    /// supervisor-unavailable fallback shares its `Rect` with Fleet/Durable/task-registry
    /// overlays but, before the fix, never called `Clear`, so stale glyphs from whatever
    /// rendered underneath survived in every cell the fallback `Paragraph` didn't touch.
    fn render_fallback_over_sentinel(app: &mut App) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                for y in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        frame.buffer_mut()[(x, y)].set_symbol("#");
                    }
                }
                app.render_subagents_slot(frame, area, 0, false, false, false);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn task_panel_fallback_clears_stale_glyphs_when_supervisor_unavailable() {
        let mut app = make_app();
        app.show_task_panel = true;
        assert!(app.task_supervisor.is_none());

        let buf = render_fallback_over_sentinel(&mut app);

        for cell in &buf.content {
            assert_ne!(
                cell.symbol(),
                "#",
                "stray sentinel glyph survived render — Clear is missing or not applied \
                 to the whole area"
            );
        }
    }
}
