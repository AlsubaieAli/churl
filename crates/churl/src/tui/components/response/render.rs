//! The response pane's render/draw pipeline: the `render` entry point and its
//! private draw helpers (fold → wrap → viewport, search overlays, gutter,
//! status summaries). Split out of the response module (M7.11) as a child
//! module, so it keeps full access to `ResponseView`'s private fields and
//! methods with no visibility widening.

use std::collections::HashMap;
use std::time::Duration;

use churl_core::model::Request;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use super::text::{DisplayRow, char_range_to_bytes, char_slice, clamp_scroll};
use super::{ResponseState, ResponseView, ViewMode, Visible, failed_request_line};
use crate::tui::highlight::HighlightJob;
use crate::tui::theme::Theme;

/// What [`render`] needs from the app, borrowed for the duration of the draw.
pub struct RenderCtx<'a> {
    /// The response pane state.
    pub state: &'a ResponseState,
    /// The selected request (shown in the idle placeholder), if any.
    pub request: Option<&'a Request>,
    /// Whether the pane is focused.
    pub focused: bool,
    /// The desired scroll offset (clamped during render).
    pub scroll: usize,
    /// The cursor display-row index (clamped during render).
    pub cursor: usize,
    /// The viewport-hash → highlighted-lines cache.
    pub cache: &'a HashMap<u64, Vec<Line<'static>>>,
    /// The colour theme.
    pub theme: &'a Theme,
    /// The jump-mode label for this pane, when jump-mode is active and it fit.
    pub jump_label: Option<char>,
    /// Monotonic tick counter for the spinner animation (tests can set to 0).
    pub tick_count: u64,
}

/// The result of a response-pane render: a highlight job to enqueue (on a cache
/// miss over a `Done` state), the clamped scroll offset, cursor, and body height.
pub struct RenderOutcome {
    /// A job the caller should enqueue on the highlight worker, if any.
    pub job: Option<HighlightJob>,
    /// The scroll offset after clamping to the viewport.
    pub clamped_scroll: usize,
    /// The cursor display-row after clamping.
    pub clamped_cursor: usize,
    /// The height (in rows) of the body viewport, for half-page scrolling.
    pub viewport_height: usize,
    /// The width (in cols) of the body viewport, for the caller's cursor→logical
    /// mapping (wrap geometry depends on it).
    pub viewport_width: usize,
    /// Total display rows in the current view (post-fold, post-wrap), for
    /// cursor/scroll motion clamping by the caller.
    pub total_rows: usize,
    /// The horizontal scroll offset after clamping to the widest visible line
    /// (M7.7). The caller writes it back onto the view so an over-pan self-corrects
    /// on the next frame; `0` while wrap is on.
    pub clamped_h_scroll: usize,
}

