//! Semantic key handling: [`Action`], the crokey-backed [`KeyMap`] with config
//! overrides, and the nucleo-backed [`FuzzyFinder`].
//!
//! Users remap *actions*, not raw keys (gitui/helix model): the config carries
//! `"key-combination" = "action-name"` string pairs; this module parses both sides
//! and fails loudly on unknown entries.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use color_eyre::eyre::{Result, eyre};
use crokey::{KeyCombination, key};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// A semantic, remappable UI action. Keys map to actions; actions drive the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Quit churl.
    Quit,
    /// Focus the next pane (Explorer → Request → Response → Explorer).
    FocusNext,
    /// Focus the previous pane.
    FocusPrev,
    /// Focus the explorer pane.
    FocusExplorer,
    /// Focus the request pane.
    FocusRequest,
    /// Focus the response pane.
    FocusResponse,
    /// Move the selection up.
    Up,
    /// Move the selection down.
    Down,
    /// Activate the current row: expand/collapse a container, select an endpoint.
    Select,
    /// Collapse the current container, or jump to the parent.
    Collapse,
    /// Expand the current container, or descend into it.
    Expand,
    /// Jump to the first row.
    Top,
    /// Jump to the last row.
    Bottom,
    /// Scroll the response body down half a page.
    HalfPageDown,
    /// Scroll the response body up half a page.
    HalfPageUp,
    /// Execute the selected endpoint's request.
    Send,
    /// Cancel the in-flight request.
    Cancel,
    /// Open the fuzzy endpoint search overlay.
    OpenSearch,
    /// Open the command palette overlay.
    OpenPalette,
    /// Enter jump-mode: label-driven pane/row navigation.
    Jump,
    /// Switch the active variable profile (palette-only; opens the profile picker).
    SwitchProfile,
    /// Open the environments & variables editor (workspace/collection/profile vars).
    OpenEnvEditor,
    /// Run the selected request sequence (opens the sequence surface, Run face).
    RunSequence,
    /// Edit the selected sequence, or create a new one (opens the sequence surface).
    EditSequence,
    /// Open a picker over all sequence names and open the chosen one (Edit face).
    OpenSequencePicker,
    /// Open a picker over all sequence names and RUN the chosen one (D1 —
    /// `<leader>s r`, so the user can choose which sequence to run).
    RunSequencePick,
    /// Open the concurrent-load runner for the selected endpoint (M7.5).
    OpenLoadRunner,
    /// Pick an endpoint, then open the concurrent-load runner over it.
    OpenLoadRunnerPick,
    /// Focus the URL bar.
    FocusUrlBar,
    /// Edit the focused URL bar's URL inline.
    EditUrl,
    /// Cycle the request method to the next (GET→POST→…→GET).
    MethodCycle,
    /// Open the one-key method-picker menu.
    MethodMenu,
    /// Save the current request to disk (format-preserving).
    Save,
    /// Select the next request-pane tab (Params→Headers→Auth→Body→Params).
    TabNext,
    /// Select the previous request-pane tab.
    TabPrev,
    /// Jump directly to the first request-pane tab (Params).
    Tab1,
    /// Jump directly to the second request-pane tab (Headers).
    Tab2,
    /// Jump directly to the third request-pane tab (Auth).
    Tab3,
    /// Jump directly to the fourth request-pane tab (Body).
    Tab4,
    /// Add a row on the active row-list tab (Params/Headers), entering edit.
    RowAdd,
    /// Delete the selected row on the active row-list tab.
    RowDelete,
    /// Toggle the selected row's `enabled` flag.
    RowToggle,
    /// Edit the selected row (or open the auth-kind picker on the Auth kind row).
    RowEdit,
    /// Create a new endpoint under the selected collection.
    NewEndpoint,
    /// Create a new collection.
    NewCollection,
    /// Rename the selected endpoint or collection.
    Rename,
    /// Delete the selected endpoint or collection (with a confirm).
    Delete,
    /// Toggle the explorer sidebar (hide / reopen).
    ToggleExplorer,
    /// Switch focus/zoom between the endpoints tree and the sequences sub-pane
    /// inside the (focused) explorer column (PR 2b).
    FocusSequencesToggle,
    /// Cycle forward within the focused region (M7.10 stage B, shipped UNBOUND):
    /// left column ⇒ Endpoints⇄Sequences; right column ⇒ next buffer/tab.
    CycleRegionFwd,
    /// Cycle backward within the focused region (M7.10 stage B, shipped UNBOUND):
    /// left column ⇒ Endpoints⇄Sequences; right column ⇒ previous buffer/tab.
    CycleRegionBack,
    /// Zoom the focused pane (Request/Response), collapsing the other.
    Zoom,
    /// Open the `?` help overlay (effective keymap).
    Help,
    /// Enter pending-leader state (the which-key popup).
    Leader,
    /// Open the URL vim-popup editor (`e` on the URL bar).
    EditUrlPopup,
    /// Toggle the response pane between body and headers view.
    ToggleHeadersView,
    /// Toggle soft-wrap in the response viewer.
    ToggleWrap,
    /// Toggle raw↔pretty (reformatted) rendering of the response body.
    TogglePretty,
    /// Toggle A→Z alphabetical sorting of pretty JSON object keys (M7.7).
    ToggleSortKeys,
    /// Open incremental literal search over the response body.
    OpenBodySearch,
    /// Jump to the next response-search match.
    SearchNext,
    /// Jump to the previous response-search match.
    SearchPrev,
    /// Fold/unfold the JSON region at the response cursor.
    ToggleFold,
    /// Collapse all top-level JSON regions, or expand all.
    ToggleAllFolds,
    /// Copy the current response view's full text to the clipboard.
    CopyResponse,
    /// Copy the response cursor's logical line to the clipboard.
    CopyLine,
    /// Open the quick-jump request picker (fuzzy over all endpoints). Reuses the
    /// endpoint-search overlay; bound to `<leader>f`.
    QuickJumpRequests,
    /// Open the quick-jump workspace picker (recently-opened workspaces). Bound
    /// to `<leader>w`.
    QuickJumpWorkspaces,
    /// Import a JSON collection (path prompt) into the workspace.
    ImportCollection,
    /// Export the selected collection as Postman v2.1 JSON.
    ExportCollectionPostman,
    /// Export the selected collection as churl-native JSON.
    ExportCollectionNative,
    /// Export the whole workspace as Postman v2.1 JSON.
    ExportWorkspacePostman,
    /// Export the whole workspace as churl-native JSON.
    ExportWorkspaceNative,
    /// Paste a curl command as a new endpoint (path/curl prompt).
    PasteCurl,
    /// Copy the selected request as a curl one-liner (`{{var}}` verbatim).
    CopyAsCurl,
    /// Copy the selected request as a curl one-liner with `{{var}}`s resolved.
    CopyAsCurlResolved,
    /// Focus the next open buffer/tab (wraps). Distinct from the request-pane
    /// `TabNext` (which cycles Params/Headers/Auth/Body).
    BufferNext,
    /// Focus the previous open buffer/tab (wraps).
    BufferPrev,
    /// Close the active buffer/tab (dirty prompts to discard).
    BufferClose,
    /// Close all open buffers/tabs (each dirty one prompts in turn).
    BufferCloseAll,
}

