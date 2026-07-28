// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use zeph_subagent::{ModelSpec, SubAgentDef, ToolPolicy, is_valid_agent_name};

use crate::layout::truncate_to_width;
use crate::metrics::{MetricsSnapshot, SubAgentMetrics};
use crate::theme::Theme;
use crate::widgets::panel;
use crate::widgets::spinner::breeze_frame;

// ── Runtime sub-agent monitor ─────────────────────────────────────────────────

/// Which base-layer view the `SubAgents` slot renders this frame.
///
/// Chosen once per frame by `App::subagent_slot_mode` and consumed by both sizing
/// (`App::panel_demands`) and rendering (`App::render_subagents_slot`) so the two decisions
/// can never disagree (#6675) — this mirrors the same priority chain that used to be
/// re-derived independently in `render_subagents_slot` and `App::effective_collapsed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentSlotMode {
    /// User is focused on the `SubAgents` panel (`a` key): interactive sidebar, optionally with
    /// a live forwarded transcript. Sized `Greedy` — the live transcript wraps.
    Interactive,
    /// DAG/orchestration plan view is active and not dismissed.
    PlanView,
    /// Recent security events summary.
    Security,
    /// Idle default: plain sub-agents list.
    List,
}

fn state_color(state: &str) -> Color {
    match state {
        "working" | "submitted" => Color::Yellow,
        "completed" => Color::Green,
        "failed" => Color::Red,
        "input_required" => Color::Cyan,
        _ => Color::DarkGray,
    }
}

fn build_agent_list_item<'a>(
    sa: &SubAgentMetrics,
    tick: u8,
    selected: bool,
    ascii: bool,
) -> ListItem<'a> {
    ListItem::new(build_agent_line(sa, tick, selected, ascii))
}

fn build_agent_line(sa: &SubAgentMetrics, tick: u8, selected: bool, ascii: bool) -> Line<'static> {
    let color = state_color(&sa.state);
    let is_working = matches!(sa.state.as_str(), "working" | "submitted");
    let spinner = if is_working {
        breeze_frame(u64::from(tick), ascii)
    } else {
        // Pad to 3 spaces to match the 3-cell active frame, preventing column jitter.
        "   "
    };

    let bg_marker = if sa.background { " [bg]" } else { "" };
    let perm_badge = match sa.permission_mode.as_str() {
        "plan" => " [plan]",
        "bypass_permissions" => " [bypass!]",
        "dont_ask" => " [dont_ask]",
        "accept_edits" => " [accept_edits]",
        _ => "",
    };

    let base_style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };

    Line::from(vec![
        Span::styled(format!(" {spinner} "), Style::default().fg(color)),
        Span::styled(
            format!("{}{}{}", sa.name, bg_marker, perm_badge),
            base_style,
        ),
        Span::styled(
            format!(" {}", sa.state.to_uppercase()),
            base_style.fg(color),
        ),
        Span::styled(
            format!(" {}/{}  {}s", sa.turns_used, sa.max_turns, sa.elapsed_secs),
            base_style,
        ),
    ])
}

/// Build the plain (non-interactive) sub-agents list's content lines: a header plus one row
/// per sub-agent, or a two-line placeholder when there are none.
///
/// Pure function of `metrics` and `theme` — never of the allocated `Rect` — so
/// [`desired_height`] and [`render`] can never disagree about how many rows this view needs.
pub(crate) fn lines(metrics: &MetricsSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    if metrics.sub_agents.is_empty() {
        return vec![
            Line::from(Span::styled(
                "agents · none",
                theme.system_message.add_modifier(Modifier::BOLD),
            )),
            Line::from("  No sub-agents. Use /agent spawn <name> to create one."),
        ];
    }

    let mut out = vec![Line::from(Span::styled(
        format!("agents · {}", metrics.sub_agents.len()),
        theme.system_message.add_modifier(Modifier::BOLD),
    ))];
    // Non-interactive view uses tick=0 (no animation); ascii flag is irrelevant when idle,
    // but kept for consistency if a working agent appears in the static view.
    out.extend(
        metrics
            .sub_agents
            .iter()
            .map(|sa| build_agent_line(sa, 0, false, false)),
    );
    out
}

/// Number of rows the plain sub-agents list needs to show all of `metrics` without truncation.
#[must_use]
pub fn desired_height(metrics: &MetricsSnapshot, theme: &Theme) -> u16 {
    u16::try_from(lines(metrics, theme).len()).unwrap_or(u16::MAX)
}

/// Non-interactive render (used when `SubAgents` panel is not focused).
pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    panel::render_lines(frame, area, lines(metrics, theme), theme);
}

