//! One-line status bar: persistent state only — focused pane, workspace/endpoint,
//! active profile, dirty marker, and the in-flight spinner. Transient action
//! messages live in the dedicated message row above (M6.7 deliverable 9), never
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
    spans.push(Span::raw(tail));
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(ctx.theme.statusline),
        area,
    );
}
