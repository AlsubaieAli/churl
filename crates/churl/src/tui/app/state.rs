//! Pure data-type definitions for the TUI app: the panes/modes/message
//! enums, the per-endpoint [`Buffer`]/[`EndpointBuffer`] state, and the small
//! body-folding free functions. Split out of `app.rs` into this child module of
//! `app` so every definition keeps its **exact original scope** — items reachable
//! throughout `app` and its descendants get `pub(in crate::tui::app)` (the scope
//! a bare-private item in `app/mod.rs` already had), and the two externally-named
//! types (`Pane`, `AppMsg`) stay `pub` and are re-exported from `app` so their
//! public paths (`tui::app::Pane`, `tui::app::AppMsg`) are unchanged. No logic
//! changes — pure movement.

use super::*;

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
    pub(in crate::tui::app) fn next(self) -> Self {
        match self {
            Pane::Explorer => Pane::UrlBar,
            Pane::UrlBar => Pane::Request,
            Pane::Request => Pane::Response,
            Pane::Response => Pane::Explorer,
        }
    }

    pub(in crate::tui::app) fn prev(self) -> Self {
        self.next().next().next()
    }

    pub(in crate::tui::app) fn name(self) -> &'static str {
        match self {
            Pane::Explorer => "EXPLORER",
            Pane::UrlBar => "URL",
            Pane::Request => "REQUEST",
            Pane::Response => "RESPONSE",
        }
    }

    /// The keymap overlay context for this pane.
    pub(in crate::tui::app) fn ctx(self) -> PaneCtx {
        match self {
            Pane::Explorer => PaneCtx::Explorer,
            Pane::UrlBar => PaneCtx::UrlBar,
            Pane::Request => PaneCtx::Request,
            Pane::Response => PaneCtx::Response,
        }
    }
}

/// Which sub-pane inside the left (explorer) column has focus/zoom.
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
///
/// The modal overlays carry their state **in their variant**
/// ([`Mode::EnvEditor`]/[`Mode::LoadRunner`]/[`Mode::Sequence`]) rather than in
/// parallel `Option<…>` fields on [`App`] — an active editor with no data is not
/// constructible, so no defensive `is_none()→Normal` guards or
/// `.expect("checked above")` panics are needed. Because those payloads are
/// non-`Copy` (and don't derive `PartialEq`), `Mode` is **non-`Copy`** and
/// **non-`PartialEq`**: compare variants with `matches!` and move/replace values
/// with [`std::mem::replace`] rather than copying.
#[derive(Debug)]
pub enum Mode {
    /// Pane navigation and editing.
    Normal,
    /// The fuzzy endpoint search overlay is open.
    Search,
    /// The command palette overlay is open.
    Palette,
    /// The quick-jump workspace picker overlay is open.
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
    /// The environments & variables editor: a modal split-view that grabs
    /// every key (same routing tier as Search/Palette/Picker). Owns its
    /// [`EnvEditorState`] — the editor cannot exist without its data.
    EnvEditor(EnvEditorState),
    /// The unified sequence surface: ONE modal with an edit⇄run switcher
    /// (`Ctrl-R`). `view` selects the active face; the Edit face drives the
    /// `editor` (step/extraction editor), the Run face drives + shows the live
    /// `runner`. The `view`/`editor`/`runner` are fields of this variant, so they
    /// cannot exist without `Mode::Sequence`. `editor`/`runner`
    /// stay `Option` INSIDE the variant: both faces are legitimately optional (a
    /// `<leader>s r` run opens the Run face with no editor built yet; an edit
    /// session never allocates a runner until the first run).
    Sequence {
        view: SeqView,
        editor: Option<SequenceEditorState>,
        runner: Option<SequenceRunnerState>,
    },
    /// The concurrent-load runner: a large modal firing N copies of the
    /// selected endpoint with live results + latency stats. Owns its
    /// [`LoadRunnerState`] — the runner cannot exist without its data.
    LoadRunner(LoadRunnerState),
    /// The session Options overlay: proxy / TLS-verification / cookie-jar
    /// controls. Owns its [`OptionsState`] — the overlay cannot exist without its
    /// data, so no parallel `Option` field is needed.
    Options(OptionsState),
    /// The debug Inspector overlay (`<leader>d`): a per-exchange, read-only
    /// view of a captured [`churl_core::debug::DebugTrace`]. Owns its
    /// [`InspectorState`] — including the trace itself (`None` when nothing
    /// was captured yet) — so an Inspector with no data to show is still
    /// constructible without a parallel `Option` field, matching every other
    /// data-carrying overlay here.
    Inspector(InspectorState),
    /// The debug Log panel overlay (`<leader>L`): a read-only, scrollable
    /// view of the bounded `tracing` ring (see
    /// [`crate::tui::log_subscriber`]). Owns its [`LogPanelState`] — scroll
    /// position only; the ring's contents live on `App`
    /// (`App::log_ring`), continuously written by the background subscriber
    /// independent of whether this mode is active, so they cannot live
    /// inside the variant the way a snapshot-at-open trace does.
    LogPanel(LogPanelState),
}

