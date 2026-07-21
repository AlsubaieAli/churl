//! The Settings panel: a categorized modal generalizing the old session-scoped
//! Options overlay (M8.5) — every base config knob, organized into five
//! categories (Request / Network / Load / Appearance / Debug), navigated
//! MENU → category PANEL (its knobs) → edit a knob.
//!
//! All UI state lives here (the `churl` crate); `churl-core` stays TUI-free. The
//! panel owns only *view* state and emits a [`SettingsOutcome`] describing what
//! the app should do — the app owns the session settings, the client rebuild,
//! and (M8.5 Wave 3) the config-writer, so the panel never mutates them
//! directly. Every knob that has a live seam applies immediately to the
//! session (client rebuild where relevant, exactly as the old Options overlay
//! did); a knob with no cheap live seam (only `leader_key` today) updates the
//! working copy only and takes effect next launch — see [`SettingsOutcome::ApplyLeaderKey`].

use std::collections::HashSet;

use churl_core::config::{RedirectPolicy, ResolvedAdvancedLimits, UrlEditMode};
use churl_core::cookies::{CookieView, SameSite};
use churl_core::load::LoadCaps;
use churl_core::secrets::SecretPolicy;

use super::line_editor::LineEditor;

mod cookie_form;
mod edit;
mod render;
#[cfg(test)]
mod tests;

pub use cookie_form::{CookieForm, CookieFormField};
pub use render::render;

/// The built-in default theme name, used both to seed a fresh session (when no
/// `theme` is set) and as the reset target the Appearance row cycles through.
pub(crate) const DEFAULT_THEME_NAME: &str = "dark";

/// The built-in default leader-key combo string (crokey's `key!(space)`).
pub(crate) const DEFAULT_LEADER_KEY: &str = "space";

/// One mebibyte, in bytes — the Request/Advanced max-body-bytes knob's
/// display unit and `J`/`K` quick-adjust step. The wire format (config,
/// `churl-core`) stays raw bytes; only this crate's display/input/step logic
/// knows about MB/KB.
pub(crate) const MB: u64 = 1_048_576;
const KB: u64 = 1_024;

/// Identifies one knob the Settings panel can persist independently — the
/// unit of "touched this session" tracking (owner-decided fix: Save must
/// persist ONLY the knobs the user actually edited IN THE PANEL this
/// session, never a CLI/session/workspace-forced value the panel merely
/// displays). `App` owns a `HashSet<SettingKey>` marking exactly which knobs
/// were edited through a panel outcome; [`churl_core::config::SettingsDefaults`]
/// (the sparse write-path shape) and the dirty-dot render both key off it.
///
/// `Copy + Eq + Hash` so a `HashSet<SettingKey>` is a cheap touched-set.
/// Deliberately flat (no nested `Load(LoadRow)`/`Advanced(AdvancedField)`
/// variants) so every call site that inserts into the touched-set is a
/// direct, greppable `SettingKey::X` — easy to audit for a missed knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingKey {
    Theme,
    LeaderKey,
    /// Request's Timeout row AND Debug's Advanced `timeout` field — same
    /// live session value (see [`RequestRow::Timeout`]'s doc).
    Timeout,
    /// Request's MaxBodyBytes row AND Debug's Advanced `body cap` field —
    /// same live session value (see [`RequestRow::MaxBodyBytes`]'s doc).
    MaxBodyBytes,
    Redirect,
    UrlEdit,
    SecretPolicy,
    Proxy,
    Insecure,
    Cookies,
    /// The panel's Debug-category toggle row ONLY — the global `<leader>D`
    /// shortcut shares the same underlying `debug_enabled` flag but must
    /// NOT mark this touched (it is not a panel edit).
    Debug,
    LoadWarnTotal,
    LoadWarnConcurrency,
    LoadMaxTotal,
    LoadMaxConcurrency,
    /// Debug-category only — no top-level knob aliases this one.
    AdvancedConcurrency,
    /// Debug-category only — no top-level knob aliases this one.
    AdvancedTotal,
}

