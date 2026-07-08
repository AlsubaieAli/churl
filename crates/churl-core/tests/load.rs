//! Integration tests for the M7.5 concurrent-load runner (`churl_core::load`),
//! asserted against a real `wiremock` server: exact N-copy fan-out with timing
//! capture, the bounded-concurrency guarantee proven with an in-responder gauge,
//! failure classification, interval pacing, and the total=0 no-op.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use churl_core::http::{DEFAULT_TIMEOUT, ExecuteOptions, build_client};
use churl_core::load::{LoadConfig, ReqOutcome, run_load, stats};
use churl_core::model::{Method, Request};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as WmRequest, Respond, ResponseTemplate};

/// A minimal GET request at `url`.
fn get(url: String) -> Request {
    Request {
        method: Method::Get,
        url,
        headers: Vec::new(),
        params: Vec::new(),
        body: None,
        auth: None,
    }
}

fn cfg(total: usize, concurrency: usize, interval_ms: u64) -> LoadConfig {
    LoadConfig {
        total,
        concurrency,
        interval: Duration::from_millis(interval_ms),
    }
}

#[tokio::test]
async fn fires_exactly_total_copies_with_timings() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ping"))
        .respond_with(ResponseTemplate::new(200).set_body_string("pong"))
        .expect(20) // the mock asserts it received EXACTLY 20 requests on drop
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let request = get(format!("{}/ping", server.uri()));
    let outcomes = run_load(
        &client,
        &request,
        &cfg(20, 5, 0),
        &ExecuteOptions::default(),
    )
    .await;

    assert_eq!(outcomes.len(), 20, "one outcome per copy");
    assert!(
        outcomes
            .iter()
            .all(|(o, t)| matches!(o, ReqOutcome::Ok { status: 200 }) && t.is_some()),
        "every copy is Ok(200) with a captured timing"
    );
    let s = stats(&outcomes);
    assert_eq!(s.ok, 20);
    assert_eq!(s.failed, 0);
    assert_eq!(s.errored, 0);
    assert!(s.min.is_some() && s.max.is_some() && s.mean.is_some());
    // `expect(20)` is verified when the server drops at end of scope.
}

/// A responder that records each request's arrival [`Instant`]. wiremock matches
/// then calls `respond` under an internal lock (so arrivals are recorded serially
/// and briefly), then applies the async `set_delay` outside that lock — so the
/// holds genuinely overlap and the true peak in-flight count is recoverable from
/// the arrival timestamps: request `i` is in flight over the interval from
/// `arrival_i` to `arrival_i` plus the hold, and the maximum number of
/// overlapping such intervals is the peak concurrency.
struct ArrivalRecorder {
    arrivals: Arc<Mutex<Vec<Instant>>>,
    hold: Duration,
}

impl Respond for ArrivalRecorder {
    fn respond(&self, _request: &WmRequest) -> ResponseTemplate {
        self.arrivals.lock().unwrap().push(Instant::now());
        // Async hold (non-blocking) so wiremock can serve overlapping copies.
        ResponseTemplate::new(200).set_delay(self.hold)
    }
}

/// The maximum number of `[arrival, arrival + hold]` intervals overlapping at
/// once — the peak concurrent in-flight count.
fn peak_overlap(arrivals: &[Instant], hold: Duration) -> usize {
    // Event sweep: +1 at each arrival, -1 at each arrival+hold.
    let mut events: Vec<(Instant, i32)> = Vec::with_capacity(arrivals.len() * 2);
    for &a in arrivals {
        events.push((a, 1));
        events.push((a + hold, -1));
    }
    // Sort by time; process ends (-1) before starts (+1) at an exact tie so an
    // interval that ends exactly when another starts does not count as overlap.
    events.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(&y.1)));
    let mut current = 0i32;
    let mut peak = 0i32;
    for (_, delta) in events {
        current += delta;
        peak = peak.max(current);
    }
    peak.max(0) as usize
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_concurrency_never_exceeds_the_cap() {
    let server = MockServer::start().await;
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let hold = Duration::from_millis(80);
    Mock::given(method("GET"))
        .and(path("/gauge"))
        .respond_with(ArrivalRecorder {
            arrivals: Arc::clone(&arrivals),
            hold,
        })
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let request = get(format!("{}/gauge", server.uri()));
    let concurrency = 4;
    let outcomes = run_load(
        &client,
        &request,
        &cfg(24, concurrency, 0),
        &ExecuteOptions::default(),
    )
    .await;

    assert_eq!(outcomes.len(), 24);
    let arrivals = arrivals.lock().unwrap().clone();
    assert_eq!(arrivals.len(), 24, "the server saw exactly 24 requests");
    let peak = peak_overlap(&arrivals, hold);
    assert!(
        peak <= concurrency,
        "peak in-flight {peak} exceeded the concurrency cap {concurrency}"
    );
    assert!(
        peak >= 2,
        "expected real overlap (peak {peak}); the bound was not meaningfully exercised"
    );
}

