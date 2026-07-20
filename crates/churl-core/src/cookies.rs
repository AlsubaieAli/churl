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

use cookie::SameSite as RawSameSite;
use cookie::time::{Duration, OffsetDateTime};
use cookie_store::{CookieStore, RawCookie};
use reqwest::Url;
use reqwest::header::HeaderValue;

/// The `SameSite` cookie attribute, surfaced by churl without leaking the
/// `cookie` crate's own type through the public API (semver hygiene — see the
/// 0.5.0 `#[non_exhaustive]` lesson in `docs/DECISIONS.md`: a dependency's
/// enum showing up in churl's own signatures couples a patch bump in that dep
/// to churl's semver).
///
/// `None` here is the RFC `SameSite=None` attribute value ("send on every
/// cross-site request, Secure required"). An **absent** `SameSite` attribute
/// is `Option::<SameSite>::None` on [`CookieView::same_site`] — a different
/// thing from this variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    /// Never sent on a cross-site request.
    Strict,
    /// Sent on cross-site "safe" (GET/HEAD/OPTIONS/TRACE) requests only.
    Lax,
    /// Sent on every cross-site request (the RFC `SameSite=None` value).
    None,
}

impl SameSite {
    fn from_raw(raw: RawSameSite) -> Self {
        match raw {
            RawSameSite::Strict => SameSite::Strict,
            RawSameSite::Lax => SameSite::Lax,
            RawSameSite::None => SameSite::None,
        }
    }

    fn to_raw(self) -> RawSameSite {
        match self {
            SameSite::Strict => RawSameSite::Strict,
            SameSite::Lax => RawSameSite::Lax,
            SameSite::None => RawSameSite::None,
        }
    }
}

/// One cookie as surfaced to the UI / `churl cookies list`. Values are shown
/// masked in the TUI (a cookie value is credential-shaped); this carries the
/// plaintext so the caller decides how to render it. Carries every field an
/// edit form needs to round-trip a cookie (added M8.5.1 alongside
/// [`ChurlCookieJar::upsert`] — a struct-literal type, not
/// `#[non_exhaustive]`, since the panel's render test still builds it via
/// `CookieView { .. }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieView {
    /// The domain the cookie is scoped to (host-only or `Domain=` suffix).
    pub domain: String,
    /// The cookie name.
    pub name: String,
    /// The cookie value (plaintext — mask before display).
    pub value: String,
    /// The cookie's `Path=` attribute (or the default path if unset).
    pub path: String,
    /// Whether the `Secure` attribute is set.
    pub secure: bool,
    /// The `SameSite` attribute, or `None` if the attribute is absent
    /// entirely (not to be confused with `Some(SameSite::None)`).
    pub same_site: Option<SameSite>,
}

