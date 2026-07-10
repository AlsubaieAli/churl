//! Sequence runner overlay (`Mode::SequenceRunner`): a large modal that drives a
//! request sequence step by step and shows live per-step status/timing plus each
//! step's response in the real response viewer.
//!
//! The runner is UI-only: it holds display state and the parsed steps, but the
//! actual run orchestration (prepare → spawn `execute` → classify → advance) lives
//! in `App`, which owns the HTTP client, generation guard, and abort handle and
//! reuses the `churl_core::sequence` primitives so the live driver and the
//! wiremock-tested `run_sequence` can never drift.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use churl_core::config::looks_like_secret_name;
use churl_core::model::{Method, OnError, SequenceStep};
use churl_core::sequence::StepResult;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::prompt;
use super::response::{self, RenderCtx, ResponseGeometry, ResponseState};
use crate::tui::highlight::HighlightJob;
use crate::tui::theme::Theme;

/// Per-step display status in the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    /// Not yet run.
    Pending,
    /// Currently in flight.
    Running,
    /// Completed with a success status (< 400) and clean extraction.
    Ok(u16),
    /// Completed with an HTTP error status (≥ 400).
    Failed(u16),
    /// The request could not be sent (transport error / prepare failure).
    HttpError(String),
    /// The request succeeded but an extraction rule failed.
    ExtractError(String),
    /// Never ran because an earlier step halted the sequence.
    Skipped,
}

impl StepStatus {
    /// Maps a core [`StepResult`] to a display status.
    pub fn from_result(result: &StepResult) -> Self {
        match result {
            StepResult::Ok { status } => StepStatus::Ok(*status),
            StepResult::Failed { status } => StepStatus::Failed(*status),
            StepResult::HttpError(msg) => StepStatus::HttpError(msg.clone()),
            StepResult::ExtractError(msg) => StepStatus::ExtractError(msg.clone()),
            StepResult::Skipped => StepStatus::Skipped,
        }
    }

    /// The single-cell glyph for this status (spinner handled separately).
    fn glyph(&self, tick: u64) -> &'static str {
        match self {
            StepStatus::Pending => "○",
            StepStatus::Running => SPINNER[(tick as usize) % SPINNER.len()],
            StepStatus::Ok(_) => "✓",
            StepStatus::Failed(_) | StepStatus::HttpError(_) | StepStatus::ExtractError(_) => "✗",
            StepStatus::Skipped => "–",
        }
    }

    /// Whether this status is a failure (for the progress summary).
    fn is_failure(&self) -> bool {
        matches!(
            self,
            StepStatus::Failed(_) | StepStatus::HttpError(_) | StepStatus::ExtractError(_)
        )
    }

    /// Whether the step actually ran to a terminal result (a `Skipped` tail did
    /// not run; `Pending`/`Running` have not finished).
    fn ran(&self) -> bool {
        matches!(
            self,
            StepStatus::Ok(_)
                | StepStatus::Failed(_)
                | StepStatus::HttpError(_)
                | StepStatus::ExtractError(_)
        )
    }

    /// A short trailing detail for the step row, when useful.
    fn detail(&self) -> Option<String> {
        match self {
            StepStatus::Ok(status) | StepStatus::Failed(status) => Some(status.to_string()),
            StepStatus::HttpError(_) => Some("error".to_owned()),
            StepStatus::ExtractError(_) => Some("extract".to_owned()),
            StepStatus::Skipped => Some("skipped".to_owned()),
            StepStatus::Pending | StepStatus::Running => None,
        }
    }
}

/// Spinner frames, reused from the in-flight style.
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One step's row in the runner.
#[derive(Debug)]
pub struct StepRow {
    /// The parsed step (endpoint path + extraction rules), for the App to prepare
    /// and extract against.
    pub step: SequenceStep,
    /// Method shown in the row (resolved once prepared; `GET` until then).
    pub method: Method,
    /// The endpoint path label.
    pub endpoint: String,
    /// The resolved URL once prepared (empty until then).
    pub url: String,
    /// Display status.
    pub status: StepStatus,
    /// Request timing once completed.
    pub timing: Option<Duration>,
    /// Values this step extracted (masked on display when secret-named).
    pub extracted: BTreeMap<String, String>,
    /// The response state for the viewer (Idle/InFlight/Done/Failed).
    pub response: ResponseState,
}

