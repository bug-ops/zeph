pub mod app;
pub mod channel;
pub mod command;
pub mod error;
pub mod event;
pub mod file_picker;
pub mod highlight;
pub mod hyperlink;
pub mod layout;
pub mod metrics;
#[cfg(test)]
pub mod test_utils;
pub mod theme;
pub mod widgets;

use std::io;

pub use app::App;
pub use channel::TuiChannel;
pub use command::TuiCommand;
pub use error::TuiError;
pub use event::{AgentEvent, AppEvent, CrosstermEventSource, EventReader, EventSource};
pub use metrics::{MetricsCollector, MetricsSnapshot};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

/// # Errors
///
/// Returns an error if terminal init/restore or rendering fails.
pub async fn run_tui(mut app: App, mut event_rx: mpsc::Receiver<AppEvent>) -> Result<(), TuiError> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture
        );
        original_hook(info);
    }));

    let mut terminal = init_terminal()?;

    let result = tui_loop(&mut app, &mut event_rx, &mut terminal).await;

    restore_terminal(&mut terminal)?;

    // Restore the default panic hook
    let _ = std::panic::take_hook();

    result
}

async fn tui_loop(
    app: &mut App,
    event_rx: &mut mpsc::Receiver<AppEvent>,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(), TuiError> {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        app.poll_metrics();
        terminal.draw(|frame| app.draw(frame))?;

        let links = app.take_hyperlinks();
        if !links.is_empty() {
            hyperlink::write_osc8(terminal.backend_mut(), &links)?;
        }

        tokio::select! {
            biased;
            Some(event) = event_rx.recv() => {
                app.handle_event(event);
            }
            Some(agent_event) = app.poll_agent_event() => {
                app.handle_agent_event(agent_event);
                while let Ok(ev) = app.try_recv_agent_event() {
                    app.handle_agent_event(ev);
                }
            }
            _ = tick.tick() => {}
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, TuiError> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<(), TuiError> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
    )?;
    terminal.show_cursor()?;
    Ok(())
}
