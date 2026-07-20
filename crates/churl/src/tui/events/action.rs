//! The semantic [`Action`] enum, its config-name/label lookup tables, and the
//! `FromStr`/`Display` conversions between an action and its stable config name.
//! Split out of the events module as a child module; the private lookup
//! tables keep their module scope via `pub(in crate::tui::events)` so the keymap
//! core in the parent reaches them unchanged.

use std::str::FromStr;

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
    /// Open the Settings panel (M8.5): Request / Network / Load / Appearance /
    /// Debug knobs, with a `Save as default` action to persist them.
    OpenSettings,
    /// Toggle insecure-TLS (certificate verification off/on) for the session.
    ToggleInsecure,
    /// Open the debug Inspector overlay over the latest exchange's captured
    /// trace (a placeholder when none was captured).
    OpenInspector,
    /// Toggle session debug capture on/off — gates whether a send builds a
    /// `DebugTrace` the Inspector can show.
    ToggleDebug,
    /// Open the debug Log panel overlay: a scrollable view of the bounded
    /// `tracing` ring captured while debug capture is on.
    OpenLogPanel,
    /// Toggle the selected endpoint's durable insecure-TLS opt-in (persisted onto
    /// the endpoint file), distinct from the session-wide [`Action::ToggleInsecure`].
    ToggleEndpointInsecure,
    /// Run the selected request sequence (opens the sequence surface, Run face).
    RunSequence,
    /// Edit the selected sequence, or create a new one (opens the sequence surface).
    EditSequence,
    /// Open a picker over all sequence names and open the chosen one (Edit face).
    OpenSequencePicker,
    /// Open a picker over all sequence names and RUN the chosen one
    /// (`<leader>s r`, so the user can choose which sequence to run).
    RunSequencePick,
    /// Open the concurrent-load runner for the selected endpoint.
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
    /// Create a new endpoint via the destination picker (`<leader>n`): choose the
    /// target collection or root explicitly, then the shared name prompt.
    NewEndpointPick,
    /// Create a new collection via the destination picker (`<leader>N`).
    NewCollectionPick,
    /// Create a new request sequence (`<leader>s n`): opens the name prompt, then
    /// the editor on the created file.
    NewSequence,
    /// Rename the selected endpoint or collection.
    Rename,
    /// Delete the selected endpoint or collection (with a confirm).
    Delete,
    /// Move the selected node (endpoint or collection) into another collection or
    /// the root, via the destination picker.
    MoveTo,
    /// Copy the selected node (endpoint or collection) into another collection or
    /// the root, via the destination picker.
    CopyTo,
    /// Duplicate the selected node (endpoint / collection / sequence) in place
    /// (`-N` suffix; recursive for a collection).
    Duplicate,
    /// Reorder the selected node up one slot among its siblings.
    MoveUp,
    /// Reorder the selected node down one slot among its siblings.
    MoveDown,
    /// Delete the selected sequence in the Sequences sub-pane (with a y/n
    /// confirm). Parallels [`Action::Delete`] for the endpoints tree.
    DeleteSequence,
    /// Toggle the explorer sidebar (hide / reopen).
    ToggleExplorer,
    /// Re-read the workspace manifest (`churl.toml`) from disk and rebuild the
    /// explorer, so external edits to `churl.toml`/`folder.toml` (another editor
    /// or a second churl instance) are picked up without a restart. Defers when
    /// a buffer is dirty so unsaved edits are never discarded.
    Reload,
    /// Switch focus/zoom between the endpoints tree and the sequences sub-pane
    /// inside the (focused) explorer column.
    FocusSequencesToggle,
    /// Cycle forward within the focused region (shipped UNBOUND):
    /// left column ⇒ Endpoints⇄Sequences; right column ⇒ next buffer/tab.
    CycleRegionFwd,
    /// Cycle backward within the focused region (shipped UNBOUND):
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
    /// Toggle A→Z alphabetical sorting of pretty JSON object keys.
    ToggleSortKeys,
    /// Toggle the response viewer's left-hand line-number gutter (default on).
    ToggleLineNumbers,
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
    /// Jump the response cursor forward/inward to the next collapsible JSON node.
    StructuralNext,
    /// Jump the response cursor backward/outward to the previous collapsible JSON
    /// node.
    StructuralPrev,
    /// Copy the current response view's full text to the clipboard.
    CopyResponse,
    /// Copy the response cursor's logical line to the clipboard.
    CopyLine,
    /// Pan the response horizontal window left (unwrapped long lines).
    ScrollBodyLeft,
    /// Pan the response horizontal window right (unwrapped long lines).
    ScrollBodyRight,
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
    /// Jump directly to the Nth open buffer/tab (1-based; `<leader>t <n>`, note
    /// #5). Out of range is a graceful no-op with a status message — never a
    /// panic or a wrong-tab jump. Distinct from the request-pane `Tab1`..`Tab4`
    /// (which switch Params/Headers/Auth/Body); those live in the Request pane
    /// overlay, this only in the tabs leader submenu, so the digits never clash.
    FocusBufferIndex(usize),
}