/// The state of the ONE open fuzzy-picker overlay, when `mode` is one of the
/// four picker modes ([`Mode::Search`]/[`Mode::Palette`]/[`Mode::WorkspacePicker`]
/// /[`Mode::SequencePicker`]). `None` when no picker is open.
///
/// The picker is an enum whose variants each carry their own items inside the
/// variant, rather than a bare `Option<PickerState>` multiplexed against parallel
/// item `Vec`s plus one-shot `bool` flags. Because the items travel WITH the
/// finder inside the variant, the [`PickerState::current`] index can only address
/// its own list, and the one-shot flags are variant fields consumed on accept —
/// `(kind, wrong-items)` and `(kind, stale-flag)` are unrepresentable, so a stale
/// index or wrong-`Vec` pairing cannot become an out-of-bounds hazard.
///
/// The shared [`PickerState`] (query/filter/selection over display strings) is the
/// `state` field of every variant; reach it uniformly via
/// [`App::picker_state`]/[`App::picker_state_mut`]. The `Mode::*` picker variants
/// stay as-is (they gate key routing + the render overlay) — this fold consolidates
/// the picker's OWN state only, not the mode axis.
#[derive(Debug)]
pub enum Picker {
    /// The fuzzy endpoint-search overlay ([`Mode::Search`]). `targets[i]` is the
    /// `(collection, endpoint)` explorer position for item `i`. `after_pick`
    /// (folds the old `load_runner_after_pick` one-shot) means the pick was made
    /// to choose a load-runner target (`<leader>l f`): accepting loads the
    /// endpoint, then chains into the load runner.
    Search {
        state: picker::PickerState,
        targets: Vec<(usize, usize)>,
        after_pick: bool,
    },
    /// The command palette ([`Mode::Palette`]). `actions[i]` is the [`Action`]
    /// dispatched when item `i` is accepted.
    Palette {
        state: picker::PickerState,
        actions: Vec<Action>,
    },
    /// The profile switcher ([`Mode::Palette`], distinct from the command palette
    /// by variant, not by an empty-`Vec` convention). `profiles[i]` is the raw
    /// profile name for item `i` (`None` = the "(none)" entry).
    Profile {
        state: picker::PickerState,
        profiles: Vec<Option<String>>,
    },
    /// The quick-jump workspace switcher ([`Mode::WorkspacePicker`]). `paths[i]` is
    /// the canonical workspace root for item `i`.
    Workspace {
        state: picker::PickerState,
        paths: Vec<PathBuf>,
    },
    /// The sequence picker ([`Mode::SequencePicker`]). `paths[i]` is the sequence
    /// file for item `i`. `runs` (folds the old `sequence_pick_runs` one-shot) =
    /// `<leader>s r` run-vs-`<leader>s o` edit intent: accepting a pick RUNS the
    /// chosen sequence when `true`, opens it in the Edit face when `false`.
    Sequence {
        state: picker::PickerState,
        paths: Vec<PathBuf>,
        runs: bool,
    },
    /// The auth-kind picker ([`Mode::Palette`], folds the old `auth_picker` bool):
    /// None / Basic / Bearer / ApiKey, addressed by the accepted item index
    /// directly (no side `Vec`).
    Auth { state: picker::PickerState },
    /// The destination picker ([`Mode::Palette`]): a fuzzy list over every
    /// collection node **including the root** (root first). `dirs[i]` is the target
    /// directory for item `i` (the workspace root for the root entry); `purpose`
    /// says what to do with the chosen destination (create here / move-to /
    /// copy-to); `source` is the node being relocated (the endpoint file or
    /// collection dir) for move/copy, `None` for a create. Shared by the
    /// `<leader>n`/`<leader>N` create gestures and move-to / copy-to.
    Destination {
        state: picker::PickerState,
        dirs: Vec<PathBuf>,
        purpose: DestPurpose,
        source: Option<PathBuf>,
    },
}

