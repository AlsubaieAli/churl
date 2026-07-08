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
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::Sender as JobSender;
use std::time::{Duration, Instant};

use churl_core::config::Config;
use churl_core::history::{HistoryStore, NewHistoryEntry, default_state_path};
use churl_core::http::ExecuteOptions;
use churl_core::interchange::{self, JsonDialect};
use churl_core::model::{ApiKeyPlacement, Auth, Body, BodyKind, Endpoint, Header, Param, Response};
use churl_core::persistence::{self, OpenWorkspace, PersistenceError};
use churl_core::template::{Resolver, Scope};
use color_eyre::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use edtui::{EditorEventHandler, EditorMode, EditorState, Lines};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use reqwest::Client;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use churl_core::config::UrlEditMode;

use super::clipboard;
use super::components::env_editor::{EnvEditorState, EnvKeyOutcome, EnvSaveResult};
use super::components::explorer::{ExplorerState, RowKind, SelectedEndpoint};
use super::components::jump::{JumpState, JumpTarget};
use super::components::line_editor::LineEditor;
use super::components::message::Message;
use super::components::request_tabs::{EditField, FieldEdit, RequestTab, RequestTabs};
use super::components::response::{ResponseMeta, ResponseState, ResponseView, ViewMode};
use churl_core::sequence::StepResult;

use super::components::sequence_editor::{EditorOutcome, SequenceEditorState};
use super::components::sequence_runner::{RunnerOutcome, SequenceRunnerState, StepStatus};
use super::components::vim_ext::{self, VimExt};
use super::components::{
    env_editor, explorer, help, message, method_menu, palette, picker, prompt, request, response,
    sequence_editor, sequence_runner, statusline, urlbar,
};
use super::events::{Action, FuzzyFinder, KeyMap, PaneCtx};
use super::highlight::{self, HighlightJob};
use super::theme::Theme;

/// Which pane has focus in [`Mode::Normal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The workspace tree on the left.
    Explorer,
    /// The focusable URL bar above the request editor.
    UrlBar,
    /// The request editor in the centre.
    Request,
    /// The response viewer on the right.
    Response,
}

impl Pane {
    /// Tab cycle order: Explorer → UrlBar → Request → Response → Explorer.
    fn next(self) -> Self {
        match self {
            Pane::Explorer => Pane::UrlBar,
            Pane::UrlBar => Pane::Request,
            Pane::Request => Pane::Response,
            Pane::Response => Pane::Explorer,
        }
    }

    fn prev(self) -> Self {
        self.next().next().next()
    }

    fn name(self) -> &'static str {
        match self {
            Pane::Explorer => "EXPLORER",
            Pane::UrlBar => "URL",
            Pane::Request => "REQUEST",
            Pane::Response => "RESPONSE",
        }
    }

    /// The keymap overlay context for this pane.
    fn ctx(self) -> PaneCtx {
        match self {
            Pane::Explorer => PaneCtx::Explorer,
            Pane::UrlBar => PaneCtx::UrlBar,
            Pane::Request => PaneCtx::Request,
            Pane::Response => PaneCtx::Response,
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
    /// The quick-jump workspace picker overlay is open (M7.2).
    WorkspacePicker,
    /// Jump-mode: label-driven pane/row navigation overlay.
    Jump,
    /// The method-picker menu is open (URL bar `M`).
    MethodMenu,
    /// The response body incremental-search input is open (Response `/`).
    BodySearch,
    /// A text-input prompt overlay is open (CRUD naming / typed confirm).
    Prompt(PromptPurpose),
    /// A y/n confirmation overlay is open.
    Confirm(ConfirmPurpose),
    /// The environments & variables editor (M7.3): a modal split-view that grabs
    /// every key (same routing tier as Search/Palette/Picker).
    EnvEditor,
    /// The sequence runner (M7.4): a large modal driving a request sequence and
    /// showing live per-step status + each step's response.
    SequenceRunner,
    /// The in-app sequence editor (M7.4 §4): edit steps, extraction rules, and
    /// `on_error`; saved through `save_sequence`.
    SequenceEditor,
}

/// What a [`Mode::Prompt`] is collecting text for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptPurpose {
    /// Name for a new endpoint in the selected collection.
    NewEndpoint,
    /// Name for a new collection.
    NewCollection,
    /// New name for the selected endpoint or collection.
    Rename,
    /// Typed-name confirmation to delete the selected collection.
    DeleteCollectionConfirm,
    /// Path of a JSON collection file to import into the workspace.
    ImportCollection,
    /// Destination path (inside the workspace) for a collection export in the
    /// carried dialect.
    ExportCollection(JsonDialect),
    /// Destination path (inside the workspace) for a workspace export in the
    /// carried dialect.
    ExportWorkspace(JsonDialect),
    /// A curl command to import as a new endpoint in the selected collection.
    PasteCurl,
    /// Name for a new request sequence (M7.4); opens the editor on the created file.
    NewSequence,
}

impl PromptPurpose {
    /// The overlay title for this prompt.
    fn title(self) -> &'static str {
        match self {
            PromptPurpose::NewEndpoint => "New endpoint",
            PromptPurpose::NewCollection => "New collection",
            PromptPurpose::Rename => "Rename",
            PromptPurpose::DeleteCollectionConfirm => "Delete collection",
            PromptPurpose::ImportCollection => "Import collection (JSON path)",
            PromptPurpose::ExportCollection(JsonDialect::Postman) => {
                "Export collection · Postman v2.1"
            }
            PromptPurpose::ExportCollection(JsonDialect::Native) => {
                "Export collection · churl JSON"
            }
            PromptPurpose::ExportWorkspace(JsonDialect::Postman) => {
                "Export workspace · Postman v2.1"
            }
            PromptPurpose::ExportWorkspace(JsonDialect::Native) => "Export workspace · churl JSON",
            PromptPurpose::PasteCurl => "Paste curl",
            PromptPurpose::NewSequence => "New sequence",
        }
    }
}

/// What a [`Mode::Confirm`] is a y/n gate for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmPurpose {
    /// Delete the selected endpoint.
    DeleteEndpoint,
    /// Discard unsaved changes before switching endpoints. The pending target
    /// lives in `App::pending_load` (it can carry a path, so it is not part of
    /// this `Copy` enum).
    DiscardChanges,
}

/// A deferred endpoint load awaiting the discard-changes confirm. Every path
/// that replaces the loaded endpoint resolves to one of these targets.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingLoad {
    /// Select the explorer row at this index (explorer Enter / jump-mode; the
    /// cursor is already on the row when the guard fires).
    Row(usize),
    /// Load the endpoint at this file path (search overlay / CRUD reselect).
    File(std::path::PathBuf),
    /// Switch to the workspace rooted at this path (quick-jump workspace picker).
    /// Always treated as "other" by the dirty guard.
    Workspace(std::path::PathBuf),
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
    /// A sequence step completed (M7.4). `run_generation` is matched against the
    /// runner's generation so results from a cancelled/superseded run are dropped.
    SequenceStep {
        /// The run generation this step belonged to.
        run_generation: u64,
        /// The step index within the runner.
        index: usize,
        /// The response, or a stringified transport error.
        outcome: Result<Response, String>,
    },
}

/// Which pane is zoomed (the other collapses to a stub). See deliverable 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomPane {
    /// Request pane zoomed; Response collapses to its stats line.
    Request,
    /// Response pane zoomed; Request collapses to its tab bar.
    Response,
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
    /// Normal-mode motion extensions (W/B/^/f/F/t/T) for the Body editor.
    editor_vim: VimExt,
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
    /// Canonical workspace paths behind an open workspace picker (index-aligned
    /// with the picker items). Non-empty only while `mode` is
    /// [`Mode::WorkspacePicker`].
    workspace_choices: Vec<PathBuf>,
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
    /// Response viewer cursor as a display-row index (post-fold, post-wrap);
    /// clamped at render time. Reset to 0 on each new response (M7).
    response_cursor: usize,
    /// Total display rows in the response viewer as of the last render, for
    /// clamping cursor/scroll motion between frames.
    response_total_rows: usize,
    /// Last rendered response body height, for half-page scrolling.
    response_viewport_height: usize,
    /// Last rendered response body width, for cursor→logical mapping (wrap).
    response_viewport_width: usize,
    /// The highlight hash last enqueued but not yet returned; guards against
    /// re-enqueueing the same viewport twice (M7 highlight micro-nit).
    pending_highlight: Option<u64>,
    /// A clipboard payload to flush to the terminal via OSC 52 after the next
    /// key is handled (dispatch has no terminal handle; the run loop owns it).
    pending_clipboard: Option<String>,
    /// The incremental body-search input editor while `Mode::BodySearch` is open.
    body_search_editor: LineEditor,
    /// Job sender for the off-thread highlight worker; `None` under `TestBackend`.
    highlight_tx: Option<JobSender<HighlightJob>>,
    /// Viewport-hash → highlighted-lines cache (capped, cleared on new response).
    highlight_cache: HashMap<u64, Vec<Line<'static>>>,
    /// History store; `None` when disabled (open failed or no data dir).
    history: Option<HistoryStore>,
    /// Transient action/status message shown in the dedicated row above the
    /// statusline (send hints, history/errors, merges, CRUD results); auto-expires
    /// after `Message::EXPIRE_SECS`.
    message: Option<Message>,
    /// Monotonic tick counter (incremented every 250 ms tick); drives the spinner
    /// animation in the response pane. `pub` so snapshot tests can set it.
    pub tick_count: u64,
    /// Request-pane tab state (active tab + per-tab selection + field edit).
    pub tabs: RequestTabs,
    /// The pristine endpoint as loaded, cloned at load time. Dirty state is
    /// *derived* by comparing it against the live request (incl. the edtui body)
    /// — no flag bookkeeping. `None` when nothing is loaded.
    loaded_snapshot: Option<Endpoint>,
    /// The inline URL-bar editor while editing the URL; `None` otherwise.
    pub url_editor: Option<LineEditor>,
    /// The text prompt's line editor while a [`Mode::Prompt`] overlay is open.
    pub prompt_editor: LineEditor,
    /// True while the open picker is the auth-kind picker (vs search/palette/profile).
    auth_picker: bool,
    /// The endpoint-switch target deferred behind an open
    /// [`ConfirmPurpose::DiscardChanges`] overlay; resolved by `s`/`d`, dropped
    /// by `Esc`.
    pending_load: Option<PendingLoad>,
    /// Which pane is zoomed (M6.7 deliverable 4), or `None` for the normal split.
    zoom: Option<ZoomPane>,
    /// Whether the explorer sidebar is hidden (M6.7 deliverable 5). Session-only.
    explorer_hidden: bool,
    /// True while in pending-leader state (the which-key popup is shown).
    pending_leader: bool,
    /// Whether the `?` help overlay is open.
    help_open: bool,
    /// Scroll offset of the help overlay.
    help_scroll: usize,
    /// Last rendered inner height of the help overlay, for half-page scrolling.
    help_viewport_height: usize,
    /// The edtui popup URL editor (`e` on the URL bar / `url_edit = "popup"`),
    /// present while the popup is open.
    url_popup: Option<EditorState>,
    /// edtui event handler for the URL popup.
    url_popup_events: EditorEventHandler,
    /// Normal-mode motion extensions (W/B/^/f/F/t/T) for the URL popup editor.
    url_popup_vim: VimExt,
    /// What `i`/`Enter` on the URL bar opens (inline vs popup); `e` always popup.
    url_edit_mode: UrlEditMode,
    /// The open environments & variables editor (M7.3), when `Mode::EnvEditor`.
    env_editor: Option<EnvEditorState>,
    /// The open sequence runner (M7.4), when `Mode::SequenceRunner`.
    sequence_runner: Option<SequenceRunnerState>,
    /// Abort handle for the in-flight sequence step, so a cancel/re-run aborts it.
    sequence_abort: Option<AbortHandle>,
    /// The open sequence editor (M7.4 §4), when `Mode::SequenceEditor`.
    sequence_editor: Option<SequenceEditorState>,
}

/// A minimal [`ResponseMeta`] for a sequence step's failed/error response view
/// (sequence steps write no history, so only the display fields matter).
fn sequence_step_meta(endpoint: &str) -> ResponseMeta {
    ResponseMeta {
        method: String::new(),
        url: endpoint.to_owned(),
        endpoint_path: Some(endpoint.to_owned()),
        executed_at_ms: now_ms(),
    }
}

