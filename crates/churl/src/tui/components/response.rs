//! Response pane: a virtualised viewer over an executed response body.
//!
//! The body is stored once as a lossy-UTF-8 [`String`] plus a `Vec<usize>` of
//! line-start byte offsets (single pass); rendering slices only the visible
//! lines, so a 1 MB response is never materialised into a `Vec<Line>`. Syntax
//! highlighting is layered on by the off-thread worker
//! ([`crate::tui::highlight`]): a cache hit draws coloured lines, a miss draws
//! plain text and enqueues a highlight job.
//!
//! ## Display pipeline (M7)
//!
//! Three pure transforms over the logical lines (snapshot-testable without a
//! runtime):
//!
//! ```text
//! logical lines (body or headers text, CRLF-stripped)
//!   → fold filter      (JSON-only; visible logical lines, folded regions elided)
//!   → wrap expansion    (optional; each display row = a slice of one logical line)
//!   → viewport slice    (scroll offset + height)
//! ```
//!
//! **Cursor** and **scroll** are display-row indices (post-fold, post-wrap).
//! Search matches are stored against *logical* lines and mapped through the
//! pipeline for navigation and highlighting.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use churl_core::model::{Request, Response, Timing};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use super::fold::{self, FoldRegion};
use crate::tui::highlight::{HighlightJob, SyntaxToken};
use crate::tui::theme::Theme;

/// Immutable metadata about a request, captured at send time so history and the
/// error view need nothing from live app state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseMeta {
    /// HTTP method string, e.g. `"GET"`.
    pub method: String,
    /// Requested URL (verbatim; no templating in M3).
    pub url: String,
    /// Workspace-relative path of the originating endpoint file, when known.
    pub endpoint_path: Option<String>,
    /// Send time as Unix milliseconds.
    pub executed_at_ms: i64,
}

/// Which body the viewer shows. Reset to [`ViewMode::Body`] on each new response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// The response body (default).
    Body,
    /// The response headers, one `name: value` per line.
    Headers,
}

/// The response pane's state machine.
#[derive(Debug)]
pub enum ResponseState {
    /// No request has been sent yet.
    Idle,
    /// A request is in flight; `started` drives the elapsed-time readout.
    InFlight {
        /// When the in-flight request began.
        started: Instant,
    },
    /// The last request was cancelled by the user.
    Cancelled,
    /// The last request failed; carries the stringified error and its metadata.
    Failed {
        /// Human-readable error.
        error: String,
        /// Metadata of the failed request.
        meta: ResponseMeta,
    },
    /// A response arrived and is ready to view.
    Done {
        /// The virtualised view over the response body.
        view: ResponseView,
    },
    /// A completed response whose body was deliberately not retained by the
    /// load runner's memory bound (R0). The status/timing/size are kept for an
    /// honest placeholder, but the body bytes are gone and NOT reconstructable —
    /// selecting this row shows a "not retained" note instead of the viewer.
    Dropped {
        /// The HTTP status code.
        status: u16,
        /// The request timing, when it completed with one.
        timing: Option<Duration>,
        /// The response body size in bytes (for the placeholder).
        size: usize,
    },
}

impl ResponseState {
    /// The idle default as a `const`, so `&self` accessors can return a
    /// `'static` reference when nothing is loaded.
    pub const IDLE: ResponseState = ResponseState::Idle;
}

/// A literal-substring search over the current view's logical lines. Matches are
/// stored as `(logical line, byte range within that line)`.
#[derive(Debug, Default, Clone)]
pub struct SearchState {
    /// The current query text.
    pub query: String,
    /// `(logical line, byte start, byte end)` per match, in reading order.
    matches: Vec<(usize, usize, usize)>,
    /// Index of the current match within `matches`, when there is one.
    current: Option<usize>,
}

impl SearchState {
    /// The number of matches.
    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// The 1-based ordinal of the current match, when any.
    pub fn current_ordinal(&self) -> Option<usize> {
        self.current.map(|i| i + 1)
    }
}

/// A visible logical line after the fold filter: either a real line, or a
/// fold-header standing in for a collapsed region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visible {
    /// A normal logical line at this index.
    Line(usize),
    /// A folded region's opener line; the region hides `hidden` inner lines.
    FoldHeader {
        /// The opener logical line index.
        line: usize,
        /// How many inner lines are hidden (for the `⋯ N lines` suffix).
        hidden: usize,
    },
}

impl Visible {
    /// The logical line index this visible entry maps to.
    fn logical(self) -> usize {
        match self {
            Visible::Line(l) => l,
            Visible::FoldHeader { line, .. } => line,
        }
    }
}

