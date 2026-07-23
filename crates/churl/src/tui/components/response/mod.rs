//! Response pane: a virtualised viewer over an executed response body.
//!
//! The body is stored once as a lossy-UTF-8 [`String`] plus a `Vec<usize>` of
//! line-start byte offsets (single pass); rendering slices only the visible
//! lines, so a 1 MB response is never materialised into a `Vec<Line>`. Syntax
//! highlighting is layered on by the off-thread worker
//! ([`crate::tui::highlight`]): a cache hit draws coloured lines, a miss draws
//! plain text and enqueues a highlight job.
//!
//! ## Display pipeline
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

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;

use churl_core::model::{Response, Timing};

use super::fold::{self, FoldRegion};
use crate::tui::highlight::SyntaxToken;

mod geometry;
mod render;
mod state;
mod text;

// Re-export the render + text items that external callers reach by path
// (`response::render`, `response::fmt_bytes`, `help.rs`'s
// `response::{build_search_spans, smart_case_matches}`, snapshot tests'
// `response::{RenderCtx, RenderOutcome}`), so every existing `use` path resolves
// unchanged after the split. Visibilities match the originals exactly.
pub(crate) use render::build_search_spans;
pub use render::{RenderCtx, RenderOutcome, collapsed_summary, fmt_bytes, render};
// `render_done` (M8.6.1): reused directly by `components::request`'s Body-tab
// browse renderer — see `render_done`'s doc for why it bypasses `render`.
pub(in crate::tui::components) use render::render_done;
pub use text::clamp_scroll;
pub(crate) use text::smart_case_matches;

// Re-export the state + geometry items so every existing path
// (`response::ResponseState`, `response::ViewMode`, `response::ResponseMeta`,
// `response::SearchState`, `response::ResponseGeometry`) resolves unchanged after
// the split. Visibilities match the originals exactly: the public types stay
// `pub`; the parent/sibling-visible fold-map entry, failure helper, geometry
// signatures and cache stay `pub(in …response)` (the child-module equivalent of
// the pre-split module-private scope — no `pub`/`pub(crate)` widening).
pub use geometry::ResponseGeometry;
pub(in crate::tui::components::response) use geometry::{
    GeomCache, MaxHScrollSig, RowsSig, VisibleSig,
};
pub use state::{ResponseMeta, ResponseState, SearchState, ViewMode};
pub(in crate::tui::components::response) use state::{Visible, failed_request_line};

