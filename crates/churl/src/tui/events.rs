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
}

/// `(action, config name, palette label)` for every action, in palette order.
const ACTION_TABLE: &[(Action, &str, &str)] = &[
    (Action::Quit, "quit", "quit"),
    (Action::FocusNext, "focus-next", "focus next pane"),
    (Action::FocusPrev, "focus-prev", "focus previous pane"),
    (Action::FocusExplorer, "focus-explorer", "focus explorer"),
    (Action::FocusRequest, "focus-request", "focus request"),
    (Action::FocusResponse, "focus-response", "focus response"),
    (Action::Up, "up", "move up"),
    (Action::Down, "down", "move down"),
    (Action::Select, "select", "select / toggle"),
    (Action::Collapse, "collapse", "collapse / parent"),
    (Action::Expand, "expand", "expand / descend"),
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

/// Key-combination → [`Action`] map. Defaults are built with crokey's `key!`
/// macro; config overrides layer on top via [`KeyMap::with_overrides`].
#[derive(Debug)]
pub struct KeyMap {
    map: HashMap<KeyCombination, Action>,
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
        bind(key!('1'), Action::FocusExplorer);
        bind(key!('2'), Action::FocusRequest);
        bind(key!('3'), Action::FocusResponse);
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
        // `SwitchProfile` has no default key binding: it lives in the command
        // palette ("switch profile"). Overlay-level modes (search/palette/jump)
        // still take routing precedence over it.
        Self { map }
    }
}

impl KeyMap {
    /// Builds the default map with config `keys` overrides layered on top.
    ///
    /// Unknown key combinations or action names are a hard error — a silently
    /// dropped binding is worse than a startup failure the user can fix.
    pub fn with_overrides(overrides: &BTreeMap<String, String>) -> Result<Self> {
        let mut keymap = Self::default();
        for (combo_str, action_str) in overrides {
            let combo = KeyCombination::from_str(combo_str)
                .map_err(|err| eyre!("bad key combination {combo_str:?} in [keys]: {err}"))?;
            let action = Action::from_str(action_str)
                .map_err(|err| eyre!("bad action for key {combo_str:?} in [keys]: {err}"))?;
            keymap.map.insert(combo.normalized(), action);
        }
        Ok(keymap)
    }

    /// Looks up the action bound to a key event, if any.
    pub fn lookup(&self, key: KeyEvent) -> Option<Action> {
        self.map.get(&KeyCombination::from(key)).copied()
    }

    /// Every `(key combination, action)` binding, unordered. Callers that need a
    /// stable order (e.g. the `keymaps` subcommand) sort the result.
    pub fn iter(&self) -> impl Iterator<Item = (KeyCombination, Action)> + '_ {
        self.map.iter().map(|(combo, action)| (*combo, *action))
    }

    /// All combinations bound to `action`, as their canonical display strings,
    /// sorted for determinism.
    pub fn combos_for(&self, action: Action) -> Vec<String> {
        let mut combos: Vec<String> = self
            .map
            .iter()
            .filter(|(_, bound)| **bound == action)
            .map(|(combo, _)| combo.to_string())
            .collect();
        combos.sort();
        combos
    }
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