/// The top-level navigation level. Menu → Panel is the whole of the outer nav;
/// "editing a knob" is a THIRD level represented by [`SettingsState::editing`]
/// (or, for the Network/Debug list-focused rows, [`PanelFocus`]) rather than a
/// further `SettingsLevel` variant — those states can only exist nested inside
/// `Panel`, so folding them in here would let an editor exist with no category,
/// an illegal state the type would then have to guard against elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsLevel {
    /// The category menu (five entries; Debug hidden unless
    /// [`SettingsState::debug_enabled`]).
    Menu,
    /// Viewing/editing one category's knobs.
    Panel,
}

/// One of the five settings categories. [`SettingsCategory::Debug`] is
/// reachable only when [`SettingsState::debug_enabled`] — menu navigation
/// skips it entirely (functionally absent, not merely unrendered), mirroring
/// how the old Options overlay gated its Advanced row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCategory {
    /// Per-request knobs: timeout, body cap, redirects, URL-edit mode, secret policy.
    Request,
    /// Proxy, TLS verification, and the cookie jar.
    Network,
    /// Concurrent-load guardrail caps.
    Load,
    /// Theme and leader key.
    Appearance,
    /// Master debug toggle + the `[advanced]` overrides. Debug-gated.
    Debug,
}

impl SettingsCategory {
    const ALL: [SettingsCategory; 5] = [
        SettingsCategory::Request,
        SettingsCategory::Network,
        SettingsCategory::Load,
        SettingsCategory::Appearance,
        SettingsCategory::Debug,
    ];

    /// The categories reachable in the menu right now — every category except
    /// [`SettingsCategory::Debug`] when debug capture is off.
    fn visible(debug_enabled: bool) -> Vec<SettingsCategory> {
        Self::ALL
            .into_iter()
            .filter(|c| debug_enabled || *c != SettingsCategory::Debug)
            .collect()
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsCategory::Request => "Request",
            SettingsCategory::Network => "Network",
            SettingsCategory::Load => "Load",
            SettingsCategory::Appearance => "Appearance",
            SettingsCategory::Debug => "Debug",
        }
    }
}

/// Which row is selected within the Request category panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestRow {
    /// The per-request timeout, in seconds. Shares the live session value
    /// with the Debug category's Advanced `timeout` field — there is only
    /// ever ONE effective timeout in a running session (see
    /// [`SettingsState::advanced`]'s doc); this row is the base-knob framing
    /// of the exact same number.
    Timeout,
    /// The response body-size cap, in bytes. Shares the live session value
    /// with the Debug category's Advanced `body cap` field, for the same
    /// reason as `Timeout`.
    MaxBodyBytes,
    /// Cross-origin redirect policy.
    Redirect,
    /// What the URL bar's `i`/`Enter` opens.
    UrlEdit,
    /// Save-time secret policy.
    SecretPolicy,
}

impl RequestRow {
    const ALL: [RequestRow; 5] = [
        RequestRow::Timeout,
        RequestRow::MaxBodyBytes,
        RequestRow::Redirect,
        RequestRow::UrlEdit,
        RequestRow::SecretPolicy,
    ];

    fn next(self) -> Self {
        cycle_next(&Self::ALL, self)
    }

    fn prev(self) -> Self {
        cycle_prev(&Self::ALL, self)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            RequestRow::Timeout => "Timeout (s)",
            RequestRow::MaxBodyBytes => "Max body",
            RequestRow::Redirect => "Redirects",
            RequestRow::UrlEdit => "URL edit",
            RequestRow::SecretPolicy => "Secret policy",
        }
    }
}

/// Which row is selected within the Network category panel. Mirrors the old
/// `OptionsRow` (minus Advanced, which moved to the Debug category).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkRow {
    /// The editable proxy URL row.
    Proxy,
    /// The TLS-verification on/off row.
    Tls,
    /// The cookie-jar on/off row (owns the cookie list below it).
    Cookies,
}

impl NetworkRow {
    /// Clamps at the ends rather than wrapping — ported verbatim from the old
    /// `OptionsRow` navigation (unlike the newly-introduced categories' row
    /// lists below, which wrap; behaviour-preserving for this ported control).
    fn next(self) -> Self {
        match self {
            NetworkRow::Proxy => NetworkRow::Tls,
            NetworkRow::Tls => NetworkRow::Cookies,
            NetworkRow::Cookies => NetworkRow::Cookies,
        }
    }