/// `(action, config name, palette label)` for every action, in palette order.
const ACTION_TABLE: &[(Action, &str, &str)] = &[
    (Action::Quit, "quit", "quit"),
    (Action::FocusNext, "focus-next", "focus next pane"),
    (Action::FocusPrev, "focus-prev", "focus previous pane"),
    (Action::FocusExplorer, "focus-explorer", "focus explorer"),
    (Action::FocusRequest, "focus-request", "focus request"),
    (Action::FocusResponse, "focus-response", "focus response"),
    // Movement in vim h/j/k/l order (the help overlay renders this table's
    // order verbatim), then g/G and paging together.
    (Action::Collapse, "collapse", "collapse / parent"),
    (Action::Down, "down", "move down"),
    (Action::Up, "up", "move up"),
    (Action::Expand, "expand", "expand / descend"),
    (Action::Select, "select", "select / toggle"),
    (Action::Top, "top", "jump to top"),
    (Action::Bottom, "bottom", "jump to bottom"),
    (
        Action::HalfPageDown,
        "half-page-down",
        "scroll half page down",
    ),
    (Action::HalfPageUp, "half-page-up", "scroll half page up"),
    (Action::Send, "send", "send request"),
    (Action::Cancel, "cancel", "cancel request"),
    (Action::OpenSearch, "open-search", "search endpoints"),
    (Action::OpenPalette, "open-palette", "command palette"),
    (Action::Jump, "jump", "jump to pane / row"),
    (Action::SwitchProfile, "switch-profile", "switch profile"),
    (Action::OpenEnvEditor, "env-editor", "Environments & vars"),
    (Action::RunSequence, "run-sequence", "run sequence"),
    (Action::EditSequence, "edit-sequence", "add sequence"),
    (
        Action::OpenSequencePicker,
        "open-sequence-picker",
        "open sequence",
    ),
    (
        Action::RunSequencePick,
        "run-sequence-pick",
        "run sequence (pick)",
    ),
    (Action::OpenLoadRunner, "load-runner", "load test endpoint"),
    (
        Action::OpenLoadRunnerPick,
        "load-runner-pick",
        "load test (pick endpoint)",
    ),
    (Action::FocusUrlBar, "focus-urlbar", "focus URL bar"),
    (Action::EditUrl, "edit-url", "edit URL"),
    (Action::MethodCycle, "method-cycle", "cycle method"),
    (Action::MethodMenu, "method-menu", "method menu"),
    (Action::Save, "save", "save request"),
    (Action::TabNext, "tab-next", "next tab"),
    (Action::TabPrev, "tab-prev", "previous tab"),
    (Action::Tab1, "tab-1", "tab: params"),
    (Action::Tab2, "tab-2", "tab: headers"),
    (Action::Tab3, "tab-3", "tab: auth"),
    (Action::Tab4, "tab-4", "tab: body"),
    (Action::RowAdd, "row-add", "add row"),
    (Action::RowDelete, "row-delete", "delete row"),
    (Action::RowToggle, "row-toggle", "toggle row enabled"),
    (Action::RowEdit, "row-edit", "edit row"),
    (Action::NewEndpoint, "new-endpoint", "new endpoint"),
    (Action::NewCollection, "new-collection", "new collection"),
    (Action::Rename, "rename", "rename"),
    (Action::Delete, "delete", "delete"),
    (
        Action::ToggleExplorer,
        "toggle-explorer",
        "toggle explorer sidebar",
    ),
    (
        Action::FocusSequencesToggle,
        "focus-sequences-toggle",
        "switch endpoints / sequences",
    ),
    (
        Action::CycleRegionFwd,
        "cycle-region-fwd",
        "cycle region forward (sub-pane / buffer)",
    ),
    (
        Action::CycleRegionBack,
        "cycle-region-back",
        "cycle region back (sub-pane / buffer)",
    ),
    (Action::Zoom, "zoom", "zoom pane"),
    (Action::Help, "help", "help overlay"),
    (Action::Leader, "leader", "leader menu"),
    (Action::EditUrlPopup, "edit-url-popup", "edit URL (popup)"),
    (
        Action::ToggleHeadersView,
        "toggle-headers-view",
        "toggle response headers view",
    ),
    (Action::ToggleWrap, "toggle-wrap", "toggle response wrap"),
    (
        Action::TogglePretty,
        "toggle-pretty",
        "toggle response pretty/raw",
    ),
    (
        Action::ToggleSortKeys,
        "toggle-sort-keys",
        "sort response keys A→Z",
    ),
    (
        Action::OpenBodySearch,
        "open-body-search",
        "search response body",
    ),
    (Action::SearchNext, "search-next", "next match"),
    (Action::SearchPrev, "search-prev", "previous match"),
    (Action::ToggleFold, "toggle-fold", "toggle fold at cursor"),
    (
        Action::ToggleAllFolds,
        "toggle-all-folds",
        "toggle all folds",
    ),
    (Action::CopyResponse, "copy-response", "copy response"),
    (Action::CopyLine, "copy-line", "copy line"),
    (
        Action::QuickJumpRequests,
        "quick-jump-requests",
        "request picker",
    ),
    (
        Action::QuickJumpWorkspaces,
        "quick-jump-workspaces",
        "workspace picker",
    ),
    (
        Action::ImportCollection,
        "import-collection",
        "import collection (JSON)",
    ),
    (
        Action::ExportCollectionPostman,
        "export-collection-postman",
        "export collection · Postman v2.1",
    ),
    (
        Action::ExportCollectionNative,
        "export-collection-native",
        "export collection · churl JSON",
    ),
    (
        Action::ExportWorkspacePostman,
        "export-workspace-postman",
        "export workspace · Postman v2.1",
    ),
    (
        Action::ExportWorkspaceNative,
        "export-workspace-native",
        "export workspace · churl JSON",
    ),
    (
        Action::PasteCurl,
        "paste-curl",
        "paste curl as new endpoint",
    ),
    (Action::CopyAsCurl, "copy-as-curl", "copy request as curl"),
    (
        Action::CopyAsCurlResolved,
        "copy-as-curl-resolved",
        "copy request as curl (resolved vars)",
    ),
    (Action::BufferNext, "buffer-next", "next buffer"),
    (Action::BufferPrev, "buffer-prev", "previous buffer"),
    (Action::BufferClose, "buffer-close", "close buffer"),
    (
        Action::BufferCloseAll,
        "buffer-close-all",
        "close all buffers",
    ),
];

impl Action {
    /// All actions, in the order they appear in the command palette.
    pub fn all() -> impl Iterator<Item = Action> {
        ACTION_TABLE.iter().map(|(action, _, _)| *action)
    }

    /// The stable config-facing name of this action (e.g. `"open-palette"`).
    pub fn name(self) -> &'static str {
        ACTION_TABLE
            .iter()
            .find(|(action, _, _)| *action == self)
            .map(|(_, name, _)| *name)
            .expect("every action is in ACTION_TABLE")
    }

    /// The human-readable palette label of this action (e.g. `"command palette"`).
    pub fn label(self) -> &'static str {
        ACTION_TABLE
            .iter()
            .find(|(action, _, _)| *action == self)
            .map(|(_, _, label)| *label)
            .expect("every action is in ACTION_TABLE")
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A nested-leader submenu: `<leader><prefix>` descends into one of these, and a
/// second key selects an action within it. Submenus are fully data-driven — the
/// built-in three (sequences/load/tabs) are seeded as default data in
/// [`KeyMap::default`], and a config `[keys.leader.<name>]` table creates or
/// extends any submenu by name (M7.10). No closed enum of submenu kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submenu {
    /// The which-key label shown next to the submenu prefix (e.g. `"sequences"`).
    pub title: String,
    /// The second-key continuations inside this submenu.
    pub binds: HashMap<KeyCombination, Action>,
}

impl Submenu {
    /// A new, empty submenu with the given which-key title.
    fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            binds: HashMap::new(),
        }
    }
}

/// A root-level leader binding: either a direct action or a descent into a named
/// submenu. Populated from the built-in root map + the `[keys.leader]` table. The
/// submenu is carried by name (not an enum variant) so config can wire arbitrary
/// submenus (M7.10 dynamic leader submenus).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderEntry {
    /// A direct action dispatched from the root which-key popup.
    Act(Action),
    /// A descent into a nested submenu, keyed by its name (the
    /// [`KeyMap::submenus`] map key).
    Submenu(String),
}

/// Error parsing an [`Action`] from its config name.
#[derive(Debug)]
pub struct UnknownAction(String);

impl std::fmt::Display for UnknownAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown action: {:?}", self.0)
    }
}

impl std::error::Error for UnknownAction {}

impl FromStr for Action {
    type Err = UnknownAction;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ACTION_TABLE
            .iter()
            .find(|(_, name, _)| *name == s)
            .map(|(action, _, _)| *action)
            .ok_or_else(|| UnknownAction(s.to_owned()))
    }
}

/// Which pane's local keymap overlay to consult. A focused pane's overlay is
/// searched before the global map (see [`KeyMap::lookup_ctx`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PaneCtx {
    /// The explorer pane (CRUD keys: `n`/`N`/`r`/`d`).
    Explorer,
    /// The focusable URL bar (`i`/`m`/`M`).
    UrlBar,
    /// The request pane (tab switching + row editing).
    Request,
    /// The response pane (M7 viewer keys: headers/wrap/search/fold/copy).
    Response,
}

impl PaneCtx {
    /// The config-table suffix for this context (`[keys.<suffix>]`).
    fn config_name(self) -> &'static str {
        match self {
            PaneCtx::Explorer => "explorer",
            PaneCtx::UrlBar => "urlbar",
            PaneCtx::Request => "request",
            PaneCtx::Response => "response",
        }
    }

    /// Every context, in a stable order (for `churl keymaps` grouping).
    pub fn all() -> [PaneCtx; 4] {
        [
            PaneCtx::Explorer,
            PaneCtx::UrlBar,
            PaneCtx::Request,
            PaneCtx::Response,
        ]
    }

    /// The display header for `churl keymaps` (`explorer` → `Explorer`).
    pub fn header(self) -> &'static str {
        match self {
            PaneCtx::Explorer => "Explorer",
            PaneCtx::UrlBar => "URL bar",
            PaneCtx::Request => "Request",
            PaneCtx::Response => "Response",
        }
    }
}