/// A cookie the user typed by hand (the Settings panel's add/edit form),
/// fed to [`ChurlCookieJar::upsert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieSpec {
    /// The domain to scope the cookie to.
    pub domain: String,
    /// The cookie name.
    pub name: String,
    /// The cookie value.
    pub value: String,
    /// The cookie's path. Empty is treated as `/` by [`ChurlCookieJar::upsert`].
    pub path: String,
    /// Whether to set the `Secure` attribute.
    pub secure: bool,
    /// The `SameSite` attribute to set, or `None` to leave it absent.
    pub same_site: Option<SameSite>,
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
            .map(|c| CookieView {
                domain: String::from(&c.domain),
                name: c.name().to_owned(),
                value: c.value().to_owned(),
                path: c.path().map(str::to_owned).unwrap_or_default(),
                secure: c.secure().unwrap_or(false),
                same_site: c.same_site().map(SameSite::from_raw),
            })
            .collect();
        views.sort_by(|a, b| a.domain.cmp(&b.domain).then_with(|| a.name.cmp(&b.name)));
        views
    }

    /// Adds or replaces a cookie the user typed by hand (the Settings panel's
    /// add/edit form). Always stored as **persistent** — a far-future
    /// `Expires` — because a session cookie (no `Max-Age`/`Expires`) would
    /// evaporate before [`Self::to_json`] ever wrote it out, and a
    /// manually-entered cookie (often an auth token) must stick.
    ///
    /// **Host-only scope.** The cookie is stored WITHOUT a `Domain=`
    /// attribute, so the store scopes it host-only (RFC 6265): it is sent
    /// back only to the exact host the user typed, never to a subdomain. This
    /// is the browser default for a hand-typed host and the safe direction —
    /// silently broadening an auth token to every `*.{domain}` subdomain is
    /// the cross-origin over-scoping churl's redirect-strip policy (R3) exists
    /// to prevent. Subdomain-wide (`Domain=`) manual cookies are a deliberate
    /// non-goal for now. [`Self::list`] still shows the host in its `domain`
    /// column.
    ///
    /// Validates `name`/`domain` are non-empty and that `domain`+`path`
    /// resolve to a URL BEFORE touching the store. The cookie is inserted via
    /// `cookie_store::CookieStore::insert_raw` against a synthesized
    /// `https://{domain}{path}` request URL — always `https` so a `Secure`
    /// cookie is never rejected at insert, and the request host IS the
    /// cookie's scope (host-only), so the store accepts it. **The store's
    /// rejection is never swallowed**: any `Err` from `insert_raw`
    /// (unparseable host, mismatch, …) is propagated as `CookieError`, never
    /// reported as a silent success — a cookie jar must never claim to hold
    /// something it doesn't.
    ///
    /// **The store keys cookies on `(domain, path, name)`.** `upsert`
    /// REPLACES a same-key entry (the "edit in place" case). If the caller is
    /// editing an existing cookie and the user changed its domain, name, or
    /// path, the OLD entry is a *different* key and is left untouched by this
    /// call — the caller (the app's `UpsertCookie` handler) is responsible
    /// for removing the old coordinates itself, via [`Self::delete_exact`]
    /// and only AFTER this upsert succeeds.
    pub fn upsert(&self, spec: CookieSpec) -> Result<(), CookieError> {
        if spec.name.is_empty() {
            return Err(CookieError("cookie name cannot be empty".to_owned()));
        }
        if spec.domain.is_empty() {
            return Err(CookieError("cookie domain cannot be empty".to_owned()));
        }
        let path = if spec.path.is_empty() {
            "/"
        } else {
            spec.path.as_str()
        };
        let url = Url::parse(&format!("https://{}{path}", spec.domain))
            .map_err(|err| CookieError(format!("invalid domain/path: {err}")))?;

        let mut raw = RawCookie::new(spec.name, spec.value);
        raw.set_path(path.to_owned());
        raw.set_secure(spec.secure);
        raw.set_same_site(spec.same_site.map(SameSite::to_raw));
        // Deliberately NO `set_domain` — a host-only cookie (see the doc
        // above). Adding `Domain={host}` would make it a `Suffix` cookie the
        // store sends to every subdomain too.
        // RFC 6265 caps dates at year 9999; `set_expires` clamps to that
        // itself, so any comfortably-far-future date works here.
        raw.set_expires(OffsetDateTime::now_utc() + Duration::weeks(520));

        self.store_write()
            .insert_raw(&raw, &url)
            .map(|_action| ())
            .map_err(|err| CookieError(err.to_string()))
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

    /// Removes the ONE cookie at the exact `(domain, path, name)` key,
    /// returning whether anything was removed. Unlike [`Self::delete`] (which
    /// is domain+name-scoped and wipes every path under that pair), this is
    /// path-precise — it surfaces the store's own `(domain, path, name)`
    /// primitive so an edit that moves a cookie to a new path can drop just
    /// the old coordinate without touching a sibling cookie of the same name
    /// at a different path.
    pub fn delete_exact(&self, domain: &str, path: &str, name: &str) -> bool {
        // `remove` returns the removed cookie (or `None` if the exact key
        // wasn't present), so a single write-lock pass answers "did anything
        // go?" without a separate read scan.
        self.store_write().remove(domain, path, name).is_some()
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

    fn spec(domain: &str, name: &str, value: &str) -> CookieSpec {
        CookieSpec {
            domain: domain.to_owned(),
            name: name.to_owned(),
            value: value.to_owned(),
            path: String::new(),
            secure: false,
            same_site: None,
        }
    }

    #[test]
    fn upsert_add_is_visible_in_list_with_all_fields() {
        let jar = ChurlCookieJar::new();
        jar.upsert(CookieSpec {
            domain: "a.example".to_owned(),
            name: "sid".to_owned(),
            value: "abc123".to_owned(),
            path: "/app".to_owned(),
            secure: true,
            same_site: Some(SameSite::Lax),
        })
        .unwrap();

        let listed = jar.list();
        assert_eq!(listed.len(), 1);
        let c = &listed[0];
        assert_eq!(c.domain, "a.example");
        assert_eq!(c.name, "sid");
        assert_eq!(c.value, "abc123");
        assert_eq!(c.path, "/app");
        assert!(c.secure);
        assert_eq!(c.same_site, Some(SameSite::Lax));
    }

    #[test]
    fn upsert_same_key_replaces_not_duplicates() {
        let jar = ChurlCookieJar::new();
        jar.upsert(spec("a.example", "sid", "first")).unwrap();
        jar.upsert(spec("a.example", "sid", "second")).unwrap();

        let listed = jar.list();
        assert_eq!(listed.len(), 1, "upsert must replace, not duplicate");
        assert_eq!(listed[0].value, "second");
    }

    #[test]
    fn upsert_then_to_json_round_trips() {
        let jar = ChurlCookieJar::new();
        jar.upsert(spec("a.example", "sid", "abc123")).unwrap();
        let json = jar.to_json().unwrap();

        let restored = ChurlCookieJar::load_json(&json).unwrap();
        let listed = restored.list();
        assert_eq!(listed.len(), 1, "upsert must persist (survive to_json)");
        assert_eq!(listed[0].value, "abc123");
    }

    #[test]
    fn upsert_rejects_empty_name_or_domain() {
        let jar = ChurlCookieJar::new();
        assert!(jar.upsert(spec("a.example", "", "v")).is_err());
        assert!(jar.upsert(spec("", "sid", "v")).is_err());
        assert!(
            jar.list().is_empty(),
            "a rejected upsert must not partially insert"
        );
    }

    #[test]
    fn upsert_surfaces_store_rejection_never_silently_swallows_it() {
        let jar = ChurlCookieJar::new();
        // A domain that cannot even parse into the synthesized request URL —
        // `upsert` must propagate the store/URL rejection as `Err`, never
        // report success while leaving nothing actually stored.
        let result = jar.upsert(spec("[not-a-valid-host", "sid", "v"));
        assert!(
            result.is_err(),
            "an unparseable domain must be a surfaced error, not a silent no-op"
        );
        assert!(jar.list().is_empty());
    }

    #[test]
    fn list_surfaces_same_site_secure_and_path_from_set_cookies() {
        let jar = ChurlCookieJar::new();
        set_cookie(
            &jar,
            "sid=abc; Max-Age=3600; Path=/app; Secure; SameSite=Strict",
            "https://a.example/",
        );
        let listed = jar.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, "/app");
        assert!(listed[0].secure);
        assert_eq!(listed[0].same_site, Some(SameSite::Strict));
    }

    #[test]
    fn upsert_is_host_only_not_sent_to_subdomain() {
        // A hand-typed cookie must be host-only: sent back to the exact host,
        // never silently broadened to every subdomain (the cross-origin
        // over-scoping churl's R3 redirect policy guards against).
        let jar = ChurlCookieJar::new();
        jar.upsert(spec("a.example", "sid", "abc123")).unwrap();

        assert!(
            jar.cookies(&url("https://a.example/")).is_some(),
            "a host-only cookie must still be offered back to its own host"
        );
        assert!(
            jar.cookies(&url("https://sub.a.example/")).is_none(),
            "a hand-typed cookie must NOT be broadened to a subdomain"
        );
        // And the list still shows the host in the domain column.
        assert_eq!(jar.list()[0].domain, "a.example");
    }

    #[test]
    fn delete_exact_removes_only_the_named_path() {
        // The path-precise primitive: a sibling cookie of the same
        // (domain, name) at a DIFFERENT path must survive.
        let jar = ChurlCookieJar::new();
        jar.upsert(CookieSpec {
            path: "/app".to_owned(),
            ..spec("a.example", "sid", "app-val")
        })
        .unwrap();
        jar.upsert(CookieSpec {
            path: "/admin".to_owned(),
            ..spec("a.example", "sid", "admin-val")
        })
        .unwrap();
        assert_eq!(jar.list().len(), 2, "two cookies, same name, two paths");

        assert!(
            jar.delete_exact("a.example", "/app", "sid"),
            "the exact /app coordinate is removed"
        );
        assert!(
            !jar.delete_exact("a.example", "/app", "sid"),
            "a second delete_exact of the same coord finds nothing"
        );

        let listed = jar.list();
        assert_eq!(listed.len(), 1, "the /admin sibling must survive");
        assert_eq!(listed[0].path, "/admin");
        assert_eq!(listed[0].value, "admin-val");
    }
}
