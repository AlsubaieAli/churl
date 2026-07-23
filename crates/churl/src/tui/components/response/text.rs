//! Response-viewer text-processing helpers: body reformatting (pretty JSON),
//! display sanitization, line indexing, wrap expansion, char-slice / char↔byte
//! mapping, and the shared smart-case matcher. Split out of the response module
//! as a child module; the free fns keep their original module-private
//! scope via `pub(in …response)` so the parent and sibling `render` module reach
//! them by path with no visibility widening.

use super::ResponseView;

/// Reformats a response body for display when the viewer is in pretty mode.
/// JSON, XML, and HTML each get their own reformatter (M8.7 adds XML/HTML;
/// JSON shipped first, M7.7); every other syntax (`Plain`) is returned
/// unchanged. On **any** parse error — or when `pretty` is off — the original
/// `text` is returned unchanged (silent raw fallback; never errors, never
/// panics). Returns an owned string so the caller can store it as the
/// displayed body.
///
/// `sort_keys` only ever applies to the JSON arm (A→Z object-key sort,
/// recursive; arrays keep element order) — XML/HTML have no analogous notion
/// and ignore it.
///
/// This is a DISPLAY-ONLY transform: `raw_text`/`raw_bytes` (what copy and
/// save read) are built from the untouched body and never pass through here —
/// see `ResponseView::build`.
pub(in crate::tui::components::response) fn reformat_body_if_needed(
    text: &str,
    syntax: crate::tui::highlight::SyntaxToken,
    pretty: bool,
    sort_keys: bool,
) -> String {
    use crate::tui::highlight::SyntaxToken;
    if !pretty {
        return text.to_owned();
    }
    match syntax {
        SyntaxToken::Json => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(mut value) => {
                if sort_keys {
                    sort_value_keys(&mut value);
                }
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_owned())
            }
            Err(_) => text.to_owned(),
        },
        SyntaxToken::Xml => reformat_xml(text).unwrap_or_else(|| text.to_owned()),
        // HTML pretty-print lands in a follow-up commit (html5ever, gated on
        // a `cargo deny check` pass) — raw for now, same as `Plain`.
        SyntaxToken::Html => text.to_owned(),
        SyntaxToken::Plain => text.to_owned(),
    }
}

/// Re-emits `text` as indented XML via `quick-xml`'s event stream (read every
/// event, write it back through an indenting `Writer`) — whitespace is largely
/// insignificant in XML, so a straight re-serialization reads fine. `None` on
/// any parse error (malformed XML, or non-UTF-8-safe output), which the caller
/// folds into the same silent raw fallback every reformatter uses.
fn reformat_xml(text: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    use quick_xml::writer::Writer;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(event) => writer.write_event(event).ok()?,
            Err(_) => return None,
        }
    }
    String::from_utf8(writer.into_inner()).ok()
}

/// Recursively sorts the keys of every JSON object in `value` A→Z, in place.
/// Object entries are rebuilt in sorted order (and each value recursed into);
/// array elements keep their order but are each recursed into; scalars are left
/// untouched. Relies on `serde_json`'s `preserve_order` feature so the rebuilt
/// map's iteration order is the sorted insertion order we write here.
fn sort_value_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            // Drain into a sorted vec, then reinsert in key order. `std::mem::take`
            // leaves an empty map we can push the sorted entries back into.
            let taken = std::mem::take(map);
            let mut entries: Vec<(String, serde_json::Value)> = taken.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            for (k, mut v) in entries {
                sort_value_keys(&mut v);
                map.insert(k, v);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                sort_value_keys(item);
            }
        }
        _ => {}
    }
}

/// Tab-stop width used when expanding `\t` for display. A fixed constant, NOT a
/// config knob: tabs advance to the next multiple of this many columns.
const TAB_WIDTH: usize = 4;

/// The visible placeholder substituted for control characters that would
/// otherwise be invisible or hostile in the terminal (U+00B7 MIDDLE DOT).
const CONTROL_PLACEHOLDER: char = '·';

