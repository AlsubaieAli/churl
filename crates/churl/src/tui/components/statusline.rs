//! One-line status bar: persistent state only — focused pane, workspace/endpoint,
//! active profile, dirty marker, and the in-flight spinner. Transient action
//! messages live in the dedicated message row above, never
//! here, so the two components stay decoupled.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;

/// Spinner frames for the in-flight indicator (advanced by the 250 ms tick).
const SPINNER: [char; 4] = ['⠋', '⠙', '⠹', '⠸'];

/// What [`render`] needs to draw the status bar — persistent state only.
pub struct StatusCtx<'a> {
    /// Focused pane name.
    pub focus: &'a str,
    /// Workspace name, if a workspace is open.
    pub workspace: Option<&'a str>,
    /// Active profile name, if one is set.
    pub profile: Option<&'a str>,
    /// Whether the loaded endpoint has unsaved changes.
    pub dirty: bool,
    /// Whether a request is in flight (drives the spinner).
    pub in_flight: bool,
    /// Whether history writes are currently failing: draws a sticky,
    /// error-styled "⚠ history not recording" flag that persists until a write
    /// succeeds — unlike the auto-expiring message row, so a persistent SQLite
    /// failure can't scroll past unnoticed ("fail loud").
    pub history_failing: bool,
    /// Monotonic tick counter (drives the spinner frame).
    pub tick_count: u64,
    /// The colour theme.
    pub theme: &'a Theme,
}

/// Renders the status bar: focus · workspace · profile · dirty · in-flight
/// spinner (or the key hints when idle).
pub fn render(frame: &mut Frame, area: Rect, ctx: StatusCtx) {
    let workspace = ctx.workspace.unwrap_or("no workspace");
    let profile = match ctx.profile {
        Some(name) => format!(" · profile {name}"),
        None => String::new(),
    };
    let tail = if ctx.in_flight {
        let frame_char = SPINNER[(ctx.tick_count as usize) % SPINNER.len()];
        format!(" · {frame_char} sending… (ctrl-c cancels)")
    } else {
        " · ? help · space leader · / search · : palette".to_owned()
    };
    // The unsaved marker is a theme-accented span (a steady visual accent, not
    // decoration) between the persistent prefix and the tail.
    let mut spans = vec![Span::raw(format!(" {} · {workspace}{profile}", ctx.focus))];
    if ctx.dirty {
        spans.push(Span::styled(" · ● unsaved · w save", ctx.theme.accent));
    }
    // Sticky history-failure flag: error-styled, stays until a write
    // succeeds. Placed before the idle/spinner tail so it reads as steady state,
    // not a transient hint.
    if ctx.history_failing {
        spans.push(Span::styled(
            " · ⚠ history not recording",
            ctx.theme.status_error,
        ));
    }
    spans.push(Span::raw(tail));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(ctx.theme.statusline),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_line(history_failing: bool) -> String {
        let theme = Theme::dark();
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    Rect::new(0, 0, 80, 1),
                    StatusCtx {
                        focus: "Request",
                        workspace: Some("demo"),
                        profile: None,
                        dirty: false,
                        in_flight: false,
                        history_failing,
                        tick_count: 0,
                        theme: &theme,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..80)
            .map(|x| buffer[(x, 0)].symbol().to_owned())
            .collect()
    }

    #[test]
    fn sticky_history_flag_shows_only_when_failing() {
        // B3: a healthy session (the common case, and every existing snapshot)
        // shows no flag; a failing one shows the sticky, persistent warning.
        assert!(!render_line(false).contains("history not recording"));
        assert!(render_line(true).contains("⚠ history not recording"));
    }
}