/// Interactive render: shows selection highlight and spinner animation.
/// Called when the `SubAgents` panel has keyboard focus (`a` key).
pub fn render_interactive(
    metrics: &MetricsSnapshot,
    sidebar: &mut crate::app::SubAgentSidebarState,
    frame: &mut Frame,
    area: Rect,
    tick: u8,
    theme: &Theme,
    ascii: bool,
) {
    use ratatui::text::Span;

    if area.height == 0 || area.width == 0 {
        return;
    }

    if metrics.sub_agents.is_empty() {
        let header = Line::from(vec![
            Span::styled("≈ ", theme.highlight),
            Span::styled(
                "agents · none  [j/k=nav  Esc=close]",
                theme.highlight.add_modifier(Modifier::BOLD),
            ),
        ]);
        let body = Paragraph::new(vec![
            header,
            Line::from("  No sub-agents. Use /agent spawn <name> to create one."),
        ]);
        frame.render_widget(body, area);
        return;
    }

    let selected = sidebar.selected();
    let items: Vec<ListItem<'_>> = metrics
        .sub_agents
        .iter()
        .enumerate()
        .map(|(i, sa)| build_agent_list_item(sa, tick, selected == Some(i), ascii))
        .collect();

    let header = Line::from(vec![
        Span::styled("≈ ", theme.highlight),
        Span::styled(
            format!(
                "agents · {}  [j/k=nav  Enter=view  Esc=close]",
                metrics.sub_agents.len()
            ),
            theme.highlight.add_modifier(Modifier::BOLD),
        ),
    ]);

    if area.height <= 1 {
        frame.render_widget(Paragraph::new(vec![header]), area);
        return;
    }

    // FR-005 (issue #6359): extend this existing runtime detail view with a live forwarded
    // transcript tail for the selected agent, rather than building a separate UI surface.
    // Falls back to the unchanged list-only layout when nothing has been forwarded yet
    // (forwarding disabled, no surface active, or the agent has not produced output) or
    // when the panel is too short to usefully split.
    let live_tail: &[String] = selected
        .and_then(|i| metrics.sub_agents.get(i))
        .map_or(&[][..], |sa| sa.live_transcript.as_slice());

    if live_tail.is_empty() || area.height < 8 {
        let splits = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
        frame.render_widget(Paragraph::new(vec![header]), splits[0]);
        frame.render_stateful_widget(List::new(items), splits[1], &mut sidebar.list_state);
        return;
    }

    let splits = Layout::vertical([
        Constraint::Length(1),
        Constraint::Percentage(45),
        Constraint::Min(3),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(vec![header]), splits[0]);
    frame.render_stateful_widget(List::new(items), splits[1], &mut sidebar.list_state);
    render_live_transcript_panel(live_tail, theme, frame, splits[2]);
}

/// Render the trailing forwarded-transcript lines for the selected subagent (FR-005).
fn render_live_transcript_panel(lines: &[String], theme: &Theme, frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .title(Span::styled(" live transcript ", theme.highlight));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = lines.join("\n");
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

// ── Definition manager ────────────────────────────────────────────────────────

/// Form field for create/edit wizard.
#[derive(Debug, Clone)]
pub struct FormField {
    pub label: &'static str,
    pub value: String,
    pub required: bool,
    pub placeholder: &'static str,
}

/// Shared state for Create and Edit forms.
#[derive(Debug, Clone)]
pub struct AgentFormState {
    pub fields: Vec<FormField>,
    /// Which field has keyboard focus.
    pub focused: usize,
    /// Cursor position within focused field value string.
    pub cursor: usize,
    pub error: Option<String>,
}

impl AgentFormState {
    #[must_use]
    pub fn new_empty() -> Self {
        Self {
            fields: vec![
                FormField {
                    label: "Name",
                    value: String::new(),
                    required: true,
                    placeholder: "e.g. code-reviewer",
                },
                FormField {
                    label: "Description",
                    value: String::new(),
                    required: true,
                    placeholder: "Short description",
                },
                FormField {
                    label: "Model",
                    value: String::new(),
                    required: false,
                    placeholder: "e.g. claude-sonnet-4-20250514 (optional)",
                },
                FormField {
                    label: "Max turns",
                    value: "20".to_owned(),
                    required: false,
                    placeholder: "20",
                },
            ],
            focused: 0,
            cursor: 0,
            error: None,
        }
    }

    #[must_use]
    pub fn from_def(def: &SubAgentDef) -> Self {
        let mut form = Self::new_empty();
        form.fields[0].value.clone_from(&def.name);
        form.fields[1].value.clone_from(&def.description);
        form.fields[2].value = def.model.as_ref().map_or("", ModelSpec::as_str).to_string();
        form.fields[3].value = def.permissions.max_turns.to_string();
        // Reset focus to beginning; cursor is char-count, not byte offset.
        form.focused = 0;
        form.cursor = form.fields[0].value.chars().count();
        form
    }

    pub fn focus_next(&mut self) {
        if self.focused + 1 < self.fields.len() {
            self.focused += 1;
            self.cursor = self.fields[self.focused].value.chars().count();
        }
    }

    pub fn focus_prev(&mut self) {
        if self.focused > 0 {
            self.focused -= 1;
            self.cursor = self.fields[self.focused].value.chars().count();
        }
    }

    pub fn insert_char(&mut self, c: char) {
        let val = &mut self.fields[self.focused].value;
        // Convert char-count cursor to byte offset before inserting.
        let byte_offset = val
            .char_indices()
            .nth(self.cursor)
            .map_or(val.len(), |(i, _)| i);
        val.insert(byte_offset, c);
        self.cursor += 1;
        self.error = None;
    }

    pub fn delete_char_before_cursor(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let val = &mut self.fields[self.focused].value;
            // Convert char-count cursor to byte offset before removing.
            if let Some((byte_offset, _)) = val.char_indices().nth(self.cursor) {
                val.remove(byte_offset);
            }
            self.error = None;
        }
    }

    /// Validate and build a `SubAgentDef`. Returns `Err` with user-facing message on failure.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` if required fields are empty or `max_turns` is not a valid integer.
    pub fn to_def(&self) -> Result<SubAgentDef, String> {
        let name = self.fields[0].value.trim().to_owned();
        let description = self.fields[1].value.trim().to_owned();
        if name.is_empty() {
            return Err("Name is required".into());
        }
        if !is_valid_agent_name(&name) {
            return Err(
                "Name must match [a-zA-Z0-9][a-zA-Z0-9_-]{0,63} (ASCII only, no spaces)".into(),
            );
        }
        if description.is_empty() {
            return Err("Description is required".into());
        }
        let model = self.fields[2].value.trim();
        let max_turns: u32 = self.fields[3]
            .value
            .trim()
            .parse()
            .map_err(|_| "Max turns must be a positive integer".to_owned())?;

        let mut def = SubAgentDef::default_template(name, description);
        if !model.is_empty() {
            def.model = Some(zeph_subagent::ModelSpec::Named(model.to_owned()));
        }
        def.permissions.max_turns = max_turns;
        Ok(def)
    }
}

/// States of the agent definition manager panel.
#[non_exhaustive]
#[derive(Debug)]
pub enum AgentManagerState {
    /// Shows a scrollable list of all definitions.
    List {
        definitions: Vec<SubAgentDef>,
        list_state: ListState,
    },
    /// Shows full detail of a selected definition.
    Detail {
        definitions: Vec<SubAgentDef>,
        index: usize,
    },
    /// Create wizard (empty form).
    Create {
        /// Preserved list for restoring on Esc.
        definitions: Vec<SubAgentDef>,
        form: AgentFormState,
    },
    /// Edit wizard (pre-filled form).
    Edit {
        definitions: Vec<SubAgentDef>,
        index: usize,
        form: AgentFormState,
    },
    /// Confirm deletion prompt.
    ConfirmDelete {
        definitions: Vec<SubAgentDef>,
        index: usize,
        /// True when definition is not project-scoped (extra warning shown).
        non_project: bool,
        /// Awaiting second confirmation for non-project scope.
        awaiting_second: bool,
    },
}

impl AgentManagerState {
    /// Create a new panel showing a loaded list of definitions.
    #[must_use]
    pub fn from_definitions(defs: Vec<SubAgentDef>) -> Self {
        let mut state = ListState::default();
        if !defs.is_empty() {
            state.select(Some(0));
        }
        Self::List {
            definitions: defs,
            list_state: state,
        }
    }

    /// Handle a key event. Returns `true` if the panel should be closed.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Extract next state from helper; None means no state transition.
        // Returns (close_panel, Option<new_state>).
        let (close, next) = handle_key_dispatch(self, key);
        if let Some(s) = next {
            *self = s;
        }
        close
    }
}