    fn prev(self) -> Self {
        match self {
            NetworkRow::Proxy => NetworkRow::Proxy,
            NetworkRow::Tls => NetworkRow::Proxy,
            NetworkRow::Cookies => NetworkRow::Tls,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            NetworkRow::Proxy => "Proxy",
            NetworkRow::Tls => "TLS verification",
            NetworkRow::Cookies => "Cookies",
        }
    }
}

/// Which row is selected within the Load category panel — the four
/// [`LoadCaps`] fields, edited directly (a cheap session-field write, applied
/// live; no client rebuild involved).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadRow {
    /// [`LoadCaps::warn_total`].
    WarnTotal,
    /// [`LoadCaps::warn_concurrency`].
    WarnConcurrency,
    /// [`LoadCaps::max_total`].
    MaxTotal,
    /// [`LoadCaps::max_concurrency`].
    MaxConcurrency,
}

impl LoadRow {
    pub(crate) const ALL: [LoadRow; 4] = [
        LoadRow::WarnTotal,
        LoadRow::WarnConcurrency,
        LoadRow::MaxTotal,
        LoadRow::MaxConcurrency,
    ];

    fn next(self) -> Self {
        cycle_next(&Self::ALL, self)
    }

    fn prev(self) -> Self {
        cycle_prev(&Self::ALL, self)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            LoadRow::WarnTotal => "warn total",
            LoadRow::WarnConcurrency => "warn concurrency",
            LoadRow::MaxTotal => "max total",
            LoadRow::MaxConcurrency => "max concurrency",
        }
    }

    /// Reads this field out of `caps`.
    pub(crate) fn get(self, caps: &LoadCaps) -> usize {
        match self {
            LoadRow::WarnTotal => caps.warn_total,
            LoadRow::WarnConcurrency => caps.warn_concurrency,
            LoadRow::MaxTotal => caps.max_total,
            LoadRow::MaxConcurrency => caps.max_concurrency,
        }
    }

    /// Writes this field into `caps` — the app's handler for
    /// [`SettingsOutcome::ApplyLoadCap`].
    pub fn set(self, caps: &mut LoadCaps, value: usize) {
        match self {
            LoadRow::WarnTotal => caps.warn_total = value,
            LoadRow::WarnConcurrency => caps.warn_concurrency = value,
            LoadRow::MaxTotal => caps.max_total = value,
            LoadRow::MaxConcurrency => caps.max_concurrency = value,
        }
    }

    /// The [`SettingKey`] this row's touched-flag lives under.
    pub(crate) fn setting_key(self) -> SettingKey {
        match self {
            LoadRow::WarnTotal => SettingKey::LoadWarnTotal,
            LoadRow::WarnConcurrency => SettingKey::LoadWarnConcurrency,
            LoadRow::MaxTotal => SettingKey::LoadMaxTotal,
            LoadRow::MaxConcurrency => SettingKey::LoadMaxConcurrency,
        }
    }
}

/// Which row is selected within the Appearance category panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppearanceRow {
    /// The colour theme, cycled between the two built-ins (`dark`/`light`).
    Theme,
    /// The leader-key combo string. Working-copy only — see
    /// [`SettingsOutcome::ApplyLeaderKey`].
    LeaderKey,
}

impl AppearanceRow {
    const ALL: [AppearanceRow; 2] = [AppearanceRow::Theme, AppearanceRow::LeaderKey];

    fn next(self) -> Self {
        cycle_next(&Self::ALL, self)
    }

    fn prev(self) -> Self {
        cycle_prev(&Self::ALL, self)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            AppearanceRow::Theme => "Theme",
            AppearanceRow::LeaderKey => "Leader key",
        }
    }
}

/// Which row is selected within the Debug category panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugRow {
    /// The master debug-capture toggle.
    DebugToggle,
    /// The `[advanced]` overrides row (owns the field list below it):
    /// concurrency / total / body-cap / timeout — ported verbatim from the
    /// old Options overlay's Advanced section.
    Advanced,
}

impl DebugRow {
    const ALL: [DebugRow; 2] = [DebugRow::DebugToggle, DebugRow::Advanced];

