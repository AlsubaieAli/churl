//! Load-runner handlers (open/start/progress/cancel/summary/key), extracted
//! from `app.rs`. Grandchild module of `app`; `impl App` here keeps
//! full access to `App`'s private fields and methods without visibility widening.

use super::super::*;

impl App {
    /// The open load runner's state, if the runner is active. The state lives in
    /// the [`Mode::LoadRunner`] variant, not a parallel field. While body-search
    /// is open OVER the runner (`Mode::BodySearch`), the runner mode is parked in
    /// `body_search_return`, so it is also consulted — the runner's response
    /// surface must stay reachable through a `/` search.
    pub(in crate::tui::app) fn load_runner(&self) -> Option<&LoadRunnerState> {
        match &self.mode {
            Mode::LoadRunner(runner) => Some(runner),
            Mode::BodySearch => match &self.body_search_return {
                Mode::LoadRunner(runner) => Some(runner),
                _ => None,
            },
            _ => None,
        }
    }

    /// Mutable accessor for the open load runner's state. Consults the
    /// same two locations as [`App::load_runner`] (the mode, or the parked mode
    /// while body-search is open over the runner).
    pub(in crate::tui::app) fn load_runner_mut(&mut self) -> Option<&mut LoadRunnerState> {
        Self::load_runner_in_mut(&mut self.mode, &mut self.body_search_return)
    }

