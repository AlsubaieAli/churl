//! Response pane: a virtualised viewer over an executed response body.
//!
//! The body is stored once as a lossy-UTF-8 [`String`] plus a `Vec<usize>` of
//! line-start byte offsets (a single pass). Rendering slices only the visible
//! lines out of that index — a 1 MB response is never materialised into a
//! `Vec<Line>`. Long lines truncate at the pane width (no wrapping in M3;
//! horizontal scroll is deferred). Syntax highlighting is layered on top by the
//! off-thread worker (see [`crate::tui::highlight`]): a cache hit draws coloured
//! lines, a miss draws plain text immediately and enqueues a highlight job.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use churl_core::model::{Request, Response, Timing};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use crate::tui::highlight::{HighlightJob, SyntaxToken};
use crate::tui::theme::Theme;

/// Immutable metadata about a request that produced (or failed to produce) a
/// response. Captured at send time so history and the error view need nothing
/// from the live app state.
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
}

/// A virtualised view over one arrived response body, built once on arrival.
#[derive(Debug)]
pub struct ResponseView {
    /// Lossy-UTF-8 decode of the full body.
    text: String,
    /// Byte offset of each line start. Empty for an empty body.
    line_offsets: Vec<usize>,
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
}

impl ResponseView {
    /// Builds the view from a response, indexing its line starts in one pass.
    pub fn build(response: &Response, generation: u64) -> Self {
        let text = String::from_utf8_lossy(&response.body).into_owned();
        let mut line_offsets = Vec::new();
        if !text.is_empty() {
            line_offsets.push(0);
            for (index, byte) in text.bytes().enumerate() {
                if byte == b'\n' {
                    line_offsets.push(index + 1);
                }
            }
        }
        let content_type = response
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("content-type"))
            .map(|header| header.value.as_str());
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
        }
    }

    /// The number of lines in the body.
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

    /// Returns the `idx`-th line without its trailing newline. Panics if `idx`
    /// is out of range (callers slice within [`ResponseView::line_count`]).
    fn line(&self, idx: usize) -> &str {
        let start = self.line_offsets[idx];
        let end = self
            .line_offsets
            .get(idx + 1)
            .map(|&next| next - 1)
            .unwrap_or(self.text.len());
        &self.text[start..end]
    }
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
/// miss over a `Done` state), the clamped scroll offset, and the body height.
pub struct RenderOutcome {
    /// A job the caller should enqueue on the highlight worker, if any.
    pub job: Option<HighlightJob>,
    /// The scroll offset after clamping to the viewport.
    pub clamped_scroll: usize,
    /// The height (in rows) of the body viewport, for half-page scrolling.
    pub viewport_height: usize,
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
    let title = match ctx.jump_label {
        Some(label) => format!(" Response [{label}] "),
        None => " Response ".to_owned(),
    };
    let block = Block::bordered()
        .border_type(border_type)
        .border_style(border_style)
        .title(title)
        .title_style(ctx.theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [status_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    let viewport_height = body_area.height as usize;

    let mut outcome = RenderOutcome {
        job: None,
        clamped_scroll: ctx.scroll,
        viewport_height,
    };

    match ctx.state {
        ResponseState::Idle => {
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
            frame.render_widget(Paragraph::new(Line::from("cancelled")), status_area);
            frame.render_widget(Paragraph::new(Line::from("request cancelled")), body_area);
        }
        ResponseState::Failed { error, .. } => {
            frame.render_widget(Paragraph::new(Line::from("request failed")), status_area);
            frame.render_widget(
                Paragraph::new(vec![Line::from("error:"), Line::from(error.clone())])
                    .wrap(Wrap { trim: false }),
                body_area,
            );
        }
        ResponseState::Done { view } => {
            frame.render_widget(
                Paragraph::new(Line::from(status_summary(view)).style(ctx.theme.response_status)),
                status_area,
            );
            let total = view.line_count();
            let scroll = clamp_scroll(ctx.scroll, total, viewport_height);
            let end = (scroll + viewport_height).min(total);
            let hash = viewport_hash(view.generation, scroll, viewport_height, view.syntax);
            outcome.clamped_scroll = scroll;

            if let Some(lines) = ctx.cache.get(&hash) {
                frame.render_widget(Paragraph::new(lines.clone()), body_area);
            } else {
                let plain: Vec<Line> = (scroll..end)
                    .map(|i| Line::from(view.line(i).to_owned()))
                    .collect();
                frame.render_widget(Paragraph::new(plain), body_area);
                outcome.job = Some(HighlightJob {
                    hash,
                    syntax: view.syntax,
                    lines: (scroll..end).map(|i| view.line(i).to_owned()).collect(),
                });
            }
        }
    }

    outcome
}

/// The one-line status summary for a completed response (e.g.
/// `200 OK · 142 ms · 4.1 KB · 3 hdrs`), with a ` · truncated at <size>`
/// suffix when the body hit the configured cap.
fn status_summary(view: &ResponseView) -> String {
    let phrase = reason_phrase(view.status);
    let status = if phrase.is_empty() {
        view.status.to_string()
    } else {
        format!("{} {}", view.status, phrase)
    };
    let mut summary = format!(
        "{status} · {} · {} · {} hdrs",
        fmt_ms(view.timing.total),
        fmt_bytes(view.byte_size),
        view.header_count
    );
    if view.truncated {
        summary.push_str(&format!(" · truncated at {}", fmt_bytes(view.byte_size)));
    }
    summary
}

/// The viewport cache key: response generation, scroll, height, and syntax.
/// Width is deliberately excluded (no wrapping; truncation happens at draw time).
fn viewport_hash(generation: u64, scroll: usize, height: usize, syntax: SyntaxToken) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    generation.hash(&mut hasher);
    scroll.hash(&mut hasher);
    height.hash(&mut hasher);
    syntax.hash(&mut hasher);
    hasher.finish()
}

/// A compact byte-size string (`B`/`KB`/`MB`).
fn fmt_bytes(bytes: usize) -> String {
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
    use churl_core::model::Response;

    fn view(body: &str) -> ResponseView {
        let response = Response {
            status: 200,
            headers: Vec::new(),
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
        assert_eq!(view.line(0), "a");
        assert_eq!(view.line(1), "bb");
        assert_eq!(view.line(2), "ccc");
    }

    #[test]
    fn empty_body_has_no_lines() {
        let view = view("");
        assert_eq!(view.line_count(), 0);
    }

    #[test]
    fn no_trailing_newline_vs_trailing_newline() {
        let no_trailing = view("x");
        assert_eq!(no_trailing.line_count(), 1);
        assert_eq!(no_trailing.line(0), "x");

        let trailing = view("x\n");
        assert_eq!(trailing.line_count(), 2);
        assert_eq!(trailing.line(0), "x");
        assert_eq!(trailing.line(1), "");
    }

    #[test]
    fn scroll_clamps_to_last_screenful() {
        // 10 lines, viewport 4 → max scroll is 6.
        assert_eq!(clamp_scroll(0, 10, 4), 0);
        assert_eq!(clamp_scroll(6, 10, 4), 6);
        assert_eq!(clamp_scroll(9, 10, 4), 6);
        // Fewer lines than the viewport → no scroll.
        assert_eq!(clamp_scroll(3, 2, 4), 0);
    }

    #[test]
    fn byte_size_formatting() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(4198), "4.1 KB");
        assert_eq!(fmt_bytes(2 * 1024 * 1024), "2.0 MB");
    }

    #[test]
    fn status_summary_appends_truncated_marker() {
        let mut v = view("body");
        assert!(!status_summary(&v).contains("truncated"));
        v.truncated = true;
        v.byte_size = 10 * 1024 * 1024;
        assert_eq!(
            status_summary(&v),
            "200 OK · 5 ms · 10.0 MB · 0 hdrs · truncated at 10.0 MB"
        );
    }
}
