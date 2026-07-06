//! Dedicated message row: action/transient messages (saves, merges, errors,
//! CRUD results) render in their own single-line row *above* the statusline —
//! never covering statusline content (the two are deliberately decoupled).
//!
//! The row appears only while a message is live and disappears after
//! [`Message::EXPIRE_SECS`] (default 6 s); a newer message replaces the current.
//! Expiry is checked on the existing 250 ms tick.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;

/// Default message lifetime in seconds (config-knob-ready named constant).
pub const MESSAGE_EXPIRE_SECS: u64 = 6;

/// A transient action/status message shown in the dedicated row above the
/// statusline. Auto-expires after [`MESSAGE_EXPIRE_SECS`].
#[derive(Debug, Clone)]
pub struct Message {
    /// The message text.
    pub text: String,
    /// When the message was set (for expiry).
    set_at: Instant,
}

impl Message {
    /// The lifetime after which a message is considered expired.
    pub const EXPIRE_SECS: u64 = MESSAGE_EXPIRE_SECS;

    /// Creates a message stamped now.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            set_at: Instant::now(),
        }
    }

    /// Whether this message has outlived [`Message::EXPIRE_SECS`].
    pub fn is_expired(&self) -> bool {
        self.set_at.elapsed().as_secs() >= Self::EXPIRE_SECS
    }
}

/// Renders the message row. Called only when a message is live; the caller
/// reserves the row's height only in that case, so the statusline never moves.
pub fn render(frame: &mut Frame, area: Rect, text: &str, theme: &Theme) {
    frame.render_widget(
        Paragraph::new(Line::from(format!(" {text}"))).style(theme.selection),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn fresh_message_not_expired() {
        assert!(!Message::new("hi").is_expired());
    }

    #[test]
    fn backdated_message_expires() {
        let msg = Message {
            text: "stale".to_owned(),
            set_at: Instant::now() - Duration::from_secs(Message::EXPIRE_SECS + 1),
        };
        assert!(msg.is_expired());
    }
}