impl StepRow {
    /// Builds a pending row from a step.
    pub fn new(step: SequenceStep) -> Self {
        let endpoint = step.endpoint.clone();
        Self {
            step,
            method: Method::Get,
            endpoint,
            url: String::new(),
            status: StepStatus::Pending,
            timing: None,
            extracted: BTreeMap::new(),
            response: ResponseState::Idle,
        }
    }
}

/// Which sub-pane of the runner has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerFocus {
    /// The step list (j/k select steps).
    Steps,
    /// The response viewer (j/k scroll; h/W/o/O operate on the view).
    Response,
}

/// What a key press asks the App to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerOutcome {
    /// Handled internally; nothing for the App to do.
    Consumed,
    /// Re-run the whole sequence from the top (App resets + drives).
    Rerun,
    /// Cancel the in-flight run (App aborts the task + marks the tail skipped).
    Cancel,
    /// Close the runner.
    Close,
}

/// The sequence runner's state.
#[derive(Debug)]
pub struct SequenceRunnerState {
    /// Sequence name (for the title).
    pub name: String,
    /// The sequence file path (reference only).
    pub path: PathBuf,
    /// Failure policy.
    pub on_error: OnError,
    /// Per-step rows in run order.
    pub steps: Vec<StepRow>,
    /// Which step's response is shown on the right.
    pub selected: usize,
    /// The running step index, if any.
    pub current: Option<usize>,
    /// Accumulated extracted values across steps (the run scope).
    pub extracted: BTreeMap<String, String>,
    /// Batch-cancel guard: a landed step result with a stale generation is dropped.
    pub run_generation: u64,
    /// Whether the run has finished (all steps terminal).
    pub finished: bool,
    /// Which sub-pane has focus.
    pub focus: RunnerFocus,
    /// Response viewer cursor/scroll/viewport geometry for the selected step. Same
    /// shape as the main pane's so the shared `response_*` handlers drive it when
    /// the Response region is focused (note #2).
    pub geometry: ResponseGeometry,
    /// Monotonic counter minting a unique generation for each step's
    /// [`ResponseView`], so the shared highlight cache never collides two steps'
    /// viewports (the viewport hash folds in the generation).
    view_seq: u64,
    /// True while awaiting an `esc`-again / `y` confirm to stop a running batch.
    pub confirming_close: bool,
}

impl SequenceRunnerState {
    /// Builds a runner for a sequence's ordered steps. Starts every step pending;
    /// the App kicks off the run.
    pub fn new(name: String, path: PathBuf, on_error: OnError, steps: Vec<SequenceStep>) -> Self {
        let steps = steps.into_iter().map(StepRow::new).collect();
        Self {
            name,
            path,
            on_error,
            steps,
            selected: 0,
            current: None,
            extracted: BTreeMap::new(),
            run_generation: 0,
            finished: false,
            focus: RunnerFocus::Steps,
            geometry: ResponseGeometry::default(),
            view_seq: 0,
            confirming_close: false,
        }
    }

    /// Mints a unique generation for the next step [`ResponseView`] built, so the
    /// shared highlight cache keys each step's viewport distinctly.
    pub fn next_view_gen(&mut self) -> u64 {
        self.view_seq += 1;
        self.view_seq
    }

    /// Whether a run is currently in progress (a step is running or pending after
    /// the current one and not finished).
    pub fn is_running(&self) -> bool {
        self.current.is_some() && !self.finished
    }

    /// Resets every step to pending for a fresh run and clears the accumulator.
    pub fn reset_for_rerun(&mut self) {
        for row in &mut self.steps {
            row.status = StepStatus::Pending;
            row.timing = None;
            row.extracted.clear();
            row.response = ResponseState::Idle;
            row.url.clear();
        }
        self.extracted.clear();
        self.current = None;
        self.finished = false;
        self.selected = 0;
        self.geometry.cursor = 0;
        self.geometry.scroll = 0;
        self.confirming_close = false;
    }

    /// Selects a step, resetting the response viewport.
    fn select(&mut self, index: usize) {
        if index < self.steps.len() {
            self.selected = index;
            self.geometry.cursor = 0;
            self.geometry.scroll = 0;
        }
    }

