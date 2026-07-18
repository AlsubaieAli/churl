//! The Options overlay: a small modal for the three session-scoped request
//! controls — proxy (an editable string), TLS verification (on/off), and the
//! persistent cookie jar (on/off plus a viewable, per-row-deletable cookie list).
//!
//! All UI state lives here (the `churl` crate); `churl-core` stays TUI-free. The
//! overlay owns only *view* state and emits an [`OptionsOutcome`] describing what
//! the app should do — the app owns the session settings, the client rebuild, and
//! the cookie jar, so the overlay never mutates them directly (it can't resolve a
//! proxy, rebuild a client, or reach the jar). Every applied change rebuilds the
//! single client through the app's install-runtime seam.

use churl_core::config::ResolvedAdvancedLimits;
use churl_core::cookies::CookieView;

use super::line_editor::LineEditor;

mod edit;
mod render;
#[cfg(test)]
mod tests;

pub use render::render;

/// Which of the top rows is selected. [`OptionsRow::Advanced`] is reachable
/// only when [`OptionsState::debug_enabled`] — the row-navigation keys in
/// `edit.rs` never select it otherwise, so it stays functionally absent (not
/// just visually hidden) outside a debug session, matching the pre-M8.3
/// Proxy/TLS/Cookies-only behaviour exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionsRow {
    /// The editable proxy URL row.
    Proxy,
    /// The TLS-verification on/off row.
    Tls,
    /// The cookie-jar on/off row (owns the cookie list below it).
    Cookies,
    /// The debug-gated Advanced-limits row (owns the field list below it):
    /// concurrency / total / body-cap / timeout overrides (M8.3 Wave 4).
    Advanced,
}

/// Which advanced-limit knob is focused in the Advanced field list.
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
        let idx = Self::ALL.iter().position(|&f| f == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|&f| f == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// This field's display label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            AdvancedField::Concurrency => "concurrency",
            AdvancedField::Total => "total",
            AdvancedField::BodyCapBytes => "body cap (bytes)",
            AdvancedField::TimeoutSecs => "timeout (s)",
        }
    }
}

/// Which pane of the overlay has focus: the top control rows, the scrollable
/// cookie list beneath the Cookies row, or the Advanced field list beneath
/// the Advanced row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionsFocus {
    /// The top control rows (Proxy / TLS / Cookies / Advanced).
    Rows,
    /// The cookie list (delete / clear).
    CookieList,
    /// The Advanced-limits field list (edit one of the four overrides).
    AdvancedList,
}

/// What the app should do after the overlay handled a key. The app applies it,
/// rebuilds the client, and refreshes the overlay's mirror of the new state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionsOutcome {
    /// Fully handled inside the overlay; nothing for the app to do.
    Consumed,
    /// Close the overlay.
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
}

/// Full state of the open Options overlay. A mirror of the app's session
/// settings at open, refreshed after each applied change.
#[derive(Debug, Clone)]
pub struct OptionsState {
    /// The active proxy URL (real value; the render masks any userinfo). `None`
    /// means no explicit proxy (env-proxy fallback).
    pub proxy: Option<String>,
    /// Whether TLS verification is OFF.
    pub insecure: bool,
    /// Whether the cookie jar is active.
    pub cookies_enabled: bool,
    /// The current jar contents (domain · name · value), refreshed after changes.
    pub cookies: Vec<CookieView>,
    /// Which pane has focus.
    pub focus: OptionsFocus,
    /// The selected top row.
    pub row: OptionsRow,
    /// Selected index within the cookie list.
    pub cookie_sel: usize,
    /// In-progress proxy edit (the real value, not the masked display), if any.
    pub editing: Option<LineEditor>,
    /// Inline status/error message shown in the footer.
    pub message: Option<String>,
    /// Whether debug capture is on this session — gates whether the
    /// Advanced row/field-list is reachable at all (M8.3 Wave 4).
    pub debug_enabled: bool,
    /// The current advanced-limit overrides mirror, refreshed after changes.
    pub advanced: ResolvedAdvancedLimits,
    /// Selected field within the Advanced field list.
    pub advanced_field: AdvancedField,
    /// In-progress advanced-field numeric edit, if any.
    pub advanced_editing: Option<LineEditor>,
}

impl OptionsState {
    /// Builds the overlay state from the app's current session settings.
    pub fn new(
        proxy: Option<String>,
        insecure: bool,
        cookies_enabled: bool,
        cookies: Vec<CookieView>,
        debug_enabled: bool,
        advanced: ResolvedAdvancedLimits,
    ) -> Self {
        Self {
            proxy,
            insecure,
            cookies_enabled,
            cookies,
            focus: OptionsFocus::Rows,
            row: OptionsRow::Proxy,
            cookie_sel: 0,
            editing: None,
            message: None,
            debug_enabled,
            advanced,
            advanced_field: AdvancedField::Concurrency,
            advanced_editing: None,
        }
    }

    /// Refreshes the overlay's mirror of the session settings after the app
    /// applied a change, keeping the cookie-list selection in range.
    #[allow(clippy::too_many_arguments)]
    pub fn refresh(
        &mut self,
        proxy: Option<String>,
        insecure: bool,
        cookies_enabled: bool,
        cookies: Vec<CookieView>,
        debug_enabled: bool,
        advanced: ResolvedAdvancedLimits,
    ) {
        self.proxy = proxy;
        self.insecure = insecure;
        self.cookies_enabled = cookies_enabled;
        self.cookies = cookies;
        self.debug_enabled = debug_enabled;
        self.advanced = advanced;
        // Debug going off mid-session (`<leader>D`) must not strand the
        // overlay on/inside a row that just became unreachable.
        if !self.debug_enabled && self.row == OptionsRow::Advanced {
            self.row = OptionsRow::Proxy;
            self.focus = OptionsFocus::Rows;
            self.advanced_editing = None;
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
        if self.cookies.is_empty() {
            self.focus = OptionsFocus::Rows;
        }
    }
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
