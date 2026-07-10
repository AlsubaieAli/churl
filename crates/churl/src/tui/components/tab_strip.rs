//! Tab strip: a 1-row strip at the top of column B listing the open buffers.
//!
//! Rendered ONLY when at least one buffer is open — the caller allots
//! `Length(0)` for an empty buffer list, so the zero-buffer render stays
//! byte-identical to the pre-tabs layout. Each tab is a filled "chip": a `▐`
//! left edge, the short name (with an accent `●` when dirty), and a `▌` right
//! edge, all on the chip background — bright ([`Theme::selection`]) for the
//! active tab, dim for the rest — with a single raw space gap between chips.
//! When the tabs overflow the strip width, a horizontal window is derived each
//! frame to keep the active tab visible, with `‹`/`›` edge markers marking
//! clipped sides. No persistent scroll state — the window is a pure function of
//! the items, active index, and width.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::theme::Theme;

/// The maximum display columns a tab's short name is truncated to (an ellipsis
/// replaces the tail when longer).
const MAX_NAME: usize = 12;

/// The `▐` (U+2590) left edge glyph of a chip. Shared with the request tab bar.
pub(super) const CHIP_LEFT: &str = "▐";
/// The `▌` (U+258C) right edge glyph of a chip. Shared with the request tab bar.
pub(super) const CHIP_RIGHT: &str = "▌";
/// Columns the chip framing adds to a tab beyond its label: the two half-block
/// edges plus the single space gap that trails every chip. Every width
/// computation counts a chip as `label_width + CHIP_OVERHEAD`.
pub(super) const CHIP_OVERHEAD: usize = 3;

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

/// The width in display columns a rendered chip occupies: the label plus the
/// [`CHIP_OVERHEAD`] framing (both half-block edges and the trailing gap). The
/// single source every width computation measures a tab by, so the window math
/// and the render loop can never drift from what actually paints.
fn chip_width(item: &TabItem) -> usize {
    label_width(item) + CHIP_OVERHEAD
}

/// Picks the first visible tab index so the `active` tab fits within `width`
/// columns, reserving room for edge markers when tabs are clipped. Scans from a
/// candidate start upward until `active` fits; a simple keep-active-visible
/// window (no persistent state). Returns `(start, show_left_marker)`.
///
/// The core is [`chip_window`], shared with the request pane's tab bar so both
/// horizontal chip strips scroll by the same proven logic. `widths` are chip
/// widths (label + edges + gap), so the window fits the columns the render loop
/// actually paints.
pub(super) fn chip_window(widths: &[usize], active: usize, width: usize) -> (usize, bool) {
    // Walk `start` up until everything from `start..=active` fits in `width`,
    // reserving a left marker when `start > 0` and a right marker when tabs
    // follow `active` (so the render loop's matching right-reserve can never
    // clip the active tab itself). This keeps the active tab visible and biases
    // toward showing as many earlier tabs as possible.
    let right_marker = usize::from(active < widths.len() - 1);
    let mut start = 0;
    loop {
        let left_marker = if start > 0 { 1 } else { 0 };
        let used: usize = widths[start..=active].iter().sum();
        if used + left_marker + right_marker <= width || start >= active {
            return (start, start > 0);
        }
        start += 1;
    }
}

