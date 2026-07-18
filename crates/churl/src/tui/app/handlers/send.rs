//! Send/response/history handlers — request dispatch, cancellation, the
//! channel message pump, response landing, highlight caching, and history
//! writes — extracted from `app.rs`. Grandchild module of `app`;
//! `impl App` here keeps full access to `App`'s private fields and methods
//! without any visibility widening.

use super::super::*;

impl App {
    /// Sends the selected endpoint's request with the live edtui body text.
    /// Spawns the execution task, keeps its `AbortHandle`, and moves the response
    /// pane to the in-flight state. Ignored (with a statusline hint) when a
    /// request is already in flight or no endpoint is selected.
    pub(in crate::tui::app) fn send_request(&mut self) {
        if self
            .active_endpoint_buffer()
            .is_some_and(|b| b.in_flight.is_some())
        {
            self.message = Some(Message::new("request already in flight — ctrl-c to cancel"));
            return;
        }
        // Pick the endpoint to send: the open buffer, else the endpoint the
        // explorer cursor hovers. When falling back to a hovered endpoint, OPEN
        // it into a buffer first — the same gesture Enter uses
        // (`open_or_focus_buffer`) — so it becomes the active buffer and the
        // in-flight/response state set at the tail lands on a real buffer that
        // `on_response` routes the reply back to for display. The common case
        // (an endpoint already open) skips this block untouched.
        if self.selected().is_none() {
            match self.hovered_endpoint() {
                Some(hovered) => self.open_or_focus_buffer(hovered),
                None => {
                    self.message = Some(Message::new("no endpoint selected — nothing to send"));
                    return;
                }
            }
        }
        // Clone `SelectedEndpoint` so `build_resolver`/`endpoint_rel_path` can
        // borrow `&self` while we later hold `&mut` the active buffer.
        let Some(selected) = self.selected().cloned() else {
            self.message = Some(Message::new("no endpoint selected — nothing to send"));
            return;
        };
        // Read the live edtui body text before borrowing the buffer mutably.
        let body_text = self
            .active_endpoint_buffer()
            .map(|b| String::from(b.editor.lines.clone()))
            .unwrap_or_default();
        let mut request = selected.endpoint.request.clone();
        overwrite_body_text(&mut request, body_text);

        // A capture sink is built only when `debug_enabled` asked for one —
        // debug off takes the exact pre-M8.3 path (no `DebugTrace` ever
        // constructed), matching the zero-overhead discipline
        // `churl_core::http::execute_traced` documents. Built BEFORE
        // substitution and carried across the spawned send below (even a
        // failed exchange), unlike headless's `run_execution`: its `?`
        // early-return on a failed send drops the local trace before it can
        // be attached; here the trace is owned by the async task and reaches
        // `on_response` via `AppMsg::Response` regardless of outcome.
        let mut trace = self
            .debug_enabled
            .then(|| Box::new(DebugTrace::from_request(&request)));

        // Resolve `{{var}}` placeholders on the cloned request only — resolved
        // values are never written to disk (this clone is discarded after the
        // send). `execute()`/`execute_traced()` stay substitution-free.
        let resolver = self.build_resolver(&selected);
        match trace.as_mut() {
            Some(trace) => resolver.substitute_request_traced(&mut request, &mut trace.var_steps),
            None => resolver.substitute_request(&mut request),
        }

        // Fail loud: refuse to send a request that still carries `{{var}}`
        // placeholders no scope resolved — a literal `{{...}}` in the URL/headers/
        // body would otherwise ship and produce a cryptic transport error. Checked
        // before the client gate so the message surfaces regardless of runtime.
        let unresolved = churl_core::template::unresolved_placeholders(&request);
        if !unresolved.is_empty() {
            self.message = Some(Message::new(unresolved_vars_message(&unresolved)));
            return;
        }

        // Pick the client honoring this endpoint's effective insecure flag
        // (`request.insecure || session_insecure`). No client = runtime-free
        // construction (snapshot tests); do nothing.
        let Some(client) = self.client_for(&request) else {
            return;
        };

        self.generation += 1;
        let generation = self.generation;
        let started = Instant::now();
        let meta = ResponseMeta {
            method: request.method.to_string(),
            url: request.url.clone(),
            endpoint_path: self.endpoint_rel_path(&selected),
            executed_at_ms: now_ms(),
        };

        let tx = self.tx.clone();
        let task_meta = meta.clone();
        let options = self.execute_options;
        // `execute_traced` has no access to the `ClientConfig` used to build
        // `client` (see `AuthCookieProxyDecisions`'s docs) — the session
        // fields are the one place both are in scope, so read them here
        // (masking the proxy immediately) and fill the trace's decisions
        // after the send, mirroring headless's `run_execution`.
        let cookie_used = self.cookies_enabled;
        let proxy_masked = self
            .session_proxy
            .as_deref()
            .map(churl_core::secrets::mask_url);
        let handle = tokio::spawn(async move {
            let mut trace = trace;
            let result =
                churl_core::http::execute_traced(&client, &request, &options, trace.as_deref_mut())
                    .await;
            // Filled regardless of success/failure — unlike headless, this
            // task holds the trace across the whole call (see the doc comment
            // above `trace`'s construction), so a failed exchange still ends
            // up with cookie/proxy decisions recorded alongside the error
            // `execute_traced` already captured internally.
            if let Some(trace) = trace.as_mut() {
                trace.decisions.cookie_used = cookie_used;
                trace.decisions.proxy = proxy_masked;
            }
            let outcome = result.map_err(|err| err.to_string());
            // Bounded channel: this is a spawned async task, so awaiting
            // on a full queue applies backpressure without stalling the UI thread.
            let _ = tx
                .send(AppMsg::Response {
                    generation,
                    outcome,
                    meta: task_meta,
                    trace,
                })
                .await;
        });

        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.in_flight = Some(InFlightRequest {
                handle: handle.abort_handle(),
                generation,
                meta,
            });
            b.response = ResponseState::InFlight { started };
            b.geometry.scroll = 0;
            b.geometry.cursor = 0;
            b.pending_highlight = None;
            b.highlight_cache.clear();
        }
        self.message = None;
    }

    /// Cancels the active buffer's in-flight request: aborts the task, records a
    /// history row with no status, and moves the pane to the cancelled state.
    pub(in crate::tui::app) fn cancel_request(&mut self) {
        let Some(in_flight) = self
            .active_endpoint_buffer_mut()
            .and_then(|b| b.in_flight.take())
        else {
            self.message = Some(Message::new("no request in flight"));
            return;
        };
        in_flight.handle.abort();
        self.write_history(&in_flight.meta, None, None);
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.response = ResponseState::Cancelled;
        }
        self.message = Some(Message::new("request cancelled"));
    }

    /// Dispatches a channel message.
    pub(in crate::tui::app) fn handle_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::Redraw => {}
            AppMsg::Response {
                generation,
                outcome,
                meta,
                trace,
            } => self.on_response(generation, outcome, meta, trace),
            AppMsg::Highlighted { hash, lines } => self.cache_highlighted(hash, lines),
            AppMsg::SequenceStep {
                run_generation,
                index,
                outcome,
                trace,
            } => self.on_sequence_step_traced(run_generation, index, outcome, trace),
            AppMsg::LoadStarted {
                run_generation,
                index,
            } => self.on_load_started(run_generation, index),
            AppMsg::LoadResult {
                run_generation,
                index,
                outcome,
                trace,
            } => self.on_load_result_traced(run_generation, index, outcome, trace),
        }
    }

    /// Applies an arrived response, dropping it if its generation is stale (the
    /// request was cancelled or superseded by a newer send).
    ///
    /// Routing: `generation` is a single global counter, so each in-flight
    /// request's generation is unique across buffers. We SCAN the buffers for the
    /// one whose `in_flight.generation` matches and write the result to THAT
    /// buffer — even if it is not the active one. (Stage 1 has ≤1 buffer, so this
    /// is trivially the active buffer; the scan is written now so Stage 2's
    /// multi-buffer routing is already correct.)
    pub(in crate::tui::app) fn on_response(
        &mut self,
        generation: u64,
        outcome: Result<Response, String>,
        meta: ResponseMeta,
        trace: Option<Box<DebugTrace>>,
    ) {
        let Some(idx) = self.buffers.iter().position(|b| {
            b.as_endpoint()
                .and_then(|e| e.in_flight.as_ref())
                .is_some_and(|f| f.generation == generation)
        }) else {
            return; // stale — no buffer awaits this generation
        };
        // The Inspector's default view: this exchange's trace (or `None` when
        // debug was off for it) — a stale/cancelled exchange never reaches
        // this line (the early return above), so `latest_trace` only ever
        // reflects an exchange that actually landed.
        self.latest_trace = trace;
        // History write needs `&mut self`; compute status args before borrowing
        // the target buffer to store the response.
        let status = outcome.as_ref().ok().map(|r| (r.status, r.timing.total));
        // Feed the session Traffic feed + the Log ring — both are debug-gated
        // via the trace itself being `Some` only when `debug_enabled` was on
        // at send time (see `Self::send_request`), so this costs nothing on
        // the off path: no clone, no push, no tracing event.
        if let Some(trace) = self.latest_trace.as_deref() {
            let label = meta
                .endpoint_path
                .clone()
                .unwrap_or_else(|| "(standalone send)".to_owned());
            let ms = status.map(|(_, total)| total.as_millis() as u64);
            let traffic_outcome = match &status {
                Some((code, _)) if *code >= 400 => TrafficOutcome::Failed(*code),
                Some((code, _)) => TrafficOutcome::Ok(*code),
                None => TrafficOutcome::Error,
            };
            tracing::debug!(
                target: "churl::send",
                method = %trace.resolved_display.method,
                url = %trace.resolved_display.url,
                status = status.map(|(code, _)| code),
                ms,
                "{label}"
            );
            traffic::push(
                &mut self.traffic,
                TrafficEntry::new(label, traffic_outcome, ms, trace.clone()),
            );
        }
        match &outcome {
            Ok(_) => {
                let (st, total) = status.expect("Ok outcome has status");
                self.write_history(&meta, Some(st), Some(total));
            }
            Err(_) => self.write_history(&meta, None, None),
        }
        let Some(b) = self.buffers[idx].as_endpoint_mut() else {
            return;
        };
        b.in_flight = None;
        b.highlight_cache.clear();
        b.geometry.scroll = 0;
        b.geometry.cursor = 0;
        b.pending_highlight = None;
        match outcome {
            Ok(response) => {
                b.response = ResponseState::Done {
                    view: ResponseView::build(&response, generation),
                };
            }
            Err(error) => {
                b.response = ResponseState::Failed { error, meta };
            }
        }
        // A send may have stored `Set-Cookie`s in the jar; persist it so cookies
        // survive a restart. Best-effort and cheap (a tiny blob, one row).
        if self.cookies_enabled {
            self.persist_cookie_jar();
        }
    }

    /// Stores highlighted viewport lines in the active buffer's cache, capping it
    /// so long scrolls do not grow it unbounded. (Only the active buffer enqueues
    /// highlight jobs, and the cache is keyed by viewport hash, so a job that
    /// lands after a buffer switch is harmless.)
    pub(in crate::tui::app) fn cache_highlighted(&mut self, hash: u64, lines: Vec<Line<'static>>) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        if b.highlight_cache.len() >= 64 {
            b.highlight_cache.clear();
        }
        // Clear the in-flight guard when its result lands.
        if b.pending_highlight == Some(hash) {
            b.pending_highlight = None;
        }
        b.highlight_cache.insert(hash, lines);
    }

    /// Inserts a history row for a terminal outcome. Insert failure warns on the
    /// statusline but never crashes.
    pub(in crate::tui::app) fn write_history(
        &mut self,
        meta: &ResponseMeta,
        status: Option<u16>,
        duration: Option<Duration>,
    ) {
        let entry = NewHistoryEntry {
            executed_at_ms: meta.executed_at_ms,
            method: meta.method.clone(),
            url: meta.url.clone(),
            status,
            duration_ms: duration.map(|d| d.as_millis() as u64),
            endpoint_path: meta.endpoint_path.clone(),
        };
        // No store (history disabled) is not a write *failure* — leave the
        // failure counter untouched. Only a real insert result flips it (B3).
        if let Some(result) = self.history.as_ref().map(|store| store.insert(&entry)) {
            match result {
                Ok(_) => self.note_history_write(true),
                Err(err) => {
                    self.note_history_write(false);
                    self.message = Some(Message::new(format!("history write failed: {err}")));
                }
            }
        }
    }
}