/// Renders the response pane. Pure aside from the returned enqueue request; the
/// caller decides whether a highlight worker exists (none under `TestBackend`,
/// so snapshots deterministically show plain text).
pub fn render(frame: &mut Frame, area: Rect, ctx: RenderCtx) -> RenderOutcome {
    let (border_type, border_style) = if ctx.focused {
        (BorderType::Thick, ctx.theme.border_focused)
    } else {
        (BorderType::Plain, ctx.theme.border_unfocused)
    };
    let left_title = match ctx.jump_label {
        Some(label) => format!(" Response [{label}] "),
        None => " Response ".to_owned(),
    };

    // Done state: stats as a right-aligned block title.
    let stats_title: Option<String> = if let ResponseState::Done { view } = ctx.state {
        Some(status_summary(view, ctx.focused))
    } else {
        None
    };

    let mut block = Block::bordered()
        .border_type(border_type)
        .border_style(border_style)
        .title(left_title)
        .title_style(ctx.theme.title);
    if let Some(ref stats) = stats_title {
        block = block.title(Line::from(format!(" {stats} ")).right_aligned());
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Done state uses the full inner area as body; other states split off a
    // status_area for their status message.
    let (viewport_height, body_area_opt, status_area_opt) = if stats_title.is_some() {
        (inner.height as usize, Some(inner), None)
    } else {
        let [status_area, body_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
        (
            body_area.height as usize,
            Some(body_area),
            Some(status_area),
        )
    };

    let mut outcome = RenderOutcome {
        job: None,
        clamped_scroll: ctx.scroll,
        clamped_cursor: ctx.cursor,
        viewport_height,
        viewport_width: 0,
        total_rows: 0,
        clamped_h_scroll: 0,
    };

    match ctx.state {
        ResponseState::Idle => {
            let status_area = status_area_opt.unwrap();
            let body_area = body_area_opt.unwrap();
            frame.render_widget(Paragraph::new(Line::from("no response yet")), status_area);
            let body = match ctx.request {
                Some(request) => vec![
                    Line::from(format!("{} {}", request.method, request.url)),
                    Line::from(""),
                    Line::from("press ctrl-s to send"),
                ],
                None => vec![Line::from(""), Line::from("no endpoint selected")],
            };
            frame.render_widget(Paragraph::new(body), body_area);
        }
        ResponseState::InFlight { started } => {
            let status_area = status_area_opt.unwrap();
            let body_area = body_area_opt.unwrap();
            let elapsed = started.elapsed().as_millis();
            let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
            let spinner = spinner_chars[(ctx.tick_count as usize) % spinner_chars.len()];
            frame.render_widget(
                Paragraph::new(Line::from(format!(
                    "sending… {elapsed} ms · ctrl-c to cancel"
                ))),
                status_area,
            );
            frame.render_widget(
                Paragraph::new(Line::from(format!("{spinner} waiting for response…"))),
                body_area,
            );
        }
        ResponseState::Cancelled => {
            let status_area = status_area_opt.unwrap();
            let body_area = body_area_opt.unwrap();
            frame.render_widget(Paragraph::new(Line::from("cancelled")), status_area);
            frame.render_widget(Paragraph::new(Line::from("request cancelled")), body_area);
        }
        ResponseState::Failed { error, meta } => {
            let status_area = status_area_opt.unwrap();
            let body_area = body_area_opt.unwrap();
            frame.render_widget(Paragraph::new(Line::from("request failed")), status_area);
            // Honest failure panel (drive-test #4a): the request method+URL (when
            // known) and the error. A TRANSPORT failure has no HTTP status, body,
            // or timing — there was no response — so we show none rather than a
            // fabricated 0. `press y to copy` mirrors the yank the copy handler
            // now supports on this state.
            let mut lines = Vec::new();
            if let Some(req) = failed_request_line(meta) {
                lines.push(Line::from(req));
                lines.push(Line::from(""));
            }
            lines.push(Line::from("error:"));
            lines.push(Line::from(error.clone()));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "no response — status, body, and timing are unavailable",
            ));
            lines.push(Line::from("press y to copy the error"));
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_area);
        }
        ResponseState::Dropped {
            status,
            timing,
            size,
        } => {
            let status_area = status_area_opt.unwrap();
            let body_area = body_area_opt.unwrap();
            frame.render_widget(
                Paragraph::new(Line::from(dropped_status(*status, *timing, *size))),
                status_area,
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("response body not retained (memory-bounded)"),
                    Line::from(""),
                    Line::from("only a bounded window of load-test responses is kept;"),
                    Line::from("this one was evicted and cannot be shown."),
                ])
                .wrap(Wrap { trim: false }),
                body_area,
            );
        }
        ResponseState::Done { view } => {
            let body_area = body_area_opt.unwrap();
            render_done(frame, body_area, view, &ctx, &mut outcome);
        }
    }

    outcome
}

/// The one-line status readout for a memory-evicted (`Dropped`) load-runner row,
/// e.g. `200 · 142 ms · 4.1 KB · not retained`.
fn dropped_status(status: u16, timing: Option<Duration>, size: usize) -> String {
    let mut parts = vec![status.to_string()];
    if let Some(t) = timing {
        parts.push(fmt_ms(t));
    }
    parts.push(fmt_bytes(size));
    parts.push("not retained".to_owned());
    parts.join(" · ")
}