    /// Field-level resolver for the runner behind the mut accessor. Split out as an
    /// associated fn taking the two field borrows explicitly so callers that also
    /// need a disjoint `&mut self` field (e.g. `orphan_response` in the `unwrap_or`)
    /// can borrow the runner without aliasing the whole `App`.
    pub(in crate::tui::app) fn load_runner_in_mut<'a>(
        mode: &'a mut Mode,
        parked: &'a mut Mode,
    ) -> Option<&'a mut LoadRunnerState> {
        match mode {
            Mode::LoadRunner(runner) => Some(runner),
            Mode::BodySearch => match parked {
                Mode::LoadRunner(runner) => Some(runner),
                _ => None,
            },
            _ => None,
        }
    }

    /// `<leader>l` / palette: open the load runner for the selected endpoint.
    /// Builds the request EXACTLY as an interactive send would (clone the endpoint
    /// request, fold in the live body editor, resolve `{{var}}`s ONCE) so the
    /// batch hits the same URL/vars/auth as a normal send, and prefills the config
    /// from the load defaults. Never auto-runs — the user reviews/edits first.
    pub(in crate::tui::app) fn open_load_runner(&mut self) {
        // Fall back to the hovered endpoint when nothing is loaded;
        // its on-disk request is used (no active buffer → `body_text` resolves empty).
        let Some(selected) = self.selected().cloned().or_else(|| self.hovered_endpoint()) else {
            self.notify("no endpoint selected — select one to load-test");
            return;
        };
        let body_text = self
            .active_endpoint_buffer()
            .map(|b| String::from(b.editor.lines.clone()))
            .unwrap_or_default();
        let mut request = selected.endpoint.request.clone();
        overwrite_body_text(&mut request, body_text);
        // Resolve `{{var}}` on the clone ONCE; every copy reuses this resolved
        // request (consistent batch, no N× re-resolution). `execute` stays
        // substitution-free.
        self.build_resolver(&selected)
            .substitute_request(&mut request);
        // Fail loud: the load runner resolves ONCE at open time and every copy
        // reuses this request. An unresolved `{{var}}` means the whole batch would
        // fire a literal placeholder — refuse to open the runner at all.
        let unresolved = churl_core::template::unresolved_placeholders(&request);
        if !unresolved.is_empty() {
            self.notify(unresolved_vars_message(&unresolved));
            return;
        }
        let url = request.url.clone();
        let endpoint_path = self.endpoint_rel_path(&selected);
        self.load_request = Some(request);
        // The default total/concurrency come from the debug-gated Advanced
        // overrides when set (M8.3 Wave 4), else `LoadConfig::default`'s
        // built-in gentle default — `Config::advanced_limits` already folds
        // that fallback in, so this is correct with or without an override.
        let cfg = LoadConfig {
            total: self.advanced_limits.total,
            concurrency: self.advanced_limits.concurrency,
            ..LoadConfig::default()
        };
        // One transition — construct the runner INTO the mode. No parallel
        // `load_runner` field, so `(Mode::LoadRunner, None)` is unrepresentable.
        self.mode = Mode::LoadRunner(LoadRunnerState::new(
            selected.endpoint.name.clone(),
            url,
            endpoint_path,
            cfg,
        ));
    }

    /// Starts (or restarts) the batch: aborts any prior launcher, resets rows,
    /// bumps the run generation, and spawns ONE launcher task that owns a
    /// `buffer_unordered` fan-out (bounded to `concurrency`, paced by `interval`).
    /// Aborting that single task drops the fan-out and cancels ALL in-flight
    /// requests — there is no detached per-request task to escape cancellation.
    pub(in crate::tui::app) fn start_load_run(&mut self) {
        // Interrupt any in-progress batch first — recording its partial summary
        // (a re-run mid-batch must not silently lose the current run).
        self.interrupt_running_batch();
        let request = self.load_request.clone();
        let (Some(runner), Some(request)) = (self.load_runner_mut(), request) else {
            return;
        };
        runner.reset_for_run();
        let run_generation = runner.run_generation;
        let cfg = runner.cfg;
        if cfg.total == 0 {
            runner.running = false;
            runner.finished = true;
            return;
        }
        // No client (snapshot tests): leave rows pending, runtime-free.
        let Some(client) = self.client.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let options = self.execute_options;
        let total = cfg.total;
        let concurrency = cfg.concurrency.max(1);
        let interval = cfg.interval;
        // A capture sink is built PER COPY only when `debug_enabled` — off
        // reaches `execute_traced(..., None)` with no `DebugTrace` ever
        // allocated, matching every other capture site's zero-overhead
        // discipline. This mirrors `churl_core::load::run_load`'s own
        // per-copy `capture.then(|| DebugTrace::from_request(&request))` +
        // `execute_traced` pattern — the live launcher stays hand-rolled
        // (buffer_unordered fan-out streaming `LoadStarted` progress per
        // copy, which `run_load`'s all-at-once return can't provide), but
        // reuses the exact same core primitive so a debug-on load run is
        // captured identically to `run_load`'s tested twin.
        let capture = self.debug_enabled;
        let handle = tokio::spawn(async move {
            use futures::stream::StreamExt;
            let start = Instant::now();
            futures::stream::iter(0..total)
                .map(|index| {
                    let client = client.clone();
                    let request = request.clone();
                    let tx = tx.clone();
                    async move {
                        // Absolute-target pacing (mirrors `run_load`): a hard floor
                        // on when copy `index` may launch.
                        if !interval.is_zero() {
                            let target =
                                interval.saturating_mul(u32::try_from(index).unwrap_or(u32::MAX));
                            let elapsed = start.elapsed();
                            if target > elapsed {
                                tokio::time::sleep(target - elapsed).await;
                            }
                        }
                        let _ = tx
                            .send(AppMsg::LoadStarted {
                                run_generation,
                                index,
                            })
                            .await;
                        let mut trace = capture.then(|| DebugTrace::from_request(&request));
                        let outcome = churl_core::http::execute_traced(
                            &client,
                            &request,
                            &options,
                            trace.as_mut(),
                        )
                        .await
                        .map_err(|err| err.to_string());
                        let _ = tx
                            .send(AppMsg::LoadResult {
                                run_generation,
                                index,
                                outcome,
                                trace: trace.map(Box::new),
                            })
                            .await;
                    }
                })
                .buffer_unordered(concurrency)
                .for_each(|()| async {})
                .await;
        });
        self.load_abort = Some(handle.abort_handle());
    }

    /// Marks copy `index` as in flight when the launcher signals it started.
    pub(in crate::tui::app) fn on_load_started(&mut self, run_generation: u64, index: usize) {
        let Some(runner) = self.load_runner_mut() else {
            return;
        };
        if run_generation != runner.run_generation {
            return; // stale
        }
        if let Some(row) = runner.results.get_mut(index)
            && matches!(row.status, LoadStatus::Pending)
        {
            row.status = LoadStatus::Running;
            row.response = ResponseState::InFlight {
                started: Instant::now(),
            };
        }
    }

    /// Lands a completed copy with no captured trace — thin wrapper over
    /// [`Self::on_load_result_traced`] kept so the many existing 3-arg call
    /// sites (tests driving the runner directly, without a real debug-traced
    /// send) stay unchanged.
    #[cfg(test)]
    pub(in crate::tui::app) fn on_load_result(
        &mut self,
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
    ) {
        self.on_load_result_traced(run_generation, index, outcome, None);
    }

    /// Lands a completed copy: drops stale results, classifies (mirroring the core
    /// `classify` seam), records it + recomputes stats, and — when the last copy
    /// lands — finishes the run and writes the batch summary. `trace` is `Some`
    /// only when debug capture was on for the run (see [`Self::start_load_run`]);
    /// it feeds the session Traffic feed and is otherwise a no-op.
    pub(in crate::tui::app) fn on_load_result_traced(
        &mut self,
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
        trace: Option<Box<DebugTrace>>,
    ) {
        let Some(runner) = self.load_runner_mut() else {
            return;
        };
        if run_generation != runner.run_generation {
            return; // stale — a cancel or re-run superseded this batch
        }
        let view_gen = runner.next_view_gen();
        let url = runner.url.clone();
        let endpoint_label = runner.endpoint_label.clone();
        // (borrow continues below; `done` is computed then the borrow is dropped
        //  before the `&mut self` follow-up so the runner state now living in
        //  `self.mode` doesn't alias the summary write.)
        let (status, timing, response, req_outcome) = match outcome {
            Ok(response) => {
                let (status, req_outcome) = if response.status >= 400 {
                    (
                        LoadStatus::Failed(response.status),
                        ReqOutcome::Failed {
                            status: response.status,
                        },
                    )
                } else {
                    (
                        LoadStatus::Ok(response.status),
                        ReqOutcome::Ok {
                            status: response.status,
                        },
                    )
                };
                let timing = Some(response.timing.total);
                let view = ResponseView::build(&response, view_gen);
                (status, timing, ResponseState::Done { view }, req_outcome)
            }
            Err(error) => (
                LoadStatus::Error(error.clone()),
                None,
                ResponseState::Failed {
                    error: error.clone(),
                    meta: load_result_meta(&url),
                },
                ReqOutcome::Error(error),
            ),
        };
        // Computed before `record_result` consumes `status` below.
        let traffic_outcome = match &status {
            LoadStatus::Ok(code) => TrafficOutcome::Ok(*code),
            LoadStatus::Failed(code) => TrafficOutcome::Failed(*code),
            _ => TrafficOutcome::Error,
        };
        let ms = timing.map(|d| d.as_millis() as u64);
        let done = runner.record_result(index, status, timing, response, req_outcome);
        if done {
            runner.running = false;
            runner.finished = true;
        }
        // Runner borrow (into `self.mode`) ends here; the batch-finish follow-up
        // touches other `self` fields, so it runs after the borrow is dropped.
        if let Some(trace) = trace {
            let label = format!("{endpoint_label} #{index}");
            tracing::debug!(
                target: "churl::load",
                method = %trace.resolved_display.method,
                url = %trace.resolved_display.url,
                ms,
                "{label}"
            );
            traffic::push(
                &mut self.traffic,
                TrafficEntry::new(label, traffic_outcome, ms, *trace),
            );
        }
        if done {
            self.load_abort = None;
            self.write_load_summary(false);
        }
    }

    /// Cancels the in-flight batch (Ctrl-C): records the partial summary + aborts
    /// the launcher + bumps the generation via the shared interrupt seam, then
    /// marks non-terminal rows cancelled and settles the runner's finished state.
    pub(in crate::tui::app) fn cancel_load_run(&mut self) {
        self.interrupt_running_batch();
        // Scope the runner borrow (into `self.mode`) so it ends before `self.notify`.
        {
            let Some(runner) = self.load_runner_mut() else {
                return;
            };
            for row in &mut runner.results {
                if matches!(row.status, LoadStatus::Pending | LoadStatus::Running) {
                    // A launched-then-cancelled row carries a real time-to-cancel.
                    // The launch `Instant` already lives in `InFlight { started }`
                    // (set by `on_load_started`); read it out before overwriting the
                    // response. Never-launched `Pending` rows have no `InFlight` and
                    // keep `timing = None` — honest: they never started.
                    if let ResponseState::InFlight { started } = row.response {
                        row.timing = Some(started.elapsed()); // Instant is Copy
                        row.response = ResponseState::Cancelled;
                    }
                    row.status = LoadStatus::Cancelled;
                }
            }
            runner.running = false;
            runner.finished = true;
            runner.cancelled = true;
            runner.confirming_close = false;
        }
        self.notify("load run cancelled");
    }

    /// Persists the run's one-row summary to the SEPARATE `load_batches` table
    /// (never per-endpoint history). Best-effort; a write failure warns.
    pub(in crate::tui::app) fn write_load_summary(&mut self, cancelled: bool) {
        let Some(runner) = self.load_runner() else {
            return;
        };
        let stats = &runner.stats;
        let ms = |d: Option<Duration>| d.map(|d| d.as_millis() as u64);
        let summary = LoadBatchSummary {
            executed_at_ms: now_ms(),
            url: runner.url.clone(),
            endpoint_path: runner.endpoint_path.clone(),
            total: runner.results.len(),
            concurrency: runner.cfg.concurrency,
            ok_count: stats.ok,
            fail_count: stats.failed,
            error_count: stats.errored,
            cancelled,
            min_ms: ms(stats.min),
            median_ms: ms(stats.median),
            p95_ms: ms(stats.p95),
            max_ms: ms(stats.max),
            mean_ms: ms(stats.mean),
        };
        // Feed the same consecutive-failure counter the per-request history
        // writes use, so a persistent SQLite failure surfaces the sticky flag
        // regardless of which write hit it (B3). A disabled store (`None`) is not
        // a failure and leaves the counter untouched.
        if let Some(result) = self
            .history
            .as_ref()
            .map(|store| store.insert_load_batch(&summary))
        {
            match result {
                Ok(_) => self.note_history_write(true),
                Err(err) => {
                    self.note_history_write(false);
                    self.notify(format!("load history write failed: {err}"));
                }
            }
        }
    }

    /// Routes a key to the open load runner and acts on its outcome.
    ///
    /// The runner state lives in `self.mode`; the router only dispatches this on
    /// `Mode::LoadRunner(_)`, so no `is_none()→Normal` guard or the
    /// `.expect("checked above")` it would protect is needed (unreachable by
    /// construction). The key is handled inside the `&mut self.mode` borrow, which
    /// is dropped (via the owned `outcome`) before any `&mut self` method runs.
    pub(in crate::tui::app) fn handle_load_runner_key(&mut self, key: KeyEvent) -> Result<()> {
        // Narrow leader-from-runner allowlist (M8.3 Wave 4): the leader key
        // is otherwise only reachable from `Mode::Normal` (see
        // `Self::handle_normal_key`), so the debug overlays could not be
        // opened from inside a runner even though the spec requires
        // park-and-return for them. Intercepted BEFORE the response-key/
        // runner routing below so it can't be shadowed by either. See
        // `Self::runner_leader_pending`'s doc for why this is a narrow
        // one-shot flag rather than engaging the general `self.leader` state
        // machine (which would broaden every leader-root action's
        // reachability, not just the two debug overlays).
        if self.keymap.is_leader(key) {
            self.runner_leader_pending = true;
            return Ok(());
        }
        // Response-region keys route through the shared response path FIRST (note
        // #2); anything not a response action delegates to the runner below.
        if self.try_route_runner_response_key(key) {
            return Ok(());
        }
        let Some(runner) = self.load_runner_mut() else {
            return Ok(());
        };
        let outcome = runner.handle_key(key);
        match outcome {
            LoadOutcome::Consumed => {}
            LoadOutcome::Run => self.request_load_run(),
            LoadOutcome::ConfirmedRun => self.start_load_run(),
            LoadOutcome::Cancel => self.cancel_load_run(),
            LoadOutcome::Close => self.close_load_runner(),
        }
        Ok(())
    }
}