/// What a [`Picker::Destination`] does once a target directory is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestPurpose {
    /// Create a new endpoint in the chosen collection (then the name prompt).
    CreateEndpoint,
    /// Create a new (sub-)collection under the chosen collection (then the prompt).
    CreateCollection,
    /// Move the source endpoint into the chosen collection.
    MoveEndpoint,
    /// Copy the source endpoint into the chosen collection.
    CopyEndpoint,
    /// Move the source collection subtree under the chosen collection.
    MoveCollection,
    /// Copy the source collection subtree under the chosen collection.
    CopyCollection,
}

impl Picker {
    /// The shared query/filter/selection state, regardless of kind.
    pub(in crate::tui::app) fn state(&self) -> &picker::PickerState {
        match self {
            Picker::Search { state, .. }
            | Picker::Palette { state, .. }
            | Picker::Profile { state, .. }
            | Picker::Workspace { state, .. }
            | Picker::Sequence { state, .. }
            | Picker::Destination { state, .. }
            | Picker::Auth { state } => state,
        }
    }

    /// The shared query/filter/selection state (mutable).
    pub(in crate::tui::app) fn state_mut(&mut self) -> &mut picker::PickerState {
        match self {
            Picker::Search { state, .. }
            | Picker::Palette { state, .. }
            | Picker::Profile { state, .. }
            | Picker::Workspace { state, .. }
            | Picker::Sequence { state, .. }
            | Picker::Destination { state, .. }
            | Picker::Auth { state } => state,
        }
    }
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
    /// Name for a new request sequence; opens the editor on the created file.
    NewSequence,
}

impl PromptPurpose {
    /// The overlay title for this prompt.
    pub(in crate::tui::app) fn title(self) -> &'static str {
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
            PromptPurpose::NewSequence => "New sequence",
        }
    }
}

/// What a [`Mode::Confirm`] is a y/n gate for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmPurpose {
    /// Delete the selected endpoint.
    DeleteEndpoint,
    /// Delete the selected sequence in the Sequences sub-pane.
    DeleteSequence,
    /// Discard unsaved changes before switching endpoints. The pending target
    /// lives in `App::pending_load` (it can carry a path, so it is not part of
    /// this `Copy` enum).
    DiscardChanges,
    /// A pasted-curl import whose derived name collides with an existing endpoint
    /// (U6). Resolved New (`n`, `-N` bump) / Overwrite (`o`, replace the existing
    /// file) / Cancel (`esc`/`c`, nothing written). The parked import data lives in
    /// `App::pending_curl_import` (it carries owned data, so it is not part of this
    /// `Copy` enum). TUI-only — the headless `churl import` path never prompts.
    ImportCollision,
}

/// A pasted-curl import parked behind an open [`ConfirmPurpose::ImportCollision`]
/// overlay: the parsed [`ImportResult`](churl_core::import::ImportResult), the
/// target collection directory, and the existing colliding file. Resolved by the
/// New / Overwrite / Cancel keys; dropped (nothing written) on Cancel.
#[derive(Debug, Clone)]
pub(in crate::tui::app) struct PendingCurlImport {
    /// The collection directory the endpoint would be created in.
    pub(in crate::tui::app) dir: std::path::PathBuf,
    /// The existing `<slug>.toml` the derived name collides with — Overwrite
    /// replaces this file's content in place (no bump).
    pub(in crate::tui::app) existing: std::path::PathBuf,
    /// The parsed import (endpoint + warnings + captured secrets) to write.
    pub(in crate::tui::app) result: churl_core::import::ImportResult,
}

/// A deferred endpoint load awaiting the discard-changes confirm. Every path
/// that replaces the loaded endpoint resolves to one of these targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui::app) enum PendingLoad {
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
pub(in crate::tui::app) enum PendingClose {
    /// A single dirty buffer close (`<leader>t x`).
    One(std::path::PathBuf),
    /// A close-all (`<leader>t X`) working through its dirty buffers one prompt
    /// at a time (front = the one currently prompting).
    All(std::collections::VecDeque<std::path::PathBuf>),
}