/// Renders the body of a `Done` response through the full pipeline
/// (fold → wrap → viewport), overlaying the cursor row and search highlights.
fn render_done(
    frame: &mut Frame,
    body_area: Rect,
    view: &ResponseView,
    ctx: &RenderCtx,
    outcome: &mut RenderOutcome,
) {
    let area_width = body_area.width as usize;
    let height = body_area.height as usize;
    // A3: the fold-filtered visible map, the wrap expansion, and the
    // widest-visible-line scan below are all served from the view's single-slot
    // geometry memo when the signature (generation/mode/fold-set/wrap/width/
    // h_scroll) is unchanged — so an idle 250ms tick no longer re-walks the full
    // body. A signature match yields byte-identical geometry, so the memo is
    // behaviour-preserving (the snapshots prove it).
    let visible = view.cached_visible_map();
    // The gutter consumes columns from the left; everything downstream (wrap
    // splitting, the h_scroll window, viewport_width, and thus the caller's
    // cursor→logical mapping) uses the reduced body width so the seam is single
    // and consistent (drive-test note #8). `saturating_sub` guards a pane
    // narrower than the gutter — the body simply gets 0 columns rather than wrap.
    let gutter = view.gutter_width().min(area_width);
    let width = area_width.saturating_sub(gutter);
    // Clamp the horizontal scroll so it can't pan past the last screenful of the
    // widest visible line (mirror of `clamp_scroll` for the vertical axis). The
    // caller writes the clamped value back so an over-pan self-corrects.
    let h_scroll = view.h_scroll.min(view.max_h_scroll(width));
    outcome.clamped_h_scroll = h_scroll;
    let rows = view.cached_expand_wrap(view.wrap, width, h_scroll);
    let total = rows.len();
    outcome.total_rows = total;
    // Report the *reduced* body width — the caller feeds it back into
    // logical_at_display_row / display_row_for_logical / ensure_column_visible,
    // so every geometry consumer sees the same gutter-adjusted width.
    outcome.viewport_width = width;

    // Clamp cursor and scroll into range, keeping the cursor within the viewport.
    let cursor = ctx.cursor.min(total.saturating_sub(1));
    let mut scroll = clamp_scroll(ctx.scroll, total, height);
    if cursor < scroll {
        scroll = cursor;
    } else if height > 0 && cursor >= scroll + height {
        scroll = cursor + 1 - height;
    }
    scroll = clamp_scroll(scroll, total, height);
    outcome.clamped_scroll = scroll;
    outcome.clamped_cursor = cursor;

    let end = (scroll + height).min(total);

    // Syntax highlighting operates on the *logical* lines shown in the viewport.
    // The cache key covers generation + view mode + fold/wrap/scroll geometry so
    // a fold/wrap/mode change re-highlights; h_scroll is included so a horizontal
    // pan (which changes the char window each row shows) also re-highlights.
    let hash = viewport_hash(view, scroll, height, h_scroll);
    // The gutter number shows on the FIRST display row of each visible entry; a
    // wrap-continuation row (same visible_idx as the row above) gets a blank,
    // aligned gutter. `char_start == 0` is *not* the predicate — an unwrapped row
    // under h_scroll starts at `char_start == h_scroll > 0` yet is still the sole
    // (first) row of its logical line and must carry a number.
    //
    // Seed `prev_visible_idx` from the row JUST ABOVE the window (`rows[scroll-1]`),
    // not `None`: we only walk the visible slice `rows[scroll..end]`, so a
    // `None` seed would treat the top painted row as a first row even when it is a
    // wrap-continuation whose numbered first row is scrolled off — painting a
    // number where a blank gutter belongs. The seed makes first-row detection
    // correct at the viewport top, uniformly for wrapped and unwrapped rows.
    let mut prev_visible_idx: Option<usize> = scroll
        .checked_sub(1)
        .and_then(|i| rows.get(i))
        .map(|r| r.visible_idx);
    let highlighted: Option<&Vec<Line<'static>>> = if view.wrap {
        // Under wrap we render plain (styled overlays only) — highlighting wrapped
        // spans is deferred (see the module + DECISIONS notes). Skip the cache.
        None
    } else {
        ctx.cache.get(&hash)
    };

    let mut out_lines: Vec<Line> = Vec::with_capacity(end - scroll);
    for (row_idx, row) in rows[scroll..end].iter().enumerate() {
        let display_idx = scroll + row_idx;
        let v = visible[row.visible_idx];
        let is_cursor = ctx.focused && display_idx == cursor;
        let line = match v {
            Visible::FoldHeader { line, hidden } => {
                let opener = view.logical_line(line);
                // Search matches on the opener line itself still highlight (the
                // ⋯ suffix stays unstyled): overlay onto a row covering the
                // whole opener text.
                let header_row = DisplayRow {
                    visible_idx: row.visible_idx,
                    char_start: 0,
                    char_end: opener.chars().count(),
                };
                let mut header =
                    overlay_search(view, line, &header_row, Line::from(opener.to_owned()));
                header.push_span(Span::styled(
                    format!(" ⋯ {hidden} lines"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
                header
            }
            Visible::Line(logical) => {
                render_line(view, logical, row, highlighted, display_idx, scroll)
            }
        };
        let line = apply_cursor(line, is_cursor, ctx.theme);
        // Prepend the gutter AFTER the cursor style so the number stays dim on the
        // cursor row (the gutter is a fixed decoration, not part of the content).
        let line = if gutter > 0 {
            let is_first_row = prev_visible_idx != Some(row.visible_idx);
            // 1-based logical line number of this row's visible entry; a fold-header
            // maps to its opener line, so `logical()` already yields the opener's
            // number. Continuation rows show a blank, right-aligned gutter.
            let label = if is_first_row {
                format!("{:>w$} ", v.logical() + 1, w = gutter - 1)
            } else {
                " ".repeat(gutter)
            };
            prepend_gutter(line, label)
        } else {
            line
        };
        prev_visible_idx = Some(row.visible_idx);
        out_lines.push(line);
    }

    frame.render_widget(Paragraph::new(out_lines), body_area);

    // Enqueue a highlight job on a cache miss (unwrapped mode only). The lines are
    // the horizontal window each row actually shows (fold-headers keep full width),
    // so the cached highlight lines up 1:1 with the painted slice under h_scroll.
    if !view.wrap && highlighted.is_none() {
        let job_lines: Vec<String> = rows[scroll..end]
            .iter()
            .map(|row| match visible[row.visible_idx] {
                Visible::FoldHeader { line, .. } => view.logical_line(line).to_owned(),
                Visible::Line(logical) => {
                    char_slice(view.logical_line(logical), row.char_start, row.char_end).to_owned()
                }
            })
            .collect();
        outcome.job = Some(HighlightJob {
            hash,
            syntax: view.syntax,
            lines: job_lines,
        });
    }
}

/// Renders one non-fold display row: the highlighted (or plain) slice of the
/// logical line, with search matches overlaid. `row` gives the char slice under
/// wrap; `highlighted` is the cached highlight for the *whole* viewport (indexed
/// by position from `scroll`).
fn render_line<'a>(
    view: &'a ResponseView,
    logical: usize,
    row: &DisplayRow,
    highlighted: Option<&'a Vec<Line<'static>>>,
    display_idx: usize,
    scroll: usize,
) -> Line<'a> {
    let full = view.logical_line(logical);
    // The char slice this display row covers (whole line when unwrapped).
    let slice = char_slice(full, row.char_start, row.char_end);

    // Base line: highlighted spans (unwrapped) or plain text.
    let base: Line = match highlighted {
        Some(lines) => lines
            .get(display_idx - scroll)
            .cloned()
            .unwrap_or_else(|| Line::from(slice.to_owned())),
        None => Line::from(slice.to_owned()),
    };

    // Overlay search matches for this logical line onto the slice.
    overlay_search(view, logical, row, base)
}