/// Key-combination → [`Action`] map. Defaults are built with crokey's `key!`
/// macro; config overrides layer on top via [`KeyMap::with_overrides`]. A
/// per-pane `overlays` layer holds pane-local bindings consulted before the
/// global `map` (see [`KeyMap::lookup_ctx`]) — the vehicle for keys like `1`–`4`
/// that must mean one thing inside the Request pane and another globally.
#[derive(Debug)]
pub struct KeyMap {
    map: HashMap<KeyCombination, Action>,
    overlays: BTreeMap<PaneCtx, HashMap<KeyCombination, Action>>,
    /// The leader key: pressing it (outside text edits) enters pending-leader
    /// state and shows the which-key popup. Remappable via `leader_key` config.
    leader: KeyCombination,
    /// Root leader continuations: the second key of a `<leader>x` chord → a
    /// direct action or a descent into a named submenu. Populated from the
    /// built-in root map + the `[keys.leader]` sub-table.
    leader_root: HashMap<KeyCombination, LeaderEntry>,
    /// Named leader submenus (M7.10 dynamic submenus): submenu name → its
    /// `(title, binds)`. The built-in `sequences`/`load`/`tabs` are seeded in
    /// [`Self::default`]; a config `[keys.leader.<name>]` table creates or
    /// extends any submenu.
    submenus: HashMap<String, Submenu>,
}

impl Default for KeyMap {
    fn default() -> Self {
        let mut map = HashMap::new();
        let mut bind = |combo: KeyCombination, action: Action| {
            map.insert(combo.normalized(), action);
        };
        bind(key!(q), Action::Quit);
        bind(key!(ctrl - c), Action::Quit);
        bind(key!(tab), Action::FocusNext);
        // crossterm always reports BackTab with SHIFT set; crokey's `key!(backtab)`
        // does not add it, so build the combination explicitly.
        bind(
            KeyCombination::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            Action::FocusPrev,
        );
        // Global `1`/`2`/`3` pane-focus binds were removed in M6.7 (DECISIONS.md):
        // navigation is Tab/Shift-Tab + `f` jump-mode only; `1`–`4` are Request-tab
        // jumps only. Pane focus stays fully reachable without digit keys.
        bind(key!(k), Action::Up);
        bind(key!(j), Action::Down);
        bind(key!(enter), Action::Select);
        bind(key!(h), Action::Collapse);
        bind(key!(l), Action::Expand);
        bind(key!(g), Action::Top);
        bind(key!(shift - g), Action::Bottom);
        bind(key!(ctrl - d), Action::HalfPageDown);
        bind(key!(ctrl - u), Action::HalfPageUp);
        bind(key!(ctrl - s), Action::Send);
        // `Cancel` has no default binding: Ctrl-C (bound to `Quit`) cancels an
        // in-flight request, and the command palette exposes `cancel request`.
        bind(key!('/'), Action::OpenSearch);
        bind(key!(':'), Action::OpenPalette);
        bind(key!(f), Action::Jump);
        // `Save` is global: `w` writes the current request (never auto-saved).
        bind(key!(w), Action::Save);
        // `?` opens the help overlay; `z` zooms the focused pane.
        bind(key!('?'), Action::Help);
        bind(key!(z), Action::Zoom);
        // `SwitchProfile` has no default key binding: it lives in the command
        // palette ("switch profile"). Overlay-level modes (search/palette/jump)
        // still take routing precedence over it.

        let mut overlays: BTreeMap<PaneCtx, HashMap<KeyCombination, Action>> = BTreeMap::new();
        let mut overlay = |ctx: PaneCtx, combo: KeyCombination, action: Action| {
            overlays
                .entry(ctx)
                .or_default()
                .insert(combo.normalized(), action);
        };
        // Explorer CRUD.
        overlay(PaneCtx::Explorer, key!(n), Action::NewEndpoint);
        overlay(PaneCtx::Explorer, key!(shift - n), Action::NewCollection);
        overlay(PaneCtx::Explorer, key!(r), Action::Rename);
        overlay(PaneCtx::Explorer, key!(d), Action::Delete);
        // `s` switches focus/zoom between the endpoints tree and the sequences
        // sub-pane (PR 2b) — a lawful in-pane move, only live when the left
        // column is focused. `s` is otherwise free in the Explorer overlay.
        overlay(PaneCtx::Explorer, key!(s), Action::FocusSequencesToggle);
        // Arrow keys navigate the explorer, mirroring the global `k`/`j`/`h`/`l`
        // (owner drive-test 2026-07-10). Scoped to the Explorer overlay — NOT
        // global — so Left/Right→Collapse/Expand never leak into other panes.
        // The flat sequences sub-pane no-ops Collapse/Expand, so Left/Right are
        // harmless there.
        overlay(PaneCtx::Explorer, key!(up), Action::Up);
        overlay(PaneCtx::Explorer, key!(down), Action::Down);
        overlay(PaneCtx::Explorer, key!(left), Action::Collapse);
        overlay(PaneCtx::Explorer, key!(right), Action::Expand);
        // URL bar: edit + method switch.
        overlay(PaneCtx::UrlBar, key!(i), Action::EditUrl);
        overlay(PaneCtx::UrlBar, key!(enter), Action::EditUrl);
        overlay(PaneCtx::UrlBar, key!(m), Action::MethodCycle);
        overlay(PaneCtx::UrlBar, key!(shift - m), Action::MethodMenu);
        // Request pane: tab switching + row editing. `1`–`4` shadow the global
        // pane-focus digits here (recorded in DECISIONS.md); pane focus stays
        // reachable via Tab/Shift-Tab and jump-mode.
        overlay(PaneCtx::Request, key!(']'), Action::TabNext);
        overlay(PaneCtx::Request, key!('['), Action::TabPrev);
        overlay(PaneCtx::Request, key!('1'), Action::Tab1);
        overlay(PaneCtx::Request, key!('2'), Action::Tab2);
        overlay(PaneCtx::Request, key!('3'), Action::Tab3);
        overlay(PaneCtx::Request, key!('4'), Action::Tab4);
        overlay(PaneCtx::Request, key!(a), Action::RowAdd);
        overlay(PaneCtx::Request, key!(d), Action::RowDelete);
        // Space is the global leader from M6.7; the Request row-toggle rebinds to
        // `t` so Space stays free everywhere (DECISIONS.md).
        overlay(PaneCtx::Request, key!(t), Action::RowToggle);
        overlay(PaneCtx::Request, key!(i), Action::RowEdit);
        // Copy-as-curl is `<leader>y` (see the leader binds below): leader keys are
        // inert during text edits, so it never shadows body-editor input. The
        // resolved-vars variant and the interchange import/export actions stay
        // palette-only (rare; a path prompt is the natural entry point).
        // URL bar: the vim-popup editor (`e`), independent of the inline `i`/Enter.
        overlay(PaneCtx::UrlBar, key!(e), Action::EditUrlPopup);
        // Response pane (M7): body/headers, wrap, search, match nav, folding,
        // copy. `h` shadows the global Collapse and `/` shadows the global
        // OpenSearch here (same precedent as Request-pane `1`–`4`; DECISIONS.md).
        overlay(PaneCtx::Response, key!(h), Action::ToggleHeadersView);
        overlay(PaneCtx::Response, key!(shift - w), Action::ToggleWrap);
        // `p` (pretty) toggles raw↔reformatted body rendering (M7.7). `p` is free
        // in the Response overlay (the global `<leader>p` switch-profile lives
        // behind the leader, not in this pane overlay).
        overlay(PaneCtx::Response, key!(p), Action::TogglePretty);
        // `s` (sort) toggles A→Z key sorting on the pretty JSON body (M7.7). Free
        // in the Response overlay; the leader `s` (sequences submenu) lives behind
        // the leader, not this pane overlay.
        overlay(PaneCtx::Response, key!(s), Action::ToggleSortKeys);
        overlay(PaneCtx::Response, key!('/'), Action::OpenBodySearch);
        overlay(PaneCtx::Response, key!(n), Action::SearchNext);
        overlay(PaneCtx::Response, key!(shift - n), Action::SearchPrev);
        overlay(PaneCtx::Response, key!(o), Action::ToggleFold);
        overlay(PaneCtx::Response, key!(shift - o), Action::ToggleAllFolds);
        overlay(PaneCtx::Response, key!(y), Action::CopyResponse);
        overlay(PaneCtx::Response, key!(shift - y), Action::CopyLine);

        // The leader chord: Space, then a continuation key. Root binds are either
        // direct actions or descents into a nested submenu (two-level which-key).
        let mut leader_root = HashMap::new();
        let mut root_bind = |combo: KeyCombination, entry: LeaderEntry| {
            leader_root.insert(combo.normalized(), entry);
        };
        // Direct root actions. `s` (formerly Send) now descends into the
        // sequences submenu — Send stays Ctrl-S ONLY (bound in `map` above).
        root_bind(key!(e), LeaderEntry::Act(Action::ToggleExplorer));
        // `<leader>S` was removed in M7.10 stage B: the sequences sub-pane is
        // always peek-visible, so a show/hide toggle had no job. Reaching the
        // sequences sub-pane stays covered by the Explorer `s` overlay, `f`-jump
        // (`s` label) and the `<leader>s f` picker.
        root_bind(key!(c), LeaderEntry::Act(Action::Cancel));
        root_bind(key!(p), LeaderEntry::Act(Action::SwitchProfile));
        // `<leader>v` opens the environments & variables editor (`v` is free).
        root_bind(key!(v), LeaderEntry::Act(Action::OpenEnvEditor));
        root_bind(key!(q), LeaderEntry::Act(Action::Quit));
        // `<leader><leader>` (Space as its own continuation) opens the
        // endpoint/request picker — owner drive-test 2026-07-10 moved it off `f`,
        // freeing `f` at root for jump-mode. A leader continuation of Space is NOT
        // flagged by `validate` (that only checks the leader against the GLOBAL map
        // and pane overlays), so the default config stays warning-clean.
        // `<leader>w` opens the recent-workspace picker.
        root_bind(key!(space), LeaderEntry::Act(Action::QuickJumpRequests));
        root_bind(key!(w), LeaderEntry::Act(Action::QuickJumpWorkspaces));
        // Copy the loaded request as a curl one-liner (`y` was free). Moved off
        // the Request-overlay `C` so it can't shadow body-editor text input.
        root_bind(key!(y), LeaderEntry::Act(Action::CopyAsCurl));
        // Submenu descents (two-level which-key). The submenu names match the
        // seeded `submenus` keys below; config can point new keys at them or
        // create fresh submenus.
        root_bind(key!(s), LeaderEntry::Submenu("sequences".to_owned()));
        root_bind(key!(l), LeaderEntry::Submenu("load".to_owned()));
        // `<leader>t` descends into the tabs/buffers submenu. `t` is free at root.
        root_bind(key!(t), LeaderEntry::Submenu("tabs".to_owned()));

        // The built-in submenus, seeded as default data (M7.10). Behaviour is
        // byte-identical to the former hardcoded `sub_*` maps.
        let mut submenus: HashMap<String, Submenu> = HashMap::new();

        // `<leader>s …`: sequence actions (add / open / run).
        let mut sequences = Submenu::new("sequences");
        sequences
            .binds
            .insert(key!(a).normalized(), Action::EditSequence);
        // `<leader>s <leader>` (Space) is the single "find/open a sequence"
        // picker, mirroring `<leader><leader>` for endpoints (owner drive-test
        // 2026-07-10) — one key for one job; the former `o`/`f` binds are gone. A
        // leader continuation of Space stays warning-clean in `validate` (see the
        // root Space bind above).
        sequences
            .binds
            .insert(key!(space).normalized(), Action::OpenSequencePicker);
        // D1: `<leader>s r` routes to a run-flavored chooser (pick which sequence
        // to run) instead of silently running `sequences[seq_cursor]`. The direct
        // `RunSequence` action stays reachable via the in-pane `r` + palette.
        sequences
            .binds
            .insert(key!(r).normalized(), Action::RunSequencePick);
        submenus.insert("sequences".to_owned(), sequences);

        // `<leader>l …`: load-test actions. `s` (load-a-sequence) is reserved for
        // a later composable-runs wave — do NOT bind it here.
        let mut load = Submenu::new("load");
        load.binds
            .insert(key!(c).normalized(), Action::OpenLoadRunner);
        // `<leader>l <leader>` (Space) picks an endpoint first, mirroring the
        // endpoint/sequence pickers (owner drive-test 2026-07-10).
        load.binds
            .insert(key!(space).normalized(), Action::OpenLoadRunnerPick);
        submenus.insert("load".to_owned(), load);

        // `<leader>t …`: buffer/tab actions. `n` next · `p` prev · `x` close ·
        // `X` (shift-x) close all. Do NOT touch `Tab`/`Shift-Tab` (cross-pane).
        let mut tabs = Submenu::new("tabs");
        tabs.binds.insert(key!(n).normalized(), Action::BufferNext);
        tabs.binds.insert(key!(p).normalized(), Action::BufferPrev);
        tabs.binds.insert(key!(x).normalized(), Action::BufferClose);
        tabs.binds
            .insert(key!(shift - x).normalized(), Action::BufferCloseAll);
        submenus.insert("tabs".to_owned(), tabs);

        Self {
            map,
            overlays,
            leader: key!(space).normalized(),
            leader_root,
            submenus,
        }
    }
}

