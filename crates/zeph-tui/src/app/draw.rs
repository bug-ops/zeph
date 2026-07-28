// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::layout::Rect;

use crate::layout::AppLayout;
use crate::widgets;
use crate::widgets::wave::EqualizerWidget;

use super::{App, EQ_PANEL_H, Panel};

impl App {
    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let collapsed = self.effective_collapsed();
        let mut layout = AppLayout::compute(
            frame.area(),
            self.show_side_panels,
            self.desired_input_height(),
            self.panel_demands(),
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
            if should_carve_equalizer(self.show_equalizer, wave_active, layout.subagents.height) {
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

        if let Some(state) = &self.mention_picker {
            widgets::mention_picker::render(self, state, frame, layout.input, &self.theme);
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

        let mut spans = vec![
            Span::styled("≈ ", theme.user_message),
            Span::styled("zeph", brand_style),
            Span::styled(meta, meta_style),
        ];

        // Persistent resume banner (spec-068 §13.5): appended to the same single-row header
        // line rather than a dedicated row, keeping `AppLayout`'s header height at 1 (OQ-I —
        // placement is an implementation choice, not a spec constraint). Stays visible after
        // the first prompt, unlike the transient status/spinner line.
        if let Some(banner) = &self.resume_banner {
            spans.push(Span::styled(
                format!("   {banner}"),
                theme.system_message.add_modifier(Modifier::ITALIC),
            ));
        }

        let line = Line::from(spans);

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
            // The badge row's height must track `compaction_badge::desired_height` exactly —
            // a fixed Length(1) here would eat a row `panel_demands()` never budgeted for
            // when no compaction has occurred, shifting `resources::render`'s own content
            // down by one and desyncing sizing from rendering (#6675).
            let badge_h = widgets::compaction_badge::desired_height(&self.metrics);
            let splits = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Length(badge_h),
                    Constraint::Min(0),
                ])
                .split(resources_area);
            widgets::context_gauge::render(&self.metrics, frame, splits[0], &self.theme);
            widgets::compaction_badge::render(&self.metrics, frame, splits[1], &self.theme);
            widgets::resources::render(&self.metrics, frame, splits[2], &self.theme);
        }

        let tick = self.throbber_state.index().cast_unsigned();
        let ascii = self.is_ascii_only();