/// Overlays search-match styling onto `base` for the char range this row covers.
/// The current match gets `theme.selection`; others get a dim underline. When
/// there is no search, `base` is returned unchanged.
fn overlay_search<'a>(
    view: &'a ResponseView,
    logical: usize,
    row: &DisplayRow,
    base: Line<'a>,
) -> Line<'a> {
    let Some(search) = view.search() else {
        return base;
    };
    if search.matches.is_empty() {
        return base;
    }
    // Byte range of the slice within the logical line.
    let full = view.logical_line(logical);
    let (slice_byte_start, slice_byte_end) =
        char_range_to_bytes(full, row.char_start, row.char_end);
    // Collect matches intersecting this slice.
    let hits: Vec<(usize, usize, bool)> = search
        .matches
        .iter()
        .enumerate()
        .filter(|(_, (l, _, _))| *l == logical)
        .filter_map(|(mi, (_, ms, me))| {
            let s = (*ms).max(slice_byte_start);
            let e = (*me).min(slice_byte_end);
            (s < e).then(|| (s, e, Some(mi) == search.current))
        })
        .collect();
    if hits.is_empty() {
        return base;
    }

    // Rebuild the slice as spans, splitting at match boundaries. Match styling is
    // applied over the plain slice text: highlight colours are dropped inside a
    // matched region so it stands out. The current match is REVERSED (standing in
    // for `theme.selection` without needing a theme handle in this pure helper);
    // other matches are dim + underlined.
    let slice = &full[slice_byte_start..slice_byte_end];
    build_search_spans(slice, slice_byte_start, &hits)
}

