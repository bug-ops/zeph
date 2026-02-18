use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, MouseEventKind};
use tokio::sync::{mpsc, oneshot};

pub trait EventSource: Send + 'static {
    fn next_event(&mut self) -> Option<AppEvent>;
}

pub struct CrosstermEventSource {
    tick_rate: Duration,
}

impl CrosstermEventSource {
    #[must_use]
    pub fn new(tick_rate: Duration) -> Self {
        Self { tick_rate }
    }
}

impl EventSource for CrosstermEventSource {
    fn next_event(&mut self) -> Option<AppEvent> {
        if event::poll(self.tick_rate).unwrap_or(false) {
            match event::read() {
                Ok(CrosstermEvent::Key(key)) => Some(AppEvent::Key(key)),
                Ok(CrosstermEvent::Resize(w, h)) => Some(AppEvent::Resize(w, h)),
                Ok(CrosstermEvent::Mouse(mouse)) => match mouse.kind {
                    MouseEventKind::ScrollUp => Some(AppEvent::MouseScroll(1)),
                    MouseEventKind::ScrollDown => Some(AppEvent::MouseScroll(-1)),
                    _ => None,
                },
                _ => None,
            }
        } else {
            Some(AppEvent::Tick)
        }
    }
}

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    Resize(u16, u16),
    MouseScroll(i8),
    Agent(AgentEvent),
}

#[derive(Debug)]
pub enum AgentEvent {
    Chunk(String),
    FullMessage(String),
    Flush,
    Typing,
    Status(String),
    ToolStart {
        tool_name: String,
        command: String,
    },
    ToolOutputChunk {
        tool_name: String,
        command: String,
        chunk: String,
    },
    ToolOutput {
        tool_name: String,
        command: String,
        output: String,
        success: bool,
        diff: Option<zeph_core::DiffData>,
        filter_stats: Option<String>,
    },
    ConfirmRequest {
        prompt: String,
        response_tx: oneshot::Sender<bool>,
    },
    QueueCount(usize),
    DiffReady(zeph_core::DiffData),
}

pub struct EventReader {
    tx: mpsc::Sender<AppEvent>,
    tick_rate: Duration,
}

impl EventReader {
    #[must_use]
    pub fn new(tx: mpsc::Sender<AppEvent>, tick_rate: Duration) -> Self {
        Self { tx, tick_rate }
    }

    /// Blocking loop — must run on a dedicated `std::thread`, not a tokio worker.
    pub fn run(self) {
        let tick_rate = self.tick_rate;
        self.run_with_source(CrosstermEventSource::new(tick_rate));
    }

    pub fn run_with_source(self, mut source: impl EventSource) {
        while let Some(evt) = source.next_event() {
            if self.tx.blocking_send(evt).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_debug() {
        let e = AgentEvent::Chunk("hello".into());
        let s = format!("{e:?}");
        assert!(s.contains("Chunk"));
    }

    #[test]
    fn app_event_variants() {
        let tick = AppEvent::Tick;
        assert!(matches!(tick, AppEvent::Tick));

        let resize = AppEvent::Resize(80, 24);
        assert!(matches!(resize, AppEvent::Resize(80, 24)));
    }

    #[test]
    fn event_reader_construction() {
        let (tx, _rx) = mpsc::channel(16);
        let reader = EventReader::new(tx, Duration::from_millis(100));
        assert_eq!(reader.tick_rate, Duration::from_millis(100));
    }

    #[test]
    fn confirm_request_debug() {
        let (tx, _rx) = oneshot::channel();
        let e = AgentEvent::ConfirmRequest {
            prompt: "delete?".into(),
            response_tx: tx,
        };
        let s = format!("{e:?}");
        assert!(s.contains("ConfirmRequest"));
        assert!(s.contains("delete?"));
    }

    #[test]
    fn app_event_mouse_scroll_variant() {
        let scroll_up = AppEvent::MouseScroll(1);
        assert!(matches!(scroll_up, AppEvent::MouseScroll(1)));

        let scroll_down = AppEvent::MouseScroll(-1);
        assert!(matches!(scroll_down, AppEvent::MouseScroll(-1)));
    }
}
