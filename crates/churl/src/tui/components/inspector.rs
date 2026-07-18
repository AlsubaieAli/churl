//! The debug Inspector overlay (`<leader>d`): a per-exchange, read-only view
//! of a captured [`DebugTrace`] — the resolved request, `{{var}}` resolution
//! steps, redirect hops + strip decisions, auth/cookie/proxy decisions, and
//! (on failure) the error cause chain.
//!
//! All UI state lives here (the `churl` crate); `churl-core` stays TUI-free —
//! this module only ever RENDERS a [`DebugTrace`] it is handed, never builds
//! one. Every rendered field comes from `DebugTrace`'s already-masked display
//! projections (`resolved_display`, `var_steps[].value_masked`,
//! `redirect_hops[].{from,to}`, `decisions.proxy`) or
//! [`DebugTrace::masked_curl`] — `resolved_raw` is never touched here, so a
//! secret can't leak through this overlay even if a future edit adds a new
//! field to render (there is nothing unmasked in scope to reach for).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use churl_core::debug::DebugTrace;
use churl_core::secrets;

use super::traffic::TrafficEntry;
use crate::tui::theme::Theme;

/// Which section of the trace is showing. Cycled with Tab/`l`/Right and
/// BackTab/`h`/Left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorSection {
    /// The masked resolved request: method, URL, headers, body presence.
    Request,
    /// Every `{{var}}` placeholder resolved for this exchange.
    Vars,
    /// Every redirect hop followed, with strip decisions.
    Redirects,
    /// Auth/cookie/proxy decisions made for this exchange.
    Decisions,
    /// The mapped error + cause chain, on a failed exchange.
    Error,
}

impl InspectorSection {
    const ALL: [InspectorSection; 5] = [
        InspectorSection::Request,
        InspectorSection::Vars,
        InspectorSection::Redirects,
        InspectorSection::Decisions,
        InspectorSection::Error,
    ];

    fn label(self) -> &'static str {
        match self {
            InspectorSection::Request => "Request",
            InspectorSection::Vars => "Vars",
            InspectorSection::Redirects => "Redirects",
            InspectorSection::Decisions => "Decisions",
            InspectorSection::Error => "Error",
        }
    }

    fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|&s| s == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|&s| s == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// What the app should do after the overlay handled a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorOutcome {
    /// Fully handled inside the overlay; nothing for the app to do.
    Consumed,
    /// Close the overlay (the app restores the parked mode).
    Close,
    /// Copy the resolved request as a masked `curl` command (the app reads
    /// `state.trace` and calls [`DebugTrace::masked_curl`] — never emitted
    /// when `trace` is `None`, so the app never needs to guard again).
    CopyCurl,
}

/// Full state of the open Inspector overlay. Per the illegal-states-
/// unrepresentable rule, this lives INSIDE `Mode::Inspector` (see
/// `tui/app/state.rs`) rather than a parallel `App` field.
#[derive(Debug, Clone)]
pub struct InspectorState {
    /// The CURRENTLY DISPLAYED exchange's trace. Starts as whatever the
    /// caller opened with (typically the latest exchange); `n`/`N` browsing
    /// (see `traffic`/`traffic_selected` below) refreshes it to the selected
    /// Traffic entry's trace. `None` when there is nothing to show — no
    /// exchange captured yet, or the selected Traffic entry's trace was
    /// evicted to bound memory — the render shows a placeholder instead of
    /// panicking or showing stale data.
    pub trace: Option<DebugTrace>,
    /// The active section tab.
    section: InspectorSection,
    /// Scroll offset (display lines) within the active section.
    scroll: usize,
    /// The session Traffic feed to browse (M8.3 Wave 4), newest first. Empty
    /// when the caller opened via [`Self::new`] (legacy/tests) or the
    /// session has captured nothing yet — `n`/`N` are then no-ops.
    traffic: Vec<TrafficEntry>,
    /// Index into `traffic` of the entry `trace` currently mirrors, once the
    /// user has navigated into the list with `n`/`N`. `None` means `trace`
    /// is still showing the ambient default the overlay opened with, not a
    /// specific Traffic entry — kept separate from `Some(0)` so `N` from the
    /// very first browsed entry can distinguish "back to the default view"
    /// from "already at the newest browsed entry".
    traffic_selected: Option<usize>,
}

