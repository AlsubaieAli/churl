//! Concurrent-load runner overlay (`Mode::LoadRunner`, M7.5): a large modal that
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

use std::collections::HashMap;
use std::time::Duration;

use churl_core::load::{LoadConfig, LoadStats};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::line_editor::LineEditor;
use super::prompt;
use super::response::{self, RenderCtx, ResponseState};
use crate::tui::highlight::HighlightJob;
use crate::tui::theme::Theme;

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
    /// The single-cell glyph for this status.
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
    /// Response viewer cursor (display row) for the selected row.
    pub resp_cursor: usize,
    /// Response viewer scroll offset for the selected row.
    pub resp_scroll: usize,
    /// Total display rows in the selected response, from the last render.
    resp_total_rows: usize,
    /// Body viewport height from the last render (half-page scrolling).
    resp_viewport_height: usize,
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
            selected: 0,
            focus: RunnerFocus::ConfigHeader,
            follow: true,
            completed: 0,
            run_generation: 0,
            running: false,
            finished: false,
            cancelled: false,
            stats: LoadStats::default(),
            resp_cursor: 0,
            resp_scroll: 0,
            resp_total_rows: 0,
            resp_viewport_height: 0,
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
        self.completed = 0;
        self.selected = 0;
        self.follow = true;
        self.running = true;
        self.finished = false;
        self.cancelled = false;
        self.stats = LoadStats::default();
        self.resp_cursor = 0;
        self.resp_scroll = 0;
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
            self.resp_cursor = 0;
            self.resp_scroll = 0;
        }
        self.completed >= self.results.len()
    }

    /// Selects a results row, resetting the response viewport.
    fn select(&mut self, index: usize) {
        if index < self.results.len() {
            self.selected = index;
            self.resp_cursor = 0;
            self.resp_scroll = 0;
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
            KeyCode::Char('r') => LoadOutcome::Run,
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
                    RunnerFocus::Response => self.handle_response_key(key),
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
            // k/up = increase, j/down = decrease (matches vertical arrow sense).
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

    /// Response-viewer keys (reuse the `ResponseView` mutators — no duplication).
    fn handle_response_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let max = self.resp_total_rows.saturating_sub(1);
        let half = (self.resp_viewport_height / 2).max(1);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.resp_cursor = (self.resp_cursor + 1).min(max);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.resp_cursor = self.resp_cursor.saturating_sub(1);
            }
            KeyCode::Char('d') if ctrl => self.resp_cursor = (self.resp_cursor + half).min(max),
            KeyCode::Char('u') if ctrl => self.resp_cursor = self.resp_cursor.saturating_sub(half),
            KeyCode::Char('g') | KeyCode::Home => self.resp_cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.resp_cursor = max,
            KeyCode::Char('h') => self.with_view(|view| {
                view.toggle_view_mode();
            }),
            KeyCode::Char('W') => self.with_view(|view| view.toggle_wrap()),
            KeyCode::Char('o') => {
                let cursor = self.resp_cursor;
                self.with_view(|view| {
                    view.toggle_fold_at(cursor);
                });
            }
            KeyCode::Char('O') => self.with_view(|view| {
                view.toggle_all_folds();
            }),
            _ => {}
        }
    }

    /// Applies `f` to the selected row's `ResponseView`, if it has a response.
    fn with_view(&mut self, f: impl FnOnce(&mut response::ResponseView)) {
        if let Some(row) = self.results.get_mut(self.selected)
            && let ResponseState::Done { view } = &mut row.response
        {
            f(view);
        }
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
        .title(format!(" Load · {} ", state.endpoint_label))
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Header (config + target url + stats) + body + footer.
    let [config_row, url_row, stats_row, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(config_spans(state, theme))),
        config_row,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("→ ", theme.statusline),
            Span::styled(state.url.clone(), theme.statusline),
        ])),
        url_row,
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
    // never a blank right half (M7.5.2 L1). A selected row with a real response
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
            scroll: state.resp_scroll,
            cursor: state.resp_cursor,
            cache,
            theme,
            jump_label: None,
            tick_count,
        },
    );
    state.resp_scroll = outcome.clamped_scroll;
    state.resp_cursor = outcome.clamped_cursor;
    state.resp_total_rows = outcome.total_rows;
    state.resp_viewport_height = outcome.viewport_height;
    outcome.job
}

