//! Top-level TUI application: state, key routing, and the async event loop.
//!
//! Key routing precedence (pinned in DECISIONS.md):
//! 1. An open overlay (search/palette) consumes every key.
//! 2. Request pane focused with edtui in a non-Normal mode (insert/visual/…):
//!    all keys go to edtui — except a CONTROL-modified key that the keymap
//!    resolves to Send or Quit, which is dispatched instead (the single
//!    documented exception: Ctrl-S/Ctrl-C are not text-input keys, and the
//!    lookup goes through the keymap so user remaps are honoured).
//! 3. Otherwise the crokey keymap is consulted first; unmapped keys fall
//!    through to edtui when the request pane is focused.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::mpsc::Sender as JobSender;
use std::time::{Duration, Instant};

use churl_core::config::Config;
use churl_core::history::{HistoryStore, NewHistoryEntry, default_state_path};
use churl_core::http::ExecuteOptions;
use churl_core::model::{Body, BodyKind, Response};
use churl_core::persistence::{OpenWorkspace, PersistenceError};
use churl_core::template::{Resolver, Scope};
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
use super::components::jump::{JumpState, JumpTarget};
use super::components::response::{ResponseMeta, ResponseState, ResponseView};
use super::components::{explorer, palette, picker, request, response, statusline, urlbar};
use super::events::{Action, FuzzyFinder, KeyMap};
use super::highlight::{self, HighlightJob};
use super::theme::Theme;

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
    /// Jump-mode: label-driven pane/row navigation overlay.
    Jump,
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

/// A transient status-line message that auto-expires after ~4 s.
struct TransientStatus {
    message: String,
    set_at: Instant,
}