impl InspectorState {
    /// Builds the overlay state from a single trace (or `None`), with no
    /// Traffic list to browse — the pre-Wave-4 constructor, kept for
    /// existing call sites/tests that don't have a Traffic feed in scope.
    pub fn new(trace: Option<DebugTrace>) -> Self {
        Self::open(trace, Vec::new())
    }

    /// Builds the overlay state from `trace` (the ambient default view —
    /// typically the latest exchange) plus `traffic` (newest-first) to
    /// browse with `n`/`N`. See the field docs above for why the default
    /// view is a separate parameter from `traffic[0]` rather than derived
    /// from it.
    pub fn open(trace: Option<DebugTrace>, traffic: Vec<TrafficEntry>) -> Self {
        Self {
            trace,
            section: InspectorSection::Request,
            scroll: 0,
            traffic,
            traffic_selected: None,
        }
    }

    /// Selects Traffic entry `index`, refreshing `trace` to mirror it and
    /// resetting scroll. A no-op when `index` is out of range.
    fn select_traffic(&mut self, index: usize) {
        if let Some(entry) = self.traffic.get(index) {
            self.traffic_selected = Some(index);
            self.trace = entry.trace.clone();
            self.scroll = 0;
        }
    }

    /// Handles one key event, returning what the app should do next.
    pub fn handle_key(&mut self, key: KeyEvent) -> InspectorOutcome {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => InspectorOutcome::Close,
            KeyCode::Tab | KeyCode::Char('l') | KeyCode::Right => {
                self.section = self.section.next();
                self.scroll = 0;
                InspectorOutcome::Consumed
            }
            KeyCode::BackTab | KeyCode::Char('h') | KeyCode::Left => {
                self.section = self.section.prev();
                self.scroll = 0;
                InspectorOutcome::Consumed
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let max = self.content_lines().len().saturating_sub(1);
                self.scroll = (self.scroll + 1).min(max);
                InspectorOutcome::Consumed
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                InspectorOutcome::Consumed
            }
            KeyCode::Char('g') => {
                self.scroll = 0;
                InspectorOutcome::Consumed
            }
            KeyCode::Char('G') => {
                self.scroll = self.content_lines().len().saturating_sub(1);
                InspectorOutcome::Consumed
            }
            // Browse the Traffic feed: `n` = older, `N` = newer (back toward
            // the ambient default). No-ops when there is nothing to browse —
            // falls through to the catch-all `Consumed` below.
            KeyCode::Char('n') if !self.traffic.is_empty() => {
                let next = self
                    .traffic_selected
                    .map_or(0, |i| (i + 1).min(self.traffic.len() - 1));
                self.select_traffic(next);
                InspectorOutcome::Consumed
            }
            KeyCode::Char('N') if !self.traffic.is_empty() => {
                if let Some(i) = self.traffic_selected
                    && i > 0
                {
                    self.select_traffic(i - 1);
                }
                InspectorOutcome::Consumed
            }
            // A no-op (falls through to `Consumed`) when there is nothing to
            // copy — never emits `CopyCurl` for the app to act on with no trace.
            KeyCode::Char('y') if self.trace.is_some() => InspectorOutcome::CopyCurl,
            _ => InspectorOutcome::Consumed,
        }
    }

    /// The active section's content as plain text lines — the single seam
    /// both scroll-clamping ([`Self::handle_key`]) and [`render`] build from,
    /// so the two can never drift apart.
    fn content_lines(&self) -> Vec<String> {
        let Some(trace) = self.trace.as_ref() else {
            // Distinguish WHY there's nothing to show: a selected Traffic
            // entry whose trace was evicted to bound memory (`Some(i)`) is a
            // different situation from simply not having navigated into
            // Traffic yet (`None`) even though history exists to browse —
            // conflating the two would misreport "evicted" when nothing was
            // ever selected.
            if self.traffic_selected.is_some() {
                return vec![
                    "this exchange's trace is no longer retained (evicted to bound".to_owned(),
                    "memory) — browse a more recent one with n/N.".to_owned(),
                ];
            }
            if !self.traffic.is_empty() {
                return vec![
                    "no trace captured for this exchange yet.".to_owned(),
                    String::new(),
                    format!(
                        "{} exchange{} in the session Traffic feed — press n to browse.",
                        self.traffic.len(),
                        if self.traffic.len() == 1 { "" } else { "s" }
                    ),
                ];
            }
            return vec![
                "no trace captured for this exchange.".to_owned(),
                String::new(),
                "enable debug capture (<leader>D) and send a request — the".to_owned(),
                "Inspector shows the most recently captured exchange.".to_owned(),
            ];
        };
        match self.section {
            InspectorSection::Request => request_lines(trace),
            InspectorSection::Vars => var_lines(trace),
            InspectorSection::Redirects => redirect_lines(trace),
            InspectorSection::Decisions => decision_lines(trace),
            InspectorSection::Error => error_lines(trace),
        }
    }
}

