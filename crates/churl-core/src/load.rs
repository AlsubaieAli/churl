//! Concurrent load / throttle runner.
//!
//! Fires **N copies** of one *already-resolved* [`Request`] through the single
//! [`execute`] HTTP chokepoint with **bounded concurrency** and optional launch
//! **pacing**, capturing each request's outcome + timing so honest latency stats
//! can be computed. This module is UI-free: [`run_load`] is the wiremock-tested
//! source of truth for run semantics, and the live TUI launcher mirrors it (a
//! single [`futures`]-driven fan-out, no detached per-request task) so the two
//! can never drift.
//!
//! Concurrency is bounded by [`futures::stream::StreamExt::buffer_unordered`],
//! which polls at most `concurrency` request futures at once — the same
//! guarantee a semaphore gives, with no manual permit bookkeeping (recorded in
//! DECISIONS.md). Pacing is an **absolute-target** delay: request `i` never
//! starts before `start + i·interval`, giving a guaranteed lower bound on the
//! run's wall-clock regardless of how the scheduler interleaves the futures.

use std::time::{Duration, Instant};

use futures::stream::StreamExt;

use crate::http::{ExecuteOptions, HttpError, execute};
use crate::model::{Request, Response};

/// A load run's knobs: how many copies to fire, how many may be in flight at
/// once, and the pacing between successive launches (`Duration::ZERO` = burst,
/// i.e. launch as fast as concurrency permits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadConfig {
    /// Total number of request copies to fire.
    pub total: usize,
    /// Maximum number of requests in flight simultaneously (the concurrency bound).
    pub concurrency: usize,
    /// Delay between successive launches; `ZERO` bursts as fast as permits free.
    pub interval: Duration,
}

impl Default for LoadConfig {
    /// A gentle default: 10 requests, 5 at a time, burst.
    fn default() -> Self {
        Self {
            total: 10,
            concurrency: 5,
            interval: Duration::ZERO,
        }
    }
}

/// Guardrail caps for a load run: warn thresholds (loud confirm above them) and
/// hard maximums (refuse above them). Overridable via the `[load]` config table
/// ([`crate::config::Config::load_caps`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadCaps {
    /// Above this total, running requires an explicit confirmation.
    pub warn_total: usize,
    /// Above this concurrency, running requires an explicit confirmation.
    pub warn_concurrency: usize,
    /// Hard cap on total: a larger request is refused outright.
    pub max_total: usize,
    /// Hard cap on concurrency: a larger request is refused outright.
    pub max_concurrency: usize,
}

impl Default for LoadCaps {
    fn default() -> Self {
        Self {
            warn_total: 100,
            warn_concurrency: 20,
            max_total: 10_000,
            max_concurrency: 200,
        }
    }
}

/// The classification of a [`LoadConfig`] against [`LoadCaps`]: safe to run,
/// needs a loud confirm, or refused outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadCheck {
    /// Within all warn thresholds — run immediately.
    Ok,
    /// Above a warn threshold — run only after an explicit confirmation. Carries
    /// a human-readable reason.
    Warn(String),
    /// Above a hard cap — refused. Carries a human-readable reason.
    Refuse(String),
}

/// Classifies `cfg` against `caps` (pure). A hard-cap breach (refuse) takes
/// precedence over a warn-threshold breach; total is checked before concurrency
/// so the message names the first offending knob. `total == 0` is not a breach
/// (it is a harmless no-op the caller can choose to reject separately).
pub fn check_config(cfg: &LoadConfig, caps: &LoadCaps) -> LoadCheck {
    if cfg.total > caps.max_total {
        return LoadCheck::Refuse(format!(
            "total {} exceeds the hard cap of {} (raise it in [load] max_total if you mean it)",
            cfg.total, caps.max_total
        ));
    }
    if cfg.concurrency > caps.max_concurrency {
        return LoadCheck::Refuse(format!(
            "concurrency {} exceeds the hard cap of {} (raise it in [load] max_concurrency if you mean it)",
            cfg.concurrency, caps.max_concurrency
        ));
    }
    if cfg.total > caps.warn_total {
        return LoadCheck::Warn(format!(
            "{} requests is above the warn threshold of {}",
            cfg.total, caps.warn_total
        ));
    }
    if cfg.concurrency > caps.warn_concurrency {
        return LoadCheck::Warn(format!(
            "concurrency {} is above the warn threshold of {}",
            cfg.concurrency, caps.warn_concurrency
        ));
    }
    LoadCheck::Ok
}

