//! Top-level TUI application: state, key routing, and the async event loop.
//!
//! Key routing precedence (pinned in DECISIONS.md):
//! 1. An open overlay (search/palette) consumes every key.
//! 2. Request pane focused with edtui in a non-Normal mode (insert/visual/…):
//!    all keys go to edtui.
//! 3. Otherwise the crokey keymap is consulted first; unmapped keys fall
//!    through to edtui when the request pane is focused.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Sender as JobSender;
use std::time::{Duration, Instant};

use churl_core::history::{HistoryStore, NewHistoryEntry, default_state_path};
use churl_core::model::{Body, BodyKind, Response};
use churl_core::persistence::{OpenWorkspace, PersistenceError};
use color_eyre::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use edtui::{EditorEventHandler, EditorMode, EditorState, Lines};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::{DefaultTerminal, Frame};
use reqwest::Client;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use super::components::explorer::{ExplorerState, SelectedEndpoint};
use super::components::response::{ResponseMeta, ResponseState, ResponseView};
use super::components::{explorer, palette, picker, request, response, statusline};
use super::events::{Action, FuzzyFinder, KeyMap};
use super::highlight::{self, HighlightJob};

/// Which pane has focus in [`Mode::Normal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The workspace tree on the left.
    Explorer,
    /// The request editor in the centre.
    Request,
    /// The response viewer on the right.
    Response,
}

impl Pane {
    fn next(self) -> Self {
        match self {
            Pane::Explorer => Pane::Request,
            Pane::Request => Pane::Response,
            Pane::Response => Pane::Explorer,
        }
    }

    fn prev(self) -> Self {
        self.next().next()
    }

    fn name(self) -> &'static str {
        match self {
            Pane::Explorer => "EXPLORER",
            Pane::Request => "REQUEST",
            Pane::Response => "RESPONSE",
        }
    }
}

/// Top-level input mode. Overlays own the keyboard; edtui manages its own vim
/// modes internally and is not represented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pane navigation and editing.
    Normal,
    /// The fuzzy endpoint search overlay is open.
    Search,
    /// The command palette overlay is open.
    Palette,
}

/// Messages delivered to the event loop over the app channel. No longer `Copy`
/// since M3: results and highlighted lines carry owned data.
#[derive(Debug)]
pub enum AppMsg {
    /// Request a redraw on the next loop iteration.
    Redraw,
    /// A request completed (or failed). `generation` is matched against the
    /// in-flight generation so stale results (after cancel/resend) are dropped;
    /// the error is stringified at the task boundary to keep core errors out of
    /// the render path.
    Response {
        /// The generation of the request that produced this result.
        generation: u64,
        /// The response, or a stringified error.
        outcome: Result<Response, String>,
        /// Metadata captured at send time.
        meta: ResponseMeta,
    },
    /// Highlighted lines for a viewport, returned by the highlight worker.
    Highlighted {
        /// Viewport hash these lines belong to (the cache key).
        hash: u64,
        /// The highlighted lines.
        lines: Vec<Line<'static>>,
    },
}

/// Bookkeeping for the single in-flight request.
struct InFlightRequest {
    /// Abort handle for the spawned execution task.
    handle: AbortHandle,
    /// The generation this request was issued under.
    generation: u64,
    /// Metadata, reused when writing the history row on cancel.
    meta: ResponseMeta,
}

/// The whole TUI state. Constructible without a tokio runtime so snapshot
/// tests can drive [`render`] through a `TestBackend`.
pub struct App {
    /// The workspace opened from the cwd, if its `churl.toml` exists.
    pub workspace: Option<OpenWorkspace>,
    /// Explorer tree state.
    pub explorer: ExplorerState,
    /// edtui state for the request body.
    pub editor: EditorState,
    editor_events: EditorEventHandler,
    /// The endpoint currently loaded into the request pane.
    pub selected: Option<SelectedEndpoint>,
    /// Focused pane in [`Mode::Normal`].
    pub focus: Pane,
    /// Current top-level input mode.
    pub mode: Mode,
    /// The open overlay's picker, when `mode` is Search or Palette.
    pub picker: Option<picker::PickerState>,
    search_targets: Vec<(usize, usize)>,
    palette_actions: Vec<Action>,
    keymap: KeyMap,
    finder: FuzzyFinder,
    /// Set to exit the event loop.
    pub should_quit: bool,
    /// Sender half of the app channel (cloned into background tasks from M3 on).
    pub tx: mpsc::UnboundedSender<AppMsg>,
    rx: mpsc::UnboundedReceiver<AppMsg>,
    /// The shared reqwest client; `None` in snapshot-test construction (runtime-free).
    pub client: Option<Client>,
    /// The single in-flight request, if any.
    in_flight: Option<InFlightRequest>,
    /// Monotonic request counter; a landed response with a stale generation is dropped.
    generation: u64,
    /// The response pane state.
    pub response: ResponseState,
    /// Response body scroll offset (clamped to the viewport at render time).
    response_scroll: usize,
    /// Last rendered response body height, for half-page scrolling.
    response_viewport_height: usize,
    /// Job sender for the off-thread highlight worker; `None` under `TestBackend`.
    highlight_tx: Option<JobSender<HighlightJob>>,
    /// Viewport-hash → highlighted-lines cache (capped, cleared on new response).
    highlight_cache: HashMap<u64, Vec<Line<'static>>>,
    /// History store; `None` when disabled (open failed or no data dir).
    history: Option<HistoryStore>,
    /// Transient status-line message (send hints, history/errors).
    status: Option<String>,
}