/// Sanitizes the *displayed* body/text so a hostile or careless server can never
/// move the cursor, recolour the pane, write the clipboard (OSC 52), or smuggle
/// invisible control bytes into the viewer. Applied to the reformatted text
/// *before* `index_lines`; the untouched `raw_text` is what copy reads, so this
/// never affects byte-exact copy. A single left-to-right scan:
///
/// 1. **Strip ANSI escapes** — CSI (`ESC [` … final byte `0x40..=0x7E`), OSC
///    (`ESC ]` … terminated by BEL `0x07` or ST `ESC \`), and any other lone
///    `ESC` — removed entirely (zero-width).
/// 2. **Expand tabs** — `\t` advances to the next multiple of [`TAB_WIDTH`],
///    measured in display columns *within the current logical line* (the column
///    counter resets at each `\n`).
/// 3. **Normalize CR** — `\r\n` collapses to `\n`; a lone `\r` is treated like
///    any other control char (step 4), so no phantom blank line appears.
/// 4. **Replace remaining control chars** — every other C0 (`0x00..=0x1F`), DEL
///    (`0x7F`), and C1 (`0x80..=0x9F`) becomes [`CONTROL_PLACEHOLDER`]. `\n` and
///    the already-expanded `\t` are exempt.
///
/// Correct JSON has its controls escaped, so pretty (serde) output is already
/// clean — the teeth are on the raw / parse-failed / `text/plain` path. Clean
/// text is returned byte-identical. Runs once per response build, not per frame.
pub(in crate::tui::components::response) fn sanitize_for_display(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Display column within the current logical line, for tab-stop math.
    let mut col = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\u{1B}' => {
                // ANSI escape. Peek the introducer to decide the terminator rule.
                match chars.peek() {
                    Some('[') => {
                        // CSI: consume until a final byte in 0x40..=0x7E.
                        chars.next();
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7E}').contains(&c) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        // OSC: consume until BEL (0x07) or ST (ESC \).
                        chars.next();
                        while let Some(c) = chars.next() {
                            if c == '\u{07}' {
                                break;
                            }
                            if c == '\u{1B}' {
                                // ST is ESC \; swallow the trailing backslash when present.
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                        }
                    }
                    // A lone ESC (or ESC followed by anything else): drop just the
                    // ESC; the following char is handled by the next iteration.
                    _ => {}
                }
            }
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\r' => {
                // `\r\n` → `\n`; a lone `\r` → placeholder (a control char).
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    out.push('\n');
                    col = 0;
                } else {
                    out.push(CONTROL_PLACEHOLDER);
                    col += 1;
                }
            }
            '\t' => {
                // Advance to the next tab stop (next multiple of TAB_WIDTH).
                let next_stop = (col / TAB_WIDTH + 1) * TAB_WIDTH;
                for _ in col..next_stop {
                    out.push(' ');
                }
                col = next_stop;
            }
            // Remaining C0, DEL, and C1 control chars → the visible placeholder.
            c if is_control_char(c) => {
                out.push(CONTROL_PLACEHOLDER);
                col += 1;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

/// Whether `c` is a control character that must be replaced by the sanitizer:
/// any C0 (`0x00..=0x1F`), DEL (`0x7F`), or C1 (`0x80..=0x9F`). `\n` and `\t` are
/// handled earlier and never reach here.
fn is_control_char(c: char) -> bool {
    matches!(c, '\u{00}'..='\u{1F}' | '\u{7F}' | '\u{80}'..='\u{9F}')
}

/// Indexes the line starts of `text` in one pass (byte offset of each line's
/// first byte). Empty for empty input.
pub(in crate::tui::components::response) fn index_lines(text: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    if !text.is_empty() {
        offsets.push(0);
        for (index, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                offsets.push(index + 1);
            }
        }
    }
    offsets
}

