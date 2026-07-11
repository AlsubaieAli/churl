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

    /// The copyable text for a [`ResponseState::Failed`] row (drive-test #4a):
    /// the error message plus the request method+URL when known, so a transport
    /// failure is yankable with `y` for debugging. Returns `None` for every
    /// other state — the copy handler falls back to this only when there is no
    /// [`ResponseView`] to copy (i.e. the row is not `Done`), keeping the three
    /// unified viewers' `Done` copy path untouched. Never fabricates a
    /// status/body/timing a transport failure genuinely lacks; it copies only
    /// what is honestly known. `meta.method`/`url` may be empty (the runner
    /// metas set only the URL) — an empty part is omitted rather than padded.
    pub fn failure_copy_text(&self) -> Option<String> {
        let ResponseState::Failed { error, meta } = self else {
            return None;
        };
        let mut out = String::new();
        let request_line = failed_request_line(meta);
        if let Some(line) = request_line {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str("error: ");
        out.push_str(error);
        Some(out)
    }
}

/// The `METHOD URL` line for a failed request, from its [`ResponseMeta`], or
/// just the URL when the method is empty (runner metas), or `None` when neither
/// is known. Shared by the failure render panel and the failure copy text so
/// the two can never drift.
fn failed_request_line(meta: &ResponseMeta) -> Option<String> {
    match (meta.method.trim(), meta.url.trim()) {
        ("", "") => None,
        ("", url) => Some(url.to_owned()),
        (method, "") => Some(method.to_owned()),
        (method, url) => Some(format!("{method} {url}")),
    }
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

#[cfg(test)]
mod tests;
