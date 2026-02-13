use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders, Sparkline};

use crate::app::SystemMetrics;
use crate::theme::Theme;

pub fn render(metrics: &SystemMetrics, frame: &mut Frame, area: Rect) {
    let theme = Theme::default();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let cpu_data: Vec<u64> = metrics.cpu_history().iter().copied().collect();
    let cpu_pct = metrics.current_cpu();
    let cpu_sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(format!(" CPU {cpu_pct}% ")),
        )
        .data(&cpu_data)
        .max(100);
    frame.render_widget(cpu_sparkline, chunks[0]);

    let mem_data: Vec<u64> = metrics.mem_history().iter().copied().collect();
    let (used_mb, total_mb) = metrics.current_mem();
    let mem_sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(format!(" Mem {used_mb}M/{total_mb}M ")),
        )
        .data(&mem_data)
        .max(total_mb);
    frame.render_widget(mem_sparkline, chunks[1]);
}
