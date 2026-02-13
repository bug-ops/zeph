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
    let (mem_used, mem_total) = metrics.current_mem();
    #[allow(clippy::cast_precision_loss)]
    let title = format!(
        " Mem {:.1}G/{:.0}G ",
        mem_used as f64 / 1024.0,
        mem_total as f64 / 1024.0,
    );
    let mem_sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.panel_border)
                .title(title),
        )
        .data(&mem_data)
        .max(mem_total);
    frame.render_widget(mem_sparkline, chunks[1]);
}
