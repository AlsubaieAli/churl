//! Tab strip: a 1-row strip at the top of column B listing the open buffers.
//!
//! Rendered ONLY when at least one buffer is open — the caller allots
//! `Length(0)` for an empty buffer list, so the zero-buffer render stays
//! byte-identical to the pre-tabs layout. Each tab shows the endpoint's short
//! name plus an accent `●` when that buffer is dirty; the active tab is
//! highlighted (reverse/accent), inactive tabs dimmed. When the tabs overflow
//! the strip width, a horizontal window is derived each frame to keep the active
//! tab visible, with `‹`/`›` edge markers marking clipped sides. No persistent
//! scroll state — the window is a pure function of the items, active index, and
//! width.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;

/// The maximum display columns a tab's short name is truncated to (an ellipsis
/// replaces the tail when longer).
const MAX_NAME: usize = 12;

/// A single tab's render inputs: its short endpoint name and dirty state.
pub struct TabItem {
    /// The endpoint's short display name (truncated to [`MAX_NAME`] cols here).
    pub short_name: String,
    /// Whether this buffer has unsaved changes (drives the `●` marker).
    pub dirty: bool,
}

/// The rendered label text of a tab, including surrounding spaces and the dirty
/// marker: `" name ● "` or `" name "`. Used both for width measurement and
/// rendering so the two never drift.
fn tab_label(item: &TabItem) -> String {
    let name = truncate(&item.short_name, MAX_NAME);
    if item.dirty {
        format!(" {name} ● ")
    } else {
        format!(" {name} ")
    }
}

/// Truncates `s` to at most `max` display columns, appending `…` when clipped.
/// Counts `char`s (approximates display width; endpoint names are plain text).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// The width in display columns a tab label occupies.
fn label_width(item: &TabItem) -> usize {
    tab_label(item).chars().count()
}

/// Picks the first visible tab index so the `active` tab fits within `width`
/// columns, reserving room for edge markers when tabs are clipped. Scans from a
/// candidate start upward until `active` fits; a simple keep-active-visible
/// window (no persistent state). Returns `(start, show_left_marker)`.
fn window_start(items: &[TabItem], active: usize, width: usize) -> (usize, bool) {
    // Walk `start` up until everything from `start..=active` fits in `width`
    // (minus a left marker when `start > 0`). This keeps the active tab visible
    // and biases toward showing as many earlier tabs as possible.
    let mut start = 0;
    loop {
        let left_marker = if start > 0 { 1 } else { 0 };
        let used: usize = items[start..=active].iter().map(label_width).sum();
        if used + left_marker <= width || start >= active {
            return (start, start > 0);
        }
        start += 1;
    }
}

/// Renders the tab strip into a 1-row `area`. Callers only invoke this with a
/// non-empty `items` (the layout gives the strip `Length(0)` otherwise); an
/// empty `items` renders nothing defensively.
pub fn render(frame: &mut Frame, area: Rect, items: &[TabItem], active: usize, theme: &Theme) {
    if items.is_empty() || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let active = active.min(items.len() - 1);
    let (start, left_marker) = window_start(items, active, width);

    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    if left_marker {
        spans.push(Span::styled("‹", theme.title));
        used += 1;
    }
    let mut clipped_right = false;
    for (i, item) in items.iter().enumerate().skip(start) {
        let label = tab_label(item);
        let w = label.chars().count();
        // Reserve a column for the right edge marker when more tabs follow.
        let more_follow = i < items.len() - 1;
        let reserve = usize::from(more_follow);
        if used + w + reserve > width && i != active {
            clipped_right = true;
            break;
        }
        if used + w > width {
            clipped_right = true;
            break;
        }
        // Split the label so the dirty `●` can carry the accent style even on the
        // active (highlighted) tab.
        let style = if i == active {
            theme.selection
        } else {
            theme.border_unfocused
        };
        if item.dirty {
            let name = truncate(&item.short_name, MAX_NAME);
            spans.push(Span::styled(format!(" {name} "), style));
            spans.push(Span::styled("●", theme.accent));
            spans.push(Span::styled(" ", style));
        } else {
            spans.push(Span::styled(label, style));
        }
        used += w;
    }
    if clipped_right && used < width {
        spans.push(Span::styled("›", theme.title));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