    fn next(self) -> Self {
        cycle_next(&Self::ALL, self)
    }

    fn prev(self) -> Self {
        cycle_prev(&Self::ALL, self)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            DebugRow::DebugToggle => "Debug capture",
            DebugRow::Advanced => "Advanced (debug)",
        }
    }
}

/// Which advanced-limit knob is focused in the Advanced field list. Ported
/// verbatim from the old Options overlay; also the field identity Request's
/// `Timeout`/`MaxBodyBytes` rows alias (see [`RequestRow`]'s doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvancedField {
    /// A new load run's default concurrency.
    Concurrency,
    /// A new load run's default total copies.
    Total,
    /// The response body-size cap, in bytes.
    BodyCapBytes,
    /// The per-request timeout, in seconds.
    TimeoutSecs,
}

impl AdvancedField {
    pub(crate) const ALL: [AdvancedField; 4] = [
        AdvancedField::Concurrency,
        AdvancedField::Total,
        AdvancedField::BodyCapBytes,
        AdvancedField::TimeoutSecs,
    ];

    fn next(self) -> Self {
        cycle_next(&Self::ALL, self)
    }

    fn prev(self) -> Self {
        cycle_prev(&Self::ALL, self)
    }

    /// This field's display label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            AdvancedField::Concurrency => "concurrency",
            AdvancedField::Total => "total",
            AdvancedField::BodyCapBytes => "body cap",
            AdvancedField::TimeoutSecs => "timeout (s)",
        }
    }

    /// Reads this field out of `limits`.
    pub(crate) fn get(self, limits: &ResolvedAdvancedLimits) -> u64 {
        match self {
            AdvancedField::Concurrency => limits.concurrency as u64,
            AdvancedField::Total => limits.total as u64,
            AdvancedField::BodyCapBytes => limits.body_cap_bytes,
            AdvancedField::TimeoutSecs => limits.timeout_secs,
        }
    }

    /// The [`SettingKey`] this field's touched-flag lives under. `BodyCapBytes`/
    /// `TimeoutSecs` map onto the SAME keys as the Request category's rows
    /// (`MaxBodyBytes`/`Timeout`) — they are the identical live session field
    /// (see [`RequestRow`]'s doc), so editing either surface marks the one key.
    pub(crate) fn setting_key(self) -> SettingKey {
        match self {
            AdvancedField::Concurrency => SettingKey::AdvancedConcurrency,
            AdvancedField::Total => SettingKey::AdvancedTotal,
            AdvancedField::BodyCapBytes => SettingKey::MaxBodyBytes,
            AdvancedField::TimeoutSecs => SettingKey::Timeout,
        }
    }
}

/// Which pane of the open category panel has focus: the top rows, the
/// scrollable cookie list beneath Network's Cookies row, or the Advanced
/// field list beneath Debug's Advanced row. Only meaningful at
/// [`SettingsLevel::Panel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelFocus {
    /// The top control rows of the active category.
    Rows,
    /// The cookie list (delete / clear) — Network category only.
    CookieList,
    /// The Advanced-limits field list — Debug category only.
    AdvancedList,
}

/// What an open text/numeric edit belongs to — pairs with the [`LineEditor`]
/// in [`SettingsState::editing`] so an editor can never exist without knowing
/// which knob it commits to (illegal states unrepresentable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    /// The Network category's proxy URL (free text).
    Proxy,
    /// An advanced-limit field (numeric) — reached from Debug's Advanced list
    /// OR aliased directly from Request's Timeout/MaxBodyBytes rows.
    Advanced(AdvancedField),
    /// A Load category cap (numeric).
    Load(LoadRow),
    /// The Appearance category's leader-key combo (free text).
    LeaderKey,
}

