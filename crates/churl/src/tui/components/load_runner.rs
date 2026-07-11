//! Concurrent-load runner overlay (`Mode::LoadRunner`): a large modal that
//! fires N copies of the selected endpoint concurrently and shows a live results
//! list, a latency stats line, and any individual response in the real response
//! viewer, atop an editable config header (total / concurrency / min gap).
//!
//! Like the sequence runner, this component is UI-only: it holds display state
//! and the transient [`LoadConfig`], while `App` owns the HTTP client, the single
//! launcher task (a `buffer_unordered` fan-out) + its abort handle, the
//! generation guard, and the guardrail caps. `App` drives the run through the
//! same `churl_core::load` primitives the wiremock-tested `run_load` uses, so the
//! live launcher and the tested twin can never drift.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use churl_core::load::{LoadConfig, LoadStats};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::line_editor::LineEditor;
use super::prompt;
use super::response::{self, RenderCtx, ResponseGeometry, ResponseState};
use crate::tui::highlight::HighlightJob;
use crate::tui::theme::Theme;

/// How many completed rows keep a full [`ResponseState::Done`] view in memory
/// (memory bound). Retention is O(concurrency + K): the last `K` OK completions
/// plus the currently-selected row are kept live; older OK rows are downgraded to
/// [`ResponseState::Dropped`] (status/timing/size only, no body). A high-`total`
/// load run therefore holds bounded memory instead of `total × body`.
const LIVE_VIEW_WINDOW: usize = 16;

/// Per-request display status in the results list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadStatus {
    /// Not yet launched.
    Pending,
    /// In flight.
    Running,
    /// Completed with a success status (< 400).
    Ok(u16),
    /// Completed with an HTTP error status (>= 400).
    Failed(u16),
    /// Could not be sent (transport error).
    Error(String),
    /// The batch was cancelled before this copy ran/finished.
    Cancelled,
}

impl LoadStatus {
    fn glyph(&self) -> &'static str {
        match self {
            LoadStatus::Pending => "·",
            LoadStatus::Running => "◐",
            LoadStatus::Ok(_) => "✓",
            LoadStatus::Failed(_) => "✗",
            LoadStatus::Error(_) => "⚠",
            LoadStatus::Cancelled => "–",
        }
    }

    /// A short trailing detail for the row.
    fn detail(&self) -> Option<String> {
        match self {
            LoadStatus::Ok(status) | LoadStatus::Failed(status) => Some(status.to_string()),
            LoadStatus::Error(_) => Some("error".to_owned()),
            LoadStatus::Cancelled => Some("cancelled".to_owned()),
            LoadStatus::Pending | LoadStatus::Running => None,
        }
    }
}

/// One request's row in the results list.
#[derive(Debug)]
pub struct ResultRow {
    /// The copy's index in launch order.
    pub index: usize,
    /// Display status.
    pub status: LoadStatus,
    /// Request timing once completed (`None` for pending / errored).
    pub timing: Option<Duration>,
    /// The response state for the viewer.
    pub response: ResponseState,
}

impl ResultRow {
    fn pending(index: usize) -> Self {
        Self {
            index,
            status: LoadStatus::Pending,
            timing: None,
            response: ResponseState::Idle,
        }
    }
}

/// Which config field the header edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadField {
    /// Total number of copies.
    Total,
    /// Concurrency bound.
    Concurrency,
    /// Launch interval in milliseconds.
    Interval,
}

impl LoadField {
    /// The next field (Total → Concurrency → Interval → Total).
    fn next(self) -> Self {
        match self {
            LoadField::Total => LoadField::Concurrency,
            LoadField::Concurrency => LoadField::Interval,
            LoadField::Interval => LoadField::Total,
        }
    }

    /// The previous field.
    fn prev(self) -> Self {
        match self {
            LoadField::Total => LoadField::Interval,
            LoadField::Concurrency => LoadField::Total,
            LoadField::Interval => LoadField::Concurrency,
        }
    }

    /// This field's display label. `Interval` reads "min gap" — its semantics
    /// are the minimum gap between request launches (a rate floor), which
    /// "interval" obscures. The enum variant + core field name stay `Interval`.
    fn label(self) -> &'static str {
        match self {
            LoadField::Total => "total",
            LoadField::Concurrency => "concurrency",
            LoadField::Interval => "min gap",
        }
    }
}