/// A single display row after wrap expansion: the visible-map index it came from
/// plus the char range `[start, end)` of the logical line it shows. Without wrap
/// each visible line maps to exactly one display row spanning the whole line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui::components::response) struct DisplayRow {
    /// Index into the visible map.
    pub(in crate::tui::components::response) visible_idx: usize,
    /// Char offset (not byte) where this display row starts within the logical line.
    pub(in crate::tui::components::response) char_start: usize,
    /// Char offset where it ends (exclusive).
    pub(in crate::tui::components::response) char_end: usize,
}

/// Expands the visible map into display rows for a given wrap width. When wrap is
/// off (or `width == 0`), each visible line is exactly one display row; a normal
/// line's row is the horizontal window `[h_scroll, h_scroll + width)`,
/// while a fold-header keeps its full width. When wrap is on, long logical lines
/// split at display-width boundaries (`unicode-width`); fold-headers never wrap.
///
/// The row *count* is unchanged by h_scroll (still one row per unwrapped line),
/// so the fold/wrap→logical mapping helpers stay correct; only the char span
/// each unwrapped normal row exposes is bounded.
pub(in crate::tui::components::response) fn expand_wrap(
    view: &ResponseView,
    visible: &[super::Visible],
    wrap: bool,
    width: usize,
    h_scroll: usize,
) -> Vec<DisplayRow> {
    use super::Visible;
    let mut rows = Vec::new();
    for (vi, v) in visible.iter().enumerate() {
        let logical = v.logical();
        let char_len = view.logical_line(logical).chars().count();
        let is_fold_header = matches!(v, Visible::FoldHeader { .. });
        if !wrap || width == 0 || is_fold_header {
            // Unwrapped: one row per line. A normal line is windowed to the
            // horizontal scroll span (fold-headers and `width == 0` geometry
            // callers keep the full line).
            let (char_start, char_end) = if !wrap && width > 0 && !is_fold_header {
                let start = h_scroll.min(char_len);
                let end = start.saturating_add(width).min(char_len);
                (start, end)
            } else {
                (0, char_len)
            };
            rows.push(DisplayRow {
                visible_idx: vi,
                char_start,
                char_end,
            });
            continue;
        }
        let line = view.logical_line(logical);
        if line.is_empty() {
            rows.push(DisplayRow {
                visible_idx: vi,
                char_start: 0,
                char_end: 0,
            });
            continue;
        }
        // Greedy width-aware split.
        let mut start_char = 0usize;
        let mut cells = 0usize;
        let mut count_from_start = 0usize;
        for (ci, ch) in line.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cells + w > width && count_from_start > 0 {
                rows.push(DisplayRow {
                    visible_idx: vi,
                    char_start: start_char,
                    char_end: ci,
                });
                start_char = ci;
                cells = 0;
                count_from_start = 0;
            }
            cells += w;
            count_from_start += 1;
        }
        // Trailing partial row.
        rows.push(DisplayRow {
            visible_idx: vi,
            char_start: start_char,
            char_end: char_len,
        });
    }
    rows
}

/// Clamps a scroll offset so the last screenful of `total` lines stays in view.
pub fn clamp_scroll(scroll: usize, total: usize, height: usize) -> usize {
    scroll.min(total.saturating_sub(height))
}

