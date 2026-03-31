// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use zeph_core::channel::{ElicitationFieldType, ElicitationRequest};

use crate::layout::centered_rect;
use crate::theme::Theme;

/// Interactive state for the elicitation modal dialog.
pub struct ElicitationDialogState {
    pub request: ElicitationRequest,
    /// Index of the currently focused field.
    pub field_idx: usize,
    /// Current text buffer for each string/number/integer field.
    pub text_buffers: Vec<String>,
    /// Selected option index for Enum fields.
    pub enum_selections: Vec<usize>,
    /// Toggle state for Boolean fields.
    pub bool_values: Vec<bool>,
    /// Scroll offset for long enum lists.
    pub enum_scroll: Vec<usize>,
}

impl ElicitationDialogState {
    /// Create initial state from an `ElicitationRequest`.
    #[must_use]
    pub fn new(request: ElicitationRequest) -> Self {
        let n = request.fields.len();
        // Initialize all boolean fields to false (unchecked by default)
        let bool_values = vec![false; n];
        Self {
            request,
            field_idx: 0,
            text_buffers: vec![String::new(); n],
            enum_selections: vec![0; n],
            bool_values,
            enum_scroll: vec![0; n],
        }
    }

    /// Move focus to the next field (wraps).
    pub fn next_field(&mut self) {
        if self.request.fields.is_empty() {
            return;
        }
        self.field_idx = (self.field_idx + 1) % self.request.fields.len();
    }

    /// Move focus to the previous field (wraps).
    pub fn prev_field(&mut self) {
        if self.request.fields.is_empty() {
            return;
        }
        let n = self.request.fields.len();
        self.field_idx = (self.field_idx + n - 1) % n;
    }

    /// Handle a character input for the current field.
    pub fn push_char(&mut self, c: char) {
        let idx = self.field_idx;
        if idx >= self.request.fields.len() {
            return;
        }
        if matches!(
            self.request.fields[idx].field_type,
            ElicitationFieldType::String
                | ElicitationFieldType::Integer
                | ElicitationFieldType::Number
        ) {
            self.text_buffers[idx].push(c);
        }
        // Boolean and Enum fields do not accept character input
    }

    /// Delete last char in the current text buffer.
    pub fn pop_char(&mut self) {
        let idx = self.field_idx;
        if idx < self.text_buffers.len() {
            self.text_buffers[idx].pop();
        }
    }

    /// Toggle boolean for current field.
    pub fn toggle_bool(&mut self) {
        let idx = self.field_idx;
        if idx < self.bool_values.len() {
            self.bool_values[idx] = !self.bool_values[idx];
        }
    }

    /// Move enum selection down for current field.
    pub fn enum_next(&mut self) {
        let idx = self.field_idx;
        if idx >= self.request.fields.len() {
            return;
        }
        if let ElicitationFieldType::Enum(ref opts) = self.request.fields[idx].field_type
            && !opts.is_empty()
        {
            self.enum_selections[idx] = (self.enum_selections[idx] + 1) % opts.len();
        }
    }

    /// Move enum selection up for current field.
    pub fn enum_prev(&mut self) {
        let idx = self.field_idx;
        if idx >= self.request.fields.len() {
            return;
        }
        if let ElicitationFieldType::Enum(ref opts) = self.request.fields[idx].field_type
            && !opts.is_empty()
        {
            let n = opts.len();
            self.enum_selections[idx] = (self.enum_selections[idx] + n - 1) % n;
        }
    }

    /// Build the JSON value to submit. Returns `None` if a required field is empty.
    #[must_use]
    pub fn build_submission(&self) -> Option<serde_json::Value> {
        let mut map = serde_json::Map::new();
        for (i, field) in self.request.fields.iter().enumerate() {
            let value = match &field.field_type {
                ElicitationFieldType::String => {
                    let v = sanitize_field_value(&self.text_buffers[i]);
                    if v.is_empty() && field.required {
                        return None;
                    }
                    serde_json::Value::String(v)
                }
                ElicitationFieldType::Integer => {
                    let v = self.text_buffers[i].trim();
                    if v.is_empty() {
                        if field.required {
                            return None;
                        }
                        continue;
                    }
                    let n: i64 = v.parse().ok()?;
                    serde_json::Value::Number(n.into())
                }
                ElicitationFieldType::Number => {
                    let v = self.text_buffers[i].trim();
                    if v.is_empty() {
                        if field.required {
                            return None;
                        }
                        continue;
                    }
                    let n: f64 = v.parse().ok()?;
                    serde_json::Value::Number(
                        serde_json::Number::from_f64(n).unwrap_or(0i64.into()),
                    )
                }
                ElicitationFieldType::Boolean => serde_json::Value::Bool(self.bool_values[i]),
                ElicitationFieldType::Enum(opts) => {
                    let sel = self.enum_selections[i];
                    if opts.is_empty() {
                        if field.required {
                            return None;
                        }
                        continue;
                    }
                    serde_json::Value::String(sanitize_field_value(&opts[sel]))
                }
            };
            map.insert(sanitize_field_name(&field.name), value);
        }
        Some(serde_json::Value::Object(map))
    }
}