impl KeyMap {
    /// Builds the default map with global `[keys]` overrides layered on top.
    ///
    /// Unknown key combinations or action names are a hard error — a silently
    /// dropped binding is worse than a startup failure the user can fix.
    pub fn with_overrides(overrides: &BTreeMap<String, String>) -> Result<Self> {
        Self::with_all_overrides(overrides, &BTreeMap::new())
    }

    /// The leader key combination (Space by default; remappable via `leader_key`).
    pub fn leader(&self) -> KeyCombination {
        self.leader
    }

    /// Whether `key` is the leader key.
    pub fn is_leader(&self, key: KeyEvent) -> bool {
        KeyCombination::from(key) == self.leader
    }

    /// The submenu named `name`, if it exists.
    fn submenu(&self, name: &str) -> Option<&Submenu> {
        self.submenus.get(name)
    }

    /// Looks up a *root* leader continuation (a direct action or a descent).
    pub fn leader_root_lookup(&self, key: KeyEvent) -> Option<LeaderEntry> {
        self.leader_root.get(&KeyCombination::from(key)).cloned()
    }

    /// Looks up a leader continuation inside the submenu named `menu` (the third
    /// key of a `<leader><prefix><key>` chord).
    pub fn leader_sub_lookup(&self, menu: &str, key: KeyEvent) -> Option<Action> {
        self.submenu(menu)?
            .binds
            .get(&KeyCombination::from(key))
            .copied()
    }

