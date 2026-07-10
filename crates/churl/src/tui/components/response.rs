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
///
/// The `Done` variant carries the full [`ResponseView`] inline (the common,
/// hot path — read on every render). Boxing it to satisfy `large_enum_variant`
/// would add an allocation + indirection on that path and ripple through ~20
/// match sites for a variant that is almost always the live one, so we keep it
/// inline and silence the lint here.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
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
    /// The body text as *displayed*: the pretty-reformatted body when `pretty`
    /// is on and reformatting succeeded, otherwise the raw decoded body. Line
    /// navigation, folds, wrap, search and highlighting all operate on this.
    text: String,
    /// The raw, on-the-wire body — a lossy-UTF-8 decode of the exact response
    /// bytes, never reformatted. Copy (`y`/`Y`) reads from here so it stays
    /// byte-exact regardless of the pretty toggle (decision 3).
    raw_text: String,
    /// Byte offset of each line start (into `text`). Empty for an empty body.
    line_offsets: Vec<usize>,
    /// Byte offset of each line start (into `raw_text`). Sanitize is
    /// line-count-preserving, so on the **non-reformatted** path (sanitize-only,
    /// no pretty JSON change) this has the same length as `line_offsets` and line
    /// `i` maps 1:1 — copy-line (`Y`) slices `raw_text` here to stay byte-exact.
    raw_line_offsets: Vec<usize>,
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
    /// Whether the left-hand line-number gutter is shown (drive-test note #8).
    /// Defaults to `true` (gutter on) and persists across pretty/sort/wrap/fold
    /// toggles for the life of the view — a new response builds a fresh view, so
    /// the default-on contract is re-established per response. Render-only: it
    /// shrinks the effective body width (see [`render_done`]) but never touches
    /// `raw_text`, copy, or the line index.
    line_numbers: bool,
    /// Horizontal scroll offset in *char columns* for unwrapped long lines
    /// (M7.7). When wrap is off, each logical line's display row shows the char
    /// window `[h_scroll, h_scroll + viewport_width)` instead of the whole line,
    /// bounding render cost and letting the user pan a minified/non-JSON line past
    /// the first screenful. Reset to 0 on build, on any generation bump, and on
    /// `ToggleWrap`. Inert while wrap is on (wrapped rows already fit the width).
    h_scroll: usize,
    /// Whether the body is rendered pretty (reformatted) rather than raw.
    /// Defaults to `true` on arrival for json-ish content-types (decision 2),
    /// `false` otherwise. Toggled by `p` in the Response overlay. When on but the
    /// body is not parseable JSON, the reformat silently falls back to raw and
    /// `text == raw_text`.
    pretty: bool,
    /// Whether pretty JSON object keys are sorted A→Z (recursively). Defaults to
    /// `false` (server wire order) and resets per response. Only affects display
    /// when `pretty` is on and the body is JSON; a no-op otherwise. Toggled by
    /// `s` in the Response overlay (M7.7).
    sort_keys: bool,
    /// The active body search, when a search is live.
    search: Option<SearchState>,
}

impl ResponseView {
    /// Builds the view from a response, indexing its line starts in one pass.
    pub fn build(response: &Response, generation: u64) -> Self {
        let raw_text = String::from_utf8_lossy(&response.body).into_owned();
        let content_type = response
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("content-type"))
            .map(|header| header.value.as_str());
        let syntax = SyntaxToken::from_content_type(content_type);
        // Pretty-by-default for json-ish content-types (decision 2); raw for
        // everything else. The reformat is a transform *before* the fold/wrap/
        // viewport stages — `text`/`line_offsets` describe what is displayed.
        let pretty = syntax == SyntaxToken::Json;
        // Wire order by default (decision): sorting is an explicit opt-in via `s`.
        let sort_keys = false;
        // Sanitize the reformatted (displayed) text: strip ANSI, expand tabs, and
        // replace remaining control chars. `raw_text` stays the untouched decode so
        // copy remains byte-exact (M7.7).
        let text = sanitize_for_display(&reformat_body_if_needed(
            &raw_text, syntax, pretty, sort_keys,
        ));
        let line_offsets = index_lines(&text);
        // Indexed once: `raw_text` never changes after build (the pretty/sort
        // toggles only rebuild the displayed `text`). Copy-line reads it on the
        // non-reformatted path so `Y` returns the exact on-the-wire line bytes.
        let raw_line_offsets = index_lines(&raw_text);
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
            syntax,
            byte_size: response.body.len(),
            truncated: response.truncated,
            status: response.status,
            timing: response.timing,
            header_count: response.headers.len(),
            generation,
            line_offsets,
            raw_line_offsets,
            text,
            raw_text,
            headers_text,
            headers_offsets,
            view_mode: ViewMode::Body,
            folds: None,
            folded: HashSet::new(),
            wrap: false,
            line_numbers: true,
            h_scroll: 0,
            pretty,
            sort_keys,
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

    /// Whether the line-number gutter is shown (drive-test note #8).
    pub fn line_numbers(&self) -> bool {
        self.line_numbers
    }

    /// Toggles the line-number gutter (`#`). Render-only — it changes only the
    /// effective body width, so no folds/generation/search reset is needed. The
    /// highlight cache is keyed on `line_numbers` (via `viewport_hash`), so the
    /// changed body width re-highlights on the next frame.
    pub fn toggle_line_numbers(&mut self) {
        self.line_numbers = !self.line_numbers;
    }

    /// The width in columns the gutter consumes when on: the digit count of the
    /// largest 1-based line number that can appear in the current source, plus one
    /// space of padding. `0` when the gutter is off. Computed from the *total*
    /// displayed line count (not the visible slice) so the width is stable while
    /// scrolling and does not jitter as folds open/close.
    fn gutter_width(&self) -> usize {
        if !self.line_numbers {
            return 0;
        }
        let last = self.source_line_count();
        digit_count(last) + 1
    }

    /// Whether the body is currently rendered pretty (reformatted).
    pub fn pretty(&self) -> bool {
        self.pretty
    }

    /// Whether pretty JSON object keys are sorted A→Z. Only affects display while
    /// `pretty` is on and the body is JSON.
    pub fn sort_keys(&self) -> bool {
        self.sort_keys
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
        let rows = expand_wrap(self, &visible, self.wrap, width, self.h_scroll);
        rows.get(row).map(|dr| visible[dr.visible_idx].logical())
    }

    /// The display-row index of the first row showing logical line `logical`,
    /// through the current pipeline at `width`. Used to move the cursor onto a
    /// search match. Returns `None` when the line is not visible (should not
    /// happen after auto-unfold).
    pub fn display_row_for_logical(&self, logical: usize, width: usize) -> Option<usize> {
        let visible = self.visible_map();
        let rows = expand_wrap(self, &visible, self.wrap, width, self.h_scroll);
        rows.iter()
            .position(|dr| visible[dr.visible_idx].logical() == logical)
    }

    /// The total number of display rows through the current pipeline at `width`.
    pub fn total_display_rows(&self, width: usize) -> usize {
        let visible = self.visible_map();
        expand_wrap(self, &visible, self.wrap, width, self.h_scroll).len()
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

    /// Toggles soft-wrap. Resets the horizontal scroll (h_scroll only applies
    /// while wrap is off, and its clamp depends on wrap geometry).
    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.h_scroll = 0;
    }