/// Strip ANSI escape sequences and ASCII control characters from field values.
/// This prevents terminal injection via malicious MCP server responses (critique risk).
fn sanitize_field_value(s: &str) -> String {
    // Remove ANSI escape sequences (\x1b[...m)
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until we find a letter (end of ANSI sequence)
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if c.is_control() && c != '\n' && c != '\t' {
            // Drop control characters except newline and tab
        } else {
            out.push(c);
        }
    }
    out
}

/// Sanitize a field name for use as a JSON key.
fn sanitize_field_name(s: &str) -> String {
    sanitize_field_value(s)
}

/// Render the elicitation modal dialog over the full screen area.
pub fn render(state: &ElicitationDialogState, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();
    let n_fields = state.request.fields.len();

    // Height: 2 (borders) + 1 (title) + 1 (message) + 1 (blank) + n*2 (fields) + 2 (buttons)
    let max_height = area.height.saturating_sub(2);
    #[allow(clippy::cast_possible_truncation)]
    let height = ((8 + n_fields * 2) as u16).min(max_height);
    let popup = centered_rect(70, height, area);

    frame.render_widget(Clear, popup);

    let server_name = sanitize_field_value(&state.request.server_name);
    let title = format!(" MCP Elicitation: {server_name} ");

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(title.as_str())
        .title_alignment(Alignment::Center);

    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    // Split inner area: message, fields, buttons
    #[allow(clippy::cast_possible_truncation)]
    let field_rows = (n_fields as u16).saturating_mul(2);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),       // server message
            Constraint::Length(1),       // blank
            Constraint::Min(field_rows), // fields
            Constraint::Length(1),       // blank
            Constraint::Length(1),       // hint line
        ])
        .split(inner);

    // Server message
    let message = sanitize_field_value(&state.request.message);
    let msg_para = Paragraph::new(message.as_str())
        .style(theme.assistant_message)
        .wrap(Wrap { trim: false });
    frame.render_widget(msg_para, chunks[0]);

    // Fields
    if !state.request.fields.is_empty() {
        render_fields(state, frame, chunks[2], &theme);
    }

    // Key hint
    let hint = Line::from(vec![
        Span::styled("[Tab]", theme.highlight),
        Span::raw(" next  "),
        Span::styled("[Enter]", theme.highlight),
        Span::raw(" submit  "),
        Span::styled("[Esc]", theme.highlight),
        Span::raw(" cancel"),
    ]);
    let hint_para = Paragraph::new(hint).alignment(Alignment::Center);
    frame.render_widget(hint_para, chunks[4]);
}

