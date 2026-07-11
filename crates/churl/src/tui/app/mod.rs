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
use churl_core::history::{HistoryStore, LoadBatchSummary, NewHistoryEntry, default_state_path};
use churl_core::http::ExecuteOptions;
use churl_core::interchange::{self, JsonDialect};
use churl_core::load::{LoadCheck, LoadConfig, ReqOutcome};
use churl_core::model::{
    ApiKeyPlacement, Auth, Body, BodyKind, Endpoint, Header, Param, Request, Response,
};
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
use super::components::load_runner::{LoadOutcome, LoadRunnerState, LoadStatus};
use super::components::message::Message;
use super::components::request_tabs::{EditField, FieldEdit, RequestTab, RequestTabs};
use super::components::response::{
    ResponseGeometry, ResponseMeta, ResponseState, ResponseView, ViewMode,
};
use churl_core::sequence::StepResult;

use super::components::sequence_editor::{EditorOutcome, SequenceEditorState};
use super::components::sequence_runner::{RunnerOutcome, SequenceRunnerState, StepStatus};
use super::components::vim_ext::{self, VimExt};
use super::components::{
    env_editor, explorer, help, load_runner, message, method_menu, palette, picker, prompt,
    request, response, sequence_editor, sequence_runner, statusline, tab_strip, urlbar,
};
use super::events::{Action, FuzzyFinder, KeyMap, LeaderEntry, PaneCtx};
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

/// Which sub-pane inside the left (explorer) column has focus/zoom (PR 2b).
/// Modelled on a separate axis from [`Pane`]: `Pane::Explorer` means "left column
/// focused", `LeftPane` decides which sub-pane. Keeps `Pane` at four variants (no
/// Tab-cycle/zoom-pairing churn) and lets the sequences sub-pane toggle off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftPane {
    /// The endpoints tree (default; the only occupant when the sub-pane is off).
    Endpoints,
    /// The sequences sub-pane at the bottom of the column.
    Sequences,
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
    /// The "open sequence" picker overlay is open (`<leader>s o`): a fuzzy list
    /// of sequence names; accepting one opens the unified sequence surface.
    SequencePicker,
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
    /// The unified sequence surface (M7.4): ONE modal with an edit⇄run switcher
    /// (`Ctrl-R`). The active face is [`App::sequence_view`]; the Edit face drives
    /// the step/extraction editor, the Run face drives + shows the live run.
    Sequence,
    /// The concurrent-load runner (M7.5): a large modal firing N copies of the
    /// selected endpoint with live results + latency stats.
    LoadRunner,
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
    /// Delete the selected sequence in the Sequences sub-pane (note #6).
    DeleteSequence,
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

/// A deferred buffer-close awaiting the discard-changes confirm. Buffers are
/// keyed by **path**, not index — closing/removing a buffer shifts the indices
/// of everything after it, so an index parked across a confirm would go stale.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingClose {
    /// A single dirty buffer close (`<leader>t x`).
    One(std::path::PathBuf),
    /// A close-all (`<leader>t X`) working through its dirty buffers one prompt
    /// at a time (front = the one currently prompting).
    All(std::collections::VecDeque<std::path::PathBuf>),
}

/// Capacity of the bounded app channel (R1 D4a). Generous headroom for the
/// bursty senders (a load run fires `total` `LoadResult`s, the highlight worker a
/// steady trickle of `Highlighted`s) while still bounding memory: an
/// arbitrarily long / high-`total` session can no longer flood the queue with
/// unbounded owned `Response`/`Highlighted` bodies. All senders live in spawned
/// async tasks (`.send().await` applies natural backpressure) or the dedicated
/// highlight OS thread (`blocking_send`) — never on the render/UI thread — so a
/// full queue slows a producer without ever stalling input.
const APP_CHANNEL_CAPACITY: usize = 1024;

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
        generation: u64,
        outcome: Result<Response, String>,
        /// Metadata captured at send time.
        meta: ResponseMeta,
    },
    /// Highlighted lines for a viewport, returned by the highlight worker.
    Highlighted {
        /// Viewport hash these lines belong to (the cache key).
        hash: u64,
        lines: Vec<Line<'static>>,
    },
    /// A sequence step completed (M7.4). `run_generation` is matched against the
    /// runner's generation so results from a cancelled/superseded run are dropped.
    SequenceStep {
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
    },
    /// One copy of a concurrent-load batch actually started executing (M7.5) —
    /// sent by the launcher when the copy enters the concurrency window, so its
    /// row shows the in-flight glyph honestly. `run_generation`-guarded.
    LoadStarted { run_generation: u64, index: usize },
    /// One copy of a concurrent-load batch completed (M7.5). `run_generation`
    /// guards against results from a cancelled/superseded batch.
    LoadResult {
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
    },
}

/// The active level of the two-level which-key leader popup. `None` (the field's
/// resting value) means no leader chord is in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderState {
    /// The root which-key popup: direct binds + submenu prefixes.
    Root,
    /// A nested submenu is open, keyed by its name (`<leader>s …` = `"sequences"`).
    Submenu(String),
}

/// The active face of the unified sequence surface ([`Mode::Sequence`]). `Ctrl-R`
/// flips between them; the two component states persist across a flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqView {
    /// The step/extraction editor face.
    Edit,
    /// The live-run face.
    Run,
}

/// Which pane is zoomed (the other collapses to a stub). See deliverable 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomPane {
    /// Request pane zoomed; Response collapses to its stats line.
    Request,
    /// Response pane zoomed; Request collapses to its tab bar.
    Response,
}

/// Which surface the shared `response_*` handlers operate on (note #2). The main
/// endpoint buffer by default; a runner's selected row/step when that runner's
/// Response region is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseSurface {
    /// The active endpoint buffer (Normal mode; today's behaviour).
    Main,
    /// The load runner's selected results row.
    LoadRunner,
    /// The sequence runner's selected step.
    Sequence,
}

/// Bookkeeping for the single in-flight request.
struct InFlightRequest {
    handle: AbortHandle,
    generation: u64,
    /// Metadata, reused when writing the history row on cancel.
    meta: ResponseMeta,
}

/// The per-endpoint edit/response/dirty/tab/editor state that used to live as
/// flat `App` fields. Everything here is scoped to a single loaded endpoint; the
/// forward-compat split (a later `Sequence` buffer kind) keeps this the payload
/// of [`BufferKind::Endpoint`] while the dedup key (file path), tab label, and
/// dirty stay reachable at the [`Buffer`] level.
struct EndpointBuffer {
    /// The endpoint currently loaded (with any in-memory edits).
    endpoint: SelectedEndpoint,
    /// The pristine endpoint as loaded, cloned at load time. Dirty state is
    /// *derived* by comparing it against the live request (incl. the edtui body).
    /// Non-`Option`: a buffer always has a baseline.
    loaded_snapshot: Endpoint,
    /// edtui state for the request body.
    editor: EditorState,
    editor_events: EditorEventHandler,
    /// Normal-mode motion extensions (W/B/^/f/F/t/T) for the Body editor.
    editor_vim: VimExt,
    /// Request-pane tab state (active tab + per-tab selection + field edit).
    tabs: RequestTabs,
    /// The inline URL-bar editor while editing the URL; `None` otherwise.
    url_editor: Option<LineEditor>,
    /// The edtui popup URL editor (`e` on the URL bar / `url_edit = "popup"`),
    /// present while the popup is open.
    url_popup: Option<EditorState>,
    url_popup_events: EditorEventHandler,
    /// Normal-mode motion extensions for the URL popup editor.
    url_popup_vim: VimExt,
    response: ResponseState,
    /// Cursor/scroll/viewport geometry for this buffer's response viewer. Shared
    /// shape with the runner states so the `response_*` handlers are mode-aware
    /// (note #2).
    geometry: ResponseGeometry,
    /// The highlight hash last enqueued but not yet returned.
    pending_highlight: Option<u64>,
    /// Viewport-hash → highlighted-lines cache (capped, cleared on new response).
    highlight_cache: HashMap<u64, Vec<Line<'static>>>,
    in_flight: Option<InFlightRequest>,
}

impl EndpointBuffer {
    /// Folds today's `load_endpoint` body-preparation into a fresh buffer: the
    /// edtui body from the request, a reset vim state, and a pristine snapshot
    /// clone for dirty derivation.
    fn new(selected: SelectedEndpoint) -> Self {
        let body = selected
            .endpoint
            .request
            .body
            .as_ref()
            .map(|body| body.content.as_str())
            .unwrap_or("");
        let editor = EditorState::new(Lines::from(body));
        let mut editor_vim = VimExt::default();
        editor_vim.reset();
        Self {
            loaded_snapshot: selected.endpoint.clone(),
            endpoint: selected,
            editor,
            editor_events: EditorEventHandler::default(),
            editor_vim,
            tabs: RequestTabs::default(),
            url_editor: None,
            url_popup: None,
            url_popup_events: EditorEventHandler::default(),
            url_popup_vim: VimExt::default(),
            response: ResponseState::Idle,
            geometry: ResponseGeometry::default(),
            pending_highlight: None,
            highlight_cache: HashMap::new(),
            in_flight: None,
        }
    }

    /// The live request currently loaded (with any in-memory edits).
    fn live_request(&self) -> &Request {
        &self.endpoint.endpoint.request
    }

    /// Whether the live endpoint (incl. the edtui body) differs from the pristine
    /// snapshot. Derived — no dirty flag to keep in sync.
    fn is_dirty(&self) -> bool {
        // Fold the live body in without mutating self, then compare.
        let mut live = self.endpoint.endpoint.clone();
        let text = String::from(self.editor.lines.clone());
        fold_body_text(&mut live.request, text);
        live != self.loaded_snapshot
    }
}