    /// Every `(key combination, action)` binding reachable through the leader —
    /// root direct actions AND every submenu's binds, unordered. Submenu actions
    /// are keyed by their *second* key only; callers wanting the full chord
    /// string use [`Self::leader_combos_for`].
    pub fn iter_leader(&self) -> impl Iterator<Item = (KeyCombination, Action)> + '_ {
        let root = self
            .leader_root
            .iter()
            .filter_map(|(combo, entry)| match entry {
                LeaderEntry::Act(action) => Some((*combo, *action)),
                LeaderEntry::Submenu(_) => None,
            });
        let subs = self
            .submenus
            .values()
            .flat_map(|menu| menu.binds.iter().map(|(combo, action)| (*combo, *action)));
        root.chain(subs)
    }

    /// Every direct `(key combination, action)` root leader bind (no submenu
    /// descents), unordered — for the which-key root popup.
    pub fn iter_leader_root_acts(&self) -> impl Iterator<Item = (KeyCombination, Action)> + '_ {
        self.leader_root
            .iter()
            .filter_map(|(combo, entry)| match entry {
                LeaderEntry::Act(action) => Some((*combo, *action)),
                LeaderEntry::Submenu(_) => None,
            })
    }

    /// Every `(key combination, action)` bind inside the submenu named `menu`,
    /// unordered — for the which-key submenu popup. An unknown name yields an
    /// empty iterator.
    pub fn iter_submenu(&self, menu: &str) -> impl Iterator<Item = (KeyCombination, Action)> + '_ {
        self.submenu(menu)
            .into_iter()
            .flat_map(|menu| menu.binds.iter().map(|(combo, action)| (*combo, *action)))
    }

    /// The which-key title of the submenu named `menu`, or the name itself when
    /// the submenu has no explicit title (a config-created submenu with no seed).
    pub fn submenu_title(&self, menu: &str) -> String {
        self.submenu(menu)
            .map(|m| m.title.clone())
            .unwrap_or_else(|| menu.to_owned())
    }

    /// The `(prefix key, label)` of every reachable leader submenu, sorted by
    /// prefix. A descent whose submenu is missing (dangling) falls back to the
    /// name as its label so which-key still shows the row.
    pub fn leader_menu_combos(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .leader_root
            .iter()
            .filter_map(|(combo, entry)| match entry {
                LeaderEntry::Submenu(name) => Some((combo.to_string(), self.submenu_title(name))),
                LeaderEntry::Act(_) => None,
            })
            .collect();
        out.sort();
        out
    }

    /// All leader chords bound to `action`, sorted. Root actions render as the
    /// second key (e.g. `"c"`); submenu actions as the full chord (e.g. `"s r"`)
    /// so `?` help and `churl keymaps` show the complete path. A submenu's prefix
    /// is the root key that descends into it (looked up live, so config-renamed
    /// descents render correctly).
    pub fn leader_combos_for(&self, action: Action) -> Vec<String> {
        let mut combos: Vec<String> = self
            .leader_root
            .iter()
            .filter_map(|(combo, entry)| match entry {
                LeaderEntry::Act(bound) if *bound == action => Some(combo.to_string()),
                _ => None,
            })
            .collect();
        // Map each submenu name to the root prefix keys that descend into it.
        for (name, menu) in &self.submenus {
            let prefixes: Vec<String> = self
                .leader_root
                .iter()
                .filter_map(|(combo, entry)| match entry {
                    LeaderEntry::Submenu(n) if n == name => Some(combo.to_string()),
                    _ => None,
                })
                .collect();
            for (combo, bound) in &menu.binds {
                if *bound == action {
                    for prefix in &prefixes {
                        combos.push(format!("{prefix} {combo}"));
                    }
                }
            }
        }
        combos.sort();
        combos
    }

    /// Overrides the leader key, re-normalizing. Fails loudly on a bad combo.
    pub fn set_leader(&mut self, combo_str: &str) -> Result<()> {
        let combo = KeyCombination::from_str(combo_str)
            .map_err(|err| eyre!("bad leader_key {combo_str:?}: {err}"))?;
        self.leader = combo.normalized();
        Ok(())
    }

    /// Builds the default map with global `[keys]` overrides *and* per-pane
    /// overlay overrides (`[keys.explorer]`, `[keys.urlbar]`, `[keys.request]`,
    /// `[keys.response]`) layered on top. An unknown overlay-table name, key
    /// combination, or action name is a hard error (fail-loud parsing).
    pub fn with_all_overrides(
        global: &BTreeMap<String, String>,
        overlays: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Result<Self> {
        let mut keymap = Self::default();
        for (combo_str, action_str) in global {
            let (combo, action) = parse_binding(combo_str, action_str, "[keys]")?;
            keymap.map.insert(combo.normalized(), action);
        }
        for (table, bindings) in overlays {
            // `[keys.leader]` is not a pane overlay: it holds root leader
            // continuations (direct actions or `"+<submenu>"` descents).
            if table == "leader" {
                for (combo_str, entry_str) in bindings {
                    let combo = KeyCombination::from_str(combo_str).map_err(|err| {
                        eyre!("bad key combination {combo_str:?} in [keys.leader]: {err}")
                    })?;
                    let entry = parse_leader_entry(combo_str, entry_str)?;
                    keymap.leader_root.insert(combo.normalized(), entry);
                }
                continue;
            }
            // `[keys.leader.<name>]`: a submenu table whose values are action
            // names. Handled BEFORE the PaneCtx lookup so it is not rejected as an
            // unknown table. The submenu is created on first mention (M7.10
            // dynamic submenus), so `g = "+git"` + `[keys.leader.git]` wires a
            // brand-new working submenu; the built-in three are extended in place.
            if let Some(menu_name) = table.strip_prefix("leader.") {
                let label = format!("[keys.leader.{menu_name}]");
                let submenu = keymap
                    .submenus
                    .entry(menu_name.to_owned())
                    .or_insert_with(|| Submenu::new(menu_name));
                for (combo_str, action_str) in bindings {
                    let (combo, action) = parse_binding(combo_str, action_str, &label)?;
                    submenu.binds.insert(combo.normalized(), action);
                }
                continue;
            }
            let ctx = PaneCtx::all()
                .into_iter()
                .find(|c| c.config_name() == table)
                .ok_or_else(|| {
                    eyre!(
                        "unknown keymap table [keys.{table}] (expected one of: \
                         explorer, urlbar, request, response, leader, \
                         or a leader submenu [keys.leader.<name>])"
                    )
                })?;
            for (combo_str, action_str) in bindings {
                let label = format!("[keys.{table}]");
                let (combo, action) = parse_binding(combo_str, action_str, &label)?;
                keymap
                    .overlays
                    .entry(ctx)
                    .or_default()
                    .insert(combo.normalized(), action);
            }
        }
        Ok(keymap)
    }

    /// Load-time conflict/shadow validator (M7.10). Runs on the final keymap and
    /// returns human-readable warnings for **genuine defects only** — documented
    /// intentional single-pane overlay shadows (Response `h`/`/`, Request `1`–`4`,
    /// …) produce ZERO warnings. Surfaced loudly but non-blocking (stderr + a
    /// first-frame toast + a `churl keymaps` section).
    ///
    /// Takes the raw config tables (not just the collapsed keymap) so it can spot
    /// two combo *strings* that normalize to the same key within one scope — a
    /// silent last-wins the [`HashMap`] has already hidden by the time we hold a
    /// built [`KeyMap`].
    ///
    /// Defect classes:
    /// - (a) the leader key is also bound as a normal global/pane action (the dead
    ///   `space=` case — the normal bind can never fire);
    /// - (b) a `"+name"` descent whose `[keys.leader.name]` table does not exist
    ///   (a dangling menu — descending shows an empty popup);
    /// - (c) a `[keys.leader.name]` table that no descent points to (an orphan /
    ///   unreachable menu);
    /// - (d) two combo strings normalizing to the same key within one scope
    ///   (silent last-wins);
    /// - (e) a global bind shadowed in **every** pane overlay (globally
    ///   unreachable).
    pub fn validate(
        &self,
        global: &BTreeMap<String, String>,
        overlays: &BTreeMap<String, BTreeMap<String, String>>,
    ) -> Vec<String> {
        let mut warnings = Vec::new();

        // (a) The leader key is also a normal action bind — the normal bind is
        // dead because the leader intercepts the key first.
        if let Some(action) = self.map.get(&self.leader) {
            warnings.push(format!(
                "leader key `{}` is also bound as a global action ({}) — the action \
                 can never fire because the leader intercepts the key first",
                self.leader,
                action.name(),
            ));
        }
        for (ctx, overlay) in &self.overlays {
            if let Some(action) = overlay.get(&self.leader) {
                warnings.push(format!(
                    "leader key `{}` is also bound in the {} overlay ({}) — the pane \
                     bind can never fire because the leader intercepts the key first",
                    self.leader,
                    ctx.header(),
                    action.name(),
                ));
            }
        }

        // (b) Dangling descents: a `"+name"` root descent with no submenu behind
        // it. (c) Orphan menus: a submenu no descent reaches. Compute the set of
        // descended-into names once.
        let mut descended: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for entry in self.leader_root.values() {
            if let LeaderEntry::Submenu(name) = entry {
                descended.insert(name.as_str());
                if !self.submenus.contains_key(name) {
                    warnings.push(format!(
                        "leader descent `+{name}` has no `[keys.leader.{name}]` table \
                         — descending into it shows an empty menu"
                    ));
                }
            }
        }
        for name in self.submenus.keys() {
            if !descended.contains(name.as_str()) {
                warnings.push(format!(
                    "leader submenu `{name}` is unreachable — no `[keys.leader]` key \
                     descends into it (add e.g. `<key> = \"+{name}\"`)"
                ));
            }
        }

        // (d) Two combo strings normalizing to the same key within one config
        // scope — a silent last-wins the map has already collapsed.
        warnings.extend(duplicate_combo_warnings("[keys]", global));
        for (table, bindings) in overlays {
            warnings.extend(duplicate_combo_warnings(
                &format!("[keys.{table}]"),
                bindings,
            ));
        }

        // (e) A global bind shadowed in EVERY pane overlay — globally
        // unreachable. Only flag combos that exist in all four overlays and are
        // rebound (to any other action) in each, so a single-pane documented
        // shadow (Response `h`) never trips this.
        for (combo, global_action) in &self.map {
            let shadowed_everywhere = PaneCtx::all().into_iter().all(|ctx| {
                self.overlays
                    .get(&ctx)
                    .and_then(|o| o.get(combo))
                    .is_some_and(|a| a != global_action)
            });
            if shadowed_everywhere {
                warnings.push(format!(
                    "global bind `{combo}` ({}) is shadowed in every pane overlay — \
                     it is globally unreachable",
                    global_action.name(),
                ));
            }
        }

        warnings.sort();
        warnings
    }

    /// Looks up the action bound to a key event in the *global* map only.
    pub fn lookup(&self, key: KeyEvent) -> Option<Action> {
        self.map.get(&KeyCombination::from(key)).copied()
    }

    /// Looks up the action bound to a key event, consulting `ctx`'s overlay first
    /// and falling back to the global map. This is the precedence that lets `1`–`4`
    /// mean tab-jump inside the Request pane but pane-focus globally.
    pub fn lookup_ctx(&self, key: KeyEvent, ctx: PaneCtx) -> Option<Action> {
        let combo = KeyCombination::from(key);
        self.overlays
            .get(&ctx)
            .and_then(|overlay| overlay.get(&combo))
            .or_else(|| self.map.get(&combo))
            .copied()
    }

    /// Every `(key combination, action)` binding in the *global* map, unordered.
    /// Callers that need a stable order (e.g. the `keymaps` subcommand) sort the
    /// result.
    pub fn iter(&self) -> impl Iterator<Item = (KeyCombination, Action)> + '_ {
        self.map.iter().map(|(combo, action)| (*combo, *action))
    }

    /// Every `(key combination, action)` binding in `ctx`'s overlay, unordered.
    pub fn iter_overlay(
        &self,
        ctx: PaneCtx,
    ) -> impl Iterator<Item = (KeyCombination, Action)> + '_ {
        self.overlays
            .get(&ctx)
            .into_iter()
            .flat_map(|overlay| overlay.iter().map(|(combo, action)| (*combo, *action)))
    }

    /// All global combinations bound to `action`, as their canonical display
    /// strings, sorted for determinism.
    pub fn combos_for(&self, action: Action) -> Vec<String> {
        sorted_combos(self.map.iter(), action)
    }

    /// All combinations bound to `action` in `ctx`'s overlay, sorted.
    pub fn overlay_combos_for(&self, ctx: PaneCtx, action: Action) -> Vec<String> {
        match self.overlays.get(&ctx) {
            Some(overlay) => sorted_combos(overlay.iter(), action),
            None => Vec::new(),
        }
    }
}