/// Renders the footer key hints, contextual to focus.
fn render_footer(frame: &mut Frame, area: Rect, state: &LoadRunnerState, theme: &Theme) {
    let hint = if state.editing.is_some() {
        "digits · enter commit · esc cancel"
    } else {
        match state.focus {
            RunnerFocus::ConfigHeader => {
                "h/l field · j/k adjust · enter edit · r run · tab results · ctrl-c cancel · q close"
            }
            RunnerFocus::Results => "j/k row · tab response · r re-run · ctrl-c cancel · q close",
            RunnerFocus::Response => "j/k scroll · W wrap · o/O fold · tab config · q close",
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme.statusline))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use churl_core::load::ReqOutcome;

    fn runner() -> LoadRunnerState {
        LoadRunnerState::new(
            "List users".to_owned(),
            "https://api.test/users".to_owned(),
            Some("users/list.toml".to_owned()),
            LoadConfig {
                total: 10,
                concurrency: 5,
                interval: Duration::ZERO,
            },
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn tab_cycles_focus() {
        let mut r = runner();
        assert_eq!(r.focus, RunnerFocus::ConfigHeader);
        r.handle_key(key(KeyCode::Tab));
        assert_eq!(r.focus, RunnerFocus::Results);
        r.handle_key(key(KeyCode::Tab));
        assert_eq!(r.focus, RunnerFocus::Response);
        r.handle_key(key(KeyCode::Tab));
        assert_eq!(r.focus, RunnerFocus::ConfigHeader);
    }

    #[test]
    fn backtab_cycles_focus_reverse() {
        let mut r = runner();
        assert_eq!(r.focus, RunnerFocus::ConfigHeader);
        r.handle_key(key(KeyCode::BackTab));
        assert_eq!(r.focus, RunnerFocus::Response);
        r.handle_key(key(KeyCode::BackTab));
        assert_eq!(r.focus, RunnerFocus::Results);
        r.handle_key(key(KeyCode::BackTab));
        assert_eq!(r.focus, RunnerFocus::ConfigHeader);
    }

    /// The derived req/s annotation reads honestly across gaps (review
    /// finding #1): sub-1-req/s rates must not truncate to `≈0 req/s`.
    #[test]
    fn derived_rate_reads_honestly_across_gaps() {
        let theme = Theme::default();
        let text = |r: &LoadRunnerState| -> String {
            config_spans(r, &theme)
                .into_iter()
                .map(|s| s.content.into_owned())
                .collect()
        };
        let mut r = runner();
        r.cfg.interval = Duration::from_millis(0);
        assert!(text(&r).contains("min gap=0ms · max rate"));
        r.cfg.interval = Duration::from_millis(500);
        assert!(text(&r).contains("≈2.0 req/s"), "got: {}", text(&r));
        r.cfg.interval = Duration::from_millis(1);
        assert!(text(&r).contains("≈1000 req/s"), "got: {}", text(&r));
        // gaps ≥ 1s were reading as ≈0 req/s under integer division.
        r.cfg.interval = Duration::from_millis(2000);
        let t = text(&r);
        assert!(t.contains("≈0.50 req/s"), "got: {t}");
        assert!(!t.contains("≈0 req/s"), "sub-1 rate truncated to zero: {t}");
    }

    #[test]
    fn config_field_nav_is_h_l() {
        let mut r = runner();
        assert_eq!(r.field, LoadField::Total);
        // l / Right advance; h / Left go back (fields are a horizontal row).
        r.handle_key(ch('l'));
        assert_eq!(r.field, LoadField::Concurrency);
        r.handle_key(key(KeyCode::Right));
        assert_eq!(r.field, LoadField::Interval);
        r.handle_key(ch('h'));
        assert_eq!(r.field, LoadField::Concurrency);
        r.handle_key(key(KeyCode::Left));
        assert_eq!(r.field, LoadField::Total);
        // j/k step the value now, not the field (see step_focused_field_value).
        r.handle_key(ch('j'));
        r.handle_key(ch('k'));
        assert_eq!(r.field, LoadField::Total);
    }

    #[test]
    fn step_focused_field_value() {
        let mut r = runner(); // total=10, concurrency=5, interval=0ms
        // k/Up increments the focused field; j/Down decrements it.
        r.handle_key(ch('k'));
        assert_eq!(r.cfg.total, 11);
        r.handle_key(key(KeyCode::Up));
        assert_eq!(r.cfg.total, 12);
        r.handle_key(ch('j'));
        assert_eq!(r.cfg.total, 11);
        r.handle_key(key(KeyCode::Down));
        assert_eq!(r.cfg.total, 10);
        // Concurrency steps too.
        r.handle_key(ch('l')); // → Concurrency
        r.handle_key(ch('k'));
        assert_eq!(r.cfg.concurrency, 6);
        // Interval steps by 1ms.
        r.handle_key(ch('l')); // → Interval
        r.handle_key(ch('k'));
        assert_eq!(r.cfg.interval, Duration::from_millis(1));
        r.handle_key(ch('j'));
        assert_eq!(r.cfg.interval, Duration::ZERO);
    }

    #[test]
    fn step_field_clamps_at_minimums() {
        let mut r = runner();
        r.cfg.total = 1;
        r.handle_key(ch('j')); // can't go below 1
        assert_eq!(r.cfg.total, 1);
        r.handle_key(ch('l')); // → Concurrency
        r.cfg.concurrency = 1;
        r.handle_key(ch('j'));
        assert_eq!(r.cfg.concurrency, 1);
        r.handle_key(ch('l')); // → Interval
        r.cfg.interval = Duration::ZERO;
        r.handle_key(ch('j')); // interval down-clamps at 0ms
        assert_eq!(r.cfg.interval, Duration::ZERO);
    }

    #[test]
    fn step_field_is_noop_while_editing_or_running() {
        // While editing, j/k feed the editor (digits ignored), not the stepper.
        let mut r = runner();
        r.handle_key(ch('i')); // begin edit of total (seed "10")
        assert!(r.editing.is_some());
        r.handle_key(ch('k'));
        r.handle_key(ch('j'));
        assert_eq!(r.cfg.total, 10, "stepping is disabled while editing");
        // While running, config is locked entirely.
        let mut r = runner();
        r.reset_for_run();
        let before = r.cfg.total;
        r.handle_key(ch('k'));
        r.handle_key(ch('j'));
        assert_eq!(r.cfg.total, before, "stepping is disabled during a run");
    }

    #[test]
    fn config_field_pick_and_numeric_edit_clamps() {
        let mut r = runner();
        // Pick the concurrency field, edit it to 0 → clamps to 1.
        r.handle_key(ch('l')); // Total → Concurrency
        assert_eq!(r.field, LoadField::Concurrency);
        r.handle_key(ch('i')); // begin edit
        assert!(r.editing.is_some());
        // Clear the seed and type 0.
        r.handle_key(key(KeyCode::Backspace));
        r.handle_key(ch('0'));
        r.handle_key(key(KeyCode::Enter));
        assert!(r.editing.is_none());
        assert_eq!(r.cfg.concurrency, 1, "concurrency clamps to >= 1");
    }

    #[test]
    fn edit_accepts_only_digits() {
        let mut r = runner();
        r.handle_key(ch('i')); // edit total (seed "10")
        r.handle_key(ch('x')); // ignored
        r.handle_key(ch('5')); // "105"
        r.handle_key(key(KeyCode::Enter));
        assert_eq!(r.cfg.total, 105);
    }

    #[test]
    fn interval_edits_to_zero_allowed() {
        let mut r = runner();
        r.handle_key(ch('l'));
        r.handle_key(ch('l')); // → Interval
        assert_eq!(r.field, LoadField::Interval);
        r.handle_key(ch('i'));
        r.handle_key(ch('2'));
        r.handle_key(ch('5'));
        r.handle_key(key(KeyCode::Enter));
        assert_eq!(r.cfg.interval, Duration::from_millis(25));
    }

    #[test]
    fn r_requests_run() {
        let mut r = runner();
        assert_eq!(r.handle_key(ch('r')), LoadOutcome::Run);
    }

    #[test]
    fn q_closes_when_idle() {
        let mut r = runner();
        assert_eq!(r.handle_key(ch('q')), LoadOutcome::Close);
    }

    #[test]
    fn q_confirms_then_closes_while_running() {
        let mut r = runner();
        r.reset_for_run();
        assert_eq!(r.handle_key(ch('q')), LoadOutcome::Consumed);
        assert!(r.confirming_close);
        assert_eq!(r.handle_key(key(KeyCode::Esc)), LoadOutcome::Consumed);
        assert!(!r.confirming_close);
        r.handle_key(ch('q'));
        assert_eq!(r.handle_key(ch('y')), LoadOutcome::Close);
    }

    #[test]
    fn ctrl_c_cancels_only_while_running() {
        let mut r = runner();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(r.handle_key(ctrl_c), LoadOutcome::Consumed);
        r.reset_for_run();
        assert_eq!(r.handle_key(ctrl_c), LoadOutcome::Cancel);
    }

    #[test]
    fn guardrail_confirm_yes_runs_no_aborts() {
        let mut r = runner();
        r.pending_confirm = Some("Fire 500 requests?".to_owned());
        // `n` aborts.
        assert_eq!(r.handle_key(ch('n')), LoadOutcome::Consumed);
        assert!(r.pending_confirm.is_none());
        // `y` confirms.
        r.pending_confirm = Some("Fire 500 requests?".to_owned());
        assert_eq!(r.handle_key(ch('y')), LoadOutcome::ConfirmedRun);
        assert!(r.pending_confirm.is_none());
    }

    #[test]
    fn record_result_updates_stats_and_follows() {
        let mut r = runner();
        r.reset_for_run(); // 10 pending rows
        let done = r.record_result(
            0,
            LoadStatus::Ok(200),
            Some(Duration::from_millis(12)),
            ResponseState::Idle,
            ReqOutcome::Ok { status: 200 },
        );
        assert!(!done);
        assert_eq!(r.completed, 1);
        assert_eq!(r.stats.ok, 1);
        assert_eq!(r.stats.min, Some(Duration::from_millis(12)));
        assert_eq!(r.selected, 0, "follow selected the completed row");
    }

    #[test]
    fn record_result_reports_done_on_last() {
        let mut r = runner();
        r.cfg.total = 2;
        r.reset_for_run();
        assert!(!r.record_result(
            0,
            LoadStatus::Ok(200),
            Some(Duration::from_millis(5)),
            ResponseState::Idle,
            ReqOutcome::Ok { status: 200 },
        ));
        assert!(r.record_result(
            1,
            LoadStatus::Failed(500),
            Some(Duration::from_millis(7)),
            ResponseState::Idle,
            ReqOutcome::Failed { status: 500 },
        ));
        assert_eq!(r.stats.ok, 1);
        assert_eq!(r.stats.failed, 1);
    }

    #[test]
    fn manual_navigation_disables_follow() {
        let mut r = runner();
        r.reset_for_run();
        r.focus = RunnerFocus::Results;
        r.handle_key(ch('j'));
        assert!(!r.follow);
        assert_eq!(r.selected, 1);
        // A later completion no longer moves the selection.
        r.record_result(
            5,
            LoadStatus::Ok(200),
            Some(Duration::from_millis(3)),
            ResponseState::Idle,
            ReqOutcome::Ok { status: 200 },
        );
        assert_eq!(r.selected, 1, "follow off keeps the user's selection");
    }

    #[test]
    fn cannot_edit_config_while_running() {
        let mut r = runner();
        r.reset_for_run();
        r.handle_key(ch('i'));
        assert!(r.editing.is_none(), "config is locked during a run");
    }

    #[test]
    fn stats_line_shows_counts_and_percentiles() {
        let mut r = runner();
        r.cfg.total = 2;
        r.reset_for_run();
        r.record_result(
            0,
            LoadStatus::Ok(200),
            Some(Duration::from_millis(10)),
            ResponseState::Idle,
            ReqOutcome::Ok { status: 200 },
        );
        r.record_result(
            1,
            LoadStatus::Failed(500),
            Some(Duration::from_millis(30)),
            ResponseState::Idle,
            ReqOutcome::Failed { status: 500 },
        );
        let line = stats_line(&r);
        assert!(line.contains("2/2 done"), "{line}");
        assert!(line.contains("1 ok"), "{line}");
        assert!(line.contains("1 failed"), "{line}");
        // Grouped latency parts (L3): range (en dash), p50/p95, avg.
        assert!(line.contains("range 10–30ms"), "{line}");
        assert!(line.contains("p50/p95"), "{line}");
        assert!(line.contains("avg "), "{line}");
        // Old separate labels are gone.
        assert!(!line.contains("min "), "{line}");
        assert!(!line.contains("mean "), "{line}");
    }

    // ---- render snapshots (TestBackend → deterministic plain text) ----

    use churl_core::model::{Header, Response, Timing};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn snapshot(state: &mut LoadRunnerState) -> String {
        let backend = TestBackend::new(116, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let theme = Theme::default();
        let cache = HashMap::new();
        terminal
            .draw(|frame| {
                let _ = render(frame, frame.area(), state, 0, &cache, &theme);
            })
            .expect("draw");
        format!("{}", terminal.backend())
    }

    fn json_response(status: u16, body: &str) -> Response {
        Response {
            status,
            headers: vec![Header {
                name: "Content-Type".into(),
                value: "application/json".into(),
                enabled: true,
            }],
            body: body.as_bytes().to_vec(),
            truncated: false,
            timing: Timing {
                connect: None,
                total: Duration::from_millis(42),
            },
        }
    }

    #[test]
    fn snapshot_config_header_pre_run() {
        let mut r = runner();
        insta::assert_snapshot!(snapshot(&mut r));
    }

    /// L1: the Response pane must be a bordered, titled pane with a placeholder
    /// from the moment the runner opens — never a blank right half — even though
    /// there is no selectable results row yet (`results` is empty pre-run).
    #[test]
    fn response_pane_renders_pre_run() {
        let mut r = runner();
        assert!(r.results.is_empty(), "no rows before the first run");
        let out = snapshot(&mut r);
        assert!(
            out.contains("Response"),
            "titled Response pane missing pre-run:\n{out}"
        );
        assert!(
            out.contains("no response yet"),
            "idle placeholder missing pre-run:\n{out}"
        );
    }

    #[test]
    fn snapshot_mid_run_live_stats_mixed_glyphs() {
        let mut r = runner();
        r.cfg.total = 8;
        r.reset_for_run();
        r.record_result(
            0,
            LoadStatus::Ok(200),
            Some(Duration::from_millis(12)),
            ResponseState::Idle,
            ReqOutcome::Ok { status: 200 },
        );
        r.record_result(
            1,
            LoadStatus::Failed(503),
            Some(Duration::from_millis(88)),
            ResponseState::Idle,
            ReqOutcome::Failed { status: 503 },
        );
        r.record_result(
            2,
            LoadStatus::Error("connection refused".into()),
            None,
            ResponseState::Idle,
            ReqOutcome::Error("connection refused".into()),
        );
        // Two copies still in flight; the rest pending.
        r.results[3].status = LoadStatus::Running;
        r.results[4].status = LoadStatus::Running;
        r.focus = RunnerFocus::Results;
        insta::assert_snapshot!(snapshot(&mut r));
    }

    #[test]
    fn snapshot_finished_with_failures_stats_line() {
        let mut r = runner();
        r.cfg.total = 5;
        r.reset_for_run();
        for (i, (status, outcome, ms)) in [
            (LoadStatus::Ok(200), ReqOutcome::Ok { status: 200 }, 10u64),
            (LoadStatus::Ok(200), ReqOutcome::Ok { status: 200 }, 20),
            (
                LoadStatus::Failed(500),
                ReqOutcome::Failed { status: 500 },
                30,
            ),
            (LoadStatus::Ok(200), ReqOutcome::Ok { status: 200 }, 40),
            (
                LoadStatus::Failed(500),
                ReqOutcome::Failed { status: 500 },
                50,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            r.record_result(
                i,
                status,
                Some(Duration::from_millis(ms)),
                ResponseState::Idle,
                outcome,
            );
        }
        r.running = false;
        r.finished = true;
        insta::assert_snapshot!(snapshot(&mut r));
    }

    #[test]
    fn snapshot_selected_response_shown() {
        let mut r = runner();
        r.cfg.total = 2;
        r.reset_for_run();
        let view =
            response::ResponseView::build(&json_response(200, "{\"ok\":true}"), r.next_view_gen());
        r.record_result(
            0,
            LoadStatus::Ok(200),
            Some(Duration::from_millis(42)),
            ResponseState::Done { view },
            ReqOutcome::Ok { status: 200 },
        );
        r.running = false;
        r.finished = true;
        r.selected = 0;
        r.focus = RunnerFocus::Response;
        insta::assert_snapshot!(snapshot(&mut r));
    }
}