    /// The current horizontal scroll offset (char columns) for unwrapped lines.
    pub fn h_scroll(&self) -> usize {
        self.h_scroll
    }

    /// Pans the horizontal window left (`right == false`) or right by `amount`
    /// char columns. A no-op while wrap is on. The offset is clamped at render
    /// time against the longest currently-visible line, so this only needs to
    /// guard the left edge; over-scrolling right is corrected on the next render.
    pub fn scroll_h(&mut self, right: bool, amount: usize) {
        if self.wrap {
            return;
        }
        self.h_scroll = if right {
            self.h_scroll.saturating_add(amount)
        } else {
            self.h_scroll.saturating_sub(amount)
        };
    }

    /// Sets the horizontal scroll offset directly (used to clamp it after a
    /// render computes the true max, and by search-into-view).
    pub fn set_h_scroll(&mut self, h_scroll: usize) {
        self.h_scroll = h_scroll;
    }

    /// The maximum lawful horizontal scroll for the current view at pane `width`:
    /// the longest currently-*visible* logical line's char length minus `width`
    /// (so the last screenful of the widest line stays on screen), mirroring
    /// [`clamp_scroll`] for the vertical axis. Fold-headers keep full width, so
    /// their opener line counts. `0` while wrap is on or with no width — h_scroll
    /// is inert there.
    fn max_h_scroll(&self, width: usize) -> usize {
        if self.wrap || width == 0 {
            return 0;
        }
        let visible = self.visible_map();
        let widest = visible
            .iter()
            .map(|v| self.logical_line(v.logical()).chars().count())
            .max()
            .unwrap_or(0);
        widest.saturating_sub(width)
    }

    /// Toggles raw↔pretty rendering of the body (`p`). Rebuilds the displayed
    /// `text` and its line index from the raw body, bumps the generation (so the
    /// viewport highlight cache invalidates), and **resets folds** — fold openers
    /// are position-based and the line layout changes between raw and pretty.
    /// Toggling to raw yields exactly the on-the-wire bytes (`text == raw_text`);
    /// toggling to pretty over unparseable JSON silently keeps the raw text.
    /// Returns the new `pretty` state.
    pub fn toggle_pretty(&mut self) -> bool {
        self.pretty = !self.pretty;
        self.text = sanitize_for_display(&reformat_body_if_needed(
            &self.raw_text,
            self.syntax,
            self.pretty,
            self.sort_keys,
        ));
        self.line_offsets = index_lines(&self.text);
        self.h_scroll = 0;
        self.generation = self.generation.wrapping_add(1);
        self.folds = None;
        self.folded.clear();
        // The line layout changes drastically raw↔pretty, so any live search's
        // `(logical_line, byte_start, byte_end)` matches now point at the wrong
        // lines/offsets. Clear it (same guard as `toggle_view_mode`) rather than
        // leave a lying `k/N` counter and mispainted overlays.
        self.search = None;
        self.pretty
    }