/// Which sub-pane of the runner has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerFocus {
    /// The editable config header.
    ConfigHeader,
    /// The results list.
    Results,
    /// The response viewer.
    Response,
}

impl RunnerFocus {
    /// The next pane (ConfigHeader → Results → Response → ConfigHeader).
    fn next(self) -> Self {
        match self {
            RunnerFocus::ConfigHeader => RunnerFocus::Results,
            RunnerFocus::Results => RunnerFocus::Response,
            RunnerFocus::Response => RunnerFocus::ConfigHeader,
        }
    }

    /// The previous pane (ConfigHeader → Response → Results → ConfigHeader).
    fn prev(self) -> Self {
        match self {
            RunnerFocus::ConfigHeader => RunnerFocus::Response,
            RunnerFocus::Response => RunnerFocus::Results,
            RunnerFocus::Results => RunnerFocus::ConfigHeader,
        }
    }
}

/// What a key press asks the App to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    /// Handled internally; nothing for the App to do.
    Consumed,
    /// Validate + guardrail-check, then (re)start the run.
    Run,
    /// The guardrail confirm was accepted — start the run without re-prompting.
    ConfirmedRun,
    /// Cancel the in-flight batch.
    Cancel,
    /// Close the runner.
    Close,
}

/// The load runner's state (UI-only).
#[derive(Debug)]
pub struct LoadRunnerState {
    /// Endpoint label for the title (name).
    pub endpoint_label: String,
    /// The resolved target URL (shown + persisted in the summary).
    pub url: String,
    /// Workspace-relative endpoint path, when any (for the summary).
    pub endpoint_path: Option<String>,
    /// The editable, transient load config.
    pub cfg: LoadConfig,
    /// Which config field is focused for editing.
    pub field: LoadField,
    /// Inline numeric editor for the focused field, when editing.
    pub editing: Option<LineEditor>,
    /// One row per copy, in launch-index order.
    pub results: Vec<ResultRow>,
    /// Completed outcomes so far (for the stats recompute); completion order.
    outcomes: Vec<(churl_core::load::ReqOutcome, Option<Duration>)>,
    /// Row indices currently holding a live [`ResponseState::Done`] view, in
    /// completion order (front = oldest). Bounds retained bodies to `K`: when a
    /// new view lands and the window overflows, the oldest non-selected row here
    /// is downgraded to [`ResponseState::Dropped`]. Drives the memory bound.
    live_views: VecDeque<usize>,
    /// The selected results row (drives the response viewer).
    pub selected: usize,
    /// Which sub-pane has focus.
    pub focus: RunnerFocus,
    /// Whether the results list auto-follows the newest completion. Turned off
    /// once the user navigates the list manually.
    follow: bool,
    /// Copies launched, completed, and currently in flight.
    pub completed: usize,
    /// Batch-cancel / re-run guard: a landed result with a stale generation is
    /// dropped by `App`.
    pub run_generation: u64,
    /// Whether a run is currently in progress.
    pub running: bool,
    /// Whether the run has finished (all copies terminal, or cancelled).
    pub finished: bool,
    /// Whether the run was cancelled.
    pub cancelled: bool,
    /// Live latency + count stats, recomputed as results land.
    pub stats: LoadStats,
    /// Response viewer cursor/scroll/viewport geometry for the selected row. Same
    /// shape as the main pane's so the shared `response_*` handlers drive it when
    /// the Response region is focused.
    pub geometry: ResponseGeometry,
    /// First visible results-list row from the last render (scroll offset).
    list_offset: usize,
    /// Results-list viewport height from the last render.
    list_viewport_height: usize,
    /// Monotonic per-row view generation so two rows' viewports never collide in
    /// the shared highlight cache.
    view_seq: u64,
    /// The guardrail confirm text, when a Warn-level run awaits `y`/`n`.
    pub pending_confirm: Option<String>,
    /// True while awaiting an `esc`-again / `y` confirm to stop a running batch.
    pub confirming_close: bool,
}

impl LoadRunnerState {
    /// Builds a runner for `total` pending copies of the given endpoint. The App
    /// kicks off the run on `r` (never auto-run).
    pub fn new(
        endpoint_label: String,
        url: String,
        endpoint_path: Option<String>,
        cfg: LoadConfig,
    ) -> Self {
        Self {
            endpoint_label,
            url,
            endpoint_path,
            cfg,
            field: LoadField::Total,
            editing: None,
            results: Vec::new(),
            outcomes: Vec::new(),
            live_views: VecDeque::new(),
            selected: 0,
            focus: RunnerFocus::ConfigHeader,
            follow: true,
            completed: 0,
            run_generation: 0,
            running: false,
            finished: false,
            cancelled: false,
            stats: LoadStats::default(),
            geometry: ResponseGeometry::default(),
            list_offset: 0,
            list_viewport_height: 0,
            view_seq: 0,
            pending_confirm: None,
            confirming_close: false,
        }
    }

