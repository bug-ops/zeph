use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::event::{AgentEvent, AppEvent};
use crate::layout::AppLayout;
use crate::metrics::MetricsSnapshot;
use crate::theme::Theme;
use crate::widgets;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Insert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub streaming: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Chat,
    Skills,
    Memory,
    Resources,
}

pub struct App {
    input: String,
    cursor_position: usize,
    input_mode: InputMode,
    messages: Vec<ChatMessage>,
    scroll_offset: usize,
    pub metrics: MetricsSnapshot,
    active_panel: Panel,
    pub should_quit: bool,
    user_input_tx: mpsc::Sender<String>,
    agent_event_rx: mpsc::Receiver<AgentEvent>,
}

impl App {
    #[must_use]
    pub fn new(
        user_input_tx: mpsc::Sender<String>,
        agent_event_rx: mpsc::Receiver<AgentEvent>,
    ) -> Self {
        Self {
            input: String::new(),
            cursor_position: 0,
            input_mode: InputMode::Insert,
            messages: Vec::new(),
            scroll_offset: 0,
            metrics: MetricsSnapshot::default(),
            active_panel: Panel::Chat,
            should_quit: false,
            user_input_tx,
            agent_event_rx,
        }
    }

    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    #[must_use]
    pub fn input_mode(&self) -> InputMode {
        self.input_mode
    }

    #[must_use]
    pub fn cursor_position(&self) -> usize {
        self.cursor_position
    }