/// The classified result of one request in a load run. Only `Ok`/`Failed` carry
/// a timing (the request completed); `Error` never does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReqOutcome {
    /// The request completed with a success status (< 400).
    Ok {
        /// The HTTP status code.
        status: u16,
    },
    /// The request completed with an HTTP error status (>= 400).
    Failed {
        /// The HTTP status code.
        status: u16,
    },
    /// The request could not be sent (transport / TLS / timeout error).
    Error(String),
}

/// Classifies one execution result: a `>= 400` status is `Failed`, a transport
/// error is `Error`, anything else is `Ok`. The single classify seam shared by
/// [`run_load`] and the live TUI launcher.
pub fn classify(result: &Result<Response, HttpError>) -> ReqOutcome {
    match result {
        Ok(response) if response.status >= 400 => ReqOutcome::Failed {
            status: response.status,
        },
        Ok(response) => ReqOutcome::Ok {
            status: response.status,
        },
        Err(err) => ReqOutcome::Error(err.to_string()),
    }
}

/// Aggregate statistics over a load run's outcomes. Counts split every request
/// into ok / failed / errored; the latency fields are computed only over the
/// requests that actually **completed** (have a timing), so an all-errored batch
/// yields `None` for every percentile without panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct LoadStats {
    /// Number of `Ok` outcomes.
    pub ok: usize,
    /// Number of `Failed` (>= 400) outcomes.
    pub failed: usize,
    /// Number of `Error` (transport failure) outcomes.
    pub errored: usize,
    /// Minimum completed-request latency, if any completed.
    pub min: Option<Duration>,
    /// Median (nearest-rank p50) completed-request latency.
    pub median: Option<Duration>,
    /// 95th-percentile (nearest-rank) completed-request latency.
    pub p95: Option<Duration>,
    /// Maximum completed-request latency.
    pub max: Option<Duration>,
    /// Arithmetic mean of completed-request latencies.
    pub mean: Option<Duration>,
}

/// Computes [`LoadStats`] over `outcomes` (pure). Percentiles use the
/// **nearest-rank** method on a sorted copy of the completed-request timings;
/// `median` is the nearest-rank p50. Empty / all-errored input yields zero
/// counts and `None` latencies — never a panic or a divide-by-zero.
pub fn stats(outcomes: &[(ReqOutcome, Option<Duration>)]) -> LoadStats {
    let mut ok = 0;
    let mut failed = 0;
    let mut errored = 0;
    let mut timings: Vec<Duration> = Vec::with_capacity(outcomes.len());
    for (outcome, timing) in outcomes {
        match outcome {
            ReqOutcome::Ok { .. } => ok += 1,
            ReqOutcome::Failed { .. } => failed += 1,
            ReqOutcome::Error(_) => errored += 1,
        }
        if let Some(timing) = timing {
            timings.push(*timing);
        }
    }
    timings.sort_unstable();

    let mean = if timings.is_empty() {
        None
    } else {
        // Sum in nanoseconds to keep precision; u128 cannot overflow here (cap
        // is 10_000 requests × sub-day latencies).
        let total: u128 = timings.iter().map(Duration::as_nanos).sum();
        Some(Duration::from_nanos(
            u64::try_from(total / timings.len() as u128).unwrap_or(u64::MAX),
        ))
    };

    LoadStats {
        ok,
        failed,
        errored,
        min: timings.first().copied(),
        median: percentile(&timings, 50.0),
        p95: percentile(&timings, 95.0),
        max: timings.last().copied(),
        mean,
    }
}

/// The nearest-rank percentile of a **sorted** slice: rank = ceil(p/100 · n),
/// taking the `rank`-th element (1-based, clamped into range). Returns `None`
/// for an empty slice.
fn percentile(sorted: &[Duration], p: f64) -> Option<Duration> {
    if sorted.is_empty() {
        return None;
    }
    // `sorted.len()` is a real slice length, so it round-trips through f64 (well
    // under 2^53) and the ceiled rank (0..=len) truncates back into usize exactly.
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    Some(sorted[index])
}