/// The number of decimal digits in `n` (at least 1, so `0` → 1). Drives the
/// stable gutter width from the total displayed line count.
pub(in crate::tui::components::response) fn digit_count(n: usize) -> usize {
    let mut n = n;
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// A char-index slice of `s` (`start..end` in chars).
pub(in crate::tui::components::response) fn char_slice(s: &str, start: usize, end: usize) -> &str {
    let (bs, be) = char_range_to_bytes(s, start, end);
    &s[bs..be]
}

/// The shared smart-case substring matcher used by both the response body
/// search and the `?` help-overlay search — one algorithm, so their matching
/// semantics can never fork. Yields `(line index, byte start, byte end)` per
/// match, in reading order, over the given `lines`.
///
/// Smart-case: an all-lowercase `query` matches case-insensitively; any
/// uppercase char makes it case-sensitive. Case-insensitive matching searches a
/// lowercased copy of each line and maps match positions back through a per-char
/// byte-offset table — exact for any case folding, including per-char length
/// shifts whose totals cancel out (e.g. `İ` 2→3 and `ẞ` 3→2 on one line). No
/// length heuristics, no whole-line fallback. An empty query yields no matches.
pub(crate) fn smart_case_matches<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    query: &str,
) -> Vec<(usize, usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let case_sensitive = query.chars().any(|c| c.is_uppercase());
    let needle_lower = query.to_lowercase();
    let mut out = Vec::new();
    for (idx, line) in lines.into_iter().enumerate() {
        if case_sensitive {
            let mut start = 0;
            while let Some(pos) = line[start..].find(query) {
                let at = start + pos;
                out.push((idx, at, at + query.len()));
                start = at + query.len().max(1);
                if start > line.len() {
                    break;
                }
            }
        } else {
            let (hay, offset_map) = lowercase_with_offsets(line);
            let mut start = 0;
            while start <= hay.len().saturating_sub(needle_lower.len()) {
                let Some(pos) = hay[start..].find(&needle_lower) else {
                    break;
                };
                let at = start + pos;
                let orig_start = map_lowered_offset(&offset_map, at, false);
                let orig_end = map_lowered_offset(&offset_map, at + needle_lower.len(), true);
                if orig_start < orig_end {
                    out.push((idx, orig_start, orig_end));
                }
                start = at + needle_lower.len().max(1);
            }
        }
    }
    out
}

/// Lowercases `s`, returning the lowered string plus a byte-offset mapping
/// table: one `(lowered_offset, original_offset)` entry per *original* char,
/// closed by a `(lowered.len(), s.len())` sentinel. Exact for any case folding,
/// including chars whose lowercase form has a different byte length.
fn lowercase_with_offsets(s: &str) -> (String, Vec<(usize, usize)>) {
    let mut lowered = String::with_capacity(s.len());
    let mut map: Vec<(usize, usize)> = Vec::new();
    for (orig_off, ch) in s.char_indices() {
        map.push((lowered.len(), orig_off));
        for lc in ch.to_lowercase() {
            lowered.push(lc);
        }
    }
    map.push((lowered.len(), s.len()));
    (lowered, map)
}

/// Maps a byte offset in the lowered string back to an original-string offset
/// via the table from [`lowercase_with_offsets`]. An offset inside a multi-byte
/// lowercase expansion rounds outward: down to the containing original char's
/// start (`round_up == false`) or up to the next char boundary (`round_up ==
/// true`), so a mapped range always covers the matched region.
fn map_lowered_offset(map: &[(usize, usize)], lowered_off: usize, round_up: bool) -> usize {
    // partition_point: first index whose lowered offset exceeds `lowered_off`.
    let idx = map.partition_point(|&(lo, _)| lo <= lowered_off);
    // `idx` is at least 1 (the first entry is (0, 0) and 0 <= lowered_off).
    let (lo, orig) = map[idx - 1];
    if lo == lowered_off || !round_up {
        orig
    } else {
        // Inside an expansion: round up to the next original char boundary.
        map.get(idx).map(|&(_, o)| o).unwrap_or(orig)
    }
}

/// Maps a char range to a byte range within `s`.
pub(in crate::tui::components::response) fn char_range_to_bytes(
    s: &str,
    start: usize,
    end: usize,
) -> (usize, usize) {
    let mut bs = s.len();
    let mut be = s.len();
    for (ci, (bi, _)) in s.char_indices().enumerate() {
        if ci == start {
            bs = bi;
        }
        if ci == end {
            be = bi;
            break;
        }
    }
    if start == 0 {
        bs = 0;
    }
    (bs.min(s.len()), be.min(s.len()))
}