    /// Routes a key. Returns what (if anything) the App must do.
    pub fn handle_key(&mut self, key: KeyEvent) -> RunnerOutcome {
        // A close-confirm (only shown while running) intercepts first.
        if self.confirming_close {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => return RunnerOutcome::Close,
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.confirming_close = false;
                    return RunnerOutcome::Consumed;
                }
                _ => return RunnerOutcome::Consumed,
            }
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.is_running() {
                    RunnerOutcome::Cancel
                } else {
                    RunnerOutcome::Consumed
                }
            }
            // Close, confirming first if a run is in progress.
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.is_running() {
                    self.confirming_close = true;
                    RunnerOutcome::Consumed
                } else {
                    RunnerOutcome::Close
                }
            }
            KeyCode::Char('r') => RunnerOutcome::Rerun,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    RunnerFocus::Steps => RunnerFocus::Response,
                    RunnerFocus::Response => RunnerFocus::Steps,
                };
                RunnerOutcome::Consumed
            }
            _ => {
                match self.focus {
                    RunnerFocus::Steps => self.handle_steps_key(key),
                    // Response-region keys are routed by `App` through the shared
                    // `response_*` handlers BEFORE this delegate (note #2 — one
                    // code path, full parity with the main pane). Anything that
                    // reaches here in Response focus is not a response action, so
                    // it is a harmless no-op.
                    RunnerFocus::Response => {}
                }
                RunnerOutcome::Consumed
            }
        }
    }

    /// Step-list navigation keys.
    fn handle_steps_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.steps.len() {
                    self.select(self.selected + 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected > 0 {
                    self.select(self.selected - 1);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => {
                self.select(self.steps.len().saturating_sub(1));
            }
            _ => {}
        }
    }

    /// Whether a runner sub-state currently owns the keyboard, so `App` must NOT
    /// route Response actions into the shared handlers (note #2). The sequence
    /// runner's only such state is the running-close confirm (no field editor).
    pub fn response_input_captured(&self) -> bool {
        self.confirming_close
    }

    /// The selected step's response state, for the shared `response_*` handlers
    /// (note #2). `None` when there is no step — the caller falls back to an idle
    /// no-op.
    pub fn selected_response(&self) -> Option<&ResponseState> {
        self.steps.get(self.selected).map(|row| &row.response)
    }

    /// Mutable selected-step response state (see [`Self::selected_response`]).
    pub fn selected_response_mut(&mut self) -> Option<&mut ResponseState> {
        self.steps
            .get_mut(self.selected)
            .map(|row| &mut row.response)
    }
}

/// Masks a secret-named extracted value for display.
fn mask(name: &str, value: &str) -> String {
    if looks_like_secret_name(name) {
        "••••••".to_owned()
    } else {
        value.to_owned()
    }
}

/// The progress summary line, e.g. `1/3 · 1 failed · 2 skipped · 420ms total ·
/// done`. Only steps that actually **ran** count toward `ran/total`; a halted
/// tail is surfaced separately as `N skipped` rather than being folded into
/// "done".
fn progress_line(state: &SequenceRunnerState) -> String {
    let total = state.steps.len();
    let ran = state.steps.iter().filter(|r| r.status.ran()).count();
    let failed = state.steps.iter().filter(|r| r.status.is_failure()).count();
    let skipped = state
        .steps
        .iter()
        .filter(|r| r.status == StepStatus::Skipped)
        .count();
    let total_ms: u128 = state
        .steps
        .iter()
        .filter_map(|r| r.timing.map(|t| t.as_millis()))
        .sum();
    let mut parts = vec![format!("{ran}/{total}")];
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if skipped > 0 {
        parts.push(format!("{skipped} skipped"));
    }
    if total_ms > 0 {
        parts.push(format!("{total_ms}ms total"));
    }
    if state.finished {
        parts.push("done".to_owned());
    }
    parts.join(" · ")
}

