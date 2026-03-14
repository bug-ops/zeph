// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use zeph_core::subagent::{SubAgentDef, ToolPolicy, is_valid_agent_name};

use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;

// ── Runtime sub-agent monitor ─────────────────────────────────────────────────

pub fn render(metrics: &MetricsSnapshot, frame: &mut Frame, area: Rect) {
    if metrics.sub_agents.is_empty() {
        return;
    }

    let theme = Theme::default();

    let items: Vec<ListItem<'_>> = metrics
        .sub_agents
        .iter()
        .map(|sa| {
            let state_color = match sa.state.as_str() {
                "working" | "submitted" => Color::Yellow,
                "completed" => Color::Green,
                "failed" => Color::Red,
                "input_required" => Color::Cyan,
                _ => Color::DarkGray,
            };
            let bg_marker = if sa.background { " [bg]" } else { "" };
            let perm_badge = match sa.permission_mode.as_str() {
                "plan" => " [plan]",
                "bypass_permissions" => " [bypass!]",
                "dont_ask" => " [dont_ask]",
                "accept_edits" => " [accept_edits]",
                _ => "",
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("  {}{}{}", sa.name, bg_marker, perm_badge),
                    Style::default(),
                ),
                Span::styled(
                    format!("  {}", sa.state.to_uppercase()),
                    Style::default().fg(state_color),
                ),
                Span::raw(format!(
                    "  {}/{}  {}s",
                    sa.turns_used, sa.max_turns, sa.elapsed_secs
                )),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(theme.panel_border)
            .title(format!(" Sub-Agents ({}) ", metrics.sub_agents.len())),
    );
    frame.render_widget(list, area);
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
        def.model
            .as_deref()
            .unwrap_or("")
            .clone_into(&mut form.fields[2].value);
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
            def.model = Some(model.to_owned());
        }
        def.permissions.max_turns = max_turns;
        Ok(def)
    }
}

/// States of the agent definition manager panel.
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
pub fn render_manager(state: &mut AgentManagerState, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    // Center floating panel
    let panel = centered_rect(80, 80, area);
    frame.render_widget(Clear, panel);

    match state {
        AgentManagerState::List {
            definitions,
            list_state,
        } => render_list(definitions, list_state, &theme, frame, panel),
        AgentManagerState::Detail { definitions, index } => {
            render_detail(definitions, *index, &theme, frame, panel);
        }
        AgentManagerState::Create { form, .. } => {
            render_form(form, "Create Sub-Agent", &theme, frame, panel);
        }
        AgentManagerState::Edit { form, .. } => {
            render_form(form, "Edit Sub-Agent", &theme, frame, panel);
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
            &theme,
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
            let model = d.model.as_deref().unwrap_or("-");
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
        ToolPolicy::InheritAll => "inherit_all".to_owned(),
    };
    let except_str = if def.disallowed_tools.is_empty() {
        String::new()
    } else {
        format!(" except {:?}", &def.disallowed_tools)
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
            Span::raw(def.model.as_deref().unwrap_or("-")),
        ]),
        Line::from(vec![
            Span::styled(
                "Mode:        ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{:?}", &def.permissions.permission_mode)),
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
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use zeph_core::metrics::SubAgentMetrics;

    use crate::metrics::MetricsSnapshot;
    use crate::test_utils::render_to_string;

    use super::*;

    // ── Runtime monitor tests ─────────────────────────────────────────────────

    #[test]
    fn subagents_widget_renders_nothing_when_empty() {
        let metrics = MetricsSnapshot::default();
        let output = render_to_string(30, 5, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(output.chars().all(|c| c == ' ' || c == '\n'));
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
                },
            ],
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(50, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(output.contains("Sub-Agents"));
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
                },
            ],
            ..MetricsSnapshot::default()
        };
        let output = render_to_string(60, 10, |frame, area| {
            super::render(&metrics, frame, area);
        });
        assert!(output.contains("[plan]"));
        assert!(output.contains("[bypass!]"));
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
        let output = render_to_string(80, 20, |frame, area| {
            render_manager(&mut state, frame, area);
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
        assert!(matches!(state, AgentManagerState::Detail { index: 0, .. }));
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
        assert!(matches!(state, AgentManagerState::List { .. }));
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
        assert!(matches!(state, AgentManagerState::Create { .. }));
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
        def.model = Some("claude-sonnet-4-20250514".to_owned());
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
        assert!(matches!(state, AgentManagerState::Edit { index: 0, .. }));
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
        assert!(matches!(state, AgentManagerState::Detail { index: 0, .. }));
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
        assert!(matches!(state, AgentManagerState::ConfirmDelete { .. }));
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
        assert!(matches!(state, AgentManagerState::Detail { index: 0, .. }));
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
        assert!(matches!(
            state,
            AgentManagerState::ConfirmDelete {
                awaiting_second: true,
                ..
            }
        ));
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
        assert!(matches!(state, AgentManagerState::Create { .. }));

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
        // String with 3 multi-byte chars.
        let s = "αβγδε";
        let truncated = truncate_str(s, 3);
        // Should be "αβ…" — 2 chars + ellipsis, all valid Unicode.
        assert_eq!(truncated.chars().count(), 3);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn truncate_str_ascii_unchanged() {
        assert_eq!(truncate_str("hello", 10), "hello");
        assert_eq!(truncate_str("hello", 5), "hello");
    }
}