/// Builds a line from `slice` with the given match byte ranges styled. Current
/// match: reversed (stands in for `theme.selection` without a theme handle);
/// others: dim + underlined. Byte offsets in `hits` are absolute (into the
/// logical line); `base_byte` is the slice's start offset.
pub(crate) fn build_search_spans(
    slice: &str,
    base_byte: usize,
    hits: &[(usize, usize, bool)],
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize; // byte offset within `slice`
    // Sort hits by start.
    let mut hits: Vec<(usize, usize, bool)> = hits.to_vec();
    hits.sort_by_key(|(s, _, _)| *s);
    for (ms, me, current) in hits {
        let rel_start = ms.saturating_sub(base_byte);
        let rel_end = me.saturating_sub(base_byte);
        let rel_start = rel_start.min(slice.len());
        let rel_end = rel_end.min(slice.len());
        if rel_start < cursor || rel_start >= rel_end {
            continue;
        }
        if cursor < rel_start {
            spans.push(Span::raw(slice[cursor..rel_start].to_owned()));
        }
        let style = if current {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().add_modifier(Modifier::DIM | Modifier::UNDERLINED)
        };
        spans.push(Span::styled(slice[rel_start..rel_end].to_owned(), style));
        cursor = rel_end;
    }
    if cursor < slice.len() {
        spans.push(Span::raw(slice[cursor..].to_owned()));
    }
    Line::from(spans)
}

/// Prepends the dim line-number `label` to `line` as a leading span (drive-test
/// note #8). The gutter reuses the same `Modifier::DIM` the viewer already uses
/// for secondary text (fold suffixes, non-current search matches).
fn prepend_gutter(line: Line<'_>, label: String) -> Line<'_> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(
        label,
        Style::default().add_modifier(Modifier::DIM),
    ));
    spans.extend(line.spans);
    Line::from(spans)
}

/// Applies the cursor-row style to a line when `is_cursor`. Reuses
/// `theme.selection` (no new slot). Layered under search styling.
fn apply_cursor<'a>(line: Line<'a>, is_cursor: bool, theme: &Theme) -> Line<'a> {
    if is_cursor {
        line.style(theme.selection)
    } else {
        line
    }
}

/// A one-line summary of the response state for the collapsed (1-row) zoom stub.
/// Shown in the request pane's collapsed area when the response pane is zoomed.
pub fn collapsed_summary(state: &ResponseState, _theme: &Theme) -> Line<'static> {
    let text = match state {
        ResponseState::Idle => "no response yet".to_owned(),
        ResponseState::InFlight { .. } => "sending…".to_owned(),
        ResponseState::Cancelled => "cancelled".to_owned(),
        ResponseState::Failed { .. } => "request failed".to_owned(),
        ResponseState::Dropped {
            status,
            timing,
            size,
        } => dropped_status(*status, *timing, *size),
        // A collapsed response is never focused (zoom follows focus), so the
        // `[h]` hint is correctly suppressed here.
        ResponseState::Done { view } => status_summary(view, false),
    };
    Line::from(text)
}