#[tokio::test]
async fn failures_are_classified_and_counted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/boom"))
        .respond_with(ResponseTemplate::new(500).set_body_string("nope"))
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let request = get(format!("{}/boom", server.uri()));
    let outcomes = run_load(
        &client,
        &request,
        &cfg(10, 3, 0),
        &ExecuteOptions::default(),
    )
    .await;

    assert_eq!(outcomes.len(), 10);
    assert!(
        outcomes
            .iter()
            .all(|(o, _)| matches!(o, ReqOutcome::Failed { status: 500 })),
        "a 500 response is a Failed outcome, not an Error"
    );
    let s = stats(&outcomes);
    assert_eq!(s.failed, 10);
    assert_eq!(s.ok, 0);
    assert_eq!(s.errored, 0);
    // A 500 still has a timing, so latency stats are populated.
    assert!(s.median.is_some());
}

#[tokio::test]
async fn transport_error_is_errored_without_timing() {
    // No server: connecting to an unroutable address errors at the transport
    // layer → Error outcome, no timing.
    let client = build_client(Duration::from_millis(300)).unwrap();
    // Reserved TEST-NET-1 address (RFC 5737) — refuses / times out fast.
    let request = get("http://192.0.2.1:9/never".to_owned());
    let outcomes = run_load(&client, &request, &cfg(3, 3, 0), &ExecuteOptions::default()).await;

    assert_eq!(outcomes.len(), 3);
    assert!(
        outcomes
            .iter()
            .all(|(o, t)| matches!(o, ReqOutcome::Error(_)) && t.is_none()),
        "unreachable host yields Error outcomes with no timing"
    );
    let s = stats(&outcomes);
    assert_eq!(s.errored, 3);
    assert_eq!(s.min, None);
    assert_eq!(s.median, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interval_pacing_spreads_the_wall_clock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/paced"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let request = get(format!("{}/paced", server.uri()));
    // total=6, concurrency=2, interval=30ms. Lower bound on wall-clock:
    // (ceil(6/2) - 1) * 30ms = 2 * 30ms = 60ms. The absolute-target pacing in
    // run_load actually floors it higher ((total-1)*interval), but assert only
    // the loose design bound to avoid flake.
    let interval_ms = 30;
    let start = Instant::now();
    let outcomes = run_load(
        &client,
        &request,
        &cfg(6, 2, interval_ms),
        &ExecuteOptions::default(),
    )
    .await;
    let elapsed = start.elapsed();

    assert_eq!(outcomes.len(), 6);
    let lower_bound = Duration::from_millis((6usize.div_ceil(2) as u64 - 1) * interval_ms);
    assert!(
        elapsed >= lower_bound,
        "paced run finished in {elapsed:?}, faster than the {lower_bound:?} floor"
    );
}

#[tokio::test]
async fn total_zero_is_a_no_op() {
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let request = get("http://192.0.2.1/unused".to_owned());
    let outcomes = run_load(&client, &request, &cfg(0, 4, 0), &ExecuteOptions::default()).await;
    assert!(outcomes.is_empty(), "total=0 fires nothing");
    let s = stats(&outcomes);
    assert_eq!(s.ok, 0);
    assert_eq!(s.min, None);
}