// Pull the module-private text helpers into this module's namespace so the
// `ResponseView` impl below (and the `use super::*` test module) reach them by
// bare name, exactly as before the split. No visibility widening: these stay
// `pub(in …response)`.
use text::{
    DisplayRow, digit_count, expand_wrap, index_lines, reformat_body_if_needed,
    sanitize_for_display,
};

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
    /// byte-exact regardless of the pretty toggle.
    raw_text: String,
    /// The exact on-the-wire response body bytes (M8.7) — never lossy, unlike
    /// `raw_text` (a `String::from_utf8_lossy` decode, which replaces invalid
    /// UTF-8 with U+FFFD and so is NOT byte-exact for a binary body). Save
    /// (`S`) reads this so a binary response round-trips byte-for-byte; empty
    /// for [`Self::build_over_text`] (the M8.6.1 request-body-browse view has
    /// no response to save — callers guard on emptiness).
    raw_bytes: Vec<u8>,
    /// The file extension the `S` save-response-body prompt seeds its default
    /// filename with, sniffed from the response `Content-Type` at build time
    /// (see [`crate::tui::highlight::SyntaxToken::extension_for_content_type`]).
    /// Meaningless (defaulted from `syntax`) for [`Self::build_over_text`],
    /// which carries no response/Content-Type at all — harmless, since
    /// `raw_bytes` is empty there and save is guarded on that first.
    save_extension: &'static str,
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
    /// Whether the left-hand line-number gutter is shown.
    /// Defaults to `true` (gutter on) and persists across pretty/sort/wrap/fold
    /// toggles for the life of the view — a new response builds a fresh view, so
    /// the default-on contract is re-established per response. Render-only: it
    /// shrinks the effective body width (see [`render_done`]) but never touches
    /// `raw_text`, copy, or the line index.
    line_numbers: bool,
    /// Horizontal scroll offset in *char columns* for unwrapped long lines.
    /// When wrap is off, each logical line's display row shows the char
    /// window `[h_scroll, h_scroll + viewport_width)` instead of the whole line,
    /// bounding render cost and letting the user pan a minified/non-JSON line past
    /// the first screenful. Reset to 0 on build, on any generation bump, and on
    /// `ToggleWrap`. Inert while wrap is on (wrapped rows already fit the width).
    h_scroll: usize,
    /// Whether the body is rendered pretty (reformatted) rather than raw.
    /// Defaults to `true` on arrival for json-ish content-types,
    /// `false` otherwise. Toggled by `p` in the Response overlay. When on but the
    /// body is not parseable JSON, the reformat silently falls back to raw and
    /// `text == raw_text`.
    pretty: bool,
    /// Whether pretty JSON object keys are sorted A→Z (recursively). Defaults to
    /// `false` (server wire order) and resets per response. Only affects display
    /// when `pretty` is on and the body is JSON; a no-op otherwise. Toggled by
    /// `s` in the Response overlay.
    sort_keys: bool,
    /// The active body search, when a search is live.
    search: Option<SearchState>,
    /// Single-slot memo of the render geometry (visible map / display rows /
    /// widest line), keyed on the inputs each product depends on (A3). Interior
    /// mutability so the pure `&self` render/geometry accessors can populate it;
    /// bounded to one value per product and dropped with the view. Excluded from
    /// any structural comparison — it is a derived cache, not state.
    geom_cache: RefCell<GeomCache>,
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
        let save_extension = SyntaxToken::extension_for_content_type(content_type);
        // Pretty-by-default for json-ish content-types; raw for
        // everything else. The reformat is a transform *before* the fold/wrap/
        // viewport stages — `text`/`line_offsets` describe what is displayed.
        let pretty = syntax == SyntaxToken::Json;
        // Wire order by default (decision): sorting is an explicit opt-in via `s`.
        let sort_keys = false;
        // Sanitize the reformatted (displayed) text: strip ANSI, expand tabs, and
        // replace remaining control chars. `raw_text` stays the untouched decode so
        // copy remains byte-exact.
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
            raw_bytes: response.body.clone(),
            save_extension,
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
            geom_cache: RefCell::new(GeomCache::default()),
        }
    }

    /// Builds a view directly over arbitrary text — no HTTP response involved.
    /// Used by the Body-tab browse mode (M8.6.1) to reuse this exact
    /// fold/pretty/wrap/search pipeline over the *request* body instead of a
    /// response. `status`/`timing`/`header_count`/`truncated` have no meaning
    /// here and are zeroed; callers that draw this view must NOT go through the
    /// response `render()` entry point (which reads them for the status-bar
    /// title) — call `render_done` directly instead, as the Body-tab renderer
    /// does. `generation` should be bumped by the caller on every rebuild, so a
    /// stale render-geometry memo elsewhere is never mistaken for this
    /// instance's (though a fresh `Self` always starts with an empty memo
    /// regardless). `raw_bytes` is deliberately left EMPTY (M8.7) — there is no
    /// *response* here to save, only the request body this view happens to
    /// render through the response pipeline; the `S` save-response-body
    /// handler guards on emptiness rather than saving this view's text.
    pub fn build_over_text(text: &str, syntax: SyntaxToken, generation: u64) -> Self {
        let raw_text = text.to_owned();
        let pretty = syntax == SyntaxToken::Json;
        let sort_keys = false;
        let text = sanitize_for_display(&reformat_body_if_needed(
            &raw_text, syntax, pretty, sort_keys,
        ));
        let line_offsets = index_lines(&text);
        let raw_line_offsets = index_lines(&raw_text);
        Self {
            syntax,
            byte_size: raw_text.len(),
            truncated: false,
            status: 0,
            timing: Timing {
                connect: None,
                total: Duration::ZERO,
            },
            header_count: 0,
            generation,
            line_offsets,
            raw_line_offsets,
            text,
            raw_text,
            raw_bytes: Vec::new(),
            save_extension: match syntax {
                SyntaxToken::Json => "json",
                SyntaxToken::Html => "html",
                SyntaxToken::Xml => "xml",
                SyntaxToken::Plain => "txt",
            },
            headers_text: String::new(),
            headers_offsets: Vec::new(),
            view_mode: ViewMode::Body,
            folds: None,
            folded: HashSet::new(),
            wrap: false,
            line_numbers: true,
            h_scroll: 0,
            pretty,
            sort_keys,
            search: None,
            geom_cache: RefCell::new(GeomCache::default()),
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

    /// Whether the line-number gutter is shown.
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

    /// The exact on-the-wire response body bytes, for the `S` save-response-body
    /// gesture (M8.7) — byte-exact regardless of view mode or the pretty
    /// toggle, unlike [`Self::copy_all`] (which is view-mode-dependent text and,
    /// for a non-UTF-8 body, `raw_text`'s lossy decode). Empty for a view built
    /// via [`Self::build_over_text`] (no response to save there).
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw_bytes
    }

    /// The file extension sniffed from the response `Content-Type` at build
    /// time, for the `S` prompt's default filename (M8.7).
    pub fn save_extension(&self) -> &'static str {
        self.save_extension
    }

    /// Whether the response body was truncated at the size cap — unlike
    /// [`Self::truncated`] (which is gated to the body view, since it answers
    /// "is what I'm looking at right now truncated"), this always reflects the
    /// underlying response regardless of view mode. Used by save (`S`), which
    /// always saves the response BODY bytes even while the headers view is
    /// showing.
    pub fn body_truncated(&self) -> bool {
        self.truncated
    }

    /// The logical line shown at display-row `row`, computed through the current
    /// fold/wrap pipeline at pane `width` (0 = unwrapped geometry). Used to map
    /// the cursor row back to a logical line for fold-at-cursor and copy-line.
    pub fn logical_at_display_row(&self, row: usize, width: usize) -> Option<usize> {
        let visible = self.cached_visible_map();
        let rows = self.cached_expand_wrap(self.wrap, width, self.h_scroll);
        rows.get(row).map(|dr| visible[dr.visible_idx].logical())
    }

    /// The display-row index of the first row showing logical line `logical`,
    /// through the current pipeline at `width`. Used to move the cursor onto a
    /// search match. Returns `None` when the line is not visible (should not
    /// happen after auto-unfold).
    pub fn display_row_for_logical(&self, logical: usize, width: usize) -> Option<usize> {
        let visible = self.cached_visible_map();
        let rows = self.cached_expand_wrap(self.wrap, width, self.h_scroll);
        rows.iter()
            .position(|dr| visible[dr.visible_idx].logical() == logical)
    }

    /// The total number of display rows through the current pipeline at `width`.
    pub fn total_display_rows(&self, width: usize) -> usize {
        self.cached_expand_wrap(self.wrap, width, self.h_scroll)
            .len()
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
        // Cache the widest-visible-line char count (A3) on a signature that
        // OMITS h_scroll: the widest line is independent of the current pan, so a
        // pure horizontal scroll (which changes only the viewport, not the
        // geometry) still hits this memo. wrap/width are keyed because they gate
        // the scan and shift the bound. Kept distinct from the display-rows memo,
        // which DOES depend on h_scroll.
        let sig = MaxHScrollSig {
            visible: self.visible_sig(),
            wrap: self.wrap,
            width,
        };
        {
            let cache = self.geom_cache.borrow();
            if let Some((cached_sig, widest)) = &cache.widest
                && *cached_sig == sig
            {
                return widest.saturating_sub(width);
            }
        }
        let visible = self.cached_visible_map();
        let widest = visible
            .iter()
            .map(|v| self.logical_line(v.logical()).chars().count())
            .max()
            .unwrap_or(0);
        self.geom_cache.borrow_mut().widest = Some((sig, widest));
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

    /// The logical line of the next (`forward`) or previous collapsible JSON node
    /// visible from `from` — structural navigation between object/array openers,
    /// skipping leaf lines.
    ///
    /// Fold-aware for free: the candidate stops are the fold-openers that survive
    /// the current fold-filtered visible map, so a folded node's header is itself
    /// a valid stop while its hidden descendants (elided from the map) are
    /// skipped. Navigation never mutates fold state — this is the skip-over-folded
    /// default; a future open-folded-on-arrival setting would branch on the
    /// visible-opener selection here. Returns `None` at the last/first stop or
    /// when the body has no collapsible nodes.
    pub fn structural_target(&mut self, from: usize, forward: bool) -> Option<usize> {
        let openers: HashSet<usize> = self.ensure_folds().iter().map(|r| r.opener).collect();
        if openers.is_empty() {
            return None;
        }
        let visible = self.cached_visible_map();
        let stops = visible
            .iter()
            .map(|v| v.logical())
            .filter(|l| openers.contains(l));
        if forward {
            stops.filter(|&l| l > from).min()
        } else {
            stops.filter(|&l| l < from).max()
        }
    }

    // ---- geometry cache (A3) ----

    /// The signature of the current fold-filtered visible map: response
    /// generation (text rebuilds), view mode (body vs headers — no generation
    /// bump), and a hash of the folded-opener set. Cheap to compute (O(folds),
    /// not O(body)); drives the geometry memo below.
    fn visible_sig(&self) -> VisibleSig {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        // Same fold-signature encoding as `viewport_hash`: the sorted folded
        // openers. Empty set ⇒ a stable "no folds" hash.
        let mut folded: Vec<usize> = self.folded.iter().copied().collect();
        folded.sort_unstable();
        folded.hash(&mut hasher);
        VisibleSig {
            generation: self.generation,
            headers: self.view_mode == ViewMode::Headers,
            fold_sig: hasher.finish(),
        }
    }

    /// The fold-filtered visible map, served from the single-slot memo when the
    /// [`VisibleSig`] is unchanged and recomputed (replacing the slot) otherwise.
    /// A signature match guarantees the recompute would be byte-identical, so
    /// serving the memo is behaviour-preserving. Returns an owned `Vec` (the
    /// callers already took ownership of `visible_map()`'s result).
    fn cached_visible_map(&self) -> Vec<Visible> {
        let sig = self.visible_sig();
        {
            let cache = self.geom_cache.borrow();
            if let Some((cached_sig, map)) = &cache.visible
                && *cached_sig == sig
            {
                return map.clone();
            }
        }
        let map = self.visible_map();
        self.geom_cache.borrow_mut().visible = Some((sig, map.clone()));
        map
    }

    /// The wrap-expanded display rows for `(wrap, width, h_scroll)`, served from
    /// the memo when the full [`RowsSig`] matches and recomputed otherwise.
    /// The signature covers every input `expand_wrap` reads (the visible map's
    /// inputs via [`VisibleSig`], plus wrap/width/h_scroll), so a match is
    /// byte-identical.
    fn cached_expand_wrap(&self, wrap: bool, width: usize, h_scroll: usize) -> Vec<DisplayRow> {
        let sig = RowsSig {
            visible: self.visible_sig(),
            wrap,
            width,
            h_scroll,
        };
        {
            let cache = self.geom_cache.borrow();
            if let Some((cached_sig, rows)) = &cache.rows
                && *cached_sig == sig
            {
                return rows.clone();
            }
        }
        // `cached_visible_map` shares the same `geom_cache` RefCell; call it
        // BEFORE re-borrowing to store the rows so the two borrows never overlap.
        let visible = self.cached_visible_map();
        let rows = expand_wrap(self, &visible, wrap, width, h_scroll);
        self.geom_cache.borrow_mut().rows = Some((sig, rows.clone()));
        rows
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
    /// horizontal window `[h_scroll, h_scroll + width)` (search-into-view). A
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
    /// copy stays byte-exact regardless of the pretty toggle. In the
    /// headers view it is the headers text.
    pub fn copy_all(&self) -> &str {
        match self.view_mode {
            ViewMode::Body => &self.raw_text,
            ViewMode::Headers => &self.headers_text,
        }
    }

    /// The text of the logical line at `logical` for the copy-line action (`Y`).
    ///
    /// Copy must stay byte-exact (the invariant): the displayed `text` is
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

    /// If `logical` is the **opener** line of a fold region, returns that region's
    /// inclusive `(opener, closer)` logical-line extent; otherwise `None`. Drives
    /// the block-aware `Y` copy: on a fold opener the whole folded region is
    /// yanked, elsewhere `Y` falls back to the single-line copy. Only JSON bodies
    /// fold, so this is `None` for headers / non-JSON views. A line is the opener
    /// of at most one region, so the match is unambiguous.
    pub fn fold_region_at_opener(&mut self, logical: usize) -> Option<(usize, usize)> {
        if !self.folding_supported() {
            return None;
        }
        self.ensure_folds()
            .iter()
            .find(|r| r.opener == logical)
            .map(|r| (r.opener, r.closer))
    }

    /// Copies the inclusive logical-line range `[start, end]` as one `\n`-joined
    /// string, for the block-aware `Y` yank of a folded region. Each line is
    /// materialised through [`Self::copy_line`], so the same byte-exact invariant
    /// holds per line (raw wire bytes on the non-reformatted path, displayed
    /// pretty lines otherwise) — the region copy is exactly the concatenation of
    /// the line copies the user would get one `Y` at a time.
    pub fn copy_region(&self, start: usize, end: usize) -> String {
        let mut out = String::new();
        for logical in start..=end {
            if logical > start {
                out.push('\n');
            }
            out.push_str(&self.copy_line(logical));
        }
        out
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

    /// Test-only probe: whether the display-rows geometry memo is currently
    /// populated with a slot whose signature matches `(wrap, width, h_scroll)` at
    /// the view's current visible-map signature — i.e. a subsequent
    /// [`Self::cached_expand_wrap`] call with those args would be a cache HIT
    /// (served from the memo, no recompute). Used by the A3 cache-hit test to
    /// lock the optimization without timing.
    #[cfg(test)]
    fn rows_cache_hits(&self, wrap: bool, width: usize, h_scroll: usize) -> bool {
        let want = RowsSig {
            visible: self.visible_sig(),
            wrap,
            width,
            h_scroll,
        };
        self.geom_cache
            .borrow()
            .rows
            .as_ref()
            .is_some_and(|(sig, _)| *sig == want)
    }
}
#[cfg(test)]
mod tests;