/// The masked resolved request: `METHOD url`, one line per enabled header,
/// then whether a body is present. Never the body content itself (mirrors the
/// M8.2 headless request-echo policy `ResolvedRequest` already follows).
fn request_lines(trace: &DebugTrace) -> Vec<String> {
    let r = &trace.resolved_display;
    let mut lines = vec![format!("{} {}", r.method, r.url)];
    if r.headers.is_empty() {
        lines.push("  (no headers)".to_owned());
    } else {
        for h in &r.headers {
            lines.push(format!("  {}: {}", h.name, h.value));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "body: {}",
        if r.body_present {
            "present (not shown)"
        } else {
            "none"
        }
    ));
    lines
}

/// One line per resolved `{{var}}`, in substitution order.
fn var_lines(trace: &DebugTrace) -> Vec<String> {
    if trace.var_steps.is_empty() {
        return vec!["(no {{var}} placeholders resolved)".to_owned()];
    }
    trace
        .var_steps
        .iter()
        .map(|v| {
            let scope = v.scope.unwrap_or("env");
            format!(
                "{}{}{}  →  {}   [{}]",
                "{{", v.name, "}}", v.value_masked, scope
            )
        })
        .collect()
}

/// One block per redirect hop: `N. status from → to`, then any method change,
/// cross-origin flag, and stripped-header names.
fn redirect_lines(trace: &DebugTrace) -> Vec<String> {
    if trace.redirect_hops.is_empty() {
        return vec!["(no redirects followed)".to_owned()];
    }
    let mut lines = Vec::new();
    for (i, hop) in trace.redirect_hops.iter().enumerate() {
        lines.push(format!(
            "{}. {} {} → {}",
            i + 1,
            hop.status,
            hop.from,
            hop.to
        ));
        if let Some((from_method, to_method)) = hop.method_change {
            lines.push(format!("   method: {from_method} → {to_method}"));
        }
        if hop.cross_origin {
            lines.push("   cross-origin".to_owned());
        }
        if !hop.stripped_headers.is_empty() {
            lines.push(format!(
                "   stripped headers: {}",
                hop.stripped_headers.join(", ")
            ));
        }
    }
    lines
}

