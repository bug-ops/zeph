// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
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
    /// Checked state of each entry of `MultiSelect` fields (empty for other fields). The cursor
    /// of a `MultiSelect` field lives in `enum_selections`.
    pub multi_marks: Vec<Vec<bool>>,
}

impl ElicitationDialogState {
    /// Create initial state from an `ElicitationRequest`.
    #[must_use]
    pub fn new(request: ElicitationRequest) -> Self {
        let n = request.fields.len();
        // Initialize all boolean fields to false (unchecked by default)
        let bool_values = vec![false; n];
        let multi_marks = request
            .fields
            .iter()
            .map(|f| match &f.field_type {
                ElicitationFieldType::MultiSelect(opts) => vec![false; opts.len()],
                _ => Vec::new(),
            })
            .collect();
        Self {
            request,
            field_idx: 0,
            text_buffers: vec![String::new(); n],
            enum_selections: vec![0; n],
            bool_values,
            enum_scroll: vec![0; n],
            multi_marks,
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

    /// Toggle the current field: the boolean value, or the mark under the cursor of a
    /// `MultiSelect` list.
    pub fn toggle_bool(&mut self) {
        let idx = self.field_idx;
        if idx >= self.request.fields.len() {
            return;
        }
        if matches!(
            self.request.fields[idx].field_type,
            ElicitationFieldType::MultiSelect(_)
        ) {
            let cursor = self.enum_selections[idx];
            if let Some(mark) = self.multi_marks[idx].get_mut(cursor) {
                *mark = !*mark;
            }
        } else {
            self.bool_values[idx] = !self.bool_values[idx];
        }
    }

    /// Move enum selection down for current field.
    pub fn enum_next(&mut self) {
        let idx = self.field_idx;
        if idx >= self.request.fields.len() {
            return;
        }
        if let ElicitationFieldType::Enum(opts) | ElicitationFieldType::MultiSelect(opts) =
            &self.request.fields[idx].field_type
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
        if let ElicitationFieldType::Enum(opts) | ElicitationFieldType::MultiSelect(opts) =
            &self.request.fields[idx].field_type
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
                // An empty array is submitted as is: the question owner rejects it and asks again,
                // so Enter on an empty list never silently cancels the dialog.
                ElicitationFieldType::MultiSelect(opts) => serde_json::Value::Array(
                    opts.iter()
                        .zip(&self.multi_marks[i])
                        .filter(|(_, marked)| **marked)
                        .map(|(opt, _)| serde_json::Value::String(sanitize_field_value(opt)))
                        .collect(),
                ),
                _ => continue,
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

/// Rows `text` occupies when wrapped at `width` columns (blank lines count as one row).
fn wrapped_rows(text: &str, width: u16) -> u16 {
    let width = usize::from(width.max(1));
    let rows: usize = text
        .split('\n')
        .map(|line| line.chars().count().max(1).div_ceil(width))
        .sum();
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// Rows a field occupies: a checkbox list needs one row per entry, anything else two.
fn field_height(field_type: &ElicitationFieldType) -> u16 {
    match field_type {
        ElicitationFieldType::MultiSelect(opts) => {
            u16::try_from(opts.len()).unwrap_or(u16::MAX).max(2)
        }
        _ => 2,
    }
}

fn fields_total_height(request: &ElicitationRequest) -> u16 {
    request
        .fields
        .iter()
        .map(|f| field_height(&f.field_type))
        .fold(0u16, u16::saturating_add)
}

/// Render the elicitation modal dialog over the full screen area.
pub fn render(state: &ElicitationDialogState, frame: &mut Frame, area: Rect, theme: &Theme) {
    // The message gets as many rows as its wrapped text needs (at least 2), so a long header
    // line cannot push the question and the option list out of view.
    let message = sanitize_field_value(&state.request.message);
    let inner_width = centered_rect(70, 1, area).width.saturating_sub(2);
    let message_rows = wrapped_rows(&message, inner_width).max(2);

    // Height: 2 (borders) + 1 (title) + message + 1 (blank) + n*2 (fields) + 2 (buttons)
    let max_height = area.height.saturating_sub(2);
    let height = 6u16
        .saturating_add(message_rows)
        .saturating_add(fields_total_height(&state.request))
        .min(max_height);
    let popup = centered_rect(70, height, area);

    frame.render_widget(Clear, popup);

    let server_name = sanitize_field_value(&state.request.server_name);
    let title = format!(" Elicitation: {server_name} ");

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.panel_border)
        .title(title.as_str())
        .title_alignment(Alignment::Center);

    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    // Split inner area: message, fields, buttons
    let field_rows = fields_total_height(&state.request);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(message_rows), // server message
            Constraint::Length(1),            // blank
            Constraint::Min(field_rows),      // fields
            Constraint::Length(1),            // blank
            Constraint::Length(1),            // hint line
        ])
        .split(inner);

    // Server message
    let msg_para = Paragraph::new(message.as_str())
        .style(theme.assistant_message)
        .wrap(Wrap { trim: false });
    frame.render_widget(msg_para, chunks[0]);

    // Fields
    if !state.request.fields.is_empty() {
        render_fields(state, frame, chunks[2], theme);
    }

    // Key hint
    let has_checklist = state
        .request
        .fields
        .iter()
        .any(|f| matches!(f.field_type, ElicitationFieldType::MultiSelect(_)));
    let mut hint_spans = Vec::new();
    if has_checklist {
        hint_spans.extend([
            Span::styled("[Up/Down]", theme.highlight),
            Span::raw(" move  "),
            Span::styled("[Space]", theme.highlight),
            Span::raw(" toggle  "),
        ]);
    }
    hint_spans.extend([
        Span::styled("[Tab]", theme.highlight),
        Span::raw(" next  "),
        Span::styled("[Enter]", theme.highlight),
        Span::raw(" submit  "),
        Span::styled("[Esc]", theme.highlight),
        Span::raw(" cancel"),
    ]);
    let hint = Line::from(hint_spans);
    let hint_para = Paragraph::new(hint).alignment(Alignment::Center);
    frame.render_widget(hint_para, chunks[4]);
}

/// One line per checklist entry (`> [x] label`), cut with an ellipsis at `width` columns so a
/// long label never wraps into the next entry's row. `cursor` is the entry under the cursor.
fn checklist_lines(
    opts: &[String],
    marks: &[bool],
    cursor: Option<usize>,
    width: usize,
) -> Vec<Line<'static>> {
    opts.iter()
        .zip(marks)
        .enumerate()
        .map(|(j, (opt, marked))| {
            let on_cursor = cursor == Some(j);
            let text = format!(
                "{} [{}] {}",
                if on_cursor { '>' } else { ' ' },
                if *marked { 'x' } else { ' ' },
                sanitize_field_value(opt).replace(['\n', '\t'], " ")
            );
            let style = if on_cursor {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(truncate_with_ellipsis(&text, width), style))
        })
        .collect()
}

fn truncate_with_ellipsis(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

fn render_fields(state: &ElicitationDialogState, frame: &mut Frame, area: Rect, theme: &Theme) {
    let n = state.request.fields.len();
    if n == 0 || area.height == 0 {
        return;
    }

    let constraints: Vec<Constraint> = state
        .request
        .fields
        .iter()
        .map(|f| Constraint::Length(field_height(&f.field_type)))
        .collect();
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
            // MultiSelect is drawn line by line below, not as one string.
            _ => String::new(),
        };

        // Split row: label on left, value on right
        #[expect(
            clippy::cast_possible_truncation,
            reason = "label_width narrows a value whose domain-specific range is bounded at this conversion site"
        )]
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
        let value_text = match &field.field_type {
            ElicitationFieldType::MultiSelect(opts) => Text::from(checklist_lines(
                opts,
                &state.multi_marks[i],
                is_focused.then_some(state.enum_selections[i]),
                usize::from(field_chunks[1].width),
            )),
            _ => Text::from(Span::styled(value_str, value_style)),
        };
        frame.render_widget(Paragraph::new(value_text), field_chunks[1]);
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

    #[test]
    fn elicitation_modal_rounded_border_snapshot() {
        use insta::assert_snapshot;

        use crate::test_utils::render_to_string;

        let req = make_request(vec![
            string_field("username", true),
            ElicitationField {
                name: "agree".to_owned(),
                description: None,
                field_type: ElicitationFieldType::Boolean,
                required: false,
            },
        ]);
        let state = ElicitationDialogState::new(req);
        let theme = crate::theme::Theme::default();
        let output = render_to_string(80, 24, |frame, area| {
            super::render(&state, frame, area, &theme);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn wrapped_rows_counts_wrapping_and_blank_lines() {
        assert_eq!(wrapped_rows("", 10), 1);
        assert_eq!(wrapped_rows("0123456789", 10), 1);
        assert_eq!(wrapped_rows("01234567890", 10), 2);
        assert_eq!(wrapped_rows("a\n\nb", 10), 3);
    }

    #[test]
    fn a_long_message_keeps_the_question_and_the_options_visible() {
        use crate::test_utils::render_to_string;

        let long_header = "Question from the assistant - never share passwords, API keys or other \
                           secrets";
        let req = ElicitationRequest {
            server_name: "assistant question".to_owned(),
            message: format!("{long_header}\n\nWhich color?\n1. red\n2. green"),
            fields: vec![string_field("answer", true)],
        };
        let state = ElicitationDialogState::new(req);
        let theme = crate::theme::Theme::default();
        let output = render_to_string(100, 30, |frame, area| {
            super::render(&state, frame, area, &theme);
        });
        for needle in ["Which color?", "1. red", "2. green"] {
            assert!(output.contains(needle), "{needle} not visible:\n{output}");
        }
    }

    fn multi_select_request(names: &[&str]) -> ElicitationRequest {
        make_request(vec![ElicitationField {
            name: "answer".to_owned(),
            description: None,
            field_type: ElicitationFieldType::MultiSelect(
                names.iter().map(|s| (*s).to_owned()).collect(),
            ),
            required: true,
        }])
    }

    #[test]
    fn multi_select_toggles_the_mark_under_the_cursor() {
        let mut state =
            ElicitationDialogState::new(multi_select_request(&["1. a", "2. b", "3. c"]));
        state.toggle_bool();
        state.enum_next();
        state.enum_next();
        state.toggle_bool();
        let val = state.build_submission().unwrap();
        assert_eq!(val["answer"], serde_json::json!(["1. a", "3. c"]));
        state.toggle_bool();
        let val = state.build_submission().unwrap();
        assert_eq!(val["answer"], serde_json::json!(["1. a"]));
    }

    #[test]
    fn multi_select_cursor_wraps_and_text_input_is_ignored() {
        let mut state = ElicitationDialogState::new(multi_select_request(&["1. a", "2. b"]));
        state.enum_prev();
        assert_eq!(state.enum_selections[0], 1);
        state.enum_next();
        assert_eq!(state.enum_selections[0], 0);
        state.push_char('x');
        assert_eq!(state.text_buffers[0], "");
    }

    #[test]
    fn multi_select_with_nothing_marked_submits_an_empty_array() {
        let state = ElicitationDialogState::new(multi_select_request(&["1. a", "2. b"]));
        assert_eq!(
            state.build_submission().unwrap()["answer"],
            serde_json::json!([])
        );
    }

    #[test]
    fn multi_select_renders_each_entry_on_its_own_row_with_a_cursor() {
        use crate::test_utils::render_to_string;

        let mut state =
            ElicitationDialogState::new(multi_select_request(&["1. red", "2. green", "3. blue"]));
        state.toggle_bool();
        state.enum_next();
        let theme = crate::theme::Theme::default();
        let output = render_to_string(80, 24, |frame, area| {
            super::render(&state, frame, area, &theme);
        });
        let rows: Vec<&str> = output.lines().collect();
        let red = rows.iter().position(|l| l.contains("[x] 1. red")).unwrap();
        assert!(rows[red].contains("  [x] 1. red"), "{output}");
        assert!(rows[red + 1].contains("> [ ] 2. green"), "{output}");
        assert!(rows[red + 2].contains("  [ ] 3. blue"), "{output}");
        assert!(
            !rows[red].contains("2. green"),
            "entries share a row:\n{output}"
        );
        assert!(output.contains("[Space]"), "{output}");
    }

    #[test]
    fn a_long_checklist_entry_is_cut_with_an_ellipsis_and_stays_on_its_row() {
        use crate::test_utils::render_to_string;

        let long = format!("1. {}", "x".repeat(200));
        let state = ElicitationDialogState::new(multi_select_request(&[&long, "2. short"]));
        let theme = crate::theme::Theme::default();
        let output = render_to_string(80, 24, |frame, area| {
            super::render(&state, frame, area, &theme);
        });
        let rows: Vec<&str> = output.lines().collect();
        let first = rows.iter().position(|l| l.contains("[ ] 1. xxx")).unwrap();
        assert!(rows[first].contains('\u{2026}'), "{output}");
        assert!(rows[first + 1].contains("[ ] 2. short"), "{output}");
    }

    #[test]
    fn truncate_with_ellipsis_respects_width_and_chars() {
        assert_eq!(truncate_with_ellipsis("abc", 3), "abc");
        assert_eq!(truncate_with_ellipsis("abcd", 3), "ab\u{2026}");
        assert_eq!(
            truncate_with_ellipsis("\u{e9}\u{e9}\u{e9}\u{e9}", 2),
            "\u{e9}\u{2026}"
        );
        assert_eq!(truncate_with_ellipsis("abc", 0), "\u{2026}");
    }
}