/// What the app should do after the panel handled a key. The app applies it,
/// rebuilds the client / re-resolves the theme where relevant, and refreshes
/// the panel's mirror of the new state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsOutcome {
    /// Fully handled inside the panel; nothing for the app to do.
    Consumed,
    /// Close the panel.
    Close,
    /// Apply a new proxy (`None` clears it → reqwest falls back to the env proxy).
    ApplyProxy(Option<String>),
    /// Flip TLS verification on/off.
    ToggleInsecure,
    /// Flip the cookie jar on/off.
    ToggleCookies,
    /// Delete the named cookie from the jar.
    DeleteCookie {
        /// The cookie's domain.
        domain: String,
        /// The cookie's name.
        name: String,
    },
    /// Clear the whole cookie jar.
    ClearCookies,
    /// Add or edit a cookie by hand (the cookie form's `s` submit).
    UpsertCookie {
        /// The ORIGINAL `(domain, name, path)` of the cookie being edited, if
        /// this is an edit — `None` for a brand-new add. Always `Some` for an
        /// edit regardless of whether the key below actually differs from it;
        /// the handler decides whether an old-key delete is needed.
        previous: Option<(String, String, String)>,
        /// The new/edited cookie's domain.
        domain: String,
        /// The new/edited cookie's name.
        name: String,
        /// The new/edited cookie's value — credential-shaped; never echo it
        /// in a notification (reference the cookie by name+domain only, like
        /// [`SettingsOutcome::DeleteCookie`] already does).
        value: String,
        /// The new/edited cookie's path (already defaulted to `/` if left
        /// blank — see `CookieForm::commit`).
        path: String,
        /// Whether the `Secure` attribute is set.
        secure: bool,
        /// The `SameSite` attribute, or `None` if left unset (attribute
        /// absent, not the RFC `SameSite=None` value).
        same_site: Option<SameSite>,
    },
    /// Apply a validated advanced-limit override. `value` is already
    /// range-checked (positive) by the inline editor; the app additionally
    /// refuses a concurrency/total value above `[load] max_*` through
    /// `load::check_config` before applying (never bypassed).
    ApplyAdvanced {
        /// Which knob to update.
        field: AdvancedField,
        /// The new value (bytes for body-cap, seconds for timeout, a bare
        /// count for concurrency/total).
        value: u64,
    },
    /// Flip session debug capture on/off (mirrors `<leader>D`).
    ToggleDebug,
    /// Apply a new cross-origin redirect policy.
    ApplyRedirect(RedirectPolicy),
    /// Apply a new URL-bar edit mode.
    ApplyUrlEdit(UrlEditMode),
    /// Apply a new save-time secret policy.
    ApplySecretPolicy(SecretPolicy),
    /// Apply a validated concurrent-load guardrail cap.
    ApplyLoadCap {
        /// Which cap to update.
        field: LoadRow,
        /// The new value (a bare count).
        value: usize,
    },
    /// Switch the live theme by name (`"dark"` | `"light"`).
    ApplyTheme(String),
    /// Set the working-copy leader-key combo. A cheap live-reparse seam DOES
    /// exist for the keymap (`KeyMap::set_leader`, M8.5.3), but this outcome
    /// only updates the session's working copy (shown/edited here, persisted
    /// by Save-as-default) — applying it is deferred to the explicit
    /// `<leader>r` reload gate (or the next launch), never applied on this
    /// keystroke, so a leader edit never changes the meaning of a key
    /// mid-combo. The app surfaces that with an inline hint naming the gate.
    ApplyLeaderKey(String),
    /// Persist the current working copy to `config.toml` (M8.5 Wave 3).
    SaveDefaults,
}

/// A snapshot of every session-scoped value the panel manages, taken from the
/// app's live state at open time (and again after every applied change). One
/// bundle rather than a long positional-argument list.
pub struct SettingsSnapshot {
    /// Mirrors `App::execute_options.redirect`.
    pub redirect: RedirectPolicy,
    /// Mirrors `App::url_edit_mode`.
    pub url_edit: UrlEditMode,
    /// Mirrors `App::secret_policy`.
    pub secret_policy: SecretPolicy,
    /// Mirrors `App::session_proxy` (real value; the render masks any userinfo).
    pub proxy: Option<String>,
    /// Mirrors `App::session_insecure`.
    pub insecure: bool,
    /// Mirrors `App::cookies_enabled`.
    pub cookies_enabled: bool,
    /// The current jar contents, refreshed after changes.
    pub cookies: Vec<CookieView>,
    /// Mirrors `App::load_caps`.
    pub load_caps: LoadCaps,
    /// Mirrors `App::theme_name`.
    pub theme_name: String,
    /// Mirrors `App::leader_key` (the working copy — see
    /// [`SettingsOutcome::ApplyLeaderKey`]).
    pub leader_key: String,
    /// Mirrors `App::debug_enabled`.
    pub debug_enabled: bool,
    /// Mirrors `App::advanced_limits`.
    pub advanced: ResolvedAdvancedLimits,
    /// The dirty-indicator baseline: what's currently on disk, freshly
    /// re-read by the app on every open/refresh (best-effort — see
    /// [`SettingsState::persisted`]'s doc).
    pub persisted: churl_core::config::ResolvedSettings,
    /// Mirrors `App`'s touched-set: exactly which [`SettingKey`]s were
    /// edited THROUGH THE PANEL this session — drives both the dirty dot
    /// (touched AND differs from `persisted`) and what a Save actually
    /// writes. See [`SettingKey`]'s doc.
    pub touched: HashSet<SettingKey>,
}