impl TransientStatus {
    const EXPIRE_SECS: u64 = 4;

    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            set_at: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.set_at.elapsed().as_secs() >= Self::EXPIRE_SECS
    }
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
    /// Profile-name items behind an open profile picker (index-aligned;
    /// `None` marks the "(none)" entry). Non-empty only while switching profiles.
    profile_choices: Vec<Option<String>>,
    /// Active jump-mode state, when `mode` is [`Mode::Jump`].
    pub jump: Option<JumpState>,
    keymap: KeyMap,
    /// The resolved colour theme, threaded through every render fn.
    pub theme: Theme,
    /// `--var key=value` overrides: the highest-precedence resolver scope.
    cli_vars: BTreeMap<String, String>,
    /// The active profile name, if any (`--profile` or the SwitchProfile picker).
    pub active_profile: Option<String>,
    finder: FuzzyFinder,
    /// Set to exit the event loop.
    pub should_quit: bool,
    /// Sender half of the app channel (cloned into background tasks from M3 on).
    pub tx: mpsc::UnboundedSender<AppMsg>,
    rx: mpsc::UnboundedReceiver<AppMsg>,
    /// The shared reqwest client; `None` in snapshot-test construction (runtime-free).
    pub client: Option<Client>,
    /// Per-execution knobs (body-size cap) resolved from config in
    /// [`App::install_runtime`]; defaults under snapshot-test construction.
    execute_options: ExecuteOptions,
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
    /// Transient status-line message (send hints, history/errors); auto-expires.
    status: Option<TransientStatus>,
    /// Monotonic tick counter (incremented every 250 ms tick); drives the spinner
    /// animation in the response pane. `pub` so snapshot tests can set it.
    pub tick_count: u64,
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
    /// Builds the app around an optionally opened workspace and a keymap, with the
    /// default theme, no CLI vars, and no active profile. Snapshot tests use this;
    /// [`App::with_config`] wires the theme/profile/vars from config + CLI.
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
            profile_choices: Vec::new(),
            jump: None,
            keymap,
            theme: Theme::default(),
            cli_vars: BTreeMap::new(),
            active_profile: None,
            finder: FuzzyFinder::new(),
            should_quit: false,
            tx,
            rx,
            client: None,
            execute_options: ExecuteOptions::default(),
            in_flight: None,
            generation: 0,
            response: ResponseState::Idle,
            response_scroll: 0,
            response_viewport_height: 0,
            highlight_tx: None,
            highlight_cache: HashMap::new(),
            history: None,
            status: None,
            tick_count: 0,
        })
    }

    /// Builds the app from a workspace, keymap, resolved theme, CLI `--var`
    /// overrides, and an optional `--profile`. An unknown profile name is a hard
    /// error listing the available profiles (fail loudly — a typo'd profile would
    /// silently send with the wrong environment otherwise).
    pub fn with_config(
        workspace: Option<OpenWorkspace>,
        keymap: KeyMap,
        theme: Theme,
        cli_vars: BTreeMap<String, String>,
        profile: Option<String>,
    ) -> Result<Self> {
        if let Some(name) = &profile {
            let available: Vec<&str> = workspace
                .as_ref()
                .map(|ws| {
                    ws.manifest()
                        .profiles
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect()
                })
                .unwrap_or_default();
            if !available.contains(&name.as_str()) {
                return Err(color_eyre::eyre::eyre!(
                    "unknown profile {name:?} (available: {})",
                    if available.is_empty() {
                        "none".to_owned()
                    } else {
                        available.join(", ")
                    }
                ));
            }
        }
        let mut app = Self::new(workspace, keymap)?;
        app.theme = theme;
        app.cli_vars = cli_vars;
        app.active_profile = profile;
        Ok(app)
    }

    /// The variables of the active profile, empty when none is set or it has no
    /// vars (an already-validated profile name that no longer resolves also
    /// yields empty).
    fn profile_vars(&self) -> BTreeMap<String, String> {
        let Some(name) = &self.active_profile else {
            return BTreeMap::new();
        };
        self.workspace
            .as_ref()
            .and_then(|ws| ws.manifest().profiles.iter().find(|p| &p.name == name))
            .map(|p| p.vars.clone())
            .unwrap_or_default()
    }

    /// The workspace-level `[vars]`, empty when there is no workspace.
    fn workspace_vars(&self) -> BTreeMap<String, String> {
        self.workspace
            .as_ref()
            .map(|ws| ws.manifest().vars.clone())
            .unwrap_or_default()
    }

    /// Builds the template [`Resolver`] for a send of `selected`, in precedence
    /// order: cli `--var` → active profile → the endpoint's collection
    /// `folder.toml` vars → workspace `[vars]` → process env (implicit).
    fn build_resolver(&mut self, selected: &SelectedEndpoint) -> Resolver {
        let collection_vars = self.explorer.collection_vars(selected.collection);
        Resolver::new(vec![
            Scope::new("cli", self.cli_vars.clone()),
            Scope::new("profile", self.profile_vars()),
            Scope::new("collection", collection_vars),
            Scope::new("workspace", self.workspace_vars()),
        ])
    }

    /// Installs the runtime-dependent pieces: the HTTP client (with the
    /// config-resolved timeout), the execution options (body-size cap), the
    /// off-thread highlight worker, and the history store. Called from
    /// [`super::run`] after [`App::new`]; snapshot tests skip it so they stay
    /// runtime-free. A failed history open is non-fatal — it disables history
    /// and warns on the statusline.
    pub fn install_runtime(&mut self, config: &Config) -> Result<()> {
        self.client = Some(churl_core::http::build_client(config.timeout())?);
        self.execute_options = ExecuteOptions {
            max_body_bytes: config.max_body_bytes(),
        };
        self.highlight_tx = Some(highlight::spawn(self.tx.clone(), self.theme.is_light()));
        match default_state_path() {
            Some(path) => match HistoryStore::open(&path) {
                Ok(store) => self.history = Some(store),
                Err(err) => {
                    self.status = Some(TransientStatus::new(format!("history disabled: {err}")));
                }
            },
            None => {
                self.status = Some(TransientStatus::new("history disabled: no data directory"));
            }
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
                _ = tick.tick() => {
                    self.tick_count = self.tick_count.wrapping_add(1);
                    // Expire transient status messages after ~4 s.
                    if self.status.as_ref().is_some_and(|s| s.is_expired()) {
                        self.status = None;
                    }
                }
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
            Mode::Jump => {
                self.handle_jump_key(key);
                Ok(())
            }
            Mode::Normal => {
                if self.focus == Pane::Request && self.editor.mode != EditorMode::Normal {
                    // The single exception to "edtui owns non-Normal modes"
                    // (DECISIONS.md): a CONTROL-modified key that the keymap
                    // resolves to Send or Quit is dispatched instead of being
                    // handed to edtui, so Ctrl-S sends and Ctrl-C cancels/quits
                    // straight from insert mode. Resolving through the keymap
                    // honours user remaps; requiring CONTROL guarantees no
                    // text-input key is ever stolen.
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && let Some(action @ (Action::Send | Action::Quit)) =
                            self.keymap.lookup(key)
                    {
                        return self.dispatch(action, Some(key));
                    }
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
            Action::Jump => self.open_jump(),
            Action::SwitchProfile => self.open_profile_picker(),
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
            self.status = Some(TransientStatus::new(
                "request already in flight — ctrl-c to cancel",
            ));
            return;
        }
        let Some(selected) = self.selected.clone() else {
            self.status = Some(TransientStatus::new(
                "no endpoint selected — nothing to send",
            ));
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

        // Resolve `{{var}}` placeholders on the cloned request only — the seam is
        // `substitute_request`; resolved values are never written to disk (this
        // clone is discarded after the send). `execute()` stays substitution-free.
        self.build_resolver(&selected)
            .substitute_request(&mut request);

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
        let handle = tokio::spawn(async move {
            let outcome = churl_core::http::execute(&client, &request, &options)
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
            self.status = Some(TransientStatus::new("no request in flight"));
            return;
        };
        in_flight.handle.abort();
        self.write_history(&in_flight.meta, None, None);
        self.response = ResponseState::Cancelled;
        self.status = Some(TransientStatus::new("request cancelled"));
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
            self.status = Some(TransientStatus::new(format!("history write failed: {err}")));
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

    /// Enters jump-mode, labelling the three panes and every visible explorer row.
    fn open_jump(&mut self) {
        // Label from the first visible row (scroll offset), not the top of the
        // tree — in a scrolled explorer the offscreen top rows must not eat the
        // label alphabet while the viewport goes unlabelled.
        let first_row = self.explorer.first_visible();
        let row_count = self.explorer.rows().len();
        self.jump = Some(JumpState::new(first_row, row_count));
        self.mode = Mode::Jump;
    }

    /// Cancels jump-mode, returning to normal navigation.
    fn close_jump(&mut self) {
        self.jump = None;
        self.mode = Mode::Normal;
    }

    /// Handles one key in jump-mode: a label char jumps (and cancels the mode);
    /// `Esc` or the Jump key again cancels; everything else is ignored (jump-mode
    /// consumes all keys).
    fn handle_jump_key(&mut self, key: KeyEvent) {
        // Esc always cancels.
        if key.code == KeyCode::Esc {
            self.close_jump();
            return;
        }
        let Some(jump) = &self.jump else {
            self.close_jump();
            return;
        };
        // An assigned label wins first — the default Jump key `f` also labels the
        // first explorer row, so a label lookup must take precedence over the
        // "Jump key again cancels" rule, or that target would be unreachable.
        if let KeyCode::Char(c) = key.code
            && let Some(target) = jump.target_for(c)
        {
            self.close_jump();
            match target {
                JumpTarget::Pane(pane) => self.focus = pane,
                JumpTarget::Row(row) => {
                    self.focus = Pane::Explorer;
                    self.explorer.cursor = row;
                    // An endpoint row also selects it (same as Enter).
                    if let Ok(Some(selected)) = self.explorer.select() {
                        self.load_endpoint(selected);
                    }
                }
            }
            return;
        }
        // Pressing the Jump key again cancels (when it labels no target).
        if self.keymap.lookup(key) == Some(Action::Jump) {
            self.close_jump();
        }
        // Any other non-label key is ignored; jump-mode stays open.
    }

    /// Opens the profile picker over the workspace's profile names plus a
    /// "(none)" entry; the active entry is prefixed with `● ` (display-only —
    /// `profile_choices` carries raw names so filtering by the profile name still
    /// works, since `● dev` contains `dev`).
    fn open_profile_picker(&mut self) {
        let active = self.active_profile.as_deref();
        let mut choices: Vec<Option<String>> = vec![None];
        // Prefix "(none)" with ● when no profile is active.
        let none_label = if active.is_none() {
            "● (none)".to_owned()
        } else {
            "(none)".to_owned()
        };
        let mut labels: Vec<String> = vec![none_label];
        if let Some(ws) = &self.workspace {
            for profile in &ws.manifest().profiles {
                choices.push(Some(profile.name.clone()));
                // Prefix the active profile entry with ●.
                let label = if active == Some(profile.name.as_str()) {
                    format!("● {}", profile.name)
                } else {
                    profile.name.clone()
                };
                labels.push(label);
            }
        }
        self.profile_choices = choices;
        self.picker = Some(picker::PickerState::new(" Switch profile ", labels));
        self.mode = Mode::Palette;
    }

    fn close_overlay(&mut self) {
        self.picker = None;
        self.profile_choices.clear();
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
        // Capture the profile choices before close_overlay() clears them.
        let profile_choices = std::mem::take(&mut self.profile_choices);
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
                // A profile picker (profile_choices set) resolves a profile;
                // otherwise it is the command palette resolving an action.
                if !profile_choices.is_empty() {
                    if let Some(choice) = profile_choices.get(index).cloned() {
                        self.set_profile(choice);
                    }
                } else if let Some(&action) = self.palette_actions.get(index) {
                    self.dispatch(action, None)?;
                }
            }
            Mode::Jump | Mode::Normal => {}
        }
        Ok(())
    }

    /// Sets (or clears with `None`) the active profile. No status message —
    /// the persistent `profile: <name>` indicator in the statusline is the
    /// single source of truth (M6.5 dedup fix).
    fn set_profile(&mut self, profile: Option<String>) {
        self.active_profile = profile;
    }
}

/// Renders the whole UI:
/// - Explorer (left column)
/// - Column B (right): URL bar (slim, display-only) / Request (top half) / Response (bottom half)
/// - Status bar (bottom, 1 line)
/// - Any open overlay
///
/// Pure (no I/O) and deterministic — `TestBackend` snapshots stay stable.
pub fn render(frame: &mut Frame, app: &mut App) {
    let [main, status] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    // Two-column layout: Explorer left, column B right. The explorer is a
    // *narrow* column (owner prompt): fixed 30 cols — Min(24)+Fill would grow
    // the explorer to half the screen (ratatui distributes excess into Min).
    let [explorer_area, right_area] =
        Layout::horizontal([Constraint::Length(30), Constraint::Fill(1)]).areas(main);
    // Column B split into three rows: URL bar (3 lines) / Request (50%) / Response (50%).
    let remaining = right_area.height.saturating_sub(urlbar::HEIGHT);
    let req_height = remaining / 2;
    let resp_height = remaining - req_height;
    let [urlbar_area, request_area, response_area] = Layout::vertical([
        Constraint::Length(urlbar::HEIGHT),
        Constraint::Length(req_height),
        Constraint::Length(resp_height),
    ])
    .areas(right_area);

    let has_ws = app.workspace.is_some();
    let explorer_focused = app.focus == Pane::Explorer && app.mode == Mode::Normal;
    let theme = app.theme.clone();
    explorer::render(
        frame,
        explorer_area,
        &mut app.explorer,
        explorer_focused,
        has_ws,
        &theme,
        app.jump.as_ref(),
    );
    let selected_request = app
        .selected
        .as_ref()
        .map(|selected| &selected.endpoint.request);
    urlbar::render(frame, urlbar_area, selected_request, &theme);
    request::render(
        frame,
        request_area,
        selected_request,
        &mut app.editor,
        app.focus == Pane::Request && app.mode == Mode::Normal,
        &theme,
        app.jump.as_ref(),
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
            theme: &theme,
            jump_label: app
                .jump
                .as_ref()
                .and_then(|j| j.label_for_pane(Pane::Response)),
            tick_count: app.tick_count,
        },
    );
    app.response_scroll = outcome.clamped_scroll;
    app.response_viewport_height = outcome.viewport_height;
    if let (Some(job), Some(tx)) = (outcome.job, &app.highlight_tx) {
        let _ = tx.send(job);
    }
    // While a request is in flight the statusline derives "sending…" from state —
    // not from the transient `status` field — so it appears/disappears instantly.
    let in_flight_msg = app.in_flight.as_ref().map(|_| "sending… (ctrl-c cancels)");
    statusline::render(
        frame,
        status,
        statusline::StatusCtx {
            focus: app.focus.name(),
            workspace: app.workspace.as_ref().map(|ws| ws.manifest().name.as_str()),
            profile: app.active_profile.as_deref(),
            message: in_flight_msg.or_else(|| app.status.as_ref().map(|s| s.message.as_str())),
            theme: &theme,
        },
    );

    if let Some(picker_state) = &app.picker {
        picker::render(frame, main, picker_state, &theme);
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
            truncated: false,
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

    /// Puts a fresh app in insert mode on the request pane.
    fn insert_mode_app(keymap: KeyMap) -> App {
        let mut app = App::new(None, keymap).unwrap();
        app.focus = Pane::Request;
        app.editor.mode = EditorMode::Insert;
        app
    }

    /// Insert mode + Ctrl-S dispatches Send (here: the no-endpoint statusline
    /// hint) instead of reaching edtui; the editor stays in insert mode.
    #[test]
    fn insert_mode_ctrl_s_dispatches_send() {
        let mut app = insert_mode_app(KeyMap::default());
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(
            app.status.as_ref().map(|s| s.message.as_str()),
            Some("no endpoint selected — nothing to send")
        );
        assert_eq!(String::from(app.editor.lines.clone()), "");
        assert_eq!(app.editor.mode, EditorMode::Insert);
        assert!(!app.should_quit);
    }

    /// Insert mode + Ctrl-C with a request in flight cancels it (never quits,
    /// never reaches edtui).
    #[tokio::test]
    async fn insert_mode_ctrl_c_cancels_in_flight() {
        let mut app = insert_mode_app(KeyMap::default());
        let handle = tokio::spawn(async {});
        app.generation = 1;
        app.in_flight = Some(InFlightRequest {
            handle: handle.abort_handle(),
            generation: 1,
            meta: meta(),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(app.in_flight.is_none());
        assert!(matches!(app.response, ResponseState::Cancelled));
        assert!(!app.should_quit);
        assert_eq!(String::from(app.editor.lines.clone()), "");
    }

    /// Insert mode + Ctrl-C with nothing in flight falls back to Quit
    /// (the Ctrl-C-in-request-context behaviour, unchanged from Normal mode).
    #[test]
    fn insert_mode_ctrl_c_without_in_flight_quits() {
        let mut app = insert_mode_app(KeyMap::default());
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(app.should_quit);
    }

    /// Plain `s`/`c` in insert mode are text input and still reach edtui.
    #[test]
    fn insert_mode_plain_chars_reach_edtui() {
        let mut app = insert_mode_app(KeyMap::default());
        for c in ['s', 'c'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(String::from(app.editor.lines.clone()), "sc");
        assert!(!app.should_quit);
        assert!(app.status.is_none());
    }

    /// The interception resolves through the keymap: a remapped send key with
    /// CONTROL is intercepted in insert mode too.
    #[test]
    fn insert_mode_remapped_ctrl_send_is_intercepted() {
        let overrides =
            std::collections::BTreeMap::from([("ctrl-b".to_string(), "send".to_string())]);
        let mut app = insert_mode_app(KeyMap::with_overrides(&overrides).unwrap());
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(
            app.status.as_ref().map(|s| s.message.as_str()),
            Some("no endpoint selected — nothing to send")
        );
        assert_eq!(String::from(app.editor.lines.clone()), "");
    }

    /// Builds a minimal workspace with one collection + endpoint and two profiles.
    fn workspace_fixture(root: &Path) -> App {
        std::fs::write(
            root.join("churl.toml"),
            "name = \"demo\"\n\n[vars]\nbase = \"ws\"\n\n[[profiles]]\nname = \"dev\"\n[profiles.vars]\nhost = \"dev.test\"\n\n[[profiles]]\nname = \"prod\"\n[profiles.vars]\nhost = \"prod.test\"\n",
        )
        .unwrap();
        let coll = root.join("users");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("get.toml"),
            "seq = 0\nname = \"Get user\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://{{host}}/users\"\n",
        )
        .unwrap();
        let ws = open_workspace(root).unwrap();
        App::new(ws, KeyMap::default()).unwrap()
    }

    /// Pressing `f` enters jump-mode; a pane label focuses that pane and exits.
    #[test]
    fn jump_label_focuses_pane() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Jump);
        assert!(app.jump.is_some());
        // 'd' is the Response pane label (a/s/d for the three panes).
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.jump.is_none());
        assert_eq!(app.focus, Pane::Response);
    }

    /// A row label focuses the explorer, moves the cursor, and selects an endpoint.
    #[test]
    fn jump_row_label_selects_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        // Expand the collection so a leaf row is visible and labellable.
        app.explorer.expand().unwrap();
        assert_eq!(app.explorer.rows().len(), 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        // Rows follow the three pane labels a/s/d → row 0 is 'f', row 1 is 'g'.
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(app.explorer.cursor, 1);
        // The endpoint row was selected and loaded.
        assert!(app.selected.is_some());
        assert_eq!(app.selected.as_ref().unwrap().endpoint.name, "Get user");
    }

    /// The default Jump key `f` also labels the first explorer row: the label
    /// wins over the "Jump key again cancels" rule so that target stays reachable.
    #[test]
    fn jump_f_selects_first_row_not_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Jump);
        // Row 0 is the "users" collection row → its label is `f`; pressing it
        // focuses the explorer and toggles (expands) that collection.
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal, "f must act as a label, not cancel");
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(app.explorer.cursor, 0);
        assert_eq!(app.explorer.rows().len(), 2, "collection expanded");
    }

    /// Esc cancels jump-mode without focusing anything.
    #[test]
    fn jump_esc_cancels() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        let before = app.focus;
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.jump.is_none());
        assert_eq!(app.focus, before);
    }

    /// A non-label char in jump-mode is ignored; the mode stays open.
    #[test]
    fn jump_ignores_unknown_char() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Jump, "unknown char must not exit jump-mode");
    }

    /// `with_config` rejects an unknown profile name, listing the available ones.
    #[test]
    fn unknown_profile_is_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("churl.toml"),
            "name = \"demo\"\n\n[[profiles]]\nname = \"dev\"\n",
        )
        .unwrap();
        let ws = open_workspace(dir.path()).unwrap();
        let result = App::with_config(
            ws,
            KeyMap::default(),
            Theme::default(),
            BTreeMap::new(),
            Some("staging".to_owned()),
        );
        let msg = match result {
            Ok(_) => panic!("expected unknown-profile error"),
            Err(err) => err.to_string(),
        };
        assert!(msg.contains("staging"), "{msg}");
        assert!(msg.contains("dev"), "must list available: {msg}");
    }

    /// The active profile's vars beat workspace vars in the send-time resolver.
    #[test]
    fn resolver_precedence_profile_beats_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.explorer.expand().unwrap();
        app.explorer.cursor = 1;
        let selected = app.explorer.select().unwrap().expect("endpoint");
        // No profile: {{host}} is unresolved (workspace has no `host`), left verbatim.
        let resolver = app.build_resolver(&selected);
        assert_eq!(resolver.substitute("{{host}}"), "{{host}}");
        assert_eq!(resolver.substitute("{{base}}"), "ws");
        // With the prod profile active, {{host}} resolves from the profile.
        app.set_profile(Some("prod".to_owned()));
        let resolver = app.build_resolver(&selected);
        assert_eq!(resolver.substitute("{{host}}"), "prod.test");
    }

    /// The profile picker sets (and clears) the active profile.
    #[test]
    fn switch_profile_picker_sets_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.dispatch(Action::SwitchProfile, None).unwrap();
        assert_eq!(app.mode, Mode::Palette);
        // Choices: (none), dev, prod.
        assert_eq!(app.profile_choices.len(), 3);
        // Select index 2 (prod).
        if let Some(picker) = app.picker.as_mut() {
            picker.selected = 2;
        }
        app.accept_overlay().unwrap();
        assert_eq!(app.active_profile.as_deref(), Some("prod"));
        assert!(app.profile_choices.is_empty());
    }

    /// `TransientStatus` expires after EXPIRE_SECS; a backdated `set_at` triggers it.
    #[test]
    fn status_expires_after_4s() {
        // A fresh message is not expired.
        let fresh = TransientStatus::new("hello");
        assert!(!fresh.is_expired(), "brand-new message must not be expired");

        // A message whose set_at is backdated past the threshold is expired.
        let old = TransientStatus {
            message: "stale".to_owned(),
            set_at: Instant::now() - Duration::from_secs(TransientStatus::EXPIRE_SECS + 1),
        };
        assert!(
            old.is_expired(),
            "message older than EXPIRE_SECS must be expired"
        );
    }

    /// While in_flight is Some, the render-time derived message is "sending…"
    /// regardless of the transient status field.
    #[test]
    fn in_flight_statusline_message_derives_from_state() {
        let app = App::new(None, KeyMap::default()).unwrap();
        // No in_flight → no derived message.
        let msg: Option<&str> = app.in_flight.as_ref().map(|_| "sending… (ctrl-c cancels)");
        assert!(msg.is_none());
    }

    /// The profile picker marks the active profile with ● and (none) with ●
    /// when no profile is active.
    #[test]
    fn profile_picker_marks_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        // No active profile: (none) is marked.
        app.dispatch(Action::SwitchProfile, None).unwrap();
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.items[0], "● (none)");
        assert_eq!(picker.items[1], "dev");
        assert_eq!(picker.items[2], "prod");

        // Set active profile to dev (directly, no close needed — open picker
        // on a fresh fixture to keep state clean).
        let dir2 = tempfile::tempdir().unwrap();
        let mut app2 = workspace_fixture(dir2.path());
        app2.active_profile = Some("dev".to_owned());
        app2.dispatch(Action::SwitchProfile, None).unwrap();
        let picker2 = app2.picker.as_ref().unwrap();
        assert_eq!(picker2.items[0], "(none)");
        assert_eq!(picker2.items[1], "● dev");
        assert_eq!(picker2.items[2], "prod");
    }
}
