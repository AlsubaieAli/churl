//! Render-geometry signatures + the single-slot geometry memo ([`GeomCache`])
//! that keys the fold/wrap/widest-line products on their inputs (A3), plus the
//! per-response cursor/scroll [`ResponseGeometry`] that lives outside the view.
//! Extracted from the parent module unchanged; the signature types + cache stay
//! `pub(super)` (reached only by the [`ResponseView`] accessors in `pipeline`),
//! and [`ResponseGeometry`] keeps its original `pub` surface.

use super::text;
use super::{RenderOutcome, Visible};

/// Identifies the inputs the fold-filtered visible map depends on: the response
/// `generation` (bumped on every text rebuild — pretty/sort toggles), the view
/// mode (body vs headers — switched WITHOUT a generation bump, different source
/// text), and a hash of the folded-opener set. Two views with an equal
/// [`VisibleSig`] produce a byte-identical [`Visible`] map, so it keys the
/// visible-map memo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VisibleSig {
    pub(super) generation: u64,
    pub(super) headers: bool,
    /// Hash of the sorted folded-opener set (same encoding as `viewport_hash`'s
    /// fold signature). Distinguishes different fold configurations.
    pub(super) fold_sig: u64,
}

/// The full geometry signature for a computed `Vec<DisplayRow>`: everything the
/// visible map depends on (via [`VisibleSig`]) PLUS the wrap/width/h_scroll the
/// expansion windows against. Equal signatures ⇒ byte-identical display rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowsSig {
    pub(super) visible: VisibleSig,
    pub(super) wrap: bool,
    pub(super) width: usize,
    pub(super) h_scroll: usize,
}

/// The signature for the `max_h_scroll` widest-visible-line scan: the visible
/// map inputs PLUS wrap+width, but NOT h_scroll (the widest line is independent
/// of the current horizontal pan — that is the whole point of computing the
/// bound). Kept deliberately distinct from [`RowsSig`] so a pure horizontal pan
/// still hits this cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MaxHScrollSig {
    pub(super) visible: VisibleSig,
    pub(super) wrap: bool,
    pub(super) width: usize,
}

/// Single-slot memo of the render geometry, hung off the [`ResponseView`] so it
/// lives and dies with the response (and thus its owning endpoint buffer / runner
/// state) — bounded to exactly one cached value per product, never grows, and is
/// dropped when a new response replaces the view. Each slot is keyed on the
/// signature that captures every input the product depends on (A3): on a frame
/// whose signature is unchanged the memo is returned verbatim, so recomputing
/// would produce a byte-identical result and serving the cache changes nothing
/// observable. A signature mismatch recomputes and replaces the slot.
#[derive(Debug, Default)]
pub(super) struct GeomCache {
    /// The fold-filtered visible map, keyed on [`VisibleSig`].
    pub(super) visible: Option<(VisibleSig, Vec<Visible>)>,
    /// The wrap-expanded display rows, keyed on [`RowsSig`].
    pub(super) rows: Option<(RowsSig, Vec<text::DisplayRow>)>,
    /// The widest-visible-line char count for `max_h_scroll`, keyed on
    /// [`MaxHScrollSig`].
    pub(super) widest: Option<(MaxHScrollSig, usize)>,
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
