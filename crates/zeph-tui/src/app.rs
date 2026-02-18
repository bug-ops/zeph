use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tracing::debug;

use crate::event::{AgentEvent, AppEvent};
use crate::hyperlink::HyperlinkSpan;
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
    Tool,
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: MessageRole,
    pub content: String,
    pub streaming: bool,
    pub tool_name: Option<String>,
    pub diff_data: Option<zeph_core::DiffData>,
    pub filter_stats: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Chat,
    Skills,
    Memory,
    Resources,
}

pub struct ConfirmState {
    pub prompt: String,
    pub response_tx: Option<oneshot::Sender<bool>>,
}

#[allow(clippy::struct_excessive_bools)]
pub struct App {
    input: String,
    cursor_position: usize,
    input_mode: InputMode,
    messages: Vec<ChatMessage>,
    show_splash: bool,
    show_side_panels: bool,
    show_help: bool,
    scroll_offset: usize,
    pub metrics: MetricsSnapshot,
    metrics_rx: Option<watch::Receiver<MetricsSnapshot>>,
    active_panel: Panel,
    tool_expanded: bool,
    compact_tools: bool,
    show_source_labels: bool,
    status_label: Option<String>,
    throbber_state: throbber_widgets_tui::ThrobberState,
    confirm_state: Option<ConfirmState>,
    pub should_quit: bool,
    user_input_tx: mpsc::Sender<String>,
    agent_event_rx: mpsc::Receiver<AgentEvent>,
    input_history: Vec<String>,
    history_index: Option<usize>,
    draft_input: String,
    queued_count: usize,
    hyperlinks: Vec<HyperlinkSpan>,
    cancel_signal: Option<Arc<Notify>>,
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
            show_splash: true,
            show_side_panels: true,
            show_help: false,
            scroll_offset: 0,
            metrics: MetricsSnapshot::default(),
            metrics_rx: None,
            active_panel: Panel::Chat,
            tool_expanded: false,
            compact_tools: false,
            show_source_labels: false,
            status_label: None,
            throbber_state: throbber_widgets_tui::ThrobberState::default(),
            confirm_state: None,
            should_quit: false,
            user_input_tx,
            agent_event_rx,
            input_history: Vec::new(),
            history_index: None,
            draft_input: String::new(),
            queued_count: 0,
            hyperlinks: Vec::new(),
            cancel_signal: None,
        }
    }

    #[must_use]
    pub fn show_splash(&self) -> bool {
        self.show_splash
    }

    #[must_use]
    pub fn show_side_panels(&self) -> bool {
        self.show_side_panels
    }

    pub fn load_history(&mut self, messages: &[(&str, &str)]) {
        const TOOL_SUFFIX: &str = "\n```";

        for &(role_str, content) in messages {
            if role_str == "user"
                && let Some((tool_name, body)) = parse_tool_output(content, TOOL_SUFFIX)
            {
                self.messages.push(ChatMessage {
                    role: MessageRole::Tool,
                    content: body,
                    streaming: false,
                    tool_name: Some(tool_name),
                    diff_data: None,
                    filter_stats: None,
                });
                continue;
            }

            let role = match role_str {
                "user" => MessageRole::User,
                "assistant" => MessageRole::Assistant,
                _ => continue,
            };
            self.messages.push(ChatMessage {
                role,
                content: content.to_owned(),
                streaming: false,
                tool_name: None,
                diff_data: None,
                filter_stats: None,
            });
        }
        if !self.messages.is_empty() {
            self.show_splash = false;
        }
    }

    #[must_use]
    pub fn with_cancel_signal(mut self, signal: Arc<Notify>) -> Self {
        self.cancel_signal = Some(signal);
        self
    }

    #[must_use]
    pub fn with_metrics_rx(mut self, rx: watch::Receiver<MetricsSnapshot>) -> Self {
        self.metrics_rx = Some(rx);
        self
    }

    pub fn poll_metrics(&mut self) {
        if let Some(ref mut rx) = self.metrics_rx
            && rx.has_changed().unwrap_or(false)
        {
            self.metrics = rx.borrow_and_update().clone();
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

    #[must_use]
    pub fn tool_expanded(&self) -> bool {
        self.tool_expanded
    }

    #[must_use]
    pub fn compact_tools(&self) -> bool {
        self.compact_tools
    }

    #[must_use]
    pub fn show_source_labels(&self) -> bool {
        self.show_source_labels
    }

    pub fn set_show_source_labels(&mut self, v: bool) {
        self.show_source_labels = v;
    }

    pub fn set_hyperlinks(&mut self, links: Vec<HyperlinkSpan>) {
        self.hyperlinks = links;
    }

    pub fn take_hyperlinks(&mut self) -> Vec<HyperlinkSpan> {
        std::mem::take(&mut self.hyperlinks)
    }

    #[must_use]
    pub fn status_label(&self) -> Option<&str> {
        self.status_label.as_deref()
    }

    #[must_use]
    pub fn queued_count(&self) -> usize {
        self.queued_count
    }

    #[must_use]
    pub fn is_agent_busy(&self) -> bool {
        self.status_label.is_some() || self.messages.last().is_some_and(|m| m.streaming)
    }

    #[must_use]
    pub fn has_running_tool(&self) -> bool {
        self.messages
            .last()
            .is_some_and(|m| m.role == MessageRole::Tool && m.streaming)
    }

    #[must_use]
    pub fn throbber_state(&self) -> &throbber_widgets_tui::ThrobberState {
        &self.throbber_state
    }

    pub fn throbber_state_mut(&mut self) -> &mut throbber_widgets_tui::ThrobberState {
        &mut self.throbber_state
    }

    /// # Errors
    ///
    /// Returns an error if event handling fails.
    pub fn handle_event(&mut self, event: AppEvent) -> anyhow::Result<()> {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Tick => {
                self.throbber_state.calc_next();
            }
            AppEvent::Resize(_, _) => {}
            AppEvent::MouseScroll(delta) => {
                if self.confirm_state.is_none() {
                    if delta > 0 {
                        self.scroll_offset = self.scroll_offset.saturating_add(1);
                    } else {
                        self.scroll_offset = self.scroll_offset.saturating_sub(1);
                    }
                }
            }
            AppEvent::Agent(agent_event) => self.handle_agent_event(agent_event),
        }
        Ok(())
    }

    pub fn poll_agent_event(&mut self) -> impl Future<Output = Option<AgentEvent>> + use<'_> {
        self.agent_event_rx.recv()
    }

    #[allow(clippy::too_many_lines)]
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Chunk(text) => {
                self.status_label = None;
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
                        tool_name: None,
                        diff_data: None,
                        filter_stats: None,
                    });
                }
                self.scroll_offset = 0;
            }
            AgentEvent::FullMessage(text) => {
                self.status_label = None;
                if !text.starts_with("[tool output") {
                    self.messages.push(ChatMessage {
                        role: MessageRole::Assistant,
                        content: text,
                        streaming: false,
                        tool_name: None,
                        diff_data: None,
                        filter_stats: None,
                    });
                }
                self.scroll_offset = 0;
            }
            AgentEvent::Flush => {
                if let Some(last) = self.messages.last_mut()
                    && last.streaming
                {
                    last.streaming = false;
                }
            }
            AgentEvent::Typing => {
                self.status_label = Some("thinking...".to_owned());
            }
            AgentEvent::Status(text) => {
                self.status_label = if text.is_empty() { None } else { Some(text) };
                self.scroll_offset = 0;
            }
            AgentEvent::ToolStart { tool_name, command } => {
                self.status_label = None;
                self.messages.push(ChatMessage {
                    role: MessageRole::Tool,
                    content: format!("$ {command}\n"),
                    streaming: true,
                    tool_name: Some(tool_name),
                    diff_data: None,
                    filter_stats: None,
                });
                self.scroll_offset = 0;
            }
            AgentEvent::ToolOutputChunk { chunk, .. } => {
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Tool && m.streaming)
                {
                    msg.content.push_str(&chunk);
                }
                self.scroll_offset = 0;
            }
            AgentEvent::ToolOutput {
                tool_name,
                output,
                diff,
                filter_stats,
                ..
            } => {
                debug!(
                    %tool_name,
                    has_diff = diff.is_some(),
                    has_filter_stats = filter_stats.is_some(),
                    output_len = output.len(),
                    "TUI ToolOutput event received"
                );
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Tool && m.streaming)
                {
                    // Shell streaming path: finalize existing streaming tool message.
                    debug!("attaching diff to existing streaming Tool message");
                    msg.streaming = false;
                    msg.diff_data = diff;
                    msg.filter_stats = filter_stats;
                } else if diff.is_some() || filter_stats.is_some() {
                    // Native tool_use path: no prior ToolStart, create the message now.
                    debug!("creating new Tool message with diff (native path)");
                    self.messages.push(ChatMessage {
                        role: MessageRole::Tool,
                        content: output,
                        streaming: false,
                        tool_name: Some(tool_name),
                        diff_data: diff,
                        filter_stats,
                    });
                } else if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Tool)
                {
                    msg.filter_stats = filter_stats;
                }
                self.scroll_offset = 0;
            }
            AgentEvent::ConfirmRequest {
                prompt,
                response_tx,
            } => {
                self.confirm_state = Some(ConfirmState {
                    prompt,
                    response_tx: Some(response_tx),
                });
            }
            AgentEvent::QueueCount(count) => {
                self.queued_count = count;
            }
            AgentEvent::DiffReady(diff) => {
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::Tool)
                {
                    msg.diff_data = Some(diff);
                }
            }
        }
    }

    #[must_use]
    pub fn confirm_state(&self) -> Option<&ConfirmState> {
        self.confirm_state.as_ref()
    }

    pub fn draw(&mut self, frame: &mut ratatui::Frame) {
        let layout = AppLayout::compute(frame.area(), self.show_side_panels);

        self.draw_header(frame, layout.header);
        if self.show_splash {
            widgets::splash::render(frame, layout.chat);
        } else {
            let max_scroll = widgets::chat::render(self, frame, layout.chat);
            self.scroll_offset = self.scroll_offset.min(max_scroll);
        }
        self.draw_side_panel(frame, &layout);
        widgets::chat::render_activity(self, frame, layout.activity);
        widgets::input::render(self, frame, layout.input);
        widgets::status::render(self, &self.metrics, frame, layout.status);

        if let Some(state) = &self.confirm_state {
            widgets::confirm::render(&state.prompt, frame, frame.area());
        }

        if self.show_help {
            widgets::help::render(frame, frame.area());
        }
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
        widgets::skills::render(&self.metrics, frame, layout.skills);
        widgets::memory::render(&self.metrics, frame, layout.memory);
        widgets::resources::render(&self.metrics, frame, layout.resources);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc => self.show_help = false,
                _ => {}
            }
            return;
        }

        if self.confirm_state.is_some() {
            self.handle_confirm_key(key);
            return;
        }

        match self.input_mode {
            InputMode::Normal => self.handle_normal_key(key),
            InputMode::Insert => self.handle_insert_key(key),
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        let response = match key.code {
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            _ => None,
        };
        if let Some(answer) = response
            && let Some(mut state) = self.confirm_state.take()
            && let Some(tx) = state.response_tx.take()
        {
            let _ = tx.send(answer);
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc if self.is_agent_busy() => {
                if let Some(ref signal) = self.cancel_signal {
                    signal.notify_waiters();
                }
            }
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
            KeyCode::Char('d') => {
                self.show_side_panels = !self.show_side_panels;
            }
            KeyCode::Char('e') => {
                self.tool_expanded = !self.tool_expanded;
            }
            KeyCode::Char('c') => {
                self.compact_tools = !self.compact_tools;
            }
            KeyCode::Tab => {
                self.active_panel = match self.active_panel {
                    Panel::Chat => Panel::Skills,
                    Panel::Skills => Panel::Memory,
                    Panel::Memory => Panel::Resources,
                    Panel::Resources => Panel::Chat,
                };
            }
            KeyCode::Char('?') => {
                self.show_help = true;
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
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                let byte_offset = self.byte_offset_of_char(self.cursor_position);
                self.input.insert(byte_offset, '\n');
                self.cursor_position += 1;
            }
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
            KeyCode::Up => {
                match self.history_index {
                    None => {
                        if self.input_history.is_empty() {
                            return;
                        }
                        self.draft_input = self.input.clone();
                        let idx = self.input_history.len() - 1;
                        self.history_index = Some(idx);
                        self.input.clone_from(&self.input_history[idx]);
                    }
                    Some(0) => return,
                    Some(i) => {
                        let idx = i - 1;
                        self.history_index = Some(idx);
                        self.input.clone_from(&self.input_history[idx]);
                    }
                }
                self.cursor_position = self.char_count();
            }
            KeyCode::Down => {
                let Some(i) = self.history_index else {
                    return;
                };
                if i + 1 < self.input_history.len() {
                    let idx = i + 1;
                    self.history_index = Some(idx);
                    self.input.clone_from(&self.input_history[idx]);
                } else {
                    self.history_index = None;
                    self.input = std::mem::take(&mut self.draft_input);
                }
                self.cursor_position = self.char_count();
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
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = self.user_input_tx.try_send("/clear-queue".to_owned());
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
        self.show_splash = false;
        self.input_history.push(text.clone());
        self.history_index = None;
        self.draft_input.clear();
        self.messages.push(ChatMessage {
            role: MessageRole::User,
            content: text.clone(),
            streaming: false,
            tool_name: None,
            diff_data: None,
            filter_stats: None,
        });
        self.input.clear();
        self.cursor_position = 0;
        self.scroll_offset = 0;

        // Non-blocking send; if channel full, message is dropped
        let _ = self.user_input_tx.try_send(text);
    }
}

fn parse_tool_output(content: &str, suffix: &str) -> Option<(String, String)> {
    // New format: [tool output: name]
    if let Some(rest) = content.strip_prefix("[tool output: ")
        && let Some(header_end) = rest.find("]\n```\n")
    {
        let name = rest[..header_end].to_owned();
        let body_start = header_end + "]\n```\n".len();
        let body_part = &rest[body_start..];
        let body = body_part.strip_suffix(suffix).unwrap_or(body_part);
        return Some((name, body.to_owned()));
    }
    // Legacy format: [tool output] — infer tool name from body
    if let Some(rest) = content.strip_prefix("[tool output]\n```\n") {
        let body = rest.strip_suffix(suffix).unwrap_or(rest);
        let name = if body.starts_with("$ ") {
            "bash"
        } else {
            "tool"
        };
        return Some((name.to_owned(), body.to_owned()));
    }
    // Native tool_use format: [tool_result: id]\ncontent
    if let Some(rest) = content.strip_prefix("[tool_result: ") {
        let body = rest.find("]\n").map_or("", |i| &rest[i + 2..]);
        let name = if body.contains("$ ") { "bash" } else { "tool" };
        return Some((name.to_owned(), body.to_owned()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app() -> (App, mpsc::Receiver<String>, mpsc::Sender<AgentEvent>) {
        let (user_tx, user_rx) = mpsc::channel(16);
        let (agent_tx, agent_rx) = mpsc::channel(16);
        let mut app = App::new(user_tx, agent_rx);
        app.messages.clear();
        (app, user_rx, agent_tx)
    }

    #[test]
    fn initial_state() {
        let (app, _rx, _tx) = make_app();
        assert!(app.input().is_empty());
        assert_eq!(app.input_mode(), InputMode::Insert);
        assert!(app.messages().is_empty());
        assert!(app.show_splash());
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
    fn full_message_skips_tool_output_new_format() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::FullMessage(
            "[tool output: bash]\n```\n$ echo hi\nhi\n```".into(),
        ));
        assert!(app.messages().is_empty());
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

    #[test]
    fn confirm_request_sets_state() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.handle_agent_event(AgentEvent::ConfirmRequest {
            prompt: "delete?".into(),
            response_tx: tx,
        });
        assert!(app.confirm_state.is_some());
        assert_eq!(app.confirm_state.as_ref().unwrap().prompt, "delete?");
    }

    #[test]
    fn confirm_modal_y_sends_true() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "proceed?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.confirm_state.is_none());
        assert!(rx.try_recv().unwrap());
    }

    #[test]
    fn confirm_modal_enter_sends_true() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "proceed?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.confirm_state.is_none());
        assert!(rx.try_recv().unwrap());
    }

    #[test]
    fn confirm_modal_n_sends_false() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "delete?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.confirm_state.is_none());
        assert!(!rx.try_recv().unwrap());
    }

    #[test]
    fn confirm_modal_escape_sends_false() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "delete?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.confirm_state.is_none());
        assert!(!rx.try_recv().unwrap());
    }

    #[test]
    fn confirm_modal_blocks_other_keys() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
        app.input_mode = InputMode::Insert;
        app.confirm_state = Some(ConfirmState {
            prompt: "test?".into(),
            response_tx: Some(tx),
        });
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.input().is_empty());
        assert!(app.confirm_state.is_some());
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let (mut app, mut rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "hello".into();
        app.cursor_position = 5;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert_eq!(app.input(), "hello\n");
        assert_eq!(app.cursor_position(), 6);
        assert!(app.messages().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn shift_enter_mid_input() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.input = "ab".into();
        app.cursor_position = 1;
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert_eq!(app.input(), "a\nb");
        assert_eq!(app.cursor_position(), 2);
    }

    #[test]
    fn d_toggles_side_panels() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        assert!(app.show_side_panels());

        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(!app.show_side_panels());

        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.show_side_panels());
    }

    #[test]
    fn mouse_scroll_up() {
        let (mut app, _rx, _tx) = make_app();
        assert_eq!(app.scroll_offset(), 0);
        app.handle_event(AppEvent::MouseScroll(1)).unwrap();
        assert_eq!(app.scroll_offset(), 1);
        app.handle_event(AppEvent::MouseScroll(1)).unwrap();
        assert_eq!(app.scroll_offset(), 2);
    }

    #[test]
    fn mouse_scroll_down() {
        let (mut app, _rx, _tx) = make_app();
        app.scroll_offset = 5;
        app.handle_event(AppEvent::MouseScroll(-1)).unwrap();
        assert_eq!(app.scroll_offset(), 4);
        app.handle_event(AppEvent::MouseScroll(-1)).unwrap();
        assert_eq!(app.scroll_offset(), 3);
    }

    #[test]
    fn mouse_scroll_down_saturates_at_zero() {
        let (mut app, _rx, _tx) = make_app();
        app.scroll_offset = 1;
        app.handle_event(AppEvent::MouseScroll(-1)).unwrap();
        assert_eq!(app.scroll_offset(), 0);
        app.handle_event(AppEvent::MouseScroll(-1)).unwrap();
        assert_eq!(app.scroll_offset(), 0);
    }

    #[test]
    fn mouse_scroll_during_confirm_blocked() {
        let (mut app, _rx, _tx) = make_app();
        let (tx, _oneshot_rx) = tokio::sync::oneshot::channel();
        app.confirm_state = Some(ConfirmState {
            prompt: "test?".into(),
            response_tx: Some(tx),
        });
        app.scroll_offset = 5;
        app.handle_event(AppEvent::MouseScroll(1)).unwrap();
        assert_eq!(app.scroll_offset(), 5);
        app.handle_event(AppEvent::MouseScroll(-1)).unwrap();
        assert_eq!(app.scroll_offset(), 5);
    }

    #[test]
    fn load_history_recognizes_tool_output_new_format() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[
            ("user", "hello"),
            ("assistant", "hi there"),
            ("user", "[tool output: bash]\n```\n$ echo hello\nhello\n```"),
            ("assistant", "done"),
        ]);
        assert_eq!(app.messages().len(), 4);
        assert_eq!(app.messages()[0].role, MessageRole::User);
        assert_eq!(app.messages()[1].role, MessageRole::Assistant);
        assert_eq!(app.messages()[2].role, MessageRole::Tool);
        assert_eq!(app.messages()[2].tool_name.as_deref(), Some("bash"));
        assert_eq!(app.messages()[2].content, "$ echo hello\nhello");
        assert_eq!(app.messages()[3].role, MessageRole::Assistant);
    }

    #[test]
    fn load_history_recognizes_legacy_tool_output() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[("user", "[tool output]\n```\n$ ls\nfile.txt\n```")]);
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, MessageRole::Tool);
        assert_eq!(app.messages()[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(app.messages()[0].content, "$ ls\nfile.txt");
    }

    #[test]
    fn load_history_legacy_non_bash_tool() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[(
            "user",
            "[tool output]\n```\n[mcp:github:list]\nresults\n```",
        )]);
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, MessageRole::Tool);
        assert_eq!(app.messages()[0].tool_name.as_deref(), Some("tool"));
    }

    #[test]
    fn load_history_recognizes_tool_result_format() {
        let (mut app, _rx, _tx) = make_app();
        app.load_history(&[("user", "[tool_result: toolu_abc]\n$ echo hello\nhello")]);
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, MessageRole::Tool);
        assert_eq!(app.messages()[0].tool_name.as_deref(), Some("bash"));
        assert_eq!(app.messages()[0].content, "$ echo hello\nhello");
    }

    #[test]
    fn tool_output_without_prior_tool_start_creates_tool_message_with_diff() {
        let (mut app, _rx, _tx) = make_app();
        let diff = zeph_core::DiffData {
            file_path: "src/lib.rs".into(),
            old_content: "fn old() {}".into(),
            new_content: "fn new() {}".into(),
        };
        app.handle_agent_event(AgentEvent::ToolOutput {
            tool_name: "edit".into(),
            command: "[tool output: edit]\n```\nok\n```".into(),
            output: "[tool output: edit]\n```\nok\n```".into(),
            success: true,
            diff: Some(diff),
            filter_stats: None,
        });

        assert_eq!(app.messages().len(), 1);
        let msg = &app.messages()[0];
        assert_eq!(msg.role, MessageRole::Tool);
        assert!(!msg.streaming);
        assert!(msg.diff_data.is_some());
    }

    #[test]
    fn tool_output_without_diff_does_not_create_spurious_message() {
        let (mut app, _rx, _tx) = make_app();
        app.handle_agent_event(AgentEvent::ToolOutput {
            tool_name: "read".into(),
            command: "[tool output: read]\n```\ncontent\n```".into(),
            output: "[tool output: read]\n```\ncontent\n```".into(),
            success: true,
            diff: None,
            filter_stats: None,
        });

        // No prior ToolStart and no diff/filter_stats: nothing to display.
        assert!(app.messages().is_empty());
    }

    #[test]
    fn show_help_defaults_to_false() {
        let (app, _rx, _tx) = make_app();
        assert!(!app.show_help);
    }

    #[test]
    fn question_mark_in_normal_mode_opens_help() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Normal;
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.show_help);
    }

    #[test]
    fn question_mark_toggles_help_closed() {
        let (mut app, _rx, _tx) = make_app();
        app.show_help = true;
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(!app.show_help);
    }

    #[test]
    fn esc_closes_help_popup() {
        let (mut app, _rx, _tx) = make_app();
        app.show_help = true;
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(!app.show_help);
    }

    #[test]
    fn other_keys_ignored_when_help_open() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        app.show_help = true;

        // Typing a character should not modify input
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.input().is_empty());
        assert!(app.show_help);

        // Enter should not submit
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.messages().is_empty());
        assert!(app.show_help);
    }

    #[test]
    fn help_popup_does_not_block_ctrl_c() {
        let (mut app, _rx, _tx) = make_app();
        app.show_help = true;
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(app.should_quit);
    }

    #[test]
    fn question_mark_in_insert_mode_does_not_open_help() {
        let (mut app, _rx, _tx) = make_app();
        app.input_mode = InputMode::Insert;
        let key = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        assert!(!app.show_help);
        assert_eq!(app.input(), "?");
    }

    #[tokio::test]
    async fn esc_in_normal_mode_cancels_when_busy() {
        let (mut app, _rx, _tx) = make_app();
        let notify = Arc::new(Notify::new());
        let notify_waiter = Arc::clone(&notify);
        let handle = tokio::spawn(async move {
            notify_waiter.notified().await;
            true
        });
        tokio::task::yield_now().await;

        app = app.with_cancel_signal(Arc::clone(&notify));
        app.input_mode = InputMode::Normal;
        app.status_label = Some("Thinking...".into());
        assert!(app.is_agent_busy());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), handle).await;
        assert!(result.is_ok(), "notify should have been triggered");
    }

    #[test]
    fn esc_in_normal_mode_does_not_cancel_when_idle() {
        let (mut app, _rx, _tx) = make_app();
        let notify = Arc::new(Notify::new());
        app = app.with_cancel_signal(notify);
        app.input_mode = InputMode::Normal;
        assert!(!app.is_agent_busy());

        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_event(AppEvent::Key(key)).unwrap();
        // No way to assert "not notified" directly, but we verify no panic
    }
}