/// Returns `(close_panel, Option<new_state>)`.
fn handle_key_dispatch(
    state: &mut AgentManagerState,
    key: KeyEvent,
) -> (bool, Option<AgentManagerState>) {
    match state {
        AgentManagerState::List {
            definitions,
            list_state,
        } => {
            match key.code {
                KeyCode::Esc => return (true, None),
                KeyCode::Down | KeyCode::Char('j') => {
                    let next = list_state
                        .selected()
                        .map_or(0, |i| (i + 1).min(definitions.len().saturating_sub(1)));
                    list_state.select(Some(next));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let prev = list_state.selected().map_or(0, |i| i.saturating_sub(1));
                    list_state.select(Some(prev));
                }
                KeyCode::Enter => {
                    if let Some(i) = list_state.selected() {
                        let defs = std::mem::take(definitions);
                        return (
                            false,
                            Some(AgentManagerState::Detail {
                                definitions: defs,
                                index: i,
                            }),
                        );
                    }
                }
                KeyCode::Char('c') => {
                    let defs = std::mem::take(definitions);
                    return (
                        false,
                        Some(AgentManagerState::Create {
                            definitions: defs,
                            form: AgentFormState::new_empty(),
                        }),
                    );
                }
                _ => {}
            }
            (false, None)
        }
        AgentManagerState::Detail { definitions, index } => {
            handle_key_detail(definitions, *index, key)
        }
        AgentManagerState::Create { definitions, form } => {
            handle_key_form_create(definitions, form, key)
        }
        AgentManagerState::Edit {
            definitions,
            index,
            form,
        } => handle_key_form_edit(definitions, *index, form, key),
        AgentManagerState::ConfirmDelete {
            definitions,
            index,
            non_project,
            awaiting_second,
        } => handle_key_confirm_delete(definitions, *index, *non_project, awaiting_second, key),
    }
}

fn handle_key_detail(
    definitions: &mut Vec<SubAgentDef>,
    index: usize,
    key: KeyEvent,
) -> (bool, Option<AgentManagerState>) {
    match key.code {
        KeyCode::Esc => {
            let defs = std::mem::take(definitions);
            let mut list_state = ListState::default();
            list_state.select(Some(index));
            (
                false,
                Some(AgentManagerState::List {
                    definitions: defs,
                    list_state,
                }),
            )
        }
        KeyCode::Char('e') => {
            let form = AgentFormState::from_def(&definitions[index]);
            let defs = std::mem::take(definitions);
            (
                false,
                Some(AgentManagerState::Edit {
                    definitions: defs,
                    index,
                    form,
                }),
            )
        }
        KeyCode::Char('d') => {
            let source = definitions[index].source.as_deref().unwrap_or("");
            let non_project = !source.starts_with("project/");
            let defs = std::mem::take(definitions);
            (
                false,
                Some(AgentManagerState::ConfirmDelete {
                    definitions: defs,
                    index,
                    non_project,
                    awaiting_second: false,
                }),
            )
        }
        _ => (false, None),
    }
}

fn handle_key_form_create(
    definitions: &mut Vec<SubAgentDef>,
    form: &mut AgentFormState,
    key: KeyEvent,
) -> (bool, Option<AgentManagerState>) {
    match key.code {
        KeyCode::Esc => {
            // Restore definitions list on cancel (S3 fix).
            let defs = std::mem::take(definitions);
            (false, Some(AgentManagerState::from_definitions(defs)))
        }
        KeyCode::Tab => {
            form.focus_next();
            (false, None)
        }
        KeyCode::BackTab => {
            form.focus_prev();
            (false, None)
        }
        KeyCode::Backspace => {
            form.delete_char_before_cursor();
            (false, None)
        }
        KeyCode::Enter => {
            match form.to_def() {
                Ok(def) => {
                    // C3: canonicalize CWD + ".zeph/agents" for project root resolution.
                    let dir = std::env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        .join(".zeph/agents");
                    match def.save_atomic(&dir) {
                        Ok(_) => {
                            // Restore list after successful create (S3 fix).
                            let defs = std::mem::take(definitions);
                            return (false, Some(AgentManagerState::from_definitions(defs)));
                        }
                        Err(e) => {
                            form.error = Some(e.to_string());
                        }
                    }
                }
                Err(msg) => {
                    form.error = Some(msg);
                }
            }
            (false, None)
        }
        KeyCode::Char(c) => {
            form.insert_char(c);
            (false, None)
        }
        _ => (false, None),
    }
}