/// The kind-specific payload of a [`Buffer`]. A single variant this stage; a
/// later wave adds `Sequence(SequenceBuffer)` additively — the dedup key, tab
/// label, and dirty already live on [`Buffer`] so nothing here needs to move.
enum BufferKind {
    Endpoint(EndpointBuffer),
}

/// A loaded tab/buffer. Stage 1 keeps `buffers.len() <= 1`; Stage 2 turns
/// [`App::open_or_focus_buffer`] into true dedup-or-push and adds the tab strip.
/// The dedup key (`file`), tab label (`display_path`), and dirty are reachable
/// here without matching `kind`.
struct Buffer {
    kind: BufferKind,
}

impl Buffer {
    /// Builds an endpoint buffer (the only kind this stage).
    fn endpoint(selected: SelectedEndpoint) -> Self {
        Self {
            kind: BufferKind::Endpoint(EndpointBuffer::new(selected)),
        }
    }

    /// The dedup key / tab identity: the buffer's endpoint file path. Reachable
    /// without matching `kind` (a future `Sequence` buffer returns its own path).
    fn file(&self) -> &Path {
        match &self.kind {
            BufferKind::Endpoint(b) => &b.endpoint.file,
        }
    }

    /// The endpoint payload, if this is an endpoint buffer.
    fn as_endpoint(&self) -> Option<&EndpointBuffer> {
        match &self.kind {
            BufferKind::Endpoint(b) => Some(b),
        }
    }

    /// The endpoint payload (mutable), if this is an endpoint buffer.
    fn as_endpoint_mut(&mut self) -> Option<&mut EndpointBuffer> {
        match &mut self.kind {
            BufferKind::Endpoint(b) => Some(b),
        }
    }

    /// Whether the buffer has unsaved changes (per-kind).
    fn is_dirty(&self) -> bool {
        match &self.kind {
            BufferKind::Endpoint(b) => b.is_dirty(),
        }
    }
}

/// Folds edtui body text into a request's `body` for **dirty derivation /
/// save**: an empty body drops the `Body` entirely; otherwise it seeds or
/// updates a text body. (The send/curl path uses [`overwrite_body_text`], which
/// keeps an existing empty body instead of dropping it — preserving the two
/// pre-refactor behaviours exactly.)
fn fold_body_text(request: &mut Request, text: String) {
    match request.body.as_mut() {
        Some(body) => {
            if text.is_empty() {
                request.body = None;
            } else {
                body.content = text;
            }
        }
        None if !text.is_empty() => {
            request.body = Some(Body {
                kind: BodyKind::Text,
                content: text,
            });
        }
        None => {}
    }
}

/// Folds edtui body text into a request's `body` for a **send / curl copy**: an
/// existing body is overwritten in place (even with empty text); a missing body
/// is seeded only when the text is non-empty. Matches the pre-refactor
/// `send_request`/`copy_as_curl` inline behaviour.
fn overwrite_body_text(request: &mut Request, text: String) {
    match request.body.as_mut() {
        Some(body) => body.content = text,
        None if !text.is_empty() => {
            request.body = Some(Body {
                kind: BodyKind::Text,
                content: text,
            });
        }
        None => {}
    }
}

/// A clipboard copy queued by a key handler and flushed by the run loop (see
/// [`App::pending_clipboard`]). Carries the capped payload plus the message to
/// show **iff** a clipboard path succeeds — on total failure the loop reports
/// "copy failed" instead, so the UI never claims a copy that did not happen.
struct PendingCopy {
    payload: String,
    success_msg: String,
}

/// The whole TUI state. Constructible without a tokio runtime so snapshot
/// tests can drive [`render`] through a `TestBackend`.
pub struct App {
    /// The workspace opened from the cwd, if its `churl.toml` exists.
    pub workspace: Option<OpenWorkspace>,
    pub explorer: ExplorerState,
    /// The open buffers (loaded endpoints). Stage 1 holds at most one
    /// (`buffers.len() <= 1`); Stage 2 adds true dedup-or-push + a tab strip.
    /// All per-endpoint edit/response/dirty/tab/editor state lives inside the
    /// active buffer — read via [`App::active_endpoint_buffer`]/`_mut`.
    buffers: Vec<Buffer>,
    /// Index of the active buffer in `buffers` (`0` while single-buffer).
    active: usize,
    /// Focused pane in [`Mode::Normal`].
    pub focus: Pane,
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
    /// Sequence files behind an open [`Mode::SequencePicker`] (index-aligned with
    /// the picker items). Non-empty only while that picker is open.
    sequence_choices: Vec<PathBuf>,
    /// One-shot flag: the endpoint search overlay was opened to pick a target for
    /// the load runner (`<leader>l f`). Set on open, consumed + cleared when the
    /// pick lands (the endpoint loads, then the load runner opens over it).
    load_runner_after_pick: bool,
    /// One-shot flag mirroring `load_runner_after_pick`: the sequence picker was
    /// opened to RUN the chosen sequence (`<leader>s r`) rather than edit it
    /// (`<leader>s o`). Set on open, consumed + cleared on accept / close.
    sequence_pick_runs: bool,
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
    pub tx: mpsc::Sender<AppMsg>,
    rx: mpsc::Receiver<AppMsg>,
    /// The shared reqwest client; `None` in snapshot-test construction (runtime-free).
    pub client: Option<Client>,
    /// Per-execution knobs (body-size cap) resolved from config in
    /// [`App::install_runtime`]; defaults under snapshot-test construction.
    execute_options: ExecuteOptions,
    /// Monotonic request counter; a landed response with a stale generation is
    /// dropped. Global (not per-buffer) so generations are unique across buffers,
    /// which is how [`App::on_response`] routes a landed response to its buffer.
    generation: u64,
    /// Orphan response driving the response-pane isolation snapshots (no loaded
    /// endpoint). UNREACHABLE in production — real responses live in the active
    /// buffer — so this stays `Idle`; the render's no-buffer branch falls back to
    /// it. Cannot be `#[cfg(test)]`-gated: that flag is off for `tests/`.
    orphan_response: ResponseState,
    /// Geometry for [`Self::orphan_response`] (isolation snapshots, no loaded
    /// endpoint). In production the orphan is `Idle`, so this stays at 0.
    orphan_geometry: ResponseGeometry,
    /// A clipboard copy queued by a key handler, flushed by the run loop after
    /// the key is handled (native copy needs no terminal, but the OSC 52
    /// fallback writes to the terminal backend, which dispatch has no handle
    /// to). The success message is deferred until the copy actually runs so it
    /// reflects reality (Constitution: fail loudly, never fake success).
    pending_clipboard: Option<PendingCopy>,
    /// The incremental body-search input editor while `Mode::BodySearch` is open.
    body_search_editor: LineEditor,
    /// The mode to restore when the body-search input closes. `Mode::Normal` for
    /// the main pane; `Mode::LoadRunner`/`Mode::Sequence` when search was opened
    /// over a runner Response region (note #2), so closing search returns to the
    /// runner rather than dropping the user into Normal mode.
    body_search_return: Mode,
    /// Job sender for the off-thread highlight worker; `None` under `TestBackend`.
    highlight_tx: Option<JobSender<HighlightJob>>,
    /// History store; `None` when disabled (open failed or no data dir).
    history: Option<HistoryStore>,
    /// Transient action/status message shown in the dedicated row above the
    /// statusline (send hints, history/errors, merges, CRUD results); auto-expires
    /// after `Message::EXPIRE_SECS`.
    message: Option<Message>,
    /// Load-time keymap conflict/shadow warnings (M7.10), surfaced as a
    /// first-frame statusline toast. Empty for a clean config. Populated from
    /// [`KeyMap::validate`] by the TUI entry point; a `false` `keymap_warned`
    /// flag ensures the toast fires exactly once.
    keymap_warnings: Vec<String>,
    /// Whether the first-frame keymap-warning toast has already fired.
    keymap_warned: bool,
    /// Monotonic tick counter (incremented every 250 ms tick); drives the spinner
    /// animation in the response pane. `pub` so snapshot tests can set it.
    pub tick_count: u64,
    /// The text prompt's line editor while a [`Mode::Prompt`] overlay is open.
    pub prompt_editor: LineEditor,
    /// True while the open picker is the auth-kind picker (vs search/palette/profile).
    auth_picker: bool,
    /// The endpoint-switch target deferred behind an open
    /// [`ConfirmPurpose::DiscardChanges`] overlay; resolved by `s`/`d`, dropped
    /// by `Esc`.
    pending_load: Option<PendingLoad>,
    /// The buffer-close intent deferred behind an open
    /// [`ConfirmPurpose::DiscardChanges`] overlay (a dirty tab close). Keyed by
    /// path, resolved by `s`/`d`, aborted by `Esc`. Mutually exclusive with
    /// `pending_load` — only one is `Some` while the confirm is up.
    pending_close: Option<PendingClose>,
    /// Which pane is zoomed (M6.7 deliverable 4), or `None` for the normal split.
    zoom: Option<ZoomPane>,
    /// Whether the explorer sidebar is hidden (M6.7 deliverable 5). Session-only.
    explorer_hidden: bool,
    /// Which sub-pane inside the left column has focus/zoom (PR 2b). The
    /// sequences sub-pane is always present (peek-symmetric, M7.10 stage B), so
    /// this is only forced back to `Endpoints` when the workspace has no
    /// sequences at all.
    left_active: LeftPane,
    /// The pane focus held before focus last moved INTO the left column from a
    /// non-left pane (PR 2b / owner #2B). `<leader>e` hiding a focused explorer
    /// restores this, instead of falling back to the URL bar.
    focus_before_explorer: Option<Pane>,
    /// The active leader chord, or `None` when no chord is in progress. `Some`
    /// drives the two-level which-key popup (root ⇄ submenu).
    leader: Option<LeaderState>,
    /// Whether the `?` help overlay is open.
    help_open: bool,
    /// Scroll offset of the help overlay.
    help_scroll: usize,
    /// Last rendered inner height of the help overlay, for half-page scrolling.
    help_viewport_height: usize,
    /// Live `/` search inside the `?` help overlay (highlight-and-jump), when the
    /// help-search input is open or committed. `None` = no help search. Mirrors
    /// the response body search (shared smart-case matcher, `n`/`N`, Esc/Enter).
    help_search: Option<help::HelpSearch>,
    /// The incremental input editor for the help-overlay `/` search.
    help_search_editor: LineEditor,
    /// Whether the help-search input is still open (typing). Enter closes the
    /// input but keeps `help_search` (matches stay highlighted for `n`/`N`).
    help_search_input: bool,
    /// What `i`/`Enter` on the URL bar opens (inline vs popup); `e` always popup.
    url_edit_mode: UrlEditMode,
    /// The open environments & variables editor (M7.3), when `Mode::EnvEditor`.
    env_editor: Option<EnvEditorState>,
    /// The unified sequence surface's run-face state (M7.4), when
    /// `Mode::Sequence`. Built lazily on the first run (Edit-only sessions never
    /// allocate it); persists across face flips.
    sequence_runner: Option<SequenceRunnerState>,
    /// Abort handle for the in-flight sequence step, so a cancel/re-run aborts it.
    sequence_abort: Option<AbortHandle>,
    /// The unified sequence surface's edit-face state (M7.4 §4), when
    /// `Mode::Sequence`. Built when the surface opens; persists across face flips.
    sequence_editor: Option<SequenceEditorState>,
    /// The active face of the unified sequence surface (Edit vs Run).
    sequence_view: SeqView,
    /// The open concurrent-load runner (M7.5), when `Mode::LoadRunner`.
    load_runner: Option<LoadRunnerState>,
    /// Abort handle for the single load-batch launcher task; aborting it drops
    /// the launcher's `buffer_unordered`, cancelling ALL in-flight requests.
    load_abort: Option<AbortHandle>,
    /// Concurrent-load guardrail caps (from `[load]` config, or defaults).
    load_caps: churl_core::load::LoadCaps,
    /// The load runner's request, resolved ONCE at open time and cloned for every
    /// copy in a run (consistent batch — no per-copy re-resolution).
    load_request: Option<Request>,
    /// In-memory Session variable store (note #6), keyed by canonical workspace
    /// root so one workspace's captured secrets never leak into another. Populated
    /// when a sequence run extracts a value for a rule listed in that step's
    /// `persist`; read as the highest resolver scope for BOTH sequence-run and
    /// standalone-send resolution. **Never** written to disk (a security feature —
    /// captured tokens stay in RAM and evaporate on exit); there is no load/save.
    session_vars: HashMap<PathBuf, BTreeMap<String, String>>,
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

/// A minimal [`ResponseMeta`] for a failed load-copy's response view (load runs
/// write only the batch summary, so only the display URL matters).
fn load_result_meta(url: &str) -> ResponseMeta {
    ResponseMeta {
        method: String::new(),
        url: url.to_owned(),
        endpoint_path: None,
        executed_at_ms: now_ms(),
    }
}

/// The user-facing "fail loud" message naming unresolved `{{var}}` placeholders,
/// shared by the two TUI send sites (main-pane send + load-runner open) so their
/// wording matches the sequence-step error ([`churl_core::sequence::SequenceError::Unresolved`]).
/// `names` is expected non-empty and already sorted/deduped (as
/// [`churl_core::template::unresolved_placeholders`] returns).
fn unresolved_vars_message(names: &[String]) -> String {
    format!(
        "unresolved variable(s): {} — set them in a profile/env or via CLI",
        names.join(", ")
    )
}

/// The status message for a `y`/`Y` copy attempt on a row with no copyable
/// content (drive-test #4a fold-in). A `Dropped` (memory-evicted) load row is
/// called out specifically — its body was intentionally not retained — while
/// every other bodyless state (`Idle`/`InFlight`/`Cancelled`) gets a plain
/// "nothing to copy". Never a silent no-op; never fabricates content.
fn nothing_to_copy_message(state: &ResponseState) -> &'static str {
    match state {
        ResponseState::Dropped { .. } => "nothing to copy — response body not retained",
        _ => "nothing to copy",
    }
}