    /// Mints a unique generation for the next row [`response::ResponseView`] built.
    pub fn next_view_gen(&mut self) -> u64 {
        self.view_seq += 1;
        self.view_seq
    }

    /// Whether a run is currently in progress.
    pub fn is_running(&self) -> bool {
        self.running && !self.finished
    }

    /// Number of copies currently in flight (launched but not yet terminal).
    pub fn in_flight(&self) -> usize {
        self.results
            .iter()
            .filter(|r| matches!(r.status, LoadStatus::Running))
            .count()
    }

    /// Resets the results to `cfg.total` pending rows and clears run state for a
    /// fresh run. Does NOT bump the generation (the App owns that).
    pub fn reset_for_run(&mut self) {
        let total = self.cfg.total;
        self.results = (0..total).map(ResultRow::pending).collect();
        self.outcomes.clear();
        self.live_views.clear();
        self.completed = 0;
        self.selected = 0;
        self.follow = true;
        self.running = true;
        self.finished = false;
        self.cancelled = false;
        self.stats = LoadStats::default();
        self.geometry = ResponseGeometry::default();
        self.list_offset = 0;
        self.pending_confirm = None;
        self.confirming_close = false;
    }

    /// Marks copy `index` as launched (Running). Called by the App as the launcher
    /// starts each copy — but since the launcher streams completions, the App
    /// marks a row Running lazily; this keeps the display honest for pending rows.
    pub fn mark_running(&mut self, index: usize) {
        if let Some(row) = self.results.get_mut(index)
            && matches!(row.status, LoadStatus::Pending)
        {
            row.status = LoadStatus::Running;
        }
    }

    /// Records a completed copy's result: sets its row, appends the outcome,
    /// recomputes stats, and auto-follows the selection to the latest completion
    /// (unless the user has scrolled the list). Returns `true` once every copy is
    /// terminal (the run is done).
    pub fn record_result(
        &mut self,
        index: usize,
        status: LoadStatus,
        timing: Option<Duration>,
        response: ResponseState,
        outcome: churl_core::load::ReqOutcome,
    ) -> bool {
        let holds_view = matches!(response, ResponseState::Done { .. });
        if let Some(row) = self.results.get_mut(index) {
            row.status = status;
            row.timing = timing;
            row.response = response;
        }
        self.outcomes.push((outcome, timing));
        self.completed += 1;
        self.stats = churl_core::load::stats(&self.outcomes);
        if self.follow {
            self.selected = index;
            self.geometry.cursor = 0;
            self.geometry.scroll = 0;
        }
        // Memory bound: track the newly-retained view and evict the oldest
        // ones beyond the window. Done *after* the follow-selection update so the
        // just-selected row is never a candidate for eviction.
        if holds_view {
            self.live_views.push_back(index);
            self.enforce_view_window();
        }
        self.completed >= self.results.len()
    }

    /// Keeps at most [`LIVE_VIEW_WINDOW`] rows holding a live `Done` view (plus
    /// the selected row, which is never evicted). When over the bound, the oldest
    /// non-selected retained row is downgraded to [`ResponseState::Dropped`] —
    /// status/timing/size survive for an honest placeholder, but the body bytes
    /// are released and are NOT reconstructable.
    fn enforce_view_window(&mut self) {
        while self.live_views.len() > LIVE_VIEW_WINDOW {
            // Find the oldest queued row that is not the current selection.
            let Some(pos) = self.live_views.iter().position(|&i| i != self.selected) else {
                // Every retained row is the selection (K == 0 edge); nothing to
                // evict without dropping what the user is viewing.
                break;
            };
            let row_index = self.live_views.remove(pos).expect("position in range");
            self.drop_row_view(row_index);
        }
    }

    /// Downgrades one row's retained `Done` view to [`ResponseState::Dropped`],
    /// releasing its body. No-op if the row no longer holds a live view.
    fn drop_row_view(&mut self, index: usize) {
        if let Some(row) = self.results.get_mut(index)
            && let ResponseState::Done { view } = &row.response
        {
            let dropped = ResponseState::Dropped {
                status: view.status(),
                timing: row.timing,
                size: view.body_len(),
            };
            row.response = dropped;
        }
    }