fn handle_key_form_edit(
    definitions: &mut Vec<SubAgentDef>,
    index: usize,
    form: &mut AgentFormState,
    key: KeyEvent,
) -> (bool, Option<AgentManagerState>) {
    match key.code {
        KeyCode::Esc => {
            let defs = std::mem::take(definitions);
            (
                false,
                Some(AgentManagerState::Detail {
                    definitions: defs,
                    index,
                }),
            )
        }
        KeyCode::Tab => {
            form.focus_next();
            (false, None)
        }
        KeyCode::BackTab => {
            form.focus_prev();
            (false, None)
        }
        KeyCode::Backspace => {
            form.delete_char_before_cursor();
            (false, None)
        }
        KeyCode::Enter => {
            match form.to_def() {
                Ok(mut def) => {
                    if let Some(path) = definitions[index].file_path.as_deref() {
                        let dir = path.parent().unwrap_or(std::path::Path::new("."));
                        // Preserve file_path on the new def so Detail view can edit/delete.
                        def.file_path = Some(path.to_path_buf());
                        def.source.clone_from(&definitions[index].source);
                        match def.save_atomic(dir) {
                            Ok(_) => {
                                // S2: update in-memory definition after save.
                                definitions[index] = def;
                                let defs = std::mem::take(definitions);
                                return (
                                    false,
                                    Some(AgentManagerState::Detail {
                                        definitions: defs,
                                        index,
                                    }),
                                );
                            }
                            Err(e) => {
                                form.error = Some(e.to_string());
                            }
                        }
                    } else {
                        form.error = Some("Cannot determine file path for this definition".into());
                    }
                }
                Err(msg) => {
                    form.error = Some(msg);
                }
            }
            (false, None)
        }
        KeyCode::Char(c) => {
            form.insert_char(c);
            (false, None)
        }
        _ => (false, None),
    }
}

fn handle_key_confirm_delete(
    definitions: &mut Vec<SubAgentDef>,
    index: usize,
    non_project: bool,
    awaiting_second: &mut bool,
    key: KeyEvent,
) -> (bool, Option<AgentManagerState>) {
    match key.code {
        KeyCode::Esc => {
            let defs = std::mem::take(definitions);
            (
                false,
                Some(AgentManagerState::Detail {
                    definitions: defs,
                    index,
                }),
            )
        }
        KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
            // IMP-04: extra confirmation for non-project scope
            if non_project && !*awaiting_second {
                *awaiting_second = true;
                return (false, None);
            }
            let next = if let Some(path) = definitions[index].file_path.as_deref() {
                match SubAgentDef::delete_file(path) {
                    Ok(()) => {
                        // S4: remove deleted entry from list, keep the rest.
                        let mut defs = std::mem::take(definitions);
                        defs.remove(index);
                        let selected = if defs.is_empty() {
                            None
                        } else {
                            Some(index.saturating_sub(1).min(defs.len() - 1))
                        };
                        let mut list_state = ListState::default();
                        list_state.select(selected);
                        AgentManagerState::List {
                            definitions: defs,
                            list_state,
                        }
                    }
                    Err(e) => {
                        // S5: surface delete error to user.
                        let defs = std::mem::take(definitions);
                        // Re-borrow after state transition is not possible here;
                        // error is shown via a Detail render with no error field.
                        tracing::warn!(error = %e, "failed to delete agent definition");
                        AgentManagerState::Detail {
                            definitions: defs,
                            index,
                        }
                    }
                }
            } else {
                // No file_path — just remove from in-memory list.
                let mut defs = std::mem::take(definitions);
                defs.remove(index);
                AgentManagerState::from_definitions(defs)
            };
            (false, Some(next))
        }
        _ => (false, None),
    }
}

/// Render the agent definition manager panel as a floating overlay.
pub fn render_manager(state: &mut AgentManagerState, frame: &mut Frame, area: Rect, theme: &Theme) {
    // Center floating panel
    let panel = centered_rect(80, 80, area);
    frame.render_widget(Clear, panel);

    match state {
        AgentManagerState::List {
            definitions,
            list_state,
        } => render_list(definitions, list_state, theme, frame, panel),
        AgentManagerState::Detail { definitions, index } => {
            render_detail(definitions, *index, theme, frame, panel);
        }
        AgentManagerState::Create { form, .. } => {
            render_form(form, "Create Sub-Agent", theme, frame, panel);
        }
        AgentManagerState::Edit { form, .. } => {
            render_form(form, "Edit Sub-Agent", theme, frame, panel);
        }
        AgentManagerState::ConfirmDelete {
            definitions,
            index,
            non_project,
            awaiting_second,
        } => render_confirm_delete(
            definitions,
            *index,
            *non_project,
            *awaiting_second,
            theme,
            frame,
            panel,
        ),
    }
}