        if effective[3] {
            self.render_collapsed_summary(
                frame,
                layout.subagents,
                "agents",
                focused_panel == Panel::SubAgents,
            );
        } else {
            self.render_subagents_slot(frame, layout.subagents, tick, ascii);
        }
    }

    /// Render the `SubAgents` slot's base layer, chosen by `App::subagent_slot_mode`, then
    /// layer any active overlay (Fleet/Durable/Settings/Tasks) on top of it. The mode
    /// selection is computed once and shared with sizing (`App::panel_demands`) so the two
    /// decisions can never disagree (#6675).
    fn render_subagents_slot(
        &mut self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        tick: u8,
        ascii: bool,
    ) {
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Clear, Paragraph};

        match self.subagent_slot_mode() {
            widgets::subagents::SubAgentSlotMode::Interactive => {
                widgets::subagents::render_interactive(
                    &self.metrics,
                    &mut self.subagent_sidebar,
                    frame,
                    area,
                    tick,
                    &self.theme,
                    ascii,
                );
            }
            widgets::subagents::SubAgentSlotMode::PlanView => {
                widgets::plan_view::render(&self.metrics, frame, area, tick, ascii, &self.theme);
            }
            widgets::subagents::SubAgentSlotMode::Security => {
                widgets::security::render(&self.metrics, frame, area, &self.theme);
            }
            widgets::subagents::SubAgentSlotMode::List => {
                widgets::subagents::render(&self.metrics, frame, area, &self.theme);
            }
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

/// Whether the equalizer slot should be carved from the bottom of the granted subagents
/// rect this frame.
///
/// Matches `App::panel_demands()`'s own accounting: when the equalizer is active, the
/// subagents demand already includes `+ EQ_PANEL_H` on top of its content rows, so a
/// fully-granted slot has height `>= content_rows + EQ_PANEL_H` (`content_rows >= 1` for
/// every base-layer mode). Requiring only `granted_height > EQ_PANEL_H` — the same
/// floor-of-1 guarantee `fit_panel_heights` upholds elsewhere — carves the slot exactly when
/// it was budgeted for (#6675 C1: the old `> EQ_PANEL_H + 2` margin predated content-driven
/// sizing and silently ate the equalizer whenever the granted height matched a small content
/// demand exactly, e.g. an empty sub-agent list).
fn should_carve_equalizer(show_equalizer: bool, wave_active: bool, granted_height: u16) -> bool {
    show_equalizer && wave_active && granted_height > EQ_PANEL_H
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

    use super::{App, EQ_PANEL_H, should_carve_equalizer};

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
                app.render_subagents_slot(frame, area, 0, false);
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

    // ── should_carve_equalizer / draw() regression (#6675 C1) ───────────────────

    #[test]
    fn should_carve_equalizer_true_when_granted_exactly_matches_budget() {
        // An empty sub-agent list's List-mode content is 2 rows; with the equalizer active
        // its demand is 2 + EQ_PANEL_H, and a fully-granted slot has exactly that height.
        // The carve must still happen here — this used to require `> EQ_PANEL_H + 2`, which
        // silently dropped the equalizer whenever granted height matched a small content
        // demand exactly.
        assert!(should_carve_equalizer(true, true, 2 + EQ_PANEL_H));
    }

    #[test]
    fn should_carve_equalizer_false_when_no_room_for_any_content() {
        assert!(!should_carve_equalizer(true, true, EQ_PANEL_H));
    }

    #[test]
    fn should_carve_equalizer_false_when_hidden_or_idle() {
        assert!(!should_carve_equalizer(false, true, 20));
        assert!(!should_carve_equalizer(true, false, 20));
    }

    #[test]
    fn draw_side_panel_rects_tile_the_column_without_overlap_under_varying_content() {
        // #6675 tester gap 4: with genuinely different content sizes per panel (so
        // `panel_demands()` produces different Rows(n) for each slot, unlike the old
        // uniform Fill(1) split), the four granted rects must still stack contiguously
        // inside `side_panel` — no gaps, no overlap, no rect exceeding the terminal.
        let mut app = make_app();
        app.metrics.active_skills = vec!["one".into(), "two".into()];
        app.metrics.total_skills = 2;
        app.metrics.sqlite_message_count = 10;
        app.metrics.provider_name = "claude".into();
        app.metrics.model_name = "opus-4".into();
        app.metrics.total_tokens = 1000;

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let layout = app.last_layout.expect("draw() must populate last_layout");

        let panels = [
            layout.skills,
            layout.memory,
            layout.resources,
            layout.subagents,
        ];
        for rect in panels {
            assert!(
                rect.y + rect.height <= layout.side_panel.y + layout.side_panel.height,
                "panel rect {rect:?} must not exceed the side_panel column {:?}",
                layout.side_panel
            );
            assert_eq!(
                rect.x, layout.side_panel.x,
                "panel rect must align with the side_panel column's left edge"
            );
            assert_eq!(rect.width, layout.side_panel.width);
        }
        // Contiguous, non-overlapping stacking: each slot starts exactly where the
        // previous one ends.
        assert_eq!(layout.skills.y, layout.side_panel.y);
        assert_eq!(layout.memory.y, layout.skills.y + layout.skills.height);
        assert_eq!(layout.resources.y, layout.memory.y + layout.memory.height);
        assert_eq!(
            layout.subagents.y,
            layout.resources.y + layout.resources.height
        );
    }

    #[test]
    fn draw_carves_equalizer_slot_for_empty_subagents_list_while_busy() {
        // Full regression for #6675 C1: with zero sub-agents and the agent busy, the
        // equalizer used to never render because the old threshold required more headroom
        // than an empty list's content-driven demand actually grants.
        let mut app = make_app();
        app.show_equalizer = true;
        app.sessions.current_mut().status_label = Some("thinking...".to_owned());
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let layout = app.last_layout.expect("draw() must populate last_layout");
        assert!(
            layout.subagents.height < 2 + EQ_PANEL_H,
            "equalizer must be carved out of the subagents slot for an empty, busy session, \
             got subagents height={}",
            layout.subagents.height
        );
    }
}