/// Renders the runner (the Run face of the unified sequence surface; title
/// carries the `Ctrl-R` edit hint). Returns a [`HighlightJob`] for the selected
/// step's response viewport on a cache miss, for the caller to enqueue into the
/// highlight worker (mirrors the main response pane). `cache` is the app's shared
/// viewport-hash → highlighted-lines cache.
#[must_use]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &mut SequenceRunnerState,
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
    let title = format!(" Sequence · {} ", state.name);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

    // Mode + progress row · one-line purpose hint · body · footer (key hints).
    let [header, hint, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Mode: RUN", theme.title),
            Span::styled(format!("   {}", progress_line(state)), theme.statusline),
            Span::styled(
                format!(
                    "   on_error: {}",
                    match state.on_error {
                        OnError::Halt => "halt",
                        OnError::Continue => "continue",
                    }
                ),
                theme.statusline,
            ),
        ])),
        header,
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Run the chain in order, piping each step's extracted values into the requests that follow.",
            theme.statusline,
        ))),
        hint,
    );

    let left_width = inner.width.saturating_sub(2) / 2;
    let left_width = left_width.clamp(1, 44);
    let [left, right] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Fill(1)]).areas(body);

    render_step_list(frame, left, state, tick_count, theme);
    let job = render_response(frame, right, state, tick_count, cache, theme);
    render_footer(frame, footer, state, theme);

    if state.confirming_close {
        prompt::render_confirm(
            frame,
            modal,
            "Run in progress",
            "Stop the run and close?",
            "y stop · esc keep running",
            theme,
        );
    }
    job
}