    /// Selects a results row, resetting the response viewport.
    fn select(&mut self, index: usize) {
        if index < self.results.len() {
            self.selected = index;
            self.geometry.cursor = 0;
            self.geometry.scroll = 0;
        }
    }

    /// Routes a key. Returns what (if anything) the App must do.
    pub fn handle_key(&mut self, key: KeyEvent) -> LoadOutcome {
        // The guardrail confirm intercepts first.
        if self.pending_confirm.is_some() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    self.pending_confirm = None;
                    LoadOutcome::ConfirmedRun
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.pending_confirm = None;
                    LoadOutcome::Consumed
                }
                _ => LoadOutcome::Consumed,
            };
        }
        // The close confirm (only while running) intercepts next.
        if self.confirming_close {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => LoadOutcome::Close,
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.confirming_close = false;
                    LoadOutcome::Consumed
                }
                _ => LoadOutcome::Consumed,
            };
        }
        // An in-progress field edit owns the keyboard.
        if self.editing.is_some() {
            return self.handle_editing_key(key);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.is_running() {
                    LoadOutcome::Cancel
                } else {
                    LoadOutcome::Consumed
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.is_running() {
                    self.confirming_close = true;
                    LoadOutcome::Consumed
                } else {
                    LoadOutcome::Close
                }
            }
            // Run/re-run the batch. Ctrl-R (not plain `r`) to match the sequence
            // surface's Ctrl-R; plain `r` does not run. Guarded like Ctrl-C above
            // — the editing/confirm sub-states
            // return before this match, so Ctrl-R only fires from a live pane.
            KeyCode::Char('r') if ctrl => LoadOutcome::Run,
            // Tab / Shift-Tab are the ONLY cross-pane traversal: Tab cycles
            // forward (config → results → response → config), BackTab reverse.
            // `h`/`j`/`k`/`l` are in-pane movement only.
            KeyCode::Tab => {
                self.focus = self.focus.next();
                LoadOutcome::Consumed
            }
            KeyCode::BackTab => {
                self.focus = self.focus.prev();
                LoadOutcome::Consumed
            }
            _ => {
                match self.focus {
                    RunnerFocus::ConfigHeader => self.handle_config_key(key),
                    RunnerFocus::Results => self.handle_results_key(key),
                    // Response-region keys are routed by `App` through the shared
                    // `response_*` handlers BEFORE this delegate (one
                    // code path, full parity with the main pane). Anything that
                    // reaches here in Response focus is not a response action, so
                    // it is a harmless no-op.
                    RunnerFocus::Response => {}
                }
                LoadOutcome::Consumed
            }
        }
    }

    /// Config-header keys: pick a field, step its value, or begin editing it. The
    /// fields render as a horizontal row, so field nav is `h`/`l` + Left/Right
    /// (in-pane); `k`/Up increments and `j`/Down decrements the focused value.
    fn handle_config_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('l') | KeyCode::Right => {
                self.field = self.field.next();
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.field = self.field.prev();
            }
            KeyCode::Char('k') | KeyCode::Up => self.step_field(1),
            KeyCode::Char('j') | KeyCode::Down => self.step_field(-1),
            KeyCode::Char('i') | KeyCode::Enter => self.begin_edit(),
            _ => {}
        }
    }

    /// Steps the focused field's value by `delta` (in its own unit: interval in
    /// ms), clamping exactly like [`Self::commit_edit`] (total ≥ 1, concurrency
    /// ≥ 1, interval ≥ 0ms). No-op while editing or while a run is in progress
    /// (mirrors [`Self::begin_edit`]).
    fn step_field(&mut self, delta: i64) {
        if self.is_running() || self.editing.is_some() {
            return;
        }
        let step =
            |value: usize, min: usize| -> usize { (value as i64 + delta).max(min as i64) as usize };
        match self.field {
            LoadField::Total => self.cfg.total = step(self.cfg.total, 1),
            LoadField::Concurrency => self.cfg.concurrency = step(self.cfg.concurrency, 1),
            LoadField::Interval => {
                let ms = step(self.cfg.interval.as_millis() as usize, 0);
                self.cfg.interval = Duration::from_millis(ms as u64);
            }
        }
    }

    /// Begins editing the focused field, seeding the editor with its current
    /// value. Editing config while a run is in progress is disallowed (the run
    /// owns `results`); it is only editable before a run or after it finishes.
    fn begin_edit(&mut self) {
        if self.is_running() {
            return;
        }
        let seed = match self.field {
            LoadField::Total => self.cfg.total.to_string(),
            LoadField::Concurrency => self.cfg.concurrency.to_string(),
            LoadField::Interval => self.cfg.interval.as_millis().to_string(),
        };
        self.editing = Some(LineEditor::new(&seed));
    }

    /// Field-edit keys: digits only; Enter commits (parse + clamp), Esc cancels.
    fn handle_editing_key(&mut self, key: KeyEvent) -> LoadOutcome {
        match key.code {
            KeyCode::Enter => {
                self.commit_edit();
                LoadOutcome::Consumed
            }
            KeyCode::Esc => {
                self.editing = None;
                LoadOutcome::Consumed
            }
            // Only digits mutate the numeric field; everything else (letters,
            // symbols) is ignored so the field stays a valid number.
            KeyCode::Char(c) if !c.is_ascii_digit() => LoadOutcome::Consumed,
            _ => {
                if let Some(editor) = self.editing.as_mut() {
                    editor.handle_key(key);
                }
                LoadOutcome::Consumed
            }
        }
    }

    /// Commits the focused field's edit: parses the digits and clamps (total ≥ 1,
    /// concurrency ≥ 1, interval ≥ 0). An empty / unparseable value keeps the old
    /// value.
    fn commit_edit(&mut self) {
        let Some(editor) = self.editing.take() else {
            return;
        };
        let text = editor.text();
        let parsed = text.trim().parse::<usize>().ok();
        if let Some(value) = parsed {
            match self.field {
                LoadField::Total => self.cfg.total = value.max(1),
                LoadField::Concurrency => self.cfg.concurrency = value.max(1),
                LoadField::Interval => self.cfg.interval = Duration::from_millis(value as u64),
            }
        }
    }

    /// Results-list navigation keys. Any manual move turns off auto-follow.
    fn handle_results_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.results.len() {
                    self.follow = false;
                    self.select(self.selected + 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected > 0 {
                    self.follow = false;
                    self.select(self.selected - 1);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.follow = false;
                self.select(0);
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.follow = false;
                self.select(self.results.len().saturating_sub(1));
            }
            _ => {}
        }
    }

    /// Whether a runner sub-state currently owns the keyboard, so `App` must NOT
    /// route Response actions into the shared handlers: the guardrail
    /// confirm, the running-close confirm, or an in-progress config-field edit. In
    /// any of these the runner's own `handle_key` interception takes the key.
    pub fn response_input_captured(&self) -> bool {
        self.pending_confirm.is_some() || self.confirming_close || self.editing.is_some()
    }

    /// The selected results row's response state, for the shared `response_*`
    /// handlers. `None` when there is no selectable row (pre-run) — the
    /// caller falls back to an idle no-op.
    pub fn selected_response(&self) -> Option<&ResponseState> {
        self.results.get(self.selected).map(|row| &row.response)
    }

    /// Mutable selected-row response state (see [`Self::selected_response`]).
    pub fn selected_response_mut(&mut self) -> Option<&mut ResponseState> {
        self.results
            .get_mut(self.selected)
            .map(|row| &mut row.response)
    }
}

