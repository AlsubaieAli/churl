//! One-line status bar: focused pane, workspace name, active profile, key hints.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;

/// What [`render`] needs to draw the status bar.
pub struct StatusCtx<'a> {
    /// Focused pane name.
    pub focus: &'a str,
    /// Workspace name, if a workspace is open.
    pub workspace: Option<&'a str>,
    /// Active profile name, if one is set.
    pub profile: Option<&'a str>,
    /// Transient message (send hints, history/errors), replacing the key hints.
    pub message: Option<&'a str>,
    /// The colour theme.
    pub theme: &'a Theme,
}

/// Renders the status bar. A transient `message` replaces the key hints when
/// present; the active profile is shown (when set) after the workspace name.
pub fn render(frame: &mut Frame, area: Rect, ctx: StatusCtx) {
    let workspace = ctx.workspace.unwrap_or("no workspace");
    let profile = match ctx.profile {
        Some(name) => format!(" · profile {name}"),
        None => String::new(),
    };
    let (line, style) = match ctx.message {
        Some(message) => (
            format!(" {} · {workspace}{profile} · {message}", ctx.focus),
            ctx.theme.statusline,
        ),
        None => (
            format!(
                " {} · {workspace}{profile} · j/k move · enter select · ctrl-s send · w save · f jump · / search · : palette · q quit",
                ctx.focus
            ),
            ctx.theme.statusline,
        ),
    };
    frame.render_widget(Paragraph::new(Line::from(line)).style(style), area);
}
