//! The persistent cookie jar behind churl's opt-in cookie support.
//!
//! [`ChurlCookieJar`] is a thin wrapper over `cookie_store::CookieStore` that
//! implements reqwest's two-method [`reqwest::cookie::CookieStore`] trait — it
//! mirrors reqwest's own `Jar` (`RwLock<cookie_store::CookieStore>`), but over an
//! **owned** store so churl can serialize it to JSON and persist it in
//! `state.sqlite` (reqwest's `Jar` hides its store, and the `reqwest_cookie_store`
//! bridge crate pins its own `cookie_store` version — owning the store here avoids
//! that version-compat coupling).
//!
//! Cross-origin safety is a property of the store, not extra code: the store does
//! RFC 6265 domain/path/`Secure` matching in `cookies()`, so a cookie set by host
//! A is never returned for host B. This layers *under* the manual cross-origin
//! header strip in [`crate::http`] (which drops a user-set `Cookie` header on a
//! foreign hop) — the two are independent belts, not a single point of failure.

use std::io::BufReader;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use cookie_store::{CookieStore, RawCookie};
use reqwest::header::HeaderValue;

/// One cookie as surfaced to the UI / `churl cookies list`. Values are shown
/// masked in the TUI (a cookie value is credential-shaped); this carries the
/// plaintext so the caller decides how to render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieView {
    /// The domain the cookie is scoped to (host-only or `Domain=` suffix).
    pub domain: String,
    /// The cookie name.
    pub name: String,
    /// The cookie value (plaintext — mask before display).
    pub value: String,
}

/// A persistent, serializable cookie jar shared (as `Arc`) with the reqwest
/// client. Interior-mutable via `RwLock` so the `&self` [`reqwest::cookie::CookieStore`]
/// trait methods can mutate the store, exactly like reqwest's `Jar`.
#[derive(Debug, Default)]
pub struct ChurlCookieJar(RwLock<CookieStore>);

impl ChurlCookieJar {
    /// An empty jar.
    pub fn new() -> Self {
        Self(RwLock::new(CookieStore::default()))
    }

    /// A read guard on the store that **recovers from a poisoned lock** instead of
    /// panicking. A panic elsewhere while holding the write guard poisons the lock;
    /// without recovery every later cookie op would then panic in turn, wedging the
    /// app. The store's data is still structurally valid after such a panic (at
    /// worst a single in-flight mutation was interrupted), so continuing is the safe
    /// choice — a cookie jar must never be a crash amplifier.
    fn store_read(&self) -> RwLockReadGuard<'_, CookieStore> {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A write guard on the store with the same poison recovery as [`Self::store_read`].
    fn store_write(&self) -> RwLockWriteGuard<'_, CookieStore> {
        self.0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Builds a jar from a JSON blob previously produced by [`Self::to_json`]. A
    /// blank/empty blob yields an empty jar (the common "no cookies stored yet"
    /// case) rather than an error.
    pub fn load_json(json: &str) -> Result<Self, CookieError> {
        if json.trim().is_empty() {
            return Ok(Self::new());
        }
        let store = cookie_store::serde::json::load(BufReader::new(json.as_bytes()))
            .map_err(|err| CookieError(err.to_string()))?;
        Ok(Self(RwLock::new(store)))
    }

    /// Serializes the **persistent, unexpired** cookies to JSON for storage.
    /// Session cookies (no `Max-Age`/`Expires`) are deliberately excluded — they
    /// live only in RAM and evaporate on exit, matching browser behaviour.
    pub fn to_json(&self) -> Result<String, CookieError> {
        let mut buf = Vec::new();
        // `save_json` only writes persistent + unexpired cookies. A serialize
        // failure is returned as an error — NOT swallowed as an empty string:
        // persisting `""` over a good on-disk blob would silently wipe the jar.
        // The caller MUST skip the write on error (see `App::persist_cookie_jar`).
        let store = self.store_read();
        cookie_store::serde::json::save(&store, &mut buf)
            .map_err(|err| CookieError(err.to_string()))?;
        String::from_utf8(buf).map_err(|err| CookieError(err.to_string()))
    }