/// The live progress + stats line, e.g. `12/50 done · 44 ok · 6 failed · range
/// 12–210ms · p50/p95 45/120ms · avg 78ms`.
fn stats_line(state: &LoadRunnerState) -> String {
    let total = state.results.len();
    let s = &state.stats;
    let mut parts = vec![format!("{}/{} done", state.completed, total)];
    parts.push(format!("{} ok", s.ok));
    if s.failed > 0 {
        parts.push(format!("{} failed", s.failed));
    }
    if s.errored > 0 {
        parts.push(format!("{} errored", s.errored));
    }
    let ms = |d: Option<Duration>| d.map(|d| d.as_millis());
    // All latency fields are `Some` together (they derive from the same
    // completed-request timings), so one guard covers them — no mean shown when
    // nothing has completed.
    if let (Some(min), Some(p50), Some(p95), Some(max), Some(mean)) =
        (ms(s.min), ms(s.median), ms(s.p95), ms(s.max), ms(s.mean))
    {
        parts.push(format!("range {min}–{max}ms"));
        parts.push(format!("p50/p95 {p50}/{p95}ms"));
        parts.push(format!("avg {mean}ms"));
    }
    parts.join(" · ")
}

/// The editable config header spans, e.g. `total=10 · concurrency=5 · min
/// gap=0ms · max rate` with the focused field highlighted (and its inline editor
/// shown while editing). The gap field carries a compact derived rate suffix
/// (` · ≈N req/s`, or ` · max rate` at gap=0).
fn config_spans(state: &LoadRunnerState, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let header_focused = state.focus == RunnerFocus::ConfigHeader;
    let fields = [
        LoadField::Total,
        LoadField::Concurrency,
        LoadField::Interval,
    ];
    for (i, field) in fields.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme.statusline));
        }
        let focused = header_focused && state.field == field;
        let value = if focused && state.editing.is_some() {
            // Show the live edit text with a caret.
            let text = state
                .editing
                .as_ref()
                .map(LineEditor::text)
                .unwrap_or_default();
            format!("{text}▏")
        } else {
            match field {
                LoadField::Total => state.cfg.total.to_string(),
                LoadField::Concurrency => state.cfg.concurrency.to_string(),
                LoadField::Interval => format!("{}ms", state.cfg.interval.as_millis()),
            }
        };
        let label_style = if focused {
            theme.title
        } else {
            theme.statusline
        };
        let value_style = if focused {
            theme.selection
        } else {
            theme.title
        };
        spans.push(Span::styled(format!("{}=", field.label()), label_style));
        spans.push(Span::styled(value, value_style));
        // The min-gap field carries a compact derived-rate annotation, styled
        // like the field separators (statusline) not the value, and omitted
        // while the field is being edited. gap==0 ⇒ unthrottled; otherwise a
        // req/s floor with adaptive precision so sub-1 rates don't read as 0.
        if field == LoadField::Interval && !(focused && state.editing.is_some()) {
            let gap_ms = state.cfg.interval.as_millis();
            let suffix = if gap_ms == 0 {
                " · max rate".to_string()
            } else {
                let rate = 1000.0 / gap_ms as f64;
                let rate_str = if rate >= 10.0 {
                    format!("{rate:.0}")
                } else if rate >= 1.0 {
                    format!("{rate:.1}")
                } else {
                    format!("{rate:.2}")
                };
                format!(" · ≈{rate_str} req/s")
            };
            spans.push(Span::styled(suffix, theme.statusline));
        }
    }
    spans
}

