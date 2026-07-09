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
    // Walk `start` up until everything from `start..=active` fits in `width`,
    // reserving a left marker when `start > 0` and a right marker when tabs
    // follow `active` (so the render loop's matching right-reserve can never
    // clip the active tab itself). This keeps the active tab visible and biases
    // toward showing as many earlier tabs as possible.
    let right_marker = usize::from(active < items.len() - 1);
    let mut start = 0;
    loop {
        let left_marker = if start > 0 { 1 } else { 0 };
        let used: usize = items[start..=active].iter().map(label_width).sum();
        if used + left_marker + right_marker <= width || start >= active {
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
        // Reserve a column for the right edge marker when more tabs follow — for
        // EVERY tab (incl. the active one), so a tab that would land exactly at
        // `width` while more tabs follow is clipped, guaranteeing a free column
        // for the `›` marker (Mn2: the exactly-full case).
        let more_follow = i < items.len() - 1;
        let reserve = usize::from(more_follow);
        if used + w + reserve > width {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, dirty: bool) -> TabItem {
        TabItem {
            short_name: name.to_owned(),
            dirty,
        }
    }

    #[test]
    fn tab_label_widths() {
        assert_eq!(tab_label(&item("ab", false)), " ab ");
        assert_eq!(tab_label(&item("ab", true)), " ab ● ");
        // Truncation to MAX_NAME cols with an ellipsis.
        let long = "a".repeat(20);
        let lbl = tab_label(&item(&long, false));
        assert!(lbl.contains('…'), "long name truncated: {lbl:?}");
        assert_eq!(
            lbl.chars().count(),
            MAX_NAME + 2,
            "MAX_NAME + surrounding spaces"
        );
    }

    /// Mn2: when the active tab is the last-visible and tabs still FOLLOW it,
    /// `window_start` reserves a right-marker column so the render loop's matching
    /// reserve can never clip the active tab — the window fits `start..=active`
    /// into `width - 1` (right marker), guaranteeing a free column for `›`.
    #[test]
    fn window_reserves_right_marker_when_more_follow() {
        // Three tabs of width 4 each (" a " → 3? no: " ab " = 4). Use 1-char
        // names so each label is " x " = 3 cols.
        let items = vec![item("a", false), item("b", false), item("c", false)];
        // widths: 3 each. active = 1 (middle), more follow (c). width chosen so
        // start..=active (a,b = 6) fits exactly, but +1 right marker forces the
        // window to drop `a`.
        let (start, left) = window_start(&items, 1, 6);
        assert_eq!(start, 1, "right-marker reserve drops the leading tab");
        assert!(left, "a left marker shows when clipped-left");

        // With one extra column (7) both a and b + right marker fit.
        let (start, _) = window_start(&items, 1, 7);
        assert_eq!(start, 0, "extra column lets the leading tab stay");

        // No tabs follow the active (active is last) → no right reserve, so with
        // width 7 the last two tabs (b,c = 6) + left marker (1) fit at start=1.
        let (start, _) = window_start(&items, 2, 7);
        assert_eq!(start, 1, "no right reserve when active is the last tab");
    }
}
