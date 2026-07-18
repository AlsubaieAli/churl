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

mod action;
mod fuzzy;

// Re-export the child modules' public items so every external `use` path stays
// unchanged after the split: `events::{Action, UnknownAction}` (config parsing,
// palette, help, snapshot tests) and `events::FuzzyFinder` (picker, app). All
// were `pub` at the module root and stay `pub`.
pub use action::{Action, UnknownAction};
pub use fuzzy::FuzzyFinder;

// `FOCUS_BUFFER_TABLE` moved with `Action` but stays module-private
// (`pub(in crate::tui::events)`); the keymap default below still consults it, so
// pull it into this module's namespace by path — no visibility widening.
use action::FOCUS_BUFFER_TABLE;

/// A nested-leader submenu: `<leader><prefix>` descends into one of these, and a
/// second key selects an action within it. Submenus are fully data-driven — the
/// built-in three (sequences/load/tabs) are seeded as default data in
/// [`KeyMap::default`], and a config `[keys.leader.<name>]` table creates or
/// extends any submenu by name. No closed enum of submenu kinds.
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
/// submenus (dynamic leader submenus).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderEntry {
    /// A direct action dispatched from the root which-key popup.
    Act(Action),
    /// A descent into a nested submenu, keyed by its name (the
    /// [`KeyMap::submenus`] map key).
    Submenu(String),
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
    /// The response pane (viewer keys: headers/wrap/search/fold/copy).
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
    /// Named leader submenus (dynamic submenus): submenu name → its
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
        // There are no global `1`/`2`/`3` pane-focus binds (DECISIONS.md):
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
        // Global buffer-tab nav: `}` next
        // buffer, `{` prev buffer — fast chip-tab switching from ANY pane, so these
        // live in the base/global map (not a pane overlay). `[`/`]` are deliberately
        // NOT touched (they stay Request-pane sub-tab prev/next in that overlay);
        // `{`/`}` were verified free globally. `<leader>t n/p` and `<leader>t <n>`
        // keep working — this is an addition.
        bind(key!('}'), Action::BufferNext);
        bind(key!('{'), Action::BufferPrev);
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
        // Reorder the selected node among its siblings: `K` up, `J` down (mnemonic
        // "move the item", mirroring the sequence-editor Ctrl-j/Ctrl-k swap). `J`/`K`
        // are free in the Explorer overlay. move-to/copy-to/duplicate stay
        // palette-only (rare reorganization ops).
        overlay(PaneCtx::Explorer, key!(shift - k), Action::MoveUp);
        overlay(PaneCtx::Explorer, key!(shift - j), Action::MoveDown);
        // `s` switches focus/zoom between the endpoints tree and the sequences
        // sub-pane — a lawful in-pane move, only live when the left
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
        // Space is the global leader; the Request row-toggle rebinds to
        // `t` so Space stays free everywhere (DECISIONS.md).
        overlay(PaneCtx::Request, key!(t), Action::RowToggle);
        overlay(PaneCtx::Request, key!(i), Action::RowEdit);
        // Copy-as-curl is `<leader>y` (see the leader binds below): leader keys are
        // inert during text edits, so it never shadows body-editor input. The
        // resolved-vars variant and the interchange import/export actions stay
        // palette-only (rare; a path prompt is the natural entry point).
        // URL bar: the vim-popup editor (`e`), independent of the inline `i`/Enter.
        overlay(PaneCtx::UrlBar, key!(e), Action::EditUrlPopup);
        // Response pane: body/headers, wrap, search, match nav, folding,
        // copy. `h` shadows the global Collapse and `/` shadows the global
        // OpenSearch here (same precedent as Request-pane `1`–`4`; DECISIONS.md).
        overlay(PaneCtx::Response, key!(h), Action::ToggleHeadersView);
        overlay(PaneCtx::Response, key!(shift - w), Action::ToggleWrap);
        // `p` (pretty) toggles raw↔reformatted body rendering. `p` is free
        // in the Response overlay (the global `<leader>p` switch-profile lives
        // behind the leader, not in this pane overlay).
        overlay(PaneCtx::Response, key!(p), Action::TogglePretty);
        // `s` (sort) toggles A→Z key sorting on the pretty JSON body. Free
        // in the Response overlay; the leader `s` (sequences submenu) lives behind
        // the leader, not this pane overlay.
        overlay(PaneCtx::Response, key!(s), Action::ToggleSortKeys);
        // `#` toggles the line-number gutter (default on). `#`
        // is free in the Response overlay and everywhere else in the keymap.
        overlay(PaneCtx::Response, key!('#'), Action::ToggleLineNumbers);
        overlay(PaneCtx::Response, key!('/'), Action::OpenBodySearch);
        overlay(PaneCtx::Response, key!(n), Action::SearchNext);
        overlay(PaneCtx::Response, key!(shift - n), Action::SearchPrev);
        overlay(PaneCtx::Response, key!(o), Action::ToggleFold);
        overlay(PaneCtx::Response, key!(shift - o), Action::ToggleAllFolds);
        // `J`/`K` jump the cursor between collapsible JSON nodes (forward/inward,
        // backward/outward), skipping leaf lines. Free in the Response overlay;
        // the global `j`/`k` are lowercase line movement, not shadowed.
        overlay(PaneCtx::Response, key!(shift - j), Action::StructuralNext);
        overlay(PaneCtx::Response, key!(shift - k), Action::StructuralPrev);
        overlay(PaneCtx::Response, key!(y), Action::CopyResponse);
        overlay(PaneCtx::Response, key!(shift - y), Action::CopyLine);
        // Horizontal window pan for unwrapped long lines. `H`/`L`
        // (shift-h/shift-l) were free in the Response overlay; Left/Right arrows are
        // aliases (also free here — global Left/Right are unbound, only the Explorer
        // overlay uses them). No-op while wrap is on. Wrap-off-only is enforced in
        // the handler, not the binding, so the keys stay reserved for panning.
        overlay(PaneCtx::Response, key!(shift - h), Action::ScrollBodyLeft);
        overlay(PaneCtx::Response, key!(shift - l), Action::ScrollBodyRight);
        overlay(PaneCtx::Response, key!(left), Action::ScrollBodyLeft);
        overlay(PaneCtx::Response, key!(right), Action::ScrollBodyRight);

        // The leader chord: Space, then a continuation key. Root binds are either
        // direct actions or descents into a nested submenu (two-level which-key).
        let mut leader_root = HashMap::new();
        let mut root_bind = |combo: KeyCombination, entry: LeaderEntry| {
            leader_root.insert(combo.normalized(), entry);
        };
        // Direct root actions. `s` (formerly Send) now descends into the
        // sequences submenu — Send stays Ctrl-S ONLY (bound in `map` above).
        root_bind(key!(e), LeaderEntry::Act(Action::ToggleExplorer));
        // There is no `<leader>S`: the sequences sub-pane is
        // always peek-visible, so a show/hide toggle has no job. Reaching the
        // sequences sub-pane stays covered by the Explorer `s` overlay, `f`-jump
        // (`s` label) and the `<leader>s f` picker.
        root_bind(key!(c), LeaderEntry::Act(Action::Cancel));
        root_bind(key!(p), LeaderEntry::Act(Action::SwitchProfile));
        // `<leader>v` opens the environments & variables editor (`v` is free).
        root_bind(key!(v), LeaderEntry::Act(Action::OpenEnvEditor));
        // `<leader>o` opens the session Options overlay (proxy / TLS / cookies);
        // `<leader>k` toggles insecure-TLS from anywhere. Both letters were
        // verified free at the leader root (mnemonics: o=options, k=insecure).
        root_bind(key!(o), LeaderEntry::Act(Action::OpenOptions));
        root_bind(key!(k), LeaderEntry::Act(Action::ToggleInsecure));
        // `<leader>K` (shift-k) toggles the SELECTED endpoint's durable insecure-TLS
        // opt-in — a persisted per-endpoint flag, deliberately distinct from the
        // session-wide `<leader>k`. `K` was free at the leader root.
        root_bind(
            key!(shift - k),
            LeaderEntry::Act(Action::ToggleEndpointInsecure),
        );
        // `<leader>d` opens the debug Inspector overlay; `<leader>D` (shift-d)
        // flips the session debug-capture toggle — mirrors the `k`/`K` pairing
        // above (a direct session action alongside a related one). Both were
        // free at the leader root: plain `d` only exists in the
        // Explorer/Request PANE overlays (Delete / RowDelete), a disjoint
        // namespace from leader-root continuations.
        root_bind(key!(d), LeaderEntry::Act(Action::OpenInspector));
        root_bind(key!(shift - d), LeaderEntry::Act(Action::ToggleDebug));
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
        // Reload the workspace from disk (re-read `churl.toml`, rebuild the
        // explorer); `r` is free at root (plain `r` is the Explorer-overlay
        // rename, a distinct non-leader state).
        root_bind(key!(r), LeaderEntry::Act(Action::Reload));
        // Unified create gestures: `<leader>n` new endpoint, `<leader>N` new
        // collection — each opens the destination picker (choose where), then the
        // shared name prompt. `n`/`N` are free at root (the Explorer overlay `n`/`N`
        // are cursor-context creates, a distinct non-leader state).
        root_bind(key!(n), LeaderEntry::Act(Action::NewEndpointPick));
        root_bind(key!(shift - n), LeaderEntry::Act(Action::NewCollectionPick));
        // Submenu descents (two-level which-key). The submenu names match the
        // seeded `submenus` keys below; config can point new keys at them or
        // create fresh submenus.
        root_bind(key!(s), LeaderEntry::Submenu("sequences".to_owned()));
        root_bind(key!(l), LeaderEntry::Submenu("load".to_owned()));
        // `<leader>t` descends into the tabs/buffers submenu. `t` is free at root.
        root_bind(key!(t), LeaderEntry::Submenu("tabs".to_owned()));

        // The built-in submenus, seeded as default data. Behaviour is
        // byte-identical to the former hardcoded `sub_*` maps.
        let mut submenus: HashMap<String, Submenu> = HashMap::new();

        // `<leader>s …`: sequence actions (new / open / run).
        let mut sequences = Submenu::new("sequences");
        // `<leader>s n` creates a new sequence. Replaces `<leader>s a`, which
        // opened the FIRST sequence (a `selected_sequence()`-always-0 bug) instead
        // of creating one.
        sequences
            .binds
            .insert(key!(n).normalized(), Action::NewSequence);
        // `<leader>s <leader>` (Space) is the single "find/open a sequence"
        // picker, mirroring `<leader><leader>` for endpoints (owner drive-test
        // 2026-07-10) — one key for one job; the former `o`/`f` binds are gone. A
        // leader continuation of Space stays warning-clean in `validate` (see the
        // root Space bind above).
        sequences
            .binds
            .insert(key!(space).normalized(), Action::OpenSequencePicker);
        // `<leader>s r` routes to a run-flavored chooser (pick which sequence
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
        // `X` (shift-x) close all · `1`..`9` jump to the Nth open tab.
        // Do NOT touch `Tab`/`Shift-Tab` (cross-pane). The digit binds live ONLY
        // in this submenu layer, so they never shadow the Request-pane `1`..`4`
        // tab-jump overlay (a separate `PaneCtx::Request` layer, bound below).
        let mut tabs = Submenu::new("tabs");
        tabs.binds.insert(key!(n).normalized(), Action::BufferNext);
        tabs.binds.insert(key!(p).normalized(), Action::BufferPrev);
        tabs.binds.insert(key!(x).normalized(), Action::BufferClose);
        tabs.binds
            .insert(key!(shift - x).normalized(), Action::BufferCloseAll);
        // `<leader>t 1` … `<leader>t 9` → jump straight to the Nth open tab.
        let digit_combos = [
            key!('1'),
            key!('2'),
            key!('3'),
            key!('4'),
            key!('5'),
            key!('6'),
            key!('7'),
            key!('8'),
            key!('9'),
        ];
        for (combo, (n, _, _)) in digit_combos.into_iter().zip(FOCUS_BUFFER_TABLE) {
            tabs.binds
                .insert(combo.normalized(), Action::FocusBufferIndex(*n));
        }
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
            // unknown table. The submenu is created on first mention (dynamic
            // submenus), so `g = "+git"` + `[keys.leader.git]` wires a
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

    /// Load-time conflict/shadow validator. Runs on the final keymap and
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
#[cfg(test)]
mod tests;
