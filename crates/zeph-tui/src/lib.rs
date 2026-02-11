pub mod app;
pub mod channel;
pub mod event;
pub mod layout;
pub mod metrics;
pub mod theme;
pub mod widgets;

use std::io;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub use app::App;
pub use channel::TuiChannel;
pub use event::{AgentEvent, AppEvent, EventReader};
pub use metrics::{MetricsCollector, MetricsSnapshot};

/// # Errors
///
/// Returns an error if terminal init/restore or rendering fails.
pub async fn run_tui(mut app: App, mut event_rx: mpsc::Receiver<AppEvent>) -> anyhow::Result<()> {
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
) -> anyhow::Result<()> {
    loop {
        app.poll_metrics();
        terminal.draw(|frame| app.draw(frame))?;

        tokio::select! {
            Some(event) = event_rx.recv() => {
                app.handle_event(event)?;
            }
            Some(agent_event) = app.poll_agent_event() => {
                app.handle_agent_event(agent_event);
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn init_terminal() -> anyhow::Result<Terminal<CrosstermBackend<io::Stdout>>> {
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

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture,
    )?;
    terminal.show_cursor()?;
    Ok(())
}