    /// Toggles A→Z alphabetical sorting of pretty JSON object keys (`s`). Like
    /// `toggle_pretty` this is a display-geometry change: it rebuilds the shown
    /// `text`/line index, bumps the generation (highlight-cache invalidation),
    /// **resets folds** (opener positions shift), and **clears any live search**
    /// (its `(line, byte, byte)` matches point at the old layout). `sort_keys` is
    /// independent of `pretty`; when pretty is off (or the body is not JSON) the
    /// reformat is a no-op — the toggle just records the flag for when pretty is
    /// next on. Returns the new `sort_keys` state.
    pub fn toggle_sort_keys(&mut self) -> bool {
        self.sort_keys = !self.sort_keys;
        self.text = sanitize_for_display(&reformat_body_if_needed(
            &self.raw_text,
            self.syntax,
            self.pretty,
            self.sort_keys,
        ));
        self.line_offsets = index_lines(&self.text);
        self.h_scroll = 0;
        self.generation = self.generation.wrapping_add(1);
        self.folds = None;
        self.folded.clear();
        self.search = None;
        self.sort_keys
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
        smart_case_matches(
            (0..self.source_line_count()).map(|idx| self.logical_line(idx)),
            query,
        )
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

    /// The `(char_start, char_end)` column range of the current match within its
    /// logical line, if any (byte offsets mapped to char columns). Used by
    /// horizontal search-into-view so a far-right match on an unwrapped line is
    /// panned into the visible window.
    pub fn current_match_columns(&self) -> Option<(usize, usize)> {
        let search = self.search.as_ref()?;
        let i = search.current?;
        let (logical, byte_start, byte_end) = search.matches[i];
        let line = self.logical_line(logical);
        let char_start = line[..byte_start.min(line.len())].chars().count();
        let char_end = line[..byte_end.min(line.len())].chars().count();
        Some((char_start, char_end))
    }

    /// Adjusts `h_scroll` so the char column range `[start, end)` is inside the
    /// horizontal window `[h_scroll, h_scroll + width)` (M7.7 search-into-view). A
    /// no-op while wrap is on or the range already fits. Mirrors the vertical
    /// scroll-into-view for the match *row*: pans left when the match is off the
    /// left edge, right when it is off the right edge, preferring to reveal the
    /// match start.
    pub fn ensure_column_visible(&mut self, start: usize, end: usize, width: usize) {
        if self.wrap || width == 0 {
            return;
        }
        if start < self.h_scroll {
            // Off the left edge: bring the match start to the left of the window.
            self.h_scroll = start;
        } else if end > self.h_scroll + width {
            // Off the right edge: place the window so the match end is the last
            // visible column (revealing the start when the match fits in `width`).
            self.h_scroll = end.saturating_sub(width);
        }
    }

    // ---- copy payloads ----

    /// The full text of the current view for the copy-view action (`y`). In the
    /// body view this is the **raw on-the-wire** body, NOT the reformatted text —
    /// copy stays byte-exact regardless of the pretty toggle (decision 3). In the
    /// headers view it is the headers text.
    pub fn copy_all(&self) -> &str {
        match self.view_mode {
            ViewMode::Body => &self.raw_text,
            ViewMode::Headers => &self.headers_text,
        }
    }

    /// The text of the logical line at `logical` for the copy-line action (`Y`).
    ///
    /// Copy must stay byte-exact (the D1 invariant): the displayed `text` is
    /// sanitized (ANSI stripped, tabs expanded, controls → `·`, `\r\n` → `\n`), so
    /// returning the *displayed* line would leak sanitized bytes. Instead:
    ///
    /// - **Headers view**: the headers text (no reformat/sanitize applies).
    /// - **Body, non-reformatted path** (`text` derived from `raw_text` by
    ///   sanitize only — detected by equal line counts, since sanitize preserves
    ///   `\n`): return the raw on-the-wire line at the same index, byte-exact
    ///   (raw ANSI/tabs/controls and all). CRLF is left intact here — `Y` copies
    ///   what the wire carried.
    /// - **Body, reformatted path** (pretty JSON active *and* reformat changed the
    ///   line layout → line counts differ): return the displayed pretty line.
    ///   serde-escaped JSON carries no raw control chars to mangle, and the raw
    ///   line index no longer corresponds to the displayed one.
    pub fn copy_line(&self, logical: usize) -> String {
        if self.view_mode == ViewMode::Headers {
            return if logical < self.source_line_count() {
                self.logical_line(logical).to_owned()
            } else {
                String::new()
            };
        }
        // Body view. The non-reformatted path is exactly when the sanitized
        // displayed text and the raw text have the same line count (sanitize never
        // adds/removes a `\n`); the pretty-JSON reformat is the only thing that can
        // change line counts.
        if self.line_offsets.len() == self.raw_line_offsets.len() {
            match self.raw_logical_line(logical) {
                Some(line) => line.to_owned(),
                None => String::new(),
            }
        } else if logical < self.source_line_count() {
            self.logical_line(logical).to_owned()
        } else {
            String::new()
        }
    }

    /// The `idx`-th logical line of the untouched `raw_text` (the on-the-wire
    /// bytes), or `None` when out of range. Unlike [`Self::logical_line`] this does
    /// **not** strip a trailing `\r` — copy-line returns exactly what the wire
    /// carried.
    fn raw_logical_line(&self, idx: usize) -> Option<&str> {
        let start = *self.raw_line_offsets.get(idx)?;
        let end = self
            .raw_line_offsets
            .get(idx + 1)
            .map(|&next| next - 1)
            .unwrap_or(self.raw_text.len());
        Some(&self.raw_text[start..end])
    }
}

/// Reformats a response body for display when the viewer is in pretty mode and
/// the body is JSON (decision 1: JSON-only in v1). Parses `text` as a
/// `serde_json::Value` and re-emits it with `to_string_pretty`. On **any** parse
/// error — or when `pretty` is off, or the syntax is not JSON — the original
/// `text` is returned unchanged (silent raw fallback; never errors, never
/// panics, decision 2). Returns an owned string so the caller can store it as the
/// displayed body.
///
/// When `sort_keys` is set, every JSON object's keys are sorted A→Z recursively
/// before emission (arrays keep element order); otherwise objects keep their
/// server wire order (`serde_json`'s `preserve_order` feature is on, so the
/// parsed `Value` is insertion-ordered).
fn reformat_body_if_needed(
    text: &str,
    syntax: SyntaxToken,
    pretty: bool,
    sort_keys: bool,
) -> String {
    if !pretty || syntax != SyntaxToken::Json {
        return text.to_owned();
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(mut value) => {
            if sort_keys {
                sort_value_keys(&mut value);
            }
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_owned())
        }
        Err(_) => text.to_owned(),
    }
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
/// config knob (M7.7): tabs advance to the next multiple of this many columns.
const TAB_WIDTH: usize = 4;

/// The visible placeholder substituted for control characters that would
/// otherwise be invisible or hostile in the terminal (U+00B7 MIDDLE DOT).
const CONTROL_PLACEHOLDER: char = '·';

/// Sanitizes the *displayed* body/text so a hostile or careless server can never
/// move the cursor, recolour the pane, write the clipboard (OSC 52), or smuggle
/// invisible control bytes into the viewer. Applied to the reformatted text
/// *before* `index_lines`; the untouched `raw_text` is what copy reads, so this
/// never affects byte-exact copy (M7.7). A single left-to-right scan:
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
fn sanitize_for_display(text: &str) -> String {
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
/// off (or `width == 0`), each visible line is exactly one display row; a normal
/// line's row is the horizontal window `[h_scroll, h_scroll + width)` (M7.7),
/// while a fold-header keeps its full width. When wrap is on, long logical lines
/// split at display-width boundaries (`unicode-width`); fold-headers never wrap.
///
/// The row *count* is unchanged by h_scroll (still one row per unwrapped line),
/// so the fold/wrap→logical mapping helpers stay correct; only the char span
/// each unwrapped normal row exposes is bounded.
fn expand_wrap(
    view: &ResponseView,
    visible: &[Visible],
    wrap: bool,
    width: usize,
    h_scroll: usize,
) -> Vec<DisplayRow> {
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

/// The per-response cursor/scroll geometry that lives *outside* the
/// [`ResponseView`] (which owns view-mode/folds/wrap/pretty/search state). One of
/// these is carried by the main-pane endpoint buffer AND by each runner state, so
/// the shared `response_*` action handlers can operate on whichever surface is the
/// active response (note #2: unified viewer). The `h_scroll` lives on the view
/// itself; the highlight cache + pending-highlight guard stay on the endpoint
/// buffer (the runners share it in render).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResponseGeometry {
    /// Body scroll offset (clamped to the viewport at render time).
    pub scroll: usize,
    /// Viewer cursor as a display-row index (post-fold, post-wrap).
    pub cursor: usize,
    /// Total display rows in the viewer as of the last render.
    pub total_rows: usize,
    /// Last rendered body height, for half-page scrolling.
    pub viewport_height: usize,
    /// Last rendered body width, for cursor→logical mapping (wrap geometry).
    pub viewport_width: usize,
}

impl ResponseGeometry {
    /// Writes the render outcome's clamped scroll/cursor + measured geometry back
    /// into this struct after a [`render`] call (the caller still applies the
    /// clamped `h_scroll` to the view + enqueues the highlight job — those live
    /// elsewhere). Shared by the main pane and both runner render paths so the
    /// write-back can never drift.
    pub fn apply_render_outcome(&mut self, outcome: &RenderOutcome) {
        self.scroll = outcome.clamped_scroll;
        self.cursor = outcome.clamped_cursor;
        self.total_rows = outcome.total_rows;
        self.viewport_height = outcome.viewport_height;
        self.viewport_width = outcome.viewport_width;
    }
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
    let area_width = body_area.width as usize;
    let height = body_area.height as usize;
    let visible = view.visible_map();
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
    let rows = expand_wrap(view, &visible, view.wrap, width, h_scroll);
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

/// Applies the cursor-row style to a line when `is_cursor`. Reuses
/// `theme.selection` (no new slot). Layered under search styling.
fn apply_cursor<'a>(line: Line<'a>, is_cursor: bool, theme: &Theme) -> Line<'a> {
    if is_cursor {
        line.style(theme.selection)
    } else {
        line
    }
}

/// The number of decimal digits in `n` (at least 1, so `0` → 1). Drives the
/// stable gutter width from the total displayed line count.
fn digit_count(n: usize) -> usize {
    let mut n = n;
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
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

/// A char-index slice of `s` (`start..end` in chars).
fn char_slice(s: &str, start: usize, end: usize) -> &str {
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
fn status_summary(view: &ResponseView, focused: bool) -> String {
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
        assert!(!status_summary(&v, true).contains("· headers"));
        assert!(!status_summary(&v, true).contains("wrap"));
        v.toggle_wrap();
        assert!(status_summary(&v, true).contains("· wrap"));
        v.view_mode = ViewMode::Headers;
        assert!(status_summary(&v, true).contains("· headers"));
    }

    #[test]
    fn status_summary_appends_truncated_marker() {
        let mut v = view("body");
        assert!(!status_summary(&v, true).contains("truncated"));
        v.truncated = true;
        v.byte_size = 10 * 1024 * 1024;
        assert_eq!(
            status_summary(&v, true),
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
        assert!(status_summary(&zero, true).contains("· 0 headers"));
        // 1 header → singular.
        let one = response_with("body", vec![header("A")], false);
        assert!(status_summary(&one, true).contains("· 1 header"));
        assert!(!status_summary(&one, true).contains("1 headers"));
        // 2 headers → plural.
        let two = response_with("body", vec![header("A"), header("B")], false);
        assert!(status_summary(&two, true).contains("· 2 headers"));
        // Body view appends the `[h]` full-headers affordance after the count —
        // but only when the pane is focused.
        assert!(status_summary(&two, true).contains("2 headers  [h]"));
    }

    #[test]
    fn status_summary_omits_hint_when_unfocused() {
        // The `[h]` full-headers affordance is focus-gated (owner drive-test
        // 2026-07-10): an unfocused Body-view response never shows it, so
        // embedded/collapsed viewers stay uncluttered.
        let v = view("body");
        assert_eq!(v.view_mode, ViewMode::Body);
        assert!(status_summary(&v, true).contains("[h]"));
        assert!(!status_summary(&v, false).contains("[h]"));
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
        let rows = expand_wrap(&v, &visible, true, 4, 0);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 4));
        assert_eq!((rows[1].char_start, rows[1].char_end), (4, 8));
        assert_eq!((rows[2].char_start, rows[2].char_end), (8, 10));
    }

    #[test]
    fn wrap_off_is_one_row_per_line() {
        let v = view("a\nbb\nccc");
        let visible = v.visible_map();
        let rows = expand_wrap(&v, &visible, false, 2, 0);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn wrap_wide_chars_respect_width() {
        // Two 2-cell CJK chars per row at width 4.
        let v = view("日本語漢字");
        let visible = v.visible_map();
        let rows = expand_wrap(&v, &visible, true, 4, 0);
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
        let rows = expand_wrap(&v, &visible, true, 4, 0);
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
        // incremental-typing jump, not just n/N). The reformatter preserves wire
        // key order (preserve_order), so `needle` stays on line 1 after the
        // (already-pretty) body is canonicalised.
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
        assert!(status_summary(&v, true).contains("no matches"));
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

    // ---- M7.7: JSON response reformatter + pretty toggle ----

    #[test]
    fn minified_json_renders_pretty_by_default() {
        // petstore-style: minified single-line JSON must arrive multi-line.
        let v = json_view(r#"{"id":1,"tags":["a","b"],"nested":{"x":true}}"#);
        assert!(v.pretty(), "json-ish content-type defaults to pretty");
        assert!(
            v.line_count() > 1,
            "minified single-line JSON should render as multiple lines, got {}",
            v.line_count()
        );
        // The pretty text is what serde_json emits.
        let expected = serde_json::to_string_pretty(
            &serde_json::from_str::<serde_json::Value>(
                r#"{"id":1,"tags":["a","b"],"nested":{"x":true}}"#,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(v.text, expected);
    }

    #[test]
    fn non_json_content_type_is_not_pretty() {
        // A plain-text body is never reformatted (pretty defaults off).
        let v = view("{\"a\":1}");
        assert!(!v.pretty());
        assert_eq!(v.text, "{\"a\":1}");
    }

    #[test]
    fn toggle_pretty_to_raw_is_byte_exact() {
        let raw = r#"{"id":1,"tags":["a","b"]}"#;
        let mut v = json_view(raw);
        assert!(v.pretty());
        // Displayed text is pretty; raw source is the exact on-the-wire bytes.
        assert_ne!(v.text, raw);
        // Toggle to raw → displayed text is exactly the original bytes.
        assert!(!v.toggle_pretty());
        assert_eq!(v.text, raw);
        assert_eq!(v.copy_all(), raw);
        // Toggle back to pretty → reformatted again, and idempotent.
        assert!(v.toggle_pretty());
        assert_eq!(
            v.text,
            serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(raw).unwrap())
                .unwrap()
        );
    }

    #[test]
    fn malformed_json_falls_back_to_raw_no_panic() {
        // Not valid JSON despite the json content-type: stay raw, never error.
        let raw = "{ this is not json ]";
        let v = json_view(raw);
        assert!(v.pretty(), "pretty flag is on (json content-type)");
        // But the displayed text is the untouched raw body (silent fallback).
        assert_eq!(v.text, raw);
        assert_eq!(v.line_count(), 1);
        assert_eq!(v.copy_all(), raw);
    }

    #[test]
    fn toggle_pretty_bumps_generation_and_resets_folds() {
        let mut v = json_view(r#"{"a":{"b":1},"c":2}"#);
        let gen0 = v.generation();
        // Fold something on the pretty text.
        assert!(v.toggle_all_folds());
        assert!(!v.folded.is_empty());
        // Toggle to raw: generation bumps (cache invalidation) and folds reset.
        v.toggle_pretty();
        assert_ne!(v.generation(), gen0, "generation must bump on toggle");
        assert!(v.folds.is_none(), "folds must reset to unscanned");
        assert!(v.folded.is_empty(), "folded openers must clear");
    }

    #[test]
    fn fold_and_wrap_operate_on_pretty_text() {
        // Minified input → pretty multi-line; folding + wrap work on the result.
        let mut v = json_view(r#"{"outer":{"inner":1,"more":2},"tail":3}"#);
        assert!(v.pretty());
        assert!(v.line_count() > 1);
        // Fold the top-level object.
        assert!(v.folding_supported());
        assert!(v.toggle_all_folds());
        let headers = v
            .visible_map()
            .iter()
            .filter(|x| matches!(x, Visible::FoldHeader { .. }))
            .count();
        assert!(headers >= 1, "a fold header should appear on pretty text");
        // Wrap expansion over the pretty lines yields >= line_count rows.
        v.toggle_all_folds(); // expand back
        let visible = v.visible_map();
        let rows = expand_wrap(&v, &visible, true, 4, 0);
        assert!(rows.len() >= v.line_count());
    }

    #[test]
    fn toggle_pretty_clears_search() {
        // A live search's matches are (logical_line, byte_start, byte_end) against
        // the OLD text; the raw↔pretty line layout differs, so a carried-over
        // search would lie about counts and mispaint overlays. Mirror
        // `view_toggle_clears_search`.
        let mut v = json_view(r#"{"needle":1,"other":2}"#);
        v.set_search("needle".to_owned());
        assert!(v.search().is_some());
        assert!(v.search().unwrap().count() >= 1);
        v.toggle_pretty();
        assert!(v.search().is_none(), "toggle_pretty must clear the search");
    }

    #[test]
    fn truncated_json_falls_back_to_raw_and_keeps_marker() {
        // A body cut off at the size cap is not valid JSON — it must render raw
        // (no panic) while still reporting truncation.
        let response = Response {
            status: 200,
            headers: vec![Header {
                name: "Content-Type".to_owned(),
                value: "application/json".to_owned(),
                enabled: true,
            }],
            // Truncated mid-object: unparseable.
            body: br#"{"items":[{"id":1},{"id":2},{"na"#.to_vec(),
            truncated: true,
            timing: Timing {
                connect: None,
                total: Duration::from_millis(5),
            },
        };
        let v = ResponseView::build(&response, 1);
        assert!(v.pretty(), "json content-type still flags pretty");
        // But the malformed (truncated) body stays raw — no reformat, no panic.
        assert_eq!(v.text, r#"{"items":[{"id":1},{"id":2},{"na"#);
        assert_eq!(v.line_count(), 1);
        assert!(v.truncated(), "truncation marker is preserved");
    }

    #[test]
    fn copy_all_is_raw_even_when_pretty() {
        // `y` copies raw on-the-wire bytes even while the body is shown pretty.
        let raw = r#"{"id":1,"name":"x"}"#;
        let v = json_view(raw);
        assert!(v.pretty());
        assert_ne!(v.text, raw, "displayed text is pretty");
        assert_eq!(v.copy_all(), raw, "copy must be the raw bytes");
    }

    // ---- M7.7: optional A→Z key-sort toggle ----

    #[test]
    fn sort_keys_sorts_object_keys_recursively() {
        // Nested object with out-of-order keys at every level.
        let mut v = json_view(r#"{"z":{"b":1,"a":2},"a":3}"#);
        assert!(v.pretty());
        assert!(!v.sort_keys(), "sort defaults off");
        assert!(v.toggle_sort_keys(), "toggle turns sort on");
        // Keys A→Z at every level: top-level "a" before "z"; inner "a" before "b".
        let expected = serde_json::to_string_pretty(&serde_json::json!({
            "a": 3,
            "z": {"a": 2, "b": 1},
        }))
        .unwrap();
        assert_eq!(v.text, expected);
        // The line where each key appears confirms ordering (a before z at top).
        let a_line = v.text.find("\"a\"").unwrap();
        let z_line = v.text.find("\"z\"").unwrap();
        assert!(a_line < z_line, "top-level a must precede z when sorted");
    }

    #[test]
    fn sort_keys_off_preserves_wire_order() {
        // Same input, sort off → preserve_order keeps server wire order (z first).
        let v = json_view(r#"{"z":{"b":1,"a":2},"a":3}"#);
        assert!(!v.sort_keys());
        let z_pos = v.text.find("\"z\"").unwrap();
        let a_pos = v.text.find("\"a\"").unwrap();
        assert!(z_pos < a_pos, "wire order: z precedes a when sort is off");
        // And inner keys stay wire order too: "b" before "a".
        let b_inner = v.text.find("\"b\"").unwrap();
        let a_inner = v.text.rfind("\"a\"").unwrap();
        assert!(b_inner < a_inner, "inner wire order: b precedes a");
    }

    #[test]
    fn sort_keys_only_affects_pretty() {
        // Non-JSON view: `s` is a display no-op (reformat returns text unchanged),
        // so toggling the flag leaves the displayed text byte-identical.
        let mut v = view(r#"{"z":1,"a":2}"#);
        assert!(!v.pretty(), "plain-text body is not pretty");
        let before = v.text.clone();
        v.toggle_sort_keys();
        assert_eq!(v.text, before, "sort must not alter a non-pretty/raw view");

        // JSON view with pretty toggled OFF: sort flips but text stays raw bytes.
        let raw = r#"{"z":1,"a":2}"#;
        let mut v = json_view(raw);
        v.toggle_pretty(); // now raw
        assert!(!v.pretty());
        let raw_text = v.text.clone();
        assert_eq!(raw_text, raw);
        v.toggle_sort_keys();
        assert_eq!(v.text, raw, "sort is inert while pretty is off");
    }

    #[test]
    fn toggle_sort_keys_clears_search_and_resets_folds() {
        let mut v = json_view(r#"{"needle":{"b":1},"a":2}"#);
        let gen0 = v.generation();
        // Fold something and set a live search on the pretty text.
        assert!(v.toggle_all_folds());
        assert!(!v.folded.is_empty());
        v.set_search("needle".to_owned());
        assert!(v.search().is_some());
        // Pan the horizontal window so we can prove the toggle resets it.
        v.scroll_h(true, 16);
        assert_eq!(v.h_scroll(), 16);
        // Toggle sort: search clears, folds reset, generation bumps, h_scroll resets.
        v.toggle_sort_keys();
        assert!(
            v.search().is_none(),
            "toggle_sort_keys must clear the search"
        );
        assert!(v.folds.is_none(), "folds must reset to unscanned");
        assert!(v.folded.is_empty(), "folded openers must clear");
        assert_ne!(v.generation(), gen0, "generation must bump on toggle");
        assert_eq!(
            v.h_scroll(),
            0,
            "toggle_sort_keys must reset the horizontal scroll (matching toggle_pretty/wrap)"
        );
    }

    #[test]
    fn copy_all_is_raw_even_when_sorted() {
        // `y` copies raw on-the-wire bytes even with pretty + sort both on.
        let raw = r#"{"z":1,"a":2}"#;
        let mut v = json_view(raw);
        assert!(v.pretty());
        assert!(v.toggle_sort_keys());
        assert_ne!(v.text, raw, "displayed text is pretty + sorted");
        assert_eq!(v.copy_all(), raw, "copy must be the exact raw bytes");
    }

    #[test]
    fn sort_keys_preserves_array_order() {
        // Arrays keep element order; only object keys sort. Elements that are
        // objects have their own keys sorted, but the array sequence is intact.
        let mut v = json_view(r#"{"list":[{"z":1,"a":2},{"y":3,"b":4}]}"#);
        assert!(v.toggle_sort_keys());
        let expected = serde_json::to_string_pretty(&serde_json::json!({
            "list": [{"a": 2, "z": 1}, {"b": 4, "y": 3}],
        }))
        .unwrap();
        assert_eq!(v.text, expected);
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

    // ---- M7.7: control-char / ANSI sanitize + explicit tab-width ----

    #[test]
    fn sanitize_strips_ansi_csi_osc_and_lone_esc() {
        // CSI colour + cursor-move sequences, an OSC-52 clipboard write (BEL- and
        // ST-terminated), and a bare ESC — all must vanish, leaving only the text.
        let csi = "a\u{1B}[31mred\u{1B}[0m\u{1B}[2Jb";
        assert_eq!(sanitize_for_display(csi), "aredb");
        // OSC-52 clipboard hijack, BEL-terminated.
        let osc_bel = "x\u{1B}]52;c;aGVsbG8=\u{07}y";
        assert_eq!(sanitize_for_display(osc_bel), "xy");
        // OSC terminated by ST (ESC \).
        let osc_st = "x\u{1B}]0;title\u{1B}\\y";
        assert_eq!(sanitize_for_display(osc_st), "xy");
        // A lone ESC with no introducer is dropped; the following char survives.
        assert_eq!(sanitize_for_display("a\u{1B}b"), "ab");
    }

    #[test]
    fn sanitize_expands_tabs_to_next_stop() {
        // Tab at column 0 → 4 spaces (next multiple of 4).
        assert_eq!(sanitize_for_display("\tx"), "    x");
        // "ab\t" → "ab" is 2 cols, tab advances to col 4 → 2 spaces.
        assert_eq!(sanitize_for_display("ab\tx"), "ab  x");
        // "abcd\t" sits exactly on a stop (col 4) → advances a full TAB_WIDTH.
        assert_eq!(sanitize_for_display("abcd\tx"), "abcd    x");
        // Tab stops reset at each newline: second line's tab starts from col 0.
        assert_eq!(sanitize_for_display("ab\tx\n\ty"), "ab  x\n    y");
        // Two consecutive tabs from col 0 → 4 then 4 more = 8 spaces.
        assert_eq!(sanitize_for_display("\t\tx"), "        x");
    }

    #[test]
    fn sanitize_replaces_control_chars_with_placeholder() {
        // NUL, BEL, VT, and a lone CR → the visible middle-dot placeholder.
        assert_eq!(sanitize_for_display("a\u{00}b"), "a·b");
        assert_eq!(sanitize_for_display("a\u{07}b"), "a·b");
        assert_eq!(sanitize_for_display("a\u{0B}b"), "a·b");
        // A lone CR (not part of CRLF) is a control char → placeholder, NOT a
        // newline (so it must not add a phantom line).
        let out = sanitize_for_display("a\rb");
        assert_eq!(out, "a·b");
        assert_eq!(index_lines(&out).len(), 1);
        // A C1 control (0x85 NEL) is also replaced.
        assert_eq!(sanitize_for_display("a\u{85}b"), "a·b");
    }

    #[test]
    fn sanitize_preserves_newlines_and_line_count() {
        // `\n` survives untouched; line count is exactly the newline count + 1.
        let out = sanitize_for_display("one\ntwo\nthree");
        assert_eq!(out, "one\ntwo\nthree");
        assert_eq!(index_lines(&out).len(), 3);
    }

    #[test]
    fn sanitize_collapses_crlf_without_phantom_blank_lines() {
        // `\r\n` → `\n`: three CRLF lines become three lines, not six.
        let out = sanitize_for_display("a\r\nb\r\nc");
        assert_eq!(out, "a\nb\nc");
        assert_eq!(index_lines(&out).len(), 3);
    }

    #[test]
    fn sanitize_returns_clean_text_byte_identical() {
        // A body with no control chars/ANSI/tabs is returned unchanged.
        let clean = "the quick brown fox\njumps over 123 {\"a\": 1}";
        assert_eq!(sanitize_for_display(clean), clean);
    }

    #[test]
    fn build_sanitizes_displayed_text_but_copy_stays_raw() {
        // A text/plain body carrying ANSI + a tab + a NUL: the DISPLAYED text is
        // sanitized (ANSI gone, tab expanded, NUL → ·) but copy returns the exact
        // on-the-wire bytes including the raw ANSI/tab/control.
        let raw = "a\u{1B}[31m\tb\u{00}c";
        let v = view(raw);
        assert!(!v.pretty(), "text/plain is not pretty");
        // Displayed: ESC[31m stripped, tab at col 1 → 3 spaces (to col 4), NUL → ·.
        assert_eq!(v.text, "a   b·c");
        // Copy is byte-exact, controls and all — BOTH copy paths.
        assert_eq!(v.copy_all(), raw, "copy-all (y) is the raw bytes");
        assert_eq!(
            v.copy_line(0),
            raw,
            "copy-line (Y) must ALSO be the raw bytes, not the sanitized display"
        );
    }

    #[test]
    fn copy_line_is_raw_on_multiline_text_plain_body() {
        // Multi-line text/plain: line 1 carries ANSI + tab + NUL. copy-line must
        // return the EXACT raw line bytes (the D1 invariant), never the sanitized
        // display. `copy_all` returns the whole raw body.
        let raw = "plain first\nsecond\u{1B}[31m\tx\u{00}y\nthird";
        let v = view(raw);
        assert!(!v.pretty());
        // Displayed line 1 is sanitized...
        assert_eq!(v.copy_line(0), "plain first");
        // ...but the mangled line's copy is byte-exact.
        assert_eq!(v.copy_line(1), "second\u{1B}[31m\tx\u{00}y");
        assert_eq!(v.copy_line(2), "third");
        assert_eq!(v.copy_all(), raw);
    }

    #[test]
    fn copy_line_over_pretty_json_returns_displayed_line() {
        // On the reformatted (pretty-JSON) path the raw line index no longer maps
        // to the displayed line, so copy-line returns the DISPLAYED pretty line
        // (serde-escaped JSON has no raw controls to leak). This is the
        // pre-existing behaviour the fix must not regress.
        let v = json_view(r#"{"a":1,"b":2}"#);
        assert!(v.pretty());
        assert!(v.line_count() > 1, "minified input rendered pretty");
        // Line 0 of the pretty text is `{`.
        assert_eq!(v.copy_line(0), "{");
        // A middle line is the displayed pretty content (indented).
        let line1 = v.copy_line(1);
        assert!(
            line1.contains("\"a\": 1"),
            "copy-line returns the displayed pretty line, got {line1:?}"
        );
    }

    #[test]
    fn copy_line_preserves_raw_crlf_bytes() {
        // A CRLF body: the display collapses `\r\n`→`\n`, but copy-line returns the
        // raw line INCLUDING its `\r` (exactly what the wire carried).
        let raw = "a\r\nb\r\nc";
        let v = view(raw);
        assert_eq!(v.line_count(), 3);
        assert_eq!(v.copy_line(0), "a\r");
        assert_eq!(v.copy_line(1), "b\r");
        assert_eq!(v.copy_line(2), "c");
    }

    // ---- M7.7: horizontal-window slice for unwrapped long lines ----

    /// A response whose single body line is `n` chars wide (unwrapped test fixture).
    fn wide_line_view(width_chars: usize) -> ResponseView {
        view(&"x".repeat(width_chars))
    }

    #[test]
    fn unwrapped_long_line_renders_bounded_slice() {
        // A 100-char line in a 10-wide viewport: the built row spans at most the
        // viewport width, NOT the whole line.
        let v = wide_line_view(100);
        let visible = v.visible_map();
        let rows = expand_wrap(&v, &visible, false, 10, 0);
        assert_eq!(rows.len(), 1, "still one row per unwrapped line");
        let (s, e) = (rows[0].char_start, rows[0].char_end);
        assert_eq!((s, e), (0, 10));
        assert!(e - s <= 10, "slice width must be bounded by the viewport");
        // Panned right by 30: the window is [30, 40).
        let rows = expand_wrap(&v, &visible, false, 10, 30);
        assert_eq!((rows[0].char_start, rows[0].char_end), (30, 40));
    }

    #[test]
    fn h_scroll_pans_and_clamps_both_ends() {
        let mut v = wide_line_view(50);
        assert_eq!(v.h_scroll(), 0);
        // Pan left at the left edge stays clamped at 0.
        v.scroll_h(false, 8);
        assert_eq!(v.h_scroll(), 0);
        // Pan right accumulates.
        v.scroll_h(true, 8);
        assert_eq!(v.h_scroll(), 8);
        v.scroll_h(true, 8);
        assert_eq!(v.h_scroll(), 16);
        // The right-edge clamp is max_h_scroll(width) = 50 - width.
        assert_eq!(v.max_h_scroll(10), 40);
        assert_eq!(
            v.max_h_scroll(60),
            0,
            "line narrower than viewport → no scroll"
        );
        // Over-pan is clamped by render via min(max_h_scroll); simulate it.
        v.set_h_scroll(999);
        let clamped = v.h_scroll().min(v.max_h_scroll(10));
        assert_eq!(clamped, 40);
    }

    #[test]
    fn h_scroll_is_inert_when_wrap_on() {
        let mut v = wide_line_view(50);
        v.toggle_wrap(); // wrap on, resets h_scroll to 0
        assert!(v.wrap());
        v.scroll_h(true, 8);
        assert_eq!(v.h_scroll(), 0, "scroll_h is a no-op while wrap is on");
        assert_eq!(v.max_h_scroll(10), 0, "no horizontal scroll under wrap");
    }

    #[test]
    fn toggle_wrap_resets_h_scroll() {
        let mut v = wide_line_view(50);
        v.scroll_h(true, 24);
        assert_eq!(v.h_scroll(), 24);
        v.toggle_wrap();
        assert_eq!(
            v.h_scroll(),
            0,
            "toggling wrap must reset the horizontal scroll"
        );
    }

    #[test]
    fn search_jump_brings_far_right_match_into_window() {
        // A single line with a needle far to the right; a 10-wide window at
        // h_scroll 0 would not show it. ensure_column_visible must pan so the
        // match columns fall inside [h_scroll, h_scroll+width).
        let body = format!("{}NEEDLE tail", "x".repeat(40));
        let mut v = view(&body);
        v.set_search("NEEDLE".to_owned());
        let (start, end) = v.current_match_columns().expect("a match");
        assert_eq!(start, 40, "match starts at column 40");
        assert_eq!(end, 46);
        // Off the right edge of a 10-wide window at h_scroll 0.
        assert!(end > 10);
        v.ensure_column_visible(start, end, 10);
        // The match end is now the last visible column: window = [36, 46).
        assert_eq!(v.h_scroll(), 36);
        assert!(v.h_scroll() <= start && end <= v.h_scroll() + 10);
        // A subsequent left-edge match pans back left.
        v.set_h_scroll(30);
        v.ensure_column_visible(2, 8, 10); // match at cols 2..8, left of window
        assert_eq!(v.h_scroll(), 2);
    }

    #[test]
    fn copy_line_returns_full_raw_line_despite_h_scroll() {
        // Panning the window must never truncate NOR sanitize what copy-line
        // returns. The body carries ANSI + a tab + a NUL amid a long run so both
        // the h_scroll window AND the sanitize pass would corrupt a naive copy —
        // copy-line must return the exact raw line bytes regardless.
        let body = format!("{}\u{1B}[31m\tmid\u{00}{}", "x".repeat(60), "y".repeat(60));
        let mut v = view(&body);
        // Sanitize actually changed the display (so this can catch the leak).
        assert_ne!(
            v.text, body,
            "displayed text is sanitized, not the raw body"
        );
        v.scroll_h(true, 50);
        assert_eq!(
            v.copy_line(0),
            body,
            "copy-line returns the full RAW line, unaffected by h_scroll or sanitize"
        );
        assert_eq!(v.copy_all(), body, "copy-all returns the full raw body");
    }

    // ---- drive-test note #8: line-number gutter ----

    /// Renders a `Done` view into a `TestBackend` of the given size and returns
    /// the body rows as strings (the whole inner area, borders included). Used to
    /// assert the gutter is actually painted (not just the geometry math).
    fn render_rows(view: ResponseView, w: u16, h: u16) -> Vec<String> {
        render_rows_scrolled(view, w, h, 0)
    }

    /// Like [`render_rows`] but with a starting display-row `scroll` — needed to
    /// exercise a wrap-continuation row *at the viewport top* (the gutter
    /// first-row seed is only wrong when `scroll > 0`).
    fn render_rows_scrolled(view: ResponseView, w: u16, h: u16, scroll: usize) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let state = ResponseState::Done { view };
        let cache = HashMap::new();
        let theme = Theme::default();
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    RenderCtx {
                        state: &state,
                        request: None,
                        focused: false,
                        scroll,
                        cursor: scroll,
                        cache: &cache,
                        theme: &theme,
                        jump_label: None,
                        tick_count: 0,
                    },
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol().to_owned()).collect())
            .collect()
    }

    #[test]
    fn gutter_is_on_by_default() {
        // The observable contract: a freshly built view shows the gutter.
        let v = view("a\nb\nc");
        assert!(v.line_numbers(), "the gutter defaults ON");
    }

    #[test]
    fn toggle_flips_gutter_visibility() {
        let mut v = view("a\nb");
        assert!(v.line_numbers());
        v.toggle_line_numbers();
        assert!(!v.line_numbers(), "toggle hides the gutter");
        v.toggle_line_numbers();
        assert!(v.line_numbers(), "toggle shows it again");
    }

    #[test]
    fn gutter_width_scales_with_line_count_and_is_zero_when_off() {
        // 1–9 lines → 1 digit + 1 pad = 2; 10+ lines → 2 digits + 1 pad = 3.
        let mut v = view("a\nb\nc"); // 3 lines
        assert_eq!(v.gutter_width(), 2);
        let big: String = (0..12)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut v2 = view(&big); // exactly 12 lines
        assert_eq!(v2.source_line_count(), 12);
        assert_eq!(v2.gutter_width(), 3, "two-digit count → width 3");
        // Off → the gutter consumes nothing.
        v.toggle_line_numbers();
        assert_eq!(v.gutter_width(), 0);
        v2.toggle_line_numbers();
        assert_eq!(v2.gutter_width(), 0);
    }

    #[test]
    fn gutter_renders_right_aligned_1_based_numbers() {
        // Ten lines so the width is 3 (two digits + pad): line 9 is ` 9 `,
        // line 10 is `10 ` — right-aligned in the same column.
        let body: String = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rows = render_rows(view(&body), 40, 14);
        // Body starts at inner row 1 (row 0 is the top border). Content rows begin
        // after the border; find the row carrying "line0".
        let first = rows
            .iter()
            .find(|r| r.contains("line0"))
            .expect("line0 row");
        assert!(
            first.contains(" 1 line0"),
            "row shows a right-aligned 1-based number, got {first:?}"
        );
        let tenth = rows
            .iter()
            .find(|r| r.contains("line9"))
            .expect("line9 row");
        assert!(
            tenth.contains("10 line9"),
            "the 10th logical line is numbered 10, got {tenth:?}"
        );
    }

    #[test]
    fn wrapped_continuation_rows_have_a_blank_gutter() {
        // One long logical line, wrap on: the first display row shows the number;
        // the continuation rows show a blank (aligned) gutter. Asserted at the
        // render level — the painted body rows must carry the gutter only once.
        let mut v = view(&"w".repeat(60));
        v.toggle_wrap();
        assert!(v.wrap());
        // 40-wide pane: inner 38, gutter 2 → 36-col body, so 60 chars → 2 rows.
        let rows = render_rows(v, 40, 12);
        // Body rows carry the `www…` run; the title (`… wrap`) has a lone `w`, so
        // filter on the run to avoid matching the title row.
        let content: Vec<&String> = rows.iter().filter(|r| r.contains("ww")).collect();
        assert!(content.len() >= 2, "the long line wrapped to ≥2 rows");
        // The first content row carries the number `1 ` before the text.
        assert!(
            content[0].contains("1 w"),
            "first row numbered, got {:?}",
            content[0]
        );
        // Continuation rows: the text after the left border (thin `│` unfocused)
        // is preceded by the blank 2-col gutter (`  w…`), never a digit.
        for cont in &content[1..] {
            let after_border = cont.trim_start_matches('│');
            assert!(
                after_border.starts_with("  w"),
                "continuation row has a blank gutter then text, got {cont:?}"
            );
        }
    }

    #[test]
    fn continuation_row_at_viewport_top_has_a_blank_gutter() {
        // P1 regression guard: line 1 is long (wraps to 9 rows at a 36-col body),
        // line 2 is short. Scroll so the TOP visible row is a wrap-continuation of
        // line 1 (its numbered first row scrolled off). With a `None` seed the top
        // row would wrongly show `1`; the seed-from-`rows[scroll-1]` fix must make
        // it a blank gutter. Row below (still a continuation) is also blank; the
        // first row of line 2 further down is correctly numbered `2`.
        let body = format!("{}\nSECONDLINE", "a".repeat(300));
        let mut v = view(&body);
        v.toggle_wrap();
        assert!(v.wrap());
        // 40-wide pane → inner 38, gutter 2 → 36-col body. 300 chars → 9 rows for
        // line 1 (rows 0..9), then line 2 at row 9 (10 rows total). Pane height 10 →
        // 8 body rows, so `clamp_scroll(2, 10, 8) == 2` holds (a too-tall pane would
        // clamp scroll back to 0 and hide the bug) while the window [2, 10) still
        // reaches line 2's row. Top two visible rows are line-1 continuations; line
        // 2's numbered first row is at the bottom.
        let rows = render_rows_scrolled(v, 40, 10, 2);
        // The `a`-run rows are line-1 continuations (blank gutter); the SECONDLINE
        // row is line 2's first row (numbered `2`).
        let a_rows: Vec<&String> = rows.iter().filter(|r| r.contains("aa")).collect();
        assert!(
            a_rows.len() >= 2,
            "two line-1 continuation rows visible at the top, got {a_rows:?}"
        );
        for cont in &a_rows {
            let body_cols = cont.trim_start_matches('│');
            assert!(
                body_cols.starts_with("  a"),
                "continuation row at/after the viewport top has a BLANK gutter (not \
                 a line number), got {cont:?}"
            );
            assert!(
                !body_cols.starts_with("1 "),
                "a continuation row must never show line 1's number, got {cont:?}"
            );
        }
        // Line 2's first row is numbered `2` (the seed didn't break real first rows).
        let second = rows
            .iter()
            .find(|r| r.contains("SECONDLINE"))
            .expect("line 2 visible");
        assert!(
            second.contains("2 SECONDLINE"),
            "line 2's first row is numbered 2, got {second:?}"
        );
    }

    #[test]
    fn unwrapped_panned_line_at_scroll_still_shows_its_number() {
        // Seed-fix guard for the unwrapped path: every unwrapped line is exactly
        // one row per visible_idx, so even with `scroll > 0` AND `h_scroll > 0`
        // (the row's char_start == h_scroll > 0) the top row is still a first row
        // and must carry its number. Ten distinct wide lines; pan right; scroll to
        // line 3 (index 2 → number 3).
        let body: String = (0..10)
            .map(|i| format!("L{i}-{}", "x".repeat(60)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut v = view(&body);
        v.scroll_h(true, 5); // h_scroll > 0
        assert_eq!(v.h_scroll(), 5);
        assert!(!v.wrap());
        // Scroll to display row 2 (0-based) → logical line index 2 → number 3.
        // 10 lines → gutter width 3 (2 digits + pad), so line 3 renders as ` 3 `.
        let rows = render_rows_scrolled(v, 40, 8, 2);
        // The top content row shows number `3` right-aligned in the 3-col gutter.
        let numbered = rows
            .iter()
            .find(|r| r.trim_start_matches('│').starts_with(" 3 "))
            .expect("top unwrapped row numbered 3 despite scroll>0 and h_scroll>0");
        // And it shows the panned window (char_start == 5), so `L3-` is off-screen
        // left; the visible text is the `x` run.
        assert!(
            numbered.contains('x'),
            "the numbered row shows the panned window, got {numbered:?}"
        );
    }

    #[test]
    fn fold_header_shows_the_opener_line_number() {
        // A JSON body folded at the top: the fold-header row carries the opener
        // line's number (line 1), then the `⋯ N lines` suffix.
        let mut v = json_view("{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}");
        assert!(v.line_count() > 1);
        v.toggle_all_folds(); // collapse the top-level object
        let rows = render_rows(v, 60, 10);
        let header = rows
            .iter()
            .find(|r| r.contains('⋯'))
            .expect("a fold-header row");
        assert!(
            header.contains("1 {"),
            "fold-header carries the opener's number (1), got {header:?}"
        );
    }

    #[test]
    fn gutter_shrinks_effective_body_width_for_wrap_and_h_scroll() {
        // The width interaction is the risk area: the gutter must be subtracted
        // from the body width BEFORE wrap/h_scroll geometry. We assert the wrap
        // boundary shifts by exactly the gutter width when it is on vs off.
        let long = "z".repeat(100);
        // Gutter OFF: with an 18-col body, the first wrap row is 18 chars.
        let mut off = view(&long);
        off.toggle_line_numbers();
        off.toggle_wrap();
        let visible = off.visible_map();
        let rows_off = expand_wrap(&off, &visible, true, 18, 0);
        assert_eq!(
            rows_off[0].char_end - rows_off[0].char_start,
            18,
            "gutter off: full 18-col body wrap"
        );
        // Gutter ON: same 20-col area, but render subtracts gutter (2) → 18-col
        // body. We emulate render's seam: body width = area - gutter_width.
        let mut on = view(&long);
        on.toggle_wrap();
        assert!(on.line_numbers());
        let area_width = 20usize;
        let body_width = area_width - on.gutter_width();
        assert_eq!(body_width, 18, "gutter on: body = area(20) - gutter(2)");
        let visible = on.visible_map();
        let rows_on = expand_wrap(&on, &visible, true, body_width, 0);
        assert_eq!(
            rows_on[0].char_end - rows_on[0].char_start,
            18,
            "wrap splits at the REDUCED body width, not the full area"
        );

        // And h_scroll: with the gutter on, max_h_scroll must be computed against
        // the reduced body width. widest visible line = 100.
        let pan = view(&long); // wrap off, gutter on
        assert!(!pan.wrap());
        // If a caller passes the reduced width (as render does), the clamp uses it.
        assert_eq!(pan.gutter_width(), 2);
        let reduced = 30 - pan.gutter_width();
        assert_eq!(pan.max_h_scroll(reduced), 100 - reduced);
    }

    #[test]
    fn copy_stays_byte_exact_with_the_gutter_on() {
        // Regression guard: the gutter is render-only. Copy must be byte-exact even
        // with the gutter on AND a mangled line (ANSI/tab/NUL) that sanitize would
        // corrupt if copy ever read the displayed text.
        let raw = "clean\nmid\u{1B}[31m\tx\u{00}y\ntail";
        let v = view(raw);
        assert!(v.line_numbers(), "gutter on");
        assert_eq!(v.copy_all(), raw, "copy-all is the raw body, gutter or not");
        assert_eq!(
            v.copy_line(1),
            "mid\u{1B}[31m\tx\u{00}y",
            "copy-line is the raw line bytes, unaffected by the gutter"
        );
    }

    #[test]
    fn viewport_width_reported_by_render_excludes_the_gutter() {
        // The reported viewport_width (fed back into cursor→logical mapping) must be
        // the reduced body width. Render a 40-wide pane: inner width = 38, minus the
        // 2-col gutter = 36.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let state = ResponseState::Done { view: view("a\nb") };
        let cache = HashMap::new();
        let theme = Theme::default();
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut captured = 0usize;
        terminal
            .draw(|frame| {
                let outcome = render(
                    frame,
                    frame.area(),
                    RenderCtx {
                        state: &state,
                        request: None,
                        focused: false,
                        scroll: 0,
                        cursor: 0,
                        cache: &cache,
                        theme: &theme,
                        jump_label: None,
                        tick_count: 0,
                    },
                );
                captured = outcome.viewport_width;
            })
            .expect("draw");
        // inner width = 40 - 2 borders = 38; gutter = 2 → body = 36.
        assert_eq!(captured, 36, "viewport_width excludes the gutter");
    }
}