/// Renders the load runner over `area`. Returns a [`HighlightJob`] for the
/// selected row's response viewport on a cache miss, for the caller to enqueue.
#[must_use]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &mut LoadRunnerState,
    tick_count: u64,
    cache: &HashMap<u64, Vec<Line<'static>>>,
    theme: &Theme,
) -> Option<HighlightJob> {
    let [modal] = Layout::horizontal([Constraint::Percentage(90)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(90)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(" Load ".to_owned())
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Top-left block, ordered name → url → config → stats, each on its own row so
    // the four are visually distinct; then the one-line purpose hint, body, footer.
    let [
        name_row,
        url_row,
        config_row,
        stats_row,
        hint_row,
        body,
        footer,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            state.endpoint_label.clone(),
            theme.title,
        ))),
        name_row,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("→ ", theme.statusline),
            Span::styled(state.url.clone(), theme.statusline),
        ])),
        url_row,
    );
    frame.render_widget(
        Paragraph::new(Line::from(config_spans(state, theme))),
        config_row,
    );
    let status_word = if state.cancelled {
        "  cancelled"
    } else if state.finished {
        "  done"
    } else if state.running {
        "  running"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(stats_line(state), theme.title),
            Span::styled(status_word.to_owned(), theme.statusline),
        ])),
        stats_row,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Fire this endpoint repeatedly at a set concurrency to measure throughput and latency.",
            theme.statusline,
        ))),
        hint_row,
    );

    let left_width = inner.width.saturating_sub(2) / 2;
    let left_width = left_width.clamp(1, 40);
    let [left, right] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Fill(1)]).areas(body);

    render_results_list(frame, left, state, theme);
    let job = render_response(frame, right, state, tick_count, cache, theme);
    render_footer(frame, footer, state, theme);

    if state.pending_confirm.is_some() {
        prompt::render_confirm(
            frame,
            modal,
            "Load guardrail",
            state.pending_confirm.as_deref().unwrap_or(""),
            "y fire · n cancel",
            theme,
        );
    } else if state.confirming_close {
        prompt::render_confirm(
            frame,
            modal,
            "Run in progress",
            "Stop the batch and close?",
            "y stop · esc keep running",
            theme,
        );
    }
    job
}