/// Capacity of the bounded app channel. Generous headroom for the
/// bursty senders (a load run fires `total` `LoadResult`s, the highlight worker a
/// steady trickle of `Highlighted`s) while still bounding memory: an
/// arbitrarily long / high-`total` session can no longer flood the queue with
/// unbounded owned `Response`/`Highlighted` bodies. All senders live in spawned
/// async tasks (`.send().await` applies natural backpressure) or the dedicated
/// highlight OS thread (`blocking_send`) — never on the render/UI thread — so a
/// full queue slows a producer without ever stalling input.
pub(in crate::tui::app) const APP_CHANNEL_CAPACITY: usize = 1024;

/// Messages delivered to the event loop over the app channel. Not `Copy`:
/// results and highlighted lines carry owned data.
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
        /// The exchange's captured debug trace, when debug capture was on for
        /// this send (`None` otherwise — see [`super::App::debug_enabled`]).
        /// Present regardless of `outcome`: `churl_core::http::execute_traced`
        /// records a failure into the trace itself before returning `Err`, and
        /// the sending task keeps the trace alive across the call (unlike
        /// headless's `run_execution`, whose early `?` return can't). Boxed:
        /// `DebugTrace` carries a full `Request` clone (`resolved_raw`),
        /// which would otherwise bloat every `AppMsg` (most variants are a
        /// few words) to its size.
        trace: Option<Box<DebugTrace>>,
    },
    /// Highlighted lines for a viewport, returned by the highlight worker.
    Highlighted {
        /// Viewport hash these lines belong to (the cache key).
        hash: u64,
        lines: Vec<Line<'static>>,
    },
    /// A sequence step completed. `run_generation` is matched against the
    /// runner's generation so results from a cancelled/superseded run are dropped.
    SequenceStep {
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
        /// This step's captured trace, when debug capture was on for the run
        /// (mirrors [`AppMsg::Response`]'s `trace` field — same
        /// present-regardless-of-outcome, boxed-for-size rationale). Folded
        /// into the session Traffic feed on landing.
        trace: Option<Box<DebugTrace>>,
    },
    /// One copy of a concurrent-load batch actually started executing —
    /// sent by the launcher when the copy enters the concurrency window, so its
    /// row shows the in-flight glyph honestly. `run_generation`-guarded.
    LoadStarted { run_generation: u64, index: usize },
    /// One copy of a concurrent-load batch completed. `run_generation`
    /// guards against results from a cancelled/superseded batch.
    LoadResult {
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
        /// This copy's captured trace, when debug capture was on for the
        /// run. See [`Self::SequenceStep`]'s `trace` field doc.
        trace: Option<Box<DebugTrace>>,
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

/// Which pane is zoomed (the other collapses to a stub).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomPane {
    /// Request pane zoomed; Response collapses to its stats line.
    Request,
    /// Response pane zoomed; Request collapses to its tab bar.
    Response,
}

/// Which surface the shared `response_*` handlers operate on. The main
/// endpoint buffer by default; a runner's selected row/step when that runner's
/// Response region is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui::app) enum ResponseSurface {
    /// The active endpoint buffer (Normal mode; today's behaviour).
    Main,
    /// The load runner's selected results row.
    LoadRunner,
    /// The sequence runner's selected step.
    Sequence,
}

/// Bookkeeping for the single in-flight request.
pub(in crate::tui::app) struct InFlightRequest {
    pub(in crate::tui::app) handle: AbortHandle,
    pub(in crate::tui::app) generation: u64,
    /// Metadata, reused when writing the history row on cancel.
    pub(in crate::tui::app) meta: ResponseMeta,
}