    /// Every stored (unexpired) cookie, for the Options overlay + `churl cookies
    /// list`. Ordered by domain then name for a stable display.
    pub fn list(&self) -> Vec<CookieView> {
        let store = self.store_read();
        let mut views: Vec<CookieView> = store
            .iter_unexpired()
            .map(|cookie| CookieView {
                domain: String::from(&cookie.domain),
                name: cookie.name().to_owned(),
                value: cookie.value().to_owned(),
            })
            .collect();
        views.sort_by(|a, b| a.domain.cmp(&b.domain).then_with(|| a.name.cmp(&b.name)));
        views
    }

    /// Removes every cookie matching `(domain, name)` (a domain can hold the same
    /// name under several paths — all are removed). Returns whether anything was
    /// removed. The store keys on `(domain, path, name)`, so the coordinates are
    /// collected first (ending the read borrow) before removing.
    pub fn delete(&self, domain: &str, name: &str) -> bool {
        let coords: Vec<(String, String, String)> = {
            let store = self.store_read();
            store
                .iter_any()
                .filter(|c| String::from(&c.domain) == domain && c.name() == name)
                .map(|c| {
                    (
                        String::from(&c.domain),
                        String::from(&c.path),
                        c.name().to_owned(),
                    )
                })
                .collect()
        };
        if coords.is_empty() {
            return false;
        }
        let mut store = self.store_write();
        for (d, p, n) in &coords {
            store.remove(d, p, n);
        }
        true
    }

    /// Empties the jar.
    pub fn clear(&self) {
        self.store_write().clear();
    }
}

impl reqwest::cookie::CookieStore for ChurlCookieJar {
    // Mirrors reqwest's own `Jar`: parse each `Set-Cookie` value into an owned
    // raw cookie and hand the batch to the store, which applies RFC 6265
    // domain/path/Secure scoping against `url`.
    fn set_cookies(
        &self,
        cookie_headers: &mut dyn Iterator<Item = &HeaderValue>,
        url: &reqwest::Url,
    ) {
        let iter = cookie_headers.filter_map(|value| {
            let s = std::str::from_utf8(value.as_bytes()).ok()?;
            RawCookie::parse(s).ok().map(|c| c.into_owned())
        });
        self.store_write().store_response_cookies(iter, url);
    }

    // Returns the `Cookie` header value for `url` — only cookies whose
    // domain/path/Secure scope matches are included, so a cookie set for host A is
    // never emitted for host B.
    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue> {
        let store = self.store_read();
        let header = store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        if header.is_empty() {
            return None;
        }
        HeaderValue::from_str(&header).ok()
    }
}

