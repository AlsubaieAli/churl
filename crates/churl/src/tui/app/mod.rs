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
use std::sync::Arc;
use std::sync::mpsc::Sender as JobSender;
use std::time::{Duration, Instant};

use churl_core::config::Config;
use churl_core::cookies::ChurlCookieJar;
use churl_core::debug::DebugTrace;
use churl_core::history::{
    CookieJarWriter, HistoryStore, LoadBatchSummary, NewHistoryEntry, default_state_path,
};
use churl_core::http::{ClientConfig, DEFAULT_TIMEOUT, ExecuteOptions, build_client_with};
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
use churl_core::secrets::SecretPolicy;

use super::clipboard;
use super::components::env_editor::{EnvEditorState, EnvKeyOutcome, EnvSaveResult};
use super::components::explorer::{ExplorerState, RowKind, SelectedEndpoint};
use super::components::inspector::InspectorState;
use super::components::jump::{JumpState, JumpTarget};
use super::components::line_editor::LineEditor;
use super::components::load_runner::{LoadOutcome, LoadRunnerState, LoadStatus};
use super::components::log_panel::LogPanelState;
use super::components::message::Message;
use super::components::request_tabs::{EditField, FieldEdit, RequestTab, RequestTabs};
use super::components::response::{
    ResponseGeometry, ResponseMeta, ResponseState, ResponseView, ViewMode,
};
use super::components::settings::{SettingsOutcome, SettingsState};
use super::components::traffic::{self, TrafficEntry, TrafficOutcome};
use churl_core::sequence::StepResult;

use super::components::sequence_editor::{EditorOutcome, SequenceEditorState};
use super::components::sequence_runner::{RunnerOutcome, SequenceRunnerState, StepStatus};
use super::components::vim_ext::{self, VimExt};
use super::components::{
    env_editor, explorer, help, inspector, load_runner, log_panel, message, method_menu, palette,
    picker, prompt, request, response, sequence_editor, sequence_runner, settings, statusline,
    tab_strip, urlbar,
};
use super::events::{Action, FuzzyFinder, KeyMap, LeaderEntry, PaneCtx};
use super::highlight::{self, HighlightJob};
use super::log_subscriber::LogRing;
use super::theme::Theme;

/// Builds an `EditorState` seeded with `text`, wired to edtui's in-memory
/// clipboard rather than its arboard-backed OS-clipboard default. Every edtui
/// surface in churl (curl prompt, URL popup, Body editor) shares this: vim's
/// own yank/paste (`y`/`p`/`d`) targets the unnamed register, not the system
/// clipboard, and edtui's arboard default is a single shared OS handle — a
/// stray in-editor yank on any surface must not clobber the user's real
/// clipboard, and concurrent access to it from more than one editor at once
/// (e.g. the test suite's parallel threads) has been observed to segfault the
/// OS clipboard backend. Terminal bracketed-paste still works with this
/// clipboard in place: edtui's paste handler writes the pasted text in
/// (`set_text`) and then reads it straight back out (`get_text`) to insert
/// it, and `InternalClipboard` round-trips that faithfully.
fn new_editor_state(text: &str) -> EditorState {
    let mut editor = EditorState::new(Lines::from(text));
    editor.set_clipboard(edtui::clipboard::InternalClipboard::default());
    editor
}

/// The multi-line vim editor state for the new-endpoint prompt's expanded
/// (curl) sub-state — the same edtui + vim_ext trio the URL popup uses, so a
/// pasted browser curl is editable across lines before submit. Vim-faithful:
/// Insert-mode Enter inserts a newline, Normal-mode Enter submits (newlines
/// preserved), and a plain name still falls through to a plain endpoint.
pub(in crate::tui::app) struct CurlPrompt {
    pub(in crate::tui::app) editor: EditorState,
    pub(in crate::tui::app) events: EditorEventHandler,
    pub(in crate::tui::app) vim: VimExt,
}

impl CurlPrompt {
    /// A fresh prompt seeded with `seed`, starting in Insert mode by default —
    /// callers that need the multi-line editor to land in Normal mode (a
    /// curl/multi-line paste, or a parse-failure re-open) set `editor.mode`
    /// afterward; see [`App::open_curl_editor`]. See [`new_editor_state`] for
    /// the clipboard rationale.
    fn new(seed: &str) -> Self {
        let mut editor = new_editor_state(seed);
        editor.mode = EditorMode::Insert;
        Self {
            editor,
            events: EditorEventHandler::default(),
            vim: VimExt::default(),
        }
    }

