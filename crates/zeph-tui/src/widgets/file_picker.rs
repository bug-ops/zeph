use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::file_picker::FilePickerState;
use crate::theme::Theme;

pub fn render(state: &FilePickerState, frame: &mut Frame, input_area: Rect) {
    let match_count = state.matches().len();
    let visible_items = u16::try_from(match_count.min(10)).unwrap_or(10);
    // border top + query line + border bottom = 3 overhead; items in between
    let height = visible_items + 3;
    let y = input_area.y.saturating_sub(height);
    let popup = Rect::new(input_area.x, y, input_area.width, height);

    frame.render_widget(Clear, popup);

    let theme = Theme::default();

    // Split popup: first line for query, rest for list
    let query_area = Rect::new(popup.x + 1, popup.y + 1, popup.width.saturating_sub(2), 1);
    let list_area = Rect::new(
        popup.x + 1,
        popup.y + 2,
        popup.width.saturating_sub(2),
        visible_items,
    );

    // Outer block
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.panel_border)
        .title(" Files ")
        .title_style(theme.panel_title);
    frame.render_widget(block, popup);

    // Query line
    let query_text = format!("> {}", state.query);
    let query_para = Paragraph::new(Span::styled(query_text, theme.highlight));
    frame.render_widget(query_para, query_area);

    // File list — borrow path strings to avoid allocation per render frame
    let items: Vec<ListItem> = state
        .matches()
        .iter()
        .map(|m| ListItem::new(Line::from(Span::raw(m.path.as_str()))))
        .collect();

    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let list = List::new(items)
        .highlight_style(selected_style)
        .highlight_symbol("> ");

    let mut list_state = ListState::default();
    if match_count > 0 {
        list_state.select(Some(state.selected));
    }

    frame.render_stateful_widget(list, list_area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use std::fs;

    use insta::assert_snapshot;

    use crate::file_picker::{FileIndex, FilePickerState};
    use crate::test_utils::render_to_string;

    fn make_state(files: &[&str], query: &str) -> (FilePickerState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        for &f in files {
            let path = dir.path().join(f);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, "").unwrap();
        }
        let idx = FileIndex::build(dir.path());
        let mut state = FilePickerState::new(&idx);
        if !query.is_empty() {
            state.update_query(query);
        }
        (state, dir)
    }

    #[test]
    fn file_picker_empty_query_snapshot() {
        let (state, _dir) = make_state(&["src/main.rs", "src/lib.rs", "README.md"], "");
        let input_area = ratatui::layout::Rect::new(0, 15, 60, 3);
        let output = render_to_string(60, 20, |frame, _area| {
            super::render(&state, frame, input_area);
        });
        assert_snapshot!(output);
    }

    #[test]
    fn file_picker_with_query_snapshot() {
        let (mut state, _dir) = make_state(&["src/main.rs", "src/lib.rs", "README.md"], "");
        state.update_query("main");
        let input_area = ratatui::layout::Rect::new(0, 15, 60, 3);
        let output = render_to_string(60, 20, |frame, _area| {
            super::render(&state, frame, input_area);
        });
        assert_snapshot!(output);
    }
}
