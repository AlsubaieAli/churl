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
    pub tx: mpsc::UnboundedSender<AppMsg>,
    rx: mpsc::UnboundedReceiver<AppMsg>,
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

    /// Handles one key while the `?` help overlay is open: `?`/Esc/`q` close;
    /// `j`/`k`/arrows scroll.
    fn handle_help_key(&mut self, key: KeyEvent) -> Result<()> {
        // While the `/` search input is open, every keystroke feeds the search.
        if self.help_search_input {
            self.handle_help_search_key(key);
            return Ok(());
        }
        match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                self.help_open = false;
                self.help_scroll = 0;
                self.help_search = None;
                self.help_search_input = false;
            }
            KeyCode::Char('/') => self.open_help_search(),
            KeyCode::Char('n') if self.help_search.is_some() => self.help_search_step(true),
            KeyCode::Char('N') if self.help_search.is_some() => self.help_search_step(false),
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

    /// `/` inside the help overlay: open the incremental help-search input
    /// (mirrors [`Self::open_body_search`]). Seeds an empty live search so
    /// highlighting engages immediately.
    fn open_help_search(&mut self) {
        self.help_search_editor = LineEditor::new("");
        let mut search = help::HelpSearch::default();
        search.set_query(String::new(), &self.keymap, &self.theme);
        self.help_search = Some(search);
        self.help_search_input = true;
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

    /// `n`/`N` inside the help overlay: step to the next/previous match
    /// (wrapping), scrolling it into view. Mirrors [`Self::response_search_step`].
    fn help_search_step(&mut self, forward: bool) {
        let stepped = self
            .help_search
            .as_mut()
            .and_then(|s| s.step(forward))
            .is_some();
        if stepped {
            self.help_center_on_match();
        }
    }

    /// Scrolls the help overlay so the current search match's line is visible,
    /// keeping it within the last-rendered viewport (jump-to-match).
    fn help_center_on_match(&mut self) {
        let Some(line) = self.help_search.as_ref().and_then(|s| s.current_line()) else {
            return;
        };
        let vh = self.help_viewport_height.max(1);
        // Bring the match into the viewport.
        if line < self.help_scroll {
            self.help_scroll = line;
        } else if line >= self.help_scroll + vh {
            self.help_scroll = line + 1 - vh;
        }
    }

    /// Handles one key in the URL vim-popup editor. Mode-aware:
    /// - `EditorMode::Search`: everything (incl. Enter/Esc) goes to edtui so
    ///   `/`-search executes (Enter jumps to match → Normal, Esc cancels). Never
    ///   commits from Search mode.
    /// - Otherwise Enter commits (single-logical-line constraint drops any Enter
    ///   edtui would turn into a newline). In Normal mode `vim_ext` motions run
    ///   before the Esc-cancel check, so Esc aborts a pending f/F/t/T find instead
    ///   of closing the popup; then Esc cancels; the rest falls through to edtui.
    ///
    /// Edge: Enter with a find pending still commits (the find drops with the popup).
    fn handle_url_popup_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(mode) = self
            .active_endpoint_buffer()
            .and_then(|b| b.url_popup.as_ref().map(|e| e.mode))
        else {
            return Ok(());
        };
        if mode == EditorMode::Search {
            if let Some(b) = self.active_endpoint_buffer_mut()
                && let Some(editor) = b.url_popup.as_mut()
            {
                b.url_popup_events.on_key_event(key, editor);
            }
            return Ok(());
        }
        // Enter commits (single logical line — no newline). Take the popup out
        // first so `commit_url` (which needs `&mut self`) does not overlap the
        // buffer borrow.
        if key.code == KeyCode::Enter {
            let taken = self
                .active_endpoint_buffer_mut()
                .and_then(|b| b.url_popup.take());
            if let Some(editor) = taken {
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
        if mode == EditorMode::Normal
            && let Some(b) = self.active_endpoint_buffer_mut()
            && let Some(editor) = b.url_popup.as_mut()
            && vim_ext::handle_key(key, editor, &mut b.url_popup_vim)
        {
            return Ok(());
        }
        // Esc in Normal mode cancels; in Insert mode edtui uses it to leave insert.
        if key.code == KeyCode::Esc && mode == EditorMode::Normal {
            if let Some(b) = self.active_endpoint_buffer_mut() {
                b.url_popup = None;
            }
            return Ok(());
        }
        if let Some(b) = self.active_endpoint_buffer_mut()
            && let Some(editor) = b.url_popup.as_mut()
        {
            b.url_popup_events.on_key_event(key, editor);
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

    /// Dedup-or-push: focus an already-open buffer for the same endpoint file
    /// (keeping its edits/response), else push a fresh buffer. Never replaces
    /// another buffer — each endpoint keeps its own edit/response/dirty state.
    fn open_or_focus_buffer(&mut self, selected: SelectedEndpoint) {
        if let Some(i) = self.buffer_index_for_path(&selected.file) {
            self.active = i;
            return;
        }
        self.buffers.push(Buffer::endpoint(selected));
        self.active = self.buffers.len() - 1;
    }

    /// Cycles to the next (`forward`) or previous buffer, wrapping. No-op when empty.
    fn buffer_cycle(&mut self, forward: bool) {
        let len = self.buffers.len();
        if len == 0 {
            return;
        }
        self.active = if forward {
            (self.active + 1) % len
        } else {
            (self.active + len - 1) % len
        };
    }

    /// Jumps directly to the `n`th open buffer/tab (1-based; `<leader>t <n>`,
    /// note #5). Reuses the same `self.active` focus mechanism `buffer_cycle`
    /// drives — no duplicated focus logic. Out of range (`n` > open count, incl.
    /// `n == 0`) is a graceful no-op with a brief status message; never a panic
    /// or a wrong-tab jump.
    fn focus_buffer_index(&mut self, n: usize) {
        let len = self.buffers.len();
        if n == 0 || n > len {
            self.notify(format!("no tab {n}"));
            return;
        }
        self.active = n - 1;
    }

    /// The active buffer index that should be selected after the buffer at
    /// `closed` is removed. Empty → `0`; a close before `active` shifts it left;
    /// closing the active buffer picks its right neighbour (clamped to the new
    /// last); a close after `active` leaves it unchanged. Assumes `closed` has
    /// already been removed, i.e. `self.buffers.len()` is the post-removal len.
    fn new_active_after_close(&self, closed: usize) -> usize {
        let len = self.buffers.len();
        if len == 0 {
            return 0;
        }
        if closed < self.active {
            self.active - 1
        } else if closed == self.active {
            closed.min(len - 1)
        } else {
            self.active
        }
    }

    /// Closes the buffer at `i`. A dirty buffer defers behind a
    /// [`ConfirmPurpose::DiscardChanges`] prompt (its path parked in
    /// `pending_close`); a clean buffer aborts any in-flight request (silently —
    /// no cancelled history row) and is removed immediately, with `active`
    /// clamped via [`App::new_active_after_close`]. No-op on an out-of-range `i`.
    fn close_buffer(&mut self, i: usize) {
        if i >= self.buffers.len() {
            return;
        }
        if self.buffers[i].is_dirty() {
            let path = self.buffers[i].file().to_path_buf();
            self.pending_close = Some(PendingClose::One(path));
            self.mode = Mode::Confirm(ConfirmPurpose::DiscardChanges);
            return;
        }
        self.remove_buffer_at(i);
    }

    /// Removes the buffer at `i` (already known clean or discarded), aborting its
    /// in-flight request first, and clamps `active`. Callers guarantee `i` is in
    /// range.
    fn remove_buffer_at(&mut self, i: usize) {
        if let Some(in_flight) = self.buffers[i]
            .as_endpoint_mut()
            .and_then(|b| b.in_flight.take())
        {
            in_flight.handle.abort();
        }
        self.buffers.remove(i);
        self.active = self.new_active_after_close(i);
    }

    /// Closes every buffer. Clean buffers close immediately; dirty buffers enter
    /// a one-at-a-time discard-confirm queue (keyed by path so removals don't
    /// shift the queue). When no dirty buffer remains the list is cleared.
    fn close_all_buffers(&mut self) {
        let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
        let mut i = 0;
        while i < self.buffers.len() {
            if self.buffers[i].is_dirty() {
                queue.push_back(self.buffers[i].file().to_path_buf());
                i += 1;
            } else {
                self.remove_buffer_at(i);
                // remove shifts everything after `i` left — revisit the same index.
            }
        }
        if queue.is_empty() {
            self.buffers.clear();
            self.active = 0;
            return;
        }
        self.pending_close = Some(PendingClose::All(queue));
        self.prompt_next_close_in_queue();
    }

    /// Advances the close-all queue: skips paths whose buffer is already gone,
    /// and opens the discard-confirm for the next still-dirty buffer. When the
    /// queue drains, clears any remaining (all-clean) buffers and returns to
    /// Normal.
    fn prompt_next_close_in_queue(&mut self) {
        loop {
            let front = match self.pending_close.as_ref() {
                Some(PendingClose::All(q)) => q.front().cloned(),
                _ => return,
            };
            let Some(front) = front else { break };
            match self.buffer_index_for_path(&front) {
                Some(idx) if self.buffers[idx].is_dirty() => {
                    self.mode = Mode::Confirm(ConfirmPurpose::DiscardChanges);
                    return;
                }
                // Front is gone or no longer dirty — drop it and continue.
                _ => {
                    if let Some(PendingClose::All(q)) = self.pending_close.as_mut() {
                        q.pop_front();
                    }
                }
            }
        }
        // Queue drained: clear whatever clean buffers remain.
        self.pending_close = None;
        self.buffers.clear();
        self.active = 0;
        self.mode = Mode::Normal;
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

    /// Whether the active endpoint (incl. the edtui body) differs from its
    /// pristine snapshot. Derived — no dirty flag to keep in sync. `false` when
    /// nothing is loaded.
    fn is_dirty(&self) -> bool {
        self.active_buffer().is_some_and(Buffer::is_dirty)
    }

    /// Whether ANY open buffer has unsaved changes (not just the active one).
    /// Drives the workspace-switch guard + its save-all resolution, so a
    /// non-active dirty buffer still prompts and is never silently destroyed.
    fn any_buffer_dirty(&self) -> bool {
        self.buffers.iter().any(Buffer::is_dirty)
    }

    /// Saves EVERY dirty buffer through the normal save path (each buffer becomes
    /// active in turn so `save_request` operates on it). Restores the original
    /// active index afterward. A refused save (e.g. literal-secret auth) leaves
    /// its buffer dirty and surfaces the error — callers gate the switch on
    /// [`App::any_buffer_dirty`] being clear afterward. No-op when nothing dirty.
    fn save_all_dirty_buffers(&mut self) {
        let restore = self.active;
        let mut i = 0;
        while i < self.buffers.len() {
            if self.buffers[i].is_dirty() {
                self.active = i;
                self.save_request();
            }
            i += 1;
        }
        // Clamp the restore target in case the buffer list shrank (it never does
        // here, but stay defensive).
        self.active = restore.min(self.buffers.len().saturating_sub(1));
    }

    /// Sends the selected endpoint's request with the live edtui body text.
    /// Spawns the execution task, keeps its `AbortHandle`, and moves the response
    /// pane to the in-flight state. Ignored (with a statusline hint) when a
    /// request is already in flight or no endpoint is selected.
    fn send_request(&mut self) {
        if self
            .active_endpoint_buffer()
            .is_some_and(|b| b.in_flight.is_some())
        {
            self.message = Some(Message::new("request already in flight — ctrl-c to cancel"));
            return;
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

        // Resolve `{{var}}` placeholders on the cloned request only — the seam is
        // `substitute_request`; resolved values are never written to disk (this
        // clone is discarded after the send). `execute()` stays substitution-free.
        self.build_resolver(&selected)
            .substitute_request(&mut request);

        // Fail loud: refuse to send a request that still carries `{{var}}`
        // placeholders no scope resolved — a literal `{{...}}` in the URL/headers/
        // body would otherwise ship and produce a cryptic transport error. Checked
        // before the client gate so the message surfaces regardless of runtime.
        let unresolved = churl_core::template::unresolved_placeholders(&request);
        if !unresolved.is_empty() {
            self.message = Some(Message::new(unresolved_vars_message(&unresolved)));
            return;
        }

        // No client = runtime-free construction (snapshot tests); do nothing.
        let Some(client) = self.client.clone() else {
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
    fn cancel_request(&mut self) {
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
    /// last render) for the *active response surface* (main pane or a focused
    /// runner Response — note #2); render clamps further. `0` when nothing is
    /// loaded.
    fn response_max_cursor(&self) -> usize {
        self.active_response_geometry().total_rows.saturating_sub(1)
    }

    /// Moves the response viewer *cursor* for a navigation action, on the active
    /// response surface. Scroll follows the cursor at render time (see
    /// `response::render`). `g`/`G` jump to the first/last visible display row.
    fn response_scroll(&mut self, action: Action) {
        let max = self.response_max_cursor();
        if !matches!(self.active_response(), ResponseState::Done { .. }) {
            return;
        }
        let g = self.active_response_geometry_mut();
        match action {
            Action::Up => g.cursor = g.cursor.saturating_sub(1),
            Action::Down => g.cursor = (g.cursor + 1).min(max),
            Action::Top => g.cursor = 0,
            Action::Bottom => g.cursor = max,
            _ => {}
        }
    }

    /// Moves the response cursor by half a viewport (scroll follows at render), on
    /// the active response surface.
    fn response_half_page(&mut self, down: bool) {
        let max = self.response_max_cursor();
        if !matches!(self.active_response(), ResponseState::Done { .. }) {
            return;
        }
        let g = self.active_response_geometry_mut();
        let half = (g.viewport_height / 2).max(1);
        g.cursor = if down {
            (g.cursor + half).min(max)
        } else {
            g.cursor.saturating_sub(half)
        };
    }

    // ---- Response viewer M7 actions ----

    /// The live `ResponseView`, when the pane holds a completed response. Reads
    /// the active buffer's response, or the orphan slot when nothing is loaded
    /// (isolation snapshots).
    fn response_view_mut(&mut self) -> Option<&mut ResponseView> {
        match self.active_response_mut() {
            ResponseState::Done { view } => Some(view),
            _ => None,
        }
    }

    /// Resets the active response surface's cursor/scroll (+ optionally the shared
    /// highlight guard/cache) after a view-geometry change. Works on the main pane
    /// or the focused runner Response (note #2). The highlight cache/guard is
    /// shared with the active endpoint buffer even for runner responses (they
    /// render through it), so it is cleared via [`Self::clear_active_highlight`].
    fn reset_response_geometry(&mut self, clear_cache: bool) {
        let g = self.active_response_geometry_mut();
        g.cursor = 0;
        g.scroll = 0;
        if clear_cache {
            self.clear_active_highlight();
        } else if let Some(b) = self.active_endpoint_buffer_mut() {
            b.pending_highlight = None;
        }
    }

    /// The logical line under the response cursor (through the last render's
    /// fold/wrap geometry), or `None` when there is no response — on the active
    /// response surface (note #2).
    fn response_cursor_logical(&self) -> Option<usize> {
        let g = self.active_response_geometry();
        let (width, cursor) = (g.viewport_width, g.cursor);
        match self.active_response() {
            ResponseState::Done { view } => view.logical_at_display_row(cursor, width),
            _ => None,
        }
    }

    /// `h`: toggle body/headers view. Resets cursor + scroll (the two views have
    /// different geometry) and clears any live search.
    fn response_toggle_headers(&mut self) {
        if let Some(view) = self.response_view_mut() {
            view.toggle_view_mode();
            self.reset_response_geometry(true);
        }
    }

    /// `W`: toggle soft-wrap. Cursor/scroll geometry changes, so reset them.
    fn response_toggle_wrap(&mut self) {
        if let Some(view) = self.response_view_mut() {
            view.toggle_wrap();
            self.reset_response_geometry(false);
        }
    }

    /// `H`/`L` (or Left/Right): pan the response horizontal window for unwrapped
    /// long lines (M7.7). A no-op while wrap is on (the view guards internally) —
    /// wrapped rows already fit the width, so there is nothing to pan. The pan
    /// amount is a fixed column step; render clamps the offset to the widest
    /// visible line and writes the clamped value back, so an over-pan self-corrects.
    fn response_scroll_h(&mut self, right: bool) {
        /// Columns panned per keypress.
        const H_STEP: usize = 8;
        if let Some(view) = self.response_view_mut() {
            view.scroll_h(right, H_STEP);
        }
    }

    /// `p`: toggle raw↔pretty body rendering (M7.7). Body text/line count change
    /// (and `toggle_pretty` resets folds), so reset cursor/scroll geometry and
    /// clear the highlight cache. Pretty is JSON-only in v1 — no-op with a notice
    /// outside a JSON body view, which would otherwise silently do nothing.
    fn response_toggle_pretty(&mut self) {
        let is_json_body = match self.active_response() {
            ResponseState::Done { view } => {
                view.view_mode() == ViewMode::Body
                    && view.syntax() == crate::tui::highlight::SyntaxToken::Json
            }
            _ => false,
        };
        if !is_json_body {
            self.notify("pretty: JSON body only");
            return;
        }
        if let Some(view) = self.response_view_mut() {
            view.toggle_pretty();
            self.reset_response_geometry(true);
        }
    }

    /// `s`: toggle A→Z sorting of pretty JSON object keys (M7.7). Only meaningful
    /// on a pretty JSON body — guard and notify otherwise (mirrors the
    /// pretty-outside-JSON notice). Text/line count change (and `toggle_sort_keys`
    /// resets folds), so reset cursor/scroll and clear the highlight cache.
    fn response_toggle_sort_keys(&mut self) {
        let is_pretty_json_body = match self.active_response() {
            ResponseState::Done { view } => {
                view.view_mode() == ViewMode::Body
                    && view.syntax() == crate::tui::highlight::SyntaxToken::Json
                    && view.pretty()
            }
            _ => false,
        };
        if !is_pretty_json_body {
            self.notify("sort: pretty JSON only");
            return;
        }
        if let Some(view) = self.response_view_mut() {
            view.toggle_sort_keys();
            self.reset_response_geometry(true);
        }
    }

    /// `#`: toggle the line-number gutter (default on; drive-test note #8). The
    /// gutter shrinks the effective body width, so wrap boundaries and the total
    /// display-row count can change — reset cursor/scroll geometry (as `W` does).
    /// The displayed text is untouched, so the highlight cache is kept (it is
    /// keyed on `line_numbers`, so the correctly-windowed lines are re-highlighted
    /// on demand); works in any view (body or headers).
    fn response_toggle_line_numbers(&mut self) {
        if let Some(view) = self.response_view_mut() {
            view.toggle_line_numbers();
            self.reset_response_geometry(false);
        }
    }

    /// Why folding is unsupported right now, or `None` when available. The headers
    /// view of a JSON response gets its own reason — "JSON responses only" would
    /// be wrong there.
    fn fold_unsupported_notice(&self) -> Option<&'static str> {
        let view = match self.active_response() {
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

    /// `o`: fold/unfold the innermost JSON region at the cursor.
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
        }
        self.clear_active_highlight();
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
            self.reset_response_geometry(true);
        }
    }

    /// `/`: open the incremental body-search input in the message-row position.
    fn open_body_search(&mut self) {
        if self.response_view_mut().is_none() {
            self.notify("no response to search");
            return;
        }
        self.body_search_editor = LineEditor::new("");
        // Seed an empty live search so highlighting/feedback engage immediately.
        if let Some(view) = self.response_view_mut() {
            view.set_search(String::new());
        }
        // Remember where to return: Normal for the main pane, or the live runner
        // mode when search was opened over a runner Response region (note #2).
        self.body_search_return = self.mode;
        self.mode = Mode::BodySearch;
    }

    /// Handles one key while the body-search input is open. Every keystroke
    /// recomputes matches and jumps to the first; Enter commits (keeps matches
    /// for `n`/`N`), Esc cancels (clears the search).
    fn handle_body_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = self.body_search_return;
                if let Some(view) = self.response_view_mut() {
                    view.clear_search();
                }
            }
            KeyCode::Enter => {
                self.mode = self.body_search_return;
                self.response_center_on_match();
            }
            _ => {
                self.body_search_editor.handle_key(key);
                let query = self.body_search_editor.text();
                if let Some(view) = self.response_view_mut() {
                    view.set_search(query);
                }
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
            self.clear_active_highlight();
            self.response_center_on_match();
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
    /// so scroll follows it into view at the next render. Also pans the horizontal
    /// window so an unwrapped match that lies past the right edge is brought into
    /// view (M7.7 horizontal search-into-view); inert while wrap is on.
    fn response_center_on_match(&mut self) {
        // Read the surface's width first (immutable), then take the view mutably —
        // the cursor lives in the geometry, the view in the response, both on the
        // same surface, so they cannot be borrowed together. Compute the target row
        // + pan the window through the view, then write the cursor back.
        let width = self.active_response_geometry().viewport_width;
        let Some(view) = self.response_view_mut() else {
            return;
        };
        let row = view
            .current_match_line()
            .and_then(|logical| view.display_row_for_logical(logical, width));
        if let Some((start, end)) = view.current_match_columns() {
            view.ensure_column_visible(start, end, width);
        }
        if let Some(row) = row {
            self.active_response_geometry_mut().cursor = row;
        }
    }

    /// `y`: copy the current response's full text via OSC 52 (capped).
    ///
    /// On a `Done` row this copies the byte-exact body (unchanged). On a `Failed`
    /// row (drive-test #4a) it copies an honest error blurb — the error message
    /// plus the request method+URL when known — via the same clipboard path, so
    /// `y` is never a silent no-op on a transport failure. This branch lives in
    /// the single shared copy handler used by the main pane, load runner, and
    /// sequence runner (note #2), so all three unified viewers get it at once.
    fn response_copy_view(&mut self) {
        if let Some(view) = self.response_view_mut() {
            let full = view.copy_all().to_owned();
            let truncated = view.truncated();
            self.copy_to_clipboard_view(&full, truncated);
        } else if let Some(text) = self.active_response().failure_copy_text() {
            self.enqueue_clipboard(&text, "copied error");
        } else {
            // No view and no failure blurb (Dropped / Idle / …). Never a silent
            // no-op (the exact class of bug #4a addressed) — say why (note #1).
            self.message = Some(Message::new(nothing_to_copy_message(
                self.active_response(),
            )));
        }
    }

    /// `Y`: copy the response cursor's logical line via the layered clipboard.
    fn response_copy_line(&mut self) {
        // A missing view (Failed / Dropped / Idle) has no line to copy. Give
        // feedback rather than silently doing nothing (drive-test #4a fold-in).
        if self.response_view_mut().is_none() {
            self.message = Some(Message::new(nothing_to_copy_message(
                self.active_response(),
            )));
            return;
        }
        let Some(logical) = self.response_cursor_logical() else {
            return;
        };
        let Some(view) = self.response_view_mut() else {
            return;
        };
        let line = view.copy_line(logical);
        self.enqueue_clipboard(&line, "copied line");
    }

    /// Copies the full-view text, reporting its size (with a `(truncated)` note
    /// when the body hit the size cap, and a `copied first X of Y` note when the
    /// 1 MB copy cap kicked in). The message is the *success* message; the run
    /// loop swaps it for "copy failed" if no clipboard path works.
    fn copy_to_clipboard_view(&mut self, text: &str, body_truncated: bool) {
        let full_len = text.len();
        let (payload, queued) = clipboard::cap_payload(text);
        let capped = full_len > queued;
        let mut msg = if capped {
            format!(
                "copied first {} of {}",
                response::fmt_bytes(queued),
                response::fmt_bytes(full_len)
            )
        } else {
            format!("copied {}", response::fmt_bytes(queued))
        };
        // The body-truncation note stacks with the cap note — both facts matter.
        if body_truncated {
            msg.push_str(" (truncated)");
        }
        self.pending_clipboard = Some(PendingCopy {
            payload,
            success_msg: msg,
        });
    }

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
            AppMsg::LoadStarted {
                run_generation,
                index,
            } => self.on_load_started(run_generation, index),
            AppMsg::LoadResult {
                run_generation,
                index,
                outcome,
            } => self.on_load_result(run_generation, index, outcome),
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
    fn on_response(
        &mut self,
        generation: u64,
        outcome: Result<Response, String>,
        meta: ResponseMeta,
    ) {
        let Some(idx) = self.buffers.iter().position(|b| {
            b.as_endpoint()
                .and_then(|e| e.in_flight.as_ref())
                .is_some_and(|f| f.generation == generation)
        }) else {
            return; // stale — no buffer awaits this generation
        };
        // History write needs `&mut self`; compute status args before borrowing
        // the target buffer to store the response.
        let status = outcome.as_ref().ok().map(|r| (r.status, r.timing.total));
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
    }

    /// Stores highlighted viewport lines in the active buffer's cache, capping it
    /// so long scrolls do not grow it unbounded. (Only the active buffer enqueues
    /// highlight jobs, and the cache is keyed by viewport hash, so a job that
    /// lands after a buffer switch is harmless.)
    fn cache_highlighted(&mut self, hash: u64, lines: Vec<Line<'static>>) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        if b.highlight_cache.len() >= 64 {
            b.highlight_cache.clear();
        }
        // Clear the in-flight guard when its result lands (M7 dedup micro-nit).
        if b.pending_highlight == Some(hash) {
            b.pending_highlight = None;
        }
        b.highlight_cache.insert(hash, lines);
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

    /// Opens the environments & variables editor over the current workspace.
    /// Requires an open workspace (there is nothing to edit otherwise).
    fn open_env_editor(&mut self) {
        let session = self.session_vars();
        let Some(ws) = self.workspace.as_ref() else {
            self.notify("open a workspace first");
            return;
        };
        match EnvEditorState::from_workspace(
            ws,
            self.active_profile.clone(),
            self.cli_vars.clone(),
            &session,
        ) {
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
            EnvKeyOutcome::ClearSession => {
                // Empty the current workspace's in-memory Session store, then
                // rebuild the editor's read-only Session group so it reflects it.
                let cleared = self.clear_session_vars();
                let session = self.session_vars();
                if let Some(editor) = self.env_editor.as_mut() {
                    editor.set_session_vars(&session);
                }
                self.notify(if cleared {
                    "session captures cleared"
                } else {
                    "no session captures to clear"
                });
            }
            EnvKeyOutcome::RevealRow => {
                // Ephemeral peek (drive-test note #3): resolve the selected masked
                // row's value through the SAME resolver a standalone send uses, then
                // hand the plaintext to the editor's transient reveal state. The
                // resolved value never touches disk, a log, or any persisted field —
                // it lives only in the editor's in-memory `reveal` until re-masked.
                let raw = self
                    .env_editor
                    .as_ref()
                    .and_then(|e| e.peekable_selected_value())
                    .map(str::to_owned);
                if let Some(raw) = raw {
                    let resolved = self.build_env_resolver().substitute(&raw);
                    if let Some(editor) = self.env_editor.as_mut() {
                        editor.set_reveal(resolved);
                    }
                }
            }
            EnvKeyOutcome::CopyRevealed => {
                // Copy the revealed plaintext through the existing clipboard path
                // (the "allow copy" the owner asked for). We read it back from the
                // editor's reveal state (still live) and route it through
                // `enqueue_clipboard` — the same seam every other copy uses.
                let revealed = self
                    .env_editor
                    .as_ref()
                    .and_then(|e| e.revealed_value())
                    .map(str::to_owned);
                if let Some(value) = revealed {
                    self.enqueue_clipboard(&value, "copied revealed value");
                }
            }
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

    /// Enter (or a jump label) on the current explorer row: loads the endpoint
    /// through the guarded seam. One seam so both the explorer `Enter` and
    /// jump-mode agree. (Sequences are no longer tree rows — they activate via
    /// the sub-pane, PR 2b.)
    fn activate_explorer_row(&mut self, row: usize) -> Result<()> {
        self.explorer.cursor = row;
        self.guarded_load(PendingLoad::Row(row))
    }

    /// Runs the sequence under the sub-pane cursor (`<leader>s r` / palette / `r`
    /// on the sequences sub-pane). Notifies when no sequence is selected.
    fn run_selected_sequence(&mut self) {
        let Some(selected) = self.explorer.selected_sequence() else {
            self.notify("select a sequence first");
            return;
        };
        self.open_sequence_runner(selected);
    }

    /// Opens a fuzzy picker over every sequence name. `run == false` (`<leader>s o`
    /// / palette) opens the chosen sequence in the Edit face; `run == true`
    /// (`<leader>s r`) loads + runs it instead (D1 — mirrors the
    /// `load_runner_after_pick` one-shot-intent pattern).
    fn open_sequence_picker(&mut self, run: bool) {
        if self.workspace.is_none() {
            self.notify("open a workspace first");
            return;
        }
        let sequences = self.explorer.all_sequences();
        if sequences.is_empty() {
            self.notify("no sequences in this workspace");
            return;
        }
        let mut items = Vec::with_capacity(sequences.len());
        let mut choices = Vec::with_capacity(sequences.len());
        for (name, file) in sequences {
            items.push(name);
            choices.push(file);
        }
        self.sequence_choices = choices;
        self.sequence_pick_runs = run;
        let title = if run {
            " Run sequence "
        } else {
            " Open sequence "
        };
        self.picker = Some(picker::PickerState::new(title, items));
        self.mode = Mode::SequencePicker;
    }

    /// Loads the sequence at `path` and opens the RUNNER over it (D1 `<leader>s r`
    /// chooser accept path). Mirrors `open_picked_sequence` but hands to the
    /// runner instead of the editor.
    fn run_sequence_at(&mut self, path: PathBuf) {
        match persistence::load_sequence(&path) {
            Ok(sequence) => {
                self.explorer.select_sequence_file(&path);
                self.open_sequence_runner(super::components::explorer::SelectedSequence {
                    name: sequence.name.clone(),
                    file: path,
                    sequence,
                });
            }
            Err(err) => self.crud_error(err),
        }
    }

    /// Loads the sequence at `path` and opens the unified surface (Edit face).
    /// Also moves the sub-pane cursor onto the picked sequence so a subsequent
    /// `<leader>s r` runs *this* sequence, not sequence #0.
    fn open_picked_sequence(&mut self, path: PathBuf) -> Result<()> {
        match persistence::load_sequence(&path) {
            Ok(sequence) => {
                self.explorer.select_sequence_file(&path);
                self.open_sequence_editor(sequence.name.clone(), path, &sequence);
            }
            Err(err) => self.crud_error(err),
        }
        Ok(())
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
        self.mode = Mode::Sequence;
        self.sequence_view = SeqView::Run;
        self.start_sequence_run();
    }

    /// The ambient run scopes (cli / active profile / workspace) for a sequence
    /// run, mirroring the send-time resolver's non-collection layers. The
    /// per-step collection scope is loaded inside `prepare_step`.
    fn sequence_run_scopes(&self) -> churl_core::sequence::RunScopes {
        churl_core::sequence::RunScopes {
            session: self.session_vars(),
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
                runner.geometry.cursor = 0;
                runner.geometry.scroll = 0;
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

        // Merge extracted values into the run-only accumulator (empty on any
        // failure). Also collect the Session-target captures (note #6): a rule
        // whose name is in this step's `persist` and that actually extracted a
        // value. `extracted` is empty on a failed extraction, so a failure never
        // writes — leaving any prior Session value intact.
        let mut session_writes: Vec<(String, String)> = Vec::new();
        for (name, value) in &extracted {
            runner.extracted.insert(name.clone(), value.clone());
            if step.persist.iter().any(|p| p == name) {
                session_writes.push((name.clone(), value.clone()));
            }
        }
        if let Some(row) = runner.steps.get_mut(index) {
            row.timing = timing;
            row.extracted = extracted;
            row.response = response;
        }
        // Write the Session captures into the current workspace's in-memory store
        // (create/overwrite — a re-login refreshes the token). Done after the
        // `runner` borrow is released. Never touches disk.
        for (name, value) in session_writes {
            self.write_session_var(name, value);
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

    /// Routes a runner Response-region key through the SAME dispatch + `response_*`
    /// handlers the main pane uses (note #2 — one code path, so the runner viewer
    /// can never drift). Returns `true` when the key resolved to a response action
    /// and was consumed; `false` when the caller must delegate to the runner for
    /// its own keys.
    ///
    /// Only fires when a runner Response region is the active response surface (so
    /// Config/Results/Steps focus is untouched, and the runner's own guard/edit
    /// sub-states — via `active_response_surface`'s `response_input_captured` gate —
    /// keep their keys). The key is looked up through the shared `[keys.response]`
    /// overlay (so remapping a Response key updates the runners too), then matched
    /// against the response action set: the overlay's parity actions PLUS the
    /// viewer cursor-nav actions (routed through the same `response_scroll` /
    /// `response_half_page` the main pane uses). Runner-owned keys (Tab/BackTab,
    /// Ctrl-R, Ctrl-C, q/Esc, and everything else) are not in the set, so they fall
    /// through to the runner unchanged.
    fn try_route_runner_response_key(&mut self, key: KeyEvent) -> bool {
        if matches!(self.active_response_surface(), ResponseSurface::Main) {
            return false;
        }
        let Some(action) = self.keymap.lookup_ctx(key, PaneCtx::Response) else {
            return false;
        };
        match action {
            // Viewer cursor nav — the SAME movement path the main Response pane
            // uses, operating on the mode-aware geometry (note #2).
            Action::Up | Action::Down | Action::Top | Action::Bottom => {
                self.response_scroll(action)
            }
            Action::HalfPageDown => self.response_half_page(true),
            Action::HalfPageUp => self.response_half_page(false),
            // Response-overlay parity actions — identical handlers to the main pane.
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
            // Not a response action — the runner keeps it (nav-law, run/cancel/close).
            _ => return false,
        }
        true
    }

    /// Routes a key to the unified sequence surface. `Ctrl-R` flips the Edit⇄Run
    /// face (a switcher, not a nav-law key, free in both components); otherwise
    /// the key goes to the active face's handler. Per-face `Esc`/`q` semantics
    /// stay isolated — the Run-face confirm-on-close-while-running never fires in
    /// the Edit face because Edit keys never reach the runner.
    fn handle_sequence_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_sequence_view();
            return Ok(());
        }
        match self.sequence_view {
            SeqView::Edit => {
                let Some(editor) = self.sequence_editor.as_mut() else {
                    self.close_sequence_surface();
                    return Ok(());
                };
                match editor.handle_key(key) {
                    EditorOutcome::Consumed => {}
                    EditorOutcome::Save => {
                        self.save_sequence_editor()?;
                    }
                    EditorOutcome::SaveAndClose => {
                        if self.save_sequence_editor()? {
                            self.close_sequence_surface();
                        }
                    }
                    EditorOutcome::Close => self.close_sequence_surface(),
                }
            }
            SeqView::Run => {
                if self.sequence_runner.is_none() {
                    self.close_sequence_surface();
                    return Ok(());
                }
                // Response-region keys route through the shared response path FIRST
                // (note #2); anything not a response action delegates to the runner.
                if self.try_route_runner_response_key(key) {
                    return Ok(());
                }
                let runner = self.sequence_runner.as_mut().expect("checked above");
                match runner.handle_key(key) {
                    RunnerOutcome::Consumed => {}
                    RunnerOutcome::Rerun => self.start_sequence_run(),
                    RunnerOutcome::Cancel => self.cancel_sequence_run(),
                    RunnerOutcome::Close => self.close_sequence_surface(),
                }
            }
        }
        Ok(())
    }

    /// Flips the sequence surface's face. Run→Edit is always safe. Edit→Run is
    /// gated on the saved sequence being the single source of truth: a DIRTY
    /// editor blocks with a notify (no auto-save, no stale run); a clean editor
    /// (re)builds the runner from the saved steps before switching. The run
    /// itself is never auto-started here — the user presses `r` in the Run face.
    fn toggle_sequence_view(&mut self) {
        match self.sequence_view {
            SeqView::Run => self.sequence_view = SeqView::Edit,
            SeqView::Edit => {
                let Some(editor) = self.sequence_editor.as_ref() else {
                    return;
                };
                if editor.is_dirty() {
                    self.notify("save (w) before running");
                    return;
                }
                let sequence = match editor.to_sequence_checked() {
                    Ok(sequence) => sequence,
                    Err(msg) => {
                        self.notify(msg);
                        return;
                    }
                };
                let name = editor.name().to_owned();
                let file = editor.path().to_owned();
                let steps = churl_core::sequence::ordered_steps(&sequence)
                    .into_iter()
                    .cloned()
                    .collect();
                // A prior run's in-flight step survives a Run→Edit flip (the abort
                // handle is kept alive). Rebuilding the runner here without aborting
                // it would ORPHAN that async step — a real POST/DELETE running to
                // completion in the background with no UI. Abort + bump the old
                // generation first (mirrors start_sequence_run/close), so a landed
                // straggler is also dropped by the generation guard.
                if let Some(handle) = self.sequence_abort.take() {
                    handle.abort();
                }
                if let Some(runner) = self.sequence_runner.as_mut() {
                    runner.run_generation += 1;
                }
                self.sequence_runner = Some(SequenceRunnerState::new(
                    name,
                    file,
                    sequence.on_error,
                    steps,
                ));
                self.sequence_view = SeqView::Run;
            }
        }
    }

    /// Closes the unified sequence surface: aborts any in-flight step, drops both
    /// component states, and returns to Normal.
    fn close_sequence_surface(&mut self) {
        if let Some(handle) = self.sequence_abort.take() {
            handle.abort();
        }
        // Bump the generation so a straggler result is dropped after close.
        if let Some(runner) = self.sequence_runner.as_mut() {
            runner.run_generation += 1;
        }
        self.sequence_runner = None;
        self.sequence_editor = None;
        self.sequence_view = SeqView::Edit;
        self.mode = Mode::Normal;
    }

    // ---- M7.5 concurrent-load runner ----

    /// `<leader>l` / palette: open the load runner for the selected endpoint.
    /// Builds the request EXACTLY as an interactive send would (clone the endpoint
    /// request, fold in the live body editor, resolve `{{var}}`s ONCE) so the
    /// batch hits the same URL/vars/auth as a normal send, and prefills the config
    /// from the load defaults. Never auto-runs — the user reviews/edits first.
    fn open_load_runner(&mut self) {
        // Fall back to the hovered endpoint when nothing is loaded (M7.10 stage B);
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
        self.load_runner = Some(LoadRunnerState::new(
            selected.endpoint.name.clone(),
            url,
            endpoint_path,
            LoadConfig::default(),
        ));
        self.mode = Mode::LoadRunner;
    }

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

    /// Starts (or restarts) the batch: aborts any prior launcher, resets rows,
    /// bumps the run generation, and spawns ONE launcher task that owns a
    /// `buffer_unordered` fan-out (bounded to `concurrency`, paced by `interval`).
    /// Aborting that single task drops the fan-out and cancels ALL in-flight
    /// requests — there is no detached per-request task to escape cancellation.
    fn start_load_run(&mut self) {
        // Interrupt any in-progress batch first — recording its partial summary
        // (a re-run mid-batch must not silently lose the current run).
        self.interrupt_running_batch();
        let (Some(runner), Some(request)) = (self.load_runner.as_mut(), self.load_request.clone())
        else {
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
                        let _ = tx.send(AppMsg::LoadStarted {
                            run_generation,
                            index,
                        });
                        let outcome = churl_core::http::execute(&client, &request, &options)
                            .await
                            .map_err(|err| err.to_string());
                        let _ = tx.send(AppMsg::LoadResult {
                            run_generation,
                            index,
                            outcome,
                        });
                    }
                })
                .buffer_unordered(concurrency)
                .for_each(|()| async {})
                .await;
        });
        self.load_abort = Some(handle.abort_handle());
    }

    /// Marks copy `index` as in flight when the launcher signals it started.
    fn on_load_started(&mut self, run_generation: u64, index: usize) {
        let Some(runner) = self.load_runner.as_mut() else {
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

    /// Lands a completed copy: drops stale results, classifies (mirroring the core
    /// `classify` seam), records it + recomputes stats, and — when the last copy
    /// lands — finishes the run and writes the batch summary.
    fn on_load_result(
        &mut self,
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
    ) {
        let Some(runner) = self.load_runner.as_mut() else {
            return;
        };
        if run_generation != runner.run_generation {
            return; // stale — a cancel or re-run superseded this batch
        }
        let view_gen = runner.next_view_gen();
        let url = runner.url.clone();
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
        let done = runner.record_result(index, status, timing, response, req_outcome);
        if done {
            runner.running = false;
            runner.finished = true;
            self.load_abort = None;
            self.write_load_summary(false);
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

    /// Cancels the in-flight batch (Ctrl-C): records the partial summary + aborts
    /// the launcher + bumps the generation via the shared interrupt seam, then
    /// marks non-terminal rows cancelled and settles the runner's finished state.
    fn cancel_load_run(&mut self) {
        self.interrupt_running_batch();
        let Some(runner) = self.load_runner.as_mut() else {
            return;
        };
        for row in &mut runner.results {
            if matches!(row.status, LoadStatus::Pending | LoadStatus::Running) {
                // D1: a launched-then-cancelled row carries a real time-to-cancel.
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
        self.notify("load run cancelled");
    }

    /// Persists the run's one-row summary to the SEPARATE `load_batches` table
    /// (never per-endpoint history). Best-effort; a write failure warns.
    fn write_load_summary(&mut self, cancelled: bool) {
        let Some(runner) = self.load_runner.as_ref() else {
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
        if let Some(Err(err)) = self
            .history
            .as_ref()
            .map(|store| store.insert_load_batch(&summary))
        {
            self.notify(format!("load history write failed: {err}"));
        }
    }

    /// Routes a key to the open load runner and acts on its outcome.
    fn handle_load_runner_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.load_runner.is_none() {
            self.mode = Mode::Normal;
            return Ok(());
        }
        // Response-region keys route through the shared response path FIRST (note
        // #2); anything not a response action delegates to the runner below.
        if self.try_route_runner_response_key(key) {
            return Ok(());
        }
        let runner = self.load_runner.as_mut().expect("checked above");
        match runner.handle_key(key) {
            LoadOutcome::Consumed => {}
            LoadOutcome::Run => self.request_load_run(),
            LoadOutcome::ConfirmedRun => self.start_load_run(),
            LoadOutcome::Cancel => self.cancel_load_run(),
            LoadOutcome::Close => self.close_load_runner(),
        }
        Ok(())
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
                self.new_sequence_prompt();
                Ok(())
            }
        }
    }

    /// Opens the "new sequence" name prompt (the `n`-on-the-sequences-sub-pane
    /// entry point, parallel to endpoints `n`=new endpoint). Guards on an open
    /// workspace like every other create path.
    fn new_sequence_prompt(&mut self) {
        if self.workspace.is_none() {
            self.notify("open a workspace first");
            return;
        }
        self.open_prompt(PromptPurpose::NewSequence, "");
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
        // A fresh open starts with no run built; the run face is entered lazily.
        self.sequence_runner = None;
        self.mode = Mode::Sequence;
        self.sequence_view = SeqView::Edit;
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
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.url_editor = Some(LineEditor::new(&url));
        }
    }

    /// Opens the centered vim-popup URL editor (`e`, or `url_edit = "popup"`).
    fn begin_url_popup(&mut self) {
        let Some(url) = self.live_request().map(|r| r.url.clone()) else {
            self.notify("no endpoint selected");
            return;
        };
        self.set_focus(Pane::UrlBar);
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.url_editor = None;
            b.url_popup = Some(EditorState::new(Lines::from(url.as_str())));
            b.url_popup_vim.reset();
        }
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

    /// Commits a new URL (from the inline editor or the popup): strips the query
    /// string, merges it into the Params tab (deliverable 3), and sets the base
    /// URL. Reports the merge and marks dirty. A no-op when nothing is loaded.
    fn commit_url(&mut self, url: String) {
        let (base, pairs) = split_query(&url);
        let Some(selected) = self.selected_mut() else {
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
                // Take the editor out first so `commit_url` can borrow `&mut self`.
                let taken = self
                    .active_endpoint_buffer_mut()
                    .and_then(|b| b.url_editor.take());
                if let Some(editor) = taken {
                    self.commit_url(editor.text());
                }
            }
            KeyCode::Esc => {
                if let Some(b) = self.active_endpoint_buffer_mut() {
                    b.url_editor = None; // revert (discard the editor's text)
                }
            }
            _ => {
                if let Some(b) = self.active_endpoint_buffer_mut()
                    && let Some(editor) = b.url_editor.as_mut()
                {
                    editor.handle_key(key);
                }
            }
        }
        Ok(())
    }

    /// Cycles the loaded request's method (GET→POST→…→GET).
    fn cycle_method(&mut self) {
        if let Some(selected) = self.selected_mut() {
            let m = selected.endpoint.request.method;
            selected.endpoint.request.method = m.cycle();
        } else {
            self.message = Some(Message::new("no endpoint selected"));
        }
    }

    /// Opens the one-key method-picker menu (focuses the URL bar).
    fn open_method_menu(&mut self) {
        if self.selected().is_none() {
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
            && let Some(selected) = self.selected_mut()
        {
            selected.endpoint.request.method = method;
            self.mode = Mode::Normal;
        }
    }

    // ---- Request-tab rows ----

    /// The number of rows on the active tab of the live request.
    fn active_tab_row_count(&self) -> usize {
        let Some(b) = self.active_endpoint_buffer() else {
            return 0;
        };
        let request = b.live_request();
        match b.tabs.active {
            RequestTab::Params => request.params.len(),
            RequestTab::Headers => request.headers.len(),
            RequestTab::Auth => auth_field_count(request.auth.as_ref()),
            RequestTab::Body => 0,
        }
    }

    /// `a`: add a row on the Params/Headers tab and immediately edit its name.
    fn row_add(&mut self) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let request = &mut b.endpoint.endpoint.request;
        let (new_row, row_count) = match b.tabs.active {
            RequestTab::Params => {
                request.params.push(Param {
                    name: String::new(),
                    value: String::new(),
                    enabled: true,
                });
                (request.params.len() - 1, request.params.len())
            }
            RequestTab::Headers => {
                request.headers.push(Header {
                    name: String::new(),
                    value: String::new(),
                    enabled: true,
                });
                (request.headers.len() - 1, request.headers.len())
            }
            // Auth/Body have no add-row.
            _ => return,
        };
        b.tabs.clamp(row_count);
        // Select and edit the new row's name field.
        match b.tabs.active {
            RequestTab::Params => b.tabs.params_sel = new_row,
            RequestTab::Headers => b.tabs.headers_sel = new_row,
            _ => {}
        }
        b.tabs.editing = Some(FieldEdit {
            row: new_row,
            field: EditField::Name,
            editor: LineEditor::new(""),
        });
    }

    /// `d`: delete the selected row on the Params/Headers tab (no confirm).
    fn row_delete(&mut self) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let sel = b.tabs.selection();
        let request = &mut b.endpoint.endpoint.request;
        let row_count = match b.tabs.active {
            RequestTab::Params if sel < request.params.len() => {
                request.params.remove(sel);
                request.params.len()
            }
            RequestTab::Headers if sel < request.headers.len() => {
                request.headers.remove(sel);
                request.headers.len()
            }
            _ => return,
        };
        b.tabs.clamp(row_count);
    }

    /// `Space`: toggle the selected row's `enabled` flag (Params/Headers), or the
    /// ApiKey placement on the Auth tab's placement row.
    fn row_toggle(&mut self) {
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let sel = b.tabs.selection();
        let active = b.tabs.active;
        let request = &mut b.endpoint.endpoint.request;
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
        let Some((sel, active)) = self
            .active_endpoint_buffer()
            .map(|b| (b.tabs.selection(), b.tabs.active))
        else {
            return;
        };
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
        // Auth fields have fixed labels — edit the value directly (no name→value
        // advance). Param/Header rows start on the name field.
        let start_field = if active == RequestTab::Auth {
            EditField::Value
        } else {
            EditField::Name
        };
        let Some(text) = self.current_field_text(active, sel, start_field) else {
            return;
        };
        if let Some(b) = self.active_endpoint_buffer_mut() {
            b.tabs.editing = Some(FieldEdit {
                row: sel,
                field: start_field,
                editor: LineEditor::new(&text),
            });
        }
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
                // Cancel; a never-committed freshly-added row (name+value both
                // empty) is removed, else `a`+Esc leaves a nameless ghost row
                // that serializes.
                let edit = self
                    .active_endpoint_buffer_mut()
                    .and_then(|b| b.tabs.editing.take());
                if let Some(edit) = edit {
                    self.discard_row_if_empty(edit.row);
                }
            }
            KeyCode::Tab => self.field_edit_advance(false),
            KeyCode::Enter => self.field_edit_advance(true),
            _ => {
                if let Some(b) = self.active_endpoint_buffer_mut()
                    && let Some(edit) = b.tabs.editing.as_mut()
                {
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
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let active = b.tabs.active;
        let request = &mut b.endpoint.endpoint.request;
        let removed = match active {
            RequestTab::Params
                if request
                    .params
                    .get(row)
                    .is_some_and(|p| p.name.is_empty() && p.value.is_empty()) =>
            {
                request.params.remove(row);
                Some(request.params.len())
            }
            RequestTab::Headers
                if request
                    .headers
                    .get(row)
                    .is_some_and(|h| h.name.is_empty() && h.value.is_empty()) =>
            {
                request.headers.remove(row);
                Some(request.headers.len())
            }
            _ => None,
        };
        if let Some(n) = removed {
            b.tabs.clamp(n);
        }
    }

    /// Commits the current field edit into the live request. `commit_row` closes
    /// the edit after the value field; otherwise name→value advances.
    fn field_edit_advance(&mut self, commit_row: bool) {
        let Some(edit) = self
            .active_endpoint_buffer_mut()
            .and_then(|b| b.tabs.editing.take())
        else {
            return;
        };
        let active = self.active_tab();
        let text = edit.editor.text();
        self.write_field(active, edit.row, edit.field, text);
        match edit.field {
            EditField::Name => {
                // Advance to the value field, seeded with its text.
                let value = self
                    .current_field_text(active, edit.row, EditField::Value)
                    .unwrap_or_default();
                if let Some(b) = self.active_endpoint_buffer_mut() {
                    b.tabs.editing = Some(FieldEdit {
                        row: edit.row,
                        field: EditField::Value,
                        editor: LineEditor::new(&value),
                    });
                }
            }
            EditField::Value => {
                if !commit_row {
                    // Tab from value wraps back to name.
                    let name = self
                        .current_field_text(active, edit.row, EditField::Name)
                        .unwrap_or_default();
                    if let Some(b) = self.active_endpoint_buffer_mut() {
                        b.tabs.editing = Some(FieldEdit {
                            row: edit.row,
                            field: EditField::Name,
                            editor: LineEditor::new(&name),
                        });
                    }
                }
                // Enter on value: editing already taken → committed.
            }
        }
    }

    /// Writes an edited field back into the live request.
    fn write_field(&mut self, tab: RequestTab, row: usize, field: EditField, text: String) {
        let Some(selected) = self.selected_mut() else {
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
        if self.selected().is_none() {
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
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let auth = &mut b.endpoint.endpoint.request.auth;
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
        b.tabs.auth_sel = 0;
    }

    // ---- Save ----

    /// `w`: save the live request to disk (format-preserving). Refreshes the
    /// snapshot on success; a secrets refusal surfaces on the statusline and the
    /// request stays dirty.
    fn save_request(&mut self) {
        self.sync_body_into_selected();
        let Some(selected) = self.selected() else {
            self.message = Some(Message::new("no endpoint to save"));
            return;
        };
        let path = selected.file.clone();
        let endpoint = selected.endpoint.clone();
        match persistence::save_endpoint(&path, &endpoint) {
            Ok(()) => {
                if let Some(b) = self.active_endpoint_buffer_mut() {
                    b.loaded_snapshot = endpoint.clone();
                }
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
            None => {
                self.message = Some(Message::new("nothing selected to delete"));
            }
        }
    }

    /// `d` on the sequences sub-pane: y/n confirm before deleting the hovered
    /// sequence file (parallels [`begin_delete`]'s endpoint arm — same low-friction
    /// y/n gate, since a sequence file carries no secret values). Notifies when no
    /// sequence is selected.
    fn begin_delete_sequence(&mut self) {
        if self.explorer.selected_sequence().is_none() {
            self.notify("select a sequence first");
            return;
        }
        self.mode = Mode::Confirm(ConfirmPurpose::DeleteSequence);
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
        // One-shot read: fall back to the hovered endpoint when nothing is loaded
        // (M7.10 stage B), so copy acts on what the cursor points at.
        let Some(selected) = self.selected().cloned().or_else(|| self.hovered_endpoint()) else {
            self.notify("no endpoint selected");
            return;
        };
        let mut endpoint = selected.endpoint.clone();
        // Fold in unsaved body edits so the copy matches what's shown.
        let body_text = self
            .active_endpoint_buffer()
            .map(|b| String::from(b.editor.lines.clone()))
            .unwrap_or_default();
        overwrite_body_text(&mut endpoint.request, body_text);
        if resolved {
            self.build_resolver(&selected)
                .substitute_request(&mut endpoint.request);
        }
        let curl = churl_core::export::export_curl(&endpoint);
        let success_msg = if resolved {
            "copied curl (vars resolved — may contain secrets)"
        } else {
            "copied curl"
        };
        self.enqueue_clipboard(&curl, success_msg);
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
        // Overwrite the default endpoint with the import, keeping the
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
        // Open the new endpoint as its own buffer (File target — no confirm).
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
                        // File target — no cross-endpoint confirm in the
                        // multi-buffer model.
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
                        let renamed_idx = self.buffer_index_for_path(&path);
                        if let Some(idx) = renamed_idx {
                            // Renaming the *loaded* endpoint: update file path +
                            // name in place so unsaved edits survive (no request-
                            // pane reload). Repoint before the reload so
                            // remap-by-path sees the live file.
                            let trimmed = new_name.trim().to_owned();
                            if let Some(b) = self.buffers[idx].as_endpoint_mut() {
                                b.endpoint.file = new_path.clone();
                                b.endpoint.endpoint.name = trimmed.clone();
                                b.loaded_snapshot.name = trimmed;
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
                        // Repoint any buffer under the renamed dir into the new
                        // directory *before* the reload, or remap-by-path sees a
                        // vanished file (and the next save fails NotFound).
                        for buf in &mut self.buffers {
                            if let Some(b) = buf.as_endpoint_mut()
                                && let Ok(rest) = b.endpoint.file.strip_prefix(&dir)
                            {
                                b.endpoint.file = new_dir.join(rest);
                            }
                        }
                        self.reload_explorer()?;
                        self.message = Some(Message::new(format!("renamed to {new_name}")));
                    }
                    Err(err) => self.crud_error(err),
                }
            }
            None => {}
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
            ConfirmPurpose::DeleteSequence => match key.code {
                KeyCode::Char('y') => {
                    self.mode = Mode::Normal;
                    if let Some(selected) = self.explorer.selected_sequence() {
                        let path = selected.file;
                        match persistence::delete_sequence(&path) {
                            Ok(()) => {
                                // A sequence editor/runner is a modal overlay
                                // (`Mode::Sequence`) and so cannot be open while this
                                // confirm runs in `Mode::Normal`; but if a stale
                                // editor/runner state still points at the just-deleted
                                // file, drop it so a later save can't resurrect the
                                // file and the runner can't act on a vanished target.
                                if self
                                    .sequence_editor
                                    .as_ref()
                                    .is_some_and(|e| e.path() == path)
                                {
                                    self.sequence_editor = None;
                                }
                                if self
                                    .sequence_runner
                                    .as_ref()
                                    .is_some_and(|r| r.path == path)
                                {
                                    self.sequence_runner = None;
                                }
                                // reload_explorer clamps the sub-pane cursor to the
                                // new sequence count, so selection lands on the
                                // next/previous sequence (or the empty-state
                                // affordance when none remain).
                                self.reload_explorer()?;
                                self.message = Some(Message::new("deleted sequence"));
                            }
                            Err(err) => self.crud_error(err),
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Esc => self.mode = Mode::Normal,
                _ => {}
            },
            // The discard-changes confirm serves TWO deferred ops (mutually
            // exclusive — only one is `Some`): an endpoint/workspace switch
            // (`pending_load`) or a dirty buffer close (`pending_close`).
            ConfirmPurpose::DiscardChanges if self.pending_close.is_some() => {
                self.handle_close_confirm_key(key)?;
            }
            ConfirmPurpose::DiscardChanges => match key.code {
                KeyCode::Char('s') => {
                    self.mode = Mode::Normal;
                    // The switch destroys ALL buffers, so save EVERY dirty buffer,
                    // not just the active one (else a non-active one is lost).
                    self.save_all_dirty_buffers();
                    // Only switch once every buffer is clean: a refused save (e.g.
                    // literal secret auth) leaves that buffer dirty with the error
                    // on the statusline — switching would destroy those edits.
                    if !self.any_buffer_dirty() {
                        if let Some(target) = self.pending_load.take() {
                            self.perform_load(target)?;
                        }
                    } else {
                        self.pending_load = None; // stay put, error visible
                    }
                }
                KeyCode::Char('d') => {
                    self.mode = Mode::Normal;
                    // Discard: drop buffers so `is_dirty()` is false and the switch
                    // is not re-guarded. `perform_load` replaces/clears next.
                    self.buffers.clear();
                    self.active = 0;
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

    /// Resolves a dirty-buffer-close discard-confirm (`pending_close` is `Some`).
    /// `s` saves the target buffer then closes it iff the save took; `d` discards
    /// its edits and closes; `Esc` aborts the whole close op (already-closed clean
    /// buffers stay closed). For a close-all queue, resolving the front re-opens
    /// the confirm for the next still-dirty buffer.
    fn handle_close_confirm_key(&mut self, key: KeyEvent) -> Result<()> {
        // The buffer path currently prompting (front of the queue, or the single).
        let path = match self.pending_close.as_ref() {
            Some(PendingClose::One(p)) => p.clone(),
            Some(PendingClose::All(q)) => match q.front() {
                Some(p) => p.clone(),
                None => {
                    self.pending_close = None;
                    self.mode = Mode::Normal;
                    return Ok(());
                }
            },
            None => return Ok(()),
        };
        let Some(idx) = self.buffer_index_for_path(&path) else {
            // The target vanished (e.g. reload removed it) — treat as resolved.
            self.advance_close(true);
            return Ok(());
        };
        match key.code {
            KeyCode::Char('s') => {
                // Save operates on the active buffer, so focus the target first.
                self.active = idx;
                self.save_request();
                if !self.is_dirty() {
                    let i = self.active;
                    self.remove_buffer_at(i);
                    self.advance_close(true);
                } else {
                    // Refused save (e.g. literal secret) — abort the whole op,
                    // error visible on the statusline.
                    self.pending_close = None;
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Char('d') => {
                self.remove_buffer_at(idx);
                self.advance_close(true);
            }
            KeyCode::Esc => {
                // Abort the remaining close op; already-closed clean buffers stay
                // closed. The still-open dirty buffers are untouched.
                self.pending_close = None;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        Ok(())
    }

    /// Advances a resolved close-confirm: a single close finishes; a close-all
    /// queue drops the resolved front (`resolved` = pop it) and prompts the next
    /// dirty buffer, finishing when the queue drains.
    fn advance_close(&mut self, resolved: bool) {
        match self.pending_close.as_mut() {
            Some(PendingClose::One(_)) => {
                self.pending_close = None;
                self.mode = Mode::Normal;
            }
            Some(PendingClose::All(q)) => {
                if resolved {
                    q.pop_front();
                }
                self.prompt_next_close_in_queue();
            }
            None => self.mode = Mode::Normal,
        }
    }

    /// Surfaces a CRUD [`PersistenceError`] on the statusline (fail-loud).
    fn crud_error(&mut self, err: PersistenceError) {
        self.message = Some(Message::new(format!("error: {err}")));
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
    // The dedicated message row (deliverable 9) sits above the statusline and
    // only occupies a row while a message is live, so the statusline never moves.
    // The body-search input takes that row when open (vim-style `/query`),
    // shadowing any transient message.
    let body_search_input: Option<String> = (app.mode == Mode::BodySearch).then(|| {
        let q = app.body_search_editor.text();
        let matches = match app.active_response() {
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
    // Column B split into three rows: URL bar / Request / Response.
    // Zoom (deliverable 4) collapses the unfocused pane to a bordered stub
    // (border + summary + border), keeping its title and tab-bar/stats visible.
    const COLLAPSED_HEIGHT: u16 = 3;
    // The tab strip occupies a single row at the very top of column B, ONLY when
    // at least one buffer is open. `Length(0)` renders nothing so the zero-buffer
    // layout stays byte-identical to the pre-tabs one; a strip shifts the URL
    // bar / request / response down by exactly one row.
    let strip_h: u16 = u16::from(!app.buffers.is_empty());
    let remaining = right_area.height.saturating_sub(urlbar::HEIGHT + strip_h);
    let (req_height, resp_height) = match app.zoom {
        Some(ZoomPane::Request) => (remaining.saturating_sub(COLLAPSED_HEIGHT), COLLAPSED_HEIGHT),
        Some(ZoomPane::Response) => (COLLAPSED_HEIGHT, remaining.saturating_sub(COLLAPSED_HEIGHT)),
        None => {
            let req = remaining / 2;
            (req, remaining - req)
        }
    };
    let [strip_area, urlbar_area, request_area, response_area] = Layout::vertical([
        Constraint::Length(strip_h),
        Constraint::Length(urlbar::HEIGHT),
        Constraint::Length(req_height),
        Constraint::Length(resp_height),
    ])
    .areas(right_area);

    let has_ws = app.workspace.is_some();
    let explorer_focused = app.focus == Pane::Explorer && app.mode == Mode::Normal;
    let theme = app.theme.clone();
    let dirty = app.is_dirty();
    // Every open dirty buffer's file — each such explorer row gets the accent
    // ● suffix (matched by path in the explorer render), and the active buffer's
    // dirtiness drives the URL-bar dot below.
    let dirty_files: Vec<std::path::PathBuf> = app
        .buffers
        .iter()
        .filter(|b| b.is_dirty())
        .map(|b| b.file().to_path_buf())
        .collect();
    if let Some(explorer_area) = explorer_area {
        // The left column always splits into endpoints + sequences (M7.10 stage
        // B — the sequences sub-pane is peek-symmetric, always present). The
        // focused sub-pane (`left_active`) gets Fill(1); the other collapses to
        // a 3-row stub. Endpoints on top, sequences on the bottom.
        let seq_focused = explorer_focused && app.left_active == LeftPane::Sequences;
        let tree_focused = explorer_focused && app.left_active == LeftPane::Endpoints;
        // In jump-mode the endpoints/sequences regions carry their `e`/`s`
        // mnemonics on whichever face is drawn (full-height or collapsed stub).
        let explorer_jump_label = app
            .jump
            .as_ref()
            .and_then(|j| j.label_for_pane(Pane::Explorer));
        let seq_jump_label = app.jump.as_ref().and_then(|j| j.label_for_sequences());
        let (tree_area, seq_area) = if app.left_active == LeftPane::Sequences {
            let [tree, seq] =
                Layout::vertical([Constraint::Length(COLLAPSED_HEIGHT), Constraint::Fill(1)])
                    .areas(explorer_area);
            (tree, seq)
        } else {
            let [tree, seq] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(COLLAPSED_HEIGHT)])
                    .areas(explorer_area);
            (tree, seq)
        };
        if app.left_active == LeftPane::Endpoints {
            explorer::render(
                frame,
                tree_area,
                &mut app.explorer,
                tree_focused,
                has_ws,
                &theme,
                app.jump.as_ref(),
                &dirty_files,
            );
        } else {
            // Endpoints collapsed to a stub summarizing the current selection.
            let summary = app
                .explorer
                .selected_name()
                .map(|name| Line::from(ratatui::text::Span::styled(name, theme.statusline)))
                .unwrap_or_else(|| Line::from(""));
            render_collapsed_stub(
                frame,
                tree_area,
                "Explorer",
                explorer_jump_label,
                summary,
                &theme,
            );
        }
        if app.left_active == LeftPane::Sequences {
            explorer::render_sequences_pane(
                frame,
                seq_area,
                &mut app.explorer,
                seq_focused,
                &theme,
                seq_jump_label,
            );
        } else {
            let summary = explorer::sequences_stub_summary(&app.explorer, &theme);
            render_collapsed_stub(
                frame,
                seq_area,
                "Sequences",
                seq_jump_label,
                summary,
                &theme,
            );
        }
    }
    // The tab strip (top of column B), rendered only when a buffer is open.
    if strip_h > 0 {
        let tab_items: Vec<tab_strip::TabItem> = app
            .buffers
            .iter()
            .map(|b| tab_strip::TabItem {
                short_name: b
                    .as_endpoint()
                    .map(|e| e.endpoint.endpoint.name.clone())
                    .unwrap_or_default(),
                dirty: b.is_dirty(),
            })
            .collect();
        tab_strip::render(frame, strip_area, &tab_items, app.active, &theme);
    }

    // Bind the active buffer by *field* access (not the `&mut self` accessor) so
    // `app.jump`/`focus`/`theme`/`highlight_tx`/… stay independently borrowable
    // while we hold the buffer mutably (the render borrow-split). The
    // request/editor/tabs/response/cache are disjoint fields, so they co-borrow
    // cleanly. With no buffer loaded we render fresh defaults — byte-identical to
    // the pre-refactor flat fields, always default with nothing loaded.
    let active = app.active;
    // No-buffer response fallback. In production `orphan_response` is always Idle
    // (a response requires a loaded endpoint); the response-pane isolation
    // snapshots set it to render a response with no endpoint. Bound before `buf`
    // (disjoint field) so both borrows coexist.
    let default_response: &ResponseState = &app.orphan_response;
    let buf = app
        .buffers
        .get_mut(active)
        .and_then(Buffer::as_endpoint_mut);
    let mut default_editor = EditorState::default();
    let mut default_tabs = RequestTabs::default();
    let default_cache: HashMap<u64, Vec<Line<'static>>> = HashMap::new();

    // Split the buffer into the disjoint pieces the render fns take.
    let (selected_request, editor, tabs, response, cache, url_editor, resp_scroll, resp_cursor) =
        match buf {
            Some(b) => (
                Some(&b.endpoint.endpoint.request),
                &mut b.editor,
                &mut b.tabs,
                &b.response,
                &b.highlight_cache,
                b.url_editor.as_mut(),
                b.geometry.scroll,
                b.geometry.cursor,
            ),
            None => (
                None,
                &mut default_editor,
                &mut default_tabs,
                default_response,
                &default_cache,
                None,
                0,
                0,
            ),
        };
    let req_focused = app.focus == Pane::Request && app.mode == Mode::Normal;
    let resp_focused =
        app.focus == Pane::Response && (app.mode == Mode::Normal || app.mode == Mode::BodySearch);
    let jump = app.jump.as_ref();
    let tick_count = app.tick_count;

    urlbar::render(
        frame,
        urlbar_area,
        urlbar::UrlBarCtx {
            request: selected_request,
            focused: app.focus == Pane::UrlBar && app.mode == Mode::Normal,
            editor: url_editor,
            dirty,
            jump_label: jump.and_then(|j| j.label_for_pane(Pane::UrlBar)),
        },
        &theme,
    );
    // Captured from `response::render`'s outcome so the buffer's response
    // geometry + highlight guard can be written back after the buffer borrow.
    let mut resp_outcome: Option<response::RenderOutcome> = None;
    match app.zoom {
        Some(ZoomPane::Request) => {
            request::render(
                frame,
                request_area,
                request::RenderCtx {
                    request: selected_request,
                    editor,
                    tabs,
                    focused: req_focused,
                    theme: &theme,
                    jump,
                },
            );
            let summary = response::collapsed_summary(response, &theme);
            render_collapsed_stub(
                frame,
                response_area,
                "Response",
                jump.and_then(|j| j.label_for_pane(Pane::Response)),
                summary,
                &theme,
            );
        }
        Some(ZoomPane::Response) => {
            let summary = request::collapsed_summary(selected_request, tabs, &theme);
            render_collapsed_stub(
                frame,
                request_area,
                "Request",
                jump.and_then(|j| j.label_for_pane(Pane::Request)),
                summary,
                &theme,
            );
            resp_outcome = Some(response::render(
                frame,
                response_area,
                response::RenderCtx {
                    state: response,
                    request: selected_request,
                    focused: resp_focused,
                    scroll: resp_scroll,
                    cursor: resp_cursor,
                    cache,
                    theme: &theme,
                    jump_label: jump.and_then(|j| j.label_for_pane(Pane::Response)),
                    tick_count,
                },
            ));
        }
        None => {
            request::render(
                frame,
                request_area,
                request::RenderCtx {
                    request: selected_request,
                    editor,
                    tabs,
                    focused: req_focused,
                    theme: &theme,
                    jump,
                },
            );
            resp_outcome = Some(response::render(
                frame,
                response_area,
                response::RenderCtx {
                    state: response,
                    request: selected_request,
                    focused: resp_focused,
                    scroll: resp_scroll,
                    cursor: resp_cursor,
                    cache,
                    theme: &theme,
                    jump_label: jump.and_then(|j| j.label_for_pane(Pane::Response)),
                    tick_count,
                },
            ));
        }
    }
    // Write the response outcome (geometry + highlight guard/job) back into the
    // active buffer, and enqueue a highlight job. Done after the borrow split so
    // it targets the same buffer the render read from.
    if let Some(outcome) = resp_outcome
        && let Some(b) = app
            .buffers
            .get_mut(active)
            .and_then(Buffer::as_endpoint_mut)
    {
        b.geometry.apply_render_outcome(&outcome);
        // Write the clamped horizontal scroll back onto the view so an over-pan
        // (past the widest visible line) self-corrects on the next frame (M7.7).
        if let ResponseState::Done { view } = &mut b.response {
            view.set_h_scroll(outcome.clamped_h_scroll);
        }
        if let Some(job) = outcome.job {
            let dup = b.pending_highlight == Some(job.hash);
            if !dup && let Some(tx) = &app.highlight_tx {
                // Mark in-flight only when the send actually succeeded — a dead
                // worker must not wedge the guard.
                let hash = job.hash;
                if tx.send(job).is_ok() {
                    b.pending_highlight = Some(hash);
                }
            }
        }
    }
    // The statusline (deliverable 9) keeps *only* persistent state: focus,
    // endpoint/workspace, profile, dirty, and the in-flight spinner. Transient
    // messages live in the dedicated row below.
    let in_flight = app
        .active_endpoint_buffer()
        .is_some_and(|b| b.in_flight.is_some());
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
    // While the body-search input is open OVER a runner Response region (note #2),
    // keep drawing that runner (with the `/query` row overlaid) instead of falling
    // through to the main two-column layout — the search targets the runner's
    // response. The overlay render decision uses this effective mode; every other
    // overlay still keys on the live `app.mode`.
    let overlay_mode = if app.mode == Mode::BodySearch {
        app.body_search_return
    } else {
        app.mode
    };
    match overlay_mode {
        Mode::MethodMenu => {
            if let Some(method) = app.live_request().map(|r| r.method) {
                method_menu::render(frame, main, method, &theme);
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
            let (title, question, hint) = confirm_text(purpose, app.pending_close.is_some());
            prompt::render_confirm(frame, main, title, question, hint, &theme);
        }
        Mode::EnvEditor => {
            if let Some(editor) = &app.env_editor {
                env_editor::render(frame, main, editor, &theme);
            }
        }
        Mode::Sequence => match app.sequence_view {
            SeqView::Edit => {
                if let Some(editor) = &app.sequence_editor {
                    sequence_editor::render(frame, main, editor, &theme);
                }
            }
            SeqView::Run => {
                let tick = app.tick_count;
                let active = app.active;
                // These overlays share the active buffer's highlight cache/guard
                // (keyed by viewport hash, so no cross-contamination); with no
                // buffer loaded they fall back to a per-frame empty cache — only
                // cross-frame caching is skipped, invisible to snapshots.
                let scratch_cache = HashMap::new();
                let mut scratch_pending = None;
                let cache = app
                    .buffers
                    .get(active)
                    .and_then(Buffer::as_endpoint)
                    .map(|b| &b.highlight_cache)
                    .unwrap_or(&scratch_cache);
                let job = match app.sequence_runner.as_mut() {
                    Some(runner) => {
                        sequence_runner::render(frame, main, runner, tick, cache, &theme)
                    }
                    None => None,
                };
                if let Some(job) = job {
                    let pending = app
                        .buffers
                        .get(active)
                        .and_then(Buffer::as_endpoint)
                        .map(|b| b.pending_highlight)
                        .unwrap_or(scratch_pending);
                    let dup = pending == Some(job.hash);
                    if !dup && let Some(tx) = &app.highlight_tx {
                        let hash = job.hash;
                        if tx.send(job).is_ok() {
                            match app
                                .buffers
                                .get_mut(active)
                                .and_then(Buffer::as_endpoint_mut)
                            {
                                Some(b) => b.pending_highlight = Some(hash),
                                None => scratch_pending = Some(hash),
                            }
                        }
                    }
                }
                let _ = scratch_pending;
            }
        },
        Mode::LoadRunner => {
            let tick = app.tick_count;
            let active = app.active;
            let scratch_cache = HashMap::new();
            let mut scratch_pending = None;
            let cache = app
                .buffers
                .get(active)
                .and_then(Buffer::as_endpoint)
                .map(|b| &b.highlight_cache)
                .unwrap_or(&scratch_cache);
            let job = match app.load_runner.as_mut() {
                Some(runner) => load_runner::render(frame, main, runner, tick, cache, &theme),
                None => None,
            };
            if let Some(job) = job {
                let pending = app
                    .buffers
                    .get(active)
                    .and_then(Buffer::as_endpoint)
                    .map(|b| b.pending_highlight)
                    .unwrap_or(scratch_pending);
                let dup = pending == Some(job.hash);
                if !dup && let Some(tx) = &app.highlight_tx {
                    let hash = job.hash;
                    if tx.send(job).is_ok() {
                        match app
                            .buffers
                            .get_mut(active)
                            .and_then(Buffer::as_endpoint_mut)
                        {
                            Some(b) => b.pending_highlight = Some(hash),
                            None => scratch_pending = Some(hash),
                        }
                    }
                }
            }
            let _ = scratch_pending;
        }
        _ => {}
    }

    // The URL vim-popup editor (deliverable 7) renders over the main area.
    if let Some(b) = app.active_endpoint_buffer_mut()
        && let Some(editor) = b.url_popup.as_mut()
    {
        urlbar::render_popup(frame, main, editor, &theme);
    }

    // The two-level which-key leader popup (deliverable 1).
    if let Some(state) = app.leader.clone() {
        let (title, entries) = leader_popup_entries(app, &state);
        leader_popup::render(frame, main, &title, &entries, &theme);
    }

    // The `?` help overlay (deliverable 8), rendered from the live keymap.
    if app.help_open {
        let outcome = help::render(
            frame,
            main,
            &app.keymap,
            app.help_scroll,
            &theme,
            app.help_search.as_ref(),
        );
        app.help_scroll = app.help_scroll.min(outcome.total.saturating_sub(1));
        app.help_viewport_height = outcome.viewport_height;
    }
}

/// Builds the `(title, entries)` for the which-key popup at `state`. Root shows
/// sorted direct binds plus one `"<key>   ▸ <submenu>"` row per submenu prefix;
/// a submenu shows its own sorted `(combo, label)` rows.
fn leader_popup_entries(app: &App, state: &LeaderState) -> (String, Vec<(String, String)>) {
    match state {
        LeaderState::Root => {
            let mut entries: Vec<(String, String)> = app
                .keymap
                .iter_leader_root_acts()
                .map(|(combo, action)| (combo.to_string(), action.label().to_owned()))
                .collect();
            entries.sort();
            // Submenu prefixes render with a ▸ marker, after the direct binds.
            for (key, label) in app.keymap.leader_menu_combos() {
                entries.push((key, format!("▸ {label}")));
            }
            (" leader ".to_owned(), entries)
        }
        LeaderState::Submenu(menu) => {
            let mut entries: Vec<(String, String)> = app
                .keymap
                .iter_submenu(menu)
                .map(|(combo, action)| (combo.to_string(), action.label().to_owned()))
                .collect();
            entries.sort();
            (
                format!(" leader · {} ", app.keymap.submenu_title(menu)),
                entries,
            )
        }
    }
}

/// The which-key leader popup: a small floating panel listing the bound
/// continuations while a leader chord is in progress.
mod leader_popup {
    use ratatui::Frame;
    use ratatui::layout::{Constraint, Flex, Layout, Rect};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

    use crate::tui::theme::Theme;

    /// Renders the popup with `title` and `(keys, label)` continuation entries.
    pub fn render(
        frame: &mut Frame,
        area: Rect,
        title: &str,
        entries: &[(String, String)],
        theme: &Theme,
    ) {
        let width = entries
            .iter()
            .map(|(k, l)| k.len() + l.len() + 5)
            .max()
            .unwrap_or(20)
            .max(title.len() + 4)
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
            .title(title.to_owned())
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
/// jump label when active) around the one-line tab-bar/stats summary, so the
/// pane keeps its chrome when collapsed rather than vanishing into a bare row.
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

/// The (title, question, key-hint) for a confirmation overlay. `closing`
/// distinguishes the two `DiscardChanges` flows — a buffer close (`<leader>t x`/
/// `X`, `pending_close` set) vs an endpoint/workspace switch (`pending_load`) —
/// so the copy matches the actual action.
fn confirm_text(
    purpose: ConfirmPurpose,
    closing: bool,
) -> (&'static str, &'static str, &'static str) {
    match purpose {
        ConfirmPurpose::DeleteEndpoint => ("Delete endpoint", "Delete this endpoint?", "[y/n]"),
        ConfirmPurpose::DeleteSequence => ("Delete sequence", "Delete this sequence?", "[y/n]"),
        ConfirmPurpose::DiscardChanges if closing => (
            "Unsaved changes",
            "Close without saving?",
            "s save · d discard · esc stay",
        ),
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

    /// A minimal loaded endpoint buffer (no workspace) so white-box tests can set
    /// per-buffer state (`in_flight`, `response`, editor) that used to live as
    /// flat `App` fields.
    fn open_bare_endpoint(app: &mut App) {
        let endpoint = Endpoint {
            seq: 0,
            name: "test".to_owned(),
            request: Request {
                method: churl_core::model::Method::Get,
                url: "https://api.test/x".to_owned(),
                headers: Vec::new(),
                params: Vec::new(),
                body: None,
                auth: None,
            },
        };
        app.open_or_focus_buffer(SelectedEndpoint {
            display_path: "test".to_owned(),
            file: std::path::PathBuf::from("/tmp/test.toml"),
            collection: 0,
            endpoint,
        });
    }

    /// White-box: put a fabricated in-flight request on the active buffer.
    fn set_active_in_flight(app: &mut App, generation: u64) {
        let handle = tokio::spawn(async {});
        app.active_endpoint_buffer_mut().unwrap().in_flight = Some(InFlightRequest {
            handle: handle.abort_handle(),
            generation,
            meta: meta(),
        });
    }

    /// White-box: whether the active buffer has an in-flight request.
    fn active_in_flight_is_some(app: &App) -> bool {
        app.active_endpoint_buffer()
            .is_some_and(|b| b.in_flight.is_some())
    }

    /// A response whose generation no longer matches the in-flight request (after
    /// a cancel+resend) must be dropped without touching the pane.
    #[tokio::test]
    async fn stale_generation_response_is_dropped() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        open_bare_endpoint(&mut app);
        app.generation = 5;
        set_active_in_flight(&mut app, 5);

        // A late result from an older generation is ignored…
        app.on_response(4, Ok(response()), meta());
        assert!(matches!(app.response(), ResponseState::Idle));
        assert!(
            active_in_flight_is_some(&app),
            "in-flight preserved on stale drop"
        );

        // …the matching generation lands and clears the in-flight slot.
        app.on_response(5, Ok(response()), meta());
        assert!(matches!(app.response(), ResponseState::Done { .. }));
        assert!(!active_in_flight_is_some(&app));
    }

    /// Puts a fresh app in insert mode on the request pane's Body tab (where the
    /// edtui editor lives). A bare endpoint buffer is loaded so the per-buffer
    /// editor/tabs exist (they moved off `App` into the active buffer).
    fn insert_mode_app(keymap: KeyMap) -> App {
        let mut app = App::new(None, keymap).unwrap();
        open_bare_endpoint(&mut app);
        app.focus = Pane::Request;
        app.test_tabs().active = RequestTab::Body;
        app.test_editor().mode = EditorMode::Insert;
        app
    }

    /// Insert mode + Ctrl-S dispatches Send instead of reaching edtui. Proof of
    /// interception is an OBSERVABLE Send effect that only Send produces: with a
    /// request already in flight, Send's in-flight guard emits the distinctive
    /// "already in flight" message. If Ctrl-S instead reached edtui (interception
    /// removed), edtui would ignore the Ctrl-modified char and leave `message`
    /// None — so this fails when the insert-mode `control_intercept` block is
    /// removed. The editor is also untouched and insert mode is preserved.
    #[tokio::test]
    async fn insert_mode_ctrl_s_dispatches_send() {
        let mut app = insert_mode_app(KeyMap::default());
        // A request already in flight: Send's guard produces an observable message.
        set_active_in_flight(&mut app, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("request already in flight — ctrl-c to cancel"),
            "Ctrl-S must dispatch Send (observable via its in-flight guard)"
        );
        assert_eq!(String::from(app.test_editor().lines.clone()), "");
        assert_eq!(app.test_editor().mode, EditorMode::Insert);
        assert!(!app.should_quit);
    }

    /// Insert mode + Ctrl-C with a request in flight cancels it (never quits,
    /// never reaches edtui).
    #[tokio::test]
    async fn insert_mode_ctrl_c_cancels_in_flight() {
        let mut app = insert_mode_app(KeyMap::default());
        app.generation = 1;
        set_active_in_flight(&mut app, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(!active_in_flight_is_some(&app));
        assert!(matches!(app.response(), ResponseState::Cancelled));
        assert!(!app.should_quit);
        assert_eq!(String::from(app.test_editor().lines.clone()), "");
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
        assert_eq!(String::from(app.test_editor().lines.clone()), "sc");
        assert!(!app.should_quit);
        assert!(app.message.is_none());
    }

    /// The interception resolves through the keymap: a remapped send key with
    /// CONTROL is intercepted in insert mode too. Discriminated by the same
    /// observable Send effect (the in-flight guard's message), so it fails if the
    /// insert-mode `control_intercept` block is removed.
    #[tokio::test]
    async fn insert_mode_remapped_ctrl_send_is_intercepted() {
        let overrides =
            std::collections::BTreeMap::from([("ctrl-b".to_string(), "send".to_string())]);
        let mut app = insert_mode_app(KeyMap::with_overrides(&overrides).unwrap());
        set_active_in_flight(&mut app, 1);
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("request already in flight — ctrl-c to cancel"),
            "remapped Ctrl-B must dispatch Send (observable via its in-flight guard)"
        );
        assert_eq!(String::from(app.test_editor().lines.clone()), "");
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
        assert_eq!(app.mode, Mode::Sequence);
        assert_eq!(app.sequence_view, SeqView::Run);
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

    // ---- note #2: unified sequence-runner response viewer ----

    /// Opens the sequence runner, lands a JSON `Done` response on step 0, selects
    /// it, and focuses the Response region — the shared setup for parity tests.
    /// Uses `continue` so a non-200 wouldn't halt (here everything is 200 anyway).
    fn seq_app_with_response(root: &Path, body: &str) -> App {
        let mut app = sequence_app(root, "continue", "");
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(json_resp(body)));
        let runner = app.sequence_runner.as_mut().unwrap();
        runner.selected = 0;
        runner.focus = sequence_runner::RunnerFocus::Response;
        app
    }

    /// The selected step's live `ResponseView`.
    fn seq_view(app: &App) -> &ResponseView {
        match app
            .sequence_runner
            .as_ref()
            .unwrap()
            .selected_response()
            .unwrap()
        {
            ResponseState::Done { view } => view,
            other => panic!("selected step is not Done: {other:?}"),
        }
    }

    /// Parity: `p`/`s`/`#`/`W`/`h` toggle the selected STEP's view via the shared
    /// handlers, identical to the main pane.
    #[test]
    fn sequence_response_parity_toggles() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_app_with_response(dir.path(), "{\"b\":1,\"a\":2}");
        assert_eq!(app.active_response_surface(), ResponseSurface::Sequence);

        assert!(seq_view(&app).pretty());
        app.handle_key(norm('p')).unwrap();
        assert!(!seq_view(&app).pretty(), "p toggled pretty off");
        app.handle_key(norm('p')).unwrap();

        app.handle_key(norm('s')).unwrap();
        assert!(seq_view(&app).sort_keys(), "s toggled sort on");

        assert!(seq_view(&app).line_numbers());
        app.handle_key(norm('#')).unwrap();
        assert!(!seq_view(&app).line_numbers(), "# toggled gutter off");

        app.handle_key(shift('W')).unwrap();
        assert!(seq_view(&app).wrap(), "W toggled wrap on");

        app.handle_key(norm('h')).unwrap();
        assert_eq!(
            seq_view(&app).view_mode(),
            ViewMode::Headers,
            "h switched to headers"
        );
    }

    /// `/` searches the step response, and Esc returns to the sequence runner (NOT
    /// Normal); `y` copies byte-exact raw wire bytes.
    #[test]
    fn sequence_response_search_and_copy() {
        let dir = tempfile::tempdir().unwrap();
        // Body carries "needle" twice (searchable) plus a tab + NUL that sanitize
        // rewrites on display — so copy proving byte-exactness must return the RAW
        // wire bytes, not the sanitized text (the M7.7 invariant, in the runner).
        let raw = "needle\tval\u{0}needle";
        let mut app = seq_app_with_response(dir.path(), raw);
        render_once(&mut app);
        app.handle_key(norm('/')).unwrap();
        assert_eq!(app.mode, Mode::BodySearch);
        assert_eq!(app.body_search_return, Mode::Sequence);
        for c in "needle".chars() {
            app.handle_key(norm(c)).unwrap();
        }
        assert!(seq_view(&app).search().is_some_and(|s| s.count() >= 2));
        app.handle_key(keyc(KeyCode::Esc)).unwrap();
        assert_eq!(
            app.mode,
            Mode::Sequence,
            "Esc returns to the sequence runner"
        );

        // Copy returns the raw on-the-wire bytes (tab + NUL intact), not the
        // sanitized display — matching the load-runner copy-invariant test.
        app.handle_key(norm('y')).unwrap();
        assert_eq!(
            app.pending_clipboard.as_ref().unwrap().payload,
            raw,
            "y returns byte-exact raw wire bytes (incl. tab + NUL) in the sequence runner"
        );
    }

    /// Cursor nav (`j`/`G`) moves the sequence runner's Response cursor via the
    /// shared movement path.
    #[test]
    fn sequence_response_cursor_nav_shared_path() {
        let dir = tempfile::tempdir().unwrap();
        let body = (0..20)
            .map(|i| format!("\"k{i}\":{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut app = seq_app_with_response(dir.path(), &format!("{{{body}}}"));
        render_once(&mut app);
        assert_eq!(app.sequence_runner.as_ref().unwrap().geometry.cursor, 0);
        app.handle_key(norm('j')).unwrap();
        assert_eq!(app.sequence_runner.as_ref().unwrap().geometry.cursor, 1);
        app.handle_key(shift('G')).unwrap();
        assert!(app.sequence_runner.as_ref().unwrap().geometry.cursor > 1);
    }

    /// PRESERVED NAV: from Response focus, Tab still toggles Steps↔Response and `r`
    /// still re-runs — response actions never eat them. And when Steps-focused,
    /// `j`/`k` still select steps.
    #[test]
    fn sequence_runner_nav_not_shadowed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_app_with_response(dir.path(), "{\"a\":1}");
        assert_eq!(
            app.sequence_runner.as_ref().unwrap().focus,
            sequence_runner::RunnerFocus::Response
        );
        // Tab toggles Response → Steps (runner-owned; not a response action).
        app.handle_key(keyc(KeyCode::Tab)).unwrap();
        assert_eq!(
            app.sequence_runner.as_ref().unwrap().focus,
            sequence_runner::RunnerFocus::Steps,
            "Tab still toggles regions"
        );
        // Steps-focused `j`/`k` select steps (not response scroll).
        app.sequence_runner.as_mut().unwrap().selected = 0;
        app.handle_key(norm('j')).unwrap();
        assert_eq!(
            app.sequence_runner.as_ref().unwrap().selected,
            1,
            "j selects the next step when Steps-focused"
        );
        // Back to Response; `r` still re-runs (bumps generation).
        app.sequence_runner.as_mut().unwrap().focus = sequence_runner::RunnerFocus::Response;
        let g0 = runner_gen(&app);
        app.handle_key(norm('r')).unwrap();
        assert!(
            runner_gen(&app) > g0,
            "r still re-runs the sequence from Response focus"
        );
    }

    /// A response action does nothing when the sequence runner focus is Steps.
    #[test]
    fn sequence_response_action_inert_when_steps_focused() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_app_with_response(dir.path(), "{\"a\":1}");
        app.sequence_runner.as_mut().unwrap().focus = sequence_runner::RunnerFocus::Steps;
        assert_eq!(app.active_response_surface(), ResponseSurface::Main);
        let before = seq_view(&app).pretty();
        app.handle_key(norm('p')).unwrap();
        assert_eq!(
            seq_view(&app).pretty(),
            before,
            "p is inert when Steps-focused"
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

    /// Builds a sequence app whose step 0 extracts `token` from `$.token`, with the
    /// rule's target chosen by `persist` (true = Session, false = Run-only).
    fn session_seq_app(root: &Path, persist: bool) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("login.toml"),
            "seq = 0\nname = \"login\"\n\n[request]\nmethod = \"POST\"\nurl = \"https://api.test/login\"\n",
        )
        .unwrap();
        let seq_dir = root.join("sequences");
        std::fs::create_dir(&seq_dir).unwrap();
        let persist_line = if persist {
            "persist = [\"token\"]\n"
        } else {
            ""
        };
        std::fs::write(
            seq_dir.join("flow.toml"),
            format!(
                "seq = 0\nname = \"Flow\"\non_error = \"halt\"\n\n[[step]]\nseq = 0\nendpoint = \"api/login.toml\"\n{persist_line}[step.extract]\ntoken = \"$.token\"\n"
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

    /// A dummy SelectedEndpoint for building a standalone resolver in tests.
    fn dummy_selected(file: &Path) -> SelectedEndpoint {
        SelectedEndpoint {
            display_path: "api/login".into(),
            file: file.to_owned(),
            collection: 0,
            endpoint: churl_core::persistence::load_endpoint(file).unwrap(),
        }
    }

    /// Note #6: a Session-target rule writes the extracted value into the workspace
    /// session store, and a subsequent STANDALONE send resolves `{{token}}` from it.
    #[test]
    fn session_rule_persists_and_resolves_standalone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut app = session_seq_app(root, true);
        // Before the run, nothing is captured.
        assert!(app.session_vars().is_empty());
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"T-123"}"#)));
        // Captured into the in-memory Session store.
        assert_eq!(
            app.session_vars().get("token").map(String::as_str),
            Some("T-123")
        );
        // A standalone resolver (as a normal send would build) substitutes it.
        let file = root.join("api/login.toml");
        let selected = dummy_selected(&file);
        let resolver = app.build_resolver(&selected);
        assert_eq!(resolver.substitute("Bearer {{token}}"), "Bearer T-123");
    }

    /// A Run-only rule (default, no `persist`) never writes to the session store.
    #[test]
    fn run_only_rule_does_not_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = session_seq_app(dir.path(), false);
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"T-123"}"#)));
        assert!(
            app.session_vars().is_empty(),
            "run-only rule must not touch the session store"
        );
    }

    /// A failed extraction (missing key) never clobbers an existing Session value.
    #[test]
    fn failed_extraction_keeps_prior_session_value() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut app = session_seq_app(root, true);
        // First run captures T-1.
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"T-1"}"#)));
        assert_eq!(
            app.session_vars().get("token").map(String::as_str),
            Some("T-1")
        );
        // Re-run; this time the body lacks `token` → extraction fails.
        app.start_sequence_run();
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"other":1}"#)));
        assert_eq!(
            app.session_vars().get("token").map(String::as_str),
            Some("T-1"),
            "a failed extraction leaves the prior value intact"
        );
    }

    /// A re-run with a Session-target rule overwrites the captured value.
    #[test]
    fn session_value_overwritten_on_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut app = session_seq_app(root, true);
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"OLD"}"#)));
        assert_eq!(
            app.session_vars().get("token").map(String::as_str),
            Some("OLD")
        );
        app.start_sequence_run();
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"NEW"}"#)));
        assert_eq!(
            app.session_vars().get("token").map(String::as_str),
            Some("NEW")
        );
    }

    /// Cross-workspace isolation: a capture in workspace A does not resolve in B.
    #[test]
    fn session_captures_are_workspace_isolated() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        // Capture a token in workspace A.
        let mut app = session_seq_app(dir_a.path(), true);
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"A-TOK"}"#)));
        assert_eq!(
            app.session_vars().get("token").map(String::as_str),
            Some("A-TOK")
        );
        // Switch the app's workspace to B (same in-memory store, different key).
        std::fs::write(dir_b.path().join("churl.toml"), "name = \"b\"\n").unwrap();
        app.workspace = Some(OpenWorkspace::open(dir_b.path()).unwrap());
        assert!(
            app.session_vars().is_empty(),
            "workspace B sees none of A's captures"
        );
    }

    /// Security: after a run that persists a token, the session value never
    /// touches disk — the sequence/workspace TOMLs are byte-unchanged.
    #[test]
    fn session_value_never_written_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut app = session_seq_app(root, true);
        let seq_path = root.join("sequences/flow.toml");
        let manifest_path = root.join("churl.toml");
        let endpoint_path = root.join("api/login.toml");
        let seq_before = std::fs::read_to_string(&seq_path).unwrap();
        let manifest_before = std::fs::read_to_string(&manifest_path).unwrap();
        let endpoint_before = std::fs::read_to_string(&endpoint_path).unwrap();

        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Ok(ok_resp(200, r#"{"token":"SECRET-TOKEN"}"#)));
        assert_eq!(
            app.session_vars().get("token").map(String::as_str),
            Some("SECRET-TOKEN")
        );

        // No file carries the captured value; the files are byte-identical.
        assert_eq!(std::fs::read_to_string(&seq_path).unwrap(), seq_before);
        assert_eq!(
            std::fs::read_to_string(&manifest_path).unwrap(),
            manifest_before
        );
        assert_eq!(
            std::fs::read_to_string(&endpoint_path).unwrap(),
            endpoint_before
        );
        // Belt-and-braces: the secret appears in NO file under the workspace.
        for entry in walkdir(root) {
            let text = std::fs::read_to_string(&entry).unwrap_or_default();
            assert!(
                !text.contains("SECRET-TOKEN"),
                "session value leaked to disk at {}",
                entry.display()
            );
        }
    }

    /// Recursively lists every file under `root` (test-only, small trees).
    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_owned()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
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
        assert_eq!(app.mode, Mode::Sequence);
        assert_eq!(app.sequence_view, SeqView::Edit);

        // `a` opens the picker, Enter accepts the first endpoint.
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
        // second rule, also named "x"
        ch(&mut app, 'a');
        ch(&mut app, 'x');
        enter(&mut app);
        for c in "$.b".chars() {
            ch(&mut app, c);
        }
        enter(&mut app);
        ch(&mut app, 'w'); // save → refused

        assert_eq!(app.mode, Mode::Sequence, "editor stays open on refusal");
        assert_eq!(app.sequence_view, SeqView::Edit);
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

    // ---- Feature C: unified sequence surface (edit ⇄ run) ----

    /// Opens a one-step sequence in the Edit face (via `open_sequence_editor`).
    fn sequence_surface_app(root: &Path) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("one.toml"),
            "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
        )
        .unwrap();
        let seq_dir = root.join("sequences");
        std::fs::create_dir(&seq_dir).unwrap();
        let seq_path = seq_dir.join("flow.toml");
        std::fs::write(
            &seq_path,
            "seq = 0\nname = \"Flow\"\non_error = \"halt\"\n\n[[step]]\nseq = 0\nendpoint = \"api/one.toml\"\n",
        )
        .unwrap();
        let ws = open_workspace(root).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        let sequence = churl_core::persistence::load_sequence(&seq_path).unwrap();
        app.open_sequence_editor("Flow".to_owned(), seq_path, &sequence);
        app
    }

    fn ctrl_r(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
    }

    /// Opening a sequence for edit lands on Mode::Sequence + the Edit face, with
    /// no runner built yet (Run is entered lazily).
    #[test]
    fn sequence_surface_opens_in_edit_face() {
        let dir = tempfile::tempdir().unwrap();
        let app = sequence_surface_app(dir.path());
        assert_eq!(app.mode, Mode::Sequence);
        assert_eq!(app.sequence_view, SeqView::Edit);
        assert!(app.sequence_editor.is_some());
        assert!(app.sequence_runner.is_none(), "run built lazily");
    }

    /// `<leader>s r` (RunSequence) opens the surface in the Run face and starts
    /// the run.
    #[test]
    fn run_sequence_opens_in_run_face() {
        let dir = tempfile::tempdir().unwrap();
        let app = sequence_app(dir.path(), "halt", "");
        assert_eq!(app.mode, Mode::Sequence);
        assert_eq!(app.sequence_view, SeqView::Run);
        assert!(app.sequence_runner.is_some());
    }

    /// `Ctrl-R` on a CLEAN editor flips to the Run face, building the runner from
    /// the saved sequence; a second `Ctrl-R` flips back.
    #[test]
    fn ctrl_r_toggles_faces_when_clean() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_surface_app(dir.path());
        assert!(!app.sequence_editor.as_ref().unwrap().is_dirty());
        ctrl_r(&mut app);
        assert_eq!(app.sequence_view, SeqView::Run, "clean edit→run flips");
        let runner = app.sequence_runner.as_ref().expect("runner built");
        assert_eq!(runner.steps.len(), 1, "runner built from the saved steps");
        // Run→Edit is always safe.
        ctrl_r(&mut app);
        assert_eq!(app.sequence_view, SeqView::Edit);
    }

    /// A DIRTY editor blocks Edit→Run with a notify (no auto-save, no stale run).
    #[test]
    fn dirty_edit_to_run_blocks_with_notify() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_surface_app(dir.path());
        // Toggle on_error to make the editor dirty (`o` in the editor).
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.sequence_editor.as_ref().unwrap().is_dirty());
        ctrl_r(&mut app);
        assert_eq!(
            app.sequence_view,
            SeqView::Edit,
            "stays in edit while dirty"
        );
        assert!(app.sequence_runner.is_none(), "no runner built while dirty");
        assert!(
            app.message
                .as_ref()
                .is_some_and(|m| m.text.contains("save (w) before running")),
            "expected a save-first notify, got {:?}",
            app.message.as_ref().map(|m| &m.text)
        );
    }

    /// `close_sequence_surface` clears both component states + the abort handle
    /// and returns to Normal.
    #[test]
    fn close_sequence_surface_clears_everything() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_surface_app(dir.path());
        // Build the runner too (clean flip), so both states are populated.
        ctrl_r(&mut app);
        assert!(app.sequence_runner.is_some());
        assert!(app.sequence_editor.is_some());
        app.close_sequence_surface();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.sequence_runner.is_none());
        assert!(app.sequence_editor.is_none());
        assert!(app.sequence_abort.is_none());
        assert_eq!(app.sequence_view, SeqView::Edit);
    }

    /// MAJOR: `Ctrl-R` Edit→Run rebuilds the runner but must first ABORT any
    /// in-flight step surviving from a prior run (kept alive across Run→Edit) —
    /// otherwise the async step (a real POST/DELETE) orphans and runs to
    /// completion in the background. Inject a live abort handle and assert the
    /// rebuild aborts it.
    #[tokio::test]
    async fn edit_to_run_aborts_orphaned_inflight_step() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_surface_app(dir.path());
        // Simulate a prior run whose in-flight step survived a Run→Edit flip: a
        // long-lived task whose abort handle is parked in `sequence_abort`.
        let task = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        let handle = task.abort_handle();
        app.sequence_abort = Some(handle.clone());
        // Force the surface into the Edit face (clean editor) so a Ctrl-R flip
        // takes the Edit→Run rebuild branch.
        app.sequence_view = SeqView::Edit;
        assert!(!handle.is_finished(), "task live before the flip");

        ctrl_r(&mut app);

        assert_eq!(app.sequence_view, SeqView::Run, "clean edit→run flips");
        assert!(
            app.sequence_abort.is_none(),
            "the prior abort handle was taken during rebuild"
        );
        // Yield so the abort propagates, then confirm the orphaned task is gone.
        tokio::task::yield_now().await;
        assert!(
            handle.is_finished(),
            "the prior in-flight step must be aborted, not orphaned"
        );
    }

    /// Pressing `f` enters jump-mode; a pane label focuses that pane and exits.
    #[test]
    fn jump_label_focuses_pane() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Jump);
        assert!(app.jump.is_some());
        // `p` is the Response mnemonic (M7.10 stage B — `s` moved to Sequences,
        // Response took `p` for res`p`onse).
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.jump.is_none());
        assert_eq!(app.focus, Pane::Response);
    }

    /// M7.10 stage B: `f`-jump labels NO endpoint rows — a row-alphabet key that
    /// used to select a row is now inert (jump-mode stays open, ignoring it).
    #[test]
    fn jump_labels_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.explorer.expand().unwrap();
        assert_eq!(app.explorer.rows().len(), 2);
        let cursor_before = app.explorer.cursor;
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        // `d` was a row label pre-B; now it maps to nothing in jump-mode.
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.mode,
            Mode::Jump,
            "a non-label key does not exit jump-mode"
        );
        assert_eq!(app.explorer.cursor, cursor_before, "no row was selected");
        assert!(
            app.selected().is_none(),
            "nothing loaded — rows are unlabelled"
        );
    }

    /// M7.10 stage B: `f` no longer labels a row, so pressing the Jump key again
    /// falls through to the "Jump key again cancels" rule.
    #[test]
    fn jump_f_again_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.explorer.expand().unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Jump);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal, "f (no longer a label) cancels");
        assert!(app.jump.is_none());
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

    /// Loads the single `{{host}}` endpoint from `workspace_fixture` into an active
    /// buffer (no profile → `host` is unresolved). Shared by the fail-loud send/load
    /// tests below.
    fn app_with_unresolved_endpoint(root: &Path) -> App {
        let mut app = workspace_fixture(root);
        app.explorer.expand().unwrap();
        app.explorer.cursor = 1;
        let selected = app.explorer.select().unwrap().expect("endpoint");
        app.load_endpoint(selected);
        app
    }

    /// Fail loud, path 1 (Ctrl-S / main-pane send): a request that still carries an
    /// unresolved `{{host}}` after substitution is NOT sent — the message names the
    /// variable and no request goes in flight.
    #[test]
    fn send_refuses_unresolved_variable_with_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_unresolved_endpoint(dir.path());
        app.send_request();
        let msg = app.message.as_ref().map(|m| m.text.as_str());
        assert_eq!(
            msg,
            Some("unresolved variable(s): host — set them in a profile/env or via CLI")
        );
        assert!(
            app.active_endpoint_buffer()
                .is_some_and(|b| b.in_flight.is_none()),
            "no request must go in flight when a variable is unresolved"
        );
        // Sanity: with the `prod` profile active, `host` resolves and the guard does
        // not fire (proves the check is not firing on a resolved request). The client
        // is None so nothing spawns; we clear the stale refusal message first and
        // assert no NEW refusal is set.
        app.set_profile(Some("prod".to_owned()));
        app.message = None;
        app.send_request();
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            None,
            "a resolved request must not be refused"
        );
    }

    /// Fail loud, path 2 (load runner): `open_load_runner` resolves once at open
    /// time; an unresolved `{{host}}` refuses to open the runner at all (the whole
    /// batch never fires) and names the variable.
    #[test]
    fn load_runner_refuses_unresolved_variable() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_unresolved_endpoint(dir.path());
        app.open_load_runner();
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("unresolved variable(s): host — set them in a profile/env or via CLI")
        );
        assert!(
            app.load_runner.is_none(),
            "the load runner must not open with an unresolved variable"
        );
        assert!(
            app.load_request.is_none(),
            "no request must be armed for the batch"
        );
        assert_ne!(app.mode, Mode::LoadRunner);
    }

    /// Builds a workspace with a single-step sequence whose endpoint request carries
    /// `{{missing}}` (no scope resolves it), then opens the runner. `on_error`
    /// selects halt vs continue.
    fn unresolved_sequence_app(root: &Path, on_error: &str) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        // Step 0's request has an unresolved var; step 1 is clean (to prove continue
        // advances past the failed step).
        std::fs::write(
            coll.join("bad.toml"),
            "seq = 0\nname = \"bad\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{{missing}}\"\n",
        )
        .unwrap();
        std::fs::write(
            coll.join("good.toml"),
            "seq = 0\nname = \"good\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/ok\"\n",
        )
        .unwrap();
        let seq_dir = root.join("sequences");
        std::fs::create_dir(&seq_dir).unwrap();
        std::fs::write(
            seq_dir.join("flow.toml"),
            format!(
                "seq = 0\nname = \"Flow\"\non_error = \"{on_error}\"\n\n[[step]]\nseq = 0\nendpoint = \"api/bad.toml\"\n\n[[step]]\nseq = 1\nendpoint = \"api/good.toml\"\n"
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

    /// Fail loud, path 3 (sequence step): a step whose request has an unresolved
    /// `{{var}}` fails that step (via `prepare_step`) with a message naming the var,
    /// and `on_error = halt` skips the rest.
    #[test]
    fn sequence_step_fails_on_unresolved_variable_and_halts() {
        let dir = tempfile::tempdir().unwrap();
        let app = unresolved_sequence_app(dir.path(), "halt");
        // Opening the runner drives step 0 synchronously through `prepare_step`,
        // which now refuses the unresolved request.
        match step_status(&app, 0) {
            StepStatus::HttpError(msg) => assert!(
                msg.contains("unresolved variable(s): missing"),
                "step error must name the unresolved variable, got: {msg}"
            ),
            other => panic!("expected HttpError for the unresolved step, got {other:?}"),
        }
        assert_eq!(
            step_status(&app, 1),
            StepStatus::Skipped,
            "halt must skip the remaining step"
        );
        assert!(app.sequence_runner.as_ref().unwrap().finished);
    }

    /// Fail loud, path 3 with `on_error = continue`: the unresolved step still fails
    /// with the naming message, but the run proceeds to the next (clean) step.
    #[test]
    fn sequence_step_unresolved_continue_advances() {
        let dir = tempfile::tempdir().unwrap();
        let app = unresolved_sequence_app(dir.path(), "continue");
        assert!(matches!(step_status(&app, 0), StepStatus::HttpError(_)));
        // Continue: step 1 (clean) runs — client is None so it stays Running.
        assert_eq!(step_status(&app, 1), StepStatus::Running);
    }

    /// Regression (M6.6 review #1): on the Body tab in edtui Normal mode, the
    /// row-editing keys (`i` insert, `a` append) must reach edtui instead of
    /// being eaten by the Request overlay's RowEdit/RowAdd — there are no rows
    /// on the Body tab.
    #[test]
    fn body_tab_row_keys_reach_edtui() {
        for c in ['i', 'a'] {
            let mut app = App::new(None, KeyMap::default()).unwrap();
            open_bare_endpoint(&mut app);
            app.focus = Pane::Request;
            app.test_tabs().active = RequestTab::Body;
            assert_eq!(app.test_editor().mode, EditorMode::Normal);
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(
                app.test_editor().mode,
                EditorMode::Insert,
                "'{c}' on the Body tab must enter edtui insert mode"
            );
            assert!(
                app.test_tabs().editing.is_none(),
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
        assert_eq!(app.selected().unwrap().collection, 0);

        // Create a collection that sorts *before* "bbb" and reload: "bbb" is
        // now index 1; the stale index 0 would read "aaa"'s (empty) vars.
        churl_core::persistence::create_collection(dir.path(), "aaa").unwrap();
        app.reload_explorer().unwrap();
        assert_eq!(
            app.selected().unwrap().collection,
            1,
            "collection index must be remapped from the file path"
        );
        let selected = app.selected().cloned().unwrap();
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
        if let Some(picker) = app.picker.as_mut() {
            picker.selected = 2; // prod
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

    fn esc(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
    }

    fn enter(app: &mut App) {
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
    }

    /// `/` in the help overlay opens a live search; Esc restores (search cleared,
    /// help still open); Enter commits (matches retained, input closed).
    #[test]
    fn help_search_esc_restores_enter_commits() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        press(&mut app, '?');
        assert!(app.help_open, "? opens the help overlay");

        // `/` opens the search input with a live (empty) search.
        press(&mut app, '/');
        assert!(app.help_search_input, "/ opens the help-search input");
        assert!(app.help_search.is_some());

        // Type a query that matches a known label.
        for c in "explorer".chars() {
            press(&mut app, c);
        }
        let count = app.help_search.as_ref().unwrap().count();
        assert!(count > 0, "typing `explorer` matches help rows");

        // Esc restores: search cleared, help still open.
        esc(&mut app);
        assert!(app.help_search.is_none(), "esc clears the help search");
        assert!(!app.help_search_input);
        assert!(app.help_open, "esc keeps the help overlay open");

        // Re-open, type, and Enter commits: matches retained, input closed.
        press(&mut app, '/');
        for c in "explorer".chars() {
            press(&mut app, c);
        }
        enter(&mut app);
        assert!(!app.help_search_input, "enter closes the search input");
        assert!(
            app.help_search.as_ref().map(|s| s.count()).unwrap_or(0) > 0,
            "enter retains the matches for n/N"
        );
        assert!(app.help_open, "enter keeps the help overlay open");

        // `n` still cycles after commit (input closed).
        let before = app.help_search.as_ref().unwrap().current_ordinal();
        press(&mut app, 'n');
        let after = app.help_search.as_ref().unwrap().current_ordinal();
        assert_ne!(before, after, "n cycles matches after enter commits");

        // Closing the overlay clears the search state entirely.
        press(&mut app, '?');
        assert!(!app.help_open);
        assert!(app.help_search.is_none());
    }

    /// Space enters the root which-key; a direct bind dispatches and dismisses.
    #[test]
    fn leader_pending_then_dispatch() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        press(&mut app, ' ');
        assert_eq!(app.leader, Some(LeaderState::Root), "space enters root");
        // `<leader>q` quits.
        press(&mut app, 'q');
        assert_eq!(app.leader, None, "dispatch dismisses the popup");
        assert!(app.should_quit);
    }

    /// An unbound root continuation dismisses the popup with no action; Esc too.
    #[test]
    fn leader_unbound_and_esc_dismiss() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        press(&mut app, ' ');
        press(&mut app, 'z'); // unbound at root
        assert_eq!(app.leader, None);
        assert!(!app.should_quit);

        press(&mut app, ' ');
        esc(&mut app);
        assert_eq!(app.leader, None, "esc dismisses at root");
    }

    /// A submenu descent, then a bound key dispatches; Esc backs out one level
    /// then cancels; an unknown key cancels at each level.
    #[test]
    fn leader_submenu_state_machine() {
        let dir = tempfile::tempdir().unwrap();
        // A workspace so RunSequence has something to act on (no panic on empty).
        let mut app = workspace_fixture(dir.path());

        // Space → Root; `s` → Submenu(Sequences).
        press(&mut app, ' ');
        assert_eq!(app.leader, Some(LeaderState::Root));
        press(&mut app, 's');
        assert_eq!(
            app.leader,
            Some(LeaderState::Submenu("sequences".to_owned()))
        );
        // A bound submenu key dispatches and closes the popup.
        press(&mut app, 'r');
        assert_eq!(app.leader, None, "submenu dispatch dismisses");

        // Esc from a submenu backs out to Root; a second Esc cancels.
        press(&mut app, ' ');
        press(&mut app, 's');
        esc(&mut app);
        assert_eq!(
            app.leader,
            Some(LeaderState::Root),
            "esc backs out one level"
        );
        esc(&mut app);
        assert_eq!(app.leader, None, "second esc cancels");

        // Unknown key cancels at Root and inside a submenu.
        press(&mut app, ' ');
        press(&mut app, 'z');
        assert_eq!(app.leader, None);
        press(&mut app, ' ');
        press(&mut app, 's');
        press(&mut app, 'z'); // unbound in the sequences submenu
        assert_eq!(app.leader, None);
    }

    /// `<leader>e` toggles the explorer sidebar (a direct root bind).
    #[test]
    fn leader_e_toggles_explorer() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        assert!(!app.explorer_hidden);
        press(&mut app, ' ');
        press(&mut app, 'e');
        assert!(app.explorer_hidden);
        assert_eq!(app.leader, None);
    }

    /// Leader is inert during text edits: Space types a space in the URL editor,
    /// never entering the which-key state.
    #[test]
    fn leader_inert_during_url_edit() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.explorer.expand().unwrap();
        app.explorer.cursor = 1;
        let selected = app.explorer.select().unwrap().expect("endpoint");
        app.load_endpoint(selected);
        app.begin_url_edit_inline();
        assert!(app.test_url_editor().is_some());
        press(&mut app, ' ');
        assert_eq!(app.leader, None, "space must not enter leader during edit");
        assert!(
            app.test_url_editor().as_ref().unwrap().text().contains(' '),
            "space types a space in the editor"
        );
    }

    /// Leader is inert in edtui insert mode on the Body tab.
    #[test]
    fn leader_inert_in_edtui_insert() {
        let mut app = insert_mode_app(KeyMap::default());
        press(&mut app, ' ');
        assert_eq!(app.leader, None);
        assert_eq!(String::from(app.test_editor().lines.clone()), " ");
    }

    /// The `?` help / `churl keymaps` traversal: the `<leader>s r` chooser
    /// (`RunSequencePick`, D1) is reachable only through the sequences submenu, so
    /// its combo must render as `s r`. Guards the `leader_combos_for`/`iter_leader`
    /// submenu traversal (the help-guard test alone can't catch a missing one).
    #[test]
    fn run_sequence_shows_submenu_chord() {
        let keymap = KeyMap::default();
        assert_eq!(
            keymap.leader_combos_for(Action::RunSequencePick),
            vec!["s r"]
        );
        assert!(
            keymap
                .iter_leader()
                .any(|(_, a)| a == Action::RunSequencePick),
            "iter_leader must traverse the submenu maps"
        );
    }

    // ---- M6.7: digit binds only act in Request ----

    /// Global digits do nothing (no pane focus); inside Request they jump tabs.
    #[test]
    fn digit_focus_removed_at_app_level() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        open_bare_endpoint(&mut app);
        app.focus = Pane::Response;
        press(&mut app, '1'); // was FocusExplorer
        assert_eq!(app.focus, Pane::Response, "digit must not change focus");
        // In the Request pane, 1–4 jump tabs.
        app.focus = Pane::Request;
        press(&mut app, '2');
        assert_eq!(app.test_tabs().active, RequestTab::Headers);
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
        // Names and values are trimmed before decode.
        let (_, pairs) = split_query("https://x/y? a = 1 & b = hello world ");
        assert_eq!(
            pairs,
            vec![
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "hello world".to_owned()),
            ]
        );
        // A key-only segment is trimmed too.
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

    /// Focusing the collapsed counterpart transfers the zoom to it, so exactly
    /// one pane is ever zoomed and a collapsed pane never holds focus (zoom
    /// follows focus).
    #[test]
    fn focus_collapsed_pane_transfers_zoom() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.focus = Pane::Request;
        app.dispatch(Action::Zoom, None).unwrap(); // Response collapsed
        assert_eq!(app.zoom, Some(ZoomPane::Request));
        app.dispatch(Action::FocusResponse, None).unwrap();
        assert_eq!(
            app.zoom,
            Some(ZoomPane::Response),
            "focusing the collapsed pane transfers the zoom to it"
        );
        assert_eq!(app.focus, Pane::Response);
    }

    /// Zoom follows focus in both directions: zoom Request then focus Response
    /// leaves Response zoomed, and the reverse leaves Request zoomed.
    #[test]
    fn zoom_follows_focus_both_directions() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        // Request zoomed → focus Response ⇒ Response zoomed.
        app.focus = Pane::Request;
        app.zoom = Some(ZoomPane::Request);
        app.set_focus(Pane::Response);
        assert_eq!(app.zoom, Some(ZoomPane::Response));
        assert_eq!(app.focus, Pane::Response);
        // Response zoomed → focus Request ⇒ Request zoomed.
        app.zoom = Some(ZoomPane::Response);
        app.set_focus(Pane::Request);
        assert_eq!(app.zoom, Some(ZoomPane::Request));
        assert_eq!(app.focus, Pane::Request);
    }

    /// Jumping (jump-mode) into the collapsed pane transfers the zoom too — jump
    /// dispatch must go through `set_focus`, not assign focus directly
    /// (review round 3, finding #4).
    #[test]
    fn jump_into_collapsed_pane_transfers_zoom() {
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
            app.zoom,
            Some(ZoomPane::Request),
            "jumping into the collapsed pane transfers the zoom to it"
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

    /// Tab cycling skips a collapsed explorer instead of reopening it: it stays
    /// hidden and focus lands on the next visible region (both directions).
    #[test]
    fn tab_skips_hidden_explorer() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.explorer_hidden = true;

        // Forward: Response → (skip Explorer) → UrlBar.
        app.focus = Pane::Response;
        app.dispatch(Action::FocusNext, None).unwrap();
        assert_eq!(app.focus, Pane::UrlBar);
        assert!(app.explorer_hidden, "tab must not reopen the explorer");

        // Backward: UrlBar → (skip Explorer) → Response.
        app.focus = Pane::UrlBar;
        app.dispatch(Action::FocusPrev, None).unwrap();
        assert_eq!(app.focus, Pane::Response);
        assert!(
            app.explorer_hidden,
            "shift-tab must not reopen the explorer"
        );
    }

    // ---- PR 2b: sequences sub-pane ----

    /// A workspace with one collection (one endpoint) and two sequences, for the
    /// sub-pane state-machine tests.
    fn seq_pane_app(root: &Path) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("one.toml"),
            "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
        )
        .unwrap();
        let seq_dir = root.join("sequences");
        std::fs::create_dir(&seq_dir).unwrap();
        for (i, name) in ["Login", "Checkout"].iter().enumerate() {
            std::fs::write(
                seq_dir.join(format!("s{i}.toml")),
                format!("seq = {i}\nname = \"{name}\"\non_error = \"halt\"\n"),
            )
            .unwrap();
        }
        let ws = open_workspace(root).unwrap();
        App::new(ws, KeyMap::default()).unwrap()
    }

    /// A workspace with one collection (one endpoint) and NO sequences dir, for
    /// the note #3 focused-empty-pane tests. Mirrors [`seq_pane_app`] minus the
    /// `sequences/` dir.
    fn empty_seq_pane_app(root: &Path) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("one.toml"),
            "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
        )
        .unwrap();
        let ws = open_workspace(root).unwrap();
        App::new(ws, KeyMap::default()).unwrap()
    }

    /// M7.10 stage B: the Explorer `s` overlay (`focus-sequences-toggle`) flips
    /// `left_active` Endpoints⇄Sequences and focuses the left column. The
    /// sub-pane is always present (peek-symmetric), so nothing ever hides.
    #[test]
    fn focus_sequences_toggle_is_a_focus_switch_never_hides() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        assert_eq!(
            app.left_active,
            LeftPane::Endpoints,
            "Explorer zoomed default"
        );
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences, "focus switched to it");
        assert_eq!(app.focus, Pane::Explorer);
        // Pressing again switches back — peek-symmetry both directions.
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Endpoints);
    }

    /// A jump to the Explorer `e` label from a focused Sequences sub-pane must
    /// land on the endpoints tree, not stay on Sequences (owner drive-test
    /// 2026-07-10). Before the fix, `f e` set focus but never reset
    /// `left_active`, so it appeared to do nothing.
    #[test]
    fn jump_e_lands_on_endpoints_from_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        // Focus the sequences sub-pane first.
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        // Enter jump-mode and press the Explorer `e` label.
        app.dispatch(Action::Jump, None).unwrap();
        assert_eq!(app.mode, Mode::Jump);
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(
            app.left_active,
            LeftPane::Endpoints,
            "`f e` must reach the endpoints tree, not stay on Sequences"
        );
    }

    /// Note #3: an EXPLICIT focus on the empty sequences sub-pane now STICKS —
    /// `set_focus` no longer force-reverts to Endpoints on a zero-length list.
    /// (Previously this asserted the blanket force-Endpoints behavior; that crutch
    /// is gone — the empty pane is focusable and renders an informative empty
    /// state. The reload/switch reconcile path keeps its own guard, exercised by
    /// `reload_emptying_sequences_forces_endpoints`.)
    #[test]
    fn explicit_focus_sticks_on_empty_sequences_pane() {
        // A workspace with an endpoint but zero sequences.
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.left_active = LeftPane::Sequences;
        assert_eq!(app.explorer.sequences_len(), 0);
        app.set_focus(Pane::Explorer);
        assert_eq!(
            app.left_active,
            LeftPane::Sequences,
            "an explicit focus on the empty sequences pane is honored (note #3)"
        );
    }

    /// Note #3: `f s` (the jump `s` label) focuses+zooms the empty sequences
    /// sub-pane and it STICKS — the old empty-list invariant no longer reverts it.
    #[test]
    fn f_jump_s_sticks_on_empty_sequences_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = empty_seq_pane_app(dir.path());
        assert_eq!(app.explorer.sequences_len(), 0);
        app.dispatch(Action::Jump, None).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(
            app.left_active,
            LeftPane::Sequences,
            "`f s` on an empty workspace zooms the sequences pane, not reverts"
        );
    }

    /// Note #3 — NOT a dead-end: from a focused-empty Sequences pane, `Tab`
    /// (FocusNext all the way round) and `f e` both return to the endpoints tree.
    #[test]
    fn focused_empty_sequences_pane_is_not_a_dead_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = empty_seq_pane_app(dir.path());
        // Zoom into the empty sequences pane.
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        // Way out #1: Tab genuinely LEAVES the left column — the first FocusNext
        // moves focus off Explorer (not a no-op that traps you on the empty pane);
        // a full 4-region cycle then returns to it.
        app.dispatch(Action::FocusNext, None).unwrap();
        assert_ne!(
            app.focus,
            Pane::Explorer,
            "Tab must move focus off the empty sequences pane, not trap on it"
        );
        assert_eq!(app.focus, Pane::UrlBar);
        for _ in 0..3 {
            app.dispatch(Action::FocusNext, None).unwrap();
        }
        assert_eq!(app.focus, Pane::Explorer);
        // Way out #2: `f e` jumps back to the endpoints tree.
        app.dispatch(Action::Jump, None).unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.left_active,
            LeftPane::Endpoints,
            "`f e` escapes the empty sequences pane back to endpoints"
        );
        // Way out #3: the `s` overlay toggles straight back too.
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Endpoints);
    }

    /// Note #3 — the add path works from the focused-empty pane: `<leader>s a`
    /// (`EditSequence`) opens the new-sequence prompt (the add entry point) with
    /// zero sequences, and none of the in-pane nav keys panic on an empty list.
    #[test]
    fn empty_sequences_pane_add_and_nav_never_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = empty_seq_pane_app(dir.path());
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        // In-pane nav with an empty list is a no-op — no index-out-of-range panic.
        for action in [
            Action::Down,
            Action::Up,
            Action::Top,
            Action::Bottom,
            Action::Select, // Enter on no selection → new-sequence prompt
        ] {
            app.dispatch(action, None).unwrap();
        }
        // Select (Enter) with no sequence opened the new-sequence name prompt.
        assert_eq!(app.mode, Mode::Prompt(PromptPurpose::NewSequence));
        assert_eq!(app.explorer.seq_cursor(), 0, "cursor stays pinned at 0");
        assert!(app.explorer.selected_sequence().is_none());

        // `<leader>s a` (EditSequence) reaches the same add entry point.
        app.mode = Mode::Normal;
        app.dispatch(Action::EditSequence, None).unwrap();
        assert_eq!(
            app.mode,
            Mode::Prompt(PromptPurpose::NewSequence),
            "<leader>s a opens the add-sequence prompt from the empty pane"
        );
        // `r` (RunSequence) with nothing selected is a safe no-op (no panic).
        app.mode = Mode::Normal;
        app.dispatch(Action::RunSequence, None).unwrap();
    }

    #[test]
    fn tab_cycle_never_changes_left_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        // Full Tab cycle back to the explorer must not touch left_active — Tab
        // lands on the left column and RESTORES its last sub-pane (not Endpoints).
        for _ in 0..4 {
            app.dispatch(Action::FocusNext, None).unwrap();
        }
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(
            app.left_active,
            LeftPane::Sequences,
            "Tab is cross-pane only; it restores the last sub-pane"
        );
    }

    #[test]
    fn explorer_nav_routes_to_seq_cursor_when_sequences_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.explorer.seq_cursor(), 0);
        let tree_cursor = app.explorer.cursor;
        // j/Down moves the sequence cursor, not the tree cursor.
        app.dispatch(Action::Down, None).unwrap();
        assert_eq!(app.explorer.seq_cursor(), 1);
        assert_eq!(app.explorer.cursor, tree_cursor, "tree cursor untouched");
        // Enter opens the unified surface (Edit face) on the hovered sequence.
        app.dispatch(Action::Select, None).unwrap();
        assert_eq!(app.mode, Mode::Sequence);
        assert_eq!(app.sequence_view, SeqView::Edit);
    }

    /// D1: `<leader>s r` (`RunSequencePick`) opens a run-flavored chooser and the
    /// accepted index RUNS the chosen sequence — not sequence #0. Mirrors the
    /// `open_load_runner_pick` tests.
    #[test]
    fn run_sequence_pick_runs_the_chosen_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        let ordering = app.explorer.all_sequences();
        assert!(ordering.len() >= 2, "fixture has ≥2 sequences");

        // `<leader>s r` opens the picker with the run intent armed.
        app.dispatch(Action::RunSequencePick, None).unwrap();
        assert_eq!(app.mode, Mode::SequencePicker);
        assert!(app.sequence_pick_runs, "run intent armed on the picker");

        // Highlight the SECOND sequence and accept it.
        app.picker.as_mut().unwrap().move_down();
        let chosen = ordering[1].0.clone();
        app.accept_overlay().unwrap();

        // The runner opened over the CHOSEN sequence (index 1), not sequences[0].
        assert_eq!(app.mode, Mode::Sequence);
        assert_eq!(app.sequence_view, SeqView::Run);
        let runner = app.sequence_runner.as_ref().expect("runner opened");
        assert_eq!(runner.name, chosen, "ran the chosen sequence, not #0");
        assert!(
            !app.sequence_pick_runs,
            "one-shot intent cleared after accept"
        );
    }

    /// D1: `<leader>s o` (`OpenSequencePicker`) still opens the chosen sequence
    /// for EDITING — the run intent stays false so the accept path edits.
    #[test]
    fn open_sequence_pick_edits_not_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        app.dispatch(Action::OpenSequencePicker, None).unwrap();
        assert_eq!(app.mode, Mode::SequencePicker);
        assert!(!app.sequence_pick_runs, "edit path: run intent not armed");
        app.accept_overlay().unwrap();
        assert_eq!(app.mode, Mode::Sequence);
        assert_eq!(
            app.sequence_view,
            SeqView::Edit,
            "picker opens the Edit face"
        );
    }

    #[test]
    fn r_on_sequences_subpane_runs_hovered_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        // `r` maps to Rename, but on the sequences sub-pane it runs the sequence.
        app.dispatch(Action::Rename, None).unwrap();
        assert_eq!(app.mode, Mode::Sequence);
        assert_eq!(app.sequence_view, SeqView::Run);
        assert!(app.sequence_runner.is_some());
    }

    /// `<leader>s <leader>` (Space) is the canonical sequence finder — it opens
    /// the sequence picker (edit face), mirroring `<leader><leader>` for
    /// endpoints (owner drive-test 2026-07-10).
    #[test]
    fn leader_s_space_opens_the_sequence_picker() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        // Space → Root → `s` → sequences submenu → Space.
        press(&mut app, ' ');
        press(&mut app, 's');
        assert_eq!(
            app.leader,
            Some(LeaderState::Submenu("sequences".to_owned()))
        );
        press(&mut app, ' ');
        assert_eq!(
            app.mode,
            Mode::SequencePicker,
            "s <leader> opens the sequence picker"
        );
        assert!(
            !app.sequence_pick_runs,
            "s <leader> is the open/edit finder, not run"
        );
        assert_eq!(app.leader, None, "submenu dispatch dismisses the popup");
    }

    /// M7.10 dynamic submenus, end-to-end at the app layer: a config
    /// `g = "+git"` + `[keys.leader.git]` wires a brand-new submenu that shows in
    /// which-key and whose keys dispatch through the leader state machine.
    #[test]
    fn dynamic_leader_submenu_dispatches_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let ws = open_workspace(dir.path()).unwrap();
        // A config-defined `git` submenu reached via `<leader>g`, binding `t` to a
        // globally-observable action (toggle the explorer sidebar).
        let global = BTreeMap::new();
        let overlays = BTreeMap::from([
            (
                "leader".to_string(),
                BTreeMap::from([("g".to_string(), "+git".to_string())]),
            ),
            (
                "leader.git".to_string(),
                BTreeMap::from([("t".to_string(), "toggle-explorer".to_string())]),
            ),
        ]);
        let keymap = KeyMap::with_all_overrides(&global, &overlays).unwrap();
        let mut app = App::new(ws, keymap).unwrap();

        // which-key: the root popup lists the `git` submenu behind `g`.
        let (_, root_entries) = leader_popup_entries(&app, &LeaderState::Root);
        assert!(
            root_entries.iter().any(|(k, l)| k == "g" && l == "▸ git"),
            "root which-key shows the config submenu: {root_entries:?}"
        );
        // which-key: inside the submenu, `t` shows its action label.
        let (title, sub_entries) =
            leader_popup_entries(&app, &LeaderState::Submenu("git".to_owned()));
        assert_eq!(title, " leader · git ");
        assert!(
            sub_entries
                .iter()
                .any(|(k, l)| k == "t" && l == "toggle explorer sidebar"),
            "submenu which-key shows its bind: {sub_entries:?}"
        );

        // Dispatch through the real key path: Space → g → t toggles the explorer.
        let before = app.explorer_hidden;
        press(&mut app, ' ');
        press(&mut app, 'g');
        assert_eq!(app.leader, Some(LeaderState::Submenu("git".to_owned())));
        press(&mut app, 't');
        assert_eq!(app.leader, None, "submenu dispatch dismisses");
        assert_ne!(app.explorer_hidden, before, "the git submenu action ran");
    }

    /// Deliverable #5 (verify): every "pick"-style leader action opens a picker
    /// Mode and never silently runs/executes on entry. Guards against a run-last
    /// regression sneaking into any of the four picker front doors.
    #[test]
    fn pickers_open_a_mode_never_run_last() {
        // Sequence pickers (open + run-pick) both open `Mode::SequencePicker`.
        let d1 = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(d1.path());
        app.dispatch(Action::OpenSequencePicker, None).unwrap();
        assert_eq!(app.mode, Mode::SequencePicker, "s o/s f open a picker");
        assert!(app.sequence_runner.is_none(), "no sequence ran on entry");

        let d2 = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(d2.path());
        app.dispatch(Action::RunSequencePick, None).unwrap();
        assert_eq!(app.mode, Mode::SequencePicker, "s r opens a picker");
        assert!(app.sequence_runner.is_none(), "no sequence ran on entry");

        // Endpoint request picker (`<leader>f`) opens the search overlay.
        let d3 = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(d3.path());
        app.dispatch(Action::QuickJumpRequests, None).unwrap();
        assert_eq!(
            app.mode,
            Mode::Search,
            "<leader>f opens the endpoint picker"
        );

        // Load-runner pick (`<leader>l f`) opens the endpoint picker first,
        // arming the after-pick hand-off — it never fires a load on entry.
        let d4 = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(d4.path());
        app.dispatch(Action::OpenLoadRunnerPick, None).unwrap();
        assert_eq!(
            app.mode,
            Mode::Search,
            "<leader>l f opens the endpoint picker"
        );
        assert!(app.load_runner.is_none(), "no load runner opened on entry");
    }

    #[test]
    fn leader_e_hide_restores_prior_focus() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        // Focus Response, then Tab-cycle into the explorer (records prior focus).
        app.set_focus(Pane::Response);
        app.set_focus(Pane::Explorer);
        assert_eq!(app.focus, Pane::Explorer);
        // Hide: focus restores to Response (owner #2B — not the URL-bar fallback).
        app.dispatch(Action::ToggleExplorer, None).unwrap();
        assert!(app.explorer_hidden);
        assert_eq!(app.focus, Pane::Response, "true prior-pane restore");
        // Show: re-focus the explorer, landing on the active sub-pane.
        app.left_active = LeftPane::Endpoints; // (default; explicit for clarity)
        app.dispatch(Action::ToggleExplorer, None).unwrap();
        assert!(!app.explorer_hidden);
        assert_eq!(app.focus, Pane::Explorer);
    }

    #[test]
    fn leader_e_hide_falls_back_to_urlbar_without_prior() {
        // No recorded prior pane (explorer focused from the start) → URL bar.
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        assert_eq!(app.focus, Pane::Explorer);
        app.dispatch(Action::ToggleExplorer, None).unwrap();
        assert!(app.explorer_hidden);
        assert_eq!(app.focus, Pane::UrlBar);
    }

    #[test]
    fn z_still_noop_from_explorer() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(app.zoom, None);
        app.dispatch(Action::Zoom, None).unwrap();
        assert_eq!(app.zoom, None, "z zooms only Request/Response");
    }

    /// B1 regression: with the sequences sub-pane focused, `d`/`N` must NOT open
    /// a tree delete/new-collection prompt targeting the off-screen tree cursor
    /// (which sits on collection "api"); `n` opens the new-SEQUENCE prompt, not a
    /// new-endpoint one. Fails pre-fix (d→delete-collection confirm, n→new
    /// endpoint under the invisible collection).
    #[test]
    fn tree_mutating_keys_suppressed_on_sequences_subpane() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        // Put the tree cursor on the collection so `d`/`n`/`N` would target it.
        app.explorer.cursor = 0;
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert!(app.left_column_on_sequences());

        // `d` → the delete-SEQUENCE confirm (never the tree delete prompt).
        app.dispatch(Action::Delete, None).unwrap();
        assert_eq!(
            app.mode,
            Mode::Confirm(ConfirmPurpose::DeleteSequence),
            "d on the sequences sub-pane confirms a sequence delete, not a tree delete"
        );
        // Back out so the rest of the assertions run from Normal.
        app.dispatch(Action::Cancel, None).ok();
        app.mode = Mode::Normal;

        // `N` → no new-collection prompt; mode stays Normal.
        app.dispatch(Action::NewCollection, None).unwrap();
        assert_eq!(
            app.mode,
            Mode::Normal,
            "N must not open a new-collection prompt"
        );

        // `n` → the new-SEQUENCE prompt (not new-endpoint).
        app.dispatch(Action::NewEndpoint, None).unwrap();
        assert_eq!(
            app.mode,
            Mode::Prompt(PromptPurpose::NewSequence),
            "n on the sequences sub-pane creates a new sequence"
        );
    }

    /// Note #6: `d` → confirm → `y` on the sequences sub-pane deletes the hovered
    /// sequence file, refreshes the pane, and the sub-pane cursor lands on a
    /// surviving sequence. Mirrors the endpoint-delete confirm flow.
    #[test]
    fn d_confirm_y_deletes_hovered_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert!(app.left_column_on_sequences());
        assert_eq!(app.explorer.sequences_len(), 2);

        // Cursor 0 → "Login" (seq = 0). Capture its file before deleting.
        let selected = app.explorer.selected_sequence().unwrap();
        assert_eq!(selected.name, "Login");
        let deleted_path = selected.file.clone();
        assert!(deleted_path.exists());

        // `d` opens the confirm, `y` performs the delete.
        app.dispatch(Action::Delete, None).unwrap();
        assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DeleteSequence));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.mode, Mode::Normal, "confirm closes on y");
        assert!(!deleted_path.exists(), "sequence file removed from disk");
        assert_eq!(app.explorer.sequences_len(), 1, "pane refreshed");
        // Selection lands sensibly on the surviving sequence.
        assert_eq!(app.explorer.selected_sequence().unwrap().name, "Checkout");
    }

    /// Note #6, empty-after-delete case: deleting the last remaining sequence
    /// leaves an empty sub-pane and reconciles the left-column focus back to the
    /// endpoints tree (never stranded on an empty sub-pane).
    #[test]
    fn deleting_last_sequence_empties_pane_and_reconciles_focus() {
        let dir = tempfile::tempdir().unwrap();
        // A workspace with exactly one sequence.
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = dir.path().join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("one.toml"),
            "seq = 0\nname = \"one\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/one\"\n",
        )
        .unwrap();
        let seq_dir = dir.path().join("sequences");
        std::fs::create_dir(&seq_dir).unwrap();
        std::fs::write(
            seq_dir.join("only.toml"),
            "seq = 0\nname = \"Only\"\non_error = \"halt\"\n",
        )
        .unwrap();
        let ws = open_workspace(dir.path()).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();

        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        assert_eq!(app.explorer.sequences_len(), 1);
        let only = app.explorer.selected_sequence().unwrap().file;

        app.dispatch(Action::Delete, None).unwrap();
        assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DeleteSequence));
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();

        assert!(!only.exists(), "last sequence file removed");
        assert_eq!(app.explorer.sequences_len(), 0, "pane now empty");
        assert!(app.explorer.selected_sequence().is_none());
        // Focus reconciled off the empty sub-pane.
        assert_eq!(
            app.left_active,
            LeftPane::Endpoints,
            "focus reconciles to endpoints when the sequence list empties"
        );
    }

    /// Note #6: `d` on the sequences sub-pane with no sequence selected notifies
    /// and never opens a confirm.
    #[test]
    fn d_on_empty_sequences_subpane_notifies() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = empty_seq_pane_app(dir.path());
        app.left_active = LeftPane::Sequences;
        app.focus = Pane::Explorer;
        assert!(app.explorer.selected_sequence().is_none());

        app.dispatch(Action::Delete, None).unwrap();
        assert_eq!(
            app.mode,
            Mode::Normal,
            "no confirm when nothing is selected"
        );
    }

    /// The same tree-mutating keys keep their normal behaviour on the endpoints
    /// tree (guard is scoped to `left_active==Sequences`).
    #[test]
    fn tree_mutating_keys_work_normally_on_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        // Sub-pane shown but endpoints active (the default occupant).
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        app.dispatch(Action::FocusSequencesToggle, None).unwrap(); // → Endpoints
        assert_eq!(app.left_active, LeftPane::Endpoints);
        app.explorer.cursor = 0; // on collection "api"

        app.dispatch(Action::Delete, None).unwrap();
        assert_eq!(
            app.mode,
            Mode::Prompt(PromptPurpose::DeleteCollectionConfirm),
            "d still deletes the selected collection on the endpoints tree"
        );
    }

    /// M1 regression: switching from a sequenced workspace (sub-pane on, sequences
    /// focused) into a sequence-less one resets to endpoints — focus is never
    /// stranded on an empty sub-pane and B1 is disarmed. Fails pre-fix
    /// (`left_active` stays `Sequences` with an empty list).
    #[test]
    fn workspace_switch_into_sequenceless_resets_to_endpoints() {
        let dir_a = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir_a.path());
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);

        // A second workspace with NO sequences.
        let dir_b = tempfile::tempdir().unwrap();
        std::fs::write(dir_b.path().join("churl.toml"), "name = \"empty\"\n").unwrap();
        let coll = dir_b.path().join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("x.toml"),
            "seq = 0\nname = \"x\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/x\"\n",
        )
        .unwrap();

        app.switch_workspace(dir_b.path().to_path_buf()).unwrap();
        assert_eq!(app.explorer.sequences_len(), 0);
        // A workspace switch is a clean slate: `switch_workspace` always lands on
        // the endpoints tree (it sets `left_active = Endpoints` directly), so the
        // old workspace's Sequences focus never carries into the new one.
        assert_eq!(
            app.left_active,
            LeftPane::Endpoints,
            "focus must not strand on an empty sequences pane"
        );
        assert!(!app.left_column_on_sequences(), "B1 disarmed");
    }

    /// M1 regression (reload path): the invariant forces Endpoints when a reload
    /// empties the sequence list even without a full workspace switch.
    #[test]
    fn reload_emptying_sequences_forces_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        // Delete every sequence file on disk, then reload.
        std::fs::remove_dir_all(dir.path().join("sequences")).unwrap();
        app.reload_explorer().unwrap();
        assert_eq!(app.explorer.sequences_len(), 0);
        assert_eq!(
            app.left_active,
            LeftPane::Endpoints,
            "an emptied sub-pane never keeps focus"
        );
    }

    // ---- M7.10 stage B: 4-region Tab, cycle-region, f-jump, hover-fallback ----

    /// B1: Tab cycles the 4 regions Explorer→UrlBar→Request→Response→Explorer, and
    /// landing back on the left column RESTORES its last sub-pane (Sequences),
    /// never force-resetting to Endpoints.
    #[test]
    fn tab_cycles_four_regions_and_restores_left_sub_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        // Put the left column on Sequences, then tab away and all the way back.
        app.dispatch(Action::FocusSequencesToggle, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        assert_eq!(app.focus, Pane::Explorer);
        let order = [Pane::UrlBar, Pane::Request, Pane::Response, Pane::Explorer];
        for expected in order {
            app.dispatch(Action::FocusNext, None).unwrap();
            assert_eq!(app.focus, expected);
        }
        assert_eq!(
            app.left_active,
            LeftPane::Sequences,
            "landing on the left column restores its last sub-pane, not Endpoints"
        );
    }

    /// B1: Tab into the left column restores the Request/Response zoom too — the
    /// zoom-follows-focus invariant survives the 4-region model.
    #[test]
    fn tab_restores_request_response_zoom() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        // Zoom Response, then Tab forward one step: Response→Explorer keeps the
        // Response zoom parked (it only transfers between Request⇄Response).
        app.set_focus(Pane::Response);
        app.dispatch(Action::Zoom, None).unwrap();
        assert_eq!(app.zoom, Some(ZoomPane::Response));
        app.dispatch(Action::FocusNext, None).unwrap(); // → Explorer
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(app.zoom, Some(ZoomPane::Response), "zoom parked across Tab");
        // Tab to Request: zoom-follows-focus flips it to Request (collapsed pane
        // never holds focus).
        app.dispatch(Action::FocusNext, None).unwrap(); // → UrlBar
        app.dispatch(Action::FocusNext, None).unwrap(); // → Request
        assert_eq!(app.focus, Pane::Request);
        assert_eq!(app.zoom, Some(ZoomPane::Request));
    }

    /// B2: `cycle-region-fwd`/`back` cycles the left column's sub-panes when the
    /// left column is focused (Endpoints⇄Sequences), and the buffer/tab ring when
    /// a right-column pane is focused.
    #[test]
    fn cycle_region_switches_sub_pane_on_left_column() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        app.set_focus(Pane::Explorer);
        assert_eq!(app.left_active, LeftPane::Endpoints);
        app.dispatch(Action::CycleRegionFwd, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Sequences);
        app.dispatch(Action::CycleRegionBack, None).unwrap();
        assert_eq!(app.left_active, LeftPane::Endpoints);
    }

    /// B2: `cycle-region` ships with NO default key binding (Ctrl-Tab is
    /// deliberately not hardcoded — terminal-unreliable). It must be unbound in
    /// the global map, every pane overlay AND the leader tree.
    #[test]
    fn cycle_region_is_unbound_by_default() {
        let keymap = KeyMap::default();
        for action in [Action::CycleRegionFwd, Action::CycleRegionBack] {
            assert!(
                keymap.combos_for(action).is_empty(),
                "{action:?} must have no global default"
            );
            assert!(
                keymap.leader_combos_for(action).is_empty(),
                "{action:?} must have no leader default"
            );
            for ctx in [
                PaneCtx::Explorer,
                PaneCtx::UrlBar,
                PaneCtx::Request,
                PaneCtx::Response,
            ] {
                assert!(
                    keymap.overlay_combos_for(ctx, action).is_empty(),
                    "{action:?} must have no {ctx:?} overlay default"
                );
            }
        }
    }

    /// B4: `f`-jump labels the five pane regions only (no rows) and the Sequences
    /// label focuses the left column on the sequences sub-pane.
    #[test]
    fn f_jump_reaches_the_sequences_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        // Enter jump-mode and verify the five region labels are assigned.
        app.dispatch(Action::Jump, None).unwrap();
        let jump = app.jump.as_ref().expect("jump-mode active");
        assert_eq!(jump.label_for_pane(Pane::Explorer), Some('e'));
        assert_eq!(jump.label_for_sequences(), Some('s'));
        assert_eq!(jump.label_for_pane(Pane::UrlBar), Some('u'));
        assert_eq!(jump.label_for_pane(Pane::Request), Some('r'));
        assert_eq!(jump.label_for_pane(Pane::Response), Some('p'));
        // Pressing `s` jumps to the sequences sub-pane.
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal, "a label jump closes jump-mode");
        assert_eq!(app.focus, Pane::Explorer);
        assert_eq!(app.left_active, LeftPane::Sequences);
    }

    /// A workspace with one collection holding two endpoints (distinct URLs), for
    /// the hover-vs-selection fallback tests.
    fn hover_fallback_ws(root: &Path) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("a.toml"),
            "seq = 0\nname = \"alpha\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/alpha\"\n",
        )
        .unwrap();
        std::fs::write(
            coll.join("b.toml"),
            "seq = 1\nname = \"beta\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/beta\"\n",
        )
        .unwrap();
        let ws = open_workspace(root).unwrap();
        App::new(ws, KeyMap::default()).unwrap()
    }

    /// B5: copy-as-curl falls back to the HOVERED endpoint when nothing is loaded.
    #[test]
    fn copy_as_curl_falls_back_to_hovered_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hover_fallback_ws(dir.path());
        // Hover the first endpoint with nothing loaded.
        app.explorer.expand().unwrap();
        app.explorer.move_down(); // onto "alpha"
        app.set_focus(Pane::Explorer);
        assert!(app.selected().is_none(), "no buffer loaded");
        assert!(
            app.hovered_endpoint().is_some(),
            "cursor hovers an endpoint"
        );
        app.copy_as_curl(false);
        let job = app.pending_clipboard.as_ref().expect("copy enqueued a job");
        assert!(
            job.payload.contains("api.test/alpha"),
            "copy fell back to the hovered endpoint's request: {}",
            job.payload
        );
    }

    /// B5: with an endpoint LOADED, copy-as-curl prefers the loaded one even when
    /// the explorer cursor hovers a different endpoint.
    #[test]
    fn copy_as_curl_prefers_loaded_over_hover() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = hover_fallback_ws(dir.path());
        app.explorer.expand().unwrap();
        app.explorer.move_down(); // onto "alpha"
        // Load "alpha" into a buffer.
        let alpha = app.explorer.select().unwrap().expect("alpha selected");
        app.load_endpoint(alpha);
        assert!(app.selected().is_some(), "alpha loaded");
        // Move the cursor onto "beta" so the hover differs from the load.
        app.set_focus(Pane::Explorer);
        app.explorer.move_down(); // onto "beta"
        assert!(
            app.hovered_endpoint()
                .is_some_and(|h| h.endpoint.request.url.contains("beta")),
            "cursor now hovers beta"
        );
        app.copy_as_curl(false);
        let job = app.pending_clipboard.as_ref().expect("copy enqueued a job");
        assert!(
            job.payload.contains("api.test/alpha") && !job.payload.contains("api.test/beta"),
            "copy prefers the loaded endpoint (alpha) over the hover (beta): {}",
            job.payload
        );
    }

    /// m1 regression: `<leader>s o` picking a sequence moves the sub-pane cursor
    /// onto it, so a later run/edit acts on the PICKED sequence, not #0.
    #[test]
    fn picking_a_sequence_sets_seq_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = seq_pane_app(dir.path());
        // Sequences sort by seq: "Login" (0), "Checkout" (1). Pick #1's file.
        let (_, checkout_file) = app
            .explorer
            .all_sequences()
            .into_iter()
            .find(|(name, _)| name == "Checkout")
            .unwrap();
        app.open_picked_sequence(checkout_file).unwrap();
        assert_eq!(app.explorer.seq_cursor(), 1);
        assert_eq!(app.explorer.selected_sequence().unwrap().name, "Checkout");
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
        assert!(app.test_url_popup().is_some());
        *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("https://api.test/x?q=1")));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.test_url_popup().is_none(), "enter commits + closes");
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
        assert!(app.test_url_popup().is_none());
        assert_eq!(app.live_request().unwrap().url, before, "url unchanged");
    }

    #[test]
    fn url_popup_single_line_constraint() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir.path());
        app.begin_url_popup();
        // A buffer that somehow holds two lines collapses to one on commit.
        *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("https://a/b\nc")));
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
        *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("https://api.test/find")));
        // `/` enters Search; type "find"; Enter runs FindFirst → Normal.
        for c in "/find".chars() {
            press(&mut app, c);
        }
        assert_eq!(
            app.test_url_popup().as_ref().unwrap().mode,
            EditorMode::Search
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let popup = app.test_url_popup().as_ref().expect("popup still open");
        assert_eq!(popup.mode, EditorMode::Normal, "search left Search mode");
        assert_eq!(popup.cursor.col, 17, "cursor jumped to the 'find' match");
        // A second Enter (now in Normal) commits.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.test_url_popup().is_none(), "second enter commits");
    }

    /// The vim motion extensions move the popup cursor in Normal mode.
    #[test]
    fn url_popup_vim_motions_move_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir.path());
        app.begin_url_popup();
        *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("foo bar baz")));
        let cursor = |app: &App| app.test_url_popup().as_ref().unwrap().cursor.col;

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
        *app.test_url_popup_mut() = Some(EditorState::new(Lines::from("foo bar")));
        press(&mut app, 'f'); // pending find…
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(
            app.test_url_popup().is_some(),
            "esc aborted the find, not the popup"
        );
        // The find is gone: a char is typed-through to edtui, not a target.
        press(&mut app, 'b');
        assert_eq!(
            app.test_url_popup().as_ref().unwrap().cursor.col,
            0,
            "aborted find must not resolve on the next char"
        );
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(
            app.test_url_popup().is_none(),
            "esc with no pending cancels"
        );
    }

    /// Body tab in Normal mode: `W` moves the editor cursor, and `f`+char is
    /// find-char inside the editor — it does NOT open jump-mode.
    #[test]
    fn body_tab_vim_motions_and_f_shadows_jump() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        open_bare_endpoint(&mut app);
        app.focus = Pane::Request;
        app.test_tabs().active = RequestTab::Body;
        *app.test_editor() = EditorState::new(Lines::from("foo bar baz"));
        app.test_editor().mode = EditorMode::Normal;

        press(&mut app, 'W');
        assert_eq!(app.test_editor().cursor.col, 4, "W moved the body cursor");

        press(&mut app, 'f');
        press(&mut app, 'z');
        assert_eq!(
            app.test_editor().cursor.col,
            10,
            "f<c> moved the body cursor"
        );
        assert!(app.jump.is_none(), "f shadowed jump inside the Body editor");
        assert_eq!(app.test_editor().mode, EditorMode::Normal);
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
        assert!(app.test_url_editor().is_some());
        assert!(app.test_url_popup().is_none());
        // Popup mode: begin_url_edit opens the popup.
        let dir2 = tempfile::tempdir().unwrap();
        let mut app = app_with_endpoint(dir2.path());
        app.set_url_edit_mode(UrlEditMode::Popup);
        app.begin_url_edit();
        assert!(app.test_url_popup().is_some());
        assert!(app.test_url_editor().is_none());
    }

    // ---- M6.7: help overlay ----

    #[test]
    fn help_opens_and_closes() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.dispatch(Action::Help, None).unwrap();
        assert!(app.help_open);
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

        // Fresh fixture with dev active, to keep state clean.
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

    /// `<leader><leader>` (Space Space) reuses the endpoint-search overlay as
    /// the request picker (owner drive-test 2026-07-10 — moved off `<leader>f`).
    #[test]
    fn leader_leader_opens_request_picker() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir.path());
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.leader,
            Some(LeaderState::Root),
            "space enters the root which-key state"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.mode,
            Mode::Search,
            "<leader><leader> opens the search overlay"
        );
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
        assert!(app.selected().is_some());
        *app.test_editor() = EditorState::new(Lines::from("dirty body"));
        app.active_profile = Some("dev".to_owned());
        app.focus = Pane::Response;
        *app.response_mut() = ResponseState::Cancelled;

        let dir_b = tempfile::tempdir().unwrap();
        other_workspace_fixture(dir_b.path());
        let path_b = dir_b.path().to_path_buf();

        app.switch_workspace(path_b.clone()).unwrap();

        // Workspace + explorer now reflect B.
        assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "other");
        let names: Vec<String> = app.explorer.rows().iter().map(|r| r.name.clone()).collect();
        assert!(names.iter().any(|n| n == "orders"), "shows B: {names:?}");
        assert!(!names.iter().any(|n| n == "users"), "no A rows: {names:?}");

        // Endpoint/workspace-scoped state is reset: the buffer (which owns the
        // editor/response/dirty state) is dropped entirely.
        assert!(app.selected().is_none());
        assert!(
            app.test_no_snapshot(),
            "buffer dropped → editor/response gone"
        );
        assert!(app.active_profile.is_none());
        assert_eq!(app.explorer.cursor, 0);
        assert_eq!(app.focus, Pane::Explorer);
        assert!(matches!(app.response(), ResponseState::Idle));
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
        *app.test_editor() = EditorState::new(Lines::from("unsaved edit"));
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
        assert!(app.selected().is_none());
    }

    /// A two-endpoint workspace whose files exist on disk, so `save_request`
    /// (which writes TOML) succeeds. Opens both endpoints as buffers (`alpha`
    /// active last).
    fn two_endpoint_app(root: &Path) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        for name in ["alpha", "beta"] {
            std::fs::write(
                coll.join(format!("{name}.toml")),
                format!(
                    "seq = 0\nname = \"{name}\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{name}\"\n"
                ),
            )
            .unwrap();
        }
        let ws = open_workspace(root).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        app.guarded_load(PendingLoad::File(coll.join("beta.toml")))
            .unwrap();
        app.guarded_load(PendingLoad::File(coll.join("alpha.toml")))
            .unwrap();
        app
    }

    /// M1: a workspace switch with MULTIPLE dirty buffers. `s` must save EVERY
    /// dirty buffer (not just the active one) before the switch destroys them all
    /// — a non-active dirty buffer must never be lost silently.
    #[test]
    fn workspace_switch_s_saves_all_dirty_buffers() {
        let dir_a = tempfile::tempdir().unwrap();
        let mut app = two_endpoint_app(dir_a.path());
        app.history = Some(HistoryStore::in_memory().unwrap());
        // Dirty BOTH buffers via a saveable body edit.
        let alpha_idx = app
            .buffer_index_for_path(&dir_a.path().join("api/alpha.toml"))
            .unwrap();
        let beta_idx = app
            .buffer_index_for_path(&dir_a.path().join("api/beta.toml"))
            .unwrap();
        app.active = beta_idx;
        *app.test_editor() = EditorState::new(Lines::from("beta-body"));
        app.active = alpha_idx;
        *app.test_editor() = EditorState::new(Lines::from("alpha-body"));
        assert!(app.any_buffer_dirty());

        let dir_b = tempfile::tempdir().unwrap();
        other_workspace_fixture(dir_b.path());
        app.guarded_load(PendingLoad::Workspace(dir_b.path().to_path_buf()))
            .unwrap();
        assert_eq!(
            app.mode,
            Mode::Confirm(ConfirmPurpose::DiscardChanges),
            "multi-dirty switch reaches the confirm"
        );

        // `s`: save all, then switch (all clean).
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.workspace.as_ref().unwrap().manifest().name, "other");
        // BOTH files were written — the non-active buffer (beta) was not lost.
        let alpha = std::fs::read_to_string(dir_a.path().join("api/alpha.toml")).unwrap();
        let beta = std::fs::read_to_string(dir_a.path().join("api/beta.toml")).unwrap();
        assert!(alpha.contains("alpha-body"), "active saved: {alpha}");
        assert!(
            beta.contains("beta-body"),
            "the NON-active dirty buffer was saved too: {beta}"
        );
    }

    /// M1: the workspace-switch guard fires even when ONLY a NON-active buffer is
    /// dirty (the guard uses `any_buffer_dirty`, not active-only).
    #[test]
    fn workspace_switch_guards_on_nonactive_dirty() {
        let dir_a = tempfile::tempdir().unwrap();
        let mut app = two_endpoint_app(dir_a.path());
        app.history = Some(HistoryStore::in_memory().unwrap());
        // Dirty ONLY the non-active buffer (beta); leave the active one (alpha)
        // clean.
        let beta_idx = app
            .buffer_index_for_path(&dir_a.path().join("api/beta.toml"))
            .unwrap();
        let alpha_idx = app
            .buffer_index_for_path(&dir_a.path().join("api/alpha.toml"))
            .unwrap();
        app.active = beta_idx;
        *app.test_editor() = EditorState::new(Lines::from("beta-edit"));
        app.active = alpha_idx;
        assert!(!app.is_dirty(), "active (alpha) is clean");
        assert!(app.any_buffer_dirty(), "but beta is dirty");

        let dir_b = tempfile::tempdir().unwrap();
        other_workspace_fixture(dir_b.path());
        app.guarded_load(PendingLoad::Workspace(dir_b.path().to_path_buf()))
            .unwrap();
        assert_eq!(
            app.mode,
            Mode::Confirm(ConfirmPurpose::DiscardChanges),
            "non-active dirty must still guard the switch"
        );
    }

    /// M1: a REFUSED save during a multi-dirty workspace switch aborts the switch
    /// and keeps every buffer (mirrors the single-buffer save-failure behaviour).
    #[test]
    fn workspace_switch_s_refused_save_aborts_switch() {
        let dir_a = tempfile::tempdir().unwrap();
        let mut app = two_endpoint_app(dir_a.path());
        app.history = Some(HistoryStore::in_memory().unwrap());
        let alpha_idx = app
            .buffer_index_for_path(&dir_a.path().join("api/alpha.toml"))
            .unwrap();
        let beta_idx = app
            .buffer_index_for_path(&dir_a.path().join("api/beta.toml"))
            .unwrap();
        // beta: a saveable edit; alpha: an UNSAVEABLE literal-secret auth.
        app.active = beta_idx;
        *app.test_editor() = EditorState::new(Lines::from("beta-body"));
        app.active = alpha_idx;
        app.selected_mut().unwrap().endpoint.request.auth = Some(churl_core::model::Auth::Bearer {
            token: "ghp_literal_secret".to_owned(),
        });
        assert!(app.any_buffer_dirty());

        let dir_b = tempfile::tempdir().unwrap();
        other_workspace_fixture(dir_b.path());
        app.guarded_load(PendingLoad::Workspace(dir_b.path().to_path_buf()))
            .unwrap();
        assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        // Switch aborted: still on workspace "demo", both buffers kept.
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.workspace.as_ref().unwrap().manifest().name,
            "demo",
            "refused save must abort the switch"
        );
        assert_eq!(app.buffers.len(), 2, "no buffer destroyed");
        assert!(app.any_buffer_dirty(), "the refused buffer stays dirty");
    }

    // ---- M7.5 concurrent-load runner ----

    /// Builds an app with one selected endpoint (targeting `url`) and opens the
    /// load runner over it. Client is `None`, so a run resets rows to Pending and
    /// leaves them (runtime-free); the state machine is exercised by injecting
    /// `on_load_started`/`on_load_result`.
    fn load_app(root: &Path, url: &str) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        std::fs::write(
            coll.join("ping.toml"),
            format!("seq = 0\nname = \"Ping\"\n\n[request]\nmethod = \"GET\"\nurl = \"{url}\"\n"),
        )
        .unwrap();
        let ws = open_workspace(root).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        app.guarded_load(PendingLoad::File(coll.join("ping.toml")))
            .unwrap();
        app.open_load_runner();
        app
    }

    fn load_gen(app: &App) -> u64 {
        app.load_runner.as_ref().unwrap().run_generation
    }

    fn load_status(app: &App, i: usize) -> LoadStatus {
        app.load_runner.as_ref().unwrap().results[i].status.clone()
    }

    #[test]
    fn load_runner_opens_with_resolved_request() {
        let dir = tempfile::tempdir().unwrap();
        let app = load_app(dir.path(), "https://api.test/ping");
        assert_eq!(app.mode, Mode::LoadRunner);
        let runner = app.load_runner.as_ref().unwrap();
        assert_eq!(runner.url, "https://api.test/ping");
        assert_eq!(runner.endpoint_path.as_deref(), Some("api/ping.toml"));
        assert!(!runner.running, "must not auto-run");
        assert!(runner.results.is_empty(), "no rows until a run starts");
        assert!(app.load_request.is_some(), "request resolved once at open");
    }

    // ---- `<leader>l f` pick-then-load-test: dirty-safe + no flag leak ----

    /// A two-endpoint workspace with `alpha` loaded into the request pane.
    fn load_pick_app(root: &Path) -> App {
        std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = root.join("api");
        std::fs::create_dir(&coll).unwrap();
        for name in ["alpha", "beta"] {
            std::fs::write(
                coll.join(format!("{name}.toml")),
                format!(
                    "seq = 0\nname = \"{name}\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/{name}\"\n"
                ),
            )
            .unwrap();
        }
        let ws = open_workspace(root).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        app.guarded_load(PendingLoad::File(coll.join("alpha.toml")))
            .unwrap();
        app
    }

    /// Drives the search picker to the single item matching `query` and accepts it.
    fn pick_search(app: &mut App, query: &str) {
        for c in query.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        app.accept_overlay().unwrap();
    }

    /// STAGE 2: `<leader>l f` with a DIRTY editor + a DIFFERENT picked endpoint no
    /// longer defers — each endpoint has its own buffer, so beta opens as a NEW
    /// buffer (dirty alpha stays open) and the runner opens over beta.
    #[test]
    fn load_runner_pick_dirty_opens_new_buffer_and_runner() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_pick_app(dir.path());
        // Make the loaded `alpha` dirty.
        *app.test_editor() = EditorState::new(Lines::from("dirty body"));
        assert!(app.is_dirty());
        let alpha_file = app.selected().unwrap().file.clone();

        app.open_load_runner_pick().unwrap();
        assert_eq!(app.mode, Mode::Search);
        assert!(app.load_runner_after_pick, "intent armed");
        // Pick the DIFFERENT endpoint (beta).
        pick_search(&mut app, "beta");

        // No confirm — the runner opens over the freshly-focused beta buffer.
        assert_eq!(app.mode, Mode::LoadRunner);
        assert_eq!(
            app.load_runner.as_ref().unwrap().url,
            "https://api.test/beta"
        );
        assert_eq!(app.buffers.len(), 2, "beta pushed as a new buffer");
        assert_eq!(app.selected().unwrap().display_path, "api/beta");
        let alpha_idx = app.buffer_index_for_path(&alpha_file).unwrap();
        assert!(
            app.buffers[alpha_idx].is_dirty(),
            "alpha's unsaved edits are preserved in its own buffer"
        );
        assert!(!app.load_runner_after_pick);
    }

    /// A clean `<leader>l f` pick loads the endpoint AND opens the load runner.
    #[test]
    fn load_runner_pick_clean_loads_and_opens_runner() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_pick_app(dir.path());
        assert!(!app.is_dirty());
        app.open_load_runner_pick().unwrap();
        pick_search(&mut app, "beta");
        assert_eq!(app.mode, Mode::LoadRunner, "clean pick opens the runner");
        // The runner targets the newly-picked endpoint, not the previously loaded one.
        assert_eq!(
            app.load_runner.as_ref().unwrap().url,
            "https://api.test/beta"
        );
        assert!(!app.load_runner_after_pick);
    }

    /// BLOCKER 3a: Esc-cancelling the `<leader>l f` picker clears the one-shot flag
    /// so the NEXT plain `<leader>f` / `/` search does not spuriously open the runner.
    #[test]
    fn load_runner_pick_esc_clears_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_pick_app(dir.path());
        app.open_load_runner_pick().unwrap();
        assert!(app.load_runner_after_pick);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Normal);
        assert!(
            !app.load_runner_after_pick,
            "esc-cancel must clear the one-shot intent"
        );
    }

    /// BLOCKER 3b: an empty-result Enter (nothing selected) clears the flag too.
    #[test]
    fn load_runner_pick_empty_enter_clears_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_pick_app(dir.path());
        app.open_load_runner_pick().unwrap();
        // Type a query that matches nothing → the picker has no current item.
        for c in "zzzznomatch".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        assert!(
            app.picker
                .as_ref()
                .and_then(picker::PickerState::current)
                .is_none(),
            "no current item for a non-matching query"
        );
        app.accept_overlay().unwrap();
        assert!(
            !app.load_runner_after_pick,
            "empty-result Enter must clear the one-shot intent"
        );
        assert!(app.load_runner.is_none());
    }

    #[test]
    fn load_run_injected_results_update_stats_and_finish() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.history = Some(HistoryStore::in_memory().unwrap());
        // total=3, then start (client None → 3 pending rows, running).
        app.load_runner.as_mut().unwrap().cfg.total = 3;
        app.start_load_run();
        let g = load_gen(&app);
        assert!(app.load_runner.as_ref().unwrap().running);
        assert_eq!(load_status(&app, 0), LoadStatus::Pending);

        app.on_load_started(g, 0);
        assert_eq!(load_status(&app, 0), LoadStatus::Running);
        app.on_load_result(g, 0, Ok(ok_resp(200, "a")));
        app.on_load_result(g, 1, Ok(ok_resp(500, "b")));
        assert!(!app.load_runner.as_ref().unwrap().finished);
        app.on_load_result(g, 2, Err("connection refused".to_owned()));

        let runner = app.load_runner.as_ref().unwrap();
        assert!(runner.finished && !runner.running && !runner.cancelled);
        assert_eq!(runner.completed, 3);
        assert_eq!(runner.stats.ok, 1);
        assert_eq!(runner.stats.failed, 1);
        assert_eq!(runner.stats.errored, 1);
        assert_eq!(load_status(&app, 0), LoadStatus::Ok(200));
        assert_eq!(load_status(&app, 1), LoadStatus::Failed(500));
        assert!(matches!(load_status(&app, 2), LoadStatus::Error(_)));

        // Exactly one batch summary row, none in per-endpoint history.
        let batches = app
            .history
            .as_ref()
            .unwrap()
            .recent_load_batches(10)
            .unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].summary.total, 3);
        assert_eq!(batches[0].summary.ok_count, 1);
        assert!(!batches[0].summary.cancelled);
        // mean latency is persisted (ok_resp timings are 5ms; error has none).
        assert_eq!(batches[0].summary.mean_ms, Some(5));
        assert_eq!(app.history.as_ref().unwrap().recent(10).unwrap().len(), 0);
    }

    // ---- note #2: unified runner response viewer (parity + preserved nav) ----

    /// A JSON response carrying a `content-type` header so the viewer defaults to
    /// pretty and JSON folding/sort are available (mirrors a real server response).
    fn json_resp(body: &str) -> Response {
        Response {
            status: 200,
            headers: vec![Header {
                name: "content-type".to_owned(),
                value: "application/json".to_owned(),
                enabled: true,
            }],
            body: body.as_bytes().to_vec(),
            truncated: false,
            timing: Timing {
                connect: None,
                total: Duration::from_millis(5),
            },
        }
    }

    /// Opens the load runner over `ping`, runs one copy that lands a JSON `Done`
    /// response, and focuses the Response region — the setup every parity test
    /// shares. `body` is the raw wire body.
    fn load_app_with_response(root: &Path, body: &str) -> App {
        let mut app = load_app(root, "https://api.test/ping");
        app.load_runner.as_mut().unwrap().cfg.total = 1;
        app.start_load_run();
        let g = load_gen(&app);
        app.on_load_result(g, 0, Ok(json_resp(body)));
        app.load_runner.as_mut().unwrap().focus = load_runner::RunnerFocus::Response;
        app
    }

    /// The selected load-runner row's live `ResponseView`, for white-box asserts.
    fn load_view(app: &App) -> &ResponseView {
        match app
            .load_runner
            .as_ref()
            .unwrap()
            .selected_response()
            .unwrap()
        {
            ResponseState::Done { view } => view,
            other => panic!("selected row is not Done: {other:?}"),
        }
    }

    fn norm(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn shift(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    fn keyc(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Drives one full-UI render through a `TestBackend` so the runner response
    /// geometry (total_rows / viewport height+width) is measured — the cursor-nav
    /// and fold-at-cursor tests need a real geometry, not the zero default.
    fn render_once(app: &mut App) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(116, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, app)).expect("draw");
    }

    /// The active response surface resolves to the runner ONLY when its Response
    /// region is focused — so Config/Results focus never routes response actions.
    #[test]
    fn load_response_surface_gated_on_focus() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app_with_response(dir.path(), "{\"b\":1,\"a\":2}");
        assert_eq!(app.active_response_surface(), ResponseSurface::LoadRunner);
        // Refocus config: a response action must now be a no-op on the runner view.
        app.load_runner.as_mut().unwrap().focus = load_runner::RunnerFocus::ConfigHeader;
        assert_eq!(app.active_response_surface(), ResponseSurface::Main);
        let before = load_view(&app).pretty();
        app.handle_key(norm('p')).unwrap();
        assert_eq!(
            load_view(&app).pretty(),
            before,
            "p on Config focus must not touch the runner view"
        );
    }

    /// `p`/`s`/`#`/`W`/`h` toggle the SELECTED row's `ResponseView` exactly as the
    /// main pane does — proving one shared path, not a re-plumbed subset.
    #[test]
    fn load_response_parity_toggles() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app_with_response(dir.path(), "{\"b\":1,\"a\":2}");

        // p: pretty on by default for JSON → toggles to raw.
        assert!(load_view(&app).pretty());
        app.handle_key(norm('p')).unwrap();
        assert!(!load_view(&app).pretty(), "p toggled pretty off");
        app.handle_key(norm('p')).unwrap();
        assert!(load_view(&app).pretty(), "p toggled pretty back on");

        // s: A→Z key sort on the pretty JSON body.
        assert!(!load_view(&app).sort_keys());
        app.handle_key(norm('s')).unwrap();
        assert!(load_view(&app).sort_keys(), "s toggled sort on");

        // #: line-number gutter (default on) toggles off.
        assert!(load_view(&app).line_numbers());
        app.handle_key(norm('#')).unwrap();
        assert!(!load_view(&app).line_numbers(), "# toggled gutter off");

        // W: wrap.
        assert!(!load_view(&app).wrap());
        app.handle_key(shift('W')).unwrap();
        assert!(load_view(&app).wrap(), "W toggled wrap on");

        // h: headers view.
        assert_eq!(load_view(&app).view_mode(), ViewMode::Body);
        app.handle_key(norm('h')).unwrap();
        assert_eq!(
            load_view(&app).view_mode(),
            ViewMode::Headers,
            "h switched to headers"
        );
    }

    /// `o` folds the region at the RUNNER cursor (not the main pane's), and `O`
    /// folds all — proving fold-at-cursor uses the mode-aware geometry.
    #[test]
    fn load_response_fold_uses_runner_cursor() {
        let dir = tempfile::tempdir().unwrap();
        // A nested object so there is a foldable region.
        let mut app = load_app_with_response(dir.path(), "{\"outer\":{\"x\":1,\"y\":2}}");
        // Render once so the runner geometry (total_rows/viewport) is measured.
        render_once(&mut app);
        let rows_before = load_view(&app).total_display_rows(40);
        // Cursor at the opener line (row 0 is `{`, row 1 is `"outer": {`).
        app.load_runner.as_mut().unwrap().geometry.cursor = 1;
        app.handle_key(norm('o')).unwrap();
        let rows_after = load_view(&app).total_display_rows(40);
        assert!(
            rows_after < rows_before,
            "fold at the runner cursor collapsed rows ({rows_before} → {rows_after})"
        );
    }

    /// `/` opens body search over the RUNNER response, `n` steps matches, and Esc
    /// returns to the runner (NOT to Normal mode — the runner stays open).
    #[test]
    fn load_response_search_targets_runner_and_returns() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app_with_response(dir.path(), "{\"needle\":\"needle\"}");
        render_once(&mut app);
        app.handle_key(norm('/')).unwrap();
        assert_eq!(app.mode, Mode::BodySearch);
        assert_eq!(
            app.body_search_return,
            Mode::LoadRunner,
            "search remembers to return to the runner"
        );
        for c in "needle".chars() {
            app.handle_key(norm(c)).unwrap();
        }
        assert!(
            load_view(&app).search().is_some_and(|s| s.count() >= 2),
            "search found matches in the runner view"
        );
        app.handle_key(keyc(KeyCode::Esc)).unwrap();
        assert_eq!(app.mode, Mode::LoadRunner, "Esc returns to the runner");
    }

    /// Horizontal pan (`L`) pans the runner view's `h_scroll`, and copy (`y`/`Y`)
    /// returns the BYTE-EXACT raw wire bytes (M7.7 invariant holds in the runner).
    #[test]
    fn load_response_hscroll_and_byte_exact_copy() {
        let dir = tempfile::tempdir().unwrap();
        // A raw body with a tab + control byte: pretty-off so copy sees wire bytes.
        let raw = "{\"k\":\"a\tb\"}";
        let mut app = load_app_with_response(dir.path(), raw);
        // Turn pretty off so the displayed text is the sanitized raw (tab expanded)
        // while copy must still return the exact tab byte.
        app.handle_key(norm('p')).unwrap();
        assert!(!load_view(&app).pretty());

        // L pans the horizontal window right (wrap is off).
        assert_eq!(load_view(&app).h_scroll(), 0);
        app.handle_key(shift('L')).unwrap();
        assert!(load_view(&app).h_scroll() > 0, "L panned h_scroll right");

        // y copies the full raw body byte-exact (the tab survives).
        app.handle_key(norm('y')).unwrap();
        let copied = app.pending_clipboard.as_ref().unwrap().payload.clone();
        assert_eq!(
            copied, raw,
            "y returns byte-exact raw wire bytes (tab intact)"
        );
    }

    // ---- drive-test #4a: honest, yankable failed-response row ----

    /// `y` on a load-runner `Failed` row copies useful text (the error + URL)
    /// via the shared copy handler — NOT a silent no-op. The failed row carries
    /// no `ResponseView`, so this exercises the state-branch fallback.
    #[test]
    fn load_failed_row_y_copies_error_not_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.load_runner.as_mut().unwrap().cfg.total = 1;
        app.start_load_run();
        let g = load_gen(&app);
        app.on_load_result(g, 0, Err("connection refused".to_owned()));
        app.load_runner.as_mut().unwrap().focus = load_runner::RunnerFocus::Response;
        // Sanity: the selected row is Failed (no view to copy).
        assert!(matches!(
            app.active_response(),
            ResponseState::Failed { .. }
        ));

        app.handle_key(norm('y')).unwrap();
        let copied = app
            .pending_clipboard
            .as_ref()
            .expect("y on a Failed row must enqueue a copy (not a no-op)")
            .payload
            .clone();
        assert!(
            copied.contains("connection refused"),
            "copied text carries the error: {copied:?}"
        );
        assert!(
            copied.contains("https://api.test/ping"),
            "copied text carries the request URL: {copied:?}"
        );
    }

    /// Drive-test #4a fold-in: `y` on a `Dropped` (memory-evicted) load row has
    /// no body to copy, but must NOT be a silent no-op — it sets a status message
    /// explaining the body was not retained, and enqueues nothing.
    #[test]
    fn load_dropped_row_y_sets_message_not_silent_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.load_runner.as_mut().unwrap().cfg.total = 1;
        app.start_load_run();
        // Force the selected row into the memory-evicted Dropped state.
        {
            let runner = app.load_runner.as_mut().unwrap();
            runner.results[0].response = ResponseState::Dropped {
                status: 200,
                timing: None,
                size: 1234,
            };
            runner.focus = load_runner::RunnerFocus::Response;
        }
        assert!(matches!(
            app.active_response(),
            ResponseState::Dropped { .. }
        ));

        app.handle_key(norm('y')).unwrap();
        assert!(
            app.pending_clipboard.is_none(),
            "a Dropped row has nothing to copy — nothing is enqueued"
        );
        let msg = app
            .message
            .as_ref()
            .map(|m| m.text.as_str())
            .expect("y on a Dropped row must set a status message (not a no-op)");
        assert!(
            msg.contains("not retained"),
            "message explains the body was evicted: {msg:?}"
        );
        assert!(!msg.is_empty());
    }

    /// `Y` (copy-line) on a `Dropped`/`Failed`/`Idle` row — no view, no line —
    /// also gives feedback instead of a silent no-op.
    #[test]
    fn load_dropped_row_shift_y_sets_message_not_silent_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.load_runner.as_mut().unwrap().cfg.total = 1;
        app.start_load_run();
        {
            let runner = app.load_runner.as_mut().unwrap();
            runner.results[0].response = ResponseState::Dropped {
                status: 200,
                timing: None,
                size: 1234,
            };
            runner.focus = load_runner::RunnerFocus::Response;
        }
        app.response_copy_line();
        assert!(
            app.pending_clipboard.is_none(),
            "no line to copy — nothing enqueued"
        );
        let msg = app.message.as_ref().map(|m| m.text.as_str()).unwrap_or("");
        assert!(
            msg.contains("not retained"),
            "Y on a Dropped row reports why: {msg:?}"
        );
    }

    /// `Y` on a `Failed` row (no view) gives the generic feedback — still not a
    /// silent no-op (drive-test #4a fold-in: `Y` on Failed was silent post-4a).
    #[test]
    fn load_failed_row_shift_y_sets_message_not_silent_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.load_runner.as_mut().unwrap().cfg.total = 1;
        app.start_load_run();
        let g = load_gen(&app);
        app.on_load_result(g, 0, Err("connection refused".to_owned()));
        app.load_runner.as_mut().unwrap().focus = load_runner::RunnerFocus::Response;
        assert!(matches!(
            app.active_response(),
            ResponseState::Failed { .. }
        ));
        app.response_copy_line();
        assert!(
            app.pending_clipboard.is_none(),
            "Y copies a line, not the failure blurb — nothing enqueued"
        );
        let msg = app.message.as_ref().map(|m| m.text.as_str()).unwrap_or("");
        assert_eq!(msg, "nothing to copy", "generic feedback, not silence");
    }

    /// `y` on a sequence-runner `Failed` step copies the error via the SAME
    /// shared handler — full parity with the load runner and main pane.
    #[test]
    fn sequence_failed_step_y_copies_error_not_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = sequence_app(dir.path(), "continue", "");
        let g = runner_gen(&app);
        app.on_sequence_step(g, 0, Err("dns error".to_owned()));
        let runner = app.sequence_runner.as_mut().unwrap();
        runner.selected = 0;
        runner.focus = sequence_runner::RunnerFocus::Response;
        assert!(matches!(
            app.active_response(),
            ResponseState::Failed { .. }
        ));

        app.handle_key(norm('y')).unwrap();
        let copied = app
            .pending_clipboard
            .as_ref()
            .expect("y on a Failed step must enqueue a copy (not a no-op)")
            .payload
            .clone();
        assert!(
            copied.contains("dns error"),
            "copied text carries the error: {copied:?}"
        );
    }

    /// `y` on a MAIN-pane `Failed` response copies the error + method + URL
    /// through the same shared handler (the third unified viewer).
    #[test]
    fn main_failed_response_y_copies_error_and_request() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.buffers = vec![Buffer::endpoint(selected_with("a.toml", None))];
        app.active = 0;
        app.buffers[0].as_endpoint_mut().unwrap().response = ResponseState::Failed {
            error: "connection refused".to_owned(),
            meta: meta(),
        };
        app.set_focus(Pane::Response);
        app.response_copy_view();
        let copied = app
            .pending_clipboard
            .as_ref()
            .expect("y on a Failed main-pane response must enqueue a copy")
            .payload
            .clone();
        assert!(
            copied.contains("connection refused"),
            "carries the error: {copied:?}"
        );
        assert!(
            copied.contains("GET") && copied.contains("https://api.test/x"),
            "carries the request method+URL from meta: {copied:?}"
        );
    }

    /// Unit: `failure_copy_text` yields the request line + error for `Failed`,
    /// and `None` for every other state (so the copy handler falls through only
    /// on a failure). Never fabricates a status/body/timing.
    #[test]
    fn failure_copy_text_shape() {
        let failed = ResponseState::Failed {
            error: "boom".to_owned(),
            meta: meta(),
        };
        let text = failed.failure_copy_text().expect("Failed yields text");
        assert_eq!(text, "GET https://api.test/x\nerror: boom");

        // Non-Failed states have no failure text.
        assert!(ResponseState::Idle.failure_copy_text().is_none());
        assert!(ResponseState::Cancelled.failure_copy_text().is_none());
        assert!(
            ResponseState::Dropped {
                status: 200,
                timing: None,
                size: 0,
            }
            .failure_copy_text()
            .is_none(),
            "Dropped has no body to copy — stays a no-op"
        );

        // A runner meta with an empty method omits it rather than padding.
        let url_only = ResponseState::Failed {
            error: "dns".to_owned(),
            meta: load_result_meta("https://api.test/ping"),
        };
        assert_eq!(
            url_only.failure_copy_text().unwrap(),
            "https://api.test/ping\nerror: dns"
        );
    }

    /// `j`/`k`/`g`/`G`/`Ctrl-D` move the runner Response cursor through the SAME
    /// path the main pane uses (mode-aware geometry), not a local table.
    #[test]
    fn load_response_cursor_nav_shared_path() {
        let dir = tempfile::tempdir().unwrap();
        let body = (0..30)
            .map(|i| format!("\"k{i}\":{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut app = load_app_with_response(dir.path(), &format!("{{{body}}}"));
        render_once(&mut app);
        assert_eq!(app.load_runner.as_ref().unwrap().geometry.cursor, 0);
        app.handle_key(norm('j')).unwrap();
        assert_eq!(
            app.load_runner.as_ref().unwrap().geometry.cursor,
            1,
            "j moved the runner cursor down via the shared path"
        );
        app.handle_key(shift('G')).unwrap();
        let bottom = app.load_runner.as_ref().unwrap().geometry.cursor;
        assert!(bottom > 1, "G jumped to the bottom row");
        app.handle_key(norm('k')).unwrap();
        assert_eq!(
            app.load_runner.as_ref().unwrap().geometry.cursor,
            bottom - 1,
            "k moved up one"
        );
    }

    /// PRESERVED NAV: from Response focus, Tab still cycles regions and Ctrl-R
    /// still re-runs — a response action never eats these runner-owned keys.
    #[test]
    fn load_runner_nav_not_shadowed_by_response() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app_with_response(dir.path(), "{\"a\":1}");
        assert_eq!(
            app.load_runner.as_ref().unwrap().focus,
            load_runner::RunnerFocus::Response
        );
        // Tab cycles Response → ConfigHeader (runner-owned; not a response action).
        app.handle_key(keyc(KeyCode::Tab)).unwrap();
        assert_eq!(
            app.load_runner.as_ref().unwrap().focus,
            load_runner::RunnerFocus::ConfigHeader,
            "Tab still cycles regions"
        );
        // Back to Response; Ctrl-R still (re-)runs the batch (bumps generation).
        app.load_runner.as_mut().unwrap().focus = load_runner::RunnerFocus::Response;
        let g0 = load_gen(&app);
        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(
            load_gen(&app) > g0,
            "Ctrl-R still re-runs from Response focus"
        );
    }

    /// PRESERVED NAV: `q` from Response focus still closes the runner when idle
    /// (a response action never eats it).
    #[test]
    fn load_runner_q_closes_from_response_focus() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app_with_response(dir.path(), "{\"a\":1}");
        // The one-copy batch has finished (not running), so q closes immediately.
        assert!(!app.load_runner.as_ref().unwrap().is_running());
        assert_eq!(
            app.load_runner.as_ref().unwrap().focus,
            load_runner::RunnerFocus::Response
        );
        app.handle_key(norm('q')).unwrap();
        assert!(app.load_runner.is_none(), "q still closes the runner");
        assert_eq!(app.mode, Mode::Normal);
    }

    /// PRESERVED NAV: when Results-focused, `j`/`k` still select rows (the response
    /// nav must not shadow the results list).
    #[test]
    fn load_results_selection_not_shadowed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.load_runner.as_mut().unwrap().cfg.total = 3;
        app.start_load_run();
        let g = load_gen(&app);
        app.on_load_result(g, 0, Ok(json_resp("{\"a\":1}")));
        app.on_load_result(g, 1, Ok(json_resp("{\"b\":2}")));
        app.on_load_result(g, 2, Ok(json_resp("{\"c\":3}")));
        app.load_runner.as_mut().unwrap().focus = load_runner::RunnerFocus::Results;
        app.load_runner.as_mut().unwrap().selected = 0;
        app.handle_key(norm('j')).unwrap();
        assert_eq!(
            app.load_runner.as_ref().unwrap().selected,
            1,
            "j selects the next results row (not response scroll)"
        );
    }

    #[test]
    fn load_stale_result_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.load_runner.as_mut().unwrap().cfg.total = 2;
        app.start_load_run();
        let g = load_gen(&app);
        // A result from a superseded generation must not land.
        app.on_load_result(g + 99, 0, Ok(ok_resp(200, "x")));
        assert_eq!(load_status(&app, 0), LoadStatus::Pending);
        assert_eq!(app.load_runner.as_ref().unwrap().completed, 0);
    }

    #[test]
    fn load_cancel_marks_pending_and_writes_partial_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.history = Some(HistoryStore::in_memory().unwrap());
        app.load_runner.as_mut().unwrap().cfg.total = 4;
        app.start_load_run();
        let g = load_gen(&app);
        app.on_load_result(g, 0, Ok(ok_resp(200, "a")));
        app.cancel_load_run();

        let runner = app.load_runner.as_ref().unwrap();
        assert!(runner.finished && runner.cancelled && !runner.running);
        assert_eq!(load_status(&app, 0), LoadStatus::Ok(200));
        assert!(
            runner.results[1..]
                .iter()
                .all(|r| r.status == LoadStatus::Cancelled),
            "every non-terminal row is cancelled"
        );
        // The generation was bumped so a straggler result is dropped.
        app.on_load_result(g, 1, Ok(ok_resp(200, "late")));
        assert_eq!(load_status(&app, 1), LoadStatus::Cancelled);

        let batches = app
            .history
            .as_ref()
            .unwrap()
            .recent_load_batches(10)
            .unwrap();
        assert_eq!(batches.len(), 1);
        assert!(
            batches[0].summary.cancelled,
            "partial summary marked cancelled"
        );
        assert_eq!(batches[0].summary.ok_count, 1);
    }

    /// D1: a launched-then-cancelled row carries a real time-to-cancel (read out
    /// of `InFlight { started }`), while a never-launched pending row keeps
    /// `timing = None` — no fabricated zero.
    #[test]
    fn load_cancel_records_time_to_cancel_for_launched_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.load_runner.as_mut().unwrap().cfg.total = 3;
        app.start_load_run();
        let g = load_gen(&app);
        // Mark rows 0 and 1 launched (InFlight); leave row 2 never-launched.
        app.on_load_started(g, 0);
        app.on_load_started(g, 1);
        app.cancel_load_run();

        let runner = app.load_runner.as_ref().unwrap();
        assert_eq!(runner.results[0].status, LoadStatus::Cancelled);
        assert!(
            runner.results[0].timing.is_some(),
            "launched-then-cancelled row shows a time-to-cancel"
        );
        assert!(
            runner.results[1].timing.is_some(),
            "second launched row also times"
        );
        assert_eq!(runner.results[2].status, LoadStatus::Cancelled);
        assert!(
            runner.results[2].timing.is_none(),
            "never-launched pending row keeps timing None (no fabricated zero)"
        );
    }

    #[test]
    fn load_rerun_while_running_writes_cancelled_summary_and_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.history = Some(HistoryStore::in_memory().unwrap());
        app.load_runner.as_mut().unwrap().cfg.total = 4;
        app.start_load_run();
        let g1 = load_gen(&app);
        app.on_load_result(g1, 0, Ok(ok_resp(200, "a")));
        assert!(app.load_runner.as_ref().unwrap().running);

        // Ctrl-R while running: cancel-record the partial, then restart fresh
        // (Run is Ctrl-R as of the 2026-07-10 owner decision).
        app.handle_load_runner_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .unwrap();
        let runner = app.load_runner.as_ref().unwrap();
        assert!(runner.running, "a fresh run started");
        assert!(!runner.finished && !runner.cancelled);
        assert!(runner.run_generation > g1, "fresh run on a new generation");
        assert_eq!(runner.completed, 0, "fresh rows");
        assert!(
            runner
                .results
                .iter()
                .all(|r| r.status == LoadStatus::Pending),
            "every row reset to pending"
        );

        // The interrupted run left exactly one cancelled summary capturing what
        // completed so far — the partial is not lost.
        let batches = app
            .history
            .as_ref()
            .unwrap()
            .recent_load_batches(10)
            .unwrap();
        assert_eq!(
            batches.len(),
            1,
            "the interrupted run's partial was recorded"
        );
        assert!(batches[0].summary.cancelled);
        assert_eq!(batches[0].summary.ok_count, 1);
    }

    #[test]
    fn load_close_while_running_writes_cancelled_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        app.history = Some(HistoryStore::in_memory().unwrap());
        app.load_runner.as_mut().unwrap().cfg.total = 4;
        app.start_load_run();
        let g = load_gen(&app);
        app.on_load_result(g, 0, Ok(ok_resp(200, "a")));
        app.on_load_result(g, 1, Ok(ok_resp(500, "b")));

        // Close mid-run: `q` asks to confirm, `y` closes.
        app.handle_load_runner_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.load_runner.as_ref().unwrap().confirming_close);
        app.handle_load_runner_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.load_runner.is_none(), "runner closed");
        assert_eq!(app.mode, Mode::Normal);

        // Closing mid-run recorded the partial (cancelled) summary.
        let batches = app
            .history
            .as_ref()
            .unwrap()
            .recent_load_batches(10)
            .unwrap();
        assert_eq!(batches.len(), 1, "close mid-run recorded the partial");
        assert!(batches[0].summary.cancelled);
        assert_eq!(batches[0].summary.ok_count, 1);
        assert_eq!(batches[0].summary.fail_count, 1);
    }

    #[test]
    fn load_guardrail_refuse_blocks_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        // Above the hard cap (default max_total = 10_000).
        app.load_runner.as_mut().unwrap().cfg.total = 20_000;
        app.request_load_run();
        let runner = app.load_runner.as_ref().unwrap();
        assert!(!runner.running, "a refused run never starts");
        assert!(runner.pending_confirm.is_none(), "refuse does not prompt");
        assert!(app.message.is_some(), "refusal is surfaced loudly");
    }

    #[test]
    fn load_guardrail_warn_requires_confirm_then_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), "https://api.test/ping");
        // Above warn_total (default 100), below the hard cap.
        app.load_runner.as_mut().unwrap().cfg.total = 500;
        app.request_load_run();
        let runner = app.load_runner.as_ref().unwrap();
        assert!(!runner.running, "warn does not run immediately");
        let confirm = runner.pending_confirm.clone().expect("confirm shown");
        assert!(
            confirm.contains("500"),
            "confirm names the count: {confirm}"
        );
        assert!(
            confirm.contains("https://api.test/ping"),
            "confirm shows the target URL: {confirm}"
        );
        // Accepting the confirm (`y`) starts the run.
        app.handle_load_runner_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .unwrap();
        let runner = app.load_runner.as_ref().unwrap();
        assert!(runner.pending_confirm.is_none());
        assert!(runner.running, "confirmed run started");
        assert_eq!(runner.results.len(), 500);
    }

    /// A request-counting responder that holds each request open, so a cancel
    /// mid-batch is observable as "the server never saw the un-launched copies".
    struct CountingSlowResponder {
        count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        delay: Duration,
    }

    impl wiremock::Respond for CountingSlowResponder {
        fn respond(&self, _req: &wiremock::Request) -> wiremock::ResponseTemplate {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            wiremock::ResponseTemplate::new(200).set_delay(self.delay)
        }
    }

    /// The batch-cancel non-negotiable, proven live: the runner owns a SINGLE
    /// launcher task; cancelling aborts it, which drops the `buffer_unordered`
    /// fan-out and every in-flight reqwest future — so the un-launched copies of a
    /// large batch are never fired. With concurrency 2 and a slow server, a cancel
    /// right after launch means the server sees only a handful of the copies, not
    /// all of them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_cancel_aborts_the_batch_live() {
        use churl_core::http::{DEFAULT_TIMEOUT, build_client};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        let count = Arc::new(AtomicUsize::new(0));
        Mock::given(method("GET"))
            .and(path("/slow"))
            .respond_with(CountingSlowResponder {
                count: Arc::clone(&count),
                delay: Duration::from_millis(400),
            })
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mut app = load_app(dir.path(), &format!("{}/slow", server.uri()));
        app.client = Some(build_client(DEFAULT_TIMEOUT).unwrap());
        app.history = Some(HistoryStore::in_memory().unwrap());
        {
            let cfg = &mut app.load_runner.as_mut().unwrap().cfg;
            cfg.total = 20;
            cfg.concurrency = 2;
            cfg.interval = Duration::ZERO;
        }
        app.start_load_run();
        assert!(app.load_abort.is_some(), "a launcher task is running");

        // Let the first couple of copies actually reach the server, then cancel.
        tokio::time::sleep(Duration::from_millis(120)).await;
        app.cancel_load_run();

        // Give any leaked / detached work ample time to hit the server.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let seen = count.load(Ordering::SeqCst);
        assert!(
            seen < 20,
            "cancel must abort the batch: the server saw {seen}/20 copies (all fired → cancel leaked)"
        );
        assert!(
            app.load_runner.as_ref().unwrap().cancelled,
            "runner marked cancelled"
        );
    }

    // ---- PR 3a: Buffer refactor (Stage 1) unit tests ----

    /// Builds a `SelectedEndpoint` with the given file + body for buffer tests.
    fn selected_with(file: &str, body: Option<&str>) -> SelectedEndpoint {
        SelectedEndpoint {
            display_path: format!("coll/{file}"),
            file: std::path::PathBuf::from(file),
            collection: 0,
            endpoint: Endpoint {
                seq: 0,
                name: "ep".to_owned(),
                request: Request {
                    method: churl_core::model::Method::Get,
                    url: "https://api.test/x".to_owned(),
                    headers: Vec::new(),
                    params: Vec::new(),
                    body: body.map(|c| Body {
                        kind: BodyKind::Text,
                        content: c.to_owned(),
                    }),
                    auth: None,
                },
            },
        }
    }

    /// `open_or_focus_buffer` builds a buffer whose editor is seeded from the
    /// request body and whose `loaded_snapshot` clones the pristine endpoint.
    /// Stage 2: a fresh open seeds the editor/snapshot; a SECOND distinct
    /// endpoint pushes a new buffer (never replaces).
    #[test]
    fn open_or_focus_buffer_seeds_editor_and_snapshot() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", Some("hello body")));
        assert_eq!(app.buffers.len(), 1);
        assert_eq!(app.active, 0);
        let b = app.active_endpoint_buffer().unwrap();
        assert_eq!(String::from(b.editor.lines.clone()), "hello body");
        assert_eq!(
            b.loaded_snapshot.request.body.as_ref().unwrap().content,
            "hello body"
        );
        assert!(!app.is_dirty(), "freshly opened buffer is not dirty");

        // Opening a second distinct endpoint PUSHES a new buffer + focuses it.
        app.open_or_focus_buffer(selected_with("b.toml", None));
        assert_eq!(app.buffers.len(), 2, "distinct endpoint pushes a buffer");
        assert_eq!(app.active, 1);
        assert_eq!(
            app.active_endpoint_buffer().unwrap().endpoint.file,
            PathBuf::from("b.toml")
        );
    }

    /// Stage 2: re-opening an already-open endpoint DEDUPS — focuses the existing
    /// buffer instead of pushing a duplicate, preserving its in-memory edits.
    #[test]
    fn open_or_focus_buffer_dedups_and_focuses_existing() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", Some("body a")));
        app.open_or_focus_buffer(selected_with("b.toml", Some("body b")));
        assert_eq!(app.buffers.len(), 2);
        assert_eq!(app.active, 1);
        // Edit B so we can prove the dedup keeps its edits.
        *app.test_editor() = EditorState::new(Lines::from("edited b"));
        assert!(app.is_dirty());

        // Re-open A: focuses it, no new buffer.
        app.open_or_focus_buffer(selected_with("a.toml", Some("body a")));
        assert_eq!(app.buffers.len(), 1 + 1, "no duplicate buffer for A");
        assert_eq!(app.active, 0, "A focused");
        assert_eq!(
            String::from(app.active_endpoint_buffer().unwrap().editor.lines.clone()),
            "body a"
        );

        // Re-open B: focuses it again, edits intact.
        app.open_or_focus_buffer(selected_with("b.toml", Some("body b")));
        assert_eq!(app.buffers.len(), 2, "still two buffers");
        assert_eq!(app.active, 1);
        assert_eq!(
            String::from(app.active_endpoint_buffer().unwrap().editor.lines.clone()),
            "edited b",
            "B's in-memory edits survive the dedup-focus"
        );
    }

    /// Response/scroll/in-flight are now PER-ENDPOINT (see DECISIONS.md, PR-3a):
    /// loading a different endpoint shows its OWN fresh `Idle` response, not the
    /// previous endpoint's stale one. On master (global `response`), A's completed
    /// response bled through under B until a send; this locks in the fix.
    #[test]
    fn loading_a_new_endpoint_shows_a_fresh_response_not_the_previous() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        // Endpoint A: drive it to a completed response with a non-zero scroll.
        app.open_or_focus_buffer(selected_with("a.toml", None));
        *app.response_mut() = ResponseState::Done {
            view: ResponseView::build(&response(), 1),
        };
        app.active_endpoint_buffer_mut().unwrap().geometry.scroll = 7;
        assert!(matches!(app.response(), ResponseState::Done { .. }));

        // Load a DIFFERENT endpoint B (no send): B gets its own fresh response.
        app.open_or_focus_buffer(selected_with("b.toml", None));
        let b = app.active_endpoint_buffer().unwrap();
        assert!(
            matches!(b.response, ResponseState::Idle),
            "B shows its own Idle response, not A's stale Done"
        );
        assert_eq!(
            b.geometry.scroll, 0,
            "B starts unscrolled, not at A's scroll"
        );
        assert!(b.in_flight.is_none(), "B has no in-flight from A");
    }

    /// `is_dirty` is derived per-`EndpointBuffer`: an in-memory body edit that
    /// diverges from the buffer's own snapshot marks it dirty.
    #[test]
    fn is_dirty_is_per_endpoint_buffer() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", Some("orig")));
        assert!(!app.is_dirty());
        // Edit the body editor away from the snapshot → dirty.
        *app.test_editor() = EditorState::new(Lines::from("changed"));
        assert!(app.is_dirty(), "body edit diverging from snapshot is dirty");
        // Restore the exact snapshot text → clean again (derived, not a flag).
        *app.test_editor() = EditorState::new(Lines::from("orig"));
        assert!(!app.is_dirty(), "reverting the edit clears dirty");
    }

    /// Distinct buffers keep independent editor state: editing A, switching to B
    /// and back leaves A's edit intact (each buffer owns its editor).
    #[test]
    fn distinct_buffers_keep_independent_editor_state() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", Some("A")));
        app.open_or_focus_buffer(selected_with("b.toml", Some("B")));
        assert_eq!(app.buffers.len(), 2);
        app.active = 0;
        *app.test_editor() = EditorState::new(Lines::from("A edited"));
        app.active = 1;
        assert_eq!(
            String::from(app.active_endpoint_buffer().unwrap().editor.lines.clone()),
            "B"
        );
        app.active = 0;
        assert_eq!(
            String::from(app.active_endpoint_buffer().unwrap().editor.lines.clone()),
            "A edited",
            "A's edit survived the round-trip"
        );
    }

    /// `buffer_cycle` wraps forward/backward and no-ops on an empty list.
    #[test]
    fn buffer_cycle_wraps_and_noops_on_empty() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.buffer_cycle(true); // no panic on empty
        assert_eq!(app.active, 0);

        for f in ["a.toml", "b.toml", "c.toml"] {
            app.open_or_focus_buffer(selected_with(f, None));
        }
        app.active = 0;
        app.buffer_cycle(true);
        assert_eq!(app.active, 1);
        app.buffer_cycle(true);
        app.buffer_cycle(true);
        assert_eq!(app.active, 0, "forward wraps 2 -> 0");
        app.buffer_cycle(false);
        assert_eq!(app.active, 2, "backward wraps 0 -> 2");
    }

    /// `new_active_after_close` neighbour rule across the closed-vs-active matrix.
    #[test]
    fn close_clean_new_active_matrix() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        let reopen = |app: &mut App| {
            app.buffers.clear();
            for f in ["a.toml", "b.toml", "c.toml"] {
                app.open_or_focus_buffer(selected_with(f, None));
            }
        };

        // closed < active: active shifts left.
        reopen(&mut app);
        app.active = 2;
        app.close_buffer(0);
        assert_eq!(app.buffers.len(), 2);
        assert_eq!(app.active, 1, "closed<active shifts active left");

        // closed == active (middle): right neighbour takes the slot.
        reopen(&mut app);
        app.active = 1;
        app.close_buffer(1);
        assert_eq!(app.active, 1, "closed==active picks the right neighbour");

        // closed > active: active unchanged.
        reopen(&mut app);
        app.active = 0;
        app.close_buffer(2);
        assert_eq!(app.active, 0, "closed>active leaves active");

        // close the tail while active is the tail: clamps to new last.
        reopen(&mut app);
        app.active = 2;
        app.close_buffer(2);
        assert_eq!(app.active, 1, "closing the active tail clamps to new last");

        // close the last remaining buffer -> empty, active 0.
        app.buffers.clear();
        app.open_or_focus_buffer(selected_with("only.toml", None));
        app.close_buffer(0);
        assert!(app.buffers.is_empty());
        assert_eq!(app.active, 0);
    }

    /// Closing a DIRTY buffer opens the discard confirm with `pending_close`
    /// parked; `d` closes it, `Esc` keeps it.
    #[test]
    fn close_dirty_buffer_confirm_discard_and_cancel() {
        // d closes.
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", Some("orig")));
        *app.test_editor() = EditorState::new(Lines::from("edited"));
        assert!(app.is_dirty());
        app.close_buffer(0);
        assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
        assert!(matches!(app.pending_close, Some(PendingClose::One(_))));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.buffers.is_empty(), "d discards + closes");
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_close.is_none());

        // Esc keeps the buffer.
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", Some("orig")));
        *app.test_editor() = EditorState::new(Lines::from("edited"));
        app.close_buffer(0);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.buffers.len(), 1, "Esc keeps the dirty buffer");
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_close.is_none());
    }

    /// Closing a CLEAN buffer skips the confirm entirely.
    #[test]
    fn close_clean_buffer_no_confirm() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", None));
        app.close_buffer(0);
        assert!(app.buffers.is_empty());
        assert_eq!(app.mode, Mode::Normal, "clean close needs no confirm");
    }

    /// close-all with two dirty buffers prompts one at a time; `d` on each drains
    /// to empty. Clean buffers close immediately.
    #[test]
    fn close_all_two_dirty_sequential_prompts() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        // A dirty, B clean, C dirty.
        app.open_or_focus_buffer(selected_with("a.toml", Some("a")));
        app.active = 0;
        *app.test_editor() = EditorState::new(Lines::from("a edited"));
        app.open_or_focus_buffer(selected_with("b.toml", None));
        app.open_or_focus_buffer(selected_with("c.toml", Some("c")));
        app.active = 2;
        *app.test_editor() = EditorState::new(Lines::from("c edited"));

        app.close_all_buffers();
        // Clean B closed immediately; A + C queued behind the confirm.
        assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
        assert!(matches!(app.pending_close, Some(PendingClose::All(_))));
        assert_eq!(app.buffers.len(), 2, "B closed immediately");

        // Discard first dirty (A) -> prompt for C.
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.mode, Mode::Confirm(ConfirmPurpose::DiscardChanges));
        assert_eq!(app.buffers.len(), 1, "A closed, C remains");
        // Discard C -> empty.
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.buffers.is_empty(), "queue drained");
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_close.is_none());
    }

    /// Esc mid close-all queue aborts the rest: the already-closed clean/discarded
    /// buffers stay closed, the still-dirty ones stay open.
    #[test]
    fn close_all_esc_mid_queue_aborts_remaining() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", Some("a")));
        app.active = 0;
        *app.test_editor() = EditorState::new(Lines::from("a edited"));
        app.open_or_focus_buffer(selected_with("c.toml", Some("c")));
        app.active = 1;
        *app.test_editor() = EditorState::new(Lines::from("c edited"));

        app.close_all_buffers();
        assert_eq!(app.buffers.len(), 2, "both dirty, both queued");
        // Discard A -> prompt for C.
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.buffers.len(), 1, "A closed");
        // Esc aborts: C stays open.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.buffers.len(), 1, "C stays open after Esc");
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.pending_close.is_none());
    }

    /// `leader_sub_lookup(Tabs, …)` resolves the buffer actions.
    #[test]
    fn leader_tabs_submenu_binds() {
        let km = KeyMap::default();
        let lk = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        assert_eq!(
            km.leader_sub_lookup("tabs", lk('n')),
            Some(Action::BufferNext)
        );
        assert_eq!(
            km.leader_sub_lookup("tabs", lk('p')),
            Some(Action::BufferPrev)
        );
        assert_eq!(
            km.leader_sub_lookup("tabs", lk('x')),
            Some(Action::BufferClose)
        );
        assert_eq!(
            km.leader_sub_lookup(
                "tabs",
                KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT)
            ),
            Some(Action::BufferCloseAll)
        );
        // Note #5: `<leader>t 1`..`9` resolve to FocusBufferIndex(n) — in the
        // tabs SUBMENU layer only (see `numbered_jump_does_not_shadow_request_...`).
        for n in 1..=9usize {
            let digit = char::from_digit(n as u32, 10).unwrap();
            assert_eq!(
                km.leader_sub_lookup("tabs", lk(digit)),
                Some(Action::FocusBufferIndex(n)),
                "<leader>t {n} → FocusBufferIndex({n})"
            );
        }
    }

    /// The tabs-submenu digit binds (`<leader>t <n>`) live ONLY in the `tabs`
    /// submenu layer and must NOT shadow the Request-pane `1`..`4` tab-jump
    /// overlay (M6.7). The submenu resolves a digit to `FocusBufferIndex`, while
    /// the Request `PaneCtx` overlay still resolves the same digit to its
    /// `Tab1`..`Tab4` action — two independent keymap layers, no clash.
    #[test]
    fn numbered_jump_does_not_shadow_request_pane_digits() {
        let km = KeyMap::default();
        let lk = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        // Submenu layer: digit → FocusBufferIndex.
        assert_eq!(
            km.leader_sub_lookup("tabs", lk('3')),
            Some(Action::FocusBufferIndex(3))
        );
        // Request-pane overlay: the same digit still means Tab3 (Auth) there.
        assert_eq!(
            km.lookup_ctx(lk('3'), PaneCtx::Request),
            Some(Action::Tab3),
            "Request-pane digit overlay is untouched by the submenu binds"
        );
        // And `5`..`9` are NOT bound in the Request pane (only `1`..`4` are), so
        // the extra submenu digits can never leak a Request-pane action.
        assert_eq!(km.lookup_ctx(lk('5'), PaneCtx::Request), None);
    }

    /// `<leader>t 3` dispatches to `FocusBufferIndex(3)`, focusing the 3rd tab.
    #[test]
    fn focus_buffer_index_focuses_nth_tab() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.buffers = vec![
            Buffer::endpoint(selected_with("a.toml", None)),
            Buffer::endpoint(selected_with("b.toml", None)),
            Buffer::endpoint(selected_with("c.toml", None)),
        ];
        app.active = 0;
        // Dispatch the action the `<leader>t 3` bind resolves to.
        app.dispatch(Action::FocusBufferIndex(3), None).unwrap();
        assert_eq!(app.active, 2, "1-based jump: tab 3 → index 2");
        assert!(app.message.is_none(), "in-range jump sets no message");
    }

    /// Out-of-range `<leader>t <n>` (n > open count, and n == 0) is a graceful
    /// no-op with a "no tab N" message — never a panic or a wrong-tab jump.
    #[test]
    fn focus_buffer_index_out_of_range_is_noop_with_message() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.buffers = vec![
            Buffer::endpoint(selected_with("a.toml", None)),
            Buffer::endpoint(selected_with("b.toml", None)),
        ];
        app.active = 1;
        // n = 5 > 2 open tabs → no-op + message, active unchanged.
        app.dispatch(Action::FocusBufferIndex(5), None).unwrap();
        assert_eq!(app.active, 1, "out-of-range never moves the active tab");
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("no tab 5")
        );
        // n = 0 is likewise a guarded no-op (defensive; not reachable via 1..9).
        app.message = None;
        app.dispatch(Action::FocusBufferIndex(0), None).unwrap();
        assert_eq!(app.active, 1);
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("no tab 0")
        );
    }

    /// `send_request`'s in-flight guard reads the ACTIVE buffer: with an
    /// in-flight already on it, a second send reports "already in flight" and
    /// does not overwrite the slot.
    #[tokio::test]
    async fn send_request_guards_on_active_buffer_in_flight() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        open_bare_endpoint(&mut app);
        set_active_in_flight(&mut app, 1);
        app.send_request();
        assert_eq!(
            app.message.as_ref().map(|m| m.text.as_str()),
            Some("request already in flight — ctrl-c to cancel")
        );
        assert!(
            active_in_flight_is_some(&app),
            "existing in-flight untouched"
        );
    }

    /// `on_response` routes by `in_flight.generation` to the MATCHING buffer,
    /// even when it is not the active one. Two buffers each carry an in-flight at
    /// a distinct generation; the response lands on the right buffer only.
    #[tokio::test]
    async fn on_response_routes_by_generation_to_matching_buffer() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        // Two buffers pushed directly (Stage 2 shape; Stage 1 keeps 1, but the
        // routing scan must already be correct).
        app.buffers = vec![
            Buffer::endpoint(selected_with("a.toml", None)),
            Buffer::endpoint(selected_with("b.toml", None)),
        ];
        // Buffer 0 in-flight at gen 7; buffer 1 in-flight at gen 9.
        app.buffers[0].as_endpoint_mut().unwrap().in_flight = Some(InFlightRequest {
            handle: tokio::spawn(async {}).abort_handle(),
            generation: 7,
            meta: meta(),
        });
        app.buffers[1].as_endpoint_mut().unwrap().in_flight = Some(InFlightRequest {
            handle: tokio::spawn(async {}).abort_handle(),
            generation: 9,
            meta: meta(),
        });
        app.active = 0;

        // A response for gen 9 must land on buffer 1 (NOT the active buffer 0).
        app.on_response(9, Ok(response()), meta());
        assert!(
            matches!(
                app.buffers[1].as_endpoint().unwrap().response,
                ResponseState::Done { .. }
            ),
            "gen 9 lands on buffer 1"
        );
        assert!(app.buffers[1].as_endpoint().unwrap().in_flight.is_none());
        // Buffer 0 is untouched (still in-flight, still Idle).
        assert!(app.buffers[0].as_endpoint().unwrap().in_flight.is_some());
        assert!(matches!(
            app.buffers[0].as_endpoint().unwrap().response,
            ResponseState::Idle
        ));

        // An unknown generation is dropped without touching any buffer.
        app.on_response(42, Ok(response()), meta());
        assert!(app.buffers[0].as_endpoint().unwrap().in_flight.is_some());
    }

    /// Closing a clean buffer aborts its in-flight request before removal (no
    /// history row written — silent, per the design).
    #[tokio::test]
    async fn close_buffer_aborts_its_in_flight() {
        let mut app = App::new(None, KeyMap::default()).unwrap();
        app.open_or_focus_buffer(selected_with("a.toml", None));
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let aborted = handle.abort_handle();
        app.active_endpoint_buffer_mut().unwrap().in_flight = Some(InFlightRequest {
            handle: aborted.clone(),
            generation: 1,
            meta: meta(),
        });
        app.close_buffer(0);
        assert!(app.buffers.is_empty(), "clean buffer closed");
        tokio::task::yield_now().await;
        assert!(
            aborted.is_finished(),
            "the in-flight task was aborted on close"
        );
    }

    /// `switch_workspace` drops every buffer (clearing all editor/response/dirty
    /// state) and aborts any in-flight request.
    #[tokio::test]
    async fn switch_workspace_clears_buffers_and_aborts_in_flight() {
        let dir_a = tempfile::tempdir().unwrap();
        let mut app = workspace_fixture(dir_a.path());
        app.explorer.expand().unwrap();
        app.guarded_load(PendingLoad::Row(1)).unwrap();
        assert!(app.selected().is_some());
        // A never-completing in-flight task so "finished" unambiguously means
        // "aborted" (an empty task would finish on its own).
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let aborted = handle.abort_handle();
        app.active_endpoint_buffer_mut().unwrap().in_flight = Some(InFlightRequest {
            handle: aborted.clone(),
            generation: 1,
            meta: meta(),
        });

        let dir_b = tempfile::tempdir().unwrap();
        other_workspace_fixture(dir_b.path());
        app.switch_workspace(dir_b.path().to_path_buf()).unwrap();

        assert!(app.buffers.is_empty(), "all buffers dropped on switch");
        assert_eq!(app.active, 0);
        assert!(app.selected().is_none());
        // Let the abort propagate, then confirm the task is gone (not orphaned).
        tokio::task::yield_now().await;
        assert!(aborted.is_finished(), "the in-flight task was aborted");
    }

    /// `remap_buffers` removes a buffer whose file vanished (post-delete) and
    /// clamps `active`.
    #[test]
    fn remap_buffers_removes_vanished_file_buffer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = dir.path().join("api");
        std::fs::create_dir(&coll).unwrap();
        let file = coll.join("ping.toml");
        std::fs::write(
            &file,
            "seq = 0\nname = \"Ping\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/ping\"\n",
        )
        .unwrap();
        let ws = open_workspace(dir.path()).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        app.guarded_load(PendingLoad::File(file.clone())).unwrap();
        assert_eq!(app.buffers.len(), 1);

        // The file disappears on disk, then a reload remaps.
        std::fs::remove_file(&file).unwrap();
        app.reload_explorer().unwrap();
        assert!(app.buffers.is_empty(), "vanished-file buffer is removed");
        assert_eq!(app.active, 0);
        assert!(app.selected().is_none());
    }

    /// Renaming the loaded endpoint repoints its buffer's file path + name +
    /// snapshot name in place (path-based), preserving unsaved edits.
    #[test]
    fn rename_repoints_loaded_buffer_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("churl.toml"), "name = \"demo\"\n").unwrap();
        let coll = dir.path().join("api");
        std::fs::create_dir(&coll).unwrap();
        let file = coll.join("ping.toml");
        std::fs::write(
            &file,
            "seq = 0\nname = \"Ping\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/ping\"\n",
        )
        .unwrap();
        let ws = open_workspace(dir.path()).unwrap();
        let mut app = App::new(ws, KeyMap::default()).unwrap();
        app.guarded_load(PendingLoad::File(file.clone())).unwrap();
        // Move the explorer cursor onto the endpoint row so rename targets it.
        app.explorer.select_file(&file).unwrap();
        // Make an unsaved edit that must survive the rename.
        *app.test_editor() = EditorState::new(Lines::from("draft body"));

        app.commit_rename("Pong".to_owned()).unwrap();

        let b = app
            .active_endpoint_buffer()
            .expect("buffer survives rename");
        assert_eq!(b.endpoint.endpoint.name, "Pong", "endpoint name repointed");
        assert_eq!(b.loaded_snapshot.name, "Pong", "snapshot name repointed");
        assert!(
            b.endpoint.file.ends_with("pong.toml"),
            "file path repointed (slugified), got {:?}",
            b.endpoint.file
        );
        assert_eq!(
            String::from(b.editor.lines.clone()),
            "draft body",
            "unsaved edit survives the in-place rename"
        );
    }
}