/// The one-line status summary for a completed response (e.g.
/// `200 OK · 142 ms · 4.1 KB · 3 headers`), with view-mode markers (`· headers`,
/// `· wrap`), search feedback, and a ` · truncated at <size>` suffix when the
/// body hit the configured cap.
pub(in crate::tui::components::response) fn status_summary(
    view: &ResponseView,
    focused: bool,
) -> String {
    let phrase = reason_phrase(view.status);
    let status = if phrase.is_empty() {
        view.status.to_string()
    } else {
        format!("{} {}", view.status, phrase)
    };
    let count = if view.header_count == 1 {
        "1 header".to_owned()
    } else {
        format!("{} headers", view.header_count)
    };
    // In body view, append a `[h]` affordance hinting the full-headers key so the
    // count earns its spot (owner call) — but ONLY when this response pane is
    // focused, so the hint doesn't clutter unfocused/embedded viewers (owner
    // drive-test 2026-07-10). In headers view the `· headers` marker below
    // already shows the state, so omit the hint there.
    let headers = if focused && view.view_mode == ViewMode::Body {
        format!("{count}  [h]")
    } else {
        count
    };
    let mut summary = format!(
        "{status} · {} · {} · {headers}",
        fmt_ms(view.timing.total),
        fmt_bytes(view.byte_size),
    );
    if view.view_mode == ViewMode::Headers {
        summary.push_str(" · headers");
    }
    if view.wrap {
        summary.push_str(" · wrap");
    }
    // Sort is only meaningful (and only togglable) on a pretty JSON body; show the
    // marker so an active A→Z key sort isn't invisible (owner drive-test 2026-07-10).
    if view.sort_keys {
        summary.push_str(" · sorted");
    }
    if let Some(search) = view.search() {
        if search.count() > 0 {
            match search.current_ordinal() {
                Some(ord) => summary.push_str(&format!(" · {ord}/{}", search.count())),
                None => summary.push_str(&format!(" · {} matches", search.count())),
            }
        } else if !search.query.is_empty() {
            summary.push_str(" · no matches");
        }
    }
    if view.truncated {
        summary.push_str(&format!(" · truncated at {}", fmt_bytes(view.byte_size)));
    }
    summary
}

/// The viewport cache key: response generation, view mode, fold signature,
/// scroll, height, and horizontal scroll. Width is excluded (unwrapped mode
/// windows at draw time; wrapped mode bypasses the cache entirely), but h_scroll
/// is keyed since panning changes each unwrapped row's char slice.
fn viewport_hash(view: &ResponseView, scroll: usize, height: usize, h_scroll: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    view.generation.hash(&mut hasher);
    (view.view_mode == ViewMode::Headers).hash(&mut hasher);
    // Fold signature: sorted folded openers.
    let mut folded: Vec<usize> = view.folded.iter().copied().collect();
    folded.sort_unstable();
    folded.hash(&mut hasher);
    scroll.hash(&mut hasher);
    height.hash(&mut hasher);
    // Horizontal window: a pan changes each row's char slice, so re-highlight.
    h_scroll.hash(&mut hasher);
    view.syntax.hash(&mut hasher);
    // Pretty and sort_keys both change the displayed text, so they key the cache
    // (belt-and-braces alongside the generation bump their toggles already do).
    view.pretty.hash(&mut hasher);
    view.sort_keys.hash(&mut hasher);
    // The gutter shrinks the effective body width, so each unwrapped row's char
    // window (and each wrapped row's split) differs between gutter-on and -off.
    // Toggling `#` does not bump the generation, so key it here to invalidate the
    // stale highlight cache built at the other width.
    view.line_numbers.hash(&mut hasher);
    hasher.finish()
}

/// A compact byte-size string (`B`/`KB`/`MB`).
pub fn fmt_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f < KB {
        format!("{bytes} B")
    } else if bytes_f < MB {
        format!("{:.1} KB", bytes_f / KB)
    } else {
        format!("{:.1} MB", bytes_f / MB)
    }
}

/// A millisecond duration string.
fn fmt_ms(total: Duration) -> String {
    format!("{} ms", total.as_millis())
}

/// A minimal reason-phrase table for common status codes; unknown codes render
/// bare (no phrase).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}
