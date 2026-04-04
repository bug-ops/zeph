// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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
    pub kept_lines: Option<Vec<usize>>,
    pub timestamp: String,
}

impl ChatMessage {
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            streaming: false,
            tool_name: None,
            diff_data: None,
            filter_stats: None,
            kept_lines: None,
            timestamp: format_local_time(),
        }
    }

    #[must_use]
    pub fn streaming(mut self) -> Self {
        self.streaming = true;
        self
    }

    #[must_use]
    pub fn with_tool(mut self, name: String) -> Self {
        self.tool_name = Some(name);
        self
    }
}

fn format_local_time() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}
