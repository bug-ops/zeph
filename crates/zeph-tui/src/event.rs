use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent};
use tokio::sync::mpsc;

use crate::metrics::MetricsSnapshot;

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    Resize(u16, u16),
    Agent(AgentEvent),
}

#[derive(Debug)]
pub enum AgentEvent {
    Chunk(String),
    FullMessage(String),
    Flush,
    Typing,
    MetricsUpdate(MetricsSnapshot),
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
        loop {
            if event::poll(self.tick_rate).unwrap_or(false) {
                let evt = match event::read() {
                    Ok(CrosstermEvent::Key(key)) => AppEvent::Key(key),
                    Ok(CrosstermEvent::Resize(w, h)) => AppEvent::Resize(w, h),
                    _ => continue,
                };
                if self.tx.blocking_send(evt).is_err() {
                    break;
                }
            } else if self.tx.blocking_send(AppEvent::Tick).is_err() {
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
}
