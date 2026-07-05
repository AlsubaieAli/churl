//! One-line status bar: focused pane, workspace name, key hints.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

/// Renders the status bar. A transient `message` (send hints, history/errors)
/// replaces the key hints when present.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    focus: &str,
    workspace: Option<&str>,
    message: Option<&str>,
) {
    let workspace = workspace.unwrap_or("no workspace");
    let line = match message {
        Some(message) => format!(" {focus} · {workspace} · {message}"),
        None => format!(
            " {focus} · {workspace} · j/k move · enter select · ctrl-s send · / search · : palette · q quit"
        ),
    };
    frame.render_widget(Paragraph::new(Line::from(line)), area);
}
