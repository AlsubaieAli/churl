//! One-line status bar: focused pane, workspace name, key hints.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

/// Renders the status bar.
pub fn render(frame: &mut Frame, area: Rect, focus: &str, workspace: Option<&str>) {
    let workspace = workspace.unwrap_or("no workspace");
    let line =
        format!(" {focus} · {workspace} · j/k move · enter select · / search · : palette · q quit");
    frame.render_widget(Paragraph::new(Line::from(line)), area);
}