    /// The editor content as one `String` (logical lines joined by `\n`).
    fn text(&self) -> String {
        self.editor.lines.clone().into()
    }
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
    /// The ONE open fuzzy-picker overlay, when `mode` is a picker mode
    /// (Search/Palette/WorkspacePicker/SequencePicker); `None` otherwise.
    ///
    /// The [`Picker`] enum carries each kind's items + one-shot intent IN its
    /// variant, so the selected index can only address its own list and a stale
    /// flag can't leak across kinds. Reach the shared finder/selection via
    /// [`App::picker_state`]/[`App::picker_state_mut`].
    pub picker: Option<Picker>,
    /// Active jump-mode state, when `mode` is [`Mode::Jump`].
    pub jump: Option<JumpState>,
    keymap: KeyMap,
    /// The resolved colour theme, threaded through every render fn.
    pub theme: Theme,
    /// The active theme's NAME (`"dark"` | `"light"`) — [`Self::theme`] is the
    /// already-resolved style table with no way back to the name it came
    /// from, so this is tracked separately (M8.5: the Settings panel's
    /// Appearance category needs to display/cycle it). Seeded from
    /// `Config::theme` (or the built-in default) by [`Self::install_runtime`];
    /// updated by every live theme switch.
    pub(in crate::tui::app) theme_name: String,
    /// Per-slot colour overrides loaded from `config.toml`'s `[theme_colors]`
    /// table, kept ONLY so a live Settings-panel theme-name switch can
    /// re-resolve [`Self::theme`] without losing the user's overrides (the
    /// panel edits the theme *name*; `[theme_colors]` stays out of scope —
    /// M8.5 design). Seeded once by [`Self::install_runtime`]; never mutated
    /// by the panel itself.
    theme_colors: BTreeMap<String, String>,
    /// The working-copy leader-key combo string (M8.5). Seeded from
    /// `Config::leader_key` (or the built-in default `"space"`) by
    /// [`Self::install_runtime`]; editable live in the Settings panel's
    /// Appearance category, but — unlike every other session control — NOT
    /// applied to the live [`Self::keymap`] on this keystroke: a cheap
    /// re-parse seam DOES exist (`KeyMap::set_leader`, M8.5.3), but a leader
    /// change is deliberately deferred to an explicit apply gate,
    /// `<leader>r` (`Self::reload_workspace`), so it never changes the
    /// meaning of a key mid-combo. It takes effect on the next `<leader>r`
    /// or launch, and is what Save-as-default persists to disk.
    pub(in crate::tui::app) leader_key: String,
    /// `--var key=value` overrides: the highest-precedence resolver scope.
    cli_vars: BTreeMap<String, String>,
    /// The active profile name, if any (`--profile` or the SwitchProfile picker).
    pub active_profile: Option<String>,
    finder: FuzzyFinder,
    /// Set to exit the event loop.
    pub should_quit: bool,
    /// Sender half of the app channel (cloned into background tasks).
    pub tx: mpsc::Sender<AppMsg>,
    rx: mpsc::Receiver<AppMsg>,
    /// The shared reqwest client; `None` in snapshot-test construction (runtime-free).
    pub client: Option<Client>,
    /// Lazily-built client that mirrors [`Self::client`]'s knobs (timeout / proxy /
    /// cookies) but forces insecure-TLS on. Used only for the divergent case — a
    /// session that verifies sending an endpoint whose `request.insecure` opted in.
    /// Built on first such send and invalidated (`None`) by every
    /// [`Self::rebuild_client`], so it can never drift from the shared client's
    /// other settings. Stays `None` when the session is already insecure (the
    /// shared client already matches).
    insecure_client: Option<Client>,
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
    /// The mode to restore when the body-search input closes, PARKED here while
    /// `Mode::BodySearch` is active. `Mode::Normal` for the main pane;
    /// `Mode::LoadRunner(state)`/`Mode::Sequence` when search was opened over a
    /// runner Response region, so closing search returns to the runner
    /// rather than dropping the user into Normal mode. Since the runner
    /// state lives IN the mode, this field owns that parked `LoadRunner(state)`
    /// payload for the duration of the overlay (moved in/out via `mem::replace`),
    /// and `load_runner()`/`active_response_surface` consult it while searching.
    body_search_return: Mode,
    /// The mode to restore when the Inspector overlay closes, PARKED here
    /// while `Mode::Inspector` is active — mirrors [`Self::body_search_return`].
    /// The Inspector can be opened over any mode (e.g. `Mode::LoadRunner`), so
    /// closing it must restore that exact mode, state intact, via
    /// `mem::replace` rather than dropping it.
    inspector_return: Mode,
    /// Whether debug capture is on for this session (`<leader>D`; seeded from
    /// `Config.debug` at startup by [`Self::install_runtime`]). Gates whether
    /// [`Self::send_request`] builds a [`DebugTrace`] — off is the exact
    /// pre-M8.3 path (no trace ever constructed). Drives the statusline DEBUG
    /// badge.
    pub(in crate::tui::app) debug_enabled: bool,
    /// The most recently captured exchange's trace, if debug capture was on
    /// for that exchange — the Inspector's default view. `None` both before
    /// any send and after a debug-off send (a stale trace from an earlier
    /// exchange would be misleading, so a debug-off send clears this rather
    /// than leaving the previous one showing). Boxed to match
    /// [`AppMsg::Response`]'s `trace` field (see its doc comment) — moved in
    /// from there without a re-box.
    latest_trace: Option<Box<DebugTrace>>,
    /// The session Traffic feed (M8.3 Wave 4): every captured exchange this
    /// session — standalone sends, and every copy/step of a load/sequence
    /// run — while [`Self::debug_enabled`] was on. Ephemeral (never
    /// persisted) and bounded (see [`super::components::traffic`]); the
    /// Inspector browses it with `n`/`N`. Unlike [`Self::latest_trace`],
    /// never cleared by a debug-off send — Traffic is deliberately full
    /// session history, not a "most recent exchange" pointer.
    pub(in crate::tui::app) traffic: std::collections::VecDeque<TrafficEntry>,
    /// The mode to restore when the Log panel overlay closes, PARKED here
    /// while `Mode::LogPanel` is active — mirrors [`Self::inspector_return`].
    log_return: Mode,
    /// Shared handle to the bounded `tracing` ring the Log panel renders.
    /// Inert (no attached subscriber, gate off) until [`super::run`] installs
    /// the real one via [`Self::set_log_ring`] — snapshot-test `App`s never
    /// call that, so they render an always-empty Log panel, consistent with
    /// [`Self::client`]'s `None` under `TestBackend`.
    pub(in crate::tui::app) log_ring: LogRing,
    /// One-shot flag: the leader key was pressed while a load/sequence
    /// runner mode was active (see [`Self::handle_load_runner_key`] /
    /// [`Self::handle_sequence_key`]'s narrow leader intercept). The *next*
    /// key is checked against ONLY the two debug-overlay leader chords
    /// (`<leader>d`/`<leader>L`) in [`Self::handle_runner_leader_key`); any
    /// other key cancels silently. Deliberately NOT the general
    /// `self.leader` state machine — that would make every leader-root
    /// action reachable from runner mode, not just the two debug overlays.
    runner_leader_pending: bool,
    /// Job sender for the off-thread highlight worker; `None` under `TestBackend`.
    highlight_tx: Option<JobSender<HighlightJob>>,
    /// History store; `None` when disabled (open failed or no data dir).
    history: Option<HistoryStore>,
    /// Transient action/status message shown in the dedicated row above the
    /// statusline (send hints, history/errors, merges, CRUD results); auto-expires
    /// after `Message::EXPIRE_SECS`.
    message: Option<Message>,
    /// Consecutive history/load-summary write failures. A single write failure
    /// only flashes a 250 ms toast that can scroll past unnoticed on a busy
    /// session, so once this crosses [`Self::HISTORY_FAIL_THRESHOLD`] the status
    /// bar shows a sticky "⚠ history not recording" flag. Reset to 0
    /// on any successful write, which also clears the flag.
    history_write_failures: u32,
    /// Load-time keymap conflict/shadow warnings, surfaced as a
    /// first-frame statusline toast. Empty for a clean config. Populated from
    /// [`KeyMap::validate`] by the TUI entry point; a `false` `keymap_warned`
    /// flag ensures the toast fires exactly once.
    keymap_warnings: Vec<String>,
    /// Whether the first-frame keymap-warning toast has already fired.
    keymap_warned: bool,
    /// Monotonic tick counter (incremented every 250 ms tick); drives the spinner
    /// animation in the response pane. `pub` so snapshot tests can set it.
    pub tick_count: u64,
    /// The text prompt's line editor while a single-line [`Mode::Prompt`] overlay
    /// is open (rename, paths, new collection/sequence).
    pub prompt_editor: LineEditor,
    /// The multi-line vim editor backing the new-endpoint prompt's expanded
    /// sub-state. `Mode::Prompt(PromptPurpose::NewEndpoint)` has two valid
    /// sub-states, discriminated by this field alone (no parallel enum/bool):
    /// `None` is the default light single-line sub-state (`prompt_editor` is the
    /// active editor, same as every other prompt purpose); `Some` is the heavy
    /// multi-line sub-state, entered on a curl/multi-line paste or `Ctrl-e`, for
    /// comfortably editing a pasted browser curl across lines before submit.
    /// Every OTHER prompt purpose never sets this — only `NewEndpoint` can
    /// transition into the `Some` sub-state.
    pub(in crate::tui::app) curl_prompt: Option<CurlPrompt>,
    /// The endpoint-switch target deferred behind an open
    /// [`ConfirmPurpose::DiscardChanges`] overlay; resolved by `s`/`d`, dropped
    /// by `Esc`.
    pending_load: Option<PendingLoad>,
    /// The buffer-close intent deferred behind an open
    /// [`ConfirmPurpose::DiscardChanges`] overlay (a dirty tab close). Keyed by
    /// path, resolved by `s`/`d`, aborted by `Esc`. Mutually exclusive with
    /// `pending_load` — only one is `Some` while the confirm is up.
    pending_close: Option<PendingClose>,
    /// A pasted-curl import whose derived name collides with an existing endpoint,
    /// parked behind an open [`ConfirmPurpose::ImportCollision`] overlay (U6).
    /// Resolved New/Overwrite/Cancel; dropped (nothing written) on Cancel. TUI
    /// only — the headless `churl import` path is non-interactive and never sets
    /// this.
    pending_curl_import: Option<PendingCurlImport>,
    /// The destination directory chosen in a `<leader>n`/`<leader>N` create
    /// gesture's destination picker, carried into the following name prompt. When
    /// `Some`, the create commits into it; when `None` (a cursor-context `n`/`N`),
    /// the commit falls back to the cursor's collection context.
    pending_create_dir: Option<PathBuf>,
    /// Which pane is zoomed, or `None` for the normal split.
    zoom: Option<ZoomPane>,
    /// Whether the explorer sidebar is hidden. Session-only.
    explorer_hidden: bool,
    /// Which sub-pane inside the left column has focus/zoom. The
    /// sequences sub-pane is always present (peek-symmetric), so
    /// this is only forced back to `Endpoints` when the workspace has no
    /// sequences at all.
    left_active: LeftPane,
    /// The pane focus held before focus last moved INTO the left column from a
    /// non-left pane. `<leader>e` hiding a focused explorer
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
    /// The save-time secret policy (strict blocks newly-authored name-anchored
    /// literals; warn never blocks). Resolved from config at startup; defaults to
    /// strict under snapshot-test construction.
    secret_policy: SecretPolicy,
    /// Abort handle for the in-flight sequence step, so a cancel/re-run aborts it.
    ///
    /// The unified sequence surface's run-face state, edit-face state, and
    /// active face live IN the [`Mode::Sequence`] variant (`{ view, editor,
    /// runner }`) so they cannot exist without the mode; this abort handle stays
    /// here because it outlives no mode — it must survive a Run→Edit face flip
    /// and is aborted on cancel/re-run/close.
    sequence_abort: Option<AbortHandle>,
    /// Abort handle for the single load-batch launcher task; aborting it drops
    /// the launcher's `buffer_unordered`, cancelling ALL in-flight requests.
    load_abort: Option<AbortHandle>,
    /// Concurrent-load guardrail caps (from `[load]` config, or defaults).
    load_caps: churl_core::load::LoadCaps,
    /// Debug-gated Advanced-limit overrides (M8.3 Wave 4): a new load run's
    /// default concurrency/total, the body-size cap, and the timeout.
    /// Session-scoped, mirroring `session_proxy`'s pattern — seeded from
    /// `Config::advanced_limits` in [`Self::install_runtime`], live-editable
    /// from the Settings panel's Debug category (reachable only when
    /// [`Self::debug_enabled`]) — also aliased directly by the Request
    /// category's Timeout/MaxBodyBytes rows, which share these same two
    /// fields (see [`crate::tui::components::settings::RequestRow`]'s doc).
    /// Never written back to `config.toml` by the app itself (M8.5 Wave 3's
    /// Save-as-default is an explicit user action, not automatic).
    pub(in crate::tui::app) advanced_limits: churl_core::config::ResolvedAdvancedLimits,
    /// The load runner's request, resolved ONCE at open time and cloned for every
    /// copy in a run (consistent batch — no per-copy re-resolution).
    load_request: Option<Request>,
    /// In-memory Session variable store, keyed by canonical workspace
    /// root so one workspace's captured secrets never leak into another. Populated
    /// when a sequence run extracts a value for a rule listed in that step's
    /// `persist`; read as the highest resolver scope for BOTH sequence-run and
    /// standalone-send resolution. **Never** written to disk (a security feature —
    /// captured tokens stay in RAM and evaporate on exit); there is no load/save.
    session_vars: HashMap<PathBuf, BTreeMap<String, String>>,
    /// Resolved per-request timeout, kept so a live proxy/insecure/cookies change
    /// can rebuild the client without re-reading config.
    client_timeout: Duration,
    /// The active proxy URL (session state; masked in any UI display). Resolved at
    /// launch (CLI `--proxy` > workspace `churl.toml` > global config > env → the
    /// env case is `None` here, reqwest honors it); editable live in the Settings
    /// panel. Every change rebuilds the client via [`App::rebuild_client`].
    session_proxy: Option<String>,
    /// Whether TLS verification is OFF for this session (accepts invalid/self-signed
    /// certs and hostname mismatches). A loud RED indicator surfaces it in the
    /// statusline. Toggled live by `<leader>k` / the Settings panel.
    session_insecure: bool,
    /// Whether the per-workspace persistent cookie jar is active this session.
    /// The jar itself ([`Self::cookie_jar`]) always exists and survives toggles;
    /// only whether it is wired into the client changes.
    cookies_enabled: bool,
    /// The workspace's cookie jar, shared (as `Arc`) with the client when
    /// [`Self::cookies_enabled`]. Held on `App` so toggling off→on keeps the
    /// accumulated cookies; persisted to `state.sqlite` on mutating sends, toggle
    /// off, clear, and exit.
    cookie_jar: Arc<ChurlCookieJar>,
    /// Off-UI-thread writer for the cookie jar: `persist_cookie_jar` serializes on
    /// the UI thread (cheap, in-RAM) then hands the snapshot here, so the blocking
    /// SQLite write never stalls the UI under WAL-lock contention. `None` in
    /// snapshot-test construction and when history is unavailable. Flushed and
    /// joined on quit so no write is lost.
    cookie_writer: Option<CookieJarWriter>,
    /// An error from the FINAL on-quit cookie flush, if any. Captured during the
    /// quit path (where a statusline message could never render) and surfaced by
    /// the caller to stderr *after* the terminal is restored, so a failed last
    /// write is reported loudly rather than swallowed.
    cookie_exit_error: Option<String>,
    /// Exactly which Settings-panel knobs the user has edited THROUGH THE
    /// PANEL this session (owner-decided fix: Save must persist only these,
    /// never a CLI/workspace/global-config value the panel merely displays —
    /// see [`super::components::settings::SettingKey`]'s doc). Populated by
    /// `handlers::settings::handle_settings_key`'s outcome arms (the ONLY
    /// place that inserts into it — never by [`Self::install_runtime`]'s
    /// resolution, and never by a keybinding that bypasses the panel, e.g.
    /// `<leader>k`/`<leader>D`); cleared by a successful Save.
    pub(in crate::tui::app) settings_touched:
        std::collections::HashSet<super::components::settings::SettingKey>,
    /// The net-change baseline for [`Self::settings_touched`]: every managed
    /// knob's value at **session start** (the panel's first-ever open) or at
    /// the moment of the last successful Save — NOT re-captured on every
    /// open (M8.5.3 fix; see [`Self::settings_baseline_established`]). A
    /// panel edit marks its knob touched only when the new value differs
    /// from THIS — so interacting with a knob without net-changing it (a
    /// toggle round-trip, an Enter-commit that changed nothing) leaves
    /// nothing to persist, while a genuinely changed-but-unsaved knob stays
    /// touched across a close/reopen instead of being silently re-baselined
    /// at its own dirty live value. Set by
    /// `handlers::settings::open_settings` (first open only)/`save_settings_defaults`;
    /// the value while the panel is closed is inert (never read).
    pub(in crate::tui::app) settings_baseline: churl_core::config::ResolvedSettings,
    /// Guards [`Self::settings_baseline`] against being re-seeded from the
    /// (possibly dirty, unsaved) live values on every Settings-panel open
    /// (M8.5.3 fix for the cross-close desync: edit a knob live, close
    /// without saving, reopen — the OLD code re-captured the baseline from
    /// the now-dirty live value, so a later net-zero adjust silently
    /// un-marked the knob and Save wrote nothing). `false` only before the
    /// very first open this session; `open_settings` seeds the baseline and
    /// flips this to `true` exactly once, then never re-seeds on a later
    /// open — only `save_settings_defaults` re-seeds it after that (a real,
    /// successful persistence, not a mere reopen).
    pub(in crate::tui::app) settings_baseline_established: bool,
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
/// when a reserved-name collision bumped the filename — the user must see
/// the real stem, never a silent rename.
fn disambiguated_stem(typed: &str, path: &Path) -> Option<String> {
    let stem = path.file_stem().and_then(|s| s.to_str())?;
    if stem == persistence::slug_of(typed) {
        None
    } else {
        Some(stem.to_owned())
    }
}

/// Renders a workspace-relative path as a portable, OS-independent identifier:
/// path components joined with `/` on every platform. These strings are
/// persisted (e.g. a sequence step's `endpoint`), so a path produced on Windows
/// must not embed `\` — otherwise the collection stops resolving when it moves
/// to another OS. Windows accepts `/` on the read side, so normalizing only the
/// written form is sufficient.
pub(in crate::tui::app) fn rel_to_logical(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<String>>()
        .join("/")
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
/// was disambiguated (reserved-name collision).
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

/// Normalises a bracketed-paste payload's line endings to `\n`: `\r\n` and lone
/// `\r` both collapse to `\n`. Terminals and multiplexers (tmux `paste-buffer`)
/// deliver paste newlines inconsistently as CR / CRLF / LF; downstream (the curl
/// importer's `\`-continuation logic, and the LineEditor buffer) only wants LF.
fn normalize_paste_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

impl App {
    /// How many consecutive history/load-summary write failures trigger the
    /// sticky "history not recording" status-bar flag. Set to 1 so the
    /// very first persistent failure surfaces stickily — a healthy session (zero
    /// failures) never shows it, so existing snapshots are unaffected.
    pub(in crate::tui::app) const HISTORY_FAIL_THRESHOLD: u32 = 1;

    /// Whether history writes are currently failing hard enough to warrant the
    /// sticky status-bar warning (drives [`statusline`] rendering).
    pub(in crate::tui::app) fn history_failing(&self) -> bool {
        self.history_write_failures >= Self::HISTORY_FAIL_THRESHOLD
    }

    /// Records the outcome of a history/load-summary write: a failure bumps the
    /// consecutive-failure counter (arming the sticky flag past the threshold); any
    /// success resets it to 0, clearing the flag.
    pub(in crate::tui::app) fn note_history_write(&mut self, ok: bool) {
        if ok {
            self.history_write_failures = 0;
        } else {
            self.history_write_failures = self.history_write_failures.saturating_add(1);
        }
    }

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
            jump: None,
            keymap,
            theme: Theme::default(),
            theme_name: super::components::settings::DEFAULT_THEME_NAME.to_owned(),
            theme_colors: BTreeMap::new(),
            leader_key: super::components::settings::DEFAULT_LEADER_KEY.to_owned(),
            cli_vars: BTreeMap::new(),
            active_profile: None,
            finder: FuzzyFinder::new(),
            should_quit: false,
            tx,
            rx,
            client: None,
            insecure_client: None,
            execute_options: ExecuteOptions::default(),
            generation: 0,
            orphan_response: ResponseState::Idle,
            orphan_geometry: ResponseGeometry::default(),
            pending_clipboard: None,
            body_search_editor: LineEditor::default(),
            body_search_return: Mode::Normal,
            inspector_return: Mode::Normal,
            debug_enabled: false,
            latest_trace: None,
            traffic: std::collections::VecDeque::new(),
            log_return: Mode::Normal,
            log_ring: LogRing::inert(super::log_subscriber::LOG_RING_CAPACITY),
            runner_leader_pending: false,
            highlight_tx: None,
            history: None,
            message: None,
            history_write_failures: 0,
            keymap_warnings: Vec::new(),
            keymap_warned: false,
            tick_count: 0,
            prompt_editor: LineEditor::default(),
            curl_prompt: None,
            pending_load: None,
            pending_close: None,
            pending_curl_import: None,
            pending_create_dir: None,
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
            secret_policy: SecretPolicy::Strict,
            sequence_abort: None,
            load_abort: None,
            load_caps: churl_core::load::LoadCaps::default(),
            advanced_limits: churl_core::config::Config::default().advanced_limits(),
            load_request: None,
            session_vars: HashMap::new(),
            client_timeout: DEFAULT_TIMEOUT,
            session_proxy: None,
            session_insecure: false,
            cookies_enabled: false,
            cookie_jar: Arc::new(ChurlCookieJar::new()),
            cookie_writer: None,
            cookie_exit_error: None,
            settings_touched: std::collections::HashSet::new(),
            settings_baseline: churl_core::config::ResolvedSettings::default(),
            settings_baseline_established: false,
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

    /// Sets a transient, auto-expiring message in the message row.
    fn notify(&mut self, text: impl Into<String>) {
        self.message = Some(Message::new(text));
    }

    /// Records the load-time keymap conflict/shadow warnings so the run
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

    /// Fires the first-frame keymap-warning toast exactly once. No-op
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
    /// row. A read-only peek — it does not load a buffer or move
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
    /// act on. Resolves by mode + runner focus: a runner whose Response
    /// region is focused routes the response actions onto its selected row/step;
    /// otherwise the main endpoint buffer (today's behaviour) is active. When the
    /// body-search input is open it resolves against the mode search was opened
    /// over ([`Self::body_search_return`]), so search-into-view keeps targeting the
    /// runner response the `/` was launched from.
    fn active_response_surface(&self) -> ResponseSurface {
        // When body-search is open, resolve against the mode it was opened over —
        // parked in `body_search_return` (it holds the whole mode, incl.
        // the `LoadRunner(state)` / `Sequence{..}` payloads, so the runner surface
        // stays reachable).
        let effective = if matches!(self.mode, Mode::BodySearch) {
            &self.body_search_return
        } else {
            &self.mode
        };
        match effective {
            Mode::LoadRunner(runner) => {
                if runner.focus == load_runner::RunnerFocus::Response
                    && !runner.response_input_captured()
                {
                    ResponseSurface::LoadRunner
                } else {
                    ResponseSurface::Main
                }
            }
            // The Run-face runner lives in the `Mode::Sequence` payload
            // (of the effective mode), not a parallel field.
            Mode::Sequence {
                view: SeqView::Run,
                runner: Some(runner),
                ..
            } if runner.focus == sequence_runner::RunnerFocus::Response
                && !runner.response_input_captured() =>
            {
                ResponseSurface::Sequence
            }
            _ => ResponseSurface::Main,
        }
    }

    /// The active response state for internal readers (render + `response_*`
    /// handlers). Resolves the runner's selected row/step when a runner Response
    /// region is focused, else the active endpoint buffer, else the
    /// test-only orphan slot (isolation snapshots) when nothing is loaded.
    fn active_response(&self) -> &ResponseState {
        match self.active_response_surface() {
            ResponseSurface::LoadRunner => self
                .load_runner()
                .and_then(|r| r.selected_response())
                .unwrap_or(&self.orphan_response),
            ResponseSurface::Sequence => self
                .sequence_runner()
                .and_then(|r| r.selected_response())
                .unwrap_or(&self.orphan_response),
            ResponseSurface::Main => match self.active_endpoint_buffer() {
                Some(b) => &b.response,
                None => &self.orphan_response,
            },
        }
    }

    /// Mutable active response, resolving the runner's selected row/step when a
    /// runner Response region is focused, else the active buffer, else
    /// the orphan slot when nothing is loaded (isolation snapshots).
    fn active_response_mut(&mut self) -> &mut ResponseState {
        match self.active_response_surface() {
            // Borrow the runner directly out of the mode / parked mode (disjoint
            // from `orphan_response`, which the `unwrap_or` also borrows mutably —
            // a whole-`self` accessor would alias here, so match the field inline).
            ResponseSurface::LoadRunner => {
                Self::load_runner_in_mut(&mut self.mode, &mut self.body_search_return)
                    .and_then(|r| r.selected_response_mut())
                    .unwrap_or(&mut self.orphan_response)
            }
            ResponseSurface::Sequence => {
                Self::sequence_runner_in_mut(&mut self.mode, &mut self.body_search_return)
                    .and_then(|r| r.selected_response_mut())
                    .unwrap_or(&mut self.orphan_response)
            }
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
    /// geometry.
    fn active_response_geometry(&self) -> &ResponseGeometry {
        match self.active_response_surface() {
            ResponseSurface::LoadRunner => self
                .load_runner()
                .map(|r| &r.geometry)
                .unwrap_or(&self.orphan_geometry),
            ResponseSurface::Sequence => self
                .sequence_runner()
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
            ResponseSurface::LoadRunner => {
                Self::load_runner_in_mut(&mut self.mode, &mut self.body_search_return)
                    .map(|r| &mut r.geometry)
                    .unwrap_or(&mut self.orphan_geometry)
            }
            ResponseSurface::Sequence => {
                Self::sequence_runner_in_mut(&mut self.mode, &mut self.body_search_return)
                    .map(|r| &mut r.geometry)
                    .unwrap_or(&mut self.orphan_geometry)
            }
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

    /// Opens the Inspector overlay directly over `trace` (tests / snapshot
    /// drivers) — mirrors [`Self::set_response`]'s seam for the response
    /// viewer: bypasses a real send (which needs a live HTTP exchange to
    /// produce redirect hops / errors) so a snapshot can drive a
    /// representative trace deterministically.
    pub fn open_inspector_with_trace(&mut self, trace: DebugTrace) {
        self.latest_trace = Some(Box::new(trace));
        self.open_inspector();
    }

    /// Pushes synthetic events directly into the Log ring (tests / snapshot
    /// drivers) — mirrors [`Self::open_inspector_with_trace`]'s bypass
    /// seam: the real `tracing` subscriber is attached only once, by
    /// [`super::run`], and never under `TestBackend`, so this is the only
    /// way to drive the Log panel deterministically outside a real session.
    pub fn seed_log_ring_for_test(
        &mut self,
        events: impl IntoIterator<Item = super::log_subscriber::LogEvent>,
    ) {
        for event in events {
            self.log_ring.push(event);
        }
    }

    /// Pushes `entry` directly into the session Traffic feed (tests /
    /// snapshot drivers) — same bypass rationale as
    /// [`Self::open_inspector_with_trace`]/[`Self::seed_log_ring_for_test`]:
    /// no live HTTP exchange is available in a snapshot test, so this drives
    /// the Traffic-backed Inspector browse UX deterministically.
    pub fn push_traffic_for_test(&mut self, entry: TrafficEntry) {
        traffic::push(&mut self.traffic, entry);
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

    /// Whether the URL popup edtui editor is in an insert-capable (non-Normal)
    /// mode on the active buffer — the gate for routing a paste there so it lands
    /// at the cursor rather than triggering vim's Normal-mode paste-after.
    fn url_popup_non_normal(&self) -> bool {
        self.active_endpoint_buffer()
            .and_then(|b| b.url_popup.as_ref())
            .is_some_and(|e| e.mode != EditorMode::Normal)
    }

    /// Whether the new-endpoint multi-line curl editor is in an insert-capable
    /// (non-Normal) mode — the same edtui + vim_ext pairing as the URL popup, so
    /// it needs the same paste gate: a paste routed to it in Normal mode would
    /// trigger vim's paste-after and land one char off.
    fn curl_prompt_non_normal(&self) -> bool {
        self.curl_prompt
            .as_ref()
            .is_some_and(|c| c.editor.mode != EditorMode::Normal)
    }

    /// Routes a bracketed-paste payload into the active text surface as literal
    /// characters (newlines never interpreted as submit). The routing MIRRORS the
    /// key precedence in [`Self::handle_key`] exactly, so every surface that
    /// accepts typed keys also accepts a paste — otherwise, with bracketed paste
    /// enabled globally, a paste into an unrouted surface would silently vanish.
    ///
    /// `LineEditor` / picker surfaces get newlines normalised to `\n` (single-line
    /// editors never want a raw CR; the curl prompt's `\`-continuation logic keys
    /// on LF). The edtui multi-line surfaces (Body editor / URL popup) get the RAW
    /// payload so a pasted body keeps its CRLF, and are gated on an insert-capable
    /// mode. When no text surface is active (Normal mode with no inline editor,
    /// jump, a y/n confirm, a pending leader, the method menu) the paste is dropped
    /// — never replayed as keys, which would fire commands.
    fn handle_paste(&mut self, text: String) {
        // Keyboard-owning overlays first (mirrors `handle_key`'s top guards).
        if self.help_open {
            if self.help_search_input {
                self.help_search_editor
                    .insert_str(&normalize_paste_newlines(&text));
            }
            return;
        }
        if self.url_popup_active() {
            // edtui, raw payload, only when insert-capable (avoid vim paste-after
            // landing it one char off in Normal mode).
            if self.url_popup_non_normal()
                && let Some(b) = self.active_endpoint_buffer_mut()
                && let Some(editor) = b.url_popup.as_mut()
            {
                b.url_popup_events.on_paste_event(text, editor);
            }
            return;
        }
        if self.leader.is_some() {
            return;
        }
        match &self.mode {
            Mode::Search | Mode::Palette | Mode::WorkspacePicker | Mode::SequencePicker => {
                let s = normalize_paste_newlines(&text);
                if let Some(picker) = self.picker.as_mut().map(Picker::state_mut) {
                    picker.push_str(&s, &mut self.finder);
                }
            }
            Mode::BodySearch => {
                let s = normalize_paste_newlines(&text);
                self.body_search_editor.insert_str(&s);
            }
            Mode::Prompt(PromptPurpose::NewEndpoint) if self.curl_prompt.is_some() => {
                // Already expanded: normalise newlines to `\n` (the curl parser
                // keys on LF continuations). Gated on insert-capable mode exactly
                // like the URL popup above — a second bracketed paste can arrive
                // while the editor still sits in Normal mode (right after a
                // curl-paste switch, or a parse-fail re-open), and routing it
                // there would trigger vim's Normal-mode paste-after, landing one
                // char off instead of at the cursor.
                if self.curl_prompt_non_normal()
                    && let Some(c) = self.curl_prompt.as_mut()
                {
                    let s = normalize_paste_newlines(&text);
                    c.events.on_paste_event(s, &mut c.editor);
                }
            }
            Mode::Prompt(PromptPurpose::NewEndpoint) => {
                // Single-line sub-state: a paste expands into the multi-line
                // editor instead of becoming part of the name, landing in Normal
                // mode, ready to review/edit/submit. The paste is consumed here —
                // it never also reaches `prompt_editor.insert_str` below, so
                // there is no double-insert.
                let s = normalize_paste_newlines(&text);
                let trimmed = s.trim();
                if handlers::crud::looks_like_curl(trimmed) {
                    // The paste itself is a curl: seed with the curl ALONE,
                    // discarding any already-typed prefix. Prepending it (as the
                    // multi-line branch below does) would make the seed
                    // `<prefix>curl …`, which fails `looks_like_curl` on submit
                    // and silently degrades the import into a plain endpoint
                    // named `<prefix>curl …` — a curl import derives its name
                    // from the URL, so the half-typed prefix is intentionally
                    // dropped, not merged.
                    self.open_curl_editor(&s, EditorMode::Normal);
                } else if s.contains('\n') {
                    // Multi-line but not a recognized curl: this is name/notes
                    // text typed across lines, not an import, so prior typed
                    // text is real content — it was typed first, so it prefixes
                    // the pasted content in the seed.
                    let seed = format!("{}{s}", self.prompt_editor.text());
                    self.open_curl_editor(&seed, EditorMode::Normal);
                } else {
                    // Short single-line non-curl paste: same as every other
                    // prompt purpose, insert it into the name as if typed.
                    self.prompt_editor.insert_str(&s);
                }
            }
            Mode::Prompt(_) => {
                let s = normalize_paste_newlines(&text);
                self.prompt_editor.insert_str(&s);
            }
            Mode::EnvEditor(_) => {
                let s = normalize_paste_newlines(&text);
                if let Mode::EnvEditor(editor) = &mut self.mode {
                    editor.paste(&s);
                }
            }
            Mode::Settings(_) => {
                let s = normalize_paste_newlines(&text);
                if let Mode::Settings(state) = &mut self.mode {
                    state.paste(&s);
                }
            }
            Mode::Sequence { .. } => {
                let s = normalize_paste_newlines(&text);
                if let Some(editor) = self.sequence_editor_mut() {
                    editor.paste(&s);
                }
            }
            Mode::LoadRunner(_) => {
                let s = normalize_paste_newlines(&text);
                if let Mode::LoadRunner(state) = &mut self.mode {
                    state.paste(&s);
                }
            }
            // The Inspector and Log panel are read-only (no text-input
            // surface), same as Jump/MethodMenu/Confirm — a paste while
            // either is open has nowhere to land.
            Mode::Jump
            | Mode::MethodMenu
            | Mode::Confirm(_)
            | Mode::Inspector(_)
            | Mode::LogPanel(_) => {}
            Mode::Normal => self.handle_paste_normal(text),
        }
    }

    /// The `Mode::Normal` half of [`Self::handle_paste`], mirroring
    /// [`Self::handle_normal_key`]'s inline-editor precedence: inline URL edit →
    /// request-row field edit (both `LineEditor`, normalised) → Body edtui in an
    /// insert-capable mode (raw payload). Anything else (a bare focused pane, a
    /// keymap-action context) has no text surface, so the paste is dropped.
    fn handle_paste_normal(&mut self, text: String) {
        if self.url_editor_active() {
            let s = normalize_paste_newlines(&text);
            if let Some(b) = self.active_endpoint_buffer_mut()
                && let Some(editor) = b.url_editor.as_mut()
            {
                editor.insert_str(&s);
            }
            return;
        }
        if self.focus == Pane::Request && self.tabs_editing_active() {
            let s = normalize_paste_newlines(&text);
            if let Some(b) = self.active_endpoint_buffer_mut()
                && let Some(edit) = b.tabs.editing.as_mut()
            {
                edit.editor.insert_str(&s);
            }
            return;
        }
        if self.focus == Pane::Request
            && self.active_tab() == RequestTab::Body
            && self.body_editor_non_normal()
            && let Some(b) = self.active_endpoint_buffer_mut()
        {
            b.editor_events.on_paste_event(text, &mut b.editor);
        }
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

    /// Sets the save-time secret policy (resolved from config; see
    /// [`churl_core::config::Config::secret_policy`]).
    pub fn set_secret_policy(&mut self, policy: SecretPolicy) {
        self.secret_policy = policy;
    }

    /// Wires the REAL `tracing` ring built by [`super::log_subscriber::init`]
    /// (attached exactly once at TUI startup) into the app, replacing the
    /// inert placeholder [`App::new`] seeded. The ring has no runtime gate of
    /// its own — debug on/off is enforced at the emit sites, and the
    /// attached layer's target filter keeps third-party logs out (see
    /// [`super::log_subscriber`]'s docs).
    pub fn set_log_ring(&mut self, ring: LogRing) {
        self.log_ring = ring;
    }

    /// Installs the runtime-dependent pieces: the HTTP client (with the
    /// config-resolved timeout), the execution options (body-size cap), the
    /// off-thread highlight worker, and the history store. Called from
    /// [`super::run`] after [`App::new`]; snapshot tests skip it so they stay
    /// runtime-free. A failed history open is non-fatal — it disables history
    /// and warns on the statusline.
    pub fn install_runtime(
        &mut self,
        config: &Config,
        cli_proxy: Option<String>,
        cli_insecure: bool,
    ) -> Result<()> {
        // M8.5: seed the Settings panel's Appearance working copy — the theme
        // NAME (Self::theme is already the resolved style table with no way
        // back to the name) and the `[theme_colors]` overrides (kept only so
        // a live theme-name switch can re-resolve without losing them), plus
        // the leader-key working copy (not live-applied — see its field doc).
        self.theme_name = config
            .theme
            .clone()
            .unwrap_or_else(|| super::components::settings::DEFAULT_THEME_NAME.to_owned());
        self.theme_colors = config.theme_colors.clone();
        self.leader_key = config
            .leader_key
            .clone()
            .unwrap_or_else(|| super::components::settings::DEFAULT_LEADER_KEY.to_owned());

        // M8.3 Wave 4: resolve the debug-gated advanced-limit overrides
        // BEFORE `execute_options`/`client_timeout` are built from them, so a
        // persisted `[advanced]` override (hand-edited `config.toml`, or a
        // prior session's Settings-panel edit) takes effect from the very
        // first send.
        self.advanced_limits = config.advanced_limits();
        self.execute_options = ExecuteOptions {
            max_body_bytes: self.advanced_limits.body_cap_bytes,
            redirect: config.redirect()?,
        };
        self.load_caps = config.load_caps();
        // M8.3: seed the session debug-capture toggle from the persisted
        // config knob; `<leader>D` flips it live for the rest of the session
        // without writing back to `config.toml` (mirrors `session_insecure`).
        self.debug_enabled = config.debug;
        // A spawn failure degrades to plain rendering: `spawn` returns
        // `None`, `highlight_tx` stays `None`, and every render site already guards
        // on `if let Some(tx) = &app.highlight_tx`, so the viewer renders fine.
        self.highlight_tx = highlight::spawn(self.tx.clone(), self.theme.is_light());
        match default_state_path() {
            Some(path) => match HistoryStore::open(&path) {
                Ok(store) => {
                    self.history = Some(store);
                    // Spawn the off-thread cookie writer against the same DB. A
                    // spawn failure is non-fatal: the jar still works in RAM, but
                    // persistence is disabled with a loud note.
                    match CookieJarWriter::spawn(&path) {
                        Ok(writer) => self.cookie_writer = Some(writer),
                        Err(err) => {
                            self.message =
                                Some(Message::new(format!("cookie persistence disabled: {err}")));
                        }
                    }
                }
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

        // Resolve the M8 session controls, then build the client from them.
        self.client_timeout = Duration::from_secs(self.advanced_limits.timeout_secs);
        let ws_proxy = self
            .workspace
            .as_ref()
            .and_then(|ws| ws.manifest().proxy.clone());
        // Precedence: CLI > workspace churl.toml > global config; a `None` result
        // means "no explicit proxy", so reqwest honors the env proxy for free.
        self.session_proxy = pure::resolve_proxy(cli_proxy, ws_proxy, config.proxy());
        // `-k` forces insecure on; otherwise the global default applies (there is
        // no CLI "force secure" — the safe default is already secure).
        self.session_insecure = cli_insecure || config.insecure();
        // Cookie jar opt-in: per-workspace `churl.toml cookies = true` OR the
        // global default turns it on.
        let ws_cookies = self
            .workspace
            .as_ref()
            .is_some_and(|ws| ws.manifest().cookies);
        self.cookies_enabled = ws_cookies || config.cookies();
        // Load any persisted jar for this workspace, so cookies survive a restart.
        self.cookie_jar = Arc::new(self.load_cookie_jar());

        self.rebuild_client()?;
        Ok(())
    }

    /// Loads the persisted cookie jar for the current workspace from `state.sqlite`
    /// (keyed by canonicalized root path), or an empty jar when there is no
    /// workspace / history store / stored blob. A corrupt blob degrades to an
    /// empty jar with a status note rather than aborting launch.
    fn load_cookie_jar(&mut self) -> ChurlCookieJar {
        let Some(key) = self.cookie_workspace_key() else {
            return ChurlCookieJar::new();
        };
        let Some(store) = self.history.as_ref() else {
            return ChurlCookieJar::new();
        };
        match store.cookie_jar(&key) {
            Ok(Some(json)) => match ChurlCookieJar::load_json(&json) {
                Ok(jar) => jar,
                Err(err) => {
                    self.message = Some(Message::new(format!("cookie jar reset (corrupt): {err}")));
                    ChurlCookieJar::new()
                }
            },
            Ok(None) => ChurlCookieJar::new(),
            Err(err) => {
                self.message = Some(Message::new(format!("cookies not loaded: {err}")));
                ChurlCookieJar::new()
            }
        }
    }

    /// The canonicalized workspace-root string used to key the cookie jar in
    /// `state.sqlite`, or `None` when no workspace is open.
    fn cookie_workspace_key(&self) -> Option<String> {
        self.workspace
            .as_ref()
            .map(|ws| canonical_path(ws.root()).to_string_lossy().into_owned())
    }

    /// Rebuilds the single shared client from the current session controls. This
    /// is the ONE seam every live proxy/insecure/cookies change routes through —
    /// there is never more than one client, and it always reflects current state.
    /// The cookie jar is wired in only when enabled; the `Arc` outlives the client,
    /// so toggling cookies off→on keeps the accumulated cookies.
    pub(in crate::tui::app) fn rebuild_client(&mut self) -> Result<()> {
        let cfg = ClientConfig {
            timeout: self.client_timeout,
            proxy: self.session_proxy.clone(),
            insecure: self.session_insecure,
            cookies: self.cookies_enabled.then(|| self.cookie_jar.clone()),
        };
        self.client = Some(build_client_with(&cfg)?);
        // Any session-knob change invalidates the cached insecure variant so it
        // rebuilds (lazily) against the new timeout/proxy/cookies next time an
        // opted-in endpoint sends.
        self.insecure_client = None;
        Ok(())
    }

    /// The client an endpoint's send must use, honoring the **effective** insecure
    /// flag (`endpoint.request.insecure || session_insecure`). The common case
    /// returns the shared [`Self::client`] untouched. Only the divergent case — a
    /// verifying session sending an endpoint that individually opted into insecure
    /// — needs a different client; that is served by a lazily-built, cached
    /// insecure variant mirroring the shared client's other knobs. A sibling secure
    /// endpoint in the same session keeps using the verifying shared client.
    pub(in crate::tui::app) fn client_for(&mut self, request: &Request) -> Option<Client> {
        if !request.insecure || self.session_insecure {
            // No divergence: either the endpoint didn't opt in, or the whole
            // session is already insecure — the shared client already matches.
            return self.client.clone();
        }
        if self.insecure_client.is_none() {
            // Snapshot-test / runtime-free construction: no shared client means no
            // runtime, so there is nothing to send with either.
            self.client.as_ref()?;
            let cfg = ClientConfig {
                timeout: self.client_timeout,
                proxy: self.session_proxy.clone(),
                insecure: true,
                cookies: self.cookies_enabled.then(|| self.cookie_jar.clone()),
            };
            match build_client_with(&cfg) {
                Ok(client) => self.insecure_client = Some(client),
                Err(err) => {
                    self.message =
                        Some(Message::new(format!("insecure client build failed: {err}")));
                    return None;
                }
            }
        }
        self.insecure_client.clone()
    }

    /// Persists the current cookie jar for this workspace. Called after a send
    /// that may have mutated the jar, on toggle-off / clear, and on exit. The jar
    /// still works in memory regardless.
    ///
    /// Serialization runs here on the UI thread (cheap, in-RAM); the blocking
    /// SQLite write is handed to the off-thread [`CookieJarWriter`] so the UI never
    /// stalls on the WAL write-lock. The write is SKIPPED on a serialize failure so
    /// a transient error can never clobber the good on-disk blob with `""` (which
    /// `ON CONFLICT DO UPDATE` would persist as an empty jar = silent permanent
    /// cookie loss) — the writer therefore only ever receives a good blob.
    ///
    /// A failure from a *prior* off-thread write is polled and surfaced FIRST (it
    /// can only be known now, asynchronously — it is not this call's write), worded
    /// as a previous save so the message is not misattributed to the current one.
    pub(in crate::tui::app) fn persist_cookie_jar(&mut self) {
        // Surface any error from an earlier off-thread write before enqueuing a new
        // one (Constitution: fail loudly, never swallow). Read into a local so the
        // immutable borrow of `cookie_writer` ends before we touch `self.message`.
        let prior_error = self.cookie_writer.as_ref().and_then(|w| w.take_error());
        if let Some(err) = prior_error {
            self.message = Some(Message::new(format!(
                "a previous cookie save failed: {err}"
            )));
        }
        let Some(key) = self.cookie_workspace_key() else {
            return;
        };
        let json = match self.cookie_jar.to_json() {
            Ok(json) => json,
            Err(err) => {
                // Do NOT enqueue — persisting an empty/partial blob would wipe a
                // good jar. Fail loud instead.
                self.message = Some(Message::new(format!(
                    "cookie save failed (serialize): {err}"
                )));
                return;
            }
        };
        let Some(writer) = self.cookie_writer.as_ref() else {
            return;
        };
        writer.enqueue(key, json, now_ms());
    }

    /// Runs the event loop: `tokio::select!` over the crossterm event stream, a
    /// 250 ms tick, and the app channel. Draws once per iteration; never awaits
    /// I/O other than these three sources.
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        // First-frame keymap-warning toast: if the loaded config has
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
                    // A bracketed paste (enabled in `tui::init`): route the whole
                    // clipboard payload into the active text surface as literal
                    // characters — newlines are preserved in the buffer instead of
                    // acting as Enter, so a multi-line curl reaches `import_curl`
                    // intact on submit.
                    Some(Ok(Event::Paste(text))) => self.handle_paste(text),
                    Some(Ok(_)) => {} // resize etc. — redraw happens next iteration
                    Some(Err(err)) => return Err(err.into()),
                    None => break, // input stream closed
                },
                _ = tick.tick() => {
                    self.tick_count = self.tick_count.wrapping_add(1);
                    if self.message.as_ref().is_some_and(|s| s.is_expired()) {
                        self.message = None;
                    }
                    // Re-mask an ephemeral secret peek on timeout, on the same
                    // 250 ms cadence that expires messages.
                    if let Mode::EnvEditor(editor) = &mut self.mode {
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
        // Quit path: the loop has exited but the tokio runtime is still
        // alive, so record an interrupted history row for every buffer with an
        // in-flight request BEFORE the runtime drops and aborts those tasks. Their
        // `AppMsg::Response` would otherwise never land and the request — possibly
        // already completed on the wire — would vanish from history with no trace.
        self.record_inflight_on_quit();
        // Flush the cookie jar on exit so a session's persistent cookies survive
        // to the next launch (best-effort; a no-op when cookies are disabled or no
        // workspace is open). This enqueues the final snapshot; the shutdown below
        // then drains it to disk before the writer thread exits — no lost writes.
        if self.cookies_enabled {
            self.persist_cookie_jar();
        }
        // Synchronously flush + join the off-thread writer so the last snapshot is
        // durable before the process (and the tokio runtime) tears down. A failed
        // FINAL flush is stashed for the caller to print AFTER terminal restore —
        // no statusline exists here, and `take_error` polling has stopped.
        if let Some(mut writer) = self.cookie_writer.take() {
            self.cookie_exit_error = writer.shutdown();
        }
        Ok(())
    }

    /// Takes the error (if any) from the final on-quit cookie flush, for the caller
    /// to surface to stderr after the terminal is restored.
    pub fn take_cookie_exit_error(&mut self) -> Option<String> {
        self.cookie_exit_error.take()
    }

    /// On quit, mirror [`Self::cancel_request`] for every in-flight buffer: abort
    /// the task and write an interrupted history row (`write_history(meta, None,
    /// None)`), so a request the user quit mid-flight is still recorded.
    /// Synchronous — never blocks quit on network completion.
    fn record_inflight_on_quit(&mut self) {
        // Drain each buffer's in-flight handle first (a short-lived immutable
        // borrow), then write history (needs `&mut self`) — same ordering the
        // response/cancel paths use to avoid overlapping borrows.
        let in_flight: Vec<InFlightRequest> = self
            .buffers
            .iter_mut()
            .filter_map(|b| b.as_endpoint_mut().and_then(|e| e.in_flight.take()))
            .collect();
        for req in in_flight {
            req.handle.abort();
            self.write_history(&req.meta, None, None);
        }
    }

    /// Routes one key event (see the module docs for the precedence rules).
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // Modal, keyboard-owning overlays take precedence over
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
        // The narrow leader-from-runner allowlist (M8.3 Wave 4, see
        // `Self::runner_leader_pending`'s doc): the leader key was pressed
        // while a load/sequence runner mode was active, and this is the
        // follow-up key. Checked here, before the mode match below, so it
        // takes precedence over the runner's own key handling exactly like
        // the general `self.leader` case above — but resolves against ONLY
        // the two debug-overlay chords, never the full leader menu.
        if self.runner_leader_pending {
            return self.handle_runner_leader_key(key);
        }
        // `Mode` is non-`Copy`, so match by reference and dispatch. The
        // payload-carrying overlay arms (`EnvEditor`/`LoadRunner`) delegate to a
        // handler that re-borrows the state out of `self.mode` itself; the small
        // `Copy` purposes are dereferenced out before the `&mut self` call so no
        // borrow of `self.mode` outlives the dispatch.
        match &self.mode {
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
            Mode::Prompt(purpose) => {
                let purpose = *purpose;
                self.handle_prompt_key(key, purpose)
            }
            Mode::Confirm(purpose) => {
                let purpose = *purpose;
                self.handle_confirm_key(key, purpose)
            }
            Mode::EnvEditor(_) => self.handle_env_editor_key(key),
            Mode::Settings(_) => self.handle_settings_key(key),
            Mode::Sequence { .. } => self.handle_sequence_key(key),
            Mode::LoadRunner(_) => self.handle_load_runner_key(key),
            Mode::Inspector(_) => {
                self.handle_inspector_key(key);
                Ok(())
            }
            Mode::LogPanel(_) => {
                self.handle_log_panel_key(key);
                Ok(())
            }
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
        // 3. edtui insert/visual mode on the Body tab (interception exception).
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
        //     Jump key there (DECISIONS.md shadowing precedent). Precedes
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
            Action::Reload => self.reload_workspace()?,
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
            Action::OpenSettings => self.open_settings(),
            Action::ToggleInsecure => self.toggle_insecure(),
            Action::ToggleEndpointInsecure => self.toggle_endpoint_insecure(),
            Action::OpenInspector => self.open_inspector(),
            Action::ToggleDebug => self.toggle_debug(),
            Action::OpenLogPanel => self.open_log_panel(),
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
            // Leader create gestures always ask *where* via the destination picker.
            Action::NewEndpointPick => self.begin_new_endpoint_pick(),
            Action::NewCollectionPick => self.begin_new_collection_pick(),
            Action::NewSequence => self.new_sequence_prompt(),
            // `r` on the sequences sub-pane runs the hovered sequence (Run face);
            // everywhere else it renames the selected tree node.
            Action::Rename if self.left_column_on_sequences() => self.run_selected_sequence(),
            Action::Rename => self.begin_rename(),
            // `d` on the sequences sub-pane deletes the hovered sequence (parallels
            // endpoints `d`); everywhere else it deletes the selected tree node.
            Action::Delete if self.left_column_on_sequences() => self.begin_delete_sequence(),
            Action::Delete => self.begin_delete(),
            Action::DeleteSequence => self.begin_delete_sequence(),
            // Tree CRUD (M7.12): move-to/copy-to open the destination picker;
            // duplicate + reorder act on the selected node's kind in place.
            Action::MoveTo => self.begin_relocate(true),
            Action::CopyTo => self.begin_relocate(false),
            Action::Duplicate => self.duplicate_selected()?,
            Action::MoveUp => self.reorder_selected(persistence::ReorderDir::Up)?,
            Action::MoveDown => self.reorder_selected(persistence::ReorderDir::Down)?,
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
            Action::StructuralNext => self.response_structural_jump(true),
            Action::StructuralPrev => self.response_structural_jump(false),
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
            // Retired as a distinct flow — the shared new-endpoint name prompt
            // auto-detects a pasted curl. Kept as a palette alias that opens it.
            Action::PasteCurl => self.begin_new_endpoint(),
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
    /// in-pane focus/zoom. Guards the sequence-specific `h/j/k/l/Enter/r`
    /// routing so it never fires while the endpoints tree is active.
    fn left_column_on_sequences(&self) -> bool {
        self.focus == Pane::Explorer && self.left_active == LeftPane::Sequences
    }

    fn explorer_action(&mut self, action: Action) -> Result<()> {
        // When the sequences sub-pane is the active occupant of the left
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
        selected.file.strip_prefix(root).ok().map(rel_to_logical)
    }

    // ---- Response viewer actions ----

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

    /// The shared query/filter/selection state of the open picker overlay, if any.
    /// The finder+items+index live inside the [`Picker`] variant, so
    /// render/overlay-key/tests reach it uniformly through this seam.
    pub fn picker_state(&self) -> Option<&picker::PickerState> {
        self.picker.as_ref().map(Picker::state)
    }

    /// The shared picker state (mutable) of the open picker overlay, if any.
    pub fn picker_state_mut(&mut self) -> Option<&mut picker::PickerState> {
        self.picker.as_mut().map(Picker::state_mut)
    }

    /// Whether the open picker is a `<leader>l f` load-runner target pick — the
    /// `after_pick` intent that folded the old `load_runner_after_pick` bool.
    /// `false` unless the open picker is a `Search` with the intent armed. Test
    /// seam replacing direct field reads.
    #[cfg(test)]
    pub(crate) fn picker_after_pick(&self) -> bool {
        matches!(
            self.picker,
            Some(Picker::Search {
                after_pick: true,
                ..
            })
        )
    }

    /// Whether the open sequence picker is a `<leader>s r` run pick — the `runs`
    /// intent that folded the old `sequence_pick_runs` bool. `false` unless the
    /// open picker is a `Sequence` with `runs`. Test seam.
    #[cfg(test)]
    pub(crate) fn picker_sequence_runs(&self) -> bool {
        matches!(self.picker, Some(Picker::Sequence { runs: true, .. }))
    }

    /// The profile-name choices behind an open profile picker (empty when the open
    /// picker isn't a `Profile`). Test seam replacing the old `profile_choices`.
    #[cfg(test)]
    pub(crate) fn picker_profiles(&self) -> &[Option<String>] {
        match &self.picker {
            Some(Picker::Profile { profiles, .. }) => profiles,
            _ => &[],
        }
    }

    /// The workspace-path choices behind an open workspace picker (empty when the
    /// open picker isn't a `Workspace`). Test seam replacing `workspace_choices`.
    #[cfg(test)]
    pub(crate) fn picker_workspaces(&self) -> &[PathBuf] {
        match &self.picker {
            Some(Picker::Workspace { paths, .. }) => paths,
            _ => &[],
        }
    }

    /// Whether the open picker is the auth-kind picker (`Picker::Auth`) — the
    /// variant that folded the old `auth_picker` bool. Test seam.
    #[cfg(test)]
    pub(crate) fn picker_is_auth(&self) -> bool {
        matches!(self.picker, Some(Picker::Auth { .. }))
    }

    fn open_search(&mut self) -> Result<()> {
        let items = super::components::search::open(&mut self.explorer)?;
        // The targets travel WITH the finder in the variant, so the
        // accepted index can only address its own list. `after_pick` starts false;
        // `open_load_runner_pick` flips it right after (a `<leader>l f` pick).
        self.picker = Some(Picker::Search {
            state: items.picker,
            targets: items.targets,
            after_pick: false,
        });
        self.mode = Mode::Search;
        // Opening search parses every collection eagerly — surface any files the
        // lenient load skipped.
        self.surface_explorer_warnings();
        Ok(())
    }

    fn open_palette(&mut self) {
        let items = palette::open();
        self.picker = Some(Picker::Palette {
            state: items.picker,
            actions: items.actions,
        });
        self.mode = Mode::Palette;
    }

    /// Enters jump-mode, labelling the five pane regions (pane-only, no row
    /// labels; row precision is the leader pickers' job).
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
                // pane zooms in and shows an informative empty state;
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
        self.picker = Some(Picker::Profile {
            state: picker::PickerState::new(" Switch profile ", labels),
            profiles: choices,
        });
        self.mode = Mode::Palette;
    }

    /// Enter (or a jump label) on the current explorer row: loads the endpoint
    /// through the guarded seam. One seam so both the explorer `Enter` and
    /// jump-mode agree. (Sequences are not tree rows — they activate via
    /// the sub-pane.)
    fn activate_explorer_row(&mut self, row: usize) -> Result<()> {
        self.explorer.cursor = row;
        self.guarded_load(PendingLoad::Row(row))
    }

    // ---- concurrent-load runner ----

    /// `<leader>l f` / palette: pick an endpoint via the fuzzy search overlay,
    /// then open the load runner over it. Reuses the endpoint-search overlay + its
    /// dirty-safe load path; a one-shot flag makes `accept_overlay` chain into
    /// `open_load_runner` once the endpoint has loaded.
    fn open_load_runner_pick(&mut self) -> Result<()> {
        self.open_search()?;
        // The one-shot intent is the `after_pick` field of the freshly-opened
        // `Picker::Search` variant, so the intent can't outlive its picker or leak
        // onto a non-search picker.
        if let Some(Picker::Search { after_pick, .. }) = &mut self.picker {
            *after_pick = true;
        }
        Ok(())
    }

    /// `r` in the runner: classify the config against the guardrail caps and act.
    /// Refuse → message (no run); Warn → loud confirm naming the target URL; Ok →
    /// run immediately.
    fn request_load_run(&mut self) {
        let Some(runner) = self.load_runner() else {
            return;
        };
        // Copy/clone what the arms need so the runner borrow (into `self.mode`)
        // ends before any `&mut self` method (`notify`/`load_runner_mut`).
        let cfg = runner.cfg;
        let url = runner.url.clone();
        match churl_core::load::check_config(&cfg, &self.load_caps) {
            LoadCheck::Refuse(reason) => self.notify(format!("load refused: {reason}")),
            LoadCheck::Warn(reason) => {
                let text = format!(
                    "Fire {} requests at concurrency {} against {url}?  ({reason})",
                    cfg.total, cfg.concurrency
                );
                if let Some(runner) = self.load_runner_mut() {
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
        let was_running = self.load_runner().is_some_and(LoadRunnerState::is_running);
        if was_running {
            // Record the partial (whatever completed so far), marked cancelled.
            self.write_load_summary(true);
        }
        if let Some(handle) = self.load_abort.take() {
            handle.abort();
        }
        if let Some(runner) = self.load_runner_mut() {
            runner.run_generation += 1;
        }
    }

    /// Closes the runner. If a batch is still running (close was confirmed via the
    /// runner's `q`→`y` guard), records its partial summary + aborts + bumps the
    /// generation through the shared interrupt seam before dropping the runner —
    /// so a run interrupted by closing is never lost from `load_batches`.
    fn close_load_runner(&mut self) {
        self.interrupt_running_batch();
        self.load_request = None;
        // Setting `Normal` drops the `LoadRunnerState` (no separate field
        // to clear). `interrupt_running_batch` above already ran while the runner
        // was still live, so the partial summary / abort are recorded first.
        self.mode = Mode::Normal;
    }

    fn close_overlay(&mut self) {
        // Dropping the `Picker` drops ALL of its variant data — the item
        // `Vec`s AND the one-shot intents (`after_pick`/`runs`). So an Esc-cancelled
        // pick can't leak an intent into the next `/` search or sequence pick.
        self.picker = None;
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
        self.picker = Some(Picker::Workspace {
            state: picker::PickerState::new(" Switch workspace ", items),
            paths: choices,
        });
        self.mode = Mode::WorkspacePicker;
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(picker) = self.picker.as_mut().map(Picker::state_mut) else {
            self.close_overlay();
            return Ok(());
        };
        // Navigation + query editing live in the shared `PickerState::handle_key`
        // (one home for the Ctrl-j/k vim aliases &c.); only the accept/cancel
        // signals come back here for the app-level follow-ups.
        match picker.handle_key(key, &mut self.finder) {
            picker::PickerKey::Accept => return self.accept_overlay(),
            picker::PickerKey::Cancel => self.close_overlay(),
            picker::PickerKey::Consumed => {}
        }
        Ok(())
    }

    fn accept_overlay(&mut self) -> Result<()> {
        // Take the whole `Picker` out (its items + one-shot intents travel
        // WITH the finder in the variant), then reset the mode. `close_overlay()`
        // would drop the picker before we can read it, so `take` it first, then
        // clear the mode ourselves. An empty-result Enter (no `current` index) still
        // resets the mode + drops the picker, dropping the intents.
        let Some(picker) = self.picker.take() else {
            self.mode = Mode::Normal;
            return Ok(());
        };
        let current = picker.state().current();
        self.mode = Mode::Normal;
        let Some(index) = current else {
            return Ok(());
        };
        // The selected index can ONLY address the variant's own list — a stale-index
        // or wrong-`Vec` pairing (the old H3 hazard) is unrepresentable.
        match picker {
            Picker::Search {
                targets,
                after_pick,
                ..
            } => {
                if let Some(&(collection, endpoint)) = targets.get(index) {
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
            Picker::Sequence { paths, runs, .. } => {
                if let Some(path) = paths.get(index).cloned() {
                    // `runs` = `<leader>s r` run-vs-edit intent.
                    if runs {
                        self.run_sequence_at(path);
                    } else {
                        self.open_picked_sequence(path)?;
                    }
                }
            }
            Picker::Profile { profiles, .. } => {
                if let Some(choice) = profiles.get(index).cloned() {
                    self.set_profile(choice);
                }
            }
            Picker::Palette { actions, .. } => {
                if let Some(&action) = actions.get(index) {
                    self.dispatch(action, None)?;
                }
            }
            Picker::Workspace { paths, .. } => {
                // Through the dirty guard: a workspace target is always "other",
                // so unsaved edits defer to the discard confirm.
                if let Some(path) = paths.get(index).cloned() {
                    self.guarded_load(PendingLoad::Workspace(path))?;
                }
            }
            Picker::Auth { .. } => self.set_auth_kind(index),
            Picker::Destination {
                dirs,
                purpose,
                source,
                ..
            } => {
                if let Some(dest) = dirs.get(index).cloned() {
                    match purpose {
                        DestPurpose::CreateEndpoint => {
                            self.pending_create_dir = Some(dest);
                            self.open_prompt(PromptPurpose::NewEndpoint, "");
                        }
                        DestPurpose::CreateCollection => {
                            self.pending_create_dir = Some(dest);
                            self.open_prompt(PromptPurpose::NewCollection, "");
                        }
                        DestPurpose::MoveEndpoint => {
                            if let Some(src) = source {
                                self.relocate_endpoint(src, dest, true)?;
                            }
                        }
                        DestPurpose::CopyEndpoint => {
                            if let Some(src) = source {
                                self.relocate_endpoint(src, dest, false)?;
                            }
                        }
                        DestPurpose::MoveCollection => {
                            if let Some(src) = source {
                                self.relocate_collection(src, dest, true)?;
                            }
                        }
                        DestPurpose::CopyCollection => {
                            if let Some(src) = source {
                                self.relocate_collection(src, dest, false)?;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Sets (or clears with `None`) the active profile. No status message —
    /// the persistent `profile: <name>` indicator in the statusline is the
    /// single source of truth.
    fn set_profile(&mut self, profile: Option<String>) {
        self.active_profile = profile;
    }

    /// A hidden explorer drops out of the Tab ring — cycling skips it and
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
        // `<leader>e` can restore true prior focus on hide.
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
    /// state; only passive emptying reconciles here. (Sub-pane always present;
    /// empty pane focusable.)
    fn enforce_left_active_invariant(&mut self) {
        if self.explorer.sequences_len() == 0 {
            self.left_active = LeftPane::Endpoints;
        }
    }

    /// `<leader>e`: toggles the explorer sidebar. Hiding it while it is focused
    /// restores focus to the pane held before we entered the left column (true
    /// prior-pane restore, URL bar only as a last resort). Showing it
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

    /// `cycle-region-fwd`/`cycle-region-back` (shipped UNBOUND).
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
}

// Child modules of `app`. Placing them as children (not siblings) keeps their
// items' access to `App`'s private fields and methods without any `pub(crate)`
// widening — see DECISIONS.md, "Module boundaries".
mod handlers;
mod pure;
mod render;
mod state;

// The state data-types live in `state`. They are pulled into this
// module's namespace so in-crate call sites — and `super::*` in the sibling
// handler modules / `tests` — resolve them unqualified. The public types are
// re-exported `pub` so the public surface (`tui::app::Pane`, `Mode`,
// `ConfirmPurpose`, …, used by the snapshot integration test) is preserved; the
// module-private types keep their crate-internal visibility via a plain `use`.
use state::{
    APP_CHANNEL_CAPACITY, Buffer, DestPurpose, EndpointBuffer, InFlightRequest, PendingClose,
    PendingCopy, PendingCurlImport, PendingLoad, Picker, ResponseSurface, fold_body_text,
    overwrite_body_text,
};
pub use state::{
    AppMsg, ConfirmPurpose, LeaderState, LeftPane, Mode, Pane, PromptPurpose, SeqView, ZoomPane,
};

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