/// Warns about two combo *strings* in `bindings` that normalize to the same key
/// (a silent last-wins). `where_` tags the scope. Bad combos are skipped here —
/// they are a hard parse error elsewhere.
fn duplicate_combo_warnings(where_: &str, bindings: &BTreeMap<String, String>) -> Vec<String> {
    let mut seen: HashMap<KeyCombination, String> = HashMap::new();
    let mut out = Vec::new();
    for combo_str in bindings.keys() {
        let Ok(combo) = KeyCombination::from_str(combo_str) else {
            continue;
        };
        let combo = combo.normalized();
        if let Some(first) = seen.get(&combo) {
            out.push(format!(
                "keys `{first}` and `{combo_str}` in {where_} both resolve to `{combo}` \
                 — the later binding silently wins"
            ));
        } else {
            seen.insert(combo, combo_str.clone());
        }
    }
    out
}

/// Parses a `"combo" = "action"` config pair, tagging errors with `where_` (the
/// config table name).
fn parse_binding(
    combo_str: &str,
    action_str: &str,
    where_: &str,
) -> Result<(KeyCombination, Action)> {
    let combo = KeyCombination::from_str(combo_str)
        .map_err(|err| eyre!("bad key combination {combo_str:?} in {where_}: {err}"))?;
    let action = Action::from_str(action_str)
        .map_err(|err| eyre!("bad action for key {combo_str:?} in {where_}: {err}"))?;
    Ok((combo, action))
}

/// Parses a `[keys.leader]` root value: either an action name or a
/// `"+<submenu>"` descent token (e.g. `"+sequences"`, `"+git"`). `combo_str` tags
/// errors. Any non-empty name is accepted as a descent — a config may name a
/// brand-new submenu; the load-time validator (not this parser) flags a descent
/// with no matching `[keys.leader.<name>]` table (a dangling menu).
fn parse_leader_entry(combo_str: &str, value: &str) -> Result<LeaderEntry> {
    if let Some(name) = value.strip_prefix('+') {
        if name.is_empty() {
            return Err(eyre!(
                "empty leader descent {value:?} for key {combo_str:?} in [keys.leader] \
                 (expected a submenu name, e.g. +sequences)"
            ));
        }
        return Ok(LeaderEntry::Submenu(name.to_owned()));
    }
    let action = Action::from_str(value)
        .map_err(|err| eyre!("bad action for key {combo_str:?} in [keys.leader]: {err}"))?;
    Ok(LeaderEntry::Act(action))
}

/// Collects and sorts the display strings of the combinations bound to `action`.
fn sorted_combos<'a>(
    bindings: impl Iterator<Item = (&'a KeyCombination, &'a Action)>,
    action: Action,
) -> Vec<String> {
    let mut combos: Vec<String> = bindings
        .filter(|(_, bound)| **bound == action)
        .map(|(combo, _)| combo.to_string())
        .collect();
    combos.sort();
    combos
}

/// Fuzzy matcher over a list of display strings, wrapping `nucleo-matcher`
/// (the sync engine used by Helix — the threaded `nucleo` crate is overkill for
/// workspace-sized endpoint lists).
pub struct FuzzyFinder {
    matcher: Matcher,
}

impl Default for FuzzyFinder {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FuzzyFinder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuzzyFinder").finish_non_exhaustive()
    }
}