    #[must_use]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// # Errors
    ///
    /// Returns an error if event handling fails.
    pub fn handle_event(&mut self, event: AppEvent) -> anyhow::Result<()> {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Tick | AppEvent::Resize(_, _) => {}
            AppEvent::Agent(agent_event) => self.handle_agent_event(agent_event),
        }
        Ok(())
    }

    pub fn poll_agent_event(&mut self) -> impl Future<Output = Option<AgentEvent>> + use<'_> {
        self.agent_event_rx.recv()
    }

    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Chunk(text) => {
                if let Some(last) = self.messages.last_mut()
                    && last.role == MessageRole::Assistant
                    && last.streaming
                {
                    last.content.push_str(&text);
                } else {
                    self.messages.push(ChatMessage {
                        role: MessageRole::Assistant,
                        content: text,
                        streaming: true,
                    });
                }
                self.scroll_offset = 0;
            }
            AgentEvent::FullMessage(text) => {
                self.messages.push(ChatMessage {
                    role: MessageRole::Assistant,
                    content: text,
                    streaming: false,
                });
                self.scroll_offset = 0;
            }
            AgentEvent::Flush => {
                if let Some(last) = self.messages.last_mut()
                    && last.streaming
                {
                    last.streaming = false;
                }
            }
            AgentEvent::Typing => {}
            AgentEvent::MetricsUpdate(snapshot) => {
                self.metrics = snapshot;
            }
        }
    }

    pub fn draw(&self, frame: &mut ratatui::Frame) {
        let layout = AppLayout::compute(frame.area());

        self.draw_header(frame, layout.header);
        widgets::chat::render(self, frame, layout.chat);
        self.draw_side_panel(frame, &layout);
        widgets::input::render(self, frame, layout.input);
        widgets::status::render(self, &self.metrics, frame, layout.status);
    }

    fn draw_header(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
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

        let text = format!(
            " Zeph v{} | Provider: {provider} | Model: {model}",
            env!("CARGO_PKG_VERSION")
        );

        let line = Line::from(Span::styled(text, theme.header));
        let paragraph = Paragraph::new(line).style(theme.header);
        frame.render_widget(paragraph, area);
    }

    fn draw_side_panel(&self, frame: &mut ratatui::Frame, layout: &AppLayout) {
        use ratatui::text::Line;
        use ratatui::widgets::{Block, Borders, Paragraph};

        let theme = Theme::default();

        // Skills panel
        let skill_lines: Vec<Line<'_>> = self
            .metrics
            .active_skills
            .iter()
            .map(|s| Line::from(format!("  - {s}")))
            .collect();
        let skills = Paragraph::new(skill_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(format!(
                    " Skills ({}/{}) ",
                    self.metrics.active_skills.len(),
                    self.metrics.total_skills
                )),
        );
        frame.render_widget(skills, layout.skills);

        // Memory panel
        let mem_lines = vec![
            Line::from(format!(
                "  SQLite: {} msgs",
                self.metrics.sqlite_message_count
            )),
            Line::from(format!(
                "  Qdrant: {}",
                if self.metrics.qdrant_available {
                    "connected"
                } else {
                    "---"
                }
            )),
            Line::from(format!(
                "  Conv ID: {}",
                self.metrics
                    .sqlite_conversation_id
                    .map_or_else(|| "---".to_string(), |id| id.to_string())
            )),
        ];
        let memory = Paragraph::new(mem_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(" Memory "),
        );
        frame.render_widget(memory, layout.memory);

        // Resources panel
        let res_lines = vec![
            Line::from(format!("  Tokens: {}", self.metrics.total_tokens)),
            Line::from(format!("  API calls: {}", self.metrics.api_calls)),
            Line::from(format!("  Latency: {}ms", self.metrics.last_llm_latency_ms)),
        ];
        let resources = Paragraph::new(res_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(" Resources "),
        );
        frame.render_widget(resources, layout.resources);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match self.input_mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Insert => self.handle_insert_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('i') => self.input_mode = InputMode::Insert,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
            }
            KeyCode::Home => {
                self.scroll_offset = self.messages.len();
            }
            KeyCode::End => {
                self.scroll_offset = 0;
            }
            KeyCode::Tab => {
                self.active_panel = match self.active_panel {
                    Panel::Chat => Panel::Skills,
                    Panel::Skills => Panel::Memory,
                    Panel::Memory => Panel::Resources,
                    Panel::Resources => Panel::Chat,
                };
            }
            _ => {}
        }
    }

    /// Returns the byte offset of the char at the given char index.
    fn byte_offset_of_char(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map_or(self.input.len(), |(i, _)| i)
    }

    fn char_count(&self) -> usize {
        self.input.chars().count()
    }

    fn handle_insert_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.submit_input(),
            KeyCode::Esc => self.input_mode = InputMode::Normal,
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    let byte_offset = self.byte_offset_of_char(self.cursor_position - 1);
                    self.input.remove(byte_offset);
                    self.cursor_position -= 1;
                }
            }
            KeyCode::Delete => {
                if self.cursor_position < self.char_count() {
                    let byte_offset = self.byte_offset_of_char(self.cursor_position);
                    self.input.remove(byte_offset);
                }
            }
            KeyCode::Left => {
                self.cursor_position = self.cursor_position.saturating_sub(1);
            }
            KeyCode::Right => {
                if self.cursor_position < self.char_count() {
                    self.cursor_position += 1;
                }
            }
            KeyCode::Home => self.cursor_position = 0,
            KeyCode::End => self.cursor_position = self.char_count(),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear();
                self.cursor_position = 0;
            }
            KeyCode::Char(c) => {
                let byte_offset = self.byte_offset_of_char(self.cursor_position);
                self.input.insert(byte_offset, c);
                self.cursor_position += 1;
            }
            _ => {}
        }
    }

    fn submit_input(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.messages.push(ChatMessage {
            role: MessageRole::User,
            content: text.clone(),
            streaming: false,
        });
        self.input.clear();
        self.cursor_position = 0;
        self.scroll_offset = 0;

        // Non-blocking send; if channel full, message is dropped
        let _ = self.user_input_tx.try_send(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> (App, mpsc::Receiver<String>, mpsc::Sender<AgentEvent>) {
        let (user_tx, user_rx) = mpsc::channel(16);
        let (agent_tx, agent_rx) = mpsc::channel(16);
        let app = App::new(user_tx, agent_rx);
        (app, user_rx, agent_tx)
    }

    #[test]
    fn initial_state() {
        let (app, _rx, _tx) = make_app();
        assert!(app.input().is_empty());
        assert_eq!(app.input_mode(), InputMode::Insert);
        assert!(app.messages().is_empty());
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_c_quits() {
        let (mut app, _rx, _tx) = make_app();
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.should_quit);
    }

    #[test]
    fn insert_mode_typing() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert_eq!(app.input(), "a");
        assert_eq!(app.cursor_position(), 1);
    }

    #[test]
    fn escape_switches_to_normal() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert_eq!(app.input_mode(), InputMode::Normal);
    }

    #[test]
    fn i_enters_insert_mode() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        let key = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert_eq!(app.input_mode(), InputMode::Insert);
    }

    #[test]
    fn q_quits_in_normal_mode() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.should_quit);
    }

    #[test]
    fn backspace_deletes_char() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "ab".into();
        app.cursor_position = 2;
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert_eq!(app.input(), "a");
        assert_eq!(app.cursor_position(), 1);
    }

    #[test]
    fn enter_submits_input() {
        let (mut app, mut rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello".into();
        app.cursor_position = 5;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.input().is_empty());
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].content, "hello");

        let sent = rx.try_recv().unwrap();
        assert_eq!(sent, "hello");
    }

    #[test]
    fn empty_enter_does_not_submit() {
        let (mut app, mut rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.messages().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_chunk_creates_streaming_message() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::Chunk("hel".into()));
        assert_eq!(app.messages().len(), 1);
        assert!(app.messages()[0].streaming);
        assert_eq!(app.messages()[0].content, "hel");

        app.handle_agent_event(AgentEvent::Chunk("lo".into()));
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].content, "hello");
    }

    #[test]
    fn agent_flush_stops_streaming() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::Chunk("test".into()));
        assert!(app.messages()[0].streaming);
        app.handle_agent_event(AgentEvent::Flush);
        assert!(!app.messages()[0].streaming);
    }

    #[test]
    fn agent_full_message() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::FullMessage("done".into()));
        assert_eq!(app.messages().len(), 1);
        assert!(!app.messages()[0].streaming);
        assert_eq!(app.messages()[0].content, "done");
    }

    #[test]
    fn scroll_in_normal_mode() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(up)).unwrap();
        assert_eq!(app.scroll_offset(), 1);

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(down)).unwrap();
        assert_eq!(app.scroll_offset(), 0);
    }

    #[test]
    fn tab_cycles_panels() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        assert_eq!(app.active_panel, Panel::Chat);

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(tab)).unwrap();
        assert_eq!(app.active_panel, Panel::Skills);

        app.handle_event(AppEvent::Key(tab)).unwrap();
        assert_eq!(app.active_panel, Panel::Memory);

        app.handle_event(AppEvent::Key(tab)).unwrap();
        assert_eq!(app.active_panel, Panel::Resources);

        app.handle_event(AppEvent::Key(tab)).unwrap();
        assert_eq!(app.active_panel, Panel::Chat);
    }

    #[test]
    fn ctrl_u_clears_input() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "some text".into();
        app.cursor_position = 9;
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.input().is_empty());
        assert_eq!(app.cursor_position(), 0);
    }

    #[test]
    fn cursor_movement() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "abc".into();
        app.cursor_position = 1;

        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(left)).unwrap();
        assert_eq!(app.cursor_position(), 0);

        // left at 0 stays at 0
        app.handle_event(AppEvent::Key(left)).unwrap();
        assert_eq!(app.cursor_position(), 0);

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(right)).unwrap();
        assert_eq!(app.cursor_position(), 1);

        let home = KeyEvent::new(KeyCode::Home, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(home)).unwrap();
        assert_eq!(app.cursor_position(), 0);

        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(end)).unwrap();
        assert_eq!(app.cursor_position(), 3);
    }

    #[test]
    fn delete_key_removes_char_at_cursor() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "abc".into();
        app.cursor_position = 1;
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert_eq!(app.input(), "ac");
        assert_eq!(app.cursor_position(), 1);
    }

    #[test]
    fn metrics_update_event() {
        let (mut app, _rx, _tx) = make_app();
        let mut m = MetricsSnapshot::default();
        m.api_calls = 42;
        app.handle_agent_event(AgentEvent::MetricsUpdate(m));
        assert_eq!(app.metrics.api_calls, 42);
    }

    #[test]
    fn unicode_input_insert_and_delete() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;

        // Type multi-byte chars
        for c in "\u{00e9}a\u{1f600}".chars() {
            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            app.handle_event(AppEvent::Key(key)).unwrap();
        }
        assert_eq!(app.input(), "\u{00e9}a\u{1f600}");
        assert_eq!(app.cursor_position(), 3);

        // Backspace removes the emoji (last char)
        let bs = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(bs)).unwrap();
        assert_eq!(app.input(), "\u{00e9}a");
        assert_eq!(app.cursor_position(), 2);

        // Move cursor left and delete 'a'
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(left)).unwrap();
        assert_eq!(app.cursor_position(), 1);

        let del = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(del)).unwrap();
        assert_eq!(app.input(), "\u{00e9}");
        assert_eq!(app.cursor_position(), 1);

        // End key uses char count, not byte count
        let end = KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(end)).unwrap();
        assert_eq!(app.cursor_position(), 1);
    }
}