/// The `(index, config name, palette label)` rows for the parameterized
/// [`Action::FocusBufferIndex`] variants (1-based, `1..=9`). Kept as a static
/// table with `'static` strings so [`Action::name`]/[`Action::label`] can return
/// `&'static str` for a data-carrying variant, and so `from_str`/`all` round-trip
/// exactly like the flat [`ACTION_TABLE`] entries.
pub(in crate::tui::events) const FOCUS_BUFFER_TABLE: &[(usize, &str, &str)] = &[
    (1, "focus-buffer-1", "tab: jump to 1"),
    (2, "focus-buffer-2", "tab: jump to 2"),
    (3, "focus-buffer-3", "tab: jump to 3"),
    (4, "focus-buffer-4", "tab: jump to 4"),
    (5, "focus-buffer-5", "tab: jump to 5"),
    (6, "focus-buffer-6", "tab: jump to 6"),
    (7, "focus-buffer-7", "tab: jump to 7"),
    (8, "focus-buffer-8", "tab: jump to 8"),
    (9, "focus-buffer-9", "tab: jump to 9"),
];

/// `(action, config name, palette label)` for every action, in palette order.
pub(in crate::tui::events) const ACTION_TABLE: &[(Action, &str, &str)] = &[
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
    (Action::Jump, "jump", "jump to pane"),
    (Action::SwitchProfile, "switch-profile", "switch profile"),
    (Action::OpenEnvEditor, "env-editor", "Environments & vars"),
    (
        Action::OpenSettings,
        "settings",
        "Settings (request · network · load · debug)",
    ),
    (
        Action::ToggleInsecure,
        "toggle-insecure",
        "toggle insecure TLS (verify off/on)",
    ),
    (
        Action::OpenInspector,
        "open-inspector",
        "debug inspector (request · vars · redirects)",
    ),
    (Action::ToggleDebug, "toggle-debug", "toggle debug capture"),
    (
        Action::OpenLogPanel,
        "open-log-panel",
        "debug log panel (tracing ring)",
    ),
    (
        Action::ToggleEndpointInsecure,
        "toggle-endpoint-insecure",
        "toggle insecure TLS for this endpoint (saved)",
    ),
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
    (
        Action::NewEndpointPick,
        "new-endpoint-pick",
        "new endpoint (choose location)",
    ),
    (
        Action::NewCollectionPick,
        "new-collection-pick",
        "new collection (choose location)",
    ),
    (Action::NewSequence, "new-sequence", "new sequence"),
    (Action::Rename, "rename", "rename"),
    (Action::Delete, "delete", "delete"),
    (Action::MoveTo, "move-to", "move to…"),
    (Action::CopyTo, "copy-to", "copy to…"),
    (Action::Duplicate, "duplicate", "duplicate"),
    (Action::MoveUp, "move-up", "reorder up"),
    (Action::MoveDown, "move-down", "reorder down"),
    (Action::DeleteSequence, "delete-sequence", "delete sequence"),
    (
        Action::ToggleExplorer,
        "toggle-explorer",
        "toggle explorer sidebar",
    ),
    (Action::Reload, "reload", "reload workspace from disk"),
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
        Action::ToggleLineNumbers,
        "toggle-line-numbers",
        "toggle line-number gutter",
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
    (
        Action::StructuralNext,
        "structural-next",
        "next collapsible node",
    ),
    (
        Action::StructuralPrev,
        "structural-prev",
        "previous collapsible node",
    ),
    (Action::CopyResponse, "copy-response", "copy response"),
    (Action::CopyLine, "copy-line", "copy line"),
    (
        Action::ScrollBodyLeft,
        "scroll-body-left",
        "scroll response left",
    ),
    (
        Action::ScrollBodyRight,
        "scroll-body-right",
        "scroll response right",
    ),
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
    /// All actions, in the order they appear in the command palette: the flat
    /// [`ACTION_TABLE`] followed by the nine parameterized
    /// [`Action::FocusBufferIndex`] variants, so the numbered tab jumps
    /// surface in the palette and the `keymaps` view like any other action.
    pub fn all() -> impl Iterator<Item = Action> {
        ACTION_TABLE.iter().map(|(action, _, _)| *action).chain(
            FOCUS_BUFFER_TABLE
                .iter()
                .map(|(n, _, _)| Action::FocusBufferIndex(*n)),
        )
    }

    /// The stable config-facing name of this action (e.g. `"open-palette"`,
    /// `"focus-buffer-3"`).
    pub fn name(self) -> &'static str {
        if let Action::FocusBufferIndex(n) = self {
            return FOCUS_BUFFER_TABLE
                .iter()
                .find(|(i, _, _)| *i == n)
                .map(|(_, name, _)| *name)
                .expect("FocusBufferIndex out of the 1..=9 range has no name");
        }
        ACTION_TABLE
            .iter()
            .find(|(action, _, _)| *action == self)
            .map(|(_, name, _)| *name)
            .expect("every action is in ACTION_TABLE")
    }

    /// The human-readable palette label of this action (e.g. `"command palette"`).
    pub fn label(self) -> &'static str {
        if let Action::FocusBufferIndex(n) = self {
            return FOCUS_BUFFER_TABLE
                .iter()
                .find(|(i, _, _)| *i == n)
                .map(|(_, _, label)| *label)
                .expect("FocusBufferIndex out of the 1..=9 range has no label");
        }
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
        if let Some((n, _, _)) = FOCUS_BUFFER_TABLE.iter().find(|(_, name, _)| *name == s) {
            return Ok(Action::FocusBufferIndex(*n));
        }
        ACTION_TABLE
            .iter()
            .find(|(_, name, _)| *name == s)
            .map(|(action, _, _)| *action)
            .ok_or_else(|| UnknownAction(s.to_owned()))
    }
}