impl FuzzyFinder {
    /// Creates a finder with the default matcher configuration.
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// Returns the indices of `items` matching `query`, best score first
    /// (original order as tie-break). An empty query returns all indices in
    /// their original order.
    pub fn filter(&mut self, query: &str, items: &[String]) -> Vec<usize> {
        if query.is_empty() {
            return (0..items.len()).collect();
        }
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                pattern
                    .score(Utf32Str::new(item, &mut buf), &mut self.matcher)
                    .map(|score| (index, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.into_iter().map(|(index, _)| index).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn default_map_lookups() {
        let keymap = KeyMap::default();
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::Quit)
        );
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            keymap.lookup(press(KeyCode::Tab, KeyModifiers::NONE)),
            Some(Action::FocusNext)
        );
        // crossterm reports Shift-Tab as BackTab with the SHIFT modifier set.
        assert_eq!(
            keymap.lookup(press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Action::FocusPrev)
        );
        // Uppercase G arrives as Char('G') + SHIFT.
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Some(Action::Bottom)
        );
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('/'), KeyModifiers::NONE)),
            Some(Action::OpenSearch)
        );
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn config_override_wins() {
        let overrides = BTreeMap::from([
            ("q".to_string(), "focus-response".to_string()),
            ("ctrl-p".to_string(), "open-palette".to_string()),
        ]);
        let keymap = KeyMap::with_overrides(&overrides).unwrap();
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(Action::FocusResponse)
        );
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(Action::OpenPalette)
        );
        // Untouched defaults survive.
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('j'), KeyModifiers::NONE)),
            Some(Action::Down)
        );
    }

    #[test]
    fn bad_action_name_errors() {
        let overrides = BTreeMap::from([("q".to_string(), "explode".to_string())]);
        let err = KeyMap::with_overrides(&overrides).unwrap_err();
        assert!(err.to_string().contains("explode"), "{err}");
    }

    #[test]
    fn bad_key_combination_errors() {
        let overrides = BTreeMap::from([("ctrl-".to_string(), "quit".to_string())]);
        assert!(KeyMap::with_overrides(&overrides).is_err());
    }

    #[test]
    fn action_name_round_trip() {
        for action in Action::all() {
            assert_eq!(action.name().parse::<Action>().unwrap(), action);
        }
    }

    #[test]
    fn iter_covers_every_default_binding() {
        let keymap = KeyMap::default();
        let bindings: Vec<_> = keymap.iter().collect();
        // Every binding round-trips through lookup.
        for (combo, action) in &bindings {
            assert_eq!(keymap.lookup((*combo).into()), Some(*action));
        }
        // `f` is bound to Jump by default.
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('f'), KeyModifiers::NONE)),
            Some(Action::Jump)
        );
        assert!(bindings.iter().any(|(_, a)| *a == Action::Jump));
    }

    #[test]
    fn combos_for_returns_sorted_bindings() {
        let keymap = KeyMap::default();
        // Quit is bound to both `q` and `ctrl-c`.
        let combos = keymap.combos_for(Action::Quit);
        assert_eq!(combos.len(), 2);
        let mut sorted = combos.clone();
        sorted.sort();
        assert_eq!(combos, sorted, "combos_for must be sorted");
        // SwitchProfile has no default binding.
        assert!(keymap.combos_for(Action::SwitchProfile).is_empty());
    }

    #[test]
    fn overlay_lookup_precedence() {
        let keymap = KeyMap::default();
        // `1` has no global binding (M6.7 removed pane-focus digits); inside the
        // Request overlay it is Tab1.
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('1'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            keymap.lookup_ctx(
                press(KeyCode::Char('1'), KeyModifiers::NONE),
                PaneCtx::Request
            ),
            Some(Action::Tab1)
        );
        // A key not in the overlay falls through to the global map.
        assert_eq!(
            keymap.lookup_ctx(
                press(KeyCode::Char('j'), KeyModifiers::NONE),
                PaneCtx::Request
            ),
            Some(Action::Down)
        );
        // `d` in the Explorer overlay is Delete; in Request it is RowDelete.
        assert_eq!(
            keymap.lookup_ctx(
                press(KeyCode::Char('d'), KeyModifiers::NONE),
                PaneCtx::Explorer
            ),
            Some(Action::Delete)
        );
        assert_eq!(
            keymap.lookup_ctx(
                press(KeyCode::Char('d'), KeyModifiers::NONE),
                PaneCtx::Request
            ),
            Some(Action::RowDelete)
        );
        // The UrlBar overlay: `m` cycles method, `i` edits URL.
        assert_eq!(
            keymap.lookup_ctx(
                press(KeyCode::Char('m'), KeyModifiers::NONE),
                PaneCtx::UrlBar
            ),
            Some(Action::MethodCycle)
        );
    }

    /// Arrow keys navigate the endpoint/sequence explorer, mirroring the global
    /// `k`/`j`/`h`/`l` (owner drive-test 2026-07-10). Scoped to the Explorer
    /// overlay so Left/Right→Collapse/Expand never leak into other panes.
    #[test]
    fn explorer_overlay_arrow_keys_navigate() {
        let keymap = KeyMap::default();
        assert_eq!(
            keymap.lookup_ctx(press(KeyCode::Up, KeyModifiers::NONE), PaneCtx::Explorer),
            Some(Action::Up)
        );
        assert_eq!(
            keymap.lookup_ctx(press(KeyCode::Down, KeyModifiers::NONE), PaneCtx::Explorer),
            Some(Action::Down)
        );
        assert_eq!(
            keymap.lookup_ctx(press(KeyCode::Left, KeyModifiers::NONE), PaneCtx::Explorer),
            Some(Action::Collapse)
        );
        assert_eq!(
            keymap.lookup_ctx(press(KeyCode::Right, KeyModifiers::NONE), PaneCtx::Explorer),
            Some(Action::Expand)
        );
        // Left/Right stay Explorer-scoped: no global bind, so Collapse/Expand
        // never leak into other panes via the arrows.
        assert_eq!(
            keymap.lookup(press(KeyCode::Left, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            keymap.lookup(press(KeyCode::Right, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn overlay_override_parses_and_wins() {
        let overlays = BTreeMap::from([(
            "request".to_string(),
            BTreeMap::from([("x".to_string(), "tab-next".to_string())]),
        )]);
        let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
        assert_eq!(
            keymap.lookup_ctx(
                press(KeyCode::Char('x'), KeyModifiers::NONE),
                PaneCtx::Request
            ),
            Some(Action::TabNext)
        );
        // Default overlay bindings survive.
        assert_eq!(
            keymap.lookup_ctx(
                press(KeyCode::Char('1'), KeyModifiers::NONE),
                PaneCtx::Request
            ),
            Some(Action::Tab1)
        );
    }

    #[test]
    fn unknown_overlay_table_errors() {
        let overlays = BTreeMap::from([(
            "bogus".to_string(),
            BTreeMap::from([("x".to_string(), "tab-next".to_string())]),
        )]);
        let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn bad_overlay_action_errors() {
        let overlays = BTreeMap::from([(
            "request".to_string(),
            BTreeMap::from([("x".to_string(), "explode".to_string())]),
        )]);
        let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
        assert!(err.to_string().contains("explode"), "{err}");
    }

    #[test]
    fn overlay_combos_and_iter_cover_bindings() {
        let keymap = KeyMap::default();
        assert_eq!(
            keymap.overlay_combos_for(PaneCtx::Request, Action::Tab1),
            vec!["1"]
        );
        let request: Vec<_> = keymap.iter_overlay(PaneCtx::Request).collect();
        assert!(request.iter().any(|(_, a)| *a == Action::TabNext));
        // Save is a global binding, not an overlay one.
        assert_eq!(keymap.combos_for(Action::Save), vec!["w"]);
    }

    #[test]
    fn digit_binds_only_act_in_request() {
        let keymap = KeyMap::default();
        // No global digit binds (1–4) after M6.7.
        for d in ['1', '2', '3', '4'] {
            assert_eq!(
                keymap.lookup(press(KeyCode::Char(d), KeyModifiers::NONE)),
                None,
                "'{d}' must have no global binding"
            );
        }
        // 1–4 still act as tab jumps inside the Request overlay.
        for (d, action) in [
            ('1', Action::Tab1),
            ('2', Action::Tab2),
            ('3', Action::Tab3),
            ('4', Action::Tab4),
        ] {
            assert_eq!(
                keymap.lookup_ctx(
                    press(KeyCode::Char(d), KeyModifiers::NONE),
                    PaneCtx::Request
                ),
                Some(action)
            );
            // And no other pane overlay binds them.
            for ctx in [PaneCtx::Explorer, PaneCtx::UrlBar, PaneCtx::Response] {
                assert_eq!(
                    keymap.lookup_ctx(press(KeyCode::Char(d), KeyModifiers::NONE), ctx),
                    None,
                    "'{d}' must be inert in {ctx:?}"
                );
            }
        }
    }

    #[test]
    fn default_leader_map() {
        let keymap = KeyMap::default();
        assert_eq!(keymap.leader(), key!(space).normalized());
        assert!(keymap.is_leader(press(KeyCode::Char(' '), KeyModifiers::NONE)));
        // Direct root actions.
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('e'), KeyModifiers::NONE)),
            Some(LeaderEntry::Act(Action::ToggleExplorer))
        );
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('c'), KeyModifiers::NONE)),
            Some(LeaderEntry::Act(Action::Cancel))
        );
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(LeaderEntry::Act(Action::Quit))
        );
        // Submenu descents.
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('s'), KeyModifiers::NONE)),
            Some(LeaderEntry::Submenu("sequences".to_owned()))
        );
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('l'), KeyModifiers::NONE)),
            Some(LeaderEntry::Submenu("load".to_owned()))
        );
        // `<leader><leader>` (Space) is the endpoint/request picker — moved off
        // `f` (owner drive-test 2026-07-10). A leader continuation of Space is
        // fine; `f` is now free at root.
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char(' '), KeyModifiers::NONE)),
            Some(LeaderEntry::Act(Action::QuickJumpRequests))
        );
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('f'), KeyModifiers::NONE)),
            None
        );
        // An unbound root continuation returns None (the popup dismisses).
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('x'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn leader_submenu_lookups() {
        let keymap = KeyMap::default();
        assert_eq!(
            keymap.leader_sub_lookup("sequences", press(KeyCode::Char('r'), KeyModifiers::NONE)),
            // D1: `<leader>s r` routes to the run-flavored chooser.
            Some(Action::RunSequencePick)
        );
        assert_eq!(
            keymap.leader_sub_lookup("sequences", press(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(Action::EditSequence)
        );
        // `<leader>s <leader>` (Space) is the single sequence finder (mirrors
        // `<leader><leader>` for endpoints); the former `o`/`f` binds are gone
        // (owner drive-test 2026-07-10).
        assert_eq!(
            keymap.leader_sub_lookup("sequences", press(KeyCode::Char(' '), KeyModifiers::NONE)),
            Some(Action::OpenSequencePicker)
        );
        assert_eq!(
            keymap.leader_sub_lookup("sequences", press(KeyCode::Char('o'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            keymap.leader_sub_lookup("sequences", press(KeyCode::Char('f'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            keymap.leader_sub_lookup("load", press(KeyCode::Char('c'), KeyModifiers::NONE)),
            Some(Action::OpenLoadRunner)
        );
        // `<leader>l <leader>` (Space) picks an endpoint first (was `f`).
        assert_eq!(
            keymap.leader_sub_lookup("load", press(KeyCode::Char(' '), KeyModifiers::NONE)),
            Some(Action::OpenLoadRunnerPick)
        );
        assert_eq!(
            keymap.leader_sub_lookup("load", press(KeyCode::Char('f'), KeyModifiers::NONE)),
            None
        );
        // `<leader>l s` is reserved (composable-runs) — must stay unbound.
        assert_eq!(
            keymap.leader_sub_lookup("load", press(KeyCode::Char('s'), KeyModifiers::NONE)),
            None
        );
        // An unknown submenu name yields no action.
        assert_eq!(
            keymap.leader_sub_lookup("nope", press(KeyCode::Char('a'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn send_reclaimed_from_leader() {
        let keymap = KeyMap::default();
        // `<leader>s` descends into the sequences submenu — it is NOT Send.
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('s'), KeyModifiers::NONE)),
            Some(LeaderEntry::Submenu("sequences".to_owned()))
        );
        // Send is nowhere in the leader tree; Ctrl-S still sends.
        assert!(
            !keymap.iter_leader().any(|(_, a)| a == Action::Send),
            "Send must not appear in any leader map"
        );
        assert_eq!(
            keymap.lookup(press(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(Action::Send)
        );
    }

    #[test]
    fn flat_sequence_actions_not_root_bound() {
        let keymap = KeyMap::default();
        // RunSequence / EditSequence are reachable only through the sequences
        // submenu now — not as direct root binds.
        for entry in keymap.leader_root.values() {
            if let LeaderEntry::Act(a) = entry {
                assert!(
                    !matches!(a, Action::RunSequence | Action::EditSequence),
                    "{a:?} must not be a direct root leader bind"
                );
            }
        }
        // leader_combos_for reports the full chord path for submenu actions.
        // D1: `<leader>s r` now maps to the run-flavored chooser.
        assert_eq!(
            keymap.leader_combos_for(Action::RunSequencePick),
            vec!["s r"]
        );
        assert_eq!(
            keymap.leader_combos_for(Action::OpenLoadRunner),
            vec!["l c"]
        );
    }

    #[test]
    fn leader_table_parses_and_overrides() {
        let overlays = BTreeMap::from([(
            "leader".to_string(),
            BTreeMap::from([("x".to_string(), "save".to_string())]),
        )]);
        let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(LeaderEntry::Act(Action::Save))
        );
        // Default root continuations survive.
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(LeaderEntry::Act(Action::Quit))
        );
        assert_eq!(keymap.leader_combos_for(Action::Save), vec!["x"]);
    }

    #[test]
    fn leader_submenu_table_parses() {
        // `[keys.leader.sequences]` binds a submenu action.
        let overlays = BTreeMap::from([(
            "leader.sequences".to_string(),
            BTreeMap::from([("z".to_string(), "run-sequence".to_string())]),
        )]);
        let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
        assert_eq!(
            keymap.leader_sub_lookup("sequences", press(KeyCode::Char('z'), KeyModifiers::NONE)),
            Some(Action::RunSequence)
        );
        // Defaults in the submenu survive.
        assert_eq!(
            keymap.leader_sub_lookup("sequences", press(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(Action::EditSequence)
        );
    }

    #[test]
    fn leader_descent_token_parses() {
        // `[keys.leader]` accepts a `"+<submenu>"` descent token.
        let overlays = BTreeMap::from([(
            "leader".to_string(),
            BTreeMap::from([("g".to_string(), "+load".to_string())]),
        )]);
        let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('g'), KeyModifiers::NONE)),
            Some(LeaderEntry::Submenu("load".to_owned()))
        );
    }

    #[test]
    fn bad_leader_table_action_errors() {
        let overlays = BTreeMap::from([(
            "leader".to_string(),
            BTreeMap::from([("x".to_string(), "explode".to_string())]),
        )]);
        let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
        assert!(err.to_string().contains("explode"), "{err}");
    }

    #[test]
    fn leader_descent_to_new_submenu_parses() {
        // M7.10: any non-empty `"+name"` descent is valid — submenus are dynamic.
        // A dangling one (no matching table) parses fine and is caught later by
        // the load-time validator, not the parser.
        let overlays = BTreeMap::from([(
            "leader".to_string(),
            BTreeMap::from([("x".to_string(), "+bogus".to_string())]),
        )]);
        let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(LeaderEntry::Submenu("bogus".to_owned()))
        );
    }

    #[test]
    fn empty_leader_descent_token_errors() {
        // A bare `"+"` (no submenu name) is still a hard error.
        let overlays = BTreeMap::from([(
            "leader".to_string(),
            BTreeMap::from([("x".to_string(), "+".to_string())]),
        )]);
        let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
        assert!(err.to_string().contains("empty leader descent"), "{err}");
    }

    #[test]
    fn leader_submenu_table_creates_new_submenu() {
        // M7.10: `[keys.leader.git]` creates a brand-new submenu (dynamic).
        let overlays = BTreeMap::from([
            (
                "leader".to_string(),
                BTreeMap::from([("g".to_string(), "+git".to_string())]),
            ),
            (
                "leader.git".to_string(),
                BTreeMap::from([("s".to_string(), "save".to_string())]),
            ),
        ]);
        let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
        assert_eq!(
            keymap.leader_root_lookup(press(KeyCode::Char('g'), KeyModifiers::NONE)),
            Some(LeaderEntry::Submenu("git".to_owned()))
        );
        assert_eq!(
            keymap.leader_sub_lookup("git", press(KeyCode::Char('s'), KeyModifiers::NONE)),
            Some(Action::Save)
        );
        // The new submenu appears in the which-key submenu list, reachable via `g`.
        assert!(
            keymap
                .leader_menu_combos()
                .iter()
                .any(|(k, l)| k == "g" && l == "git")
        );
        // And a clean end-to-end config produces no validator warnings.
        let overlays_config = overlays.clone();
        assert!(
            keymap
                .validate(&BTreeMap::new(), &overlays_config)
                .is_empty()
        );
    }

    #[test]
    fn bad_leader_submenu_table_action_errors() {
        // The submenu name is now free, but a bad action inside it still fails.
        let overlays = BTreeMap::from([(
            "leader.git".to_string(),
            BTreeMap::from([("x".to_string(), "explode".to_string())]),
        )]);
        let err = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap_err();
        assert!(err.to_string().contains("explode"), "{err}");
    }

    #[test]
    fn set_leader_remaps() {
        let mut keymap = KeyMap::default();
        keymap.set_leader("ctrl-b").unwrap();
        assert!(keymap.is_leader(press(KeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(!keymap.is_leader(press(KeyCode::Char(' '), KeyModifiers::NONE)));
        assert!(keymap.set_leader("ctrl-").is_err());
    }

    #[test]
    fn validate_clean_default_config_has_no_warnings() {
        // The out-of-the-box keymap must produce ZERO warnings — documented
        // single-pane shadows (Response `h`/`/`, Request `1`–`4`) are lawful,
        // and so is Space bound as a leader *continuation*
        // (`<leader><leader>`/`<leader>s <leader>`/`<leader>l <leader>`, owner
        // drive-test 2026-07-10): `validate` only flags the leader key when it
        // is (re)bound in the GLOBAL map or a PANE overlay, never as a leader
        // continuation.
        let keymap = KeyMap::default();
        assert!(
            keymap
                .validate(&BTreeMap::new(), &BTreeMap::new())
                .is_empty(),
            "default keymap warnings: {:?}",
            keymap.validate(&BTreeMap::new(), &BTreeMap::new())
        );
    }

    #[test]
    fn validate_flags_leader_key_bound_as_action() {
        // The dead `space=…` case: the leader key also bound as a global action.
        let global = BTreeMap::from([("space".to_string(), "save".to_string())]);
        let keymap = KeyMap::with_all_overrides(&global, &BTreeMap::new()).unwrap();
        let warnings = keymap.validate(&global, &BTreeMap::new());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("leader key"), "{warnings:?}");
    }

    #[test]
    fn validate_flags_dangling_descent() {
        // A `"+name"` descent with no matching `[keys.leader.name]` table.
        let overlays = BTreeMap::from([(
            "leader".to_string(),
            BTreeMap::from([("g".to_string(), "+git".to_string())]),
        )]);
        let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
        let warnings = keymap.validate(&BTreeMap::new(), &overlays);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("git"), "{warnings:?}");
        assert!(
            warnings[0].contains("no `[keys.leader.git]`"),
            "{warnings:?}"
        );
    }

    #[test]
    fn validate_flags_orphan_submenu() {
        // A `[keys.leader.name]` table that no descent points to.
        let overlays = BTreeMap::from([(
            "leader.git".to_string(),
            BTreeMap::from([("s".to_string(), "save".to_string())]),
        )]);
        let keymap = KeyMap::with_all_overrides(&BTreeMap::new(), &overlays).unwrap();
        let warnings = keymap.validate(&BTreeMap::new(), &overlays);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("unreachable"), "{warnings:?}");
    }

    #[test]
    fn validate_flags_duplicate_combo_in_scope() {
        // Two combo strings normalizing to the same key within one scope
        // (modifier order differs, but they resolve to the same combination).
        let global = BTreeMap::from([
            ("ctrl-shift-a".to_string(), "quit".to_string()),
            ("shift-ctrl-a".to_string(), "save".to_string()),
        ]);
        let keymap = KeyMap::with_all_overrides(&global, &BTreeMap::new()).unwrap();
        let warnings = keymap.validate(&global, &BTreeMap::new());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("silently wins"), "{warnings:?}");
    }

    #[test]
    fn validate_flags_global_shadowed_in_every_overlay() {
        // A global bind rebound in all four pane overlays — globally unreachable.
        let global = BTreeMap::from([("ctrl-g".to_string(), "quit".to_string())]);
        let overlays = BTreeMap::from([
            (
                "explorer".to_string(),
                BTreeMap::from([("ctrl-g".to_string(), "save".to_string())]),
            ),
            (
                "urlbar".to_string(),
                BTreeMap::from([("ctrl-g".to_string(), "save".to_string())]),
            ),
            (
                "request".to_string(),
                BTreeMap::from([("ctrl-g".to_string(), "save".to_string())]),
            ),
            (
                "response".to_string(),
                BTreeMap::from([("ctrl-g".to_string(), "save".to_string())]),
            ),
        ]);
        let keymap = KeyMap::with_all_overrides(&global, &overlays).unwrap();
        let warnings = keymap.validate(&global, &overlays);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("globally unreachable"), "{warnings:?}");
    }

    #[test]
    fn fuzzy_empty_query_returns_all() {
        let items = vec!["alpha".to_string(), "beta".to_string()];
        let mut finder = FuzzyFinder::new();
        assert_eq!(finder.filter("", &items), vec![0, 1]);
    }

    #[test]
    fn fuzzy_match_ordering() {
        let items = vec![
            "users/list all users".to_string(),
            "orders/create order".to_string(),
            "users/get user".to_string(),
        ];
        let mut finder = FuzzyFinder::new();
        let hits = finder.filter("user", &items);
        // Both user endpoints match; the order collection does not.
        assert!(!hits.contains(&1));
        assert_eq!(hits.len(), 2);
        // Case-insensitive.
        let hits = finder.filter("USER", &items);
        assert_eq!(hits.len(), 2);
    }
}
