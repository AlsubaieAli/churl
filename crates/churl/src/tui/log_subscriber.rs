//! Bounded-ring `tracing` subscriber backing the Log panel (`<leader>L`,
//! M8.3 Wave 4). A dedicated [`tracing_subscriber::Layer`] writes every
//! emitted event into a fixed-capacity ring buffer shared with the TUI; the
//! panel renders the ring (see [`super::components::log_panel`]). Attached
//! EXACTLY ONCE, at TUI startup ([`super::run`]) — never in headless, where
//! installing a second global subscriber would panic.
//!
//! **Secrets discipline — only churl's own targets reach the ring.** The
//! writing layer is wrapped in a [`Targets`] filter that admits ONLY the
//! `churl`/`churl_core` targets and excludes everything else. This keeps
//! third-party dependency logs — reqwest/hyper/h2/tokio, which at TRACE level
//! dump raw cleartext socket bytes (Authorization/Cookie/body on plaintext
//! `http`) — out of the ring entirely. churl's own events are secret-safe by
//! construction (every emit site logs already-masked projections), so the
//! ring can never carry an unmasked secret.
//!
//! **Zero-overhead-when-off is enforced at the EMIT sites, not here.** Each
//! `tracing::debug!(...)` call in the send/load/sequence paths sits behind an
//! `if let Some(trace) = …` guard that is `Some` only when
//! `App::debug_enabled` — so with debug off, no churl event is ever emitted
//! and the ring stays empty. (A layer-level `enabled()` gate keyed on a
//! mutable flag would NOT achieve this: `tracing` caches per-callsite
//! interest, so it would not re-consult a flag flipped at runtime. The
//! emit-site guard is the real, correct gate.)

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

/// The one true target allow-list: churl's own crates only. `Targets` matches
/// by `::`-separated path segments, so `"churl"` admits `churl::send` /
/// `churl::load` / … but NOT `churl_core` (a distinct first segment), hence
/// both entries. Anything unmatched (reqwest, hyper, h2, tokio, …) is
/// excluded — there is no default level, so non-churl targets never pass.
fn churl_targets() -> Targets {
    Targets::new()
        .with_target("churl", Level::TRACE)
        .with_target("churl_core", Level::TRACE)
}

/// Entries kept in the ring. Log lines are lightweight (no bodies/traces —
/// those live in the Traffic feed), so this can afford a generous cap.
pub const LOG_RING_CAPACITY: usize = 256;

/// One captured log line.
#[derive(Debug, Clone)]
pub struct LogEvent {
    /// The event's level (`ERROR`/`WARN`/`INFO`/`DEBUG`/`TRACE`).
    pub level: Level,
    /// The event's target (module path by default).
    pub target: String,
    /// The formatted `message` field, plus any other fields appended as
    /// `name=value` pairs.
    pub message: String,
}

/// Shared handle to the ring buffer. Cheap to clone (`Arc`-backed);
/// [`super::app::App`] holds one (seeded inert at construction, see
/// [`LogRing::inert`]), and the [`RingLayer`] attached by [`init`] holds
/// another pointing at the same buffer.
#[derive(Debug, Clone)]
pub struct LogRing {
    events: Arc<Mutex<VecDeque<LogEvent>>>,
    capacity: usize,
}

impl LogRing {
    /// An inert ring with no attached subscriber — the default for `App`
    /// construction (snapshot tests, or before [`init`] runs). The Log panel
    /// still renders (empty), it just never receives events.
    pub fn inert(capacity: usize) -> Self {
        Self {
            events: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Pushes `event` directly into the ring — the [`RingLayer`] uses this
    /// from `on_event`; it is also the seam
    /// [`super::app::App::seed_log_ring_for_test`] uses to drive the Log
    /// panel deterministically in snapshot tests, where the real `tracing`
    /// subscriber is never attached (see the module docs).
    pub fn push(&self, event: LogEvent) {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events.push_back(event);
        while events.len() > self.capacity {
            events.pop_front();
        }
    }

    /// A snapshot of the ring's current contents, oldest first — cloned out
    /// from behind the lock so the render path never holds it.
    pub fn snapshot(&self) -> Vec<LogEvent> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// The `tracing_subscriber::Layer` that writes into a [`LogRing`]. It has no
/// `enabled()` gate of its own — target filtering is the [`Targets`] wrapper
/// applied in [`ring_layer`], and debug on/off gating happens at the emit
/// sites (see the module docs).
struct RingLayer {
    ring: LogRing,
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.ring.push(LogEvent {
            level: *event.metadata().level(),
            target: event.metadata().target().to_owned(),
            message: visitor.finish(),
        });
    }
}

/// Builds the churl-targets-only writing layer over `ring`: [`RingLayer`]
/// wrapped in the [`churl_targets`] filter. Shared by [`init`] (global
/// attach) and the unit test (scoped attach) so both exercise the exact same
/// filter construction.
fn ring_layer<S>(ring: LogRing) -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    RingLayer { ring }.with_filter(churl_targets())
}

/// Renders an event's fields into one display string: the `message` field
/// first (tracing's conventional unnamed field), then any other fields as
/// `name=value`, space-separated.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    extra: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            if !self.extra.is_empty() {
                self.extra.push(' ');
            }
            self.extra
                .push_str(&format!("{}={:?}", field.name(), value));
        }
    }
}