/// The per-endpoint edit/response/dirty/tab/editor state, scoped to a single
/// loaded endpoint rather than held as flat `App` fields. The
/// forward-compat split (a later `Sequence` buffer kind) keeps this the payload
/// of [`BufferKind::Endpoint`] while the dedup key (file path), tab label, and
/// dirty stay reachable at the [`Buffer`] level.
pub(in crate::tui::app) struct EndpointBuffer {
    /// The endpoint currently loaded (with any in-memory edits).
    pub(in crate::tui::app) endpoint: SelectedEndpoint,
    /// The pristine endpoint as loaded, cloned at load time. Dirty state is
    /// *derived* by comparing it against the live request (incl. the edtui body).
    /// Non-`Option`: a buffer always has a baseline.
    pub(in crate::tui::app) loaded_snapshot: Endpoint,
    /// edtui state for the request body.
    pub(in crate::tui::app) editor: EditorState,
    pub(in crate::tui::app) editor_events: EditorEventHandler,
    /// Normal-mode motion extensions (W/B/^/f/F/t/T) for the Body editor.
    pub(in crate::tui::app) editor_vim: VimExt,
    /// Request-pane tab state (active tab + per-tab selection + field edit).
    pub(in crate::tui::app) tabs: RequestTabs,
    /// The inline URL-bar editor while editing the URL; `None` otherwise.
    pub(in crate::tui::app) url_editor: Option<LineEditor>,
    /// The edtui popup URL editor (`e` on the URL bar / `url_edit = "popup"`),
    /// present while the popup is open.
    pub(in crate::tui::app) url_popup: Option<EditorState>,
    pub(in crate::tui::app) url_popup_events: EditorEventHandler,
    /// Normal-mode motion extensions for the URL popup editor.
    pub(in crate::tui::app) url_popup_vim: VimExt,
    pub(in crate::tui::app) response: ResponseState,
    /// Cursor/scroll/viewport geometry for this buffer's response viewer. Shared
    /// shape with the runner states so the `response_*` handlers are mode-aware.
    pub(in crate::tui::app) geometry: ResponseGeometry,
    /// The highlight hash last enqueued but not yet returned.
    pub(in crate::tui::app) pending_highlight: Option<u64>,
    /// Viewport-hash → highlighted-lines cache (capped, cleared on new response).
    pub(in crate::tui::app) highlight_cache: HashMap<u64, Vec<Line<'static>>>,
    pub(in crate::tui::app) in_flight: Option<InFlightRequest>,
}

impl EndpointBuffer {
    /// Folds today's `load_endpoint` body-preparation into a fresh buffer: the
    /// edtui body from the request, a reset vim state, and a pristine snapshot
    /// clone for dirty derivation.
    pub(in crate::tui::app) fn new(selected: SelectedEndpoint) -> Self {
        let body = selected
            .endpoint
            .request
            .body
            .as_ref()
            .map(|body| body.content.as_str())
            .unwrap_or("");
        // Clipboard: see `new_editor_state` in `app/mod.rs` — in-memory, not the OS clipboard.
        let editor = new_editor_state(body);
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
    pub(in crate::tui::app) fn live_request(&self) -> &Request {
        &self.endpoint.endpoint.request
    }

    /// Whether the live endpoint (incl. the edtui body) differs from the pristine
    /// snapshot. Derived — no dirty flag to keep in sync.
    pub(in crate::tui::app) fn is_dirty(&self) -> bool {
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
pub(in crate::tui::app) struct Buffer {
    kind: BufferKind,
}

impl Buffer {
    /// Builds an endpoint buffer (the only kind this stage).
    pub(in crate::tui::app) fn endpoint(selected: SelectedEndpoint) -> Self {
        Self {
            kind: BufferKind::Endpoint(EndpointBuffer::new(selected)),
        }
    }

    /// The dedup key / tab identity: the buffer's endpoint file path. Reachable
    /// without matching `kind` (a future `Sequence` buffer returns its own path).
    pub(in crate::tui::app) fn file(&self) -> &Path {
        match &self.kind {
            BufferKind::Endpoint(b) => &b.endpoint.file,
        }
    }

    /// The endpoint payload, if this is an endpoint buffer.
    pub(in crate::tui::app) fn as_endpoint(&self) -> Option<&EndpointBuffer> {
        match &self.kind {
            BufferKind::Endpoint(b) => Some(b),
        }
    }

    /// The endpoint payload (mutable), if this is an endpoint buffer.
    pub(in crate::tui::app) fn as_endpoint_mut(&mut self) -> Option<&mut EndpointBuffer> {
        match &mut self.kind {
            BufferKind::Endpoint(b) => Some(b),
        }
    }

    /// Whether the buffer has unsaved changes (per-kind).
    pub(in crate::tui::app) fn is_dirty(&self) -> bool {
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
pub(in crate::tui::app) fn fold_body_text(request: &mut Request, text: String) {
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
pub(in crate::tui::app) fn overwrite_body_text(request: &mut Request, text: String) {
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
pub(in crate::tui::app) struct PendingCopy {
    pub(in crate::tui::app) payload: String,
    pub(in crate::tui::app) success_msg: String,
}
