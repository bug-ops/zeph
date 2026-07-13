// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Read-only TUI settings view: browse configured LLM providers, MCP servers, and
//! sub-agent definitions from a running session (issue #6024).
//!
//! Data is read exclusively from [`MetricsSnapshot`] — a pure snapshot read with no
//! background operation, since `providers`/`agent_definitions`/`mcp_servers` are already
//! kept current on the metrics watch channel (see `MetricsSnapshot::providers` docs).
//! Write/edit is explicitly out of scope for v1 (NFR-005 of
//! `/specs/061-tui-settings-editor-parity/spec.md`).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use crate::layout::truncate_to_width;
use crate::metrics::{
    AgentDefSummary, McpServerConnectionStatus, McpServerStatus, MetricsSnapshot, ProviderSummary,
};
use crate::theme::Theme;

/// Which configuration class the settings view is currently browsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    /// Configured `[[llm.providers]]` entries.
    Providers,
    /// Configured MCP servers and their live connection status.
    Mcp,
    /// Configured sub-agent definitions (templates), not runtime instances.
    Agents,
}

impl SettingsTab {
    const ALL: [SettingsTab; 3] = [
        SettingsTab::Providers,
        SettingsTab::Mcp,
        SettingsTab::Agents,
    ];

    fn index(self) -> usize {
        match self {
            SettingsTab::Providers => 0,
            SettingsTab::Mcp => 1,
            SettingsTab::Agents => 2,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SettingsTab::Providers => "Providers",
            SettingsTab::Mcp => "MCP",
            SettingsTab::Agents => "Agents",
        }
    }

    /// Cycle to the next tab, wrapping around.
    #[must_use]
    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    /// Cycle to the previous tab, wrapping around.
    #[must_use]
    pub fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// View-layer state for the read-only settings panel: active tab and a per-tab
/// selection index (so switching tabs and back preserves each tab's cursor).
#[derive(Debug, Clone)]
pub struct SettingsViewState {
    /// Currently active tab.
    pub tab: SettingsTab,
    /// Selected row index per tab, indexed by [`SettingsTab::index`] (0=Providers,
    /// 1=Mcp, 2=Agents).
    selected: [usize; 3],
}

impl Default for SettingsViewState {
    fn default() -> Self {
        Self {
            tab: SettingsTab::Providers,
            selected: [0; 3],
        }
    }
}

impl SettingsViewState {
    /// Switch to the next tab (wraps).
    pub fn next_tab(&mut self) {
        self.tab = self.tab.next();
    }

    /// Switch to the previous tab (wraps).
    pub fn previous_tab(&mut self) {
        self.tab = self.tab.previous();
    }

    /// Move the active tab's selection down by one, clamped to `count - 1`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_tui::widgets::settings::SettingsViewState;
    ///
    /// let mut state = SettingsViewState::default();
    /// state.select_next(3);
    /// assert_eq!(state.selected_index(), 1);
    /// state.select_next(3);
    /// state.select_next(3);
    /// assert_eq!(state.selected_index(), 2, "clamped at count - 1");
    /// ```
    pub fn select_next(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let idx = self.tab.index();
        self.selected[idx] = (self.selected[idx] + 1).min(count - 1);
    }

    /// Move the active tab's selection up by one, clamped to `0`.
    pub fn select_previous(&mut self, count: usize) {
        let idx = self.tab.index();
        self.selected[idx] = self.selected[idx].saturating_sub(1);
        if count == 0 {
            self.selected[idx] = 0;
        }
    }

    /// Returns the selected row index for the currently active tab.
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected[self.tab.index()]
    }
}

/// Render the settings view: tab header, entry list, and a detail block for the
/// selected entry. Overlays the subagents slot, mirroring the Fleet/Durable/Tasks
/// precedent (`render_subagents_slot`).
pub fn render(
    metrics: &MetricsSnapshot,
    state: &mut SettingsViewState,
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);

    render_tab_header(state.tab, frame, chunks[0], theme);

    let body = chunks[1];
    match state.tab {
        SettingsTab::Providers => render_providers(&metrics.providers, state, frame, body, theme),
        SettingsTab::Mcp => render_mcp(&metrics.mcp_servers, state, frame, body, theme),
        SettingsTab::Agents => {
            render_agents(&metrics.agent_definitions, state, frame, body, theme);
        }
    }
}