/// Full state of the open Settings panel.
#[derive(Debug, Clone)]
pub struct SettingsState {
    pub level: SettingsLevel,
    /// At [`SettingsLevel::Menu`]: the highlighted category. At
    /// [`SettingsLevel::Panel`]: the open category.
    pub category: SettingsCategory,
    pub focus: PanelFocus,

    // Request category working copy (mostly aliases `advanced`/other fields
    // below — see each row's doc).
    pub redirect: RedirectPolicy,
    pub url_edit: UrlEditMode,
    pub secret_policy: SecretPolicy,
    pub request_row: RequestRow,

    // Network category working copy (ported from the old OptionsState).
    pub proxy: Option<String>,
    pub insecure: bool,
    pub cookies_enabled: bool,
    pub cookies: Vec<CookieView>,
    pub network_row: NetworkRow,
    pub cookie_sel: usize,
    /// The open cookie add/edit form, if any (`a`/`e` from the Cookies row
    /// or its list). `None` when no form is open.
    pub cookie_form: Option<CookieForm>,

    // Load category working copy.
    pub load_caps: LoadCaps,
    pub load_row: LoadRow,

    // Appearance category working copy.
    pub theme_name: String,
    pub leader_key: String,
    pub appearance_row: AppearanceRow,

    // Debug category working copy.
    pub debug_enabled: bool,
    pub advanced: ResolvedAdvancedLimits,
    pub debug_row: DebugRow,
    pub advanced_field: AdvancedField,

    /// The one open text/numeric edit, if any — paired with what it commits to.
    pub editing: Option<(EditTarget, LineEditor)>,
    /// Whether the Appearance category's leader-key row is in "press a key…"
    /// capture mode: the NEXT `KeyEvent` (any key) is normalized and
    /// registered as the new leader-key combo — see
    /// `SettingsState::handle_leader_capture_key` (in `edit.rs`). Mutually exclusive
    /// with `editing` (illegal states unrepresentable: a row is either being
    /// captured or typed into, never both) — entering capture mode never sets
    /// `editing`, and the free-type fallback it can hand off to always clears
    /// this flag first.
    pub capturing_leader_key: bool,
    /// Inline status/error message shown in the footer.
    pub message: Option<String>,
    /// Dirty-indicator baseline (best-effort): what's currently on disk. A row
    /// renders its dirty dot when its [`SettingKey`] is in [`Self::touched`]
    /// AND its working value differs from the matching field here — NOT from
    /// a bare value comparison (see [`SettingKey`]'s doc for why: a
    /// CLI/session-forced value differing from disk must NOT show dirty, or
    /// Save would look safe to fire when it would write nothing for that
    /// knob). Refreshed alongside everything else, so it never drifts stale
    /// mid-session.
    pub persisted: churl_core::config::ResolvedSettings,
    /// Exactly which knobs were edited THROUGH THIS PANEL this session (see
    /// [`SettingKey`]'s doc). Mirrors `App::settings_touched`; cleared there
    /// on a successful Save (this mirror follows on the next refresh), which
    /// is what makes every dirty dot disappear together.
    pub touched: HashSet<SettingKey>,
}