/// A virtualised view over one arrived response body, built once on arrival.
/// Also owns the per-response viewer UI state (view mode, folds, wrap, search)
/// so it all resets together when a new response replaces it.
#[derive(Debug)]
pub struct ResponseView {
    /// Lossy-UTF-8 decode of the full body.
    text: String,
    /// Byte offset of each line start. Empty for an empty body.
    line_offsets: Vec<usize>,
    /// The headers rendered as `name: value` lines, built lazily on first use.
    headers_text: String,
    /// Line-start offsets into `headers_text`, built alongside it.
    headers_offsets: Vec<usize>,
    /// Syntax token derived from the response `Content-Type`.
    syntax: SyntaxToken,
    /// Raw body size in bytes (the truncated size when `truncated` — what we
    /// actually hold).
    byte_size: usize,
    /// Whether the body was cut off at the configured size cap.
    truncated: bool,
    /// HTTP status code.
    status: u16,
    /// Coarse timing.
    timing: Timing,
    /// Number of response headers (shown as a count in the status line).
    header_count: usize,
    /// Response generation, part of the viewport cache key.
    generation: u64,
    /// Which body is showing (body vs headers).
    view_mode: ViewMode,
    /// JSON fold regions over the body, scanned lazily on first fold action.
    /// `None` until scanned; empty when the body is not JSON or has none.
    folds: Option<Vec<FoldRegion>>,
    /// Opener line indices of the regions currently folded (body only).
    folded: HashSet<usize>,
    /// Whether soft-wrap is on.
    wrap: bool,
    /// The active body search, when a search is live.
    search: Option<SearchState>,
}

