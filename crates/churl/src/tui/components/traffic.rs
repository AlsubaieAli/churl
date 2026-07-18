//! The session Traffic feed (M8.3 Wave 4): an ephemeral, in-memory record of
//! EVERY exchange this session — standalone sends, and every copy/step of a
//! load or sequence run — captured only while [`crate::tui::app::App::debug_enabled`]
//! is on. Sits alongside the durable History (metadata-only,
//! standalone-sends-only, unaffected by this feed): Traffic is the full-trace
//! ephemeral counterpart, never written to `state.sqlite`.
//!
//! Bounded like [`super::load_runner`]'s live-view window, but at session
//! grain rather than one batch's: pushing beyond [`TRAFFIC_CAP`] evicts the
//! oldest entry entirely (the feed spans the whole session, so it must shed
//! history over time, unlike a load run's fixed-size result set); within the
//! retained window, only the newest [`LIVE_TRAFFIC_WINDOW`] entries keep a
//! live [`DebugTrace`] — older ones are downgraded to `trace: None`, an
//! honest placeholder mirroring [`super::response::ResponseState::Dropped`]:
//! the summary line survives, the trace bytes are released.

use std::collections::VecDeque;

use churl_core::debug::DebugTrace;
use churl_core::model::Method;

/// Max entries retained in the feed; pushing past this evicts the oldest.
pub const TRAFFIC_CAP: usize = 64;

/// Within the retained entries, how many of the newest keep a live
/// [`DebugTrace`]. Mirrors `load_runner::LIVE_VIEW_WINDOW`'s magnitude.
pub const LIVE_TRAFFIC_WINDOW: usize = 16;

/// How a captured exchange completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficOutcome {
    /// Completed with a success status (< 400).
    Ok(u16),
    /// Completed with an HTTP error status (>= 400).
    Failed(u16),
    /// Could not be sent (transport error).
    Error,
}

/// One completed exchange captured for the session Traffic feed.
#[derive(Debug, Clone)]
pub struct TrafficEntry {
    /// A short label identifying the exchange's origin (e.g. the endpoint
    /// name, `"load #3"`, or `"sequence: step name"`).
    pub label: String,
    /// The request method (from the trace's masked display projection, kept
    /// even after the trace itself is dropped so the list still reads).
    pub method: Method,
    /// The masked resolved URL (see [`churl_core::secrets::mask_url`]).
    pub url: String,
    /// How the exchange completed.
    pub outcome: TrafficOutcome,
    /// Request duration, when known.
    pub ms: Option<u64>,
    /// The full captured trace, when still retained (see the module docs'
    /// live-window bounding). `None` once evicted — the fields above still
    /// render an honest summary line.
    pub trace: Option<DebugTrace>,
}

impl TrafficEntry {
    /// Builds an entry from a freshly captured `trace` (never `None` at
    /// construction — only [`push`] downgrades it later).
    pub fn new(label: String, outcome: TrafficOutcome, ms: Option<u64>, trace: DebugTrace) -> Self {
        Self {
            label,
            method: trace.resolved_display.method,
            url: trace.resolved_display.url.clone(),
            outcome,
            ms,
            trace: Some(trace),
        }
    }
}

/// Pushes `entry` into `feed`, then enforces both bounds: evict the oldest
/// entry once beyond [`TRAFFIC_CAP`], and drop the trace payload (not the
/// entry) on anything older than the newest [`LIVE_TRAFFIC_WINDOW`].
pub fn push(feed: &mut VecDeque<TrafficEntry>, entry: TrafficEntry) {
    feed.push_back(entry);
    while feed.len() > TRAFFIC_CAP {
        feed.pop_front();
    }
    let live_from = feed.len().saturating_sub(LIVE_TRAFFIC_WINDOW);
    for e in feed.iter_mut().take(live_from) {
        e.trace = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use churl_core::model::{Method, Request};

    fn trace() -> DebugTrace {
        DebugTrace::from_request(&Request {
            method: Method::Get,
            url: "https://api.test/x".to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
            insecure: false,
        })
    }

    fn entry(label: &str) -> TrafficEntry {
        TrafficEntry::new(label.to_owned(), TrafficOutcome::Ok(200), Some(12), trace())
    }

    #[test]
    fn push_retains_entries_up_to_the_cap() {
        let mut feed = VecDeque::new();
        for i in 0..TRAFFIC_CAP {
            push(&mut feed, entry(&i.to_string()));
        }
        assert_eq!(feed.len(), TRAFFIC_CAP);
        assert_eq!(feed.front().unwrap().label, "0");
    }

    #[test]
    fn push_beyond_cap_evicts_oldest_entirely() {
        let mut feed = VecDeque::new();
        for i in 0..(TRAFFIC_CAP + 5) {
            push(&mut feed, entry(&i.to_string()));
        }
        assert_eq!(feed.len(), TRAFFIC_CAP);
        // The oldest 5 entries (0..5) are gone; the feed starts at "5".
        assert_eq!(feed.front().unwrap().label, "5");
        assert_eq!(feed.back().unwrap().label, (TRAFFIC_CAP + 4).to_string());
    }

    #[test]
    fn only_newest_live_window_keeps_a_trace() {
        let mut feed = VecDeque::new();
        for i in 0..(LIVE_TRAFFIC_WINDOW + 3) {
            push(&mut feed, entry(&i.to_string()));
        }
        let dropped = feed.iter().filter(|e| e.trace.is_none()).count();
        let live = feed.iter().filter(|e| e.trace.is_some()).count();
        assert_eq!(live, LIVE_TRAFFIC_WINDOW);
        assert_eq!(dropped, 3);
        // The newest entries are the ones that stayed live.
        assert!(feed.back().unwrap().trace.is_some());
        assert!(feed.front().unwrap().trace.is_none());
    }

    #[test]
    fn dropped_entry_keeps_its_summary_metadata() {
        let mut feed = VecDeque::new();
        for i in 0..(LIVE_TRAFFIC_WINDOW + 1) {
            push(&mut feed, entry(&i.to_string()));
        }
        let oldest = feed.front().unwrap();
        assert!(oldest.trace.is_none());
        assert_eq!(oldest.label, "0");
        assert_eq!(oldest.url, "https://api.test/x");
        assert_eq!(oldest.outcome, TrafficOutcome::Ok(200));
    }
}