fn render_list(
    defs: &[SubAgentDef],
    list_state: &mut ListState,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    let items: Vec<ListItem<'_>> = defs
        .iter()
        .map(|d| {
            let scope = d.source.as_deref().unwrap_or("-");
            let model = d.model.as_ref().map_or("-", ModelSpec::as_str);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {:<24}", d.name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" {scope:<12}")),
                Span::styled(
                    format!(" {:<36}", truncate_str(&d.description, 36)),
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(format!(" {model}"), Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(" Agent Definitions  [j/k] navigate  [Enter] detail  [c] create  [Esc] close ");

    if defs.is_empty() {
        let para = Paragraph::new("No definitions found. Press [c] to create one.")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, area);
    } else {
        let list = List::new(items).block(block).highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(list, area, list_state);
    }
}

fn render_detail(defs: &[SubAgentDef], index: usize, theme: &Theme, frame: &mut Frame, area: Rect) {
    let def = &defs[index];
    let tools_str = match &def.tools {
        ToolPolicy::AllowList(v) => format!("allow {v:?}"),
        ToolPolicy::DenyList(v) => format!("deny {v:?}"),
        _ => "all".to_owned(),
    };
    let except_str = if def.disallowed_tools.is_empty() {
        String::new()
    } else {
        format!(" except {:?}", def.disallowed_tools)
    };
    let mut text = vec![
        Line::from(vec![
            Span::styled(
                "Name:        ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(&def.name),
        ]),
        Line::from(vec![
            Span::styled(
                "Description: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(&def.description),
        ]),
        Line::from(vec![
            Span::styled(
                "Source:      ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(def.source.as_deref().unwrap_or("-")),
        ]),
        Line::from(vec![
            Span::styled(
                "Model:       ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(def.model.as_ref().map_or("-", ModelSpec::as_str)),
        ]),
        Line::from(vec![
            Span::styled(
                "Mode:        ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{:?}", def.permissions.permission_mode)),
        ]),
        Line::from(vec![
            Span::styled(
                "Max turns:   ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(def.permissions.max_turns.to_string()),
        ]),
        Line::from(vec![
            Span::styled(
                "Background:  ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(def.permissions.background.to_string()),
        ]),
        Line::from(vec![
            Span::styled(
                "Tools:       ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{tools_str}{except_str}")),
        ]),
    ];

    if !def.system_prompt.is_empty() {
        text.push(Line::raw(""));
        text.push(Line::from(Span::styled(
            "System prompt:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        let mut lines = def.system_prompt.lines();
        for line in lines.by_ref().take(10) {
            text.push(Line::raw(line.to_owned()));
        }
        if lines.next().is_some() {
            text.push(Line::from(Span::styled(
                "(truncated — use CLI `zeph agents show` for full prompt)",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let para = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(format!(
                    " {} ({}/{})  [e] edit  [d] delete  [Esc] back ",
                    def.name,
                    index + 1,
                    defs.len()
                )),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_form(form: &AgentFormState, title: &str, theme: &Theme, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(
            std::iter::repeat_n(Constraint::Length(3), form.fields.len())
                .chain([Constraint::Length(2), Constraint::Min(0)])
                .collect::<Vec<_>>(),
        )
        .split(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(format!(
            " {title}  [Tab] next field  [Enter] save  [Esc] cancel "
        ));
    frame.render_widget(block, area);

    for (i, field) in form.fields.iter().enumerate() {
        let is_focused = i == form.focused;
        let display = if field.value.is_empty() && !is_focused {
            Span::styled(field.placeholder, Style::default().fg(Color::DarkGray))
        } else {
            Span::raw(&field.value)
        };
        let label_suffix = if field.required { " *" } else { "" };
        let field_block = Block::default()
            .borders(Borders::ALL)
            .border_style(if is_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            })
            .title(format!(" {}{} ", field.label, label_suffix));

        let para = Paragraph::new(Line::from(vec![display])).block(field_block);
        if i < chunks.len() {
            frame.render_widget(para, chunks[i]);
        }
    }

    // Error message
    if let Some(err) = &form.error {
        let err_idx = form.fields.len();
        if err_idx < chunks.len() {
            let err_para =
                Paragraph::new(format!("  {err}")).style(Style::default().fg(Color::Red));
            frame.render_widget(err_para, chunks[err_idx]);
        }
    }
}

fn render_confirm_delete(
    defs: &[SubAgentDef],
    index: usize,
    non_project: bool,
    awaiting_second: bool,
    _theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    let def = &defs[index];
    let path_str = def
        .file_path
        .as_ref()
        .map_or_else(|| def.name.clone(), |p| p.display().to_string());

    let mut lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            format!("  Delete: {path_str}"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
    ];

    if non_project && !awaiting_second {
        lines.push(Line::from(Span::styled(
            "  WARNING: This is a USER-level definition shared across all projects.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));
        lines.push(Line::raw(
            "  Press [Enter/y] again to confirm, [Esc] to cancel.",
        ));
    } else if awaiting_second {
        lines.push(Line::from(Span::styled(
            "  Are you absolutely sure? This cannot be undone.",
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::raw(""));
        lines.push(Line::raw("  Press [Enter/y] to DELETE, [Esc] to cancel."));
    } else {
        lines.push(Line::raw("  Press [Enter/y] to confirm, [Esc] to cancel."));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title(" Confirm Delete "),
    );
    frame.render_widget(para, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn truncate_str(s: &str, max: usize) -> String {
    truncate_to_width(s, max)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use zeph_core::metrics::SubAgentMetrics;

    use crate::metrics::MetricsSnapshot;
    use crate::test_utils::render_to_string;

    use super::*;

    // ── Runtime monitor tests ─────────────────────────────────────────────────

    #[test]
    fn subagents_widget_renders_placeholder_when_empty() {
        let metrics = MetricsSnapshot::default();
        let output = render_to_string(60, 5, |frame, area| {
            let theme = crate::theme::Theme::default();
            super::render(&metrics, frame, area, &theme);
        });
        assert!(
            output.contains("agents") && output.contains("No sub-agents"),
            "expected placeholder text with data-first header, got: {output:?}"
        );
    }

    #[test]
    fn subagents_widget_renders_entries() {
        let metrics = MetricsSnapshot {
            sub_agents: vec![
                SubAgentMetrics {
                    id: "abc123".into(),
                    name: "code-reviewer".into(),
                    state: "working".into(),
                    turns_used: 3,
                    max_turns: 20,
                    background: false,
                    elapsed_secs: 42,
                    permission_mode: String::new(),
                    transcript_dir: None,
                    live_transcript: Vec::new(),
                },
                SubAgentMetrics {
                    id: "def456".into(),
                    name: "test-writer".into(),
                    state: "completed".into(),
                    turns_used: 10,
                    max_turns: 20,
                    background: true,
                    elapsed_secs: 100,
                    permission_mode: "dont_ask".into(),
                    transcript_dir: None,
                    live_transcript: Vec::new(),
                },
            ],
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(50, 10, |frame, area| {
            let theme = crate::theme::Theme::default();
            super::render(&metrics, frame, area, &theme);
        });
        assert!(
            output.contains("agents"),
            "expected data-first header; got: {output:?}"
        );
        assert!(output.contains("code-reviewer"));
        assert!(output.contains("test-writer"));
        assert!(output.contains("[dont_ask]"));
    }

    #[test]
    fn subagents_widget_renders_permission_badges() {
        let metrics = MetricsSnapshot {
            sub_agents: vec![
                SubAgentMetrics {
                    id: "a".into(),
                    name: "planner".into(),
                    state: "working".into(),
                    turns_used: 1,
                    max_turns: 5,
                    background: false,
                    elapsed_secs: 1,
                    permission_mode: "plan".into(),
                    transcript_dir: None,
                    live_transcript: Vec::new(),
                },
                SubAgentMetrics {
                    id: "b".into(),
                    name: "bypasser".into(),
                    state: "working".into(),
                    turns_used: 1,
                    max_turns: 5,
                    background: false,
                    elapsed_secs: 1,
                    permission_mode: "bypass_permissions".into(),
                    transcript_dir: None,
                    live_transcript: Vec::new(),
                },
            ],
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(60, 10, |frame, area| {
            let theme = crate::theme::Theme::default();
            super::render(&metrics, frame, area, &theme);
        });
        assert!(output.contains("[plan]"));
        assert!(output.contains("[bypass!]"));
    }

    // ── desired_height / lines parity (#6675) ──────────────────────────────────

    #[test]
    fn desired_height_two_when_empty() {
        let metrics = MetricsSnapshot::default();
        let theme = crate::theme::Theme::default();
        assert_eq!(desired_height(&metrics, &theme), 2);
    }

    #[test]
    fn desired_height_matches_header_plus_agent_count() {
        let metrics = MetricsSnapshot {
            sub_agents: vec![
                SubAgentMetrics {
                    id: "a".into(),
                    name: "one".into(),
                    state: "working".into(),
                    turns_used: 1,
                    max_turns: 5,
                    background: false,
                    elapsed_secs: 1,
                    permission_mode: String::new(),
                    transcript_dir: None,
                    live_transcript: Vec::new(),
                },
                SubAgentMetrics {
                    id: "b".into(),
                    name: "two".into(),
                    state: "completed".into(),
                    turns_used: 2,
                    max_turns: 5,
                    background: false,
                    elapsed_secs: 2,
                    permission_mode: String::new(),
                    transcript_dir: None,
                    live_transcript: Vec::new(),
                },
            ],
            ..MetricsSnapshot::default()
        };
        let theme = crate::theme::Theme::default();
        assert_eq!(desired_height(&metrics, &theme), 3);
        assert_eq!(lines(&metrics, &theme).len(), 3);
    }

    #[test]
    fn render_shows_overflow_indicator_when_area_too_small() {
        let metrics = MetricsSnapshot {
            sub_agents: (0..5)
                .map(|i| SubAgentMetrics {
                    id: format!("agent-{i}"),
                    name: format!("agent-{i}"),
                    state: "working".into(),
                    turns_used: 1,
                    max_turns: 5,
                    background: false,
                    elapsed_secs: 1,
                    permission_mode: String::new(),
                    transcript_dir: None,
                    live_transcript: Vec::new(),
                })
                .collect(),
            ..MetricsSnapshot::default()
        };
        let theme = crate::theme::Theme::default();
        // 6 lines (header + 5 agents) into a 2-row area.
        let output = render_to_string(60, 2, |frame, area| {
            super::render(&metrics, frame, area, &theme);
        });
        assert!(
            output.contains("more"),
            "must show overflow indicator when granted area is smaller than content, got:\n{output}"
        );
    }

    // ── Live transcript panel tests (issue #6359, FR-005) ─────────────────────

    fn agent_with_live_transcript(id: &str, live_transcript: Vec<String>) -> SubAgentMetrics {
        SubAgentMetrics {
            id: id.into(),
            name: "watched-agent".into(),
            state: "working".into(),
            turns_used: 2,
            max_turns: 10,
            background: false,
            elapsed_secs: 5,
            permission_mode: String::new(),
            transcript_dir: None,
            live_transcript,
        }
    }

    #[test]
    fn render_interactive_shows_live_transcript_for_selected_agent() {
        let metrics = MetricsSnapshot {
            sub_agents: vec![agent_with_live_transcript(
                "abc",
                vec![
                    "first forwarded turn".into(),
                    "second forwarded turn".into(),
                ],
            )],
            ..MetricsSnapshot::default()
        };
        let mut sidebar = crate::app::SubAgentSidebarState::new();
        sidebar.select_next(1);

        let output = render_to_string(60, 20, |frame, area| {
            let theme = crate::theme::Theme::default();
            super::render_interactive(&metrics, &mut sidebar, frame, area, 0, &theme, false);
        });

        assert!(
            output.contains("live transcript"),
            "expected a live-transcript panel title, got: {output:?}"
        );
        assert!(output.contains("first forwarded turn"));
        assert!(output.contains("second forwarded turn"));
    }

    #[test]
    fn render_interactive_omits_panel_when_nothing_forwarded() {
        let metrics = MetricsSnapshot {
            sub_agents: vec![agent_with_live_transcript("abc", Vec::new())],
            ..MetricsSnapshot::default()
        };
        let mut sidebar = crate::app::SubAgentSidebarState::new();
        sidebar.select_next(1);

        let output = render_to_string(60, 20, |frame, area| {
            let theme = crate::theme::Theme::default();
            super::render_interactive(&metrics, &mut sidebar, frame, area, 0, &theme, false);
        });

        assert!(
            !output.contains("live transcript"),
            "no forwarded lines yet — the panel must not appear, got: {output:?}"
        );
    }

    #[test]
    fn render_interactive_omits_panel_when_area_too_short() {
        let metrics = MetricsSnapshot {
            sub_agents: vec![agent_with_live_transcript("abc", vec!["a line".into()])],
            ..MetricsSnapshot::default()
        };
        let mut sidebar = crate::app::SubAgentSidebarState::new();
        sidebar.select_next(1);

        let output = render_to_string(60, 4, |frame, area| {
            let theme = crate::theme::Theme::default();
            super::render_interactive(&metrics, &mut sidebar, frame, area, 0, &theme, false);
        });

        assert!(
            !output.contains("live transcript"),
            "too little vertical room — must fall back to the list-only layout, got: {output:?}"
        );
    }

    // ── AgentManagerState tests ───────────────────────────────────────────────

    fn make_def(name: &str, description: &str) -> SubAgentDef {
        SubAgentDef::default_template(name, description)
    }

    #[test]
    fn agent_manager_list_renders_definitions() {
        let defs = vec![
            make_def("reviewer", "Reviews code"),
            make_def("writer", "Writes tests"),
        ];
        let mut state = AgentManagerState::from_definitions(defs);
        let theme = crate::theme::Theme::default();
        let output = render_to_string(80, 20, |frame, area| {
            render_manager(&mut state, frame, area, &theme);
        });
        assert!(output.contains("reviewer"));
        assert!(output.contains("writer"));
    }

    #[test]
    fn agent_manager_form_field_navigation() {
        let mut form = AgentFormState::new_empty();
        assert_eq!(form.focused, 0);
        form.focus_next();
        assert_eq!(form.focused, 1);
        form.focus_next();
        assert_eq!(form.focused, 2);
        form.focus_prev();
        assert_eq!(form.focused, 1);
    }

    #[test]
    fn agent_manager_form_char_input() {
        let mut form = AgentFormState::new_empty();
        form.insert_char('h');
        form.insert_char('i');
        assert_eq!(form.fields[0].value, "hi");
        assert_eq!(form.cursor, 2);
    }

    #[test]
    fn agent_manager_form_backspace() {
        let mut form = AgentFormState::new_empty();
        form.insert_char('a');
        form.insert_char('b');
        form.delete_char_before_cursor();
        assert_eq!(form.fields[0].value, "a");
        assert_eq!(form.cursor, 1);
    }

    #[test]
    fn agent_manager_form_submit_empty_name_fails() {
        let form = AgentFormState::new_empty();
        let result = form.to_def();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Name"));
    }

    #[test]
    fn agent_manager_form_submit_valid() {
        let mut form = AgentFormState::new_empty();
        for c in "reviewer".chars() {
            form.insert_char(c);
        }
        form.focus_next();
        for c in "Reviews code".chars() {
            form.insert_char(c);
        }
        let result = form.to_def();
        assert!(result.is_ok());
        let def = result.unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews code");
    }

    #[test]
    fn agent_panel_list_to_detail_transition() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![make_def("reviewer", "Reviews code")];
        let mut state = AgentManagerState::from_definitions(defs);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let closed = state.handle_key(enter);
        assert!(!closed);
        assert_matches!(state, AgentManagerState::Detail { index: 0, .. });
    }

    #[test]
    fn agent_panel_detail_esc_returns_to_list() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![make_def("reviewer", "Reviews code")];
        let mut state = AgentManagerState::Detail {
            definitions: defs,
            index: 0,
        };
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let closed = state.handle_key(esc);
        assert!(!closed);
        assert_matches!(state, AgentManagerState::List { .. });
    }

    #[test]
    fn agent_panel_list_esc_closes_panel() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let mut state = AgentManagerState::from_definitions(Vec::new());
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let closed = state.handle_key(esc);
        assert!(closed);
    }

    #[test]
    fn agent_panel_detail_to_create_transition() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![make_def("reviewer", "Reviews code")];
        let mut state = AgentManagerState::from_definitions(defs);
        let c_key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        state.handle_key(c_key);
        assert_matches!(state, AgentManagerState::Create { .. });
    }

    #[test]
    fn agent_command_entries_present() {
        use crate::command::extra_command_registry;
        let all = extra_command_registry();
        assert!(all.iter().any(|e| e.id == "agents:show"));
        assert!(all.iter().any(|e| e.id == "agents:create"));
        assert!(all.iter().any(|e| e.id == "agents:edit"));
        assert!(all.iter().any(|e| e.id == "agents:delete"));
    }

    // ── New tests for review findings ─────────────────────────────────────────

    #[test]
    fn agent_manager_form_submit_invalid_name_fails() {
        let mut form = AgentFormState::new_empty();
        for c in "my agent".chars() {
            form.insert_char(c);
        }
        form.focus_next();
        for c in "desc".chars() {
            form.insert_char(c);
        }
        let result = form.to_def();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Name must match"));
    }

    #[test]
    fn agent_manager_form_submit_empty_description_fails() {
        let mut form = AgentFormState::new_empty();
        for c in "reviewer".chars() {
            form.insert_char(c);
        }
        let result = form.to_def();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Description"));
    }

    #[test]
    fn agent_manager_form_submit_invalid_max_turns_fails() {
        let mut form = AgentFormState::new_empty();
        for c in "reviewer".chars() {
            form.insert_char(c);
        }
        form.focus_next();
        for c in "Reviews code".chars() {
            form.insert_char(c);
        }
        // Override max_turns field with invalid value
        form.fields[3].value = "not-a-number".to_owned();
        let result = form.to_def();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("integer"));
    }

    #[test]
    fn agent_manager_form_from_def_populates_fields() {
        let mut def = SubAgentDef::default_template("reviewer", "Reviews code");
        def.model = Some(zeph_subagent::ModelSpec::Named(
            "claude-sonnet-4-20250514".to_owned(),
        ));
        def.permissions.max_turns = 5;
        let form = AgentFormState::from_def(&def);
        assert_eq!(form.fields[0].value, "reviewer");
        assert_eq!(form.fields[1].value, "Reviews code");
        assert_eq!(form.fields[2].value, "claude-sonnet-4-20250514");
        assert_eq!(form.fields[3].value, "5");
    }

    #[test]
    fn agent_panel_detail_to_edit_transition() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![make_def("reviewer", "Reviews code")];
        let mut state = AgentManagerState::Detail {
            definitions: defs,
            index: 0,
        };
        let e_key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);
        let closed = state.handle_key(e_key);
        assert!(!closed);
        assert_matches!(state, AgentManagerState::Edit { index: 0, .. });
    }

    #[test]
    fn agent_panel_edit_esc_returns_to_detail() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![make_def("reviewer", "Reviews code")];
        let form = AgentFormState::from_def(&defs[0]);
        let mut state = AgentManagerState::Edit {
            definitions: defs,
            index: 0,
            form,
        };
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let closed = state.handle_key(esc);
        assert!(!closed);
        assert_matches!(state, AgentManagerState::Detail { index: 0, .. });
    }

    #[test]
    fn agent_panel_detail_to_confirm_delete_transition() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![make_def("reviewer", "Reviews code")];
        let mut state = AgentManagerState::Detail {
            definitions: defs,
            index: 0,
        };
        let d_key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        let closed = state.handle_key(d_key);
        assert!(!closed);
        assert_matches!(state, AgentManagerState::ConfirmDelete { .. });
    }

    #[test]
    fn agent_panel_confirm_delete_esc_returns_to_detail() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![make_def("reviewer", "Reviews code")];
        let mut state = AgentManagerState::ConfirmDelete {
            definitions: defs,
            index: 0,
            non_project: false,
            awaiting_second: false,
        };
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let closed = state.handle_key(esc);
        assert!(!closed);
        assert_matches!(state, AgentManagerState::Detail { index: 0, .. });
    }

    #[test]
    fn agent_panel_confirm_delete_non_project_two_step() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![make_def("reviewer", "Reviews code")];
        let mut state = AgentManagerState::ConfirmDelete {
            definitions: defs,
            index: 0,
            non_project: true,
            awaiting_second: false,
        };
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        // First Enter: sets awaiting_second = true, does NOT delete.
        state.handle_key(enter);
        assert_matches!(
            state,
            AgentManagerState::ConfirmDelete {
                awaiting_second: true,
                ..
            }
        );
    }

    #[test]
    fn agent_panel_create_esc_restores_definitions() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let defs = vec![
            make_def("reviewer", "Reviews code"),
            make_def("writer", "Writes tests"),
        ];
        let mut state = AgentManagerState::from_definitions(defs);
        // Press 'c' to enter Create, then Esc to cancel.
        let c_key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        state.handle_key(c_key);
        assert_matches!(state, AgentManagerState::Create { .. });

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        state.handle_key(esc);
        // Should be back to List with 2 definitions.
        if let AgentManagerState::List { definitions, .. } = &state {
            assert_eq!(definitions.len(), 2);
        } else {
            panic!("expected List state");
        }
    }

    #[test]
    fn agent_form_multibyte_char_insert_and_delete() {
        let mut form = AgentFormState::new_empty();
        // Insert ASCII chars normally.
        form.insert_char('a');
        form.insert_char('b');
        assert_eq!(form.fields[0].value, "ab");
        assert_eq!(form.cursor, 2);
        // Delete one char.
        form.delete_char_before_cursor();
        assert_eq!(form.fields[0].value, "a");
        assert_eq!(form.cursor, 1);
    }

    #[test]
    fn truncate_str_unicode_safe() {
        use unicode_width::UnicodeWidthStr;
        let s = "αβγδε";
        let truncated = truncate_str(s, 3);
        assert!(truncated.width() <= 3, "width={}", truncated.width());
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn truncate_str_ascii_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_cjk_wide_chars() {
        use unicode_width::UnicodeWidthStr;
        // 6 CJK chars = width 12; max=5 → truncated result must fit in 5 cols
        let r = truncate_str("日本語テスト", 5);
        assert!(r.width() <= 5, "width={} result={r:?}", r.width());
        assert!(r.ends_with('…'));
    }

    #[test]
    fn truncate_str_emoji_wide_chars() {
        use unicode_width::UnicodeWidthStr;
        // 3 emoji = width 6; max=4 → must fit in 4 cols
        let r = truncate_str("🎉🎊🎈", 4);
        assert!(r.width() <= 4, "width={} result={r:?}", r.width());
        assert!(r.ends_with('…'));
    }
}
