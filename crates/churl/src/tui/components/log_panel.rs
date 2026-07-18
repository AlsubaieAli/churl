//! The debug Log panel overlay (`<leader>L`): a read-only, scrollable view of
//! the bounded `tracing` ring captured by [`crate::tui::log_subscriber`].
//!
//! The ring buffer itself lives on `App` (continuously written by the
//! background subscriber, independent of whether the panel is open), so
//! unlike [`super::inspector::InspectorState`] this component holds only
//! transient VIEW state — the ring's contents are handed to [`render`] each
//! frame, never owned here. All UI state lives in the `churl` crate;
//! `churl-core` stays TUI-free.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::tui::log_subscriber::LogEvent;
use crate::tui::theme::Theme;

/// What the app should do after the overlay handled a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogPanelOutcome {
    /// Fully handled inside the overlay; nothing for the app to do.
    Consumed,
    /// Close the overlay (the app restores the parked mode).
    Close,
}

/// View-only state of the open Log panel overlay — lives inside
/// `Mode::LogPanel` (see `tui/app/state.rs`) per the illegal-states rule,
/// rather than a parallel `App` field.
#[derive(Debug, Clone, Default)]
pub struct LogPanelState {
    /// Scroll offset (display lines) from the top of the ring.
    scroll: usize,
}

impl LogPanelState {
    /// A fresh panel view, scrolled to the top.
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles one key event against `line_count` currently-rendered lines
    /// (the caller passes the ring snapshot's length so scroll clamps
    /// correctly without this component owning the ring).
    pub fn handle_key(&mut self, key: KeyEvent, line_count: usize) -> LogPanelOutcome {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => LogPanelOutcome::Close,
            KeyCode::Char('j') | KeyCode::Down => {
                let max = line_count.saturating_sub(1);
                self.scroll = (self.scroll + 1).min(max);
                LogPanelOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                LogPanelOutcome::Consumed
            }
            KeyCode::Char('g') => {
                self.scroll = 0;
                LogPanelOutcome::Consumed
            }
            KeyCode::Char('G') => {
                self.scroll = line_count.saturating_sub(1);
                LogPanelOutcome::Consumed
            }
            _ => LogPanelOutcome::Consumed,
        }
    }
}

/// The style for one log level — mirrors the Inspector's masked-value
/// treatment idea: a steady, recognizable colour cue per severity. Reuses
/// the existing `status_error` slot for both ERROR and WARN (no new theme
/// slot needed) and leaves INFO/DEBUG/TRACE at the pane's plain style.
fn level_style(level: tracing::Level, theme: &Theme) -> Style {
    match level {
        tracing::Level::ERROR | tracing::Level::WARN => theme.status_error,
        _ => Style::default(),
    }
}

/// Renders the Log panel overlay over `area`: a bordered modal listing the
/// ring's captured events, newest last (chronological, matching a normal
/// scrollback), each line `LEVEL target: message`.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    events: &[LogEvent],
    state: &LogPanelState,
    theme: &Theme,
) {
    let [modal] = Layout::horizontal([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(modal);
    frame.render_widget(Clear, modal);

    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(format!(
            " Log — {} event{} · j/k scroll · g/G top/bottom · q/esc close ",
            events.len(),
            if events.len() == 1 { "" } else { "s" }
        ))
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let lines: Vec<Line> = if events.is_empty() {
        vec![Line::raw(
            "no log events captured yet — enable debug capture (<leader>D) and use churl; \
             events show up here as they are emitted.",
        )]
    } else {
        events
            .iter()
            .map(|event| {
                let style = level_style(event.level, theme);
                Line::from(vec![
                    Span::styled(format!("{:>5} ", event.level), style),
                    Span::raw(format!("{}: ", event.target)),
                    Span::raw(event.message.clone()),
                ])
            })
            .collect()
    };
    let total = lines.len();
    let scroll = state.scroll.min(total.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn q_and_esc_close() {
        let mut state = LogPanelState::new();
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q')), 0),
            LogPanelOutcome::Close
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Esc), 0),
            LogPanelOutcome::Close
        );
    }

    #[test]
    fn scroll_clamps_to_line_count() {
        let mut state = LogPanelState::new();
        for _ in 0..10 {
            state.handle_key(key(KeyCode::Down), 3);
        }
        assert_eq!(state.scroll, 2);
        state.handle_key(key(KeyCode::Char('k')), 3);
        assert_eq!(state.scroll, 1);
    }

    #[test]
    fn g_and_shift_g_jump_to_ends() {
        let mut state = LogPanelState::new();
        state.handle_key(key(KeyCode::Char('G')), 5);
        assert_eq!(state.scroll, 4);
        state.handle_key(key(KeyCode::Char('g')), 5);
        assert_eq!(state.scroll, 0);
    }
}