impl SettingsState {
    /// Builds the panel state from the app's current session settings, at the
    /// category menu.
    pub fn new(snapshot: SettingsSnapshot) -> Self {
        Self {
            level: SettingsLevel::Menu,
            category: SettingsCategory::Request,
            focus: PanelFocus::Rows,
            redirect: snapshot.redirect,
            url_edit: snapshot.url_edit,
            secret_policy: snapshot.secret_policy,
            request_row: RequestRow::Timeout,
            proxy: snapshot.proxy,
            insecure: snapshot.insecure,
            cookies_enabled: snapshot.cookies_enabled,
            cookies: snapshot.cookies,
            network_row: NetworkRow::Proxy,
            cookie_sel: 0,
            cookie_form: None,
            load_caps: snapshot.load_caps,
            load_row: LoadRow::WarnTotal,
            theme_name: snapshot.theme_name,
            leader_key: snapshot.leader_key,
            appearance_row: AppearanceRow::Theme,
            debug_enabled: snapshot.debug_enabled,
            advanced: snapshot.advanced,
            debug_row: DebugRow::DebugToggle,
            advanced_field: AdvancedField::Concurrency,
            editing: None,
            capturing_leader_key: false,
            message: None,
            persisted: snapshot.persisted,
            touched: snapshot.touched,
        }
    }

    /// Refreshes the panel's mirror of the session settings after the app
    /// applied a change, keeping the cookie-list selection in range and
    /// backing out of the Debug category if debug just went off mid-session.
    pub fn refresh(&mut self, snapshot: SettingsSnapshot) {
        self.redirect = snapshot.redirect;
        self.url_edit = snapshot.url_edit;
        self.secret_policy = snapshot.secret_policy;
        self.proxy = snapshot.proxy;
        self.insecure = snapshot.insecure;
        self.cookies_enabled = snapshot.cookies_enabled;
        self.cookies = snapshot.cookies;
        self.load_caps = snapshot.load_caps;
        self.theme_name = snapshot.theme_name;
        self.leader_key = snapshot.leader_key;
        self.debug_enabled = snapshot.debug_enabled;
        self.advanced = snapshot.advanced;
        self.persisted = snapshot.persisted;
        self.touched = snapshot.touched;
        // Debug going off mid-session (`<leader>D`) must not strand the panel
        // inside the category/list that just became unreachable.
        if !self.debug_enabled && self.category == SettingsCategory::Debug {
            self.level = SettingsLevel::Menu;
            self.category = SettingsCategory::Request;
            self.focus = PanelFocus::Rows;
            self.editing = None;
        }
        self.clamp_cookie_sel();
    }

    /// The `(domain, name)` of the selected cookie, or `None` when the list is
    /// empty or the Cookies pane is not focused.
    fn selected_cookie(&self) -> Option<(String, String)> {
        self.cookies
            .get(self.cookie_sel)
            .map(|c| (c.domain.clone(), c.name.clone()))
    }

    fn clamp_cookie_sel(&mut self) {
        if self.cookie_sel >= self.cookies.len() {
            self.cookie_sel = self.cookies.len().saturating_sub(1);
        }
        if self.cookies.is_empty() && self.focus == PanelFocus::CookieList {
            self.focus = PanelFocus::Rows;
        }
    }
}

/// Cycles `current` to the next element of `all` (wrapping), by identity.
fn cycle_next<T: Copy + PartialEq>(all: &[T], current: T) -> T {
    let idx = all.iter().position(|v| *v == current).unwrap_or(0);
    all[(idx + 1) % all.len()]
}

/// Cycles `current` to the previous element of `all` (wrapping), by identity.
fn cycle_prev<T: Copy + PartialEq>(all: &[T], current: T) -> T {
    let idx = all.iter().position(|v| *v == current).unwrap_or(0);
    all[(idx + all.len() - 1) % all.len()]
}

/// Masks any userinfo (`user:pass@`) in a proxy URL for display — a proxy may
/// carry credentials at runtime, but they must never be shown on screen. Thin
/// re-export of [`churl_core::config::mask_proxy`] so the whole app masks
/// identically. `"(none — env proxy)"` is the caller's job for a `None` proxy.
pub(crate) fn mask_proxy(proxy: &str) -> String {
    churl_core::config::mask_proxy(proxy)
}