/// Auth/cookie/proxy decisions, one line each.
fn decision_lines(trace: &DebugTrace) -> Vec<String> {
    let d = &trace.decisions;
    vec![
        format!(
            "auth injected: {}",
            d.auth_injected.as_deref().unwrap_or("(none)")
        ),
        format!(
            "cookie jar used: {}",
            if d.cookie_used { "yes" } else { "no" }
        ),
        format!("proxy: {}", d.proxy.as_deref().unwrap_or("(none)")),
    ]
}

/// The mapped error's message, then its source chain, outermost cause first.
/// `(no error …)` on a successful exchange, or one that has not completed.
fn error_lines(trace: &DebugTrace) -> Vec<String> {
    match &trace.error {
        None => vec!["(no error — exchange succeeded, or has not completed yet)".to_owned()],
        Some(err) => {
            let mut lines = vec![err.message.clone()];
            for cause in &err.source_chain {
                lines.push(format!("  caused by: {cause}"));
            }
            lines
        }
    }
}

/// Renders the Inspector overlay over `area`: a bordered modal with a section
/// tab row and the active section's scrollable content below.
pub fn render(frame: &mut Frame, area: Rect, state: &InspectorState, theme: &Theme) {
    let [modal] = Layout::horizontal([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(modal);
    frame.render_widget(Clear, modal);

    let title = if state.traffic.is_empty() {
        " Inspector — tab sections · y copy curl (masked) · q/esc close ".to_owned()
    } else {
        let pos = state
            .traffic_selected
            .map_or("live".to_owned(), |i| (i + 1).to_string());
        format!(
            " Inspector — tab sections · n/N traffic [{pos}/{}] · y copy curl (masked) · q/esc close ",
            state.traffic.len()
        )
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let [tabs_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    render_tabs(frame, tabs_area, state.section, theme);

    let content = state.content_lines();
    let total = content.len();
    // Lines that echo a mask token (e.g. a header/var value that was
    // secret-shaped) get the same visual treatment as a masked literal
    // elsewhere in the app — a steady, recognizable "this was redacted" cue.
    let lines: Vec<Line> = content
        .into_iter()
        .map(|text| {
            if text.contains(secrets::SECRET_MASK) {
                Line::styled(text, theme.auth_mask)
            } else {
                Line::raw(text)
            }
        })
        .collect();
    let scroll = state.scroll.min(total.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body_area);
}

/// The section tab row: the active section gets the bright `selection` chip
/// fill, every other section the dim `tab_inactive` fill — the same pair the
/// request/tab-strip tab bars use, so a tab row reads consistently everywhere.
fn render_tabs(frame: &mut Frame, area: Rect, active: InspectorSection, theme: &Theme) {
    let spans: Vec<Span> = InspectorSection::ALL
        .iter()
        .map(|&section| {
            let style = if section == active {
                theme.selection
            } else {
                theme.tab_inactive
            };
            Span::styled(format!(" {} ", section.label()), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use churl_core::model::{Method, Request};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    fn request() -> Request {
        Request {
            method: Method::Get,
            url: "https://api.test/x".to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
            insecure: false,
        }
    }

    #[test]
    fn no_trace_shows_placeholder_and_never_copies() {
        let mut state = InspectorState::new(None);
        assert!(state.content_lines()[0].contains("no trace captured"));
        assert_eq!(
            state.handle_key(key(KeyCode::Char('y'))),
            InspectorOutcome::Consumed,
            "y is a no-op with no trace to copy"
        );
    }

    #[test]
    fn tab_cycles_sections_forward_and_back() {
        let mut state = InspectorState::new(Some(DebugTrace::from_request(&request())));
        assert_eq!(state.section, InspectorSection::Request);
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.section, InspectorSection::Vars);
        state.handle_key(key(KeyCode::BackTab));
        assert_eq!(state.section, InspectorSection::Request);
        // Wraps at both ends.
        state.handle_key(key(KeyCode::BackTab));
        assert_eq!(state.section, InspectorSection::Error);
    }

    #[test]
    fn switching_section_resets_scroll() {
        let mut state = InspectorState::new(Some(DebugTrace::from_request(&request())));
        state.scroll = 3;
        state.handle_key(key(KeyCode::Tab));
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn scroll_clamps_to_content_length() {
        let mut state = InspectorState::new(Some(DebugTrace::from_request(&request())));
        // The Request section has a handful of lines; scrolling far past the
        // end must clamp, never wrap or underflow.
        for _ in 0..50 {
            state.handle_key(key(KeyCode::Down));
        }
        let max = state.content_lines().len().saturating_sub(1);
        assert_eq!(state.scroll, max);
        state.handle_key(key(KeyCode::Char('k')));
        assert_eq!(state.scroll, max - 1);
    }

    #[test]
    fn y_emits_copy_curl_only_with_a_trace() {
        let mut state = InspectorState::new(Some(DebugTrace::from_request(&request())));
        assert_eq!(
            state.handle_key(key(KeyCode::Char('y'))),
            InspectorOutcome::CopyCurl
        );
    }

    #[test]
    fn q_and_esc_close() {
        let mut state = InspectorState::new(None);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q'))),
            InspectorOutcome::Close
        );
        assert_eq!(state.handle_key(key(KeyCode::Esc)), InspectorOutcome::Close);
    }

    #[test]
    fn error_section_shows_no_error_placeholder_on_a_clean_trace() {
        let trace = DebugTrace::from_request(&request());
        let lines = error_lines(&trace);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("no error"));
    }

    fn traffic_entry(url: &str) -> TrafficEntry {
        let mut req = request();
        req.url = url.to_owned();
        TrafficEntry::new(
            url.to_owned(),
            crate::tui::components::traffic::TrafficOutcome::Ok(200),
            Some(5),
            DebugTrace::from_request(&req),
        )
    }

    #[test]
    fn n_and_shift_n_browse_traffic_and_wrap_correctly() {
        let traffic = vec![
            traffic_entry("https://api.test/newest"),
            traffic_entry("https://api.test/older"),
        ];
        let mut state = InspectorState::open(None, traffic);
        // Ambient default: nothing selected yet, but Traffic has history.
        assert!(state.trace.is_none());
        assert!(
            state.content_lines()[0].contains("no trace captured for this exchange yet"),
            "{:?}",
            state.content_lines()
        );

        state.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            state.trace.as_ref().unwrap().resolved_display.url,
            "https://api.test/newest"
        );
        state.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            state.trace.as_ref().unwrap().resolved_display.url,
            "https://api.test/older"
        );
        // At the oldest entry, `n` (older) stays put — no wrap, no panic.
        state.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            state.trace.as_ref().unwrap().resolved_display.url,
            "https://api.test/older"
        );
        // `N` (newer) steps back.
        state.handle_key(key(KeyCode::Char('N')));
        assert_eq!(
            state.trace.as_ref().unwrap().resolved_display.url,
            "https://api.test/newest"
        );
    }

    #[test]
    fn n_is_a_no_op_with_an_empty_traffic_list() {
        let mut state = InspectorState::new(Some(DebugTrace::from_request(&request())));
        let before = state.trace.as_ref().unwrap().resolved_display.url.clone();
        state.handle_key(key(KeyCode::Char('n')));
        assert_eq!(state.trace.as_ref().unwrap().resolved_display.url, before);
    }

    #[test]
    fn evicted_traffic_entry_shows_a_distinct_placeholder_from_never_selected() {
        let mut entry = traffic_entry("https://api.test/gone");
        entry.trace = None; // simulates eviction past the live-trace window
        let mut state = InspectorState::open(None, vec![entry]);
        state.handle_key(key(KeyCode::Char('n')));
        assert!(state.trace.is_none());
        assert!(
            state.content_lines()[0].contains("no longer retained"),
            "{:?}",
            state.content_lines()
        );
    }
}