/// Fires `cfg.total` copies of `request` through [`execute`], bounded to
/// `cfg.concurrency` in flight at once and paced by `cfg.interval` between
/// launches. Returns one `(outcome, timing)` per request, **in request-index
/// order** (0..total), so the caller can line results up with copies.
///
/// This is the run-semantics source of truth: the same `buffer_unordered` +
/// absolute-target pacing the TUI launcher uses for its live stream. The request
/// is cloned per copy (it is already resolved — no re-resolution) and driven
/// through the single `execute` chokepoint. `cfg.total == 0` returns an empty
/// vec without touching the network.
pub async fn run_load(
    client: &reqwest::Client,
    request: &Request,
    cfg: &LoadConfig,
    options: &ExecuteOptions,
) -> Vec<(ReqOutcome, Option<Duration>)> {
    if cfg.total == 0 {
        return Vec::new();
    }
    let concurrency = cfg.concurrency.max(1);
    let interval = cfg.interval;
    let start = Instant::now();

    let mut indexed: Vec<(usize, ReqOutcome, Option<Duration>)> =
        futures::stream::iter(0..cfg.total)
            .map(|i| {
                let client = client.clone();
                let request = request.clone();
                let options = *options;
                async move {
                    // Absolute-target pacing: never start copy `i` before
                    // `start + i·interval`. A hard floor on when the launch happens,
                    // so the run's wall-clock has a guaranteed lower bound.
                    if !interval.is_zero() {
                        let target = interval.saturating_mul(u32::try_from(i).unwrap_or(u32::MAX));
                        let elapsed = start.elapsed();
                        if target > elapsed {
                            tokio::time::sleep(target - elapsed).await;
                        }
                    }
                    let result = execute(&client, &request, &options).await;
                    let timing = result.as_ref().ok().map(|response| response.timing.total);
                    (i, classify(&result), timing)
                }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;

    indexed.sort_by_key(|(index, ..)| *index);
    indexed
        .into_iter()
        .map(|(_, outcome, timing)| (outcome, timing))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn ok(timing: u64) -> (ReqOutcome, Option<Duration>) {
        (ReqOutcome::Ok { status: 200 }, Some(ms(timing)))
    }

    fn failed(timing: u64) -> (ReqOutcome, Option<Duration>) {
        (ReqOutcome::Failed { status: 500 }, Some(ms(timing)))
    }

    fn errored() -> (ReqOutcome, Option<Duration>) {
        (ReqOutcome::Error("boom".to_owned()), None)
    }

    #[test]
    fn classify_maps_status_bands() {
        let response = |status: u16| Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            truncated: false,
            timing: crate::model::Timing {
                connect: None,
                total: ms(1),
            },
        };
        assert_eq!(classify(&Ok(response(200))), ReqOutcome::Ok { status: 200 });
        assert_eq!(classify(&Ok(response(204))), ReqOutcome::Ok { status: 204 });
        assert_eq!(
            classify(&Ok(response(404))),
            ReqOutcome::Failed { status: 404 }
        );
        assert_eq!(
            classify(&Ok(response(503))),
            ReqOutcome::Failed { status: 503 }
        );
        assert!(matches!(
            classify(&Err(HttpError::Timeout)),
            ReqOutcome::Error(_)
        ));
    }

    #[test]
    fn stats_odd_vector_exact() {
        // Sorted latencies 10,20,30,40,50 (n=5).
        let outcomes = vec![ok(30), ok(10), ok(50), ok(20), ok(40)];
        let s = stats(&outcomes);
        assert_eq!(s.ok, 5);
        assert_eq!(s.failed, 0);
        assert_eq!(s.errored, 0);
        assert_eq!(s.min, Some(ms(10)));
        assert_eq!(s.max, Some(ms(50)));
        assert_eq!(s.mean, Some(ms(30))); // (10+20+30+40+50)/5
        // nearest-rank p50: ceil(0.5*5)=3 → 3rd element = 30.
        assert_eq!(s.median, Some(ms(30)));
        // nearest-rank p95: ceil(0.95*5)=5 → 5th element = 50.
        assert_eq!(s.p95, Some(ms(50)));
    }

    #[test]
    fn stats_even_vector_exact() {
        // Sorted latencies 10,20,30,40 (n=4).
        let outcomes = vec![ok(10), ok(20), ok(30), ok(40)];
        let s = stats(&outcomes);
        assert_eq!(s.min, Some(ms(10)));
        assert_eq!(s.max, Some(ms(40)));
        assert_eq!(s.mean, Some(ms(25)));
        // nearest-rank p50: ceil(0.5*4)=2 → 2nd element = 20.
        assert_eq!(s.median, Some(ms(20)));
        // nearest-rank p95: ceil(0.95*4)=ceil(3.8)=4 → 4th element = 40.
        assert_eq!(s.p95, Some(ms(40)));
    }

    #[test]
    fn stats_counts_mixed_outcomes_and_only_times_completed() {
        // 2 ok, 1 failed, 2 errored. Timings come from ok+failed only.
        let outcomes = vec![ok(10), failed(30), errored(), ok(20), errored()];
        let s = stats(&outcomes);
        assert_eq!(s.ok, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.errored, 2);
        // Completed timings sorted: 10,20,30 → min10, max30, mean20, p50 20, p95 30.
        assert_eq!(s.min, Some(ms(10)));
        assert_eq!(s.max, Some(ms(30)));
        assert_eq!(s.mean, Some(ms(20)));
        assert_eq!(s.median, Some(ms(20)));
        assert_eq!(s.p95, Some(ms(30)));
    }

    #[test]
    fn stats_single_completed() {
        let s = stats(&[ok(42)]);
        assert_eq!(s.min, Some(ms(42)));
        assert_eq!(s.median, Some(ms(42)));
        assert_eq!(s.p95, Some(ms(42)));
        assert_eq!(s.max, Some(ms(42)));
        assert_eq!(s.mean, Some(ms(42)));
    }

    #[test]
    fn stats_empty_is_all_none() {
        let s = stats(&[]);
        assert_eq!(s, LoadStats::default());
        assert_eq!(s.min, None);
        assert_eq!(s.median, None);
        assert_eq!(s.p95, None);
        assert_eq!(s.max, None);
        assert_eq!(s.mean, None);
    }

    #[test]
    fn stats_all_errored_counts_but_no_latency() {
        let s = stats(&[errored(), errored(), errored()]);
        assert_eq!(s.errored, 3);
        assert_eq!(s.ok, 0);
        assert_eq!(s.min, None);
        assert_eq!(s.median, None);
        assert_eq!(s.p95, None);
        assert_eq!(s.max, None);
        assert_eq!(s.mean, None);
    }

    #[test]
    fn check_config_ok_within_thresholds() {
        let caps = LoadCaps::default();
        let cfg = LoadConfig {
            total: 10,
            concurrency: 5,
            interval: Duration::ZERO,
        };
        assert_eq!(check_config(&cfg, &caps), LoadCheck::Ok);
    }

    #[test]
    fn check_config_warns_above_warn_thresholds() {
        let caps = LoadCaps::default();
        // Above warn_total (100), below hard cap.
        let cfg = LoadConfig {
            total: 500,
            concurrency: 5,
            interval: Duration::ZERO,
        };
        assert!(matches!(check_config(&cfg, &caps), LoadCheck::Warn(_)));
        // Above warn_concurrency (20), below hard cap.
        let cfg = LoadConfig {
            total: 10,
            concurrency: 50,
            interval: Duration::ZERO,
        };
        assert!(matches!(check_config(&cfg, &caps), LoadCheck::Warn(_)));
    }

    #[test]
    fn check_config_refuses_above_hard_caps() {
        let caps = LoadCaps::default();
        let cfg = LoadConfig {
            total: 20_000,
            concurrency: 5,
            interval: Duration::ZERO,
        };
        assert!(matches!(check_config(&cfg, &caps), LoadCheck::Refuse(_)));
        let cfg = LoadConfig {
            total: 10,
            concurrency: 500,
            interval: Duration::ZERO,
        };
        assert!(matches!(check_config(&cfg, &caps), LoadCheck::Refuse(_)));
    }

    #[test]
    fn check_config_refuse_beats_warn() {
        let caps = LoadCaps::default();
        // Both total and concurrency over their warn thresholds, and total over
        // the hard cap → Refuse wins.
        let cfg = LoadConfig {
            total: 50_000,
            concurrency: 50,
            interval: Duration::ZERO,
        };
        assert!(matches!(check_config(&cfg, &caps), LoadCheck::Refuse(_)));
    }

    #[test]
    fn check_config_zero_total_is_ok() {
        let caps = LoadCaps::default();
        let cfg = LoadConfig {
            total: 0,
            concurrency: 1,
            interval: Duration::ZERO,
        };
        assert_eq!(check_config(&cfg, &caps), LoadCheck::Ok);
    }
}
