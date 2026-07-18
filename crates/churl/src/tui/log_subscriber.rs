//! Bounded-ring `tracing` subscriber backing the Log panel (`<leader>L`,
//! M8.3 Wave 4). A dedicated [`tracing_subscriber::Layer`] writes every
//! emitted event into a fixed-capacity ring buffer shared with the TUI; the
//! panel renders the ring (see [`super::components::log_panel`]). Attached
//! EXACTLY ONCE, at TUI startup ([`super::run`]) — never in headless, where
//! installing a second global subscriber would panic.
//!
//! Zero-overhead-when-off: [`LogRing::set_enabled`] mirrors
//! `App::debug_enabled` into a shared `AtomicBool`; [`RingLayer::enabled`]
//! reads it before doing any work, so `tracing`'s per-callsite interest cache
//! marks every event site disabled while debug capture is off — a
//! `tracing::debug!(...)` call site then costs one relaxed atomic load, no
//! field formatting, no allocation, no lock.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

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

/// Shared handle to the ring buffer and the debug-capture gate. Cheap to
/// clone (`Arc`-backed); [`super::app::App`] holds one (seeded inert at
/// construction, see [`LogRing::inert`]), and the [`RingLayer`] attached by
/// [`init`] holds another pointing at the same buffer/gate.
#[derive(Debug, Clone)]
pub struct LogRing {
    events: Arc<Mutex<VecDeque<LogEvent>>>,
    enabled: Arc<AtomicBool>,
    capacity: usize,
}

impl LogRing {
    /// An inert ring with no attached subscriber — the default for `App`
    /// construction (snapshot tests, or before [`init`] runs). The Log panel
    /// still renders (empty), it just never receives events.
    pub fn inert(capacity: usize) -> Self {
        Self {
            events: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            enabled: Arc::new(AtomicBool::new(false)),
            capacity,
        }
    }

    /// Flips the capture gate — kept in lockstep with `App::debug_enabled`
    /// (the same master toggle gates traces, Traffic, and the Log ring).
    /// `false` makes [`RingLayer::enabled`] return `false` for every event
    /// site: the zero-overhead path.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Pushes `event` directly into the ring, bypassing the capture gate —
    /// the [`RingLayer`] uses this from `on_event`; it is also the seam
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

/// The `tracing_subscriber::Layer` that writes into a [`LogRing`].
struct RingLayer {
    ring: LogRing,
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn enabled(&self, _metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        self.ring.enabled.load(Ordering::Relaxed)
    }

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
    let layer = RingLayer { ring: ring.clone() };
    if tracing_subscriber::registry()
        .with(layer)
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
    fn inert_ring_starts_empty_and_disabled() {
        let ring = LogRing::inert(4);
        assert!(ring.snapshot().is_empty());
        assert!(!ring.enabled.load(Ordering::Relaxed));
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

    #[test]
    fn set_enabled_flips_the_gate() {
        let ring = LogRing::inert(4);
        assert!(!ring.enabled.load(Ordering::Relaxed));
        ring.set_enabled(true);
        assert!(ring.enabled.load(Ordering::Relaxed));
        ring.set_enabled(false);
        assert!(!ring.enabled.load(Ordering::Relaxed));
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