fn render_fields(state: &ElicitationDialogState, frame: &mut Frame, area: Rect, theme: &Theme) {
    let n = state.request.fields.len();
    if n == 0 || area.height == 0 {
        return;
    }

    // Allocate 2 rows per field
    let constraints: Vec<Constraint> = (0..n).map(|_| Constraint::Length(2)).collect();
    let rows = Layout::vertical(constraints).split(area);

    for (i, field) in state.request.fields.iter().enumerate() {
        if i >= rows.len() {
            break;
        }
        let row = rows[i];
        let is_focused = i == state.field_idx;

        let name = sanitize_field_value(&field.name);
        let req_marker = if field.required { "*" } else { "" };
        let label = format!("{name}{req_marker}: ");

        let label_style = if is_focused {
            theme.highlight.add_modifier(Modifier::BOLD)
        } else {
            theme.panel_title
        };

        let value_str = match &field.field_type {
            ElicitationFieldType::String
            | ElicitationFieldType::Integer
            | ElicitationFieldType::Number => {
                let buf = &state.text_buffers[i];
                if is_focused {
                    format!("{buf}▌")
                } else {
                    buf.clone()
                }
            }
            ElicitationFieldType::Boolean => {
                if state.bool_values[i] {
                    "[x] Yes".to_owned()
                } else {
                    "[ ] No".to_owned()
                }
            }
            ElicitationFieldType::Enum(opts) => {
                if opts.is_empty() {
                    "(none)".to_owned()
                } else {
                    let sel = state.enum_selections[i];
                    let opt = sanitize_field_value(&opts[sel.min(opts.len() - 1)]);
                    format!("▼ {opt}")
                }
            }
        };

        // Split row: label on left, value on right
        #[allow(clippy::cast_possible_truncation)]
        let label_width = (label.len() as u16).min(row.width / 2);
        let field_chunks =
            Layout::horizontal([Constraint::Length(label_width), Constraint::Min(1)]).split(row);

        let label_para = Paragraph::new(Span::styled(label, label_style));
        frame.render_widget(label_para, field_chunks[0]);

        let value_style = if is_focused {
            Style::default().add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
        };
        let value_para = Paragraph::new(Span::styled(value_str, value_style));
        frame.render_widget(value_para, field_chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_core::channel::{ElicitationField, ElicitationFieldType, ElicitationRequest};

    fn make_request(fields: Vec<ElicitationField>) -> ElicitationRequest {
        ElicitationRequest {
            server_name: "test-server".to_owned(),
            message: "Please fill in the form".to_owned(),
            fields,
        }
    }

    fn string_field(name: &str, required: bool) -> ElicitationField {
        ElicitationField {
            name: name.to_owned(),
            description: None,
            field_type: ElicitationFieldType::String,
            required,
        }
    }

    #[test]
    fn build_submission_returns_none_when_required_string_empty() {
        let req = make_request(vec![string_field("username", true)]);
        let state = ElicitationDialogState::new(req);
        assert!(state.build_submission().is_none());
    }

    #[test]
    fn build_submission_returns_value_when_required_filled() {
        let req = make_request(vec![string_field("username", true)]);
        let mut state = ElicitationDialogState::new(req);
        state.text_buffers[0] = "alice".to_owned();
        let val = state.build_submission().unwrap();
        assert_eq!(val["username"], "alice");
    }

    #[test]
    fn build_submission_bool_default_false() {
        let req = make_request(vec![ElicitationField {
            name: "agree".to_owned(),
            description: None,
            field_type: ElicitationFieldType::Boolean,
            required: false,
        }]);
        let state = ElicitationDialogState::new(req);
        let val = state.build_submission().unwrap();
        assert_eq!(val["agree"], false);
    }

    #[test]
    fn toggle_bool_flips_value() {
        let req = make_request(vec![ElicitationField {
            name: "agree".to_owned(),
            description: None,
            field_type: ElicitationFieldType::Boolean,
            required: false,
        }]);
        let mut state = ElicitationDialogState::new(req);
        state.toggle_bool();
        let val = state.build_submission().unwrap();
        assert_eq!(val["agree"], true);
    }

    #[test]
    fn enum_selection_cycles() {
        let req = make_request(vec![ElicitationField {
            name: "color".to_owned(),
            description: None,
            field_type: ElicitationFieldType::Enum(vec![
                "red".to_owned(),
                "green".to_owned(),
                "blue".to_owned(),
            ]),
            required: false,
        }]);
        let mut state = ElicitationDialogState::new(req);
        state.enum_next();
        let val = state.build_submission().unwrap();
        assert_eq!(val["color"], "green");
        state.enum_next();
        state.enum_next();
        let val2 = state.build_submission().unwrap();
        assert_eq!(val2["color"], "red"); // wraps
    }

    #[test]
    fn sanitize_strips_ansi_escapes() {
        let s = "\x1b[31mWARNING\x1b[0m";
        assert_eq!(sanitize_field_value(s), "WARNING");
    }

    #[test]
    fn sanitize_strips_control_chars() {
        let s = "hello\x00world\x07";
        assert_eq!(sanitize_field_value(s), "helloworld");
    }

    #[test]
    fn next_field_wraps() {
        let req = make_request(vec![string_field("a", false), string_field("b", false)]);
        let mut state = ElicitationDialogState::new(req);
        state.next_field();
        assert_eq!(state.field_idx, 1);
        state.next_field();
        assert_eq!(state.field_idx, 0);
    }
}