impl MessageVisitor {
    fn finish(mut self) -> String {
        if !self.extra.is_empty() {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push_str(&self.extra);
        }
        self.message
    }
}

/// Builds a fresh [`LogRing`] and attaches its writing layer to the GLOBAL
/// `tracing` subscriber. Callable safely more than once (a second attach is
/// logged and skipped rather than panicking), but by contract [`super::run`]
/// is the only call site and calls it exactly once — headless never touches
/// this module.
pub fn init(capacity: usize) -> LogRing {
    let ring = LogRing::inert(capacity);
    if tracing_subscriber::registry()
        .with(ring_layer(ring.clone()))
        .try_init()
        .is_err()
    {
        // Non-fatal: a global subscriber is already installed (should not
        // happen under the one-call-site contract). The ring still works as
        // a plain shared buffer — it just never receives events — so the Log
        // panel degrades to "empty" instead of crashing the session.
        eprintln!("churl: tracing subscriber already initialized — Log panel will stay empty");
    }
    ring
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inert_ring_starts_empty() {
        let ring = LogRing::inert(4);
        assert!(ring.snapshot().is_empty());
    }

    #[test]
    fn push_bounds_to_capacity_evicting_oldest() {
        let ring = LogRing::inert(3);
        for i in 0..5 {
            ring.push(LogEvent {
                level: Level::DEBUG,
                target: "test".to_owned(),
                message: i.to_string(),
            });
        }
        let snapshot = ring.snapshot();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].message, "2");
        assert_eq!(snapshot[2].message, "4");
    }

    /// Drives REAL `tracing` events through the actual attached layer + filter
    /// (via a thread-scoped subscriber, so it never touches the process-global
    /// one) and asserts the target filter's security contract: a churl-target
    /// event lands in the ring, but a foreign-target event (simulating
    /// reqwest/hyper's raw cleartext socket dumps) does NOT — even one carrying
    /// a secret-shaped payload. This is the test-theater fix: it exercises the
    /// subscriber end-to-end, not the `push`/`seed_log_ring_for_test` bypass.
    #[test]
    fn target_filter_admits_churl_and_rejects_foreign_targets() {
        let ring = LogRing::inert(16);
        let subscriber = tracing_subscriber::registry().with(ring_layer(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            // Foreign targets (reqwest/hyper/h2/tokio) — the ones that dump raw
            // cleartext. None may reach the ring.
            tracing::info!(target: "hyper::client", "Authorization: Bearer sk-live-LEAKED");
            tracing::trace!(target: "reqwest::connect", "raw bytes GET / cookie=sk-live-LEAKED");
            tracing::error!(target: "h2::codec", "sk-live-LEAKED");
            tracing::debug!(target: "tokio::net", "sk-live-LEAKED");
            // churl's own target — secret-safe by construction — DOES land.
            tracing::debug!(target: "churl::send", "GET https://api.test/x status=200");
            // A churl_core sub-target also lands (distinct first path segment).
            tracing::debug!(target: "churl_core::http", "redirect followed");
        });

        let events = ring.snapshot();
        assert_eq!(
            events.len(),
            2,
            "only the two churl-target events may land, got: {events:?}"
        );
        assert!(events.iter().all(|e| e.target.starts_with("churl")));
        assert!(
            !events.iter().any(|e| e.message.contains("sk-live-LEAKED")),
            "no foreign cleartext may reach the ring: {events:?}"
        );
        assert!(events.iter().any(|e| e.target == "churl::send"));
        assert!(events.iter().any(|e| e.target == "churl_core::http"));
    }

    #[test]
    fn message_visitor_renders_message_then_extra_fields() {
        // `record_debug` is exercised indirectly via `tracing::debug!` in the
        // integration path; here we just verify the join-order seam. Simulate
        // what `event.record(&mut visitor)` would do for
        // `tracing::debug!(status = 200, "sent")`.
        let mut visitor = MessageVisitor::default();
        visitor.extra.push_str("status=200");
        visitor.message = "\"sent\"".to_owned();
        assert_eq!(visitor.finish(), "\"sent\" status=200");
    }
}