fn render_tab_header(active: SettingsTab, frame: &mut Frame, area: Rect, theme: &Theme) {
    let mut spans = vec![Span::styled(
        " Settings  ",
        theme.panel_title.add_modifier(Modifier::BOLD),
    )];
    for tab in SettingsTab::ALL {
        let style = if tab == active {
            theme.highlight.add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!("[{}] ", tab.label()), style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Split the body area into a scrollable list (top) and a fixed detail block (bottom).
fn split_body(area: Rect) -> (Rect, Rect) {
    const DETAIL_HEIGHT: u16 = 5;
    if area.height <= DETAIL_HEIGHT {
        return (area, Rect::default());
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(DETAIL_HEIGHT)])
        .split(area);
    (chunks[0], chunks[1])
}

fn render_empty(message: &str, frame: &mut Frame, area: Rect) {
    let p = Paragraph::new(message).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(p, area);
}

fn list_state_for(selected: usize, len: usize) -> ListState {
    let mut ls = ListState::default();
    if len > 0 {
        ls.select(Some(selected.min(len - 1)));
    }
    ls
}

fn render_providers(
    providers: &[ProviderSummary],
    state: &SettingsViewState,
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
) {
    if providers.is_empty() {
        render_empty(
            "No LLM providers configured in [[llm.providers]].",
            frame,
            area,
        );
        return;
    }
    let (list_area, detail_area) = split_body(area);
    let selected = state.selected_index().min(providers.len() - 1);

    let items: Vec<ListItem> = providers
        .iter()
        .map(|p| {
            let marker = if p.active {
                "* "
            } else if p.default {
                "d "
            } else {
                "  "
            };
            let name = truncate_to_width(&p.name, 20);
            let model = truncate_to_width(p.model.as_deref().unwrap_or("(default)"), 24);
            let line = Line::from(vec![
                Span::styled(marker, theme.tool_success),
                Span::styled(format!("{name:<20}"), theme.system_message),
                Span::styled(
                    format!(" [{}] ", p.provider_type),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(model),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state = list_state_for(selected, providers.len());
    frame.render_stateful_widget(list, list_area, &mut list_state);

    if detail_area.height > 0
        && let Some(p) = providers.get(selected)
    {
        let lines = vec![
            Line::from(format!(
                "name: {}   type: {}{}{}",
                p.name,
                p.provider_type,
                if p.default { "   default" } else { "" },
                if p.active { "   active" } else { "" },
            )),
            Line::from(format!(
                "model: {}   base_url: {}",
                p.model.as_deref().unwrap_or("—"),
                p.base_url.as_deref().unwrap_or("—"),
            )),
            Line::from(format!(
                "max_tokens: {}   embedding_model: {}   stt_model: {}",
                p.max_tokens.map_or("—".to_owned(), |v| v.to_string()),
                p.embedding_model.as_deref().unwrap_or("—"),
                p.stt_model.as_deref().unwrap_or("—"),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(Color::Gray)),
            detail_area,
        );
    }
}

fn render_mcp(
    servers: &[McpServerStatus],
    state: &SettingsViewState,
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
) {
    if servers.is_empty() {
        render_empty("No MCP servers configured.", frame, area);
        return;
    }
    let (list_area, detail_area) = split_body(area);
    let selected = state.selected_index().min(servers.len() - 1);

    let items: Vec<ListItem> = servers
        .iter()
        .map(|s| {
            let (status_text, status_style) = match s.status {
                McpServerConnectionStatus::Connected => ("connected", theme.tool_success),
                McpServerConnectionStatus::Failed => ("failed", theme.tool_failure),
                // McpServerConnectionStatus is #[non_exhaustive]; treat unknown as transitional.
                _ => ("connecting", Style::default().fg(Color::Yellow)),
            };
            let id = truncate_to_width(&s.id, 24);
            let line = Line::from(vec![
                Span::styled(format!("{id:<24} "), theme.system_message),
                Span::styled(format!("{status_text:<11}"), status_style),
                Span::raw(format!(" tools: {}", s.tool_count)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state = list_state_for(selected, servers.len());
    frame.render_stateful_widget(list, list_area, &mut list_state);

    if detail_area.height > 0
        && let Some(s) = servers.get(selected)
    {
        let lines = vec![
            Line::from(format!(
                "id: {}   status: {:?}   tools: {}",
                s.id, s.status, s.tool_count
            )),
            Line::from(if s.error.is_empty() {
                "error: —".to_owned()
            } else {
                format!("error: {}", s.error)
            }),
            Line::from(format!(
                "input_schemas_dropped: {}   output_schemas_dropped: {}",
                s.input_schemas_dropped, s.output_schemas_dropped
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(Color::Gray)),
            detail_area,
        );
    }
}

fn render_agents(
    defs: &[AgentDefSummary],
    state: &SettingsViewState,
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
) {
    if defs.is_empty() {
        render_empty("No sub-agent definitions found.", frame, area);
        return;
    }
    let (list_area, detail_area) = split_body(area);
    let selected = state.selected_index().min(defs.len() - 1);

    let items: Vec<ListItem> = defs
        .iter()
        .map(|d| {
            let name = truncate_to_width(&d.name, 20);
            let desc = truncate_to_width(&d.description, 40);
            let line = Line::from(vec![
                Span::styled(format!("{name:<20} "), theme.system_message),
                Span::raw(desc),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state = list_state_for(selected, defs.len());
    frame.render_stateful_widget(list, list_area, &mut list_state);

    if detail_area.height > 0
        && let Some(d) = defs.get(selected)
    {
        let lines = vec![
            Line::from(format!(
                "name: {}   model: {}   source: {}",
                d.name,
                d.model.as_deref().unwrap_or("inherit"),
                d.source.as_deref().unwrap_or("—"),
            )),
            Line::from(format!(
                "memory: {}",
                d.memory_scope.as_deref().unwrap_or("—")
            )),
            Line::from(format!("tools: {}", d.tools_summary)),
        ];
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().fg(Color::Gray)),
            detail_area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::render_to_string;

    fn provider(name: &str, active: bool) -> ProviderSummary {
        ProviderSummary {
            name: name.to_owned(),
            provider_type: "claude".to_owned(),
            active,
            ..ProviderSummary::default()
        }
    }

    #[test]
    fn settings_tab_cycles_wrap() {
        assert_eq!(SettingsTab::Providers.next(), SettingsTab::Mcp);
        assert_eq!(SettingsTab::Mcp.next(), SettingsTab::Agents);
        assert_eq!(SettingsTab::Agents.next(), SettingsTab::Providers);
        assert_eq!(SettingsTab::Providers.previous(), SettingsTab::Agents);
    }

    #[test]
    fn per_tab_selection_is_independent() {
        let mut state = SettingsViewState::default();
        state.select_next(5);
        state.select_next(5);
        assert_eq!(state.selected_index(), 2);
        state.next_tab();
        assert_eq!(
            state.selected_index(),
            0,
            "switching tabs must not carry over the previous tab's selection"
        );
        state.previous_tab();
        assert_eq!(
            state.selected_index(),
            2,
            "returning to a tab must restore its own selection"
        );
    }

    #[test]
    fn select_next_clamps_at_count_minus_one() {
        let mut state = SettingsViewState::default();
        for _ in 0..10 {
            state.select_next(3);
        }
        assert_eq!(state.selected_index(), 2);
    }

    #[test]
    fn select_previous_clamps_at_zero() {
        let mut state = SettingsViewState::default();
        state.select_previous(3);
        assert_eq!(state.selected_index(), 0);
    }

    #[test]
    fn render_empty_providers_shows_empty_state() {
        let metrics = MetricsSnapshot::default();
        let mut state = SettingsViewState::default();
        let output = render_to_string(80, 24, |frame, area| {
            render(&metrics, &mut state, frame, area, &Theme::default());
        });
        assert!(output.contains("No LLM providers configured"));
    }

    #[test]
    fn render_providers_never_shows_secret_values() {
        // SC-003 (settings-view side): even if a caller mistakenly seeded a name/model
        // containing what looks like a credential, ProviderSummary has no field that
        // could carry api_key/cocoon_access_hash/hf_token — assert the rendered output
        // never contains the sentinel value used across the crate's other secret tests.
        let metrics = MetricsSnapshot {
            providers: vec![provider("prod", true)].into(),
            ..MetricsSnapshot::default()
        };
        let mut state = SettingsViewState::default();
        let output = render_to_string(80, 24, |frame, area| {
            render(&metrics, &mut state, frame, area, &Theme::default());
        });
        assert!(output.contains("prod"));
        assert!(!output.contains("SUPERSECRET"));
    }

    #[test]
    fn render_mcp_tab_shows_empty_state() {
        let metrics = MetricsSnapshot::default();
        let mut state = SettingsViewState {
            tab: SettingsTab::Mcp,
            ..SettingsViewState::default()
        };
        let output = render_to_string(80, 24, |frame, area| {
            render(&metrics, &mut state, frame, area, &Theme::default());
        });
        assert!(output.contains("No MCP servers configured"));
    }

    #[test]
    fn render_agents_tab_shows_empty_state() {
        let metrics = MetricsSnapshot::default();
        let mut state = SettingsViewState {
            tab: SettingsTab::Agents,
            ..SettingsViewState::default()
        };
        let output = render_to_string(80, 24, |frame, area| {
            render(&metrics, &mut state, frame, area, &Theme::default());
        });
        assert!(output.contains("No sub-agent definitions found"));
    }

    #[test]
    fn render_zero_area_does_not_panic() {
        let metrics = MetricsSnapshot::default();
        let mut state = SettingsViewState::default();
        let output = render_to_string(80, 24, |frame, _area| {
            render(
                &metrics,
                &mut state,
                frame,
                Rect::default(),
                &Theme::default(),
            );
        });
        let _ = output;
    }
}