/// Renders the left step-list column.
fn render_step_list(
    frame: &mut Frame,
    area: Rect,
    state: &SequenceRunnerState,
    tick_count: u64,
    theme: &Theme,
) {
    let focused = state.focus == RunnerFocus::Steps;
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
        .title(" Steps ")
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in state.steps.iter().enumerate() {
        let selected = i == state.selected;
        let glyph = row.status.glyph(tick_count);
        let glyph_style = match &row.status {
            StepStatus::Ok(_) => theme.response_status,
            StepStatus::Failed(_) | StepStatus::HttpError(_) | StepStatus::ExtractError(_) => {
                theme.status_error
            }
            StepStatus::Running => theme.border_focused,
            _ => theme.statusline,
        };
        let mut spans = vec![
            Span::styled(format!("{glyph} "), glyph_style),
            Span::styled(format!("{} ", row.method), theme.title),
            Span::raw(row.endpoint.clone()),
        ];
        if let Some(detail) = row.status.detail() {
            spans.push(Span::styled(format!("  {detail}"), theme.statusline));
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

        // The selected step's extracted values (masked) under it.
        if selected && !row.extracted.is_empty() {
            for (name, value) in &row.extracted {
                lines.push(Line::from(Span::styled(
                    format!("    ↳ {name} = {}", mask(name, value)),
                    theme.auth_mask,
                )));
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Renders the right response column: the selected step's response in the real
/// viewer, or a status stub for a pending/running step. Returns the highlight job
/// the caller should enqueue (on a cache miss over a `Done` step).
fn render_response(
    frame: &mut Frame,
    area: Rect,
    state: &mut SequenceRunnerState,
    tick_count: u64,
    cache: &HashMap<u64, Vec<Line<'static>>>,
    theme: &Theme,
) -> Option<HighlightJob> {
    let focused = state.focus == RunnerFocus::Response;
    let row = state.steps.get(state.selected)?;
    let outcome = response::render(
        frame,
        area,
        RenderCtx {
            state: &row.response,
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
    // Store clamp geometry for the next key press (mirrors the top-level render).
    state.geometry.apply_render_outcome(&outcome);
    // Write the clamped horizontal scroll back onto the selected view so an
    // over-pan self-corrects on the next frame — mirrors the main pane (M7.7),
    // now reachable in the runner via the shared hscroll parity (note #2).
    if let Some(ResponseState::Done { view }) = state.selected_response_mut() {
        view.set_h_scroll(outcome.clamped_h_scroll);
    }
    outcome.job
}

/// Renders the footer key hints, contextual to focus.
fn render_footer(frame: &mut Frame, area: Rect, state: &SequenceRunnerState, theme: &Theme) {
    let hint = match state.focus {
        RunnerFocus::Steps => {
            "j/k step · tab response · r re-run · ^R edit · ctrl-c cancel · q close"
        }
        RunnerFocus::Response => {
            "j/k scroll · h hdrs · p/s pretty · / search · y copy · tab steps · ^R edit · q close"
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

    fn step(endpoint: &str) -> SequenceStep {
        SequenceStep {
            seq: 0,
            endpoint: endpoint.to_owned(),
            extract: BTreeMap::new(),
            persist: Vec::new(),
        }
    }

    fn runner() -> SequenceRunnerState {
        SequenceRunnerState::new(
            "Flow".to_owned(),
            PathBuf::from("sequences/flow.toml"),
            OnError::Halt,
            vec![step("a.toml"), step("b.toml"), step("c.toml")],
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn j_k_select_steps_clamped() {
        let mut r = runner();
        assert_eq!(r.selected, 0);
        r.handle_key(key(KeyCode::Char('k'))); // clamp at top
        assert_eq!(r.selected, 0);
        r.handle_key(key(KeyCode::Char('j')));
        assert_eq!(r.selected, 1);
        r.handle_key(key(KeyCode::Char('G')));
        assert_eq!(r.selected, 2);
        r.handle_key(key(KeyCode::Char('j'))); // clamp at bottom
        assert_eq!(r.selected, 2);
        r.handle_key(key(KeyCode::Char('g')));
        assert_eq!(r.selected, 0);
    }

    #[test]
    fn tab_toggles_focus() {
        let mut r = runner();
        assert_eq!(r.focus, RunnerFocus::Steps);
        r.handle_key(key(KeyCode::Tab));
        assert_eq!(r.focus, RunnerFocus::Response);
        r.handle_key(key(KeyCode::Tab));
        assert_eq!(r.focus, RunnerFocus::Steps);
    }

    #[test]
    fn q_closes_when_idle() {
        let mut r = runner();
        assert_eq!(r.handle_key(key(KeyCode::Char('q'))), RunnerOutcome::Close);
    }

    #[test]
    fn q_confirms_then_closes_while_running() {
        let mut r = runner();
        r.current = Some(0);
        r.steps[0].status = StepStatus::Running;
        // First q asks to confirm (consumed), not close.
        assert_eq!(
            r.handle_key(key(KeyCode::Char('q'))),
            RunnerOutcome::Consumed
        );
        assert!(r.confirming_close);
        // esc keeps running.
        assert_eq!(r.handle_key(key(KeyCode::Esc)), RunnerOutcome::Consumed);
        assert!(!r.confirming_close);
        // q then y stops + closes.
        r.handle_key(key(KeyCode::Char('q')));
        assert_eq!(r.handle_key(key(KeyCode::Char('y'))), RunnerOutcome::Close);
    }

    #[test]
    fn ctrl_c_cancels_only_while_running() {
        let mut r = runner();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(r.handle_key(ctrl_c), RunnerOutcome::Consumed); // not running
        r.current = Some(0);
        assert_eq!(r.handle_key(ctrl_c), RunnerOutcome::Cancel);
    }

    #[test]
    fn r_requests_rerun() {
        let mut r = runner();
        assert_eq!(r.handle_key(key(KeyCode::Char('r'))), RunnerOutcome::Rerun);
    }

    #[test]
    fn status_maps_from_core_result() {
        assert_eq!(
            StepStatus::from_result(&StepResult::Ok { status: 201 }),
            StepStatus::Ok(201)
        );
        assert_eq!(
            StepStatus::from_result(&StepResult::Failed { status: 503 }),
            StepStatus::Failed(503)
        );
        assert_eq!(
            StepStatus::from_result(&StepResult::Skipped),
            StepStatus::Skipped
        );
    }

    #[test]
    fn secret_named_extracted_value_is_masked() {
        assert_eq!(mask("token", "abc"), "••••••");
        assert_eq!(mask("user_id", "42"), "42");
    }

    #[test]
    fn progress_counts_ran_and_skipped_honestly() {
        let mut r = runner();
        // A halt: step 0 failed (ran), steps 1 & 2 skipped.
        r.steps[0].status = StepStatus::Failed(500);
        r.steps[0].timing = Some(Duration::from_millis(40));
        r.steps[1].status = StepStatus::Skipped;
        r.steps[2].status = StepStatus::Skipped;
        r.finished = true;
        let line = progress_line(&r);
        assert!(line.contains("1/3"), "ran count wrong: {line}");
        assert!(line.contains("1 failed"), "{line}");
        assert!(line.contains("2 skipped"), "{line}");
        assert!(line.contains("done"), "{line}");
        assert!(
            !line.contains("3/3"),
            "skipped must not count as done: {line}"
        );
    }

    #[test]
    fn next_view_gen_is_unique() {
        let mut r = runner();
        let a = r.next_view_gen();
        let b = r.next_view_gen();
        assert_ne!(a, b, "each step view must get a distinct generation");
    }
}