/// Error loading a cookie jar from a stored JSON blob.
#[derive(Debug, thiserror::Error)]
#[error("failed to load cookie jar: {0}")]
pub struct CookieError(String);

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore as _;

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    fn set_cookie(jar: &ChurlCookieJar, header: &str, at: &str) {
        let value = HeaderValue::from_str(header).unwrap();
        let mut iter = std::iter::once(&value);
        jar.set_cookies(&mut iter, &url(at));
    }

    #[test]
    fn set_then_get_same_origin() {
        let jar = ChurlCookieJar::new();
        set_cookie(&jar, "sid=abc123", "https://a.example/");
        let header = jar.cookies(&url("https://a.example/dashboard")).unwrap();
        assert_eq!(header.to_str().unwrap(), "sid=abc123");
    }

    #[test]
    fn cross_origin_cookie_does_not_leak() {
        // The security corner: a cookie set by host A must never be sent to host B.
        let jar = ChurlCookieJar::new();
        set_cookie(&jar, "sid=secret", "https://a.example/");
        assert!(
            jar.cookies(&url("https://b.example/")).is_none(),
            "host A's cookie must not be offered to host B"
        );
        // And it IS still offered back to host A.
        assert!(jar.cookies(&url("https://a.example/")).is_some());
    }

    #[test]
    fn persistent_cookie_json_round_trip() {
        let jar = ChurlCookieJar::new();
        // Max-Age makes this cookie persistent, so it survives serialization.
        set_cookie(&jar, "token=xyz; Max-Age=3600", "https://a.example/");
        let json = jar.to_json().unwrap();
        assert!(!json.is_empty(), "a persistent cookie must serialize");

        let restored = ChurlCookieJar::load_json(&json).unwrap();
        let header = restored.cookies(&url("https://a.example/")).unwrap();
        assert_eq!(header.to_str().unwrap(), "token=xyz");
    }

    #[test]
    fn session_cookie_excluded_from_json() {
        let jar = ChurlCookieJar::new();
        // No Max-Age/Expires → a session cookie, RAM-only, never persisted.
        set_cookie(&jar, "sess=temp", "https://a.example/");
        let json = jar.to_json().unwrap();
        let restored = ChurlCookieJar::load_json(&json).unwrap();
        assert!(
            restored.cookies(&url("https://a.example/")).is_none(),
            "session cookies must not survive serialization"
        );
    }

    #[test]
    fn list_and_delete_and_clear() {
        let jar = ChurlCookieJar::new();
        set_cookie(&jar, "a=1; Max-Age=3600", "https://x.example/");
        set_cookie(&jar, "b=2; Max-Age=3600", "https://x.example/");
        let listed = jar.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "a");
        assert_eq!(listed[0].domain, "x.example");

        assert!(jar.delete("x.example", "a"));
        assert!(!jar.delete("x.example", "a"), "second delete finds nothing");
        assert_eq!(jar.list().len(), 1);

        jar.clear();
        assert!(jar.list().is_empty());
    }

    #[test]
    fn poisoned_lock_recovers_without_panicking() {
        use std::sync::Arc;

        let jar = Arc::new(ChurlCookieJar::new());
        set_cookie(&jar, "a=1; Max-Age=3600", "https://x.example/");

        // Poison the lock: a thread panics while holding the write guard.
        let poisoner = Arc::clone(&jar);
        let joined = std::thread::spawn(move || {
            let _guard = poisoner.0.write().unwrap();
            panic!("boom while holding the cookie write guard");
        })
        .join();
        assert!(joined.is_err(), "the poisoning thread must have panicked");
        assert!(jar.0.is_poisoned(), "the lock must now be poisoned");

        // Every op must now recover instead of panicking, and see sane data.
        let listed = jar.list();
        assert_eq!(
            listed.len(),
            1,
            "read recovers and sees the pre-poison cookie"
        );
        assert_eq!(listed[0].name, "a");

        // A write recovers too.
        assert!(jar.delete("x.example", "a"), "write recovers and mutates");
        assert!(jar.list().is_empty());

        // The reqwest-trait hot-path methods recover as well.
        set_cookie(&jar, "b=2; Max-Age=3600", "https://y.example/");
        assert!(
            jar.cookies(&url("https://y.example/")).is_some(),
            "set_cookies + cookies recover after poisoning"
        );

        // Serialization (the persistence path) also recovers.
        assert!(jar.to_json().is_ok());
    }

    #[test]
    fn load_json_blank_is_empty_jar() {
        assert!(ChurlCookieJar::load_json("").unwrap().list().is_empty());
        assert!(ChurlCookieJar::load_json("   ").unwrap().list().is_empty());
    }

    #[test]
    fn to_json_is_fallible_and_empty_jar_serializes_ok() {
        // `to_json` returns a Result (a serialize failure is surfaced, never
        // silently returned as "" — which, persisted over a good blob, would wipe
        // the jar). An empty jar serializes cleanly and round-trips to empty.
        let jar = ChurlCookieJar::new();
        let json = jar.to_json().expect("empty jar must serialize");
        assert!(
            ChurlCookieJar::load_json(&json).unwrap().list().is_empty(),
            "empty jar round-trips to empty"
        );
    }
}