impl ResponseView {
    /// Builds the view from a response, indexing its line starts in one pass.
    pub fn build(response: &Response, generation: u64) -> Self {
        let text = String::from_utf8_lossy(&response.body).into_owned();
        let line_offsets = index_lines(&text);
        let content_type = response
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("content-type"))
            .map(|header| header.value.as_str());
        // Build the headers text eagerly — it is tiny and lets the headers view
        // reuse the exact same pipeline with no lazy-init branches.
        let headers_text = response
            .headers
            .iter()
            .map(|h| format!("{}: {}", h.name, h.value))
            .collect::<Vec<_>>()
            .join("\n");
        let headers_offsets = index_lines(&headers_text);
        Self {
            syntax: SyntaxToken::from_content_type(content_type),
            byte_size: response.body.len(),
            truncated: response.truncated,
            status: response.status,
            timing: response.timing,
            header_count: response.headers.len(),
            generation,
            line_offsets,
            text,
            headers_text,
            headers_offsets,
            view_mode: ViewMode::Body,
            folds: None,
            folded: HashSet::new(),
            wrap: false,
            search: None,
        }
    }

    /// The number of logical lines in the *body*.
    pub fn line_count(&self) -> usize {
        self.line_offsets.len()
    }

    /// The syntax token detected for the body.
    pub fn syntax(&self) -> SyntaxToken {
        self.syntax
    }

    /// The response generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The current view mode.
    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// Whether wrap is on.
    pub fn wrap(&self) -> bool {
        self.wrap
    }

    /// Whether the body was truncated at the size cap (only meaningful for the
    /// body view).
    pub fn truncated(&self) -> bool {
        self.truncated && self.view_mode == ViewMode::Body
    }

    /// The active search, if any.
    pub fn search(&self) -> Option<&SearchState> {
        self.search.as_ref()
    }

    /// The HTTP status code of this response.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The retained body size in bytes (the truncated size when truncated).
    pub fn body_len(&self) -> usize {
        self.byte_size
    }

    /// The logical line shown at display-row `row`, computed through the current
    /// fold/wrap pipeline at pane `width` (0 = unwrapped geometry). Used to map
    /// the cursor row back to a logical line for fold-at-cursor and copy-line.
    pub fn logical_at_display_row(&self, row: usize, width: usize) -> Option<usize> {
        let visible = self.visible_map();
        let rows = expand_wrap(self, &visible, self.wrap, width);
        rows.get(row).map(|dr| visible[dr.visible_idx].logical())
    }

    /// The display-row index of the first row showing logical line `logical`,
    /// through the current pipeline at `width`. Used to move the cursor onto a
    /// search match. Returns `None` when the line is not visible (should not
    /// happen after auto-unfold).
    pub fn display_row_for_logical(&self, logical: usize, width: usize) -> Option<usize> {
        let visible = self.visible_map();
        let rows = expand_wrap(self, &visible, self.wrap, width);
        rows.iter()
            .position(|dr| visible[dr.visible_idx].logical() == logical)
    }

    /// The total number of display rows through the current pipeline at `width`.
    pub fn total_display_rows(&self, width: usize) -> usize {
        let visible = self.visible_map();
        expand_wrap(self, &visible, self.wrap, width).len()
    }

    // ---- source selection (body vs headers) ----

    /// The active logical text (body or headers) per the view mode.
    fn source_text(&self) -> &str {
        match self.view_mode {
            ViewMode::Body => &self.text,
            ViewMode::Headers => &self.headers_text,
        }
    }

    /// The line offsets for the active source.
    fn source_offsets(&self) -> &[usize] {
        match self.view_mode {
            ViewMode::Body => &self.line_offsets,
            ViewMode::Headers => &self.headers_offsets,
        }
    }

    /// The number of logical lines in the active source.
    fn source_line_count(&self) -> usize {
        self.source_offsets().len()
    }

    /// Returns the `idx`-th logical line of the active source, without its
    /// trailing newline **and with a trailing `\r` stripped** (CRLF bodies).
    /// Panics if `idx` is out of range (callers slice within
    /// [`ResponseView::source_line_count`]).
    fn logical_line(&self, idx: usize) -> &str {
        let offsets = self.source_offsets();
        let text = self.source_text();
        let start = offsets[idx];
        let end = offsets
            .get(idx + 1)
            .map(|&next| next - 1)
            .unwrap_or(text.len());
        let line = &text[start..end];
        line.strip_suffix('\r').unwrap_or(line)
    }

    // ---- toggles ----

    /// Toggles between the body and headers view. Clears the live search (its
    /// matches are view-specific) and returns the new mode. Fold state is body-
    /// specific and untouched (inert in the headers view anyway).
    pub fn toggle_view_mode(&mut self) -> ViewMode {
        self.view_mode = match self.view_mode {
            ViewMode::Body => ViewMode::Headers,
            ViewMode::Headers => ViewMode::Body,
        };
        self.search = None;
        self.view_mode
    }

    /// Toggles soft-wrap.
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
    }

    // ---- folding ----

    /// Whether the current view supports folding (JSON body only).
    pub fn folding_supported(&self) -> bool {
        self.view_mode == ViewMode::Body && self.syntax == SyntaxToken::Json
    }

    /// Ensures the fold regions are scanned (lazy, once per response).
    fn ensure_folds(&mut self) -> &[FoldRegion] {
        if self.folds.is_none() {
            let regions = if self.syntax == SyntaxToken::Json {
                fold::scan_regions(&self.text, &self.line_offsets)
            } else {
                Vec::new()
            };
            self.folds = Some(regions);
        }
        self.folds.as_deref().unwrap()
    }

    /// Folds or unfolds the innermost region whose opener is at (or whose span
    /// covers) `logical` — the cursor's logical line. No-op when nothing covers
    /// it. Returns `true` when a region was toggled.
    pub fn toggle_fold_at(&mut self, logical: usize) -> bool {
        let regions = self.ensure_folds().to_vec();
        // The innermost covering region is the one with the largest opener that
        // still contains `logical`.
        let target = regions
            .iter()
            .filter(|r| r.opener <= logical && logical <= r.closer)
            .max_by_key(|r| r.opener);
        let Some(region) = target else {
            return false;
        };
        if self.folded.contains(&region.opener) {
            self.folded.remove(&region.opener);
        } else {
            self.folded.insert(region.opener);
        }
        true
    }

    /// Collapse-all when any region is open, else expand-all — but only over
    /// *top-level* regions (those not nested inside another). Returns `true` when
    /// the view supports folding (so the caller knows whether to no-op-notify).
    pub fn toggle_all_folds(&mut self) -> bool {
        if !self.folding_supported() {
            return false;
        }
        let regions = self.ensure_folds().to_vec();
        let top_level: Vec<usize> = regions
            .iter()
            .filter(|r| {
                !regions
                    .iter()
                    .any(|o| o.opener < r.opener && r.closer <= o.closer)
            })
            .map(|r| r.opener)
            .collect();
        let any_open = top_level.iter().any(|op| !self.folded.contains(op));
        if any_open {
            for op in top_level {
                self.folded.insert(op);
            }
        } else {
            // Expand everything (top-level and nested).
            self.folded.clear();
        }
        true
    }

    /// If `logical` sits inside a folded region, unfold that region (used by
    /// search auto-unfold so a match is never hidden). Returns `true` if it
    /// unfolded something.
    fn unfold_covering(&mut self, logical: usize) -> bool {
        let Some(regions) = self.folds.clone() else {
            return false;
        };
        let mut changed = false;
        // Unfold every folded region strictly containing this line (nested folds
        // may all need opening).
        let openers: Vec<usize> = self.folded.iter().copied().collect();
        for op in openers {
            if let Some(region) = regions.iter().find(|r| r.opener == op)
                && region.opener < logical
                && logical <= region.closer
            {
                self.folded.remove(&op);
                changed = true;
            }
        }
        changed
    }

    // ---- the visible map (fold filter) ----

    /// The visible logical lines after applying the fold filter. Lines strictly
    /// inside a folded region are elided; the opener becomes a fold-header. In
    /// the headers view (or non-JSON) this is a 1:1 pass over the logical lines.
    fn visible_map(&self) -> Vec<Visible> {
        let n = self.source_line_count();
        if self.view_mode != ViewMode::Body || self.folded.is_empty() {
            return (0..n).map(Visible::Line).collect();
        }
        let regions = match &self.folds {
            Some(r) => r,
            None => return (0..n).map(Visible::Line).collect(),
        };
        // Precompute, per folded opener, its closer.
        let mut out = Vec::new();
        let mut line = 0;
        while line < n {
            // Is this line a folded opener? Pick the region with the widest span
            // opening here (so an outer fold hides inner state).
            let folded_here = regions
                .iter()
                .filter(|r| r.opener == line && self.folded.contains(&r.opener))
                .max_by_key(|r| r.closer);
            if let Some(region) = folded_here {
                out.push(Visible::FoldHeader {
                    line,
                    hidden: region.hidden_count(),
                });
                line = region.closer + 1;
            } else {
                out.push(Visible::Line(line));
                line += 1;
            }
        }
        out
    }

    // ---- search ----

    /// Recomputes matches for `query` against the current view's logical lines
    /// (smart-case: an all-lowercase query is case-insensitive, any uppercase is
    /// case-sensitive). Sets the current match to the first one and auto-unfolds
    /// the region covering it (same path as `n`/`N` navigation), so incremental
    /// typing jumps to a match even inside a collapsed fold. An empty query
    /// clears matches but keeps the search live (the input is still open).
    pub fn set_search(&mut self, query: String) {
        let matches = self.compute_matches(&query);
        let current = (!matches.is_empty()).then_some(0);
        let first_line = matches.first().map(|&(line, _, _)| line);
        self.search = Some(SearchState {
            query,
            matches,
            current,
        });
        if let Some(line) = first_line {
            self.unfold_covering(line);
        }
    }

    /// Clears the live search entirely.
    pub fn clear_search(&mut self) {
        self.search = None;
    }

    /// Computes literal, smart-case substring matches over the active source.
    fn compute_matches(&self, query: &str) -> Vec<(usize, usize, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        let case_sensitive = query.chars().any(|c| c.is_uppercase());
        let needle_lower = query.to_lowercase();
        let mut out = Vec::new();
        for idx in 0..self.source_line_count() {
            let line = self.logical_line(idx);
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
                // Case-insensitive: search a lowercased copy and map every match
                // position back through a per-char byte-offset table — exact for
                // any case folding, including per-char length shifts whose totals
                // cancel out (e.g. `İ` 2→3 and `ẞ` 3→2 on one line). No length
                // heuristics, no whole-line fallback.
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

    /// Advances to the next (`forward`) or previous match, wrapping. Auto-unfolds
    /// the region covering the new match. Returns the new current match's logical
    /// line so the caller can scroll it into view, or `None` when no matches.
    pub fn step_match(&mut self, forward: bool) -> Option<usize> {
        let next = {
            let search = self.search.as_ref()?;
            if search.matches.is_empty() {
                return None;
            }
            let len = search.matches.len();
            match search.current {
                Some(i) if forward => (i + 1) % len,
                Some(i) => (i + len - 1) % len,
                None => 0,
            }
        };
        let logical = {
            let search = self.search.as_mut()?;
            search.current = Some(next);
            search.matches[next].0
        };
        self.unfold_covering(logical);
        Some(logical)
    }

    /// The logical line of the current match, if any.
    pub fn current_match_line(&self) -> Option<usize> {
        let search = self.search.as_ref()?;
        let i = search.current?;
        Some(search.matches[i].0)
    }

    // ---- copy payloads ----

    /// The full text of the current view (body as decoded, or the headers text),
    /// for the copy-view action.
    pub fn copy_all(&self) -> &str {
        self.source_text()
    }

    /// The text of the logical line at `logical` (the copy-line action). Returns
    /// an owned string because CRLF stripping may borrow a sub-slice.
    pub fn copy_line(&self, logical: usize) -> String {
        if logical < self.source_line_count() {
            self.logical_line(logical).to_owned()
        } else {
            String::new()
        }
    }
}

/// Indexes the line starts of `text` in one pass (byte offset of each line's
/// first byte). Empty for empty input.
fn index_lines(text: &str) -> Vec<usize> {
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
struct DisplayRow {
    /// Index into the visible map.
    visible_idx: usize,
    /// Char offset (not byte) where this display row starts within the logical line.
    char_start: usize,
    /// Char offset where it ends (exclusive).
    char_end: usize,
}

/// Expands the visible map into display rows for a given wrap width. When wrap is
/// off (or `width == 0`), each visible line is exactly one display row covering
/// the whole line. When on, long logical lines split at display-width boundaries
/// (`unicode-width`); fold-headers never wrap.
fn expand_wrap(
    view: &ResponseView,
    visible: &[Visible],
    wrap: bool,
    width: usize,
) -> Vec<DisplayRow> {
    let mut rows = Vec::new();
    for (vi, v) in visible.iter().enumerate() {
        let logical = v.logical();
        let char_len = view.logical_line(logical).chars().count();
        if !wrap || width == 0 || matches!(v, Visible::FoldHeader { .. }) {
            rows.push(DisplayRow {
                visible_idx: vi,
                char_start: 0,
                char_end: char_len,
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

    // For Done state, embed the stats as a right-aligned block title.
    let stats_title: Option<String> = if let ResponseState::Done { view } = ctx.state {
        Some(status_summary(view))
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

    // For Done state, use the full inner area as body (no status_area split).
    // For non-Done states, keep the status_area split for status messages.
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
        ResponseState::Failed { error, .. } => {
            let status_area = status_area_opt.unwrap();
            let body_area = body_area_opt.unwrap();
            frame.render_widget(Paragraph::new(Line::from("request failed")), status_area);
            frame.render_widget(
                Paragraph::new(vec![Line::from("error:"), Line::from(error.clone())])
                    .wrap(Wrap { trim: false }),
                body_area,
            );
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
    let width = body_area.width as usize;
    let height = body_area.height as usize;
    let visible = view.visible_map();
    let rows = expand_wrap(view, &visible, view.wrap, width);
    let total = rows.len();
    outcome.total_rows = total;
    outcome.viewport_width = width;

    // Clamp cursor and scroll into range; keep the cursor within the viewport.
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
    // a fold/wrap/mode change re-highlights.
    let hash = viewport_hash(view, scroll, height);
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
        out_lines.push(line);
    }

    frame.render_widget(Paragraph::new(out_lines), body_area);

    // Enqueue a highlight job on a cache miss (unwrapped mode only).
    if !view.wrap && highlighted.is_none() {
        let job_lines: Vec<String> = rows[scroll..end]
            .iter()
            .map(|row| match visible[row.visible_idx] {
                Visible::FoldHeader { line, .. } => view.logical_line(line).to_owned(),
                Visible::Line(logical) => view.logical_line(logical).to_owned(),
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
fn build_search_spans(
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

/// Applies the cursor-row style to a line when `is_cursor`. Reuses
/// `theme.selection` (no new slot). Layered under search styling.
fn apply_cursor<'a>(line: Line<'a>, is_cursor: bool, theme: &Theme) -> Line<'a> {
    if is_cursor {
        line.style(theme.selection)
    } else {
        line
    }
}

/// A char-index slice of `s` (`start..end` in chars).
fn char_slice(s: &str, start: usize, end: usize) -> &str {
    let (bs, be) = char_range_to_bytes(s, start, end);
    &s[bs..be]
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
fn char_range_to_bytes(s: &str, start: usize, end: usize) -> (usize, usize) {
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
        ResponseState::Done { view } => status_summary(view),
    };
    Line::from(text)
}

/// The one-line status summary for a completed response (e.g.
/// `200 OK · 142 ms · 4.1 KB · 3 headers`), with view-mode markers (`· headers`,
/// `· wrap`), search feedback, and a ` · truncated at <size>` suffix when the
/// body hit the configured cap.
fn status_summary(view: &ResponseView) -> String {
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
    // count earns its spot (owner call). In headers view the `· headers` marker
    // below already shows the state, so omit the hint there.
    let headers = if view.view_mode == ViewMode::Body {
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
/// scroll, and height. Width is excluded (unwrapped mode truncates at draw time;
/// wrapped mode bypasses the cache entirely).
fn viewport_hash(view: &ResponseView, scroll: usize, height: usize) -> u64 {
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
    view.syntax.hash(&mut hasher);
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

#[cfg(test)]
mod tests {
    use super::*;
    use churl_core::model::{Header, Response};

    fn view(body: &str) -> ResponseView {
        response_with(body, Vec::new(), false)
    }

    fn response_with(body: &str, headers: Vec<Header>, truncated: bool) -> ResponseView {
        let response = Response {
            status: 200,
            headers,
            body: body.as_bytes().to_vec(),
            truncated,
            timing: Timing {
                connect: None,
                total: Duration::from_millis(5),
            },
        };
        ResponseView::build(&response, 1)
    }

    fn json_view(body: &str) -> ResponseView {
        let response = Response {
            status: 200,
            headers: vec![Header {
                name: "Content-Type".to_owned(),
                value: "application/json".to_owned(),
                enabled: true,
            }],
            body: body.as_bytes().to_vec(),
            truncated: false,
            timing: Timing {
                connect: None,
                total: Duration::from_millis(5),
            },
        };
        ResponseView::build(&response, 1)
    }

    #[test]
    fn line_offsets_split_multiline_body() {
        let view = view("a\nbb\nccc");
        assert_eq!(view.line_count(), 3);
        assert_eq!(view.logical_line(0), "a");
        assert_eq!(view.logical_line(1), "bb");
        assert_eq!(view.logical_line(2), "ccc");
    }

    #[test]
    fn empty_body_has_no_lines() {
        assert_eq!(view("").line_count(), 0);
    }

    #[test]
    fn crlf_stripped_from_logical_lines() {
        let view = view("a\r\nb\r\nc");
        assert_eq!(view.logical_line(0), "a");
        assert_eq!(view.logical_line(1), "b");
        assert_eq!(view.logical_line(2), "c");
    }

    #[test]
    fn scroll_clamps_to_last_screenful() {
        assert_eq!(clamp_scroll(0, 10, 4), 0);
        assert_eq!(clamp_scroll(6, 10, 4), 6);
        assert_eq!(clamp_scroll(9, 10, 4), 6);
        assert_eq!(clamp_scroll(3, 2, 4), 0);
    }

    #[test]
    fn byte_size_formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(4198), "4.1 KB");
        assert_eq!(fmt_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    #[test]
    fn status_summary_markers() {
        let mut v = view("body");
        // The `· headers` view-mode marker (distinct from the `N headers` count).
        assert!(!status_summary(&v).contains("· headers"));
        assert!(!status_summary(&v).contains("wrap"));
        v.toggle_wrap();
        assert!(status_summary(&v).contains("· wrap"));
        v.view_mode = ViewMode::Headers;
        assert!(status_summary(&v).contains("· headers"));
    }

    #[test]
    fn status_summary_appends_truncated_marker() {
        let mut v = view("body");
        assert!(!status_summary(&v).contains("truncated"));
        v.truncated = true;
        v.byte_size = 10 * 1024 * 1024;
        assert_eq!(
            status_summary(&v),
            "200 OK · 5 ms · 10.0 MB · 0 headers  [h] · truncated at 10.0 MB"
        );
    }

    #[test]
    fn status_summary_pluralizes_header_count() {
        let header = |name: &str| Header {
            name: name.to_owned(),
            value: "x".to_owned(),
            enabled: true,
        };
        // 0 headers → plural.
        let zero = response_with("body", Vec::new(), false);
        assert!(status_summary(&zero).contains("· 0 headers"));
        // 1 header → singular.
        let one = response_with("body", vec![header("A")], false);
        assert!(status_summary(&one).contains("· 1 header"));
        assert!(!status_summary(&one).contains("1 headers"));
        // 2 headers → plural.
        let two = response_with("body", vec![header("A"), header("B")], false);
        assert!(status_summary(&two).contains("· 2 headers"));
        // Body view appends the `[h]` full-headers affordance after the count.
        assert!(status_summary(&two).contains("2 headers  [h]"));
    }

    #[test]
    fn headers_view_is_alternate_source() {
        let headers = vec![
            Header {
                name: "Content-Type".to_owned(),
                value: "application/json".to_owned(),
                enabled: true,
            },
            Header {
                name: "X-Req".to_owned(),
                value: "42".to_owned(),
                enabled: true,
            },
        ];
        let mut v = response_with("{}", headers, false);
        assert_eq!(v.view_mode(), ViewMode::Body);
        assert_eq!(v.toggle_view_mode(), ViewMode::Headers);
        assert_eq!(v.source_line_count(), 2);
        assert_eq!(v.logical_line(0), "Content-Type: application/json");
        assert_eq!(v.logical_line(1), "X-Req: 42");
    }

    #[test]
    fn folding_only_for_json_body() {
        let mut plain = view("{\n1\n}");
        assert!(!plain.folding_supported());
        assert!(!plain.toggle_all_folds());
        let mut json = json_view("{\n  \"a\": 1\n}");
        assert!(json.folding_supported());
        assert!(json.toggle_all_folds());
    }

    #[test]
    fn fold_at_cursor_hides_inner_lines() {
        let mut v = json_view("{\n  \"a\": 1,\n  \"b\": 2\n}");
        // 4 logical lines; fold the region opening at line 0.
        assert!(v.toggle_fold_at(0));
        let visible = v.visible_map();
        // Opener header + nothing else (closer is inside the folded span).
        assert_eq!(visible.len(), 1);
        assert!(matches!(
            visible[0],
            Visible::FoldHeader { line: 0, hidden: 3 }
        ));
        // Unfolding restores all 4 lines.
        assert!(v.toggle_fold_at(0));
        assert_eq!(v.visible_map().len(), 4);
    }

    #[test]
    fn nested_fold_outer_hides_inner_state() {
        let body = "{\n  \"o\": {\n    \"i\": 1\n  }\n}";
        let mut v = json_view(body);
        // Fold inner (opener line 1) then outer (line 0).
        assert!(v.toggle_fold_at(1));
        assert!(v.toggle_fold_at(0));
        // Only the outer header shows.
        let visible = v.visible_map();
        assert_eq!(visible.len(), 1);
        assert!(matches!(visible[0], Visible::FoldHeader { line: 0, .. }));
        // Unfolding the outer restores the inner *folded* state (line 1 header).
        assert!(v.toggle_fold_at(0));
        let visible = v.visible_map();
        assert!(
            visible
                .iter()
                .any(|x| matches!(x, Visible::FoldHeader { line: 1, .. }))
        );
    }

    #[test]
    fn toggle_all_collapses_then_expands() {
        let body = "{\n  \"a\": 1\n}\n[\n  2\n]";
        let mut v = json_view(body);
        // Two top-level regions. Collapse all.
        assert!(v.toggle_all_folds());
        let headers = v
            .visible_map()
            .iter()
            .filter(|x| matches!(x, Visible::FoldHeader { .. }))
            .count();
        assert_eq!(headers, 2);
        // Expand all.
        assert!(v.toggle_all_folds());
        assert_eq!(
            v.visible_map()
                .iter()
                .filter(|x| matches!(x, Visible::FoldHeader { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn wrap_expansion_splits_long_lines() {
        let v = view("abcdefghij");
        let visible = v.visible_map();
        // Width 4 → 3 rows (4+4+2).
        let rows = expand_wrap(&v, &visible, true, 4);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 4));
        assert_eq!((rows[1].char_start, rows[1].char_end), (4, 8));
        assert_eq!((rows[2].char_start, rows[2].char_end), (8, 10));
    }

    #[test]
    fn wrap_off_is_one_row_per_line() {
        let v = view("a\nbb\nccc");
        let visible = v.visible_map();
        let rows = expand_wrap(&v, &visible, false, 2);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn wrap_wide_chars_respect_width() {
        // Two 2-cell CJK chars per row at width 4.
        let v = view("日本語漢字");
        let visible = v.visible_map();
        let rows = expand_wrap(&v, &visible, true, 4);
        // 5 wide chars * 2 cells = 10 cells; width 4 → 2,2,1 chars → 3 rows.
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 2));
        assert_eq!((rows[1].char_start, rows[1].char_end), (2, 4));
        assert_eq!((rows[2].char_start, rows[2].char_end), (4, 5));
    }

    #[test]
    fn wrap_exact_width_line_is_one_row() {
        let v = view("abcd");
        let visible = v.visible_map();
        let rows = expand_wrap(&v, &visible, true, 4);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 4));
    }

    #[test]
    fn smart_case_insensitive_and_sensitive() {
        let mut v = view("Hello hello HELLO");
        // Lowercase query → case-insensitive: 3 matches.
        v.set_search("hello".to_owned());
        assert_eq!(v.search().unwrap().count(), 3);
        // Uppercase in query → case-sensitive: only "Hello".
        v.set_search("Hello".to_owned());
        assert_eq!(v.search().unwrap().count(), 1);
    }

    #[test]
    fn case_insensitive_match_with_length_shifting_folds() {
        // `İ` (2 bytes) lowercases to 3 bytes; `ẞ` (3 bytes) to 2 — the totals
        // cancel, which used to defeat the equal-length heuristic. The mapping
        // table must yield exactly one match with the *original* byte range.
        let line = "İneedleẞ";
        let v = view(line);
        let mut v = v;
        v.set_search("needle".to_owned());
        let search = v.search().unwrap();
        assert_eq!(search.count(), 1);
        let (l, s, e) = search.matches[0];
        assert_eq!(l, 0);
        assert_eq!(&line[s..e], "needle");
        assert_eq!((s, e), (2, 8));
    }

    #[test]
    fn case_insensitive_counts_every_occurrence() {
        let mut v = view("BOB and BOB");
        v.set_search("bob".to_owned());
        assert_eq!(v.search().unwrap().count(), 2);
        let m = &v.search().unwrap().matches;
        assert_eq!((m[0].1, m[0].2), (0, 3));
        assert_eq!((m[1].1, m[1].2), (8, 11));
    }

    #[test]
    fn set_search_auto_unfolds_first_match() {
        // Fold everything, then start typing a query whose first match is
        // buried inside the fold — set_search itself must unfold it (the
        // incremental-typing jump, not just n/N).
        let body = "{\n  \"needle\": 1,\n  \"b\": 2\n}";
        let mut v = json_view(body);
        assert!(v.toggle_all_folds()); // collapse all
        assert_eq!(v.visible_map().len(), 1);
        v.set_search("needle".to_owned());
        assert_eq!(v.current_match_line(), Some(1));
        assert!(
            v.visible_map().iter().any(|x| x.logical() == 1),
            "set_search must auto-unfold the region covering match 1"
        );
    }

    #[test]
    fn search_step_wraps_around() {
        let mut v = view("x\nx\nx");
        v.set_search("x".to_owned());
        assert_eq!(v.search().unwrap().count(), 3);
        assert_eq!(v.current_match_line(), Some(0));
        assert_eq!(v.step_match(true), Some(1));
        assert_eq!(v.step_match(true), Some(2));
        // Wrap forward → back to 0.
        assert_eq!(v.step_match(true), Some(0));
        // Wrap backward → 2.
        assert_eq!(v.step_match(false), Some(2));
    }

    #[test]
    fn search_no_matches_keeps_search_live() {
        let mut v = view("abc");
        v.set_search("zzz".to_owned());
        assert_eq!(v.search().unwrap().count(), 0);
        assert!(v.search().is_some());
        assert!(status_summary(&v).contains("no matches"));
    }

    #[test]
    fn search_auto_unfolds_match() {
        let body = "{\n  \"needle\": 1,\n  \"b\": 2\n}";
        let mut v = json_view(body);
        assert!(v.toggle_fold_at(0)); // fold the outer object
        assert_eq!(v.visible_map().len(), 1); // only the header shows
        v.set_search("needle".to_owned());
        // The only match is on line 1 (inside the fold); step to it.
        let line = v.step_match(true).or_else(|| v.current_match_line());
        assert_eq!(line, Some(1));
        // Auto-unfold happened → the match line is now visible.
        let visible = v.visible_map();
        assert!(visible.iter().any(|x| x.logical() == 1));
    }

    #[test]
    fn view_toggle_clears_search() {
        let mut v = view("hello");
        v.set_search("hello".to_owned());
        assert!(v.search().is_some());
        v.toggle_view_mode();
        assert!(v.search().is_none());
    }

    #[test]
    fn copy_all_and_line() {
        let v = view("line one\nline two");
        assert_eq!(v.copy_all(), "line one\nline two");
        assert_eq!(v.copy_line(1), "line two");
        assert_eq!(v.copy_line(99), "");
    }

    #[test]
    fn build_search_spans_splits_around_match() {
        // "ab[cd]ef": match bytes 2..4 of the slice starting at byte 0.
        let line = build_search_spans("abcdef", 0, &[(2, 4, false)]);
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].content, "ab");
        assert_eq!(line.spans[1].content, "cd");
        assert_eq!(line.spans[2].content, "ef");
    }
}