/// The current Unix time in milliseconds (saturating to `0` before the epoch).
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Opens the workspace at `dir`. A missing `churl.toml` yields `None` (empty
/// state); a malformed one is a hard error.
pub fn open_workspace(dir: &Path) -> Result<Option<OpenWorkspace>> {
    match OpenWorkspace::open(dir) {
        Ok(ws) => Ok(Some(ws)),
        Err(PersistenceError::Read { ref source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}

impl App {
    /// Builds the app around an optionally opened workspace and a keymap.
    pub fn new(workspace: Option<OpenWorkspace>, keymap: KeyMap) -> Result<Self> {
        let explorer = ExplorerState::new(workspace.as_ref())?;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Self {
            workspace,
            explorer,
            editor: EditorState::default(),
            editor_events: EditorEventHandler::default(),
            selected: None,
            focus: Pane::Explorer,
            mode: Mode::Normal,
            picker: None,
            search_targets: Vec::new(),
            palette_actions: Vec::new(),
            keymap,
            finder: FuzzyFinder::new(),
            should_quit: false,
            tx,
            rx,
            client: None,
            in_flight: None,
            generation: 0,
            response: ResponseState::Idle,
            response_scroll: 0,
            response_viewport_height: 0,
            highlight_tx: None,
            highlight_cache: HashMap::new(),
            history: None,
            status: None,
        })
    }

    /// Installs the runtime-dependent pieces: the HTTP client, the off-thread
    /// highlight worker, and the history store. Called from [`super::run`] after
    /// [`App::new`]; snapshot tests skip it so they stay runtime-free. A failed
    /// history open is non-fatal — it disables history and warns on the statusline.
    pub fn install_runtime(&mut self) -> Result<()> {
        self.client = Some(churl_core::http::build_client()?);
        self.highlight_tx = Some(highlight::spawn(self.tx.clone()));
        match default_state_path() {
            Some(path) => match HistoryStore::open(&path) {
                Ok(store) => self.history = Some(store),
                Err(err) => self.status = Some(format!("history disabled: {err}")),
            },
            None => self.status = Some("history disabled: no data directory".to_owned()),
        }
        Ok(())
    }

    /// Runs the event loop: `tokio::select!` over the crossterm event stream, a
    /// 250 ms tick, and the app channel. Draws once per iteration; never awaits
    /// I/O other than these three sources.
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        while !self.should_quit {
            terminal.draw(|frame| render(frame, self))?;
            tokio::select! {
                maybe_event = events.next() => match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        self.handle_key(key)?;
                    }
                    Some(Ok(_)) => {} // resize etc. — redraw happens next iteration
                    Some(Err(err)) => return Err(err.into()),
                    None => break, // input stream closed
                },
                _ = tick.tick() => {}
                msg = self.rx.recv() => {
                    if let Some(msg) = msg {
                        self.handle_msg(msg);
                    }
                }
            }
        }
        Ok(())
    }

    /// Routes one key event (see the module docs for the precedence rules).
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        match self.mode {
            Mode::Search | Mode::Palette => self.handle_overlay_key(key),
            Mode::Normal => {
                if self.focus == Pane::Request && self.editor.mode != EditorMode::Normal {
                    self.editor_events.on_key_event(key, &mut self.editor);
                    return Ok(());
                }
                if let Some(action) = self.keymap.lookup(key) {
                    self.dispatch(action, Some(key))
                } else if self.focus == Pane::Request {
                    self.editor_events.on_key_event(key, &mut self.editor);
                    Ok(())
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Executes an action. `key` carries the originating key event so that
    /// navigation actions can fall through to edtui when the request pane is
    /// focused (palette-run actions have no key).
    fn dispatch(&mut self, action: Action, key: Option<KeyEvent>) -> Result<()> {
        match action {
            Action::Quit => {
                // Ctrl-C cancels an in-flight request instead of quitting (the
                // "Ctrl-C in request context" behaviour); `q`/`Esc` always quit.
                let is_ctrl_c = key.is_some_and(|key| {
                    matches!(key.code, KeyCode::Char('c'))
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                });
                if is_ctrl_c && self.in_flight.is_some() {
                    self.cancel_request();
                } else {
                    self.should_quit = true;
                }
            }
            Action::FocusNext => self.focus = self.focus.next(),
            Action::FocusPrev => self.focus = self.focus.prev(),
            Action::FocusExplorer => self.focus = Pane::Explorer,
            Action::FocusRequest => self.focus = Pane::Request,
            Action::FocusResponse => self.focus = Pane::Response,
            Action::OpenSearch => self.open_search()?,
            Action::OpenPalette => self.open_palette(),
            Action::Send => self.send_request(),
            Action::Cancel => self.cancel_request(),
            Action::HalfPageDown | Action::HalfPageUp => {
                if self.focus == Pane::Response {
                    self.response_half_page(matches!(action, Action::HalfPageDown));
                }
            }
            Action::Up
            | Action::Down
            | Action::Select
            | Action::Collapse
            | Action::Expand
            | Action::Top
            | Action::Bottom => match self.focus {
                Pane::Explorer => self.explorer_action(action)?,
                Pane::Request => {
                    // Same keys are edtui's normal-mode motions: forward them.
                    if let Some(key) = key {
                        self.editor_events.on_key_event(key, &mut self.editor);
                    }
                }
                Pane::Response => self.response_scroll(action),
            },
        }
        Ok(())
    }

    fn explorer_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Up => self.explorer.move_up(),
            Action::Down => self.explorer.move_down(),
            Action::Top => self.explorer.top(),
            Action::Bottom => self.explorer.bottom(),
            Action::Collapse => self.explorer.collapse(),
            Action::Expand => self.explorer.expand()?,
            Action::Select => {
                if let Some(selected) = self.explorer.select()? {
                    self.load_endpoint(selected);
                }
            }
            _ => unreachable!("only navigation actions reach explorer_action"),
        }
        Ok(())
    }

    /// Loads an endpoint into the request pane: body into the edtui buffer,
    /// metadata for read-only display.
    fn load_endpoint(&mut self, selected: SelectedEndpoint) {
        let body = selected
            .endpoint
            .request
            .body
            .as_ref()
            .map(|body| body.content.as_str())
            .unwrap_or("");
        self.editor = EditorState::new(Lines::from(body));
        self.selected = Some(selected);
    }

    /// Sends the selected endpoint's request with the live edtui body text.
    /// Spawns the execution task, keeps its `AbortHandle`, and moves the response
    /// pane to the in-flight state. Ignored (with a statusline hint) when a
    /// request is already in flight or no endpoint is selected.
    fn send_request(&mut self) {
        if self.in_flight.is_some() {
            self.status = Some("request already in flight — ctrl-c to cancel".to_owned());
            return;
        }
        let Some(selected) = self.selected.as_ref() else {
            self.status = Some("no endpoint selected — nothing to send".to_owned());
            return;
        };
        // No client means runtime-free construction (snapshot tests); do nothing.
        let Some(client) = self.client.clone() else {
            return;
        };

        let mut request = selected.endpoint.request.clone();
        let body_text = String::from(self.editor.lines.clone());
        match request.body.as_mut() {
            Some(body) => body.content = body_text,
            None if !body_text.is_empty() => {
                request.body = Some(Body {
                    kind: BodyKind::Text,
                    content: body_text,
                });
            }
            None => {}
        }

        self.generation += 1;
        let generation = self.generation;
        let started = Instant::now();
        let meta = ResponseMeta {
            method: request.method.to_string(),
            url: request.url.clone(),
            endpoint_path: self.endpoint_rel_path(selected),
            executed_at_ms: now_ms(),
        };

        let tx = self.tx.clone();
        let task_meta = meta.clone();
        let handle = tokio::spawn(async move {
            let outcome = churl_core::http::execute(&client, &request)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(AppMsg::Response {
                generation,
                outcome,
                meta: task_meta,
            });
        });

        self.in_flight = Some(InFlightRequest {
            handle: handle.abort_handle(),
            generation,
            meta,
        });
        self.response = ResponseState::InFlight { started };
        self.response_scroll = 0;
        self.highlight_cache.clear();
        self.status = None;
    }

    /// Cancels the in-flight request: aborts the task, records a history row with
    /// no status, and moves the pane to the cancelled state.
    fn cancel_request(&mut self) {
        let Some(in_flight) = self.in_flight.take() else {
            self.status = Some("no request in flight".to_owned());
            return;
        };
        in_flight.handle.abort();
        self.write_history(&in_flight.meta, None, None);
        self.response = ResponseState::Cancelled;
        self.status = Some("request cancelled".to_owned());
    }

    /// The workspace-relative path of a selected endpoint's file, if inside the
    /// open workspace.
    fn endpoint_rel_path(&self, selected: &SelectedEndpoint) -> Option<String> {
        let root = self.workspace.as_ref()?.root();
        selected
            .file
            .strip_prefix(root)
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    }

    /// The coarse maximum scroll offset (line count minus one); render clamps
    /// further to the true last screenful.
    fn response_max_scroll(&self) -> usize {
        match &self.response {
            ResponseState::Done { view } => view.line_count().saturating_sub(1),
            _ => 0,
        }
    }

    /// Moves the response body scroll offset for a navigation action.
    fn response_scroll(&mut self, action: Action) {
        let max = self.response_max_scroll();
        match action {
            Action::Up => self.response_scroll = self.response_scroll.saturating_sub(1),
            Action::Down => self.response_scroll = (self.response_scroll + 1).min(max),
            Action::Top => self.response_scroll = 0,
            Action::Bottom => self.response_scroll = max,
            _ => {}
        }
    }

    /// Scrolls the response body by half a viewport.
    fn response_half_page(&mut self, down: bool) {
        let half = (self.response_viewport_height / 2).max(1);
        let max = self.response_max_scroll();
        self.response_scroll = if down {
            (self.response_scroll + half).min(max)
        } else {
            self.response_scroll.saturating_sub(half)
        };
    }

    /// Dispatches a channel message.
    fn handle_msg(&mut self, msg: AppMsg) {
        match msg {
            AppMsg::Redraw => {}
            AppMsg::Response {
                generation,
                outcome,
                meta,
            } => self.on_response(generation, outcome, meta),
            AppMsg::Highlighted { hash, lines } => self.cache_highlighted(hash, lines),
        }
    }

    /// Applies an arrived response, dropping it if its generation is stale (the
    /// request was cancelled or superseded by a newer send).
    fn on_response(
        &mut self,
        generation: u64,
        outcome: Result<Response, String>,
        meta: ResponseMeta,
    ) {
        if self.in_flight.as_ref().map(|f| f.generation) != Some(generation) {
            return; // stale — drop
        }
        self.in_flight = None;
        self.highlight_cache.clear();
        self.response_scroll = 0;
        match outcome {
            Ok(response) => {
                self.write_history(&meta, Some(response.status), Some(response.timing.total));
                self.response = ResponseState::Done {
                    view: ResponseView::build(&response, generation),
                };
            }
            Err(error) => {
                self.write_history(&meta, None, None);
                self.response = ResponseState::Failed { error, meta };
            }
        }
    }

    /// Stores highlighted viewport lines, capping the cache so long scrolls do
    /// not grow it unbounded.
    fn cache_highlighted(&mut self, hash: u64, lines: Vec<Line<'static>>) {
        if self.highlight_cache.len() >= 64 {
            self.highlight_cache.clear();
        }
        self.highlight_cache.insert(hash, lines);
    }

    /// Inserts a history row for a terminal outcome. Insert failure warns on the
    /// statusline but never crashes.
    fn write_history(
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
        if let Some(Err(err)) = self.history.as_ref().map(|store| store.insert(&entry)) {
            self.status = Some(format!("history write failed: {err}"));
        }
    }

    fn open_search(&mut self) -> Result<()> {
        let items = super::components::search::open(&mut self.explorer)?;
        self.picker = Some(items.picker);
        self.search_targets = items.targets;
        self.mode = Mode::Search;
        Ok(())
    }

    fn open_palette(&mut self) {
        let items = palette::open();
        self.picker = Some(items.picker);
        self.palette_actions = items.actions;
        self.mode = Mode::Palette;
    }

    fn close_overlay(&mut self) {
        self.picker = None;
        self.mode = Mode::Normal;
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(picker) = &mut self.picker else {
            self.close_overlay();
            return Ok(());
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.close_overlay(),
            KeyCode::Enter => return self.accept_overlay(),
            KeyCode::Up => picker.move_up(),
            KeyCode::Down => picker.move_down(),
            KeyCode::Char('p') if ctrl => picker.move_up(),
            KeyCode::Char('n') if ctrl => picker.move_down(),
            KeyCode::Backspace => picker.backspace(&mut self.finder),
            KeyCode::Char(c) if !ctrl => picker.push_char(c, &mut self.finder),
            _ => {}
        }
        Ok(())
    }

    fn accept_overlay(&mut self) -> Result<()> {
        let mode = self.mode;
        let current = self.picker.as_ref().and_then(picker::PickerState::current);
        self.close_overlay();
        let Some(index) = current else {
            return Ok(());
        };
        match mode {
            Mode::Search => {
                if let Some(&(collection, endpoint)) = self.search_targets.get(index) {
                    self.focus = Pane::Explorer;
                    if let Some(selected) = self.explorer.jump_to(collection, endpoint)? {
                        self.load_endpoint(selected);
                    }
                }
            }
            Mode::Palette => {
                if let Some(&action) = self.palette_actions.get(index) {
                    self.dispatch(action, None)?;
                }
            }
            Mode::Normal => {}
        }
        Ok(())
    }
}

/// Renders the whole UI: three panes, status bar, and any open overlay.
/// Pure (no I/O) and deterministic, so `TestBackend` snapshots stay stable.
pub fn render(frame: &mut Frame, app: &mut App) {
    let [main, status] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let [explorer_area, request_area, response_area] = Layout::horizontal([
        Constraint::Min(24),
        Constraint::Percentage(40),
        Constraint::Percentage(35),
    ])
    .areas(main);

    let has_ws = app.workspace.is_some();
    let explorer_focused = app.focus == Pane::Explorer && app.mode == Mode::Normal;
    explorer::render(
        frame,
        explorer_area,
        &mut app.explorer,
        explorer_focused,
        has_ws,
    );
    let selected_request = app
        .selected
        .as_ref()
        .map(|selected| &selected.endpoint.request);
    request::render(
        frame,
        request_area,
        selected_request,
        &mut app.editor,
        app.focus == Pane::Request && app.mode == Mode::Normal,
    );
    let outcome = response::render(
        frame,
        response_area,
        response::RenderCtx {
            state: &app.response,
            request: selected_request,
            focused: app.focus == Pane::Response && app.mode == Mode::Normal,
            scroll: app.response_scroll,
            cache: &app.highlight_cache,
        },
    );
    app.response_scroll = outcome.clamped_scroll;
    app.response_viewport_height = outcome.viewport_height;
    if let (Some(job), Some(tx)) = (outcome.job, &app.highlight_tx) {
        let _ = tx.send(job);
    }
    statusline::render(
        frame,
        status,
        app.focus.name(),
        app.workspace.as_ref().map(|ws| ws.manifest().name.as_str()),
        app.status.as_deref(),
    );

    if let Some(picker_state) = &app.picker {
        picker::render(frame, main, picker_state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use churl_core::model::Timing;

    fn meta() -> ResponseMeta {
        ResponseMeta {
            method: "GET".to_owned(),
            url: "https://api.test/x".to_owned(),
            endpoint_path: Some("users/get.toml".to_owned()),
            executed_at_ms: 1,
        }
    }

    fn response() -> Response {
        Response {
            status: 200,
            headers: Vec::new(),
            body: b"{}".to_vec(),
            timing: Timing {
                connect: None,
                total: Duration::from_millis(3),
            },
        }
    }

    /// A response whose generation no longer matches the in-flight request (after
    /// a cancel+resend) must be dropped without touching the pane.
    #[tokio::test]
    async fn stale_generation_response_is_dropped() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        // Pretend a request at generation 5 is in flight.
        let handle = tokio::spawn(async {});
        app.generation = 5;
        app.in_flight = Some(InFlightRequest {
            handle: handle.abort_handle(),
            generation: 5,
            meta: meta(),
        });

        // A late result from an older generation is ignored…
        app.on_response(4, Ok(response()), meta());
        assert!(matches!(app.response, ResponseState::Idle));
        assert!(app.in_flight.is_some(), "in-flight preserved on stale drop");

        // …the matching generation lands and clears the in-flight slot.
        app.on_response(5, Ok(response()), meta());
        assert!(matches!(app.response, ResponseState::Done { .. }));
        assert!(app.in_flight.is_none());
    }
}
