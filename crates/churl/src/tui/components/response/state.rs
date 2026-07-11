//! Response-pane state types: the pane state machine ([`ResponseState`]),
//! per-request metadata ([`ResponseMeta`]), view mode ([`ViewMode`]), search
//! state ([`SearchState`]), and the fold-filtered [`Visible`] map entry. Pure
//! data + trivial accessors, extracted from the parent module so the pipeline
//! transforms live apart from the state they operate over. Item visibilities
//! match the originals exactly (`pub` for the public types, `pub(super)` for the
//! parent/sibling-visible `Visible` map + `failed_request_line`).

use std::time::{Duration, Instant};

use super::ResponseView;

/// Immutable metadata about a request, captured at send time so history and the
/// error view need nothing from live app state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseMeta {
    /// HTTP method string, e.g. `"GET"`.
    pub method: String,
    /// Requested URL (verbatim; no templating).
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
    /// load runner's memory bound. The status/timing/size are kept for an
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

    /// The copyable text for a [`ResponseState::Failed`] row:
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
pub(super) fn failed_request_line(meta: &ResponseMeta) -> Option<String> {
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
    pub(super) matches: Vec<(usize, usize, usize)>,
    /// Index of the current match within `matches`, when there is one.
    pub(super) current: Option<usize>,
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
pub(super) enum Visible {
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
    pub(super) fn logical(self) -> usize {
        match self {
            Visible::Line(l) => l,
            Visible::FoldHeader { line, .. } => line,
        }
    }
}
