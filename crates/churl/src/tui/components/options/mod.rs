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

use churl_core::cookies::CookieView;

use super::line_editor::LineEditor;

mod edit;
mod render;
#[cfg(test)]
mod tests;

pub use render::render;

/// Which of the three top rows is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionsRow {
    /// The editable proxy URL row.
    Proxy,
    /// The TLS-verification on/off row.
    Tls,
    /// The cookie-jar on/off row (owns the cookie list below it).
    Cookies,
}

/// Which pane of the overlay has focus: the three-row control list, or the
/// scrollable cookie list beneath the Cookies row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionsFocus {
    /// The top control rows (Proxy / TLS / Cookies).
    Rows,
    /// The cookie list (delete / clear).
    CookieList,
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
}

impl OptionsState {
    /// Builds the overlay state from the app's current session settings.
    pub fn new(
        proxy: Option<String>,
        insecure: bool,
        cookies_enabled: bool,
        cookies: Vec<CookieView>,
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
        }
    }

    /// Refreshes the overlay's mirror of the session settings after the app
    /// applied a change, keeping the cookie-list selection in range.
    pub fn refresh(
        &mut self,
        proxy: Option<String>,
        insecure: bool,
        cookies_enabled: bool,
        cookies: Vec<CookieView>,
    ) {
        self.proxy = proxy;
        self.insecure = insecure;
        self.cookies_enabled = cookies_enabled;
        self.cookies = cookies;
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
/// never renders in plaintext. A proxy still being typed (no `@` yet) is returned
/// as-is — a proper masked-secret input widget for the pre-`@` case is deferred
/// (M8.1); this closes the concrete "editing a stored `user:pass@` proxy renders
/// the password verbatim" leak the review flagged.
pub(crate) fn mask_proxy_password(proxy: &str) -> String {
    let (scheme, rest) = match proxy.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), proxy),
    };
    let Some((authority, tail)) = rest.split_once('@') else {
        return proxy.to_owned();
    };
    match authority.split_once(':') {
        Some((user, _pass)) => format!("{scheme}{user}:••••@{tail}"),
        None => format!("{scheme}{authority}@{tail}"),
    }
}