/// The current Unix time in milliseconds (saturating to `0` before the epoch).
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// The canonical (absolute, symlink-resolved) form of `path`, falling back to
/// the path as-given when canonicalization fails (e.g. it no longer exists).
/// Used so the same workspace is never stored under two different spellings.
fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A workspace path shortened for display: `$HOME` collapses to `~`.
fn display_workspace_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = Path::new(path).strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.to_owned()
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
            editor_vim: VimExt::default(),
            selected: None,
            focus: Pane::Explorer,
            mode: Mode::Normal,
            picker: None,
            search_targets: Vec::new(),
            palette_actions: Vec::new(),
            profile_choices: Vec::new(),
            workspace_choices: Vec::new(),
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
            response_cursor: 0,
            response_total_rows: 0,
            response_viewport_height: 0,
            response_viewport_width: 0,
            pending_highlight: None,
            pending_clipboard: None,
            body_search_editor: LineEditor::default(),
            highlight_tx: None,
            highlight_cache: HashMap::new(),
            history: None,
            message: None,
            tick_count: 0,
            tabs: RequestTabs::default(),
            loaded_snapshot: None,
            url_editor: None,
            prompt_editor: LineEditor::default(),
            auth_picker: false,
            pending_load: None,
            zoom: None,
            explorer_hidden: false,
            pending_leader: false,
            help_open: false,
            help_scroll: 0,
            help_viewport_height: 10,
            url_popup: None,
            url_popup_events: EditorEventHandler::default(),
            url_popup_vim: VimExt::default(),
            url_edit_mode: UrlEditMode::Inline,
            env_editor: None,
            sequence_runner: None,
            sequence_abort: None,
            sequence_editor: None,
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

    /// Sets a transient message in the dedicated message row (auto-expires).
    fn notify(&mut self, text: impl Into<String>) {
        self.message = Some(Message::new(text));
    }

    /// Drains any lenient-load warnings the explorer accumulated (skipped /
    /// unparseable endpoint files) and, if there are any, surfaces an aggregate in
    /// the message row. A single bad file never aborts the load, but it is never
    /// swallowed silently either (Constitution: fail loudly).
    fn surface_explorer_warnings(&mut self) {
        let warnings = self.explorer.take_warnings();
        if warnings.is_empty() {
            return;
        }
        let n = warnings.len();
        // Name up to two files inline; the rest are summarised by count.
        let named: Vec<&str> = warnings
            .iter()
            .take(2)
            .map(|w| {
                w.trim_start_matches("skipped ")
                    .split(':')
                    .next()
                    .unwrap_or(w)
            })
            .collect();
        let detail = if n <= named.len() {
            named.join(", ")
        } else {
            format!("{}, +{} more", named.join(", "), n - named.len())
        };
        self.notify(format!("{n} file(s) skipped (unparseable): {detail}"));
    }

    /// Sets what `i`/`Enter` on the URL bar opens (inline vs popup).
    pub fn set_url_edit_mode(&mut self, mode: UrlEditMode) {
        self.url_edit_mode = mode;
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
                    self.message = Some(Message::new(format!("history disabled: {err}")));
                }
            },
            None => {
                self.message = Some(Message::new("history disabled: no data directory"));
            }
        }
        // Seed workspace recency with the workspace we launched in, so it shows
        // up in the quick-jump workspace picker. Best-effort: a write failure is
        // non-fatal (the picker just won't list this one yet).
        if let (Some(store), Some(ws)) = (self.history.as_ref(), self.workspace.as_ref()) {
            let canonical = canonical_path(ws.root());
            let _ = store.touch_workspace(&canonical.to_string_lossy(), now_ms());
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
                        // Flush any clipboard payload a copy action queued: OSC 52
                        // goes straight to the terminal's backend writer (dispatch
                        // has no terminal handle). A write error is non-fatal — the
                        // copy silently no-ops on terminals that reject it anyway.
                        if let Some(payload) = self.pending_clipboard.take() {
                            let _ = clipboard::copy_osc52(payload.as_str(), terminal.backend_mut());
                        }
                    }
                    Some(Ok(_)) => {} // resize etc. — redraw happens next iteration
                    Some(Err(err)) => return Err(err.into()),
                    None => break, // input stream closed
                },
                _ = tick.tick() => {
                    self.tick_count = self.tick_count.wrapping_add(1);
                    // Expire transient status messages after ~4 s.
                    if self.message.as_ref().is_some_and(|s| s.is_expired()) {
                        self.message = None;
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
        // Modal, keyboard-owning overlays introduced in M6.7 take precedence over
        // everything (help / URL popup / pending-leader).
        if self.help_open {
            return self.handle_help_key(key);
        }
        if self.url_popup.is_some() {
            return self.handle_url_popup_key(key);
        }
        if self.pending_leader {
            return self.handle_leader_key(key);
        }
        match self.mode {
            Mode::Search | Mode::Palette | Mode::WorkspacePicker => self.handle_overlay_key(key),
            Mode::Jump => self.handle_jump_key(key),
            Mode::MethodMenu => {
                self.handle_method_menu_key(key);
                Ok(())
            }
            Mode::BodySearch => {
                self.handle_body_search_key(key);
                Ok(())
            }
            Mode::Prompt(purpose) => self.handle_prompt_key(key, purpose),
            Mode::Confirm(purpose) => self.handle_confirm_key(key, purpose),
            Mode::EnvEditor => self.handle_env_editor_key(key),
            Mode::SequenceRunner => self.handle_sequence_runner_key(key),
            Mode::SequenceEditor => self.handle_sequence_editor_key(key),
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    /// Key routing in [`Mode::Normal`]: inline editors (URL bar / request row edit)
    /// intercept first (with the Ctrl-S/Ctrl-C exception), then the focused pane's
    /// keymap overlay + global map, then edtui fall-through for the Request pane.
    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<()> {
        // 1. Inline URL editing: the LineEditor owns keys, except the documented
        //    Ctrl-S/Ctrl-C interception (Send/Quit reach through).
        if self.url_editor.is_some() {
            if let Some(action) = self.control_intercept(key) {
                return self.dispatch(action, Some(key));
            }
            return self.handle_url_edit_key(key);
        }
        // 2. Inline request-row field editing (same interception rule).
        if self.focus == Pane::Request && self.tabs.editing.is_some() {
            if let Some(action) = self.control_intercept(key) {
                return self.dispatch(action, Some(key));
            }
            return self.handle_field_edit_key(key);
        }
        // 3. edtui insert/visual mode on the Body tab (M4 interception exception).
        if self.focus == Pane::Request
            && self.tabs.active == RequestTab::Body
            && self.editor.mode != EditorMode::Normal
        {
            if let Some(action) = self.control_intercept(key) {
                return self.dispatch(action, Some(key));
            }
            self.editor_events.on_key_event(key, &mut self.editor);
            return Ok(());
        }
        // 3b. Body tab in Normal mode: churl-side vim motions (W/B/^/f/F/t/T)
        //     win before leader/keymap. `f` becomes find-char inside the Body
        //     editor, shadowing the global Jump key there (DECISIONS.md — M6.6
        //     shadowing precedent); the others are unbound today so nothing is
        //     lost. This precedes the leader/keymap steps so a pending find's
        //     next char reaches vim_ext even when it's Space or a mapped key.
        if self.focus == Pane::Request
            && self.tabs.active == RequestTab::Body
            && self.editor.mode == EditorMode::Normal
            && vim_ext::handle_key(key, &mut self.editor, &mut self.editor_vim)
        {
            return Ok(());
        }
        // 4. Leader key: outside every text-edit context (guarded above), Space
        //    enters pending-leader state and shows the which-key popup. Inside an
        //    edit, control never reaches here — Space types a space.
        if self.keymap.is_leader(key) {
            self.pending_leader = true;
            return Ok(());
        }
        // 5. Keymap: the focused pane's overlay wins over the global map.
        if let Some(action) = self.keymap.lookup_ctx(key, self.focus.ctx()) {
            self.dispatch(action, Some(key))
        } else if self.focus == Pane::Request && self.tabs.active == RequestTab::Body {
            // Unmapped key on the Body tab falls through to edtui.
            self.editor_events.on_key_event(key, &mut self.editor);
            Ok(())
        } else {
            Ok(())
        }
    }

    /// The Ctrl-S/Ctrl-C interception used while an inline editor holds the
    /// keyboard: a CONTROL-modified key the keymap resolves to Send or Quit is
    /// dispatched instead of reaching the editor (DECISIONS.md — those keys are
    /// not text input, and resolving through the keymap honours user remaps).
    fn control_intercept(&self, key: KeyEvent) -> Option<Action> {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return None;
        }
        match self.keymap.lookup(key) {
            Some(action @ (Action::Send | Action::Quit)) => Some(action),
            _ => None,
        }
    }

    /// Handles one key in pending-leader state: a bound continuation dispatches
    /// its action (and dismisses the popup); any unbound key or Esc dismisses.
    fn handle_leader_key(&mut self, key: KeyEvent) -> Result<()> {
        self.pending_leader = false;
        if key.code == KeyCode::Esc {
            return Ok(());
        }
        if let Some(action) = self.keymap.leader_lookup(key) {
            return self.dispatch(action, Some(key));
        }
        // Unbound continuation: dismiss silently (already done).
        Ok(())
    }

    /// Handles one key while the `?` help overlay is open: `?`/Esc/`q` close;
    /// `j`/`k`/arrows scroll.
    fn handle_help_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                self.help_open = false;
                self.help_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => self.help_scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::Char('d') => {
                let half = (self.help_viewport_height / 2).max(1);
                self.help_scroll += half;
            }
            KeyCode::Char('u') => {
                let half = (self.help_viewport_height / 2).max(1);
                self.help_scroll = self.help_scroll.saturating_sub(half);
            }
            _ => {}
        }
        Ok(())
    }

    /// Handles one key in the URL vim-popup editor. Mode-aware:
    /// - In `EditorMode::Search`, everything (incl. Enter/Esc) goes to edtui so
    ///   `/`-search executes: Enter runs FindFirst (jump to match → Normal), Esc
    ///   cancels the search. Never commits from Search mode.
    /// - Otherwise Enter commits (running the param merge; the single-logical-line
    ///   constraint drops any Enter that edtui would turn into a newline); in
    ///   Normal mode `vim_ext` motions (W/B/^/f/F/t/T) run next — before the
    ///   Esc-cancel check, so Esc aborts a pending find instead of closing the
    ///   popup; then Esc in Normal cancels; the rest falls through to edtui.
    ///
    /// Accepted edge: Enter while an f/F/t/T find is pending still commits (the
    /// pending find is dropped with the popup).
    fn handle_url_popup_key(&mut self, key: KeyEvent) -> Result<()> {
        let mode = self.url_popup.as_ref().map(|e| e.mode);
        // Search mode: pass everything to edtui — Enter/Esc drive the search.
        if mode == Some(EditorMode::Search) {
            if let Some(editor) = self.url_popup.as_mut() {
                self.url_popup_events.on_key_event(key, editor);
            }
            return Ok(());
        }
        // Enter commits (single logical line — no newline).
        if key.code == KeyCode::Enter {
            if let Some(editor) = self.url_popup.take() {
                let text: String = editor.lines.clone().into();
                // Collapse any stray newlines to enforce a single logical line.
                let text = text.replace(['\n', '\r'], "");
                self.commit_url(text);
            }
            return Ok(());
        }
        // Normal mode: churl-side vim motions win before the Esc-cancel check —
        // Esc while an f/F/t/T find is pending aborts the find (vim), it must
        // not close the popup.
        if mode == Some(EditorMode::Normal)
            && let Some(editor) = self.url_popup.as_mut()
            && vim_ext::handle_key(key, editor, &mut self.url_popup_vim)
        {
            return Ok(());
        }
        // Esc in Normal mode cancels; in Insert mode edtui uses it to leave insert.
        if key.code == KeyCode::Esc && mode == Some(EditorMode::Normal) {
            self.url_popup = None;
            return Ok(());
        }
        if let Some(editor) = self.url_popup.as_mut() {
            self.url_popup_events.on_key_event(key, editor);
        }
        Ok(())
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
            Action::FocusNext => self.set_focus(self.focus.next()),
            Action::FocusPrev => self.set_focus(self.focus.prev()),
            Action::FocusExplorer => self.set_focus(Pane::Explorer),
            Action::FocusUrlBar => self.set_focus(Pane::UrlBar),
            Action::FocusRequest => self.set_focus(Pane::Request),
            Action::FocusResponse => self.set_focus(Pane::Response),
            Action::ToggleExplorer => self.toggle_explorer(),
            Action::Zoom => self.toggle_zoom(),
            Action::Help => self.help_open = true,
            Action::Leader => self.pending_leader = true,
            Action::EditUrlPopup => self.begin_url_popup(),
            Action::OpenSearch => self.open_search()?,
            Action::OpenPalette => self.open_palette(),
            Action::Jump => self.open_jump(),
            Action::SwitchProfile => self.open_profile_picker(),
            Action::OpenEnvEditor => self.open_env_editor(),
            Action::RunSequence => self.run_selected_sequence(),
            Action::EditSequence => self.edit_selected_sequence()?,
            Action::Send => self.send_request(),
            Action::Cancel => self.cancel_request(),
            Action::Save => self.save_request(),
            Action::EditUrl => self.begin_url_edit(),
            Action::MethodCycle => self.cycle_method(),
            Action::MethodMenu => self.open_method_menu(),
            Action::TabNext => self.tabs.tab_next(),
            Action::TabPrev => self.tabs.tab_prev(),
            Action::Tab1 => self.tabs.tab_jump(0),
            Action::Tab2 => self.tabs.tab_jump(1),
            Action::Tab3 => self.tabs.tab_jump(2),
            Action::Tab4 => self.tabs.tab_jump(3),
            // On the Body tab there are no rows: the originating key (i/a/d/…)
            // belongs to edtui, same as the motion keys in `request_nav`.
            Action::RowAdd | Action::RowDelete | Action::RowToggle | Action::RowEdit
                if self.focus == Pane::Request && self.tabs.active == RequestTab::Body =>
            {
                if let Some(key) = key {
                    self.editor_events.on_key_event(key, &mut self.editor);
                }
            }
            Action::RowAdd => self.row_add(),
            Action::RowDelete => self.row_delete(),
            Action::RowToggle => self.row_toggle(),
            Action::RowEdit => self.row_edit(),
            Action::NewEndpoint => self.begin_new_endpoint(),
            Action::NewCollection => self.begin_new_collection(),
            Action::Rename => self.begin_rename(),
            Action::Delete => self.begin_delete(),
            Action::HalfPageDown | Action::HalfPageUp => {
                if self.focus == Pane::Response {
                    self.response_half_page(matches!(action, Action::HalfPageDown));
                }
            }
            Action::ToggleHeadersView => self.response_toggle_headers(),
            Action::ToggleWrap => self.response_toggle_wrap(),
            Action::OpenBodySearch => self.open_body_search(),
            Action::SearchNext => self.response_search_step(true),
            Action::SearchPrev => self.response_search_step(false),
            Action::ToggleFold => self.response_toggle_fold(),
            Action::ToggleAllFolds => self.response_toggle_all_folds(),
            Action::CopyResponse => self.response_copy_view(),
            Action::CopyLine => self.response_copy_line(),
            // `<leader>f` reuses the endpoint-search overlay as the request picker.
            Action::QuickJumpRequests => self.open_search()?,
            Action::QuickJumpWorkspaces => self.open_workspace_picker(),
            Action::ImportCollection => self.begin_import_collection(),
            Action::ExportCollectionPostman => self.begin_export_collection(JsonDialect::Postman),
            Action::ExportCollectionNative => self.begin_export_collection(JsonDialect::Native),
            Action::ExportWorkspacePostman => self.begin_export_workspace(JsonDialect::Postman),
            Action::ExportWorkspaceNative => self.begin_export_workspace(JsonDialect::Native),
            Action::PasteCurl => self.begin_paste_curl(),
            Action::CopyAsCurl => self.copy_as_curl(false),
            Action::CopyAsCurlResolved => self.copy_as_curl(true),
            Action::Up
            | Action::Down
            | Action::Select
            | Action::Collapse
            | Action::Expand
            | Action::Top
            | Action::Bottom => match self.focus {
                Pane::Explorer => self.explorer_action(action)?,
                Pane::UrlBar => {}
                Pane::Request => self.request_nav(action, key),
                Pane::Response => self.response_scroll(action),
            },
        }
        Ok(())
    }

    /// Navigation within the Request pane. On the Body tab the motion keys forward
    /// to edtui; on the row-list tabs `j`/`k` move the selection, `Enter` edits.
    fn request_nav(&mut self, action: Action, key: Option<KeyEvent>) {
        if self.tabs.active == RequestTab::Body {
            if let Some(key) = key {
                self.editor_events.on_key_event(key, &mut self.editor);
            }
            return;
        }
        match action {
            Action::Up => self.tabs.move_up(),
            Action::Down => {
                let n = self.active_tab_row_count();
                self.tabs.move_down(n);
            }
            Action::Select => self.row_edit(),
            _ => {}
        }
    }

    fn explorer_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Up => self.explorer.move_up(),
            Action::Down => self.explorer.move_down(),
            Action::Top => self.explorer.top(),
            Action::Bottom => self.explorer.bottom(),
            Action::Collapse => self.explorer.collapse(),
            Action::Expand => {
                self.explorer.expand()?;
                self.surface_explorer_warnings();
            }
            Action::Select => {
                // A sequence row opens the runner; otherwise the guarded seam:
                // switching to a *different* endpoint while dirty prompts to
                // save/discard first (a collection toggle / same endpoint is not
                // guarded).
                self.activate_explorer_row(self.explorer.cursor)?;
            }
            _ => unreachable!("only navigation actions reach explorer_action"),
        }
        Ok(())
    }

    /// Loads an endpoint into the request pane: body into the edtui buffer, the
    /// request itself into the live `selected` slot, and a pristine clone into
    /// `loaded_snapshot` for dirty derivation. Resets tab state.
    fn load_endpoint(&mut self, selected: SelectedEndpoint) {
        let body = selected
            .endpoint
            .request
            .body
            .as_ref()
            .map(|body| body.content.as_str())
            .unwrap_or("");
        self.editor = EditorState::new(Lines::from(body));
        self.editor_vim.reset();
        self.loaded_snapshot = Some(selected.endpoint.clone());
        self.selected = Some(selected);
        self.tabs = RequestTabs::default();
        self.url_editor = None;
    }

    /// The live request currently loaded (with any in-memory edits), or `None`.
    fn live_request(&self) -> Option<&churl_core::model::Request> {
        self.selected.as_ref().map(|s| &s.endpoint.request)
    }

    /// Mirrors the current edtui body text into the live request's `body` so the
    /// live endpoint reflects unsaved body edits (for dirty derivation and save).
    /// A body that becomes empty drops the `Body` entirely.
    fn sync_body_into_selected(&mut self) {
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        let text = String::from(self.editor.lines.clone());
        match selected.endpoint.request.body.as_mut() {
            Some(body) => {
                if text.is_empty() {
                    selected.endpoint.request.body = None;
                } else {
                    body.content = text;
                }
            }
            None if !text.is_empty() => {
                selected.endpoint.request.body = Some(Body {
                    kind: BodyKind::Text,
                    content: text,
                });
            }
            None => {}
        }
    }

    /// Whether the live endpoint (incl. the edtui body) differs from the pristine
    /// snapshot. Derived — no dirty flag to keep in sync.
    fn is_dirty(&self) -> bool {
        let Some(snapshot) = &self.loaded_snapshot else {
            return false;
        };
        let Some(selected) = &self.selected else {
            return false;
        };
        // Compare with the live body folded in (without mutating self).
        let mut live = selected.endpoint.clone();
        let text = String::from(self.editor.lines.clone());
        match live.request.body.as_mut() {
            Some(body) => {
                if text.is_empty() {
                    live.request.body = None;
                } else {
                    body.content = text;
                }
            }
            None if !text.is_empty() => {
                live.request.body = Some(Body {
                    kind: BodyKind::Text,
                    content: text,
                });
            }
            None => {}
        }
        &live != snapshot
    }

    /// Sends the selected endpoint's request with the live edtui body text.
    /// Spawns the execution task, keeps its `AbortHandle`, and moves the response
    /// pane to the in-flight state. Ignored (with a statusline hint) when a
    /// request is already in flight or no endpoint is selected.
    fn send_request(&mut self) {
        if self.in_flight.is_some() {
            self.message = Some(Message::new("request already in flight — ctrl-c to cancel"));
            return;
        }
        let Some(selected) = self.selected.clone() else {
            self.message = Some(Message::new("no endpoint selected — nothing to send"));
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
        self.response_cursor = 0;
        self.pending_highlight = None;
        self.highlight_cache.clear();
        self.message = None;
    }

    /// Cancels the in-flight request: aborts the task, records a history row with
    /// no status, and moves the pane to the cancelled state.
    fn cancel_request(&mut self) {
        let Some(in_flight) = self.in_flight.take() else {
            self.message = Some(Message::new("no request in flight"));
            return;
        };
        in_flight.handle.abort();
        self.write_history(&in_flight.meta, None, None);
        self.response = ResponseState::Cancelled;
        self.message = Some(Message::new("request cancelled"));
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

    /// The coarse maximum cursor row (total display rows minus one, as of the
    /// last render); render clamps further.
    fn response_max_cursor(&self) -> usize {
        self.response_total_rows.saturating_sub(1)
    }

    /// Moves the response viewer *cursor* for a navigation action. Scroll follows
    /// the cursor at render time (see `response::render`). `g`/`G` jump to the
    /// first/last visible display row.
    fn response_scroll(&mut self, action: Action) {
        if !matches!(self.response, ResponseState::Done { .. }) {
            return;
        }
        let max = self.response_max_cursor();
        match action {
            Action::Up => self.response_cursor = self.response_cursor.saturating_sub(1),
            Action::Down => self.response_cursor = (self.response_cursor + 1).min(max),
            Action::Top => self.response_cursor = 0,
            Action::Bottom => self.response_cursor = max,
            _ => {}
        }
    }

    /// Moves the response cursor by half a viewport (scroll follows at render).
    fn response_half_page(&mut self, down: bool) {
        if !matches!(self.response, ResponseState::Done { .. }) {
            return;
        }
        let half = (self.response_viewport_height / 2).max(1);
        let max = self.response_max_cursor();
        self.response_cursor = if down {
            (self.response_cursor + half).min(max)
        } else {
            self.response_cursor.saturating_sub(half)
        };
    }

    // ---- Response viewer M7 actions ----

    /// The live `ResponseView`, when the pane holds a completed response.
    fn response_view_mut(&mut self) -> Option<&mut ResponseView> {
        match &mut self.response {
            ResponseState::Done { view } => Some(view),
            _ => None,
        }
    }

    /// The logical line under the response cursor (through the last render's
    /// fold/wrap geometry), or `None` when there is no response.
    fn response_cursor_logical(&self) -> Option<usize> {
        let width = self.response_viewport_width;
        let cursor = self.response_cursor;
        match &self.response {
            ResponseState::Done { view } => view.logical_at_display_row(cursor, width),
            _ => None,
        }
    }

    /// `h`: toggle body/headers view. Resets cursor + scroll (the two views have
    /// different geometry) and clears any live search.
    fn response_toggle_headers(&mut self) {
        if let Some(view) = self.response_view_mut() {
            view.toggle_view_mode();
            self.response_cursor = 0;
            self.response_scroll = 0;
            self.pending_highlight = None;
            self.highlight_cache.clear();
        }
    }

    /// `W`: toggle soft-wrap. Cursor/scroll geometry changes, so reset them.
    fn response_toggle_wrap(&mut self) {
        if let Some(view) = self.response_view_mut() {
            view.toggle_wrap();
            self.response_cursor = 0;
            self.response_scroll = 0;
            self.pending_highlight = None;
        }
    }

    /// `o`: fold/unfold the innermost JSON region at the cursor. Non-JSON views
    /// notify and no-op.
    /// Why folding is unsupported right now, or `None` when it is available.
    /// The headers view of a JSON response gets its own reason — "JSON responses
    /// only" would be wrong there.
    fn fold_unsupported_notice(&mut self) -> Option<&'static str> {
        let view = match &self.response {
            ResponseState::Done { view } => view,
            _ => return Some("folding: no response"),
        };
        if view.view_mode() == ViewMode::Headers {
            return Some("folding: body view only");
        }
        if view.syntax() != crate::tui::highlight::SyntaxToken::Json {
            return Some("folding: JSON responses only");
        }
        None
    }

    fn response_toggle_fold(&mut self) {
        let Some(logical) = self.response_cursor_logical() else {
            return;
        };
        if let Some(notice) = self.fold_unsupported_notice() {
            self.notify(notice);
            return;
        }
        if let Some(view) = self.response_view_mut() {
            view.toggle_fold_at(logical);
            self.pending_highlight = None;
            self.highlight_cache.clear();
        }
    }

    /// `O`: collapse all top-level JSON regions, or expand all. Non-JSON no-ops
    /// with a notice.
    fn response_toggle_all_folds(&mut self) {
        if let Some(notice) = self.fold_unsupported_notice() {
            self.notify(notice);
            return;
        }
        if let Some(view) = self.response_view_mut() {
            view.toggle_all_folds();
            self.response_cursor = 0;
            self.response_scroll = 0;
            self.pending_highlight = None;
            self.highlight_cache.clear();
        }
    }

    /// `/`: open the incremental body-search input in the message-row position.
    fn open_body_search(&mut self) {
        if self.response_view_mut().is_none() {
            self.notify("no response to search");
            return;
        }
        self.body_search_editor = LineEditor::new("");
        // Seed an empty (live) search so highlighting/feedback engage immediately.
        if let Some(view) = self.response_view_mut() {
            view.set_search(String::new());
        }
        self.mode = Mode::BodySearch;
    }

    /// Handles one key while the body-search input is open. Every keystroke
    /// recomputes matches and jumps to the first; Enter commits (keeps matches
    /// for `n`/`N`), Esc cancels (clears the search).
    fn handle_body_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                if let Some(view) = self.response_view_mut() {
                    view.clear_search();
                }
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                // Move the cursor onto the current match, if any.
                self.response_center_on_match();
            }
            _ => {
                self.body_search_editor.handle_key(key);
                let query = self.body_search_editor.text();
                if let Some(view) = self.response_view_mut() {
                    view.set_search(query);
                }
                // Jump to the first match live while typing.
                self.response_center_on_match();
            }
        }
    }

    /// `n`/`N`: step to the next/previous match (wrapping), scrolling it into
    /// view and auto-unfolding its region.
    fn response_search_step(&mut self, forward: bool) {
        let has_search = self
            .response_view_mut()
            .map(|v| v.search().is_some())
            .unwrap_or(false);
        if !has_search {
            return;
        }
        let stepped = self.response_view_mut().and_then(|v| v.step_match(forward));
        if stepped.is_some() {
            self.pending_highlight = None;
            self.highlight_cache.clear();
            self.response_center_on_match();
            // Feedback: `match k/N` in the message row.
            if let Some(view) = self.response_view_mut()
                && let Some(search) = view.search()
                && let Some(ord) = search.current_ordinal()
            {
                let total = search.count();
                self.notify(format!("match {ord}/{total}"));
            }
        } else {
            self.notify("no matches");
        }
    }

    /// Moves the response cursor onto the current search match's logical line,
    /// so scroll follows it into view at the next render.
    fn response_center_on_match(&mut self) {
        let width = self.response_viewport_width;
        let row = match &self.response {
            ResponseState::Done { view } => view
                .current_match_line()
                .and_then(|logical| view.display_row_for_logical(logical, width)),
            _ => None,
        };
        if let Some(row) = row {
            self.response_cursor = row;
        }
    }

    /// `y`: copy the current response view's full text via OSC 52 (capped).
    fn response_copy_view(&mut self) {
        let Some(view) = self.response_view_mut() else {
            return;
        };
        let full = view.copy_all().to_owned();
        let truncated = view.truncated();
        self.copy_to_clipboard_view(&full, truncated);
    }

    /// `Y`: copy the response cursor's logical line via OSC 52.
    fn response_copy_line(&mut self) {
        let Some(logical) = self.response_cursor_logical() else {
            return;
        };
        let Some(view) = self.response_view_mut() else {
            return;
        };
        let line = view.copy_line(logical);
        self.enqueue_clipboard(&line);
        self.notify("copied line");
    }

    /// Copies the full-view text, reporting its size (with a `(truncated)` note
    /// when the body hit the size cap, and a `copied first X of Y` note when the
    /// 1 MB OSC 52 cap kicked in).
    fn copy_to_clipboard_view(&mut self, text: &str, body_truncated: bool) {
        let full_len = text.len();
        let payload = self.enqueue_clipboard(text);
        let capped = full_len > payload;
        let mut msg = if capped {
            format!(
                "copied first {} of {}",
                response::fmt_bytes(payload),
                response::fmt_bytes(full_len)
            )
        } else {
            format!("copied {}", response::fmt_bytes(payload))
        };
        // The body-truncation note stacks with the cap note — both facts matter.
        if body_truncated {
            msg.push_str(" (truncated)");
        }
        self.notify(msg);
    }

    /// Queues `text` (capped at [`clipboard::MAX_COPY_BYTES`], on a char
    /// boundary) for an OSC 52 write on the next loop iteration. Returns the
    /// number of bytes actually queued.
    fn enqueue_clipboard(&mut self, text: &str) -> usize {
        let payload = if text.len() > clipboard::MAX_COPY_BYTES {
            let mut end = clipboard::MAX_COPY_BYTES;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text[..end].to_owned()
        } else {
            text.to_owned()
        };
        let len = payload.len();
        self.pending_clipboard = Some(payload);
        len
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
            AppMsg::SequenceStep {
                run_generation,
                index,
                outcome,
            } => self.on_sequence_step(run_generation, index, outcome),
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
        self.response_cursor = 0;
        self.pending_highlight = None;
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
        // Clear the in-flight guard when its result lands (M7 dedup micro-nit).
        if self.pending_highlight == Some(hash) {
            self.pending_highlight = None;
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
            self.message = Some(Message::new(format!("history write failed: {err}")));
        }
    }

    fn open_search(&mut self) -> Result<()> {
        let items = super::components::search::open(&mut self.explorer)?;
        self.picker = Some(items.picker);
        self.search_targets = items.targets;
        self.mode = Mode::Search;
        // Opening search parses every collection eagerly — surface any files the
        // lenient load skipped.
        self.surface_explorer_warnings();
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
    fn handle_jump_key(&mut self, key: KeyEvent) -> Result<()> {
        // Esc always cancels.
        if key.code == KeyCode::Esc {
            self.close_jump();
            return Ok(());
        }
        let Some(jump) = &self.jump else {
            self.close_jump();
            return Ok(());
        };
        // An assigned label wins first — the default Jump key `f` also labels the
        // first explorer row, so a label lookup must take precedence over the
        // "Jump key again cancels" rule, or that target would be unreachable.
        if let KeyCode::Char(c) = key.code
            && let Some(target) = jump.target_for(c)
        {
            self.close_jump();
            match target {
                // Through set_focus, not a raw assignment — jumping into a
                // collapsed pane must auto-unzoom (the set_focus invariant).
                JumpTarget::Pane(pane) => self.set_focus(pane),
                JumpTarget::Row(row) => {
                    self.set_focus(Pane::Explorer);
                    // An endpoint row selects it (same as Enter) — through the
                    // guarded seam so dirty edits are never lost silently; a
                    // sequence row opens the runner.
                    self.activate_explorer_row(row)?;
                }
            }
            return Ok(());
        }
        // Pressing the Jump key again cancels (when it labels no target).
        if self.keymap.lookup(key) == Some(Action::Jump) {
            self.close_jump();
        }
        // Any other non-label key is ignored; jump-mode stays open.
        Ok(())
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

    /// Opens the environments & variables editor over the current workspace.
    /// Requires an open workspace (there is nothing to edit otherwise).
    fn open_env_editor(&mut self) {
        let Some(ws) = self.workspace.as_ref() else {
            self.notify("open a workspace first");
            return;
        };
        match EnvEditorState::from_workspace(ws, self.active_profile.clone(), self.cli_vars.clone())
        {
            Ok(state) => {
                self.env_editor = Some(state);
                self.mode = Mode::EnvEditor;
            }
            Err(err) => self.notify(format!("couldn't open editor: {err}")),
        }
    }

    /// Routes a key to the open env editor and acts on its outcome (save / close).
    fn handle_env_editor_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(editor) = self.env_editor.as_mut() else {
            self.mode = Mode::Normal;
            return Ok(());
        };
        match editor.handle_key(key) {
            EnvKeyOutcome::Consumed => {}
            EnvKeyOutcome::Save => {
                self.env_save()?;
            }
            EnvKeyOutcome::SaveAndClose => {
                // Close only if the save actually took (a secrets refusal / IO
                // error keeps the editor open with the error visible).
                if self.env_save()? {
                    self.close_env_editor();
                }
            }
            EnvKeyOutcome::Close => self.close_env_editor(),
        }
        Ok(())
    }

    /// Closes the editor and returns to normal mode.
    fn close_env_editor(&mut self) {
        self.env_editor = None;
        self.mode = Mode::Normal;
    }

    /// Runs the editor's save against the current workspace and, on success,
    /// live-refreshes the app so edits take effect without a restart. Returns
    /// whether the save succeeded (drives the save-and-close path).
    fn env_save(&mut self) -> Result<bool> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            self.notify("no workspace to save into");
            return Ok(false);
        };
        let name = self
            .workspace
            .as_ref()
            .map(|ws| ws.manifest().name.clone())
            .unwrap_or_default();
        let Some(editor) = self.env_editor.as_mut() else {
            return Ok(false);
        };
        match editor.save(&root, &name) {
            EnvSaveResult::Ok { active_profile, .. } => {
                // Live-refresh: re-open the manifest and reload the explorer so the
                // send-time resolver (workspace/collection/profile vars) reflects
                // the edits immediately.
                self.active_profile = active_profile;
                self.workspace = Some(OpenWorkspace::open(&root)?);
                self.reload_explorer()?;
                self.notify("saved · vars applied");
                Ok(true)
            }
            EnvSaveResult::Refused(msg) | EnvSaveResult::Failed(msg) => {
                self.notify(msg);
                Ok(false)
            }
        }
    }

    // ---- M7.4 sequence runner ----

    /// Enter (or a jump label) on the current explorer row: opens the sequence
    /// runner on a sequence row, otherwise loads the endpoint through the guarded
    /// seam. One seam so both the explorer `Enter` and jump-mode agree.
    fn activate_explorer_row(&mut self, row: usize) -> Result<()> {
        self.explorer.cursor = row;
        if self.explorer.selected_kind() == Some(RowKind::Sequence) {
            self.run_selected_sequence();
            Ok(())
        } else {
            self.guarded_load(PendingLoad::Row(row))
        }
    }

    /// Runs the sequence under the explorer cursor (`<leader>r` / palette / Enter
    /// on a sequence row). Notifies when the cursor is not on a sequence.
    fn run_selected_sequence(&mut self) {
        let Some(selected) = self.explorer.selected_sequence() else {
            self.notify("select a sequence in the SEQUENCES section first");
            return;
        };
        self.open_sequence_runner(selected);
    }

    /// Opens the runner over `selected` and starts the run.
    fn open_sequence_runner(&mut self, selected: super::components::explorer::SelectedSequence) {
        let steps = churl_core::sequence::ordered_steps(&selected.sequence)
            .into_iter()
            .cloned()
            .collect();
        self.sequence_runner = Some(SequenceRunnerState::new(
            selected.name,
            selected.file,
            selected.sequence.on_error,
            steps,
        ));
        self.mode = Mode::SequenceRunner;
        self.start_sequence_run();
    }

    /// The ambient run scopes (cli / active profile / workspace) for a sequence
    /// run, mirroring the send-time resolver's non-collection layers. The
    /// per-step collection scope is loaded inside `prepare_step`.
    fn sequence_run_scopes(&self) -> churl_core::sequence::RunScopes {
        churl_core::sequence::RunScopes {
            cli: self.cli_vars.clone(),
            profile: self.profile_vars(),
            workspace: self.workspace_vars(),
        }
    }

    /// (Re)starts the run from the top: resets rows, bumps the run generation,
    /// aborts any in-flight step, and drives the first step.
    fn start_sequence_run(&mut self) {
        if let Some(handle) = self.sequence_abort.take() {
            handle.abort();
        }
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        runner.reset_for_rerun();
        runner.run_generation += 1;
        if runner.steps.is_empty() {
            runner.finished = true;
            return;
        }
        runner.current = Some(0);
        self.drive_sequence_step(0);
    }

    /// Prepares step `index` and spawns its execution (or records a prepare
    /// failure and advances). No HTTP client (snapshot tests) leaves the step
    /// pending — deterministic and runtime-free.
    fn drive_sequence_step(&mut self, index: usize) {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            return;
        };
        let scopes = self.sequence_run_scopes();
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        let run_generation = runner.run_generation;
        let Some(row) = runner.steps.get_mut(index) else {
            return;
        };
        let step = row.step.clone();
        match churl_core::sequence::prepare_step(&root, &step, &runner.extracted, &scopes) {
            Err(err) => {
                // A prepare failure is a transport-class failure for the step.
                row.response = ResponseState::Failed {
                    error: err.to_string(),
                    meta: sequence_step_meta(&step.endpoint),
                };
                self.finish_sequence_step(index, StepResult::HttpError(err.to_string()));
            }
            Ok(prepared) => {
                row.method = prepared.method;
                row.url = prepared.url.clone();
                row.status = StepStatus::Running;
                row.response = ResponseState::InFlight {
                    started: Instant::now(),
                };
                runner.selected = index;
                runner.resp_cursor = 0;
                runner.resp_scroll = 0;
                // No client (snapshot tests): leave the step running, no spawn.
                let Some(client) = self.client.clone() else {
                    return;
                };
                let tx = self.tx.clone();
                let options = self.execute_options;
                let request = prepared.request;
                let handle = tokio::spawn(async move {
                    let outcome = churl_core::http::execute(&client, &request, &options)
                        .await
                        .map_err(|err| err.to_string());
                    let _ = tx.send(AppMsg::SequenceStep {
                        run_generation,
                        index,
                        outcome,
                    });
                });
                self.sequence_abort = Some(handle.abort_handle());
            }
        }
    }

    /// Lands a completed sequence step: drops stale results, classifies with the
    /// shared core seam, merges extracted values, and advances or finishes.
    fn on_sequence_step(
        &mut self,
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
    ) {
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        if run_generation != runner.run_generation {
            return; // stale — a cancel or re-run superseded this step
        }
        self.sequence_abort = None;
        let step = match runner.steps.get(index) {
            Some(row) => row.step.clone(),
            None => return,
        };
        let view_gen = runner.next_view_gen();

        // Classify with the shared core seam for the success branch; a transport
        // error maps to `HttpError` exactly as `classify_step` does (the one line
        // the TUI mirrors — guarded by `sequence_transition_matches_core`).
        let (result, extracted, timing, response) = match outcome {
            Ok(response) => {
                let (result, extracted) = churl_core::sequence::classify_response(&response, &step);
                let timing = Some(response.timing.total);
                let view = ResponseView::build(&response, view_gen);
                (result, extracted, timing, ResponseState::Done { view })
            }
            Err(error) => (
                StepResult::HttpError(error.clone()),
                BTreeMap::new(),
                None,
                ResponseState::Failed {
                    error,
                    meta: sequence_step_meta(&step.endpoint),
                },
            ),
        };

        // Merge extracted values into the accumulator (empty on any failure).
        for (name, value) in &extracted {
            runner.extracted.insert(name.clone(), value.clone());
        }
        if let Some(row) = runner.steps.get_mut(index) {
            row.timing = timing;
            row.extracted = extracted;
            row.response = response;
        }
        self.finish_sequence_step(index, result);
    }

    /// Applies a step's classified `result`: sets its display status, then makes
    /// the halt/advance decision through the shared `should_halt` seam — the
    /// single place the TUI mirrors core's per-step transition, so the two cannot
    /// drift. Halt marks the remaining steps `Skipped` and finishes; otherwise the
    /// run advances.
    fn finish_sequence_step(&mut self, index: usize, result: churl_core::sequence::StepResult) {
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        if let Some(row) = runner.steps.get_mut(index) {
            row.status = StepStatus::from_result(&result);
        }
        runner.selected = index;
        if churl_core::sequence::should_halt(&result, runner.on_error) {
            for row in runner.steps.iter_mut().skip(index + 1) {
                row.status = StepStatus::Skipped;
            }
            runner.current = None;
            runner.finished = true;
        } else {
            self.advance_sequence(index);
        }
    }

    /// Advances to the step after `index`, or finishes the run.
    fn advance_sequence(&mut self, index: usize) {
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        let next = index + 1;
        if next >= runner.steps.len() {
            runner.current = None;
            runner.finished = true;
            return;
        }
        runner.current = Some(next);
        self.drive_sequence_step(next);
    }

    /// Cancels the in-flight run: aborts the task, bumps the generation so a
    /// landed result is dropped, and marks every non-terminal step skipped.
    fn cancel_sequence_run(&mut self) {
        if let Some(handle) = self.sequence_abort.take() {
            handle.abort();
        }
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        runner.run_generation += 1;
        for row in &mut runner.steps {
            if matches!(row.status, StepStatus::Pending | StepStatus::Running) {
                row.status = StepStatus::Skipped;
                if matches!(row.response, ResponseState::InFlight { .. }) {
                    row.response = ResponseState::Cancelled;
                }
            }
        }
        runner.current = None;
        runner.finished = true;
        runner.confirming_close = false;
        self.notify("run cancelled");
    }

    /// Routes a key to the open sequence runner and acts on its outcome.
    fn handle_sequence_runner_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(runner) = self.sequence_runner.as_mut() else {
            self.mode = Mode::Normal;
            return Ok(());
        };
        match runner.handle_key(key) {
            RunnerOutcome::Consumed => {}
            RunnerOutcome::Rerun => self.start_sequence_run(),
            RunnerOutcome::Cancel => self.cancel_sequence_run(),
            RunnerOutcome::Close => self.close_sequence_runner(),
        }
        Ok(())
    }

    /// Closes the runner, aborting any in-flight step.
    fn close_sequence_runner(&mut self) {
        if let Some(handle) = self.sequence_abort.take() {
            handle.abort();
        }
        // Bump the generation so a straggler result is dropped after close.
        if let Some(runner) = self.sequence_runner.as_mut() {
            runner.run_generation += 1;
        }
        self.sequence_runner = None;
        self.mode = Mode::Normal;
    }

    // ---- M7.4 sequence editor ----

    /// `<leader>a` / palette: edit the sequence under the cursor, or prompt for a
    /// name to create a new one.
    fn edit_selected_sequence(&mut self) -> Result<()> {
        if self.workspace.is_none() {
            self.notify("open a workspace first");
            return Ok(());
        }
        match self.explorer.selected_sequence() {
            Some(selected) => {
                self.open_sequence_editor(selected.name, selected.file, &selected.sequence);
                Ok(())
            }
            None => {
                self.open_prompt(PromptPurpose::NewSequence, "");
                Ok(())
            }
        }
    }

    /// Workspace-relative endpoint paths for the editor's add-step picker.
    fn endpoint_rel_paths(&mut self) -> Vec<String> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            return Vec::new();
        };
        self.explorer
            .all_endpoint_files()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| {
                path.strip_prefix(&root)
                    .ok()
                    .map(|rel| rel.to_string_lossy().into_owned())
            })
            .collect()
    }

    /// Opens the editor over a loaded sequence.
    fn open_sequence_editor(
        &mut self,
        name: String,
        file: PathBuf,
        sequence: &churl_core::model::Sequence,
    ) {
        let endpoints = self.endpoint_rel_paths();
        self.sequence_editor = Some(SequenceEditorState::new(name, file, sequence, endpoints));
        self.mode = Mode::SequenceEditor;
    }

    /// Commits the "new sequence" name prompt: creates the file and opens the
    /// editor on it.
    fn commit_new_sequence(&mut self, name: String) -> Result<()> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            self.notify("no workspace open");
            return Ok(());
        };
        match persistence::create_sequence(&root, &name) {
            Ok(path) => {
                let sequence = persistence::load_sequence(&path)?;
                self.reload_explorer()?;
                self.open_sequence_editor(sequence.name.clone(), path, &sequence);
            }
            Err(err) => self.crud_error(err),
        }
        Ok(())
    }

    /// Routes a key to the open sequence editor and acts on its outcome.
    fn handle_sequence_editor_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(editor) = self.sequence_editor.as_mut() else {
            self.mode = Mode::Normal;
            return Ok(());
        };
        match editor.handle_key(key) {
            EditorOutcome::Consumed => {}
            EditorOutcome::Save => {
                self.save_sequence_editor()?;
            }
            EditorOutcome::SaveAndClose => {
                if self.save_sequence_editor()? {
                    self.close_sequence_editor();
                }
            }
            EditorOutcome::Close => self.close_sequence_editor(),
        }
        Ok(())
    }

    /// Saves the editor's sequence through the format-preserving seam and reloads
    /// the explorer so the change is visible. Returns whether the save took.
    fn save_sequence_editor(&mut self) -> Result<bool> {
        let Some(editor) = self.sequence_editor.as_ref() else {
            return Ok(false);
        };
        let path = editor.path().to_owned();
        // Validate first (duplicate rule names refuse the whole save — nothing is
        // written and the editor stays open + dirty, mirroring the M7.3 gate).
        let sequence = match editor.to_sequence_checked() {
            Ok(sequence) => sequence,
            Err(msg) => {
                self.notify(msg);
                return Ok(false);
            }
        };
        match persistence::save_sequence(&path, &sequence) {
            Ok(()) => {
                if let Some(editor) = self.sequence_editor.as_mut() {
                    editor.mark_saved();
                }
                self.reload_explorer()?;
                self.notify("sequence saved");
                Ok(true)
            }
            Err(err) => {
                self.crud_error(err);
                Ok(false)
            }
        }
    }

    /// Closes the editor and returns to normal mode.
    fn close_sequence_editor(&mut self) {
        self.sequence_editor = None;
        self.mode = Mode::Normal;
    }

    fn close_overlay(&mut self) {
        self.picker = None;
        self.profile_choices.clear();
        self.workspace_choices.clear();
        self.auth_picker = false;
        self.mode = Mode::Normal;
    }

    /// Opens the quick-jump workspace picker over the recently-opened workspaces
    /// (recency from the SQLite state DB). With no history store or an empty
    /// list, shows a message instead of an empty picker.
    fn open_workspace_picker(&mut self) {
        let Some(history) = self.history.as_ref() else {
            self.notify("no recent workspaces");
            return;
        };
        let recent = match history.recent_workspaces(20) {
            Ok(recent) => recent,
            Err(err) => {
                self.notify(format!("workspace picker failed: {err}"));
                return;
            }
        };
        if recent.is_empty() {
            self.notify("no recent workspaces");
            return;
        }
        let mut items = Vec::with_capacity(recent.len());
        let mut choices = Vec::with_capacity(recent.len());
        for path in recent {
            items.push(display_workspace_path(&path));
            choices.push(PathBuf::from(path));
        }
        self.workspace_choices = choices;
        self.picker = Some(picker::PickerState::new(" Switch workspace ", items));
        self.mode = Mode::WorkspacePicker;
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
        // Capture the profile/workspace choices + auth-picker flag before
        // close_overlay() clears them.
        let profile_choices = std::mem::take(&mut self.profile_choices);
        let workspace_choices = std::mem::take(&mut self.workspace_choices);
        let auth_picker = self.auth_picker;
        self.close_overlay();
        let Some(index) = current else {
            return Ok(());
        };
        if auth_picker {
            self.set_auth_kind(index);
            return Ok(());
        }
        match mode {
            Mode::Search => {
                if let Some(&(collection, endpoint)) = self.search_targets.get(index) {
                    self.focus = Pane::Explorer;
                    // jump_to only navigates (expand + cursor); the load itself
                    // goes through the guarded seam so dirty edits are never
                    // lost silently.
                    if let Some(selected) = self.explorer.jump_to(collection, endpoint)? {
                        self.guarded_load(PendingLoad::File(selected.file.clone()))?;
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
            Mode::WorkspacePicker => {
                // Route the switch through the dirty guard: a workspace target is
                // always "other", so unsaved edits defer to the discard confirm.
                if let Some(path) = workspace_choices.get(index).cloned() {
                    self.guarded_load(PendingLoad::Workspace(path))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Sets (or clears with `None`) the active profile. No status message —
    /// the persistent `profile: <name>` indicator in the statusline is the
    /// single source of truth (M6.5 dedup fix).
    fn set_profile(&mut self, profile: Option<String>) {
        self.active_profile = profile;
    }

    // ---- URL bar editing + method switching ----

    /// Begins URL editing via `i`/`Enter`: opens the inline editor or the popup
    /// per `url_edit_mode` (deliverable 7). A no-op when no endpoint is loaded.
    fn begin_url_edit(&mut self) {
        match self.url_edit_mode {
            UrlEditMode::Inline => self.begin_url_edit_inline(),
            UrlEditMode::Popup => self.begin_url_popup(),
        }
    }

    /// Opens the inline URL editor (seeds the LineEditor with the current URL).
    fn begin_url_edit_inline(&mut self) {
        let Some(url) = self.live_request().map(|r| r.url.clone()) else {
            self.notify("no endpoint selected");
            return;
        };
        self.set_focus(Pane::UrlBar);
        self.url_editor = Some(LineEditor::new(&url));
    }

    /// Opens the centered vim-popup URL editor (`e`, or `url_edit = "popup"`).
    fn begin_url_popup(&mut self) {
        let Some(url) = self.live_request().map(|r| r.url.clone()) else {
            self.notify("no endpoint selected");
            return;
        };
        self.set_focus(Pane::UrlBar);
        self.url_editor = None;
        self.url_popup = Some(EditorState::new(Lines::from(url.as_str())));
        self.url_popup_vim.reset();
    }

    /// Sets focus, honouring the M6.7 collapse/hide invariants: a collapsed pane
    /// (zoom) cannot hold focus — targeting it auto-unzooms first — and focusing
    /// the explorer auto-reopens it when hidden.
    fn set_focus(&mut self, pane: Pane) {
        if pane == Pane::Explorer && self.explorer_hidden {
            self.explorer_hidden = false;
        }
        // A collapsed (non-zoomed) pane cannot hold focus: auto-unzoom.
        match (self.zoom, pane) {
            (Some(ZoomPane::Request), Pane::Response) => self.zoom = None,
            (Some(ZoomPane::Response), Pane::Request) => self.zoom = None,
            _ => {}
        }
        self.focus = pane;
    }

    /// `<leader>e`: toggles the explorer sidebar. Hiding it while it is focused
    /// moves focus to the URL bar (a hidden pane cannot hold focus).
    fn toggle_explorer(&mut self) {
        self.explorer_hidden = !self.explorer_hidden;
        if self.explorer_hidden && self.focus == Pane::Explorer {
            self.focus = Pane::UrlBar;
        }
    }

    /// `z`: zooms the focused Request/Response pane (collapsing the other), or
    /// restores the split if already zoomed. No-op from other panes.
    fn toggle_zoom(&mut self) {
        let target = match self.focus {
            Pane::Request => ZoomPane::Request,
            Pane::Response => ZoomPane::Response,
            _ => return,
        };
        self.zoom = if self.zoom == Some(target) {
            None
        } else {
            Some(target)
        };
    }

    /// Commits a new URL (from the inline editor or the popup): strips the query
    /// string, merges it into the Params tab (deliverable 3), and sets the base
    /// URL. Reports the merge and marks dirty. A no-op when nothing is loaded.
    fn commit_url(&mut self, url: String) {
        let (base, pairs) = split_query(&url);
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        selected.endpoint.request.url = base;
        let report = merge_query_params(&mut selected.endpoint.request.params, &pairs);
        if let Some(report) = report {
            self.notify(format!("params: {report}"));
        }
    }

    /// Handles one key while editing the URL inline: Enter commits, Esc reverts,
    /// everything else goes to the LineEditor.
    fn handle_url_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Enter => {
                if let Some(editor) = self.url_editor.take() {
                    // Commit runs the URL→Params query merge (deliverable 3).
                    self.commit_url(editor.text());
                }
            }
            KeyCode::Esc => {
                self.url_editor = None; // revert (discard the editor's text)
            }
            _ => {
                if let Some(editor) = self.url_editor.as_mut() {
                    editor.handle_key(key);
                }
            }
        }
        Ok(())
    }

    /// Cycles the loaded request's method (GET→POST→…→GET).
    fn cycle_method(&mut self) {
        if let Some(selected) = self.selected.as_mut() {
            let m = selected.endpoint.request.method;
            selected.endpoint.request.method = m.cycle();
        } else {
            self.message = Some(Message::new("no endpoint selected"));
        }
    }

    /// Opens the one-key method-picker menu (focuses the URL bar).
    fn open_method_menu(&mut self) {
        if self.selected.is_none() {
            self.message = Some(Message::new("no endpoint selected"));
            return;
        }
        self.focus = Pane::UrlBar;
        self.mode = Mode::MethodMenu;
    }

    /// Handles one key in the method menu: a label sets the method, Esc cancels.
    fn handle_method_menu_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            return;
        }
        if let KeyCode::Char(c) = key.code
            && let Some(method) = method_menu::method_for(c)
            && let Some(selected) = self.selected.as_mut()
        {
            selected.endpoint.request.method = method;
            self.mode = Mode::Normal;
        }
        // Any other key is ignored; the menu stays open.
    }

    // ---- Request-tab rows ----

    /// The number of rows on the active tab of the live request.
    fn active_tab_row_count(&self) -> usize {
        let Some(request) = self.live_request() else {
            return 0;
        };
        match self.tabs.active {
            RequestTab::Params => request.params.len(),
            RequestTab::Headers => request.headers.len(),
            RequestTab::Auth => auth_field_count(request.auth.as_ref()),
            RequestTab::Body => 0,
        }
    }

    /// `a`: add a row on the Params/Headers tab and immediately edit its name.
    fn row_add(&mut self) {
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        let request = &mut selected.endpoint.request;
        let new_row = match self.tabs.active {
            RequestTab::Params => {
                request.params.push(Param {
                    name: String::new(),
                    value: String::new(),
                    enabled: true,
                });
                request.params.len() - 1
            }
            RequestTab::Headers => {
                request.headers.push(Header {
                    name: String::new(),
                    value: String::new(),
                    enabled: true,
                });
                request.headers.len() - 1
            }
            // Auth/Body have no add-row.
            _ => return,
        };
        self.tabs.clamp(self.active_tab_row_count());
        // Select and begin editing the new row's name field.
        match self.tabs.active {
            RequestTab::Params => self.tabs.params_sel = new_row,
            RequestTab::Headers => self.tabs.headers_sel = new_row,
            _ => {}
        }
        self.tabs.editing = Some(FieldEdit {
            row: new_row,
            field: EditField::Name,
            editor: LineEditor::new(""),
        });
    }

    /// `d`: delete the selected row on the Params/Headers tab (no confirm).
    fn row_delete(&mut self) {
        let sel = self.tabs.selection();
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        let request = &mut selected.endpoint.request;
        match self.tabs.active {
            RequestTab::Params if sel < request.params.len() => {
                request.params.remove(sel);
            }
            RequestTab::Headers if sel < request.headers.len() => {
                request.headers.remove(sel);
            }
            _ => return,
        }
        let n = self.active_tab_row_count();
        self.tabs.clamp(n);
    }

    /// `Space`: toggle the selected row's `enabled` flag (Params/Headers), or the
    /// ApiKey placement on the Auth tab's placement row.
    fn row_toggle(&mut self) {
        let sel = self.tabs.selection();
        let active = self.tabs.active;
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        let request = &mut selected.endpoint.request;
        match active {
            RequestTab::Params => {
                if let Some(param) = request.params.get_mut(sel) {
                    param.enabled = !param.enabled;
                }
            }
            RequestTab::Headers => {
                if let Some(header) = request.headers.get_mut(sel) {
                    header.enabled = !header.enabled;
                }
            }
            RequestTab::Auth => toggle_auth_placement(request.auth.as_mut(), sel),
            RequestTab::Body => {}
        }
    }

    /// `Enter`/`i`: edit the selected row. On the Auth kind row, opens the auth
    /// kind picker instead.
    fn row_edit(&mut self) {
        let sel = self.tabs.selection();
        let active = self.tabs.active;
        // The Auth-tab kind row (row 0) opens the kind picker.
        if active == RequestTab::Auth && sel == 0 {
            self.open_auth_kind_picker();
            return;
        }
        // The ApiKey placement row (row 3) toggles on Enter, same as Space (the
        // pinned design: "placement row toggles header/query with Space/Enter").
        if active == RequestTab::Auth && sel == 3 {
            self.row_toggle();
            return;
        }
        // Auth fields have fixed labels — edit the single value directly (no
        // name→value advance). Param/Header rows start on the name field.
        let start_field = if active == RequestTab::Auth {
            EditField::Value
        } else {
            EditField::Name
        };
        let Some(text) = self.current_field_text(active, sel, start_field) else {
            return;
        };
        self.tabs.editing = Some(FieldEdit {
            row: sel,
            field: start_field,
            editor: LineEditor::new(&text),
        });
    }

    /// The current text of a given row/field on `tab`, or `None` when out of range
    /// or not editable.
    fn current_field_text(&self, tab: RequestTab, row: usize, field: EditField) -> Option<String> {
        let request = self.live_request()?;
        let (name, value) = match tab {
            RequestTab::Params => {
                let p = request.params.get(row)?;
                (p.name.clone(), p.value.clone())
            }
            RequestTab::Headers => {
                let h = request.headers.get(row)?;
                (h.name.clone(), h.value.clone())
            }
            RequestTab::Auth => return auth_field_text(request.auth.as_ref(), row),
            RequestTab::Body => return None,
        };
        Some(match field {
            EditField::Name => name,
            EditField::Value => value,
        })
    }

    /// Handles one key during an in-progress row field edit. Tab/Enter advance
    /// name→value (or commit the row on the value field); Esc cancels.
    fn handle_field_edit_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                // Cancel the field edit; a freshly-added row that was never
                // committed (name and value both still empty) is removed, or
                // `a` + Esc would leave a nameless ghost row that serializes.
                if let Some(edit) = self.tabs.editing.take() {
                    self.discard_row_if_empty(edit.row);
                }
            }
            KeyCode::Tab => self.field_edit_advance(false),
            KeyCode::Enter => self.field_edit_advance(true),
            _ => {
                if let Some(edit) = self.tabs.editing.as_mut() {
                    edit.editor.handle_key(key);
                }
            }
        }
        Ok(())
    }

    /// Removes a Params/Headers row whose stored name *and* value are both
    /// empty — the ghost a cancelled `a`(dd) would otherwise leave behind (it is
    /// nameless, enabled, and would serialize on save).
    fn discard_row_if_empty(&mut self, row: usize) {
        let active = self.tabs.active;
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        let request = &mut selected.endpoint.request;
        let removed = match active {
            RequestTab::Params
                if request
                    .params
                    .get(row)
                    .is_some_and(|p| p.name.is_empty() && p.value.is_empty()) =>
            {
                request.params.remove(row);
                true
            }
            RequestTab::Headers
                if request
                    .headers
                    .get(row)
                    .is_some_and(|h| h.name.is_empty() && h.value.is_empty()) =>
            {
                request.headers.remove(row);
                true
            }
            _ => false,
        };
        if removed {
            let n = self.active_tab_row_count();
            self.tabs.clamp(n);
        }
    }

    /// Commits the current field edit into the live request. `commit_row` closes
    /// the edit after the value field; otherwise name→value advances.
    fn field_edit_advance(&mut self, commit_row: bool) {
        let Some(edit) = self.tabs.editing.take() else {
            return;
        };
        let text = edit.editor.text();
        self.write_field(self.tabs.active, edit.row, edit.field, text);
        match edit.field {
            EditField::Name => {
                // Advance to the value field, seeded with its current text.
                let value = self
                    .current_field_text(self.tabs.active, edit.row, EditField::Value)
                    .unwrap_or_default();
                self.tabs.editing = Some(FieldEdit {
                    row: edit.row,
                    field: EditField::Value,
                    editor: LineEditor::new(&value),
                });
            }
            EditField::Value => {
                if !commit_row {
                    // Tab from value wraps back to name.
                    let name = self
                        .current_field_text(self.tabs.active, edit.row, EditField::Name)
                        .unwrap_or_default();
                    self.tabs.editing = Some(FieldEdit {
                        row: edit.row,
                        field: EditField::Name,
                        editor: LineEditor::new(&name),
                    });
                }
                // Enter on value: editing already taken → committed.
            }
        }
    }

    /// Writes an edited field back into the live request.
    fn write_field(&mut self, tab: RequestTab, row: usize, field: EditField, text: String) {
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        let request = &mut selected.endpoint.request;
        match tab {
            RequestTab::Params => {
                if let Some(p) = request.params.get_mut(row) {
                    match field {
                        EditField::Name => p.name = text,
                        EditField::Value => p.value = text,
                    }
                }
            }
            RequestTab::Headers => {
                if let Some(h) = request.headers.get_mut(row) {
                    match field {
                        EditField::Name => h.name = text,
                        EditField::Value => h.value = text,
                    }
                }
            }
            RequestTab::Auth => write_auth_field(request.auth.as_mut(), row, field, text),
            RequestTab::Body => {}
        }
    }

    /// Opens the auth-kind picker (None / Basic / Bearer / ApiKey).
    fn open_auth_kind_picker(&mut self) {
        if self.selected.is_none() {
            return;
        }
        let labels = vec![
            "None".to_owned(),
            "Basic".to_owned(),
            "Bearer".to_owned(),
            "ApiKey".to_owned(),
        ];
        self.picker = Some(picker::PickerState::new(" Auth kind ", labels));
        self.auth_picker = true;
        self.mode = Mode::Palette;
    }

    /// Applies an auth-kind picker choice, swapping in default-empty fields.
    fn set_auth_kind(&mut self, index: usize) {
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        let auth = &mut selected.endpoint.request.auth;
        *auth = match index {
            0 => None,
            1 => Some(Auth::Basic {
                username: String::new(),
                password: String::new(),
            }),
            2 => Some(Auth::Bearer {
                token: String::new(),
            }),
            3 => Some(Auth::ApiKey {
                name: String::new(),
                value: String::new(),
                placement: ApiKeyPlacement::Header,
            }),
            _ => return,
        };
        self.tabs.auth_sel = 0;
    }

    // ---- Save ----

    /// `w`: save the live request to disk (format-preserving). Refreshes the
    /// snapshot on success; a secrets refusal surfaces on the statusline and the
    /// request stays dirty.
    fn save_request(&mut self) {
        self.sync_body_into_selected();
        let Some(selected) = self.selected.as_ref() else {
            self.message = Some(Message::new("no endpoint to save"));
            return;
        };
        let path = selected.file.clone();
        let endpoint = selected.endpoint.clone();
        match persistence::save_endpoint(&path, &endpoint) {
            Ok(()) => {
                self.loaded_snapshot = Some(endpoint.clone());
                self.refresh_explorer_endpoint(&path, endpoint.clone());
                self.message = Some(Message::new(format!("Saved {}", endpoint.name)));
            }
            Err(PersistenceError::SecretsInAuth { names }) => {
                self.message = Some(Message::new(format!(
                    "not saved: secret auth values ({}) — use {{{{var}}}}",
                    names.join(", ")
                )));
            }
            Err(err) => {
                self.message = Some(Message::new(format!("save failed: {err}")));
            }
        }
    }

    /// Updates the explorer's cached copy of an endpoint after a save so the tree
    /// name and any re-selection stay consistent.
    fn refresh_explorer_endpoint(&mut self, path: &Path, endpoint: Endpoint) {
        self.explorer.update_endpoint(path, endpoint);
    }

    // ---- CRUD (explorer) ----

    /// `n`: prompt for a new endpoint name (under the selected collection).
    fn begin_new_endpoint(&mut self) {
        if self.explorer.selected_collection_dir().is_none() {
            self.message = Some(Message::new("select a collection first"));
            return;
        }
        self.open_prompt(PromptPurpose::NewEndpoint, "");
    }

    /// `N`: prompt for a new collection name.
    fn begin_new_collection(&mut self) {
        if self.workspace.is_none() {
            self.message = Some(Message::new("no workspace open"));
            return;
        }
        self.open_prompt(PromptPurpose::NewCollection, "");
    }

    /// `r`: prompt to rename the selected endpoint or collection.
    fn begin_rename(&mut self) {
        let Some(name) = self.explorer.selected_name() else {
            self.message = Some(Message::new("nothing selected to rename"));
            return;
        };
        self.open_prompt(PromptPurpose::Rename, &name);
    }

    /// `d`: delete the selected item — y/n for an endpoint, typed name for a
    /// collection (risk-proportional friction).
    fn begin_delete(&mut self) {
        match self.explorer.selected_kind() {
            Some(RowKind::Endpoint) => {
                self.mode = Mode::Confirm(ConfirmPurpose::DeleteEndpoint);
            }
            Some(RowKind::Collection) => {
                self.open_prompt(PromptPurpose::DeleteCollectionConfirm, "");
            }
            Some(RowKind::Sequence) | Some(RowKind::SequenceHeader) | None => {
                self.message = Some(Message::new("nothing selected to delete"));
            }
        }
    }

    /// Opens a text prompt seeded with `seed`.
    fn open_prompt(&mut self, purpose: PromptPurpose, seed: &str) {
        self.prompt_editor = LineEditor::new(seed);
        self.mode = Mode::Prompt(purpose);
    }

    // ---- M7.1 collection interchange ----

    /// Palette: prompt for a JSON collection file path to import.
    fn begin_import_collection(&mut self) {
        if self.workspace.is_none() {
            self.notify("no workspace open");
            return;
        }
        self.open_prompt(PromptPurpose::ImportCollection, "");
    }

    /// Palette: prompt for an export destination for the selected collection.
    fn begin_export_collection(&mut self, dialect: JsonDialect) {
        let Some(name) = self.selected_collection_name() else {
            self.notify("select a collection first");
            return;
        };
        let seed = default_export_path(&name);
        self.open_prompt(PromptPurpose::ExportCollection(dialect), &seed);
    }

    /// Palette: prompt for an export destination for the whole workspace.
    fn begin_export_workspace(&mut self, dialect: JsonDialect) {
        let Some(ws) = self.workspace.as_ref() else {
            self.notify("no workspace open");
            return;
        };
        let seed = default_export_path(&ws.manifest().name);
        self.open_prompt(PromptPurpose::ExportWorkspace(dialect), &seed);
    }

    /// Palette: prompt for a curl command to import as a new endpoint.
    fn begin_paste_curl(&mut self) {
        if self.explorer.selected_collection_dir().is_none() {
            self.notify("select a collection first");
            return;
        }
        self.open_prompt(PromptPurpose::PasteCurl, "");
    }

    /// The display name of the collection the selection belongs to (a collection
    /// row itself, or the collection owning the selected endpoint).
    fn selected_collection_name(&self) -> Option<String> {
        let dir = self.explorer.selected_collection_dir()?;
        dir.file_name().and_then(|n| n.to_str()).map(str::to_owned)
    }

    /// Loads the selected collection's endpoints from disk (name + endpoints).
    fn selected_collection_endpoints(&self) -> Option<(String, Vec<Endpoint>)> {
        let dir = self.explorer.selected_collection_dir()?;
        let name = dir.file_name()?.to_str()?.to_owned();
        let collection = persistence::Collection {
            name: name.clone(),
            path: dir,
        };
        let endpoints = collection
            .endpoints_lenient()
            .ok()?
            .endpoints
            .into_iter()
            .map(|(_, ep)| ep)
            .collect();
        Some((name, endpoints))
    }

    /// `C` / palette: copy the loaded request as a curl one-liner. `resolved`
    /// substitutes `{{var}}`s first (secrets caution — the explicit opt-in).
    fn copy_as_curl(&mut self, resolved: bool) {
        let Some(selected) = self.selected.clone() else {
            self.notify("no endpoint selected");
            return;
        };
        let mut endpoint = selected.endpoint.clone();
        // Fold in any unsaved edtui body edits so the copy matches what's shown.
        let body_text = String::from(self.editor.lines.clone());
        match endpoint.request.body.as_mut() {
            Some(body) => body.content = body_text,
            None if !body_text.is_empty() => {
                endpoint.request.body = Some(Body {
                    kind: BodyKind::Text,
                    content: body_text,
                });
            }
            None => {}
        }
        if resolved {
            self.build_resolver(&selected)
                .substitute_request(&mut endpoint.request);
        }
        let curl = churl_core::export::export_curl(&endpoint);
        self.enqueue_clipboard(&curl);
        if resolved {
            self.notify("copied curl (vars resolved — may contain secrets)");
        } else {
            self.notify("copied curl");
        }
    }

    /// Commits an import-collection prompt: read the file, map it, and write the
    /// endpoints into the workspace (shared core `write_import`).
    fn commit_import_collection(&mut self, path: String) -> Result<()> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            self.notify("no workspace open");
            return Ok(());
        };
        let path = path.trim();
        if path.is_empty() {
            self.notify("no path given");
            return Ok(());
        }
        let json = match std::fs::read_to_string(path) {
            Ok(json) => json,
            Err(err) => {
                self.notify(format!("import failed: cannot read {path}: {err}"));
                return Ok(());
            }
        };
        let import = match interchange::import_postman_v21(&json) {
            Ok(import) => import,
            Err(err) => {
                self.notify(format!("import failed: {err}"));
                return Ok(());
            }
        };
        match interchange::write_import(&root, &import) {
            Ok(summary) => {
                self.reload_explorer()?;
                let mut msg = format!(
                    "imported {} endpoint(s) into {} collection(s)",
                    summary.endpoints, summary.collections
                );
                if !summary.warnings.is_empty() {
                    msg.push_str(&format!(" ({} warning(s))", summary.warnings.len()));
                }
                self.notify(msg);
            }
            Err(err) => self.notify(format!("import failed: {err}")),
        }
        Ok(())
    }

    /// Commits an export prompt for the given `scope`+`dialect` to `path` (which
    /// must resolve inside the workspace root).
    fn commit_export(&mut self, purpose: PromptPurpose, path: String) {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            self.notify("no workspace open");
            return;
        };
        let target = match export_target(&root, &path) {
            Ok(target) => target,
            Err(err) => {
                self.notify(format!("export failed: {err}"));
                return;
            }
        };
        // Build the export string per scope + dialect.
        let contents = match purpose {
            PromptPurpose::ExportCollection(dialect) => {
                let Some((name, endpoints)) = self.selected_collection_endpoints() else {
                    self.notify("select a collection first");
                    return;
                };
                interchange::export_collection(&name, &endpoints, dialect)
            }
            PromptPurpose::ExportWorkspace(dialect) => {
                let ws = self
                    .workspace
                    .as_ref()
                    .expect("root came from the workspace");
                interchange::export_workspace(ws, dialect)
            }
            _ => return,
        };
        let contents = match contents {
            Ok(contents) => contents,
            Err(err) => {
                self.notify(format!("export failed: {err}"));
                return;
            }
        };
        if let Some(parent) = target.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            self.notify(format!("export failed: {err}"));
            return;
        }
        match std::fs::write(&target, contents) {
            Ok(()) => {
                let shown = target.strip_prefix(&root).unwrap_or(&target);
                self.notify(format!("exported to {}", shown.display()));
            }
            Err(err) => self.notify(format!("export failed: {err}")),
        }
    }

    /// Commits a paste-curl prompt: import the curl command and create an
    /// endpoint in the selected collection.
    fn commit_paste_curl(&mut self, curl: String) -> Result<()> {
        let Some(dir) = self.explorer.selected_collection_dir() else {
            self.notify("select a collection first");
            return Ok(());
        };
        let result = match churl_core::import::import_curl(&curl) {
            Ok(result) => result,
            Err(err) => {
                self.notify(format!("curl import failed: {err}"));
                return Ok(());
            }
        };
        let path = match persistence::create_endpoint(&dir, &result.endpoint.name) {
            Ok(path) => path,
            Err(err) => {
                self.crud_error(err);
                return Ok(());
            }
        };
        // Overwrite the default endpoint with the imported request, keeping the
        // collection-assigned seq.
        let mut endpoint = result.endpoint;
        endpoint.seq = persistence::load_endpoint(&path)
            .map(|e| e.seq)
            .unwrap_or(0);
        if let Err(err) = persistence::save_endpoint(&path, &endpoint) {
            self.crud_error(err);
            return Ok(());
        }
        self.reload_explorer()?;
        let mut msg = format!("pasted curl → {}", endpoint.name);
        if !result.warnings.is_empty() {
            msg.push_str(&format!(" ({} warning(s))", result.warnings.len()));
        }
        self.notify(msg);
        // Guarded: loading the new endpoint must not silently discard dirty edits.
        self.guarded_load(PendingLoad::File(path))?;
        Ok(())
    }

    /// Handles one key in a text-prompt overlay.
    fn handle_prompt_key(&mut self, key: KeyEvent, purpose: PromptPurpose) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let text = self.prompt_editor.text();
                self.mode = Mode::Normal;
                self.commit_prompt(purpose, text)?;
            }
            _ => {
                self.prompt_editor.handle_key(key);
            }
        }
        Ok(())
    }

    /// Commits a text prompt: performs the CRUD op via the core seams.
    fn commit_prompt(&mut self, purpose: PromptPurpose, text: String) -> Result<()> {
        match purpose {
            PromptPurpose::NewEndpoint => {
                let Some(dir) = self.explorer.selected_collection_dir() else {
                    return Ok(());
                };
                match persistence::create_endpoint(&dir, &text) {
                    Ok(path) => {
                        self.reload_explorer()?;
                        self.message = Some(Message::new(format!("created {text}")));
                        // Guarded: loading the new endpoint must not silently
                        // discard dirty edits on the currently-loaded one.
                        self.guarded_load(PendingLoad::File(path))?;
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            PromptPurpose::NewCollection => {
                let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
                    return Ok(());
                };
                match persistence::create_collection(&root, &text) {
                    Ok(_) => {
                        self.reload_explorer()?;
                        self.message = Some(Message::new(format!("created {text}")));
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            PromptPurpose::Rename => self.commit_rename(text)?,
            PromptPurpose::DeleteCollectionConfirm => {
                let Some((dir, name)) = self
                    .explorer
                    .selected_collection_dir()
                    .zip(self.explorer.selected_name())
                else {
                    return Ok(());
                };
                if text != name {
                    self.message = Some(Message::new("name mismatch — not deleted"));
                    return Ok(());
                }
                match persistence::delete_collection(&dir) {
                    Ok(()) => {
                        // reload_explorer's remap clears a selection whose file
                        // vanished with the collection.
                        self.reload_explorer()?;
                        self.message = Some(Message::new(format!("deleted {name}")));
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            PromptPurpose::ImportCollection => self.commit_import_collection(text)?,
            PromptPurpose::ExportCollection(_) | PromptPurpose::ExportWorkspace(_) => {
                self.commit_export(purpose, text)
            }
            PromptPurpose::PasteCurl => self.commit_paste_curl(text)?,
            PromptPurpose::NewSequence => self.commit_new_sequence(text)?,
        }
        Ok(())
    }

    /// Renames the selected endpoint or collection to `new_name`.
    fn commit_rename(&mut self, new_name: String) -> Result<()> {
        match self.explorer.selected_kind() {
            Some(RowKind::Endpoint) => {
                let Some(path) = self.explorer.selected_endpoint_file() else {
                    return Ok(());
                };
                match persistence::rename_endpoint(&path, &new_name) {
                    Ok(new_path) => {
                        let renamed_loaded = self.selected.as_ref().is_some_and(|s| s.file == path);
                        if renamed_loaded {
                            // Renaming the *loaded* endpoint: update its file
                            // path + name in place so unsaved edits survive —
                            // no reload of the request pane. Repoint before the
                            // reload so remap-by-path sees the live file.
                            let trimmed = new_name.trim().to_owned();
                            if let Some(selected) = self.selected.as_mut() {
                                selected.file = new_path.clone();
                                selected.endpoint.name = trimmed.clone();
                            }
                            if let Some(snapshot) = self.loaded_snapshot.as_mut() {
                                snapshot.name = trimmed;
                            }
                            self.reload_explorer()?;
                            // Move the cursor onto the renamed row without
                            // touching the request pane.
                            self.explorer.select_file(&new_path)?;
                        } else {
                            self.reload_explorer()?;
                            // Guarded: selecting the renamed endpoint must not
                            // silently discard dirty edits on the loaded one.
                            self.guarded_load(PendingLoad::File(new_path))?;
                        }
                        self.message = Some(Message::new(format!("renamed to {new_name}")));
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            Some(RowKind::Collection) => {
                let Some(dir) = self.explorer.selected_collection_dir() else {
                    return Ok(());
                };
                match persistence::rename_collection(&dir, &new_name) {
                    Ok(new_dir) => {
                        // The loaded endpoint may live inside the renamed
                        // collection: repoint its file into the new directory
                        // *before* the reload, or remap-by-path would see a
                        // vanished file (and the next save would fail NotFound).
                        if let Some(selected) = self.selected.as_mut()
                            && let Ok(rest) = selected.file.strip_prefix(&dir)
                        {
                            selected.file = new_dir.join(rest);
                        }
                        self.reload_explorer()?;
                        self.message = Some(Message::new(format!("renamed to {new_name}")));
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            Some(RowKind::Sequence) | Some(RowKind::SequenceHeader) | None => {}
        }
        Ok(())
    }

    /// Handles one key in a y/n confirmation overlay.
    fn handle_confirm_key(&mut self, key: KeyEvent, purpose: ConfirmPurpose) -> Result<()> {
        match purpose {
            ConfirmPurpose::DeleteEndpoint => match key.code {
                KeyCode::Char('y') => {
                    self.mode = Mode::Normal;
                    if let Some(path) = self.explorer.selected_endpoint_file() {
                        match persistence::delete_endpoint(&path) {
                            Ok(()) => {
                                // reload_explorer's remap clears a selection
                                // whose file vanished.
                                self.reload_explorer()?;
                                self.message = Some(Message::new("deleted endpoint"));
                            }
                            Err(err) => self.crud_error(err),
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => self.mode = Mode::Normal,
                _ => {}
            },
            ConfirmPurpose::DiscardChanges => match key.code {
                KeyCode::Char('s') => {
                    self.mode = Mode::Normal;
                    self.save_request();
                    // Only switch when the save actually took: a refused save
                    // (e.g. literal secret auth) leaves the request dirty and
                    // the error on the statusline — switching anyway would
                    // destroy the unsaved edits it just refused to write.
                    if !self.is_dirty() {
                        if let Some(target) = self.pending_load.take() {
                            self.perform_load(target)?;
                        }
                    } else {
                        self.pending_load = None; // stay put, error visible
                    }
                }
                KeyCode::Char('d') => {
                    self.mode = Mode::Normal;
                    // Discard: drop the snapshot so the switch is not re-guarded.
                    self.loaded_snapshot = None;
                    if let Some(target) = self.pending_load.take() {
                        self.perform_load(target)?;
                    }
                }
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.pending_load = None;
                }
                _ => {}
            },
        }
        Ok(())
    }

    /// Surfaces a CRUD [`PersistenceError`] on the statusline (fail-loud).
    fn crud_error(&mut self, err: PersistenceError) {
        self.message = Some(Message::new(format!("error: {err}")));
    }

    /// Rebuilds the explorer tree from disk, preserving expansion + cursor as best
    /// it can (cursor clamps to the new row count), then re-derives the loaded
    /// endpoint's indices from its file path (see [`App::remap_selected`]).
    fn reload_explorer(&mut self) -> Result<()> {
        self.explorer.reload(self.workspace.as_ref())?;
        self.remap_selected();
        self.surface_explorer_warnings();
        Ok(())
    }

    /// Re-derives the loaded endpoint's explorer indices from its file path
    /// after a tree reload. Collections are name-sorted, so creating, renaming,
    /// or deleting *another* collection shifts indices — a stale
    /// `selected.collection` would silently read the wrong collection's
    /// `folder.toml` vars at send time. A vanished file clears the selection
    /// (the post-delete case).
    fn remap_selected(&mut self) {
        let Some(selected) = self.selected.as_mut() else {
            return;
        };
        if !selected.file.exists() {
            self.selected = None;
            self.loaded_snapshot = None;
            self.editor = EditorState::default();
            return;
        }
        if let Some(ci) = self.explorer.collection_index_for_file(&selected.file) {
            selected.collection = ci;
        }
    }

    /// Moves the explorer cursor onto the endpoint at `file` (expanding its
    /// collection) and loads it into the request pane.
    fn select_endpoint_file(&mut self, file: &Path) -> Result<()> {
        if let Some(selected) = self.explorer.select_file(file)? {
            self.load_endpoint(selected);
        }
        Ok(())
    }

    /// The single guarded endpoint-switch seam: every path that replaces the
    /// loaded endpoint (explorer Enter, jump-mode row, search overlay, CRUD
    /// reselect) goes through here. When the loaded endpoint has unsaved changes
    /// and the target is a *different* endpoint, the load is deferred behind a
    /// [`ConfirmPurpose::DiscardChanges`] overlay with the target parked in
    /// `pending_load`; otherwise it loads immediately.
    fn guarded_load(&mut self, target: PendingLoad) -> Result<()> {
        if self.is_dirty() {
            if self.target_is_other(&target) {
                self.pending_load = Some(target);
                self.mode = Mode::Confirm(ConfirmPurpose::DiscardChanges);
                return Ok(());
            }
            // Reselecting the loaded endpoint while dirty must not silently
            // revert it from disk — keep the in-memory edits. (Collection rows
            // fall through: toggling expansion needs no guard.)
            if self.target_is_same_endpoint(&target) {
                return Ok(());
            }
        }
        self.perform_load(target)
    }

    /// Whether `target` refers to the endpoint that is already loaded.
    fn target_is_same_endpoint(&self, target: &PendingLoad) -> bool {
        let Some(selected) = self.selected.as_ref() else {
            return false;
        };
        match target {
            PendingLoad::Row(_) => self
                .explorer
                .selected_endpoint_file()
                .is_some_and(|file| file == selected.file),
            PendingLoad::File(file) => *file == selected.file,
            // A workspace switch is never "the same endpoint".
            PendingLoad::Workspace(_) => false,
        }
    }

    /// Whether `target` refers to a different endpoint than the loaded one. A
    /// collection row is never "other" (toggling needs no guard).
    fn target_is_other(&self, target: &PendingLoad) -> bool {
        match target {
            PendingLoad::Row(_) => self
                .explorer
                .cursor_is_other_endpoint(self.selected.as_ref()),
            PendingLoad::File(file) => self.selected.as_ref().is_none_or(|s| &s.file != file),
            // A workspace switch always replaces the loaded endpoint's context, so
            // a dirty switch must defer to the discard-changes confirm.
            PendingLoad::Workspace(_) => true,
        }
    }

    /// Performs a (possibly previously deferred) endpoint load.
    fn perform_load(&mut self, target: PendingLoad) -> Result<()> {
        match target {
            PendingLoad::Row(row) => {
                self.explorer.cursor = row;
                if let Some(selected) = self.explorer.select()? {
                    self.load_endpoint(selected);
                }
            }
            PendingLoad::File(file) => self.select_endpoint_file(&file)?,
            PendingLoad::Workspace(path) => self.switch_workspace(path)?,
        }
        Ok(())
    }

    /// Switches the whole app to the workspace rooted at `path` (quick-jump
    /// workspace picker). Opens the new manifest, rebuilds the explorer, and
    /// resets every endpoint/workspace-scoped field so nothing from the old
    /// workspace leaks in. On a failed open, the current state is left intact and
    /// the error is surfaced (fail loudly, never wipe on failure).
    fn switch_workspace(&mut self, path: PathBuf) -> Result<()> {
        let new_ws = match OpenWorkspace::open(&path) {
            Ok(ws) => ws,
            Err(err) => {
                self.notify(format!("failed to open workspace: {err}"));
                return Ok(());
            }
        };
        let name = new_ws.manifest().name.clone();

        // Abort any in-flight request from the old workspace (its response is no
        // longer relevant); dropping the handle also drops the stale generation.
        if let Some(in_flight) = self.in_flight.take() {
            in_flight.handle.abort();
        }

        // Swap in the new workspace and rebuild the explorer against it.
        self.workspace = Some(new_ws);
        self.explorer.reload(self.workspace.as_ref())?;
        self.explorer.cursor = 0;

        // Reset all endpoint/workspace-scoped state.
        self.selected = None;
        self.loaded_snapshot = None; // clears derived dirty state
        self.editor = EditorState::default();
        self.editor_vim.reset();
        self.tabs = RequestTabs::default();
        self.url_editor = None;
        self.url_popup = None;
        // The active profile is defined per-workspace; a stale name could
        // accidentally resolve against the new workspace's profiles.
        self.active_profile = None;
        self.response = ResponseState::Idle;
        self.response_scroll = 0;
        self.response_cursor = 0;
        self.response_total_rows = 0;
        self.pending_highlight = None;
        self.highlight_cache.clear();
        self.pending_load = None;
        self.zoom = None;
        // set_focus(Explorer) also un-hides the explorer if it was hidden.
        self.set_focus(Pane::Explorer);

        // Record the switch in the recency table (canonical path, deduped).
        if let Some(store) = self.history.as_ref() {
            let canonical = canonical_path(&path);
            let _ = store.touch_workspace(&canonical.to_string_lossy(), now_ms());
        }
        self.notify(format!("switched to {name}"));
        Ok(())
    }
}

/// Splits a URL into its base (everything before `?`) and the decoded
/// `(name, value)` query pairs. A pair with no `=` yields an empty value; a
/// trailing/empty segment is skipped. A URL without `?` yields an empty pair list.
fn split_query(url: &str) -> (String, Vec<(String, String)>) {
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_owned(), Vec::new());
    };
    let pairs = query
        .split('&')
        .filter(|seg| !seg.is_empty())
        .map(|seg| match seg.split_once('=') {
            Some((name, value)) => (percent_decode(name.trim()), percent_decode(value.trim())),
            None => (percent_decode(seg.trim()), String::new()),
        })
        .collect();
    (base.to_owned(), pairs)
}

/// Minimal `application/x-www-form-urlencoded` decoding: `+` → space and `%XX`
/// hex escapes; invalid escapes are passed through verbatim.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Merges committed URL query `pairs` into the request `params` per the M6.7
/// deliverable-3 policy, returning a human-readable report (`"A updated, B added"`)
/// or `None` when nothing changed:
/// - (a) exact `name=value` row exists → ensure enabled (no duplicate);
/// - (b) name exists with a different value → the first such row gets the new
///   value + enabled;
/// - (c) name absent → append an enabled row;
/// - (d) duplicate names within the URL map positionally onto existing rows of
///   that name (extras appended), preserving multi-value params.
fn merge_query_params(params: &mut Vec<Param>, pairs: &[(String, String)]) -> Option<String> {
    let mut added: Vec<String> = Vec::new();
    let mut updated: Vec<String> = Vec::new();
    // Track how many rows of each name we have already claimed positionally, so
    // `?tag=a&tag=b` maps onto the 1st then 2nd existing `tag` row (rule d).
    let mut claimed: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for (name, value) in pairs {
        // (a) an exact name+value row already exists → ensure enabled.
        if let Some(row) = params
            .iter_mut()
            .find(|p| &p.name == name && &p.value == value)
        {
            if !row.enabled {
                row.enabled = true;
                updated.push(name.clone());
            }
            // Count this exact row as claimed for positional mapping.
            *claimed.entry(name.clone()).or_insert(0) += 1;
            continue;
        }
        // (b)/(d): find the Nth existing row of this name not yet claimed.
        let skip = *claimed.get(name).unwrap_or(&0);
        let target = params.iter_mut().filter(|p| &p.name == name).nth(skip);
        if let Some(row) = target {
            row.value = value.clone();
            row.enabled = true;
            updated.push(name.clone());
            *claimed.entry(name.clone()).or_insert(0) += 1;
        } else {
            // (c) name absent (or all rows of it claimed → extra) → append.
            params.push(Param {
                name: name.clone(),
                value: value.clone(),
                enabled: true,
            });
            added.push(name.clone());
            *claimed.entry(name.clone()).or_insert(0) += 1;
        }
    }

    if added.is_empty() && updated.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if !updated.is_empty() {
        parts.push(format!("{} updated", updated.join(", ")));
    }
    if !added.is_empty() {
        parts.push(format!("{} added", added.join(", ")));
    }
    Some(parts.join(", "))
}

/// The number of editable rows on the Auth tab for a given auth (row 0 is always
/// the kind row).
fn auth_field_count(auth: Option<&Auth>) -> usize {
    match auth {
        None => 1,                      // kind row only
        Some(Auth::Basic { .. }) => 3,  // kind + username + password
        Some(Auth::Bearer { .. }) => 2, // kind + token
        Some(Auth::ApiKey { .. }) => 4, // kind + name + value + placement
    }
}

/// The text of an Auth-tab row's value field (row 0 is the kind row, not text).
fn auth_field_text(auth: Option<&Auth>, row: usize) -> Option<String> {
    match (auth, row) {
        (Some(Auth::Basic { username, .. }), 1) => Some(username.clone()),
        (Some(Auth::Basic { password, .. }), 2) => Some(password.clone()),
        (Some(Auth::Bearer { token }), 1) => Some(token.clone()),
        (Some(Auth::ApiKey { name, .. }), 1) => Some(name.clone()),
        (Some(Auth::ApiKey { value, .. }), 2) => Some(value.clone()),
        _ => None,
    }
}

/// Writes an edited Auth-tab field back (both name+value edits land in the value
/// column here — auth fields have fixed labels, so `field` is ignored and the
/// text always replaces the row's single editable value).
fn write_auth_field(auth: Option<&mut Auth>, row: usize, _field: EditField, text: String) {
    match (auth, row) {
        (Some(Auth::Basic { username, .. }), 1) => *username = text,
        (Some(Auth::Basic { password, .. }), 2) => *password = text,
        (Some(Auth::Bearer { token }), 1) => *token = text,
        (Some(Auth::ApiKey { name, .. }), 1) => *name = text,
        (Some(Auth::ApiKey { value, .. }), 2) => *value = text,
        _ => {}
    }
}

/// Toggles the ApiKey placement on the Auth tab's placement row (row 3).
fn toggle_auth_placement(auth: Option<&mut Auth>, row: usize) {
    if let (Some(Auth::ApiKey { placement, .. }), 3) = (auth, row) {
        *placement = match placement {
            ApiKeyPlacement::Header => ApiKeyPlacement::Query,
            ApiKeyPlacement::Query => ApiKeyPlacement::Header,
        };
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
    // The dedicated message row (deliverable 9) sits directly above the
    // statusline and only occupies a row while a message is live — so the
    // statusline never moves and its content is never covered.
    // While the body-search input is open it occupies the message-row position
    // (vim-style `/query`), shadowing any transient message.
    let body_search_input: Option<String> = (app.mode == Mode::BodySearch).then(|| {
        let q = app.body_search_editor.text();
        let matches = match &app.response {
            ResponseState::Done { view } => view.search().map(|s| s.count()).unwrap_or(0),
            _ => 0,
        };
        if q.is_empty() {
            "/".to_owned()
        } else if matches == 0 {
            format!("/{q}    no matches")
        } else {
            format!("/{q}    {matches} matches")
        }
    });
    let message_text = body_search_input
        .clone()
        .or_else(|| app.message.as_ref().map(|m| m.text.clone()));
    let (main, msg_area, status) = if message_text.is_some() {
        let [main, msg, status] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        (main, Some(msg), status)
    } else {
        let [main, status] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        (main, None, status)
    };
    // Two-column layout: Explorer left, column B right. The explorer is a
    // *narrow* column (owner prompt): fixed 30 cols — Min(24)+Fill would grow
    // the explorer to half the screen (ratatui distributes excess into Min).
    // When hidden (deliverable 5) the right column takes the full width.
    let (explorer_area, right_area) = if app.explorer_hidden {
        (None, main)
    } else {
        let [explorer_area, right_area] =
            Layout::horizontal([Constraint::Length(30), Constraint::Fill(1)]).areas(main);
        (Some(explorer_area), right_area)
    };
    // Column B split into three rows: URL bar (3 lines) / Request / Response.
    // Zoom (deliverable 4) collapses the unfocused pane to a bordered stub
    // (border top + summary line + border bottom) keeping its title and
    // tab-bar/stats content visible.
    const COLLAPSED_HEIGHT: u16 = 3;
    let remaining = right_area.height.saturating_sub(urlbar::HEIGHT);
    let (req_height, resp_height) = match app.zoom {
        Some(ZoomPane::Request) => (remaining.saturating_sub(COLLAPSED_HEIGHT), COLLAPSED_HEIGHT),
        Some(ZoomPane::Response) => (COLLAPSED_HEIGHT, remaining.saturating_sub(COLLAPSED_HEIGHT)),
        None => {
            let req = remaining / 2;
            (req, remaining - req)
        }
    };
    let [urlbar_area, request_area, response_area] = Layout::vertical([
        Constraint::Length(urlbar::HEIGHT),
        Constraint::Length(req_height),
        Constraint::Length(resp_height),
    ])
    .areas(right_area);

    let has_ws = app.workspace.is_some();
    let explorer_focused = app.focus == Pane::Explorer && app.mode == Mode::Normal;
    let theme = app.theme.clone();
    let dirty = app.is_dirty();
    // The loaded endpoint's file while dirty — its explorer row gets the accent
    // ● suffix (matched by path in the explorer render).
    let dirty_file: Option<std::path::PathBuf> = dirty
        .then(|| app.selected.as_ref().map(|s| s.file.clone()))
        .flatten();
    if let Some(explorer_area) = explorer_area {
        explorer::render(
            frame,
            explorer_area,
            &mut app.explorer,
            explorer_focused,
            has_ws,
            &theme,
            app.jump.as_ref(),
            dirty_file.as_deref(),
        );
    }
    let selected_request = app
        .selected
        .as_ref()
        .map(|selected| &selected.endpoint.request);
    urlbar::render(
        frame,
        urlbar_area,
        urlbar::UrlBarCtx {
            request: selected_request,
            focused: app.focus == Pane::UrlBar && app.mode == Mode::Normal,
            editor: app.url_editor.as_mut(),
            dirty,
            jump_label: app
                .jump
                .as_ref()
                .and_then(|j| j.label_for_pane(Pane::UrlBar)),
        },
        &theme,
    );
    match app.zoom {
        Some(ZoomPane::Request) => {
            // Request is zoomed: render it full-size, show collapsed summary for response.
            request::render(
                frame,
                request_area,
                request::RenderCtx {
                    request: selected_request,
                    editor: &mut app.editor,
                    tabs: &mut app.tabs,
                    focused: app.focus == Pane::Request && app.mode == Mode::Normal,
                    theme: &theme,
                    jump: app.jump.as_ref(),
                },
            );
            let summary = response::collapsed_summary(&app.response, &theme);
            render_collapsed_stub(
                frame,
                response_area,
                "Response",
                app.jump
                    .as_ref()
                    .and_then(|j| j.label_for_pane(Pane::Response)),
                summary,
                &theme,
            );
        }
        Some(ZoomPane::Response) => {
            // Response is zoomed: render it full-size, show collapsed summary for request.
            let summary = request::collapsed_summary(selected_request, &app.tabs, &theme);
            render_collapsed_stub(
                frame,
                request_area,
                "Request",
                app.jump
                    .as_ref()
                    .and_then(|j| j.label_for_pane(Pane::Request)),
                summary,
                &theme,
            );
            let outcome = response::render(
                frame,
                response_area,
                response::RenderCtx {
                    state: &app.response,
                    request: selected_request,
                    focused: app.focus == Pane::Response
                        && (app.mode == Mode::Normal || app.mode == Mode::BodySearch),
                    scroll: app.response_scroll,
                    cursor: app.response_cursor,
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
            app.response_cursor = outcome.clamped_cursor;
            app.response_total_rows = outcome.total_rows;
            app.response_viewport_height = outcome.viewport_height;
            app.response_viewport_width = outcome.viewport_width;
            if let Some(job) = outcome.job {
                let dup = app.pending_highlight == Some(job.hash);
                if !dup && let Some(tx) = &app.highlight_tx {
                    // Mark in-flight only when the send actually succeeded — a
                    // dead worker must not wedge the guard.
                    let hash = job.hash;
                    if tx.send(job).is_ok() {
                        app.pending_highlight = Some(hash);
                    }
                }
            }
        }
        None => {
            // Normal split: render both.
            request::render(
                frame,
                request_area,
                request::RenderCtx {
                    request: selected_request,
                    editor: &mut app.editor,
                    tabs: &mut app.tabs,
                    focused: app.focus == Pane::Request && app.mode == Mode::Normal,
                    theme: &theme,
                    jump: app.jump.as_ref(),
                },
            );
            let outcome = response::render(
                frame,
                response_area,
                response::RenderCtx {
                    state: &app.response,
                    request: selected_request,
                    focused: app.focus == Pane::Response
                        && (app.mode == Mode::Normal || app.mode == Mode::BodySearch),
                    scroll: app.response_scroll,
                    cursor: app.response_cursor,
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
            app.response_cursor = outcome.clamped_cursor;
            app.response_total_rows = outcome.total_rows;
            app.response_viewport_height = outcome.viewport_height;
            app.response_viewport_width = outcome.viewport_width;
            if let Some(job) = outcome.job {
                let dup = app.pending_highlight == Some(job.hash);
                if !dup && let Some(tx) = &app.highlight_tx {
                    // Mark in-flight only when the send actually succeeded — a
                    // dead worker must not wedge the guard.
                    let hash = job.hash;
                    if tx.send(job).is_ok() {
                        app.pending_highlight = Some(hash);
                    }
                }
            }
        }
    }
    // The statusline (deliverable 9) keeps *only* persistent state: focus,
    // endpoint/workspace, profile, dirty, and the in-flight spinner. Transient
    // messages live in the dedicated row below. The in-flight spinner still
    // derives from state so it appears/disappears instantly.
    let in_flight = app.in_flight.is_some();
    statusline::render(
        frame,
        status,
        statusline::StatusCtx {
            focus: app.focus.name(),
            workspace: app.workspace.as_ref().map(|ws| ws.manifest().name.as_str()),
            profile: app.active_profile.as_deref(),
            dirty,
            in_flight,
            tick_count: app.tick_count,
            theme: &theme,
        },
    );
    // The dedicated message row, only when a message is live.
    if let (Some(area), Some(text)) = (msg_area, &message_text) {
        message::render(frame, area, text, &theme);
    }

    if let Some(picker_state) = &app.picker {
        picker::render(frame, main, picker_state, &theme);
    }

    // CRUD / method overlays render over the whole main area.
    match app.mode {
        Mode::MethodMenu => {
            if let Some(request) = selected_request {
                method_menu::render(frame, main, request.method, &theme);
            }
        }
        Mode::Prompt(purpose) => {
            let hint = prompt_hint(app, purpose);
            prompt::render_prompt(
                frame,
                main,
                purpose.title(),
                &app.prompt_editor,
                hint.as_deref(),
                &theme,
            );
        }
        Mode::Confirm(purpose) => {
            let (title, question, hint) = confirm_text(purpose);
            prompt::render_confirm(frame, main, title, question, hint, &theme);
        }
        Mode::EnvEditor => {
            if let Some(editor) = &app.env_editor {
                env_editor::render(frame, main, editor, &theme);
            }
        }
        Mode::SequenceRunner => {
            let tick = app.tick_count;
            let job = {
                let cache = &app.highlight_cache;
                match app.sequence_runner.as_mut() {
                    Some(runner) => {
                        sequence_runner::render(frame, main, runner, tick, cache, &theme)
                    }
                    None => None,
                }
            };
            if let Some(job) = job {
                let dup = app.pending_highlight == Some(job.hash);
                if !dup && let Some(tx) = &app.highlight_tx {
                    let hash = job.hash;
                    if tx.send(job).is_ok() {
                        app.pending_highlight = Some(hash);
                    }
                }
            }
        }
        Mode::SequenceEditor => {
            if let Some(editor) = &app.sequence_editor {
                sequence_editor::render(frame, main, editor, &theme);
            }
        }
        _ => {}
    }

    // The URL vim-popup editor (deliverable 7) renders over the main area.
    if let Some(editor) = app.url_popup.as_mut() {
        urlbar::render_popup(frame, main, editor, &theme);
    }

    // The which-key leader popup (deliverable 1).
    if app.pending_leader {
        let entries: Vec<(String, String)> = {
            let mut actions: Vec<Action> =
                app.keymap.iter_leader().map(|(_, action)| action).collect();
            actions.sort_by_key(|a| a.name());
            actions.dedup();
            actions
                .into_iter()
                .map(|a| {
                    (
                        app.keymap.leader_combos_for(a).join("/"),
                        a.label().to_owned(),
                    )
                })
                .collect()
        };
        leader_popup::render(frame, main, &entries, &theme);
    }

    // The `?` help overlay (deliverable 8), rendered from the live keymap.
    if app.help_open {
        let outcome = help::render(frame, main, &app.keymap, app.help_scroll, &theme);
        app.help_scroll = app.help_scroll.min(outcome.total.saturating_sub(1));
        app.help_viewport_height = outcome.viewport_height;
    }
}

/// The which-key leader popup: a small floating panel listing the bound
/// continuations while in pending-leader state.
mod leader_popup {
    use ratatui::Frame;
    use ratatui::layout::{Constraint, Flex, Layout, Rect};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

    use crate::tui::theme::Theme;

    /// Renders the popup with `(keys, label)` continuation entries.
    pub fn render(frame: &mut Frame, area: Rect, entries: &[(String, String)], theme: &Theme) {
        let width = entries
            .iter()
            .map(|(k, l)| k.len() + l.len() + 5)
            .max()
            .unwrap_or(20)
            .clamp(20, 50) as u16;
        let height = entries.len() as u16 + 2;
        let [modal] = Layout::horizontal([Constraint::Length(width)])
            .flex(Flex::End)
            .areas(area);
        let [modal] = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::End)
            .areas(modal);
        frame.render_widget(Clear, modal);
        let block = Block::bordered()
            .border_type(BorderType::Thick)
            .border_style(theme.border_focused)
            .title(" leader ")
            .title_style(theme.title);
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        let lines: Vec<Line> = entries
            .iter()
            .map(|(keys, label)| {
                Line::from(vec![
                    Span::styled(format!(" {keys} "), theme.jump_label),
                    Span::raw(format!(" {label}")),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Renders a collapsed zoom stub: the pane's unfocused border + title (with its
/// jump label when jump-mode is active) around the one-line tab-bar/stats
/// summary — the pane keeps its chrome when collapsed, it doesn't vanish into a
/// bare text row.
fn render_collapsed_stub(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    name: &str,
    jump_label: Option<char>,
    summary: Line<'static>,
    theme: &crate::tui::theme::Theme,
) {
    let title = match jump_label {
        Some(label) => format!(" {name} [{label}] "),
        None => format!(" {name} "),
    };
    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(theme.border_unfocused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(summary), inner);
}

/// The dim hint line under a prompt (the collection name to type for a
/// typed-confirm delete; none otherwise).
fn prompt_hint(app: &App, purpose: PromptPurpose) -> Option<String> {
    match purpose {
        PromptPurpose::DeleteCollectionConfirm => app
            .explorer
            .selected_name()
            .map(|name| format!("type \"{name}\" to confirm")),
        PromptPurpose::ImportCollection => Some("path to a Postman v2.1 JSON file".to_owned()),
        PromptPurpose::ExportCollection(_) | PromptPurpose::ExportWorkspace(_) => {
            Some("destination path (must stay inside the workspace)".to_owned())
        }
        PromptPurpose::PasteCurl => Some("paste a curl command".to_owned()),
        _ => None,
    }
}

/// A sensible default export destination inside the workspace: `exports/<slug>.json`.
fn default_export_path(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "export" } else { slug };
    format!("exports/{slug}.json")
}

/// Resolves a user-typed export path against the workspace `root`, refusing any
/// path that escapes the root (`..` traversal or an absolute path outside it).
/// The `root` is canonicalized (it exists); the target is normalized lexically
/// (it may not exist yet, so `Path::canonicalize` cannot be used on it).
fn export_target(root: &Path, input: &str) -> Result<PathBuf, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("no path given".to_owned());
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let joined = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        root.join(input)
    };
    let normalized = lexical_normalize(&joined);
    if !normalized.starts_with(&root) {
        return Err("path escapes the workspace root".to_owned());
    }
    // The lexical check above can be fooled by a symlinked component *inside* the
    // root that points elsewhere (the subsequent write follows symlinks).
    // Canonicalize the deepest existing ancestor of the target and re-check it
    // against the root, so an `exports -> /etc` symlink can't tunnel out.
    if let Some(real) = existing_ancestor_canonical(&normalized)
        && !real.starts_with(&root)
    {
        return Err("path escapes the workspace root (symlinked component)".to_owned());
    }
    Ok(normalized)
}

/// Canonicalizes the deepest ancestor of `path` that actually exists on disk
/// (the target itself usually does not exist yet). Returns `None` if nothing up
/// the chain resolves.
fn existing_ancestor_canonical(path: &Path) -> Option<PathBuf> {
    let mut probe = path;
    loop {
        if let Ok(real) = probe.canonicalize() {
            return Some(real);
        }
        probe = probe.parent()?;
    }
}

/// Resolves `.`/`..` components without touching the filesystem. A leading `..`
/// that would climb above the path root simply pops nothing further (so an
/// escaping path fails the later `starts_with` check).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The (title, question, key-hint) for a confirmation overlay.
fn confirm_text(purpose: ConfirmPurpose) -> (&'static str, &'static str, &'static str) {
    match purpose {
        ConfirmPurpose::DeleteEndpoint => ("Delete endpoint", "Delete this endpoint?", "[y/n]"),
        ConfirmPurpose::DiscardChanges => (
            "Unsaved changes",
            "Save changes before switching?",
            "s save · d discard · esc stay",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use churl_core::model::{OnError, Timing};

    #[test]
    fn export_target_stays_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A relative path resolves under the (canonicalized) root.
        let target = export_target(root, "exports/api.json").unwrap();
        let canon_root = root.canonicalize().unwrap();
        assert!(target.starts_with(&canon_root), "{target:?}");
        assert!(target.ends_with("exports/api.json"));
        // A nested `..` that stays inside is fine.
        let ok = export_target(root, "exports/../api.json").unwrap();
        assert_eq!(ok, canon_root.join("api.json"));
    }

    #[test]
    fn export_target_rejects_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(export_target(root, "../escape.json").is_err());
        assert!(export_target(root, "exports/../../escape.json").is_err());
        assert!(export_target(root, "/etc/passwd").is_err());
        assert!(export_target(root, "   ").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn export_target_rejects_symlinked_component() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let outside = tempfile::tempdir().unwrap();
        // A symlinked dir inside the root that points elsewhere: lexically the
        // target is "under" the root, but the write would follow the link out.
        std::os::unix::fs::symlink(outside.path(), root.join("exports")).unwrap();
        assert!(
            export_target(root, "exports/leak.json").is_err(),
            "a symlinked component must not tunnel out of the workspace"
        );
    }

    #[test]
    fn default_export_path_slugifies() {
        assert_eq!(default_export_path("My API"), "exports/my-api.json");
        assert_eq!(default_export_path("  "), "exports/export.json");
    }

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

    /// Puts a fresh app in insert mode on the request pane's Body tab (where the
    /// edtui editor lives).
    fn insert_mode_app(keymap: KeyMap) -> App {
        let mut app = App::new(None, keymap).unwrap();
        app.focus = Pane::Request;
        app.tabs.active = RequestTab::Body;
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
            app.message.as_ref().map(|m| m.text.as_str()),
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
        assert!(app.message.is_none());
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
            app.message.as_ref().map(|m| m.text.as_str()),
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

    // ---- M7.4 sequence runner state machine (injected outcomes, no server) ----

    /// Builds a workspace with three GET endpoints and a `sequences/flow.toml`
    /// running them in order, then opens the runner (client `None`, so steps stay
    /// in the `Running` stub until an outcome is injected).
    fn sequence_app(root: &Path, on_error: &str, extract: &str) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        for name in ["one", "two", "three"] {
            std::fs::write(
                coll.join(format!("{name}.toml")),
                format!(
                    "seq = 0\nname = \"{name}\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{name}\"\n"
                ),
            )
            .unwrap();
        }
        let seq_dir = root.join("sequences");
        std::fs::create_dir(&seq_dir).unwrap();
        std::fs::write(
            seq_dir.join("flow.toml"),
            format!(
                "seq = 0\nname = \"Flow\"\non_error = \"{on_error}\"\n\n[[step]]\nseq = 0\nendpoint = \"api/one.toml\"\n{extract}\n[[step]]\nseq = 1\nendpoint = \"api/two.toml\"\n\n[[step]]\nseq = 2\nendpoint = \"api/three.toml\"\n"
            ),
        )
        .unwrap();
        let ws = open_workspace(root).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        let file = seq_dir.join("flow.toml");
        let sequence = churl_core::persistence::load_sequence(&file).unwrap();
        app.open_sequence_runner(super::super::components::explorer::SelectedSequence {
            name: sequence.name.clone(),
            file,
            sequence,
        });
        app
    }

    fn ok_resp(status: u16, body: &str) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
            truncated: false,
            timing: Timing {
                connect: None,
                total: std::time::Duration::from_millis(5),
            },
        }
    }

    fn runner_gen(app: &App) -> u64 {
        app.sequence_runner.as_ref().unwrap().run_generation
    }

    fn step_status(app: &App, i: usize) -> StepStatus {
        app.sequence_runner.as_ref().unwrap().steps[i]
            .status
            .clone()
    }

    #[test]
    fn sequence_runner_opens_with_first_step_running() {
        let dir = tempfile::tempdir().unwrap();
        let app = sequence_app(dir.path(), "halt", "");
        assert_eq!(app.mode, Mode::SequenceRunner);
        let runner = app.sequence_runner.as_ref().unwrap();
        assert_eq!(runner.steps.len(), 3);
        assert_eq!(runner.current, Some(0));
        assert_eq!(step_status(&app, 0), StepStatus::Running);
        assert_eq!(step_status(&app, 1), StepStatus::Pending);
    }

    #[test]
    fn sequence_run_halts_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_app(dir.path(), "halt", "");
        let run_gen = runner_gen(&app);
        app.on_sequence_step(run_gen, 0, Ok(ok_resp(200, "{}")));
        assert_eq!(step_status(&app, 0), StepStatus::Ok(200));
        assert_eq!(step_status(&app, 1), StepStatus::Running);
        // Step 1 fails with 500 → halt → step 2 skipped, run finished.
        app.on_sequence_step(run_gen, 1, Ok(ok_resp(500, "err")));
        assert_eq!(step_status(&app, 1), StepStatus::Failed(500));
        assert_eq!(step_status(&app, 2), StepStatus::Skipped);
        assert!(app.sequence_runner.as_ref().unwrap().finished);
    }

    #[test]
    fn sequence_run_continue_runs_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_app(dir.path(), "continue", "");
        let run_gen = runner_gen(&app);
        app.on_sequence_step(run_gen, 0, Ok(ok_resp(500, "err")));
        assert_eq!(step_status(&app, 0), StepStatus::Failed(500));
        // Continue: step 1 runs despite the earlier failure.
        assert_eq!(step_status(&app, 1), StepStatus::Running);
        app.on_sequence_step(run_gen, 1, Ok(ok_resp(200, "{}")));
        assert_eq!(step_status(&app, 2), StepStatus::Running);
        app.on_sequence_step(run_gen, 2, Ok(ok_resp(200, "{}")));
        assert!(app.sequence_runner.as_ref().unwrap().finished);
    }

    #[test]
    fn sequence_run_transport_error_halts() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_app(dir.path(), "halt", "");
        let run_gen = runner_gen(&app);
        app.on_sequence_step(run_gen, 0, Err("connection refused".to_owned()));
        assert!(matches!(step_status(&app, 0), StepStatus::HttpError(_)));
        assert_eq!(step_status(&app, 1), StepStatus::Skipped);
        assert_eq!(step_status(&app, 2), StepStatus::Skipped);
    }

    #[test]
    fn sequence_run_accumulates_and_chains_extracted() {
        let dir = tempfile::tempdir().unwrap();
        // Step one extracts token = $.token.
        let mut app = sequence_app(dir.path(), "halt", "[step.extract]\ntoken = \"$.token\"");
        let run_gen = runner_gen(&app);
        app.on_sequence_step(run_gen, 0, Ok(ok_resp(200, r#"{"token":"XYZ"}"#)));
        let runner = app.sequence_runner.as_ref().unwrap();
        assert_eq!(
            runner.extracted.get("token").map(String::as_str),
            Some("XYZ")
        );
        assert_eq!(runner.steps[0].extracted.get("token").unwrap(), "XYZ");
        assert_eq!(step_status(&app, 1), StepStatus::Running);
    }

    #[test]
    fn sequence_extract_error_halts() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_app(dir.path(), "halt", "[step.extract]\nx = \"$.missing\"");
        let run_gen = runner_gen(&app);
        app.on_sequence_step(run_gen, 0, Ok(ok_resp(200, "{}")));
        assert!(matches!(step_status(&app, 0), StepStatus::ExtractError(_)));
        assert_eq!(step_status(&app, 1), StepStatus::Skipped);
    }

    #[test]
    fn sequence_stale_result_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_app(dir.path(), "halt", "");
        let run_gen = runner_gen(&app);
        // A result from a superseded generation must not advance the run.
        app.on_sequence_step(run_gen + 99, 0, Ok(ok_resp(200, "{}")));
        assert_eq!(step_status(&app, 0), StepStatus::Running);
    }

    #[test]
    fn sequence_cancel_marks_pending_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_app(dir.path(), "halt", "");
        app.cancel_sequence_run();
        let runner = app.sequence_runner.as_ref().unwrap();
        assert!(runner.finished);
        assert!(
            runner.steps.iter().all(|s| s.status == StepStatus::Skipped),
            "all non-terminal steps must be skipped on cancel"
        );
    }

    /// Cross-check: the same scripted scenarios produce identical per-step
    /// results + extracted maps through the core `run_sequence` path (real HTTP
    /// via wiremock) AND the live TUI driver (`on_sequence_step`) — the guard
    /// against the two drifting (the milestone's stated highest risk).
    #[tokio::test]
    async fn sequence_transition_matches_core() {
        use churl_core::http::{DEFAULT_TIMEOUT, ExecuteOptions, build_client};
        use churl_core::sequence::{RunScopes, StepResult, run_sequence};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // (label, on_error, [statuses], step0 extract expr or "")
        let scenarios: [(&str, OnError, [u16; 3], &str); 4] = [
            ("all-ok+extract", OnError::Halt, [200, 200, 200], "$.token"),
            ("halt-on-500", OnError::Halt, [200, 500, 200], ""),
            ("continue-past-500", OnError::Continue, [200, 500, 200], ""),
            (
                "extract-error-halts",
                OnError::Halt,
                [200, 200, 200],
                "$.missing",
            ),
        ];

        // Maps both result types to a comparable shape (kind tag, status).
        fn norm_core(r: &StepResult) -> (u8, Option<u16>) {
            match r {
                StepResult::Ok { status } => (0, Some(*status)),
                StepResult::Failed { status } => (1, Some(*status)),
                StepResult::HttpError(_) => (2, None),
                StepResult::ExtractError(_) => (3, None),
                StepResult::Skipped => (4, None),
            }
        }
        fn norm_tui(s: &StepStatus) -> (u8, Option<u16>) {
            match s {
                StepStatus::Ok(status) => (0, Some(*status)),
                StepStatus::Failed(status) => (1, Some(*status)),
                StepStatus::HttpError(_) => (2, None),
                StepStatus::ExtractError(_) => (3, None),
                StepStatus::Skipped => (4, None),
                StepStatus::Pending | StepStatus::Running => (9, None),
            }
        }

        for (label, on_error, statuses, extract) in scenarios {
            let server = MockServer::start().await;
            let bodies = [r#"{"token":"T"}"#, "{}", "{}"];
            for (i, status) in statuses.iter().enumerate() {
                Mock::given(method("GET"))
                    .and(path(format!("/s{i}")))
                    .respond_with(ResponseTemplate::new(*status).set_body_string(bodies[i]))
                    .mount(&server)
                    .await;
            }

            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
            let coll = root.join("api");
            std::fs::create_dir(&coll).unwrap();
            for i in 0..3 {
                std::fs::write(
                    coll.join(format!("s{i}.toml")),
                    format!(
                        "seq = 0\nname = \"s{i}\"\n\n[request]\nmethod = \"GET\"\nurl = \"{}/s{i}\"\n",
                        server.uri()
                    ),
                )
                .unwrap();
            }
            let extract_block = if extract.is_empty() {
                String::new()
            } else {
                format!("[step.extract]\ntoken = \"{extract}\"\n")
            };
            let seq_dir = root.join("sequences");
            std::fs::create_dir(&seq_dir).unwrap();
            std::fs::write(
                seq_dir.join("flow.toml"),
                format!(
                    "seq = 0\nname = \"Flow\"\non_error = \"{}\"\n\n[[step]]\nseq = 0\nendpoint = \"api/s0.toml\"\n{extract_block}\n[[step]]\nseq = 1\nendpoint = \"api/s1.toml\"\n\n[[step]]\nseq = 2\nendpoint = \"api/s2.toml\"\n",
                    match on_error { OnError::Halt => "halt", OnError::Continue => "continue" }
                ),
            )
            .unwrap();

            // --- core path (real HTTP) ---
            let client = build_client(DEFAULT_TIMEOUT).unwrap();
            let file = seq_dir.join("flow.toml");
            let sequence = churl_core::persistence::load_sequence(&file).unwrap();
            let core_run = run_sequence(
                &client,
                root,
                &sequence,
                &RunScopes::default(),
                &ExecuteOptions::default(),
            )
            .await;
            let core_results: Vec<_> = core_run
                .steps
                .iter()
                .map(|s| norm_core(&s.result))
                .collect();
            let core_extracted = core_run.steps[0].extracted.clone();

            // --- TUI path (injected outcomes matching the mock) ---
            let ws = open_workspace(root).unwrap();
            let mut app = App::new(ws, KeyMap::default()).unwrap();
            let seq2 = churl_core::persistence::load_sequence(&file).unwrap();
            app.open_sequence_runner(super::super::components::explorer::SelectedSequence {
                name: seq2.name.clone(),
                file: file.clone(),
                sequence: seq2,
            });
            // Feed each step that actually runs the same response the mock returned.
            while let Some(i) = app.sequence_runner.as_ref().unwrap().current {
                let g = app.sequence_runner.as_ref().unwrap().run_generation;
                app.on_sequence_step(g, i, Ok(ok_resp(statuses[i], bodies[i])));
            }
            let runner = app.sequence_runner.as_ref().unwrap();
            let tui_results: Vec<_> = runner.steps.iter().map(|s| norm_tui(&s.status)).collect();
            let tui_extracted = runner.steps[0].extracted.clone();

            assert_eq!(
                core_results, tui_results,
                "scenario {label}: core vs TUI step results diverged"
            );
            assert_eq!(
                core_extracted, tui_extracted,
                "scenario {label}: core vs TUI extracted maps diverged"
            );
        }
    }

    #[test]
    fn sequence_rerun_resets_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_app(dir.path(), "halt", "");
        let run_gen = runner_gen(&app);
        app.on_sequence_step(run_gen, 0, Ok(ok_resp(500, "err")));
        assert!(app.sequence_runner.as_ref().unwrap().finished);
        app.start_sequence_run();
        let runner = app.sequence_runner.as_ref().unwrap();
        assert!(!runner.finished);
        assert_eq!(runner.run_generation, run_gen + 1);
        assert_eq!(runner.steps[0].status, StepStatus::Running);
        assert!(
            runner.steps[1..]
                .iter()
                .all(|s| s.status == StepStatus::Pending)
        );
    }

    /// End-to-end: create a sequence via the prompt, add a step through the
    /// picker, toggle on_error, save, and assert the file on disk.
    #[test]
    fn sequence_editor_create_add_step_save() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("login.toml"),
            "seq = 0\nname = \"login\"\n\n[request]\nmethod = \"POST\"\nurl = \"https://api.test/login\"\n",
        )
        .unwrap();
        let ws = open_workspace(root).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();

        // `<leader>a` with no sequence selected → new-sequence name prompt.
        app.edit_selected_sequence().unwrap();
        assert_eq!(app.mode, Mode::Prompt(PromptPurpose::NewSequence));
        app.prompt_editor = LineEditor::new("Auth flow");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::SequenceEditor);

        // Add a step: `a` opens the picker, Enter accepts the first endpoint.
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        // Toggle on_error to continue, then save with `w`.
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE))
            .unwrap();

        let path = root.join("sequences").join("auth-flow.toml");
        let sequence = churl_core::persistence::load_sequence(&path).unwrap();
        assert_eq!(sequence.name, "Auth flow");
        assert_eq!(sequence.on_error, OnError::Continue);
        assert_eq!(sequence.steps.len(), 1);
        assert_eq!(sequence.steps[0].endpoint, "api/login.toml");
    }

    /// Two extraction rules with the same name on one step refuse the whole save:
    /// the message names the duplicate, nothing is written, the editor stays open.
    #[test]
    fn sequence_editor_duplicate_rule_names_refuse_save() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("one.toml"),
            "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
        )
        .unwrap();
        // A sequence with one step, no rules yet.
        let seq_dir = root.join("sequences");
        std::fs::create_dir(&seq_dir).unwrap();
        let seq_path = seq_dir.join("flow.toml");
        std::fs::write(
            &seq_path,
            "seq = 0\nname = \"Flow\"\non_error = \"halt\"\n\n[[step]]\nseq = 0\nendpoint = \"api/one.toml\"\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&seq_path).unwrap();

        let ws = open_workspace(root).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        let sequence = churl_core::persistence::load_sequence(&seq_path).unwrap();
        app.open_sequence_editor("Flow".to_owned(), seq_path.clone(), &sequence);

        let ch = |app: &mut App, c: char| {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        };
        let enter = |app: &mut App| {
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap();
        };
        ch(&mut app, 'l'); // focus rules
        // rule "x" = $.a
        ch(&mut app, 'a');
        ch(&mut app, 'x');
        enter(&mut app);
        for c in "$.a".chars() {
            ch(&mut app, c);
        }
        enter(&mut app);
        // second rule also "x"
        ch(&mut app, 'a');
        ch(&mut app, 'x');
        enter(&mut app);
        for c in "$.b".chars() {
            ch(&mut app, c);
        }
        enter(&mut app);
        // Save → refused.
        ch(&mut app, 'w');

        assert_eq!(
            app.mode,
            Mode::SequenceEditor,
            "editor stays open on refusal"
        );
        assert!(
            app.message
                .as_ref()
                .is_some_and(|m| m.text.contains("duplicate rule name 'x'")),
            "expected a duplicate-name refusal message, got {:?}",
            app.message.as_ref().map(|m| &m.text)
        );
        // Nothing was written — the file is byte-for-byte unchanged.
        assert_eq!(std::fs::read_to_string(&seq_path).unwrap(), before);
    }

    /// Pressing `f` enters jump-mode; a pane label focuses that pane and exits.
    #[test]
    fn jump_label_focuses_pane() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Jump);
        assert!(app.jump.is_some());
        // 's' is the Response pane mnemonic (e/u/r/s for
        // Explorer/UrlBar/Request/Response).
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
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
        // Rows use the row alphabet (panes hold e/u/r/s) → row 0 is 'a', row 1 is 'd'.
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(app.explorer.cursor, 1);
        // The endpoint row was selected and loaded.
        assert!(app.selected.is_some());
        assert_eq!(app.selected.as_ref().unwrap().endpoint.name, "Get user");
    }

    /// The default Jump key `f` is also a row label (3rd row): when assigned,
    /// the label wins over the "Jump key again cancels" rule so that row stays
    /// reachable by pressing `f` twice.
    #[test]
    fn jump_f_acts_as_row_label_not_cancel() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zoo")).unwrap();
        let mut app = workspace_fixture(dir.path());
        // Expand "users" → 3 visible rows (users, Get user, zoo) = labels a/d/f.
        app.explorer.expand().unwrap();
        assert_eq!(app.explorer.rows().len(), 3);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Jump);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal, "f must act as a label, not cancel");
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(app.explorer.cursor, 2);
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

    /// Regression (M6.6 review #1): on the Body tab in edtui Normal mode, the
    /// row-editing keys (`i` insert, `a` append) must reach edtui instead of
    /// being eaten by the Request overlay's RowEdit/RowAdd — there are no rows
    /// on the Body tab.
    #[test]
    fn body_tab_row_keys_reach_edtui() {
        for c in ['i', 'a'] {
            let mut app = App::new(None, KeyMap::default()).unwrap();
            app.focus = Pane::Request;
            app.tabs.active = RequestTab::Body;
            assert_eq!(app.editor.mode, EditorMode::Normal);
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(
                app.editor.mode,
                EditorMode::Insert,
                "'{c}' on the Body tab must enter edtui insert mode"
            );
            assert!(
                app.tabs.editing.is_none(),
                "no row edit must start for '{c}'"
            );
        }
    }

    /// Regression (M6.6 review #4): after an explorer reload shifts the
    /// name-sorted collection indices (a new collection sorting first), the
    /// loaded endpoint's collection index is remapped from its file path, so
    /// the send-time resolver still reads the *right* collection's
    /// `folder.toml` vars.
    #[test]
    fn reload_remaps_selected_collection_index_for_resolver() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        // One collection "bbb" with folder.toml vars + an endpoint.
        let bbb = dir.path().join("bbb");
        std::fs::create_dir(&bbb).unwrap();
        std::fs::write(bbb.join("folder.toml"), "[vars]\nwho = \"bbb-vars\"\n").unwrap();
        std::fs::write(
            bbb.join("get.toml"),
            "seq = 0\nname = \"Get\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://{{who}}/x\"\n",
        )
        .unwrap();
        let ws = open_workspace(dir.path()).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        app.explorer.expand().unwrap();
        app.explorer.cursor = 1;
        let selected = app.explorer.select().unwrap().expect("endpoint");
        app.load_endpoint(selected);
        assert_eq!(app.selected.as_ref().unwrap().collection, 0);

        // Create a collection that sorts *before* "bbb" and reload: "bbb" is
        // now index 1; the stale index 0 would read "aaa"'s (empty) vars.
        churl_core::persistence::create_collection(dir.path(), "aaa").unwrap();
        app.reload_explorer().unwrap();
        assert_eq!(
            app.selected.as_ref().unwrap().collection,
            1,
            "collection index must be remapped from the file path"
        );
        let selected = app.selected.clone().unwrap();
        let resolver = app.build_resolver(&selected);
        assert_eq!(
            resolver.substitute("{{who}}"),
            "bbb-vars",
            "resolver must read the loaded endpoint's own collection vars"
        );
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

    /// A newer message replaces the current one in the dedicated message row.
    #[test]
    fn newer_message_replaces_current() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.notify("first");
        assert_eq!(app.message.as_ref().map(|m| m.text.as_str()), Some("first"));
        app.notify("second");
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("second"),
            "newer message must replace the current one"
        );
    }

    // ---- M6.7: leader state machine ----

    fn press(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .unwrap();
    }

    /// Space enters pending-leader; a bound continuation dispatches and dismisses.
    #[test]
    fn leader_pending_then_dispatch() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        press(&mut app, ' ');
        assert!(app.pending_leader, "space enters pending-leader");
        // `<leader>q` quits.
        press(&mut app, 'q');
        assert!(!app.pending_leader, "dispatch dismisses the popup");
        assert!(app.should_quit);
    }

    /// An unbound continuation dismisses the popup with no action; Esc too.
    #[test]
    fn leader_unbound_and_esc_dismiss() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        press(&mut app, ' ');
        press(&mut app, 'x'); // unbound
        assert!(!app.pending_leader);
        assert!(!app.should_quit);

        press(&mut app, ' ');
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(!app.pending_leader, "esc dismisses");
    }

    /// `<leader>e` toggles the explorer sidebar.
    #[test]
    fn leader_e_toggles_explorer() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        assert!(!app.explorer_hidden);
        press(&mut app, ' ');
        press(&mut app, 'e');
        assert!(app.explorer_hidden);
        assert!(!app.pending_leader);
    }

    /// Leader is inert during text edits: Space types a space in the URL editor,
    /// never entering pending-leader.
    #[test]
    fn leader_inert_during_url_edit() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.explorer.expand().unwrap();
        app.explorer.cursor = 1;
        let selected = app.explorer.select().unwrap().expect("endpoint");
        app.load_endpoint(selected);
        app.begin_url_edit_inline();
        assert!(app.url_editor.is_some());
        press(&mut app, ' ');
        assert!(
            !app.pending_leader,
            "space must not enter leader during edit"
        );
        assert!(
            app.url_editor.as_ref().unwrap().text().contains(' '),
            "space types a space in the editor"
        );
    }

    /// Leader is inert in edtui insert mode on the Body tab.
    #[test]
    fn leader_inert_in_edtui_insert() {
        let mut app = insert_mode_app(KeyMap::default());
        press(&mut app, ' ');
        assert!(!app.pending_leader);
        assert_eq!(String::from(app.editor.lines.clone()), " ");
    }

    // ---- M6.7: digit binds only act in Request ----

    /// Global digits do nothing (no pane focus); inside Request they jump tabs.
    #[test]
    fn digit_focus_removed_at_app_level() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.focus = Pane::Response;
        press(&mut app, '1'); // was FocusExplorer
        assert_eq!(app.focus, Pane::Response, "digit must not change focus");
        // In the Request pane, 1–4 jump tabs.
        app.focus = Pane::Request;
        press(&mut app, '2');
        assert_eq!(app.tabs.active, RequestTab::Headers);
    }

    // ---- M6.7: URL→Params merge policy ----

    fn params(pairs: &[(&str, &str, bool)]) -> Vec<Param> {
        pairs
            .iter()
            .map(|(n, v, e)| Param {
                name: (*n).to_owned(),
                value: (*v).to_owned(),
                enabled: *e,
            })
            .collect()
    }

    #[test]
    fn merge_rule_a_exact_match_ensures_enabled() {
        // Exact name+value exists but disabled → enabled, no duplicate.
        let mut p = params(&[("a", "1", false)]);
        let report = merge_query_params(&mut p, &[("a".into(), "1".into())]);
        assert_eq!(p.len(), 1);
        assert!(p[0].enabled);
        assert_eq!(report.as_deref(), Some("a updated"));
        // Already enabled + exact → no change, no report.
        let mut p = params(&[("a", "1", true)]);
        assert!(merge_query_params(&mut p, &[("a".into(), "1".into())]).is_none());
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn merge_rule_b_updates_first_row_of_name() {
        let mut p = params(&[("a", "old", false)]);
        let report = merge_query_params(&mut p, &[("a".into(), "new".into())]);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].value, "new");
        assert!(p[0].enabled);
        assert_eq!(report.as_deref(), Some("a updated"));
    }

    #[test]
    fn merge_rule_c_appends_absent() {
        let mut p = params(&[("a", "1", true)]);
        let report = merge_query_params(&mut p, &[("b".into(), "2".into())]);
        assert_eq!(p.len(), 2);
        assert_eq!(p[1].name, "b");
        assert!(p[1].enabled);
        assert_eq!(report.as_deref(), Some("b added"));
    }

    #[test]
    fn merge_rule_d_duplicate_names_positional() {
        // Two existing `tag` rows; `?tag=x&tag=y` maps positionally.
        let mut p = params(&[("tag", "a", true), ("tag", "b", true)]);
        merge_query_params(
            &mut p,
            &[("tag".into(), "x".into()), ("tag".into(), "y".into())],
        );
        assert_eq!(p.len(), 2, "no new rows — positional map");
        assert_eq!(p[0].value, "x");
        assert_eq!(p[1].value, "y");
        // A third duplicate appends (extra).
        let mut p = params(&[("tag", "a", true)]);
        merge_query_params(
            &mut p,
            &[("tag".into(), "x".into()), ("tag".into(), "y".into())],
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].value, "x");
        assert_eq!(p[1].value, "y");
    }

    #[test]
    fn split_query_trims_whitespace() {
        // Names and values with leading/trailing whitespace (e.g. " name = value ")
        // are trimmed before decode so the resulting params are clean.
        let (_, pairs) = split_query("https://x/y? a = 1 & b = hello world ");
        assert_eq!(
            pairs,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "hello world".to_owned()),
            ]
        );
        // A key-only segment with surrounding spaces is also trimmed.
        let (_, pairs) = split_query("https://x/y? flag ");
        assert_eq!(pairs, vec![("flag".to_owned(), String::new())]);
    }

    #[test]
    fn split_query_strips_and_decodes() {
        let (base, pairs) = split_query("https://x/y?a=1&b=hello%20world&c");
        assert_eq!(base, "https://x/y");
        assert_eq!(
            pairs,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "hello world".to_owned()),
                ("c".to_owned(), String::new()),
            ]
        );
        // No query → empty pairs, url unchanged.
        let (base, pairs) = split_query("https://x/y");
        assert_eq!(base, "https://x/y");
        assert!(pairs.is_empty());
    }

    /// Committing a URL edit strips the query, merges into params, reports the
    /// merge, and marks the request dirty.
    #[test]
    fn commit_url_merges_and_marks_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.explorer.expand().unwrap();
        app.explorer.cursor = 1;
        let selected = app.explorer.select().unwrap().expect("endpoint");
        app.load_endpoint(selected);
        assert!(!app.is_dirty());
        app.commit_url("https://api.test/users?page=2&sort=name".to_owned());
        let req = app.live_request().unwrap();
        assert_eq!(req.url, "https://api.test/users", "query stripped from URL");
        assert_eq!(req.params.len(), 2);
        assert!(req.params.iter().all(|p| p.enabled));
        assert!(
            app.message
                .as_ref()
                .is_some_and(|m| m.text.contains("added")),
            "statusline reports the merge"
        );
        assert!(app.is_dirty(), "commit marks the request dirty");
    }

    // ---- M6.7: zoom state machine ----

    #[test]
    fn zoom_toggles_and_restores() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.focus = Pane::Request;
        app.dispatch(Action::Zoom, None).unwrap();
        assert_eq!(app.zoom, Some(ZoomPane::Request));
        app.dispatch(Action::Zoom, None).unwrap();
        assert_eq!(app.zoom, None, "z again restores the split");
        // From Response.
        app.focus = Pane::Response;
        app.dispatch(Action::Zoom, None).unwrap();
        assert_eq!(app.zoom, Some(ZoomPane::Response));
        // No-op from the Explorer/UrlBar.
        app.zoom = None;
        app.focus = Pane::Explorer;
        app.dispatch(Action::Zoom, None).unwrap();
        assert_eq!(app.zoom, None);
    }

    /// Focusing the collapsed pane auto-unzooms first (a collapsed pane cannot
    /// hold focus).
    #[test]
    fn focus_collapsed_pane_auto_unzooms() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.focus = Pane::Request;
        app.dispatch(Action::Zoom, None).unwrap(); // Response collapsed
        assert_eq!(app.zoom, Some(ZoomPane::Request));
        app.dispatch(Action::FocusResponse, None).unwrap();
        assert_eq!(app.zoom, None, "focusing the collapsed pane unzooms");
        assert_eq!(app.focus, Pane::Response);
    }

    /// Jumping (jump-mode) into the collapsed pane auto-unzooms too — jump
    /// dispatch must go through `set_focus`, not assign focus directly
    /// (review round 3, finding #4).
    #[test]
    fn jump_into_collapsed_pane_auto_unzooms() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.focus = Pane::Response;
        app.dispatch(Action::Zoom, None).unwrap(); // Request collapsed
        assert_eq!(app.zoom, Some(ZoomPane::Response));
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Jump);
        // 'r' is the Request pane mnemonic.
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.focus, Pane::Request);
        assert_eq!(
            app.zoom, None,
            "jumping into the collapsed pane must unzoom"
        );
    }

    // ---- M6.7: explorer toggle + auto-reopen ----

    #[test]
    fn explorer_toggle_and_auto_reopen() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.focus = Pane::Request;
        app.dispatch(Action::ToggleExplorer, None).unwrap();
        assert!(app.explorer_hidden);
        // Focusing the explorer auto-reopens it.
        app.dispatch(Action::FocusExplorer, None).unwrap();
        assert!(!app.explorer_hidden, "focus explorer must auto-reopen");
        assert_eq!(app.focus, Pane::Explorer);
        // Hiding while focused moves focus off the explorer.
        app.dispatch(Action::ToggleExplorer, None).unwrap();
        assert!(app.explorer_hidden);
        assert_ne!(app.focus, Pane::Explorer, "hidden pane cannot hold focus");
    }

    /// Tab cycling onto the hidden explorer auto-reopens it.
    #[test]
    fn tab_onto_hidden_explorer_reopens() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.explorer_hidden = true;
        app.focus = Pane::Response; // next() → Explorer
        app.dispatch(Action::FocusNext, None).unwrap();
        assert_eq!(app.focus, Pane::Explorer);
        assert!(!app.explorer_hidden, "tab onto explorer reopens it");
    }

    // ---- M6.7: URL popup editor ----

    fn app_with_endpoint(dir: &Path) -> App {
        let mut app = workspace_fixture(dir);
        app.explorer.expand().unwrap();
        app.explorer.cursor = 1;
        let selected = app.explorer.select().unwrap().expect("endpoint");
        app.load_endpoint(selected);
        app
    }

    #[test]
    fn url_popup_commit_runs_merge() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir.path());
        app.begin_url_popup();
        assert!(app.url_popup.is_some());
        // Replace the buffer and commit.
        app.url_popup = Some(EditorState::new(Lines::from("https://api.test/x?q=1")));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.url_popup.is_none(), "enter commits + closes");
        let req = app.live_request().unwrap();
        assert_eq!(req.url, "https://api.test/x");
        assert!(req.params.iter().any(|p| p.name == "q" && p.enabled));
    }

    #[test]
    fn url_popup_esc_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir.path());
        let before = app.live_request().unwrap().url.clone();
        app.begin_url_popup();
        // In Normal mode, Esc cancels (edtui popups open in Normal mode).
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.url_popup.is_none());
        assert_eq!(app.live_request().unwrap().url, before, "url unchanged");
    }

    #[test]
    fn url_popup_single_line_constraint() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir.path());
        app.begin_url_popup();
        // A buffer that somehow holds two lines collapses to one on commit.
        app.url_popup = Some(EditorState::new(Lines::from("https://a/b\nc")));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.live_request().unwrap().url, "https://a/bc");
    }

    /// `/`-search in the popup executes on Enter (jump to match → Normal), and
    /// the popup stays open; a second Enter commits. Regression: `handle_url_popup_key`
    /// used to commit on any Enter, so Search could never run.
    #[test]
    fn url_popup_search_executes_then_commits() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir.path());
        app.begin_url_popup();
        app.url_popup = Some(EditorState::new(Lines::from("https://api.test/find")));
        // `/` enters Search; type "find"; Enter runs FindFirst → Normal.
        for c in "/find".chars() {
            press(&mut app, c);
        }
        assert_eq!(app.url_popup.as_ref().unwrap().mode, EditorMode::Search);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let popup = app.url_popup.as_ref().expect("popup still open");
        assert_eq!(popup.mode, EditorMode::Normal, "search left Search mode");
        assert_eq!(popup.cursor.col, 17, "cursor jumped to the 'find' match");
        // A second Enter (now in Normal) commits.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.url_popup.is_none(), "second enter commits");
    }

    /// The vim motion extensions move the popup cursor in Normal mode.
    #[test]
    fn url_popup_vim_motions_move_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir.path());
        app.begin_url_popup();
        app.url_popup = Some(EditorState::new(Lines::from("foo bar baz")));
        let cursor = |app: &App| app.url_popup.as_ref().unwrap().cursor.col;

        press(&mut app, 'W'); // → start of "bar"
        assert_eq!(cursor(&app), 4);
        press(&mut app, 'B'); // → back to "foo"
        assert_eq!(cursor(&app), 0);
        press(&mut app, 'f'); // find-char forward…
        press(&mut app, 'z');
        assert_eq!(cursor(&app), 10); // the 'z' in "baz"
        press(&mut app, 'F'); // find-char backward…
        press(&mut app, 'o');
        assert_eq!(cursor(&app), 2); // last 'o' before the cursor
        press(&mut app, '^'); // first non-blank of the row
        assert_eq!(cursor(&app), 0);
    }

    /// Esc while an f/F/t/T find is pending aborts the find (vim) — it must not
    /// close the popup. A second Esc (no pending) cancels as usual.
    #[test]
    fn url_popup_esc_aborts_pending_find_not_popup() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir.path());
        app.begin_url_popup();
        app.url_popup = Some(EditorState::new(Lines::from("foo bar")));
        press(&mut app, 'f'); // pending find…
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(
            app.url_popup.is_some(),
            "esc aborted the find, not the popup"
        );
        // The find is gone: a char is typed-through to edtui, not a target.
        press(&mut app, 'b');
        assert_eq!(
            app.url_popup.as_ref().unwrap().cursor.col,
            0,
            "aborted find must not resolve on the next char"
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.url_popup.is_none(), "esc with no pending cancels");
    }

    /// Body tab in Normal mode: `W` moves the editor cursor, and `f`+char is
    /// find-char inside the editor — it does NOT open jump-mode.
    #[test]
    fn body_tab_vim_motions_and_f_shadows_jump() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.focus = Pane::Request;
        app.tabs.active = RequestTab::Body;
        app.editor = EditorState::new(Lines::from("foo bar baz"));
        app.editor.mode = EditorMode::Normal;

        press(&mut app, 'W');
        assert_eq!(app.editor.cursor.col, 4, "W moved the body cursor");

        press(&mut app, 'f');
        press(&mut app, 'z');
        assert_eq!(app.editor.cursor.col, 10, "f<c> moved the body cursor");
        assert!(app.jump.is_none(), "f shadowed jump inside the Body editor");
        assert_eq!(app.editor.mode, EditorMode::Normal);
    }

    /// The `f` shadow is Body-scoped: from the Explorer pane `f` still enters
    /// jump-mode (guard is per-pane/tab).
    #[test]
    fn f_from_explorer_still_enters_jump() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.focus = Pane::Explorer;
        press(&mut app, 'f');
        assert!(app.jump.is_some(), "jump-mode still reachable outside Body");
    }

    #[test]
    fn url_edit_mode_selects_inline_vs_popup() {
        let dir = tempfile::tempdir().unwrap();
        // Default (inline): begin_url_edit opens the inline editor.
        let mut app = app_with_endpoint(dir.path());
        app.begin_url_edit();
        assert!(app.url_editor.is_some());
        assert!(app.url_popup.is_none());
        // Popup mode: begin_url_edit opens the popup.
        let dir2 = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir2.path());
        app.set_url_edit_mode(UrlEditMode::Popup);
        app.begin_url_edit();
        assert!(app.url_popup.is_some());
        assert!(app.url_editor.is_none());
    }

    // ---- M6.7: help overlay ----

    #[test]
    fn help_opens_and_closes() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.dispatch(Action::Help, None).unwrap();
        assert!(app.help_open);
        // `j`/`k` scroll; `?` closes.
        press(&mut app, 'j');
        assert_eq!(app.help_scroll, 1);
        press(&mut app, 'k');
        assert_eq!(app.help_scroll, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))
            .unwrap();
        assert!(!app.help_open);
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

    // ---- M7.2 quick-jump pickers ----

    /// A second workspace fixture with a distinctly-named collection, so a
    /// workspace switch is observable in the explorer tree.
    fn other_workspace_fixture(root: &Path) {
        std::fs::write(root.join("churl.toml"), "name = \"other\"\n").unwrap();
        let coll = root.join("orders");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("create.toml"),
            "seq = 0\nname = \"Create order\"\n\n[request]\nmethod = \"POST\"\nurl = \"https://x/orders\"\n",
        )
        .unwrap();
    }

    /// `<leader>f` reuses the endpoint-search overlay as the request picker.
    #[test]
    fn leader_f_opens_request_picker() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .unwrap();
        assert!(app.pending_leader, "space enters pending-leader state");
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Search, "<leader>f opens the search overlay");
        let picker = app.picker.as_ref().expect("picker open");
        assert!(
            picker.items.iter().any(|i| i.contains("Get user")),
            "request picker lists the workspace endpoints: {:?}",
            picker.items
        );
    }

    /// `<leader>w` with no history store shows a message, not an empty picker.
    #[test]
    fn workspace_picker_empty_without_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.dispatch(Action::QuickJumpWorkspaces, None).unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.picker.is_none());
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("no recent workspaces")
        );
    }

    /// `<leader>w` opens the workspace picker over the recency list (newest
    /// first), storing canonical paths index-aligned with the display items.
    #[test]
    fn workspace_picker_lists_recent() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        let store = HistoryStore::in_memory().unwrap();
        store.touch_workspace("/ws/alpha", 1_000).unwrap();
        store.touch_workspace("/ws/beta", 2_000).unwrap();
        app.history = Some(store);

        app.dispatch(Action::QuickJumpWorkspaces, None).unwrap();
        assert_eq!(app.mode, Mode::WorkspacePicker);
        let picker = app.picker.as_ref().expect("picker open");
        assert_eq!(picker.items, vec!["/ws/beta", "/ws/alpha"]);
        assert_eq!(
            app.workspace_choices,
            vec![PathBuf::from("/ws/beta"), PathBuf::from("/ws/alpha")]
        );
    }

    /// Switching workspaces rebuilds the explorer against the new tree and resets
    /// every endpoint/workspace-scoped field (a stale one would be a correctness
    /// bug). Also records the switch in the recency table.
    #[test]
    fn switch_workspace_resets_state_and_loads_new_tree() {
        let dir_a = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir_a.path());
        app.history = Some(HistoryStore::in_memory().unwrap());

        // Load an endpoint from A and make the app "busy": dirty editor, response
        // pane focus, an active profile, a non-Idle response.
        app.explorer.expand().unwrap();
        app.guarded_load(PendingLoad::Row(1)).unwrap();
        assert!(app.selected.is_some());
        app.editor = EditorState::new(Lines::from("dirty body"));
        app.active_profile = Some("dev".to_owned());
        app.focus = Pane::Response;
        app.response = ResponseState::Cancelled;

        let dir_b = tempfile::tempdir().unwrap();
        other_workspace_fixture(dir_b.path());
        let path_b = dir_b.path().to_path_buf();

        app.switch_workspace(path_b.clone()).unwrap();

        // The workspace + explorer now reflect B.
        assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "other");
        let names: Vec<String> = app.explorer.rows().iter().map(|r| r.name.clone()).collect();
        assert!(names.iter().any(|n| n == "orders"), "shows B: {names:?}");
        assert!(!names.iter().any(|n| n == "users"), "no A rows: {names:?}");

        // Endpoint/workspace-scoped state is reset.
        assert!(app.selected.is_none());
        assert!(app.loaded_snapshot.is_none());
        assert_eq!(String::from(app.editor.lines.clone()), "");
        assert!(app.active_profile.is_none());
        assert_eq!(app.explorer.cursor, 0);
        assert_eq!(app.focus, Pane::Explorer);
        assert!(matches!(app.response, ResponseState::Idle));
        assert!(!app.is_dirty(), "no dirty state after switch");
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("switched to other")
        );

        // Recency recorded the switch (canonical path of B).
        let recent = app.history.as_ref().unwrap().recent_workspaces(10).unwrap();
        let canon = canonical_path(&path_b).to_string_lossy().into_owned();
        assert!(recent.contains(&canon), "recency has B: {recent:?}");
    }

    /// A dirty switch defers to the discard-changes confirm (workspace targets
    /// are always "other"); discarding then performs the switch.
    #[test]
    fn dirty_workspace_switch_defers_to_confirm() {
        let dir_a = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir_a.path());
        app.history = Some(HistoryStore::in_memory().unwrap());
        app.explorer.expand().unwrap();
        app.guarded_load(PendingLoad::Row(1)).unwrap();
        app.editor = EditorState::new(Lines::from("unsaved edit"));
        assert!(app.is_dirty());

        let dir_b = tempfile::tempdir().unwrap();
        other_workspace_fixture(dir_b.path());

        app.guarded_load(PendingLoad::Workspace(dir_b.path().to_path_buf()))
            .unwrap();
        // Deferred: confirm overlay open, switch not yet performed.
        assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
        assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "demo");

        // Discard: the switch goes through.
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "other");
        assert!(app.selected.is_none());
    }
}