/// Masks ONLY the password segment of a proxy for the inline edit line, keeping
/// the scheme/user/host visible so the field stays editable while the password
/// never renders in plaintext — including **while it is being typed**, before the
/// closing `@` is entered.
///
/// The password lives in the *userinfo*: everything before the FIRST `@`. Anything
/// after the `@` is `host[:port]`, where a `:` introduces a port, never a password,
/// so it is left untouched. When no `@` is present yet the user is mid-type, so the
/// whole remainder is treated as userinfo and the run after its first `:` is masked
/// as it grows — a half-typed `user:pass` is indistinguishable from a `host:port`
/// until the `@` (or end of input) resolves it, and erring toward masking is the
/// safe direction for a secret.
pub(crate) fn mask_proxy_password(proxy: &str) -> String {
    let (scheme, rest) = match proxy.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), proxy),
    };
    let (userinfo, tail) = match rest.split_once('@') {
        Some((user, host)) => (user, Some(host)),
        None => (rest, None),
    };
    let masked_userinfo = match userinfo.split_once(':') {
        Some((user, _pass)) => format!("{user}:••••"),
        None => userinfo.to_owned(),
    };
    match tail {
        Some(tail) => format!("{scheme}{masked_userinfo}@{tail}"),
        None => format!("{scheme}{masked_userinfo}"),
    }
}

/// Formats a raw byte count as a compact, human display: the largest clean
/// unit the value divides evenly into (`"10 MB"`, `"512 KB"`), falling back
/// to a bare byte count when it isn't a whole multiple of either. Display
/// only — the wire format (`max_body_bytes` in `churl-core::config`) stays
/// bytes; this never touches it.
pub(crate) fn format_body_cap(bytes: u64) -> String {
    if bytes != 0 && bytes.is_multiple_of(MB) {
        format!("{} MB", bytes / MB)
    } else if bytes != 0 && bytes.is_multiple_of(KB) {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} bytes")
    }
}

/// Parses a max-body-bytes entry into a raw byte count: `"10MB"`, `"512kb"`,
/// `"1048576"`, or `"1 bytes"` (a bare number, with or without a trailing
/// `bytes`/`byte`, is bytes — back-compat with the old raw-bytes input, and
/// the `bytes` suffix specifically so [`format_body_cap`]'s own fallback
/// output always round-trips through this parser). The unit is
/// case-insensitive and an optional space may separate the number from it.
/// `None` on anything that isn't `<number>[unit]` (empty, non-numeric, or an
/// overflow past `u64::MAX`) — the caller's job is to turn that into an
/// inline error, same stance as every other numeric edit in this panel.
pub(crate) fn parse_body_cap(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let (number, multiplier) = if let Some(n) = lower.strip_suffix("mb") {
        (n, MB)
    } else if let Some(n) = lower.strip_suffix("kb") {
        (n, KB)
    } else if let Some(n) = lower.strip_suffix("bytes") {
        (n, 1)
    } else if let Some(n) = lower.strip_suffix("byte") {
        (n, 1)
    } else {
        (lower.as_str(), 1)
    };
    number.trim().parse::<u64>().ok()?.checked_mul(multiplier)
}

/// Whether two leader-key combo strings denote the SAME key, compared
/// canonically rather than by raw string equality. The panel produces the
/// leader-key value through three different surfaces — the built-in default
/// (`"space"`, lowercase), the free-type editor (verbatim user text), and the
/// capture path (crokey's `Display` form, e.g. `"Space"`, `"Ctrl-b"`) — which
/// can spell the identical combination with different casing/aliases. Both
/// sides are parsed through the SAME `crokey` parser the real keymap uses and
/// compared as `KeyCombination`s, so a captured `"Space"` is seen as equal to
/// the default `"space"` (no spurious dirty dot, no net-zero re-write). A side
/// that doesn't parse falls back to a trimmed case-insensitive string compare
/// — the values still can't be judged unequal on a mere casing difference.
pub(crate) fn leader_key_eq(a: &str, b: &str) -> bool {
    use crokey::KeyCombination;
    use std::str::FromStr;
    match (KeyCombination::from_str(a), KeyCombination::from_str(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a.trim().eq_ignore_ascii_case(b.trim()),
    }
}