/// Renders the left results-list column, O(viewport): only the visible slice of
/// rows is built regardless of how large `total` is.
fn render_results_list(frame: &mut Frame, area: Rect, state: &mut LoadRunnerState, theme: &Theme) {
    let focused = state.focus == RunnerFocus::Results;
    let block = Block::bordered()
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(if focused {
            theme.border_focused
        } else {
            theme.border_unfocused
        })
        .title(" Results ")
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let height = inner.height as usize;
    state.list_viewport_height = height;
    // Scroll the window so the selected row stays visible (O(viewport) render).
    if state.selected < state.list_offset {
        state.list_offset = state.selected;
    } else if state.selected >= state.list_offset + height {
        state.list_offset = state.selected + 1 - height;
    }
    let end = (state.list_offset + height).min(state.results.len());

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    for row in &state.results[state.list_offset..end] {
        let selected = row.index == state.selected;
        let glyph_style = match &row.status {
            LoadStatus::Ok(_) => theme.response_status,
            LoadStatus::Failed(_) | LoadStatus::Error(_) => theme.status_error,
            LoadStatus::Running => theme.border_focused,
            _ => theme.statusline,
        };
        let mut spans = vec![
            Span::styled(format!("{} ", row.status.glyph()), glyph_style),
            Span::styled(format!("#{:<4} ", row.index + 1), theme.statusline),
        ];
        if let Some(detail) = row.status.detail() {
            spans.push(Span::raw(detail));
        }
        if let Some(timing) = row.timing {
            spans.push(Span::styled(
                format!("  {}ms", timing.as_millis()),
                theme.statusline,
            ));
        }
        let mut line = Line::from(spans);
        if selected {
            line = line.style(theme.selection);
        }
        lines.push(line);
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Renders the right response column: the selected row's response in the real
/// viewer. Returns the highlight job the caller should enqueue.
fn render_response(
    frame: &mut Frame,
    area: Rect,
    state: &mut LoadRunnerState,
    tick_count: u64,
    cache: &HashMap<u64, Vec<Line<'static>>>,
    theme: &Theme,
) -> Option<HighlightJob> {
    let focused = state.focus == RunnerFocus::Response;
    // Always render the bordered "Response" pane, from the moment the runner
    // opens. When there is no selectable row yet (pre-run), delegate to
    // `response::render` with an `Idle` state so the pane draws the same
    // bordered block + dim placeholder it uses for an idle/no-response row —
    // never a blank right half. A selected row with a real response
    // delegates to the same renderer with its own state, unchanged.
    let render_state = match state.results.get(state.selected) {
        Some(row) => &row.response,
        None => &ResponseState::Idle,
    };
    let outcome = response::render(
        frame,
        area,
        RenderCtx {
            state: render_state,
            request: None,
            focused,
            scroll: state.geometry.scroll,
            cursor: state.geometry.cursor,
            cache,
            theme,
            jump_label: None,
            tick_count,
        },
    );
    state.geometry.apply_render_outcome(&outcome);
    // Write the clamped horizontal scroll back onto the selected view so an
    // over-pan self-corrects on the next frame — mirrors the main pane,
    // now reachable in the runner via the shared hscroll parity.
    if let Some(ResponseState::Done { view }) = state.selected_response_mut() {
        view.set_h_scroll(outcome.clamped_h_scroll);
    }
    outcome.job
}

/// Renders the footer key hints, contextual to focus.
fn render_footer(frame: &mut Frame, area: Rect, state: &LoadRunnerState, theme: &Theme) {
    let hint = if state.editing.is_some() {
        "digits · enter commit · esc cancel"
    } else {
        match state.focus {
            RunnerFocus::ConfigHeader => {
                "h/l field · j/k adjust · enter edit · ^R run · tab results · ctrl-c cancel · q close"
            }
            RunnerFocus::Results => "j/k row · tab response · ^R re-run · ctrl-c cancel · q close",
            RunnerFocus::Response => {
                "j/k scroll · h hdrs · p/s pretty · / search · y copy · tab config · q close"
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme.statusline))),
        area,
    );
}

#[cfg(test)]
mod tests;