/// The on-disk stem a create/rename landed on, or `None` when it matched the
/// naive slug of the typed name (no disambiguation happened). Used to fail loud
/// when a reserved-name collision (R1 D1) bumped the filename — the user must see
/// the real stem, never a silent rename.
fn disambiguated_stem(typed: &str, path: &Path) -> Option<String> {
    let stem = path.file_stem().and_then(|s| s.to_str())?;
    if stem == persistence::slug_of(typed) {
        None
    } else {
        Some(stem.to_owned())
    }
}

/// The confirmation message for a create, noting the actual on-disk name when it
/// was disambiguated (e.g. an endpoint named `churl` written as `churl-2`).
fn created_message(typed: &str, path: &Path) -> String {
    match disambiguated_stem(typed, path) {
        Some(stem) => format!("created {typed} (as {stem} — name was reserved)"),
        None => format!("created {typed}"),
    }
}

/// The confirmation message for a rename, noting the actual on-disk name when it
/// was disambiguated (reserved-name collision, R1 D1).
fn renamed_message(typed: &str, path: &Path) -> String {
    match disambiguated_stem(typed, path) {
        Some(stem) => format!("renamed to {typed} (as {stem} — name was reserved)"),
        None => format!("renamed to {typed}"),
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
        let (tx, rx) = mpsc::channel(APP_CHANNEL_CAPACITY);
        Ok(Self {
            workspace,
            explorer,
            buffers: Vec::new(),
            active: 0,
            focus: Pane::Explorer,
            mode: Mode::Normal,
            picker: None,
            search_targets: Vec::new(),
            palette_actions: Vec::new(),
            profile_choices: Vec::new(),
            workspace_choices: Vec::new(),
            sequence_choices: Vec::new(),
            load_runner_after_pick: false,
            sequence_pick_runs: false,
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
            generation: 0,
            orphan_response: ResponseState::Idle,
            orphan_geometry: ResponseGeometry::default(),
            pending_clipboard: None,
            body_search_editor: LineEditor::default(),
            body_search_return: Mode::Normal,
            highlight_tx: None,
            history: None,
            message: None,
            keymap_warnings: Vec::new(),
            keymap_warned: false,
            tick_count: 0,
            prompt_editor: LineEditor::default(),
            auth_picker: false,
            pending_load: None,
            pending_close: None,
            zoom: None,
            explorer_hidden: false,
            left_active: LeftPane::Endpoints,
            focus_before_explorer: None,
            leader: None,
            help_open: false,
            help_scroll: 0,
            help_viewport_height: 10,
            help_search: None,
            help_search_editor: LineEditor::default(),
            help_search_input: false,
            url_edit_mode: UrlEditMode::Inline,
            env_editor: None,
            sequence_runner: None,
            sequence_abort: None,
            sequence_editor: None,
            sequence_view: SeqView::Edit,
            load_runner: None,
            load_abort: None,
            load_caps: churl_core::load::LoadCaps::default(),
            load_request: None,
            session_vars: HashMap::new(),
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

    /// Sets a transient, auto-expiring message in the message row.
    fn notify(&mut self, text: impl Into<String>) {
        self.message = Some(Message::new(text));
    }

    /// Records the load-time keymap conflict/shadow warnings (M7.10) so the run
    /// loop can flash a first-frame toast. Called by the TUI entry point after
    /// [`KeyMap::validate`].
    pub fn set_keymap_warnings(&mut self, warnings: Vec<String>) {
        self.keymap_warnings = warnings;
    }

    /// The load-time keymap warnings, for tests/inspection.
    #[cfg(test)]
    pub fn keymap_warnings(&self) -> &[String] {
        &self.keymap_warnings
    }

    /// Fires the first-frame keymap-warning toast exactly once (M7.10). No-op
    /// when the config is clean or the toast has already fired.
    fn arm_keymap_warning_toast(&mut self) {
        if self.keymap_warned || self.keymap_warnings.is_empty() {
            return;
        }
        self.keymap_warned = true;
        let n = self.keymap_warnings.len();
        let plural = if n == 1 { "issue" } else { "issues" };
        self.notify(format!("⚠ {n} keymap {plural} — run churl keymaps"));
    }

    // ---- Buffer accessors ----

    /// The active buffer, or `None` when nothing is loaded.
    fn active_buffer(&self) -> Option<&Buffer> {
        self.buffers.get(self.active)
    }

    fn active_buffer_mut(&mut self) -> Option<&mut Buffer> {
        self.buffers.get_mut(self.active)
    }

    /// The index of the buffer whose endpoint file is `path`, if any. The dedup
    /// key for Stage 2's open-or-focus (Stage 1 has at most one buffer).
    fn buffer_index_for_path(&self, path: &Path) -> Option<usize> {
        self.buffers.iter().position(|b| b.file() == path)
    }

    /// The active buffer's endpoint payload — the forward-compat seam every
    /// endpoint-only reader routes through (a future `Sequence` buffer yields
    /// `None`, so those actions no-op cleanly). `None` when nothing is loaded.
    fn active_endpoint_buffer(&self) -> Option<&EndpointBuffer> {
        self.active_buffer().and_then(Buffer::as_endpoint)
    }

    /// The active buffer's endpoint payload (mutable). See
    /// [`App::active_endpoint_buffer`].
    fn active_endpoint_buffer_mut(&mut self) -> Option<&mut EndpointBuffer> {
        self.active_buffer_mut().and_then(Buffer::as_endpoint_mut)
    }

    /// The active endpoint's live [`SelectedEndpoint`], for tests / snapshot
    /// drivers that inspect the loaded endpoint.
    pub fn selected(&self) -> Option<&SelectedEndpoint> {
        self.active_endpoint_buffer().map(|b| &b.endpoint)
    }

    /// Mutable access to the active endpoint's [`SelectedEndpoint`] (tests).
    pub fn selected_mut(&mut self) -> Option<&mut SelectedEndpoint> {
        self.active_endpoint_buffer_mut().map(|b| &mut b.endpoint)
    }

    /// The endpoint under the explorer cursor (the *hovered* endpoint), when the
    /// left column is on the endpoints tree and the cursor sits on an endpoint
    /// row (M7.10 stage B). A read-only peek — it does not load a buffer or move
    /// the cursor. Used as a fallback for one-shot read actions (copy-as-curl,
    /// the load runner) when no endpoint is loaded, so they act on what the user
    /// is pointing at rather than refusing.
    fn hovered_endpoint(&self) -> Option<SelectedEndpoint> {
        if self.left_active != LeftPane::Endpoints {
            return None;
        }
        self.explorer.hovered_endpoint()
    }

    /// Which surface owns the "active response" the shared `response_*` handlers
    /// act on (note #2). Resolves by mode + runner focus: a runner whose Response
    /// region is focused routes the response actions onto its selected row/step;
    /// otherwise the main endpoint buffer (today's behaviour) is active. When the
    /// body-search input is open it resolves against the mode search was opened
    /// over ([`Self::body_search_return`]), so search-into-view keeps targeting the
    /// runner response the `/` was launched from.
    fn active_response_surface(&self) -> ResponseSurface {
        let effective = if self.mode == Mode::BodySearch {
            self.body_search_return
        } else {
            self.mode
        };
        match effective {
            Mode::LoadRunner => match &self.load_runner {
                Some(runner)
                    if runner.focus == load_runner::RunnerFocus::Response
                        && !runner.response_input_captured() =>
                {
                    ResponseSurface::LoadRunner
                }
                _ => ResponseSurface::Main,
            },
            Mode::Sequence if self.sequence_view == SeqView::Run => match &self.sequence_runner {
                Some(runner)
                    if runner.focus == sequence_runner::RunnerFocus::Response
                        && !runner.response_input_captured() =>
                {
                    ResponseSurface::Sequence
                }
                _ => ResponseSurface::Main,
            },
            _ => ResponseSurface::Main,
        }
    }

    /// The active response state for internal readers (render + `response_*`
    /// handlers). Resolves the runner's selected row/step when a runner Response
    /// region is focused (note #2), else the active endpoint buffer, else the
    /// test-only orphan slot (isolation snapshots) when nothing is loaded.
    fn active_response(&self) -> &ResponseState {
        match self.active_response_surface() {
            ResponseSurface::LoadRunner => self
                .load_runner
                .as_ref()
                .and_then(|r| r.selected_response())
                .unwrap_or(&self.orphan_response),
            ResponseSurface::Sequence => self
                .sequence_runner
                .as_ref()
                .and_then(|r| r.selected_response())
                .unwrap_or(&self.orphan_response),
            ResponseSurface::Main => match self.active_endpoint_buffer() {
                Some(b) => &b.response,
                None => &self.orphan_response,
            },
        }
    }

    /// Mutable active response, resolving the runner's selected row/step when a
    /// runner Response region is focused (note #2), else the active buffer, else
    /// the orphan slot when nothing is loaded (isolation snapshots).
    fn active_response_mut(&mut self) -> &mut ResponseState {
        match self.active_response_surface() {
            ResponseSurface::LoadRunner => self
                .load_runner
                .as_mut()
                .and_then(|r| r.selected_response_mut())
                .unwrap_or(&mut self.orphan_response),
            ResponseSurface::Sequence => self
                .sequence_runner
                .as_mut()
                .and_then(|r| r.selected_response_mut())
                .unwrap_or(&mut self.orphan_response),
            ResponseSurface::Main => match self
                .buffers
                .get_mut(self.active)
                .and_then(Buffer::as_endpoint_mut)
            {
                Some(b) => &mut b.response,
                None => &mut self.orphan_response,
            },
        }
    }

    /// The active response geometry (cursor/scroll/viewport), mode + focus-aware:
    /// the same [`ResponseSurface`] resolution as [`Self::active_response`], so a
    /// runner Response region's motion/search/copy operate on the runner's own
    /// geometry (note #2).
    fn active_response_geometry(&self) -> &ResponseGeometry {
        match self.active_response_surface() {
            ResponseSurface::LoadRunner => self
                .load_runner
                .as_ref()
                .map(|r| &r.geometry)
                .unwrap_or(&self.orphan_geometry),
            ResponseSurface::Sequence => self
                .sequence_runner
                .as_ref()
                .map(|r| &r.geometry)
                .unwrap_or(&self.orphan_geometry),
            ResponseSurface::Main => match self.active_endpoint_buffer() {
                Some(b) => &b.geometry,
                None => &self.orphan_geometry,
            },
        }
    }

    /// Mutable active response geometry (see [`Self::active_response_geometry`]).
    fn active_response_geometry_mut(&mut self) -> &mut ResponseGeometry {
        match self.active_response_surface() {
            ResponseSurface::LoadRunner => self
                .load_runner
                .as_mut()
                .map(|r| &mut r.geometry)
                .unwrap_or(&mut self.orphan_geometry),
            ResponseSurface::Sequence => self
                .sequence_runner
                .as_mut()
                .map(|r| &mut r.geometry)
                .unwrap_or(&mut self.orphan_geometry),
            ResponseSurface::Main => match self
                .buffers
                .get_mut(self.active)
                .and_then(Buffer::as_endpoint_mut)
            {
                Some(b) => &mut b.geometry,
                None => &mut self.orphan_geometry,
            },
        }
    }

    /// Clears the highlight cache + pending-highlight guard for the active response
    /// surface. The runners share the active endpoint buffer's cache/guard (they
    /// render through it), so all three surfaces route here to the same place — the
    /// active buffer's cache when one is loaded (a no-op on the orphan slot).
    fn clear_active_highlight(&mut self) {
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.pending_highlight = None;
            b.highlight_cache.clear();
        }
    }

    /// The active buffer's response state (tests / snapshot drivers).
    pub fn response(&self) -> &ResponseState {
        self.active_response()
    }

    /// Sets the active buffer's response state, or the orphan slot when nothing
    /// is loaded (isolation snapshots).
    pub fn set_response(&mut self, response: ResponseState) {
        match self.active_endpoint_buffer_mut() {
            Some(b) => b.response = response,
            None => self.orphan_response = response,
        }
    }

    /// Mutable access to the active buffer's response state, or the orphan slot
    /// when nothing is loaded (isolation snapshots that assign
    /// `*app.response_mut() = …` without a loaded endpoint).
    pub fn response_mut(&mut self) -> &mut ResponseState {
        match self
            .buffers
            .get_mut(self.active)
            .and_then(Buffer::as_endpoint_mut)
        {
            Some(b) => &mut b.response,
            None => &mut self.orphan_response,
        }
    }

    /// Loads an endpoint into a single active buffer (test/white-box entry point;
    /// production code goes through [`App::open_or_focus_buffer`]).
    #[cfg(test)]
    fn load_endpoint(&mut self, selected: SelectedEndpoint) {
        self.open_or_focus_buffer(selected);
    }

    /// White-box: the active endpoint buffer's editor (tests).
    #[cfg(test)]
    fn test_editor(&mut self) -> &mut EditorState {
        &mut self
            .active_endpoint_buffer_mut()
            .expect("no active endpoint buffer")
            .editor
    }

    /// White-box: the active endpoint buffer's request tabs (tests).
    #[cfg(test)]
    fn test_tabs(&mut self) -> &mut RequestTabs {
        &mut self
            .active_endpoint_buffer_mut()
            .expect("no active endpoint buffer")
            .tabs
    }

    /// White-box: the active buffer's inline URL editor slot (tests).
    #[cfg(test)]
    fn test_url_editor(&self) -> &Option<LineEditor> {
        &self
            .active_endpoint_buffer()
            .expect("no active endpoint buffer")
            .url_editor
    }

    /// White-box: the active buffer's URL popup slot, immutable (tests).
    #[cfg(test)]
    fn test_url_popup(&self) -> &Option<EditorState> {
        &self
            .active_endpoint_buffer()
            .expect("no active endpoint buffer")
            .url_popup
    }

    /// White-box: the active buffer's URL popup slot, mutable (tests).
    #[cfg(test)]
    fn test_url_popup_mut(&mut self) -> &mut Option<EditorState> {
        &mut self
            .active_endpoint_buffer_mut()
            .expect("no active endpoint buffer")
            .url_popup
    }

    /// White-box: whether nothing is loaded. In the buffer model the snapshot is
    /// non-Option, so "no snapshot" means "no buffer" (tests that asserted
    /// `loaded_snapshot.is_none()`).
    #[cfg(test)]
    fn test_no_snapshot(&self) -> bool {
        self.active_endpoint_buffer().is_none()
    }

    /// The active request tab, or the pre-refactor default (`RequestTabs`'s
    /// `Params`) when nothing is loaded. Used by the event-routing guards, which
    /// pre-refactor read the flat `tabs.active` field that always existed.
    fn active_tab(&self) -> RequestTab {
        self.active_endpoint_buffer()
            .map(|b| b.tabs.active)
            .unwrap_or(RequestTab::Params)
    }

    /// Whether the active buffer's Body editor is in a non-Normal edtui mode
    /// (Insert/Visual/Search) — the interception guard. `false` when nothing is
    /// loaded.
    fn body_editor_non_normal(&self) -> bool {
        self.active_endpoint_buffer()
            .is_some_and(|b| b.editor.mode != EditorMode::Normal)
    }

    /// Whether a request-row field edit is in progress on the active buffer.
    /// `false` when nothing is loaded.
    fn tabs_editing_active(&self) -> bool {
        self.active_endpoint_buffer()
            .is_some_and(|b| b.tabs.editing.is_some())
    }

    /// Forwards a key into the active buffer's Body edtui editor. No-op when
    /// nothing is loaded (pre-refactor the flat editor absorbed it invisibly).
    fn body_editor_on_key(&mut self, key: KeyEvent) {
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.editor_events.on_key_event(key, &mut b.editor);
        }
    }

    /// Runs churl-side vim motions (W/B/^/f/F/t/T) on the active buffer's Body
    /// editor; returns whether the key was consumed. `false` when nothing is
    /// loaded.
    fn body_vim_handle_key(&mut self, key: KeyEvent) -> bool {
        match self.active_endpoint_buffer_mut() {
            Some(b) => vim_ext::handle_key(key, &mut b.editor, &mut b.editor_vim),
            None => false,
        }
    }

    /// Whether the inline URL editor is open on the active buffer.
    fn url_editor_active(&self) -> bool {
        self.active_endpoint_buffer()
            .is_some_and(|b| b.url_editor.is_some())
    }

    /// Whether the URL popup editor is open on the active buffer.
    fn url_popup_active(&self) -> bool {
        self.active_endpoint_buffer()
            .is_some_and(|b| b.url_popup.is_some())
    }

    /// The success message of a queued-but-not-yet-flushed clipboard copy, if
    /// any. Exposed for tests: the actual copy (and its real success/fail
    /// message) runs in the terminal-owning run loop, which drives a real
    /// clipboard — tests assert on the queued intent instead.
    pub fn pending_copy_message(&self) -> Option<&str> {
        self.pending_clipboard
            .as_ref()
            .map(|p| p.success_msg.as_str())
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

    /// The in-memory Session store key for the current workspace: its canonical
    /// root path. `None` when no workspace is open (nothing to key on).
    fn session_key(&self) -> Option<PathBuf> {
        self.workspace.as_ref().map(|ws| canonical_path(ws.root()))
    }

    /// The current workspace's in-memory Session captures (note #6), empty when
    /// none or no workspace. The highest resolver scope for a standalone send and
    /// (threaded through [`RunScopes`]) for a sequence run.
    fn session_vars(&self) -> BTreeMap<String, String> {
        self.session_key()
            .and_then(|key| self.session_vars.get(&key).cloned())
            .unwrap_or_default()
    }

    /// Writes `name → value` into the current workspace's Session store,
    /// creating the workspace entry on first write and overwriting an existing
    /// name (a re-login refreshes the token). In-memory only — never persisted.
    /// No-op when no workspace is open.
    fn write_session_var(&mut self, name: String, value: String) {
        let Some(key) = self.session_key() else {
            return;
        };
        self.session_vars
            .entry(key)
            .or_default()
            .insert(name, value);
    }

    /// Clears the current workspace's Session captures (the env-editor Session
    /// group's clear action). Returns whether anything was cleared.
    fn clear_session_vars(&mut self) -> bool {
        let Some(key) = self.session_key() else {
            return false;
        };
        match self.session_vars.get_mut(&key) {
            Some(map) if !map.is_empty() => {
                map.clear();
                true
            }
            _ => false,
        }
    }

    /// Builds the template [`Resolver`] for a send of `selected`, in precedence
    /// order: in-memory Session captures → cli `--var` → active profile → the
    /// endpoint's collection `folder.toml` vars → workspace `[vars]` → process env
    /// (implicit). The Session scope (note #6) sits at the top so a standalone
    /// request using `{{token}}` resolves a value captured by an earlier sequence
    /// run; it is empty until a Session-target rule writes into it.
    fn build_resolver(&mut self, selected: &SelectedEndpoint) -> Resolver {
        let collection_vars = self.explorer.collection_vars(selected.collection);
        Resolver::new(vec![
            Scope::new("session", self.session_vars()),
            Scope::new("cli", self.cli_vars.clone()),
            Scope::new("profile", self.profile_vars()),
            Scope::new("collection", collection_vars),
            Scope::new("workspace", self.workspace_vars()),
        ])
    }

    /// Builds the resolver used by the env-editor's ephemeral peek (drive-test
    /// note #3). It mirrors [`build_resolver`] but omits the per-endpoint
    /// `collection` scope — the env editor is not tied to a loaded endpoint, so
    /// there is no single collection to consult (a collection var resolves
    /// per-request, not globally). Session captures still sit highest, so a peeked
    /// `{{token}}` reveals what a standalone send would use. The resolved value is
    /// returned by value and never stored — the caller hands it straight to the
    /// editor's transient reveal state.
    fn build_env_resolver(&self) -> Resolver {
        Resolver::new(vec![
            Scope::new("session", self.session_vars()),
            Scope::new("cli", self.cli_vars.clone()),
            Scope::new("profile", self.profile_vars()),
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
        self.load_caps = config.load_caps();
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
        // First-frame keymap-warning toast (M7.10): if the loaded config has
        // conflict/shadow issues, flash a non-blocking statusline notice once
        // (stderr already carried the detail before raw mode; `churl keymaps`
        // has the full list).
        self.arm_keymap_warning_toast();
        while !self.should_quit {
            terminal.draw(|frame| render(frame, self))?;
            tokio::select! {
                maybe_event = events.next() => match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        self.handle_key(key)?;
                        // Flush any clipboard copy a key handler queued: native
                        // clipboard first, then OSC 52 (with tmux/screen
                        // passthrough) on the terminal backend — which dispatch has
                        // no handle to, so the run loop owns this. The message
                        // reports the real outcome (Constitution: fail loudly).
                        if let Some(pending) = self.pending_clipboard.take() {
                            let outcome = clipboard::copy(
                                pending.payload.as_str(),
                                terminal.backend_mut(),
                            );
                            let msg = if outcome.succeeded() {
                                pending.success_msg
                            } else {
                                "copy failed".to_owned()
                            };
                            self.notify(msg);
                        }
                    }
                    Some(Ok(_)) => {} // resize etc. — redraw happens next iteration
                    Some(Err(err)) => return Err(err.into()),
                    None => break, // input stream closed
                },
                _ = tick.tick() => {
                    self.tick_count = self.tick_count.wrapping_add(1);
                    if self.message.as_ref().is_some_and(|s| s.is_expired()) {
                        self.message = None;
                    }
                    // Re-mask an ephemeral secret peek on timeout (drive-test
                    // note #3), on the same 250 ms cadence that expires messages.
                    if let Some(editor) = self.env_editor.as_mut() {
                        editor.expire_reveal();
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
        if self.url_popup_active() {
            return self.handle_url_popup_key(key);
        }
        if self.leader.is_some() {
            return self.handle_leader_key(key);
        }
        match self.mode {
            Mode::Search | Mode::Palette | Mode::WorkspacePicker | Mode::SequencePicker => {
                self.handle_overlay_key(key)
            }
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
            Mode::Sequence => self.handle_sequence_key(key),
            Mode::LoadRunner => self.handle_load_runner_key(key),
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    /// Key routing in [`Mode::Normal`]: inline editors (URL bar / request row edit)
    /// intercept first (with the Ctrl-S/Ctrl-C exception), then the focused pane's
    /// keymap overlay + global map, then edtui fall-through for the Request pane.
    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<()> {
        // 1. Inline URL editing: the LineEditor owns keys, except the documented
        //    Ctrl-S/Ctrl-C interception (Send/Quit reach through).
        if self.url_editor_active() {
            if let Some(action) = self.control_intercept(key) {
                return self.dispatch(action, Some(key));
            }
            return self.handle_url_edit_key(key);
        }
        // 2. Inline request-row field editing (same interception rule).
        if self.focus == Pane::Request && self.tabs_editing_active() {
            if let Some(action) = self.control_intercept(key) {
                return self.dispatch(action, Some(key));
            }
            return self.handle_field_edit_key(key);
        }
        // 3. edtui insert/visual mode on the Body tab (M4 interception exception).
        if self.focus == Pane::Request
            && self.active_tab() == RequestTab::Body
            && self.body_editor_non_normal()
        {
            if let Some(action) = self.control_intercept(key) {
                return self.dispatch(action, Some(key));
            }
            self.body_editor_on_key(key);
            return Ok(());
        }
        // 3b. Body tab in Normal mode: churl-side vim motions (W/B/^/f/F/t/T) win
        //     before leader/keymap. `f` becomes find-char, shadowing the global
        //     Jump key there (DECISIONS.md — M6.6 shadowing precedent). Precedes
        //     leader/keymap so a pending find's next char reaches vim_ext even
        //     when it's Space or a mapped key.
        if self.focus == Pane::Request
            && self.active_tab() == RequestTab::Body
            && !self.body_editor_non_normal()
            && self.body_vim_handle_key(key)
        {
            return Ok(());
        }
        // 4. Leader key: outside every text-edit context (guarded above), Space
        //    enters pending-leader state and shows the which-key popup. Inside an
        //    edit, control never reaches here — Space types a space.
        if self.keymap.is_leader(key) {
            self.leader = Some(LeaderState::Root);
            return Ok(());
        }
        // 5. Keymap: the focused pane's overlay wins over the global map.
        if let Some(action) = self.keymap.lookup_ctx(key, self.focus.ctx()) {
            self.dispatch(action, Some(key))
        } else if self.focus == Pane::Request && self.active_tab() == RequestTab::Body {
            // Unmapped key on the Body tab falls through to edtui.
            self.body_editor_on_key(key);
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

    /// The two-level which-key state machine. At Root: Esc/unknown cancel, a
    /// direct action dispatches (and closes), a submenu prefix descends. In a
    /// submenu: Esc backs out ONE level to Root, a bound key dispatches (and
    /// closes), an unknown key cancels. (A second Esc from Root cancels — "Esc
    /// backs out one level then cancels".)
    fn handle_leader_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(state) = self.leader.clone() else {
            return Ok(());
        };
        match state {
            LeaderState::Root => {
                if key.code == KeyCode::Esc {
                    self.leader = None;
                    return Ok(());
                }
                match self.keymap.leader_root_lookup(key) {
                    Some(LeaderEntry::Act(action)) => {
                        self.leader = None;
                        self.dispatch(action, Some(key))
                    }
                    Some(LeaderEntry::Submenu(menu)) => {
                        self.leader = Some(LeaderState::Submenu(menu));
                        Ok(())
                    }
                    None => {
                        self.leader = None;
                        Ok(())
                    }
                }
            }
            LeaderState::Submenu(menu) => {
                if key.code == KeyCode::Esc {
                    self.leader = Some(LeaderState::Root);
                    return Ok(());
                }
                match self.keymap.leader_sub_lookup(&menu, key) {
                    Some(action) => {
                        self.leader = None;
                        self.dispatch(action, Some(key))
                    }
                    None => {
                        self.leader = None;
                        Ok(())
                    }
                }
            }
        }
    }

    /// Handles one key while the help-search input is open. Every keystroke
    /// recomputes matches (smart-case, via the shared matcher) and jumps to the
    /// current one; Enter commits (keeps matches for `n`/`N`, closes the input),
    /// Esc cancels (clears the search, keeps the help overlay open). Mirrors
    /// [`Self::handle_body_search_key`].
    fn handle_help_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.help_search = None;
                self.help_search_input = false;
            }
            KeyCode::Enter => {
                self.help_search_input = false;
                self.help_center_on_match();
            }
            _ => {
                self.help_search_editor.handle_key(key);
                let query = self.help_search_editor.text();
                if let Some(search) = self.help_search.as_mut() {
                    search.set_query(query, &self.keymap, &self.theme);
                }
                self.help_center_on_match();
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
                if is_ctrl_c
                    && self
                        .active_endpoint_buffer()
                        .is_some_and(|b| b.in_flight.is_some())
                {
                    self.cancel_request();
                } else {
                    self.should_quit = true;
                }
            }
            Action::FocusNext => {
                let target = self.skip_hidden_explorer(self.focus.next(), true);
                self.set_focus(target);
            }
            Action::FocusPrev => {
                let target = self.skip_hidden_explorer(self.focus.prev(), false);
                self.set_focus(target);
            }
            Action::FocusExplorer => self.set_focus(Pane::Explorer),
            Action::FocusUrlBar => self.set_focus(Pane::UrlBar),
            Action::FocusRequest => self.set_focus(Pane::Request),
            Action::FocusResponse => self.set_focus(Pane::Response),
            Action::ToggleExplorer => self.toggle_explorer(),
            Action::FocusSequencesToggle => self.focus_sequences_toggle(),
            Action::CycleRegionFwd => self.cycle_region(true),
            Action::CycleRegionBack => self.cycle_region(false),
            Action::Zoom => self.toggle_zoom(),
            Action::Help => self.help_open = true,
            Action::Leader => self.leader = Some(LeaderState::Root),
            Action::EditUrlPopup => self.begin_url_popup(),
            Action::OpenSearch => self.open_search()?,
            Action::OpenPalette => self.open_palette(),
            Action::Jump => self.open_jump(),
            Action::SwitchProfile => self.open_profile_picker(),
            Action::OpenEnvEditor => self.open_env_editor(),
            Action::RunSequence => self.run_selected_sequence(),
            Action::EditSequence => self.edit_selected_sequence()?,
            Action::OpenSequencePicker => self.open_sequence_picker(false),
            Action::RunSequencePick => self.open_sequence_picker(true),
            Action::OpenLoadRunner => self.open_load_runner(),
            Action::OpenLoadRunnerPick => self.open_load_runner_pick()?,
            Action::Send => self.send_request(),
            Action::Cancel => self.cancel_request(),
            Action::Save => self.save_request(),
            Action::EditUrl => self.begin_url_edit(),
            Action::MethodCycle => self.cycle_method(),
            Action::MethodMenu => self.open_method_menu(),
            Action::TabNext => self.with_active_tabs(RequestTabs::tab_next),
            Action::TabPrev => self.with_active_tabs(RequestTabs::tab_prev),
            Action::Tab1 => self.with_active_tabs(|t| t.tab_jump(0)),
            Action::Tab2 => self.with_active_tabs(|t| t.tab_jump(1)),
            Action::Tab3 => self.with_active_tabs(|t| t.tab_jump(2)),
            Action::Tab4 => self.with_active_tabs(|t| t.tab_jump(3)),
            // On the Body tab there are no rows: the originating key (i/a/d/…)
            // belongs to edtui, same as the motion keys in `request_nav`.
            Action::RowAdd | Action::RowDelete | Action::RowToggle | Action::RowEdit
                if self.focus == Pane::Request && self.active_tab() == RequestTab::Body =>
            {
                if let Some(key) = key {
                    self.body_editor_on_key(key);
                }
            }
            Action::RowAdd => self.row_add(),
            Action::RowDelete => self.row_delete(),
            Action::RowToggle => self.row_toggle(),
            Action::RowEdit => self.row_edit(),
            // On the sequences sub-pane, tree-mutating keys must NEVER touch the
            // hidden endpoints tree cursor: `n` creates a new sequence (parallels
            // endpoints `n`), `N`/`d` no-op with a note.
            Action::NewEndpoint if self.left_column_on_sequences() => self.new_sequence_prompt(),
            Action::NewEndpoint => self.begin_new_endpoint(),
            Action::NewCollection if self.left_column_on_sequences() => {
                self.notify("not available in the sequences pane")
            }
            Action::NewCollection => self.begin_new_collection(),
            // `r` on the sequences sub-pane runs the hovered sequence (Run face);
            // everywhere else it renames the selected tree node.
            Action::Rename if self.left_column_on_sequences() => self.run_selected_sequence(),
            Action::Rename => self.begin_rename(),
            // `d` on the sequences sub-pane deletes the hovered sequence (parallels
            // endpoints `d`); everywhere else it deletes the selected tree node.
            Action::Delete if self.left_column_on_sequences() => self.begin_delete_sequence(),
            Action::Delete => self.begin_delete(),
            Action::DeleteSequence => self.begin_delete_sequence(),
            Action::HalfPageDown | Action::HalfPageUp => {
                if self.focus == Pane::Response {
                    self.response_half_page(matches!(action, Action::HalfPageDown));
                }
            }
            Action::ToggleHeadersView => self.response_toggle_headers(),
            Action::ToggleWrap => self.response_toggle_wrap(),
            Action::TogglePretty => self.response_toggle_pretty(),
            Action::ToggleSortKeys => self.response_toggle_sort_keys(),
            Action::ToggleLineNumbers => self.response_toggle_line_numbers(),
            Action::OpenBodySearch => self.open_body_search(),
            Action::SearchNext => self.response_search_step(true),
            Action::SearchPrev => self.response_search_step(false),
            Action::ToggleFold => self.response_toggle_fold(),
            Action::ToggleAllFolds => self.response_toggle_all_folds(),
            Action::CopyResponse => self.response_copy_view(),
            Action::CopyLine => self.response_copy_line(),
            Action::ScrollBodyLeft => self.response_scroll_h(false),
            Action::ScrollBodyRight => self.response_scroll_h(true),
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
            Action::BufferNext => self.buffer_cycle(true),
            Action::BufferPrev => self.buffer_cycle(false),
            Action::BufferClose => self.close_buffer(self.active),
            Action::BufferCloseAll => self.close_all_buffers(),
            Action::FocusBufferIndex(n) => self.focus_buffer_index(n),
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
        if self.active_tab() == RequestTab::Body {
            if let Some(key) = key {
                self.body_editor_on_key(key);
            }
            return;
        }
        match action {
            Action::Up => self.with_active_tabs(RequestTabs::move_up),
            Action::Down => {
                let n = self.active_tab_row_count();
                self.with_active_tabs(|t| t.move_down(n));
            }
            Action::Select => self.row_edit(),
            _ => {}
        }
    }

    /// Runs `f` against the active buffer's request tabs, if any. Endpoint-only;
    /// no-op when nothing is loaded (matches the pre-refactor flat `tabs` being
    /// a no-op target with nothing selected).
    fn with_active_tabs(&mut self, f: impl FnOnce(&mut RequestTabs)) {
        if let Some(b) = self.active_endpoint_buffer_mut() {
            f(&mut b.tabs);
        }
    }

    /// True when the left column is focused AND the sequences sub-pane holds the
    /// in-pane focus/zoom (PR 2b). Guards the sequence-specific `h/j/k/l/Enter/r`
    /// routing so it never fires while the endpoints tree is active.
    fn left_column_on_sequences(&self) -> bool {
        self.focus == Pane::Explorer && self.left_active == LeftPane::Sequences
    }

    fn explorer_action(&mut self, action: Action) -> Result<()> {
        // PR 2b: when the sequences sub-pane is the active occupant of the left
        // column, in-pane nav drives the sequence cursor (a flat list — h/l
        // no-op) and Enter opens the unified surface (Edit face) on the hovered
        // sequence.
        if self.left_active == LeftPane::Sequences {
            match action {
                Action::Up => self.explorer.seq_move_up(),
                Action::Down => self.explorer.seq_move_down(),
                Action::Top => self.explorer.seq_top(),
                Action::Bottom => self.explorer.seq_bottom(),
                // Flat list: collapse/expand are no-ops.
                Action::Collapse | Action::Expand => {}
                Action::Select => self.edit_selected_sequence()?,
                _ => unreachable!("only navigation actions reach explorer_action"),
            }
            return Ok(());
        }
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
                // The guarded seam: switching to a *different* endpoint while
                // dirty prompts to save/discard first (a collection toggle /
                // same endpoint is not guarded).
                self.activate_explorer_row(self.explorer.cursor)?;
            }
            _ => unreachable!("only navigation actions reach explorer_action"),
        }
        Ok(())
    }

    /// The loaded live request (with in-memory edits), or `None`.
    fn live_request(&self) -> Option<&Request> {
        self.active_endpoint_buffer()
            .map(EndpointBuffer::live_request)
    }

    /// Mirrors the current edtui body text into the active buffer's live request
    /// `body` so the live endpoint reflects unsaved body edits (for dirty
    /// derivation and save). A body that becomes empty drops the `Body` entirely.
    fn sync_body_into_selected(&mut self) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let text = String::from(b.editor.lines.clone());
        fold_body_text(&mut b.endpoint.endpoint.request, text);
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

    // ---- Response viewer M7 actions ----

    /// Queues `text` (capped at [`clipboard::MAX_COPY_BYTES`], on a char
    /// boundary) for a layered clipboard write on the next loop iteration.
    /// `success_msg` is shown only if a clipboard path plausibly succeeded;
    /// otherwise the run loop reports "copy failed". Returns the queued byte
    /// count for callers that report it.
    fn enqueue_clipboard(&mut self, text: &str, success_msg: impl Into<String>) -> usize {
        let (payload, len) = clipboard::cap_payload(text);
        self.pending_clipboard = Some(PendingCopy {
            payload,
            success_msg: success_msg.into(),
        });
        len
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

    /// Enters jump-mode, labelling the five pane regions (M7.10 stage B —
    /// pane-only, no row labels; row precision is the leader pickers' job).
    fn open_jump(&mut self) {
        self.jump = Some(JumpState::new());
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
        // An assigned label wins first: `f` (the default Jump key) isn't a region
        // mnemonic (e/s/u/r/p), but label-lookup precedence stays robust to a remap.
        if let KeyCode::Char(c) = key.code
            && let Some(target) = jump.target_for(c)
        {
            self.close_jump();
            match target {
                // Via set_focus (not raw assignment) so jumping into a collapsed
                // pane transfers the zoom to it (the set_focus invariant).
                // The `e` label must land on the endpoints tree (mirroring `s` →
                // Sequences). Without resetting `left_active`, an `f e` from a
                // focused Sequences sub-pane would stay on Sequences and appear to
                // do nothing (owner drive-test 2026-07-10).
                JumpTarget::Pane(Pane::Explorer) => {
                    self.left_active = LeftPane::Endpoints;
                    self.set_focus(Pane::Explorer);
                }
                JumpTarget::Pane(pane) => self.set_focus(pane),
                // Sequences sub-pane lives in the left column: focus it and make
                // it active. An EXPLICIT `f s` sticks even on an empty list — the
                // pane zooms in and shows an informative empty state (note #3);
                // `set_focus` no longer force-reverts to Endpoints.
                JumpTarget::Sequences => {
                    self.left_active = LeftPane::Sequences;
                    self.set_focus(Pane::Explorer);
                }
            }
            return Ok(());
        }
        // Pressing the Jump key again cancels (when it labels no target).
        if self.keymap.lookup(key) == Some(Action::Jump) {
            self.close_jump();
        }
        Ok(())
    }

    /// Opens the profile picker over the workspace's profile names plus a
    /// "(none)" entry; the active entry is prefixed with `● ` (display-only —
    /// `profile_choices` carries raw names so filtering by the profile name still
    /// works, since `● dev` contains `dev`).
    fn open_profile_picker(&mut self) {
        let active = self.active_profile.as_deref();
        let mut choices: Vec<Option<String>> = vec![None];
        let none_label = if active.is_none() {
            "● (none)".to_owned()
        } else {
            "(none)".to_owned()
        };
        let mut labels: Vec<String> = vec![none_label];
        if let Some(ws) = &self.workspace {
            for profile in &ws.manifest().profiles {
                choices.push(Some(profile.name.clone()));
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

    /// Enter (or a jump label) on the current explorer row: loads the endpoint
    /// through the guarded seam. One seam so both the explorer `Enter` and
    /// jump-mode agree. (Sequences are no longer tree rows — they activate via
    /// the sub-pane, PR 2b.)
    fn activate_explorer_row(&mut self, row: usize) -> Result<()> {
        self.explorer.cursor = row;
        self.guarded_load(PendingLoad::Row(row))
    }

    // ---- M7.5 concurrent-load runner ----

    /// `<leader>l f` / palette: pick an endpoint via the fuzzy search overlay,
    /// then open the load runner over it. Reuses the endpoint-search overlay + its
    /// dirty-safe load path; a one-shot flag makes `accept_overlay` chain into
    /// `open_load_runner` once the endpoint has loaded.
    fn open_load_runner_pick(&mut self) -> Result<()> {
        self.open_search()?;
        self.load_runner_after_pick = true;
        Ok(())
    }

    /// `r` in the runner: classify the config against the guardrail caps and act.
    /// Refuse → message (no run); Warn → loud confirm naming the target URL; Ok →
    /// run immediately.
    fn request_load_run(&mut self) {
        let Some(runner) = self.load_runner.as_ref() else {
            return;
        };
        let cfg = runner.cfg;
        match churl_core::load::check_config(&cfg, &self.load_caps) {
            LoadCheck::Refuse(reason) => self.notify(format!("load refused: {reason}")),
            LoadCheck::Warn(reason) => {
                let text = format!(
                    "Fire {} requests at concurrency {} against {}?  ({reason})",
                    cfg.total, cfg.concurrency, runner.url
                );
                if let Some(runner) = self.load_runner.as_mut() {
                    runner.pending_confirm = Some(text);
                }
            }
            LoadCheck::Ok => self.start_load_run(),
        }
    }

    /// The single interrupt seam shared by every batch-interrupt path (Ctrl-C
    /// cancel, `r` re-run mid-batch, and close mid-batch): if a run is in
    /// progress, record its partial summary marked cancelled (so a partial run is
    /// never lost from `load_batches`), then abort the single launcher (dropping
    /// the fan-out and every in-flight request) and bump the generation so
    /// straggler results are dropped. Always bumps the generation, even for the
    /// first run, so a fresh run starts on a distinct generation.
    fn interrupt_running_batch(&mut self) {
        let was_running = self
            .load_runner
            .as_ref()
            .is_some_and(LoadRunnerState::is_running);
        if was_running {
            // Record the partial (whatever completed so far), marked cancelled.
            self.write_load_summary(true);
        }
        if let Some(handle) = self.load_abort.take() {
            handle.abort();
        }
        if let Some(runner) = self.load_runner.as_mut() {
            runner.run_generation += 1;
        }
    }

    /// Closes the runner. If a batch is still running (close was confirmed via the
    /// runner's `q`→`y` guard), records its partial summary + aborts + bumps the
    /// generation through the shared interrupt seam before dropping the runner —
    /// so a run interrupted by closing is never lost from `load_batches`.
    fn close_load_runner(&mut self) {
        self.interrupt_running_batch();
        self.load_runner = None;
        self.load_request = None;
        self.mode = Mode::Normal;
    }

    fn close_overlay(&mut self) {
        self.picker = None;
        self.profile_choices.clear();
        self.workspace_choices.clear();
        self.sequence_choices.clear();
        self.auth_picker = false;
        // Clear the one-shot picker intents so an Esc-cancelled pick never leaks
        // into the next `<leader>f` / `/` search (`<leader>l f`) or sequence pick.
        self.load_runner_after_pick = false;
        self.sequence_pick_runs = false;
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
            KeyCode::Char('k') if ctrl => picker.move_up(),
            KeyCode::Char('j') if ctrl => picker.move_down(),
            KeyCode::Backspace => picker.backspace(&mut self.finder),
            KeyCode::Char(c) if !ctrl => picker.push_char(c, &mut self.finder),
            _ => {}
        }
        Ok(())
    }

    fn accept_overlay(&mut self) -> Result<()> {
        let mode = self.mode;
        let current = self.picker.as_ref().and_then(picker::PickerState::current);
        // Capture all choices + one-shot intents BEFORE close_overlay() clears
        // them, so an empty-result Enter (early-return below) still drops the flags
        // while a real pick can act on them.
        let profile_choices = std::mem::take(&mut self.profile_choices);
        let workspace_choices = std::mem::take(&mut self.workspace_choices);
        let sequence_choices = std::mem::take(&mut self.sequence_choices);
        let after_pick = std::mem::take(&mut self.load_runner_after_pick);
        let sequence_runs = std::mem::take(&mut self.sequence_pick_runs);
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
                    // jump_to only navigates; the load goes through the guarded
                    // seam so dirty edits are never lost silently.
                    if let Some(selected) = self.explorer.jump_to(collection, endpoint)? {
                        self.guarded_load(PendingLoad::File(selected.file.clone()))?;
                    }
                    // Only open the load runner if the endpoint actually loaded.
                    // A dirty editor + different pick makes `guarded_load` DEFER
                    // into a discard-changes confirm without loading — opening the
                    // runner then would fire over the STALE selection. (User
                    // re-triggers after resolving the confirm.)
                    let deferred =
                        matches!(self.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
                    if after_pick && !deferred {
                        self.open_load_runner();
                    }
                }
            }
            Mode::SequencePicker => {
                if let Some(path) = sequence_choices.get(index).cloned() {
                    // `sequence_runs` = `<leader>s r` run-vs-edit intent.
                    if sequence_runs {
                        self.run_sequence_at(path);
                    } else {
                        self.open_picked_sequence(path)?;
                    }
                }
            }
            Mode::Palette => {
                // profile_choices set → resolve a profile; else the command
                // palette resolves an action.
                if !profile_choices.is_empty() {
                    if let Some(choice) = profile_choices.get(index).cloned() {
                        self.set_profile(choice);
                    }
                } else if let Some(&action) = self.palette_actions.get(index) {
                    self.dispatch(action, None)?;
                }
            }
            Mode::WorkspacePicker => {
                // Through the dirty guard: a workspace target is always "other",
                // so unsaved edits defer to the discard confirm.
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

    /// M6.7: a hidden explorer drops out of the Tab ring — cycling skips it and
    /// lands on the next visible region. Only Explorer is ever hidden, so a single
    /// skip suffices. Explicit focus (jump-mode, `FocusExplorer`) still auto-reopens
    /// via `set_focus`.
    fn skip_hidden_explorer(&self, target: Pane, forward: bool) -> Pane {
        if target == Pane::Explorer && self.explorer_hidden {
            if forward {
                target.next()
            } else {
                target.prev()
            }
        } else {
            target
        }
    }

    fn set_focus(&mut self, pane: Pane) {
        if pane == Pane::Explorer && self.explorer_hidden {
            self.explorer_hidden = false;
        }
        // Remember the pane we left when focus moves INTO the left column, so
        // `<leader>e` can restore true prior focus on hide (owner #2B).
        if pane == Pane::Explorer && self.focus != Pane::Explorer {
            self.focus_before_explorer = Some(self.focus);
        }
        // Zoom follows focus: moving to the collapsed counterpart transfers the
        // zoom to it, so exactly one pane is ever zoomed and a collapsed pane
        // never holds focus.
        match (self.zoom, pane) {
            (Some(ZoomPane::Request), Pane::Response) => self.zoom = Some(ZoomPane::Response),
            (Some(ZoomPane::Response), Pane::Request) => self.zoom = Some(ZoomPane::Request),
            _ => {}
        }
        self.focus = pane;
    }

    /// Reconciles the left-column focus after a *passive* transition that may have
    /// emptied the sequence list (a disk reload / workspace switch): a focus that
    /// was resting on the sequences sub-pane is dropped back to `Endpoints` so it
    /// never strands on a list that vanished under it. This is NOT called from the
    /// generic focus path (`set_focus`) — an EXPLICIT focus on an empty Sequences
    /// pane (`f s`, the `s` overlay) is honored and shows an informative empty
    /// state (note #3); only passive emptying reconciles here. (Sub-pane always
    /// present — M7.10 stage B; empty pane focusable — note #3.)
    fn enforce_left_active_invariant(&mut self) {
        if self.explorer.sequences_len() == 0 {
            self.left_active = LeftPane::Endpoints;
        }
    }

    /// `<leader>e`: toggles the explorer sidebar. Hiding it while it is focused
    /// restores focus to the pane held before we entered the left column (owner
    /// #2B — true prior-pane restore, URL bar only as a last resort). Showing it
    /// re-focuses the left column, landing on the active sub-pane.
    fn toggle_explorer(&mut self) {
        if self.explorer_hidden {
            self.explorer_hidden = false;
            self.set_focus(Pane::Explorer);
        } else {
            // Hide: a hidden pane cannot hold focus.
            self.explorer_hidden = true;
            if self.focus == Pane::Explorer {
                self.focus = self.focus_before_explorer.take().unwrap_or(Pane::UrlBar);
            }
        }
    }

    /// Explorer overlay `s`: the endpoints⇄sequences mutually-exclusive-zoom
    /// switch — canonical keyboard route to the sequences sub-pane (alongside
    /// `f`-jump `s` and the `<leader>s f` picker). Focuses the left column.
    fn focus_sequences_toggle(&mut self) {
        self.left_active = match self.left_active {
            LeftPane::Endpoints => LeftPane::Sequences,
            LeftPane::Sequences => LeftPane::Endpoints,
        };
        self.set_focus(Pane::Explorer);
    }

    /// `cycle-region-fwd`/`cycle-region-back` (M7.10 stage B — shipped UNBOUND).
    /// Region-aware cycling: left column focused → cycle its sub-panes
    /// (Endpoints⇄Sequences); otherwise cycle open buffers/tabs. `forward` only
    /// matters for the buffer ring — the two-way toggle ignores it. Ctrl-Tab is
    /// deliberately NOT a default (terminal-unreliable); owner maps a portable key.
    fn cycle_region(&mut self, forward: bool) {
        if self.focus == Pane::Explorer {
            self.focus_sequences_toggle();
        } else {
            self.buffer_cycle(forward);
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

    /// Rebuilds the explorer tree from disk, preserving expansion + cursor as best
    /// it can (cursor clamps to the new row count), then re-derives each buffer's
    /// indices from its file path (see [`App::remap_buffers`]).
    fn reload_explorer(&mut self) -> Result<()> {
        self.explorer.reload(self.workspace.as_ref())?;
        self.remap_buffers();
        self.surface_explorer_warnings();
        // A reload can empty the sequence list (e.g. every sequence deleted on
        // disk); reconcile the left-column focus so it never strands on an empty
        // sub-pane.
        self.enforce_left_active_invariant();
        Ok(())
    }

    /// Re-derives each buffer's explorer indices from its file path after a tree
    /// reload. Collections are name-sorted, so creating, renaming, or deleting
    /// *another* collection shifts indices — a stale `endpoint.collection` would
    /// silently read the wrong collection's `folder.toml` vars at send time. A
    /// buffer whose file vanished is removed (the post-delete case); `active` is
    /// clamped so it still points at a live buffer.
    fn remap_buffers(&mut self) {
        let mut i = 0;
        while i < self.buffers.len() {
            let file = self.buffers[i].file().to_path_buf();
            if !file.exists() {
                self.buffers.remove(i);
                // A removal before/at `active` shifts it left (never below 0).
                if self.active > i || self.active >= self.buffers.len() {
                    self.active = self.active.saturating_sub(1);
                }
                continue;
            }
            if let Some(ci) = self.explorer.collection_index_for_file(&file)
                && let Some(b) = self.buffers[i].as_endpoint_mut()
            {
                b.endpoint.collection = ci;
            }
            i += 1;
        }
    }

    /// Moves the explorer cursor onto the endpoint at `file` (expanding its
    /// collection) and loads it into a buffer.
    fn select_endpoint_file(&mut self, file: &Path) -> Result<()> {
        if let Some(selected) = self.explorer.select_file(file)? {
            self.open_or_focus_buffer(selected);
        }
        Ok(())
    }

    /// The endpoint-switch seam: every path that opens/focuses an endpoint or
    /// switches workspace goes through here. Multi-buffer removed the
    /// cross-endpoint discard guard — opening endpoint Y no longer destroys X
    /// (each keeps its own buffer), so `Row`/`File` targets open-or-focus with NO
    /// confirm. Only a `Workspace` switch destroys every buffer, so a dirty
    /// workspace switch still defers behind a single
    /// [`ConfirmPurpose::DiscardChanges`] (discard-all) overlay, target parked in
    /// `pending_load`.
    fn guarded_load(&mut self, target: PendingLoad) -> Result<()> {
        // The guard fires when ANY buffer is dirty (not just the active one) — a
        // switch destroys every buffer, so a non-active dirty buffer must still
        // prompt (else its unsaved edits vanish silently).
        if matches!(target, PendingLoad::Workspace(_)) && self.any_buffer_dirty() {
            self.pending_load = Some(target);
            self.mode = Mode::Confirm(ConfirmPurpose::DiscardChanges);
            return Ok(());
        }
        self.perform_load(target)
    }

    /// Performs a (possibly previously deferred) endpoint load.
    fn perform_load(&mut self, target: PendingLoad) -> Result<()> {
        match target {
            PendingLoad::Row(row) => {
                self.explorer.cursor = row;
                if let Some(selected) = self.explorer.select()? {
                    self.open_or_focus_buffer(selected);
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

        // Abort any in-flight request from the old workspace's buffers (their
        // responses are no longer relevant); dropping the handle also drops the
        // stale generation.
        for buf in &mut self.buffers {
            if let Some(in_flight) = buf.as_endpoint_mut().and_then(|b| b.in_flight.take()) {
                in_flight.handle.abort();
            }
        }

        self.workspace = Some(new_ws);
        self.explorer.reload(self.workspace.as_ref())?;
        self.explorer.cursor = 0;

        // Drop every buffer — all endpoint/response/dirty/editor state lives
        // inside them, so a clear resets it all in one move (nothing from the old
        // workspace leaks in).
        self.buffers.clear();
        self.active = 0;
        // The active profile is defined per-workspace; a stale name could
        // accidentally resolve against the new workspace's profiles.
        self.active_profile = None;
        self.pending_load = None;
        self.zoom = None;
        // Clean slate for the sequences sub-pane: the new workspace's list is
        // unrelated to the old one. Reset to endpoints-zoomed — a fresh workspace
        // always lands on the endpoints tree regardless of its sequence count.
        self.left_active = LeftPane::Endpoints;
        self.focus_before_explorer = None;
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

// Child modules of `app`. Placing them as children (not siblings) keeps their
// items' access to `App`'s private fields and methods without any `pub(crate)`
// widening — see DECISIONS.md, "Module boundaries" (M7.11).
mod handlers;
mod pure;
mod render;

// `render::render` is the public draw entry point (`churl::tui::app::render`,
// used by the snapshot integration test); the pure/render helpers are pulled
// into this module's namespace so in-crate call sites — and `super::*` in
// `tests` — resolve them unqualified, exactly as before the split.
use pure::{
    auth_field_count, auth_field_text, default_export_path, export_target, merge_query_params,
    split_query, toggle_auth_placement, write_auth_field,
};
pub use render::render;
// Only the `tests` child module calls this render helper directly; re-exported
// here so its `use super::*` resolves it unqualified.
#[cfg(test)]
use render::leader_popup_entries;

#[cfg(test)]
mod tests;