/// [`chip_window`] specialised to this strip's [`TabItem`]s.
fn window_start(items: &[TabItem], active: usize, width: usize) -> (usize, bool) {
    let widths: Vec<usize> = items.iter().map(chip_width).collect();
    chip_window(&widths, active, width)
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
        let w = chip_width(item);
        // Reserve a column for the right edge marker when more tabs follow — for
        // EVERY tab (incl. the active one), so a tab that would land exactly at
        // `width` while more tabs follow is clipped, guaranteeing a free column
        // for the `›` marker (Mn2: the exactly-full case). `w` already counts the
        // chip edges + trailing gap.
        let more_follow = i < items.len() - 1;
        let reserve = usize::from(more_follow);
        if used + w + reserve > width {
            clipped_right = true;
            break;
        }
        // Active chip = bright `selection` fill; inactive = the dim `tab_inactive`
        // fill (both carry a real bg, so every chip reads as filled).
        let style = if i == active {
            theme.selection
        } else {
            theme.tab_inactive
        };
        // Chip: `▐` left edge, the label, `▌` right edge — all on the chip bg —
        // then a raw space gap. The label is split so the dirty `●` can carry the
        // accent foreground while still sitting on the chip bg.
        spans.push(Span::styled(CHIP_LEFT, style));
        if item.dirty {
            let name = truncate(&item.short_name, MAX_NAME);
            spans.push(Span::styled(format!(" {name} "), style));
            spans.push(Span::styled("●", style.patch(theme.accent)));
            spans.push(Span::styled(" ", style));
        } else {
            spans.push(Span::styled(tab_label(item), style));
        }
        spans.push(Span::styled(CHIP_RIGHT, style));
        spans.push(Span::raw(" "));
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

    #[test]
    fn chip_width_counts_edges_and_gap() {
        // Label " x " is 3 cols; a chip adds `▐` + `▌` + the trailing gap = 3.
        assert_eq!(label_width(&item("x", false)), 3);
        assert_eq!(chip_width(&item("x", false)), 3 + CHIP_OVERHEAD);
        assert_eq!(CHIP_OVERHEAD, 3, "▐ + ▌ + one gap space");
        // A dirty chip still counts its `●` label plus the same framing.
        assert_eq!(
            chip_width(&item("x", true)),
            label_width(&item("x", true)) + CHIP_OVERHEAD
        );
    }

    /// Mn2: when the active tab is the last-visible and tabs still FOLLOW it,
    /// `window_start` reserves a right-marker column so the render loop's matching
    /// reserve can never clip the active tab — the window fits `start..=active`
    /// into `width - 1` (right marker), guaranteeing a free column for `›`. The
    /// widths here are CHIP widths (label 3 + edges/gap 3 = 6 each).
    #[test]
    fn window_reserves_right_marker_when_more_follow() {
        // 1-char names → label " x " = 3 cols → chip = 6 cols each.
        let items = vec![item("a", false), item("b", false), item("c", false)];
        // active = 1 (middle), more follow (c). width chosen so start..=active
        // (a,b = 12) fits exactly, but +1 right marker forces the window to drop
        // `a`.
        let (start, left) = window_start(&items, 1, 12);
        assert_eq!(start, 1, "right-marker reserve drops the leading tab");
        assert!(left, "a left marker shows when clipped-left");

        // With one extra column (13) both a and b + right marker fit.
        let (start, _) = window_start(&items, 1, 13);
        assert_eq!(start, 0, "extra column lets the leading tab stay");

        // No tabs follow the active (active is last) → no right reserve, so with
        // width 13 the last two chips (b,c = 12) + left marker (1) fit at start=1.
        let (start, _) = window_start(&items, 2, 13);
        assert_eq!(start, 1, "no right reserve when active is the last tab");
    }

    /// The chip framing widens tabs, so a set that previously fit at the old
    /// label widths now overflows: the window must clip (start advances) rather
    /// than paint past `width`, and it must keep the active chip visible.
    #[test]
    fn wider_chips_clip_where_bare_labels_fit() {
        // Four chips of 6 cols each (1-char names). Bare labels (3 each) would fit
        // 4 tabs in 12 cols; chips (6 each) do not.
        let items = vec![
            item("a", false),
            item("b", false),
            item("c", false),
            item("d", false),
        ];
        // Width 12: only two chips fit at all. Active = last (d) → window must
        // scroll so d stays visible (start advances past the earlier tabs).
        let (start, left) = window_start(&items, 3, 12);
        assert!(
            start >= 2,
            "wider chips force the window to scroll: {start}"
        );
        assert!(left, "clipped-left marker shows when scrolled");
        // And the visible span from `start` never exceeds the width (accounting a
        // left marker), so nothing paints past the pane.
        let used: usize = items[start..=3].iter().map(chip_width).sum();
        // +1 for the clipped-left `‹` marker; the window must fit within width.
        assert!(used < 12, "active chip window (+ left marker) fits: {used}");
    }
}
