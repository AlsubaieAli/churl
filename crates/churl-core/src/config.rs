//! Global churl configuration and workspace secrets enforcement.
//!
//! The global config lives at `<config_dir>/churl/config.toml` (e.g.
//! `~/.config/churl/config.toml` on Linux). Workspace files (`churl.toml`, endpoint
//! files) must never contain secrets — see [`secret_violations`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::model::Workspace;

/// Error loading a churl [`Config`] file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The config file exists but could not be read.
    #[error("failed to read config {path}: {source}")]
    Read {
        /// Path of the file that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The config file is not valid TOML for [`Config`].
    #[error("failed to parse config {path}: {source}")]
    Parse {
        /// Path of the file that failed to parse.
        path: PathBuf,
        /// Underlying TOML deserialization error.
        source: toml_edit::de::Error,
    },
    /// A config value is outside its allowed set (e.g. `url_edit = "nope"`).
    #[error("bad config value for {key}: {value:?} (expected {expected})")]
    BadValue {
        /// The offending config key.
        key: String,
        /// The value the user supplied.
        value: String,
        /// A human-readable description of the allowed values.
        expected: &'static str,
    },
    /// The `[keys]` structure is malformed: a sub-table nested where only
    /// `combo = "action"` scalars are allowed. Only `[keys.leader]` may carry
    /// further sub-tables (its submenus).
    #[error("bad [keys] structure: {0}")]
    BadKeys(String),
}

/// Global churl configuration. Every field is optional; a missing file means defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Config {
    /// Name of the colour theme to use; `None` means the built-in default
    /// (`"dark"` | `"light"`). The TUI layer parses it and rejects unknown names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Per-slot colour overrides under a `[theme_colors]` table: slot name →
    /// colour (named ANSI or `#rrggbb` hex). Core carries only strings; the TUI
    /// layer parses them over the selected built-in and fails loudly on an
    /// unknown slot or bad colour (see the TUI `Theme`).
    ///
    /// The table is `[theme_colors]` rather than `[theme.colors]`: `theme` is a
    /// scalar key (`theme = "dark"`), so a `[theme.colors]` sub-table would
    /// collide with it in TOML.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub theme_colors: BTreeMap<String, String>,
    /// Keybinding overrides under a flat `[keys]` table: key-combination string →
    /// action name (e.g. `"ctrl-p" = "open-palette"`). Core carries only strings;
    /// the TUI layer parses combinations and action names and rejects unknown
    /// entries loudly at startup.
    ///
    /// Nested sub-tables under `[keys]` (e.g. `[keys.request]`) are split out into
    /// [`Config::key_overlays`] by [`Config::split_key_overlays`] after load so
    /// this stays a flat string→string map. The raw `[keys]` table (which may mix
    /// flat scalars and sub-tables) deserializes into [`Config::raw_keys`] first.
    #[serde(skip)]
    pub keys: BTreeMap<String, String>,
    /// Per-pane keymap overlay tables: `[keys.explorer]`, `[keys.urlbar]`,
    /// `[keys.request]`, `[keys.response]` — each a `combo → action` map. Split
    /// from the raw `[keys]` table after load; the TUI layer layers these over the
    /// pane-context overlays and fails loudly on an unknown table/combo/action.
    #[serde(skip)]
    pub key_overlays: BTreeMap<String, BTreeMap<String, String>>,
    /// The raw `[keys]` table as parsed from TOML: values are either a flat action
    /// string or a nested overlay table. [`Config::split_key_overlays`] partitions
    /// it into [`Config::keys`] + [`Config::key_overlays`].
    #[serde(default, rename = "keys", skip_serializing)]
    pub raw_keys: BTreeMap<String, KeyEntry>,
    /// Response body-size cap in bytes; `None` means the 10 MB default
    /// ([`crate::http::DEFAULT_MAX_BODY_BYTES`]). Resolved via [`Config::max_body_bytes`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<u64>,
    /// Per-request timeout in seconds; `None` means the 30 s default
    /// ([`crate::http::DEFAULT_TIMEOUT`]). Resolved via [`Config::timeout`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// The leader key combination string (e.g. `"space"`, `"ctrl-b"`); `None`
    /// means the built-in default (Space). The TUI layer parses it and fails
    /// loudly on a bad combination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_key: Option<String>,
    /// What the URL bar's `i`/`Enter` opens: `"inline"` (default) or `"popup"`.
    /// `e` always opens the popup regardless. The TUI layer parses it via
    /// [`Config::url_edit`] and fails loudly on an unknown value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url_edit: Option<String>,
    /// Concurrent-load guardrail caps under a `[load]` table; absent →
    /// [`crate::load::LoadCaps::default`]. A malformed value (e.g. a non-integer
    /// `warn_total`) fails the whole config parse loudly, like every other knob.
    /// Resolved via [`Config::load_caps`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<LoadSection>,
    /// Save-time secret policy: `"strict"` (default) blocks a newly-authored
    /// name-anchored literal secret and warns on the rest; `"warn"` warns on
    /// everything and blocks nothing. Resolved via [`Config::secret_policy`],
    /// which fails loudly on an unknown value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_policy: Option<String>,
    /// Cross-origin redirect policy: `"strip"` (default) follows redirects but
    /// drops auth-bearing headers when a hop crosses the origin (scheme + host +
    /// port); `"strict"` follows same-origin hops only and surfaces a
    /// cross-origin 3xx instead of following it; `"follow-all"` follows every
    /// hop keeping all headers (a foot-gun that warns once). Resolved via
    /// [`Config::redirect`], which fails loudly on an unknown value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<String>,
    /// Global HTTP/HTTPS proxy URL applied to every request; `None` disables the
    /// explicit proxy so reqwest honors the `HTTP(S)_PROXY`/`NO_PROXY`
    /// environment instead. Lowest of the explicit-precedence sources (CLI
    /// `--proxy` > per-workspace `churl.toml` > this > env). A persisted proxy
    /// must be credential-free ([`proxy_has_credentials`]). Resolved via
    /// [`Config::proxy`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Whether to disable TLS certificate verification by default (accept
    /// invalid/self-signed certs *and* hostname mismatches — the rustls verifier
    /// ignores both). `false` (verify) unless set. A session-scoped launch
    /// default only; there is no per-endpoint/per-workspace TLS downgrade.
    /// Resolved via [`Config::insecure`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
    /// Whether to enable the persistent per-workspace cookie jar by default.
    /// `false` (off) unless set; a per-workspace `churl.toml cookies = true`
    /// overrides it upward. Resolved via [`Config::cookies`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cookies: bool,
    /// Master debug toggle (M8.3): opt-in, persisted, default off. `false`
    /// unless set. Gates the Inspector overlay, Log panel, Traffic feed, and
    /// debug-only advanced settings (later waves); OFF means zero
    /// trace-capture overhead (`crate::debug`/`crate::http::execute_traced`'s
    /// `sink: None` path).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub debug: bool,
    /// Debug-gated advanced-limit overrides under an `[advanced]` table (M8.3
    /// Wave 4): default load-run concurrency/total, response body-size cap,
    /// and per-request timeout. Deferred from Wave 1's `debug` knob (ruling
    /// 6) — reachable only through the Options overlay's Advanced section
    /// when [`Config::debug`] is on. Resolved via [`Config::advanced_limits`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced: Option<AdvancedSection>,
}

/// The `[load]` config table: optional per-field overrides of the load-run
/// guardrail caps. A missing field falls back to the [`crate::load::LoadCaps`]
/// default for that knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LoadSection {
    /// Override for `warn_total`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_total: Option<usize>,
    /// Override for `warn_concurrency`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_concurrency: Option<usize>,
    /// Override for `max_total`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total: Option<usize>,
    /// Override for `max_concurrency`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
}

/// The `[advanced]` config table (M8.3 Wave 4, debug-gated): optional
/// overrides for the four "advanced/dangerous" knobs surfaced in the Options
/// overlay's Advanced section — a NEW load run's default concurrency/total
/// (overriding [`crate::load::LoadConfig::default`]), the response body-size
/// cap, and the per-request timeout. A missing field falls back to the
/// existing default for that knob (see [`Config::advanced_limits`]). Mirrors
/// [`LoadSection`]'s override-over-default shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdvancedSection {
    /// Override for a new load run's default concurrency. Still refused above
    /// `[load] max_concurrency` by [`crate::load::check_config`] even when set
    /// here — this only changes the *default* offered, not the guardrail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
    /// Override for a new load run's default total copies. Still refused
    /// above `[load] max_total` — see `concurrency`'s doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// Override for the response body-size cap, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_cap_bytes: Option<u64>,
    /// Override for the per-request timeout, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Fully-resolved advanced-limit overrides — every field defaulted, ready for
/// the session to apply. Returned by [`Config::advanced_limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAdvancedLimits {
    /// A new load run's default concurrency.
    pub concurrency: usize,
    /// A new load run's default total copies.
    pub total: usize,
    /// The response body-size cap, in bytes.
    pub body_cap_bytes: u64,
    /// The per-request timeout, in seconds.
    pub timeout_secs: u64,
}

/// Where the URL bar's `i`/`Enter` opens the editor: inline in the bar, or in a
/// centered vim popup. `e` on the bar always opens the popup regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UrlEditMode {
    /// Edit the URL inline in the bar.
    #[default]
    Inline,
    /// Open the centered edtui vim popup.
    Popup,
}

/// How redirects are followed and what happens to auth-bearing headers when a
/// hop crosses the origin. Origin is scheme + host + port: an `http`→`https`
/// upgrade, a host change, or a port change all cross it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RedirectPolicy {
    /// Follow same-origin redirects only. A cross-origin 3xx stops and the
    /// redirect response (status + `Location`) is surfaced to the user rather
    /// than followed silently.
    Strict,
    /// Follow redirects, but drop every auth-bearing header (`Authorization`,
    /// `Cookie`, any secret-named header, and the churl-injected auth header)
    /// before following a hop whose target origin differs — curl/browser
    /// behaviour. Same-origin hops keep all headers.
    #[default]
    Strip,
    /// Follow every hop keeping all headers across origins, up to the hop cap.
    /// The foot-gun setting: a resolved secret can leak to a redirect target,
    /// so its first use warns once.
    FollowAll,
}

/// One entry in the raw `[keys]` TOML table: either a flat `combo = "action"`
/// binding, or a nested overlay sub-table (`[keys.request]`, `[keys.leader]`).
///
/// The overlay variant is recursive (`BTreeMap<String, KeyEntry>`) so a table can
/// itself carry nested sub-tables. Only `[keys.leader]` uses that today — it may
/// mix flat root binds (`x = "quit"`) with submenu sub-tables
/// (`[keys.leader.sequences]`); [`Config::split_key_overlays`] flattens the nested
/// sub-tables into dotted [`Config::key_overlays`] keys (e.g. `"leader.sequences"`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum KeyEntry {
    /// A flat global binding: the action name string.
    Action(String),
    /// A nested overlay table: `combo → entry` for one context (values are
    /// action strings, or further sub-tables in the leader case).
    Overlay(BTreeMap<String, KeyEntry>),
}

impl Config {
    /// Partitions [`Config::raw_keys`] into the flat [`Config::keys`] map and the
    /// nested [`Config::key_overlays`] tables. Called by [`load_config`] after
    /// deserialization; idempotent.
    ///
    /// An overlay table's string members become that table's `combo → action`
    /// map. `[keys.leader]` may additionally carry submenu sub-tables
    /// (`[keys.leader.sequences]` / `.load`); those flatten into dotted overlay
    /// keys (`"leader.sequences"`). A sub-table nested anywhere else — or a
    /// submenu carrying its own sub-table — is a hard error (fail loud).
    fn split_key_overlays(&mut self) -> Result<(), ConfigError> {
        self.keys.clear();
        self.key_overlays.clear();
        for (name, entry) in &self.raw_keys {
            match entry {
                KeyEntry::Action(action) => {
                    self.keys.insert(name.clone(), action.clone());
                }
                KeyEntry::Overlay(table) => {
                    let mut flat = BTreeMap::new();
                    for (key, value) in table {
                        match value {
                            KeyEntry::Action(action) => {
                                flat.insert(key.clone(), action.clone());
                            }
                            KeyEntry::Overlay(sub) => {
                                // Only `[keys.leader]` may nest a further table.
                                if name != "leader" {
                                    return Err(ConfigError::BadKeys(format!(
                                        "sub-table [keys.{name}.{key}] is not allowed \
                                         (only [keys.leader] may carry sub-tables)"
                                    )));
                                }
                                // The submenu's members must all be scalars.
                                let mut sub_flat = BTreeMap::new();
                                for (sub_key, sub_val) in sub {
                                    match sub_val {
                                        KeyEntry::Action(action) => {
                                            sub_flat.insert(sub_key.clone(), action.clone());
                                        }
                                        KeyEntry::Overlay(_) => {
                                            return Err(ConfigError::BadKeys(format!(
                                                "[keys.{name}.{key}.{sub_key}] nests too deep \
                                                 (leader submenus take combo = \"action\" only)"
                                            )));
                                        }
                                    }
                                }
                                self.key_overlays.insert(format!("{name}.{key}"), sub_flat);
                            }
                        }
                    }
                    self.key_overlays.insert(name.clone(), flat);
                }
            }
        }
        Ok(())
    }

    /// The resolved response body-size cap: `max_body_bytes`, or the 10 MB default.
    pub fn max_body_bytes(&self) -> u64 {
        self.max_body_bytes
            .unwrap_or(crate::http::DEFAULT_MAX_BODY_BYTES)
    }

    /// The resolved per-request timeout: `timeout_secs`, or the 30 s default.
    pub fn timeout(&self) -> Duration {
        self.timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(crate::http::DEFAULT_TIMEOUT)
    }

    /// The resolved concurrent-load guardrail caps: the `[load]` overrides folded
    /// over [`crate::load::LoadCaps::default`] (each absent field keeps its
    /// default). An absent `[load]` table yields the defaults verbatim.
    pub fn load_caps(&self) -> crate::load::LoadCaps {
        let defaults = crate::load::LoadCaps::default();
        match &self.load {
            None => defaults,
            Some(section) => crate::load::LoadCaps {
                warn_total: section.warn_total.unwrap_or(defaults.warn_total),
                warn_concurrency: section
                    .warn_concurrency
                    .unwrap_or(defaults.warn_concurrency),
                max_total: section.max_total.unwrap_or(defaults.max_total),
                max_concurrency: section.max_concurrency.unwrap_or(defaults.max_concurrency),
            },
        }
    }

    /// The resolved advanced-limit overrides (M8.3 Wave 4): each `[advanced]`
    /// field folded over the existing default for that knob — a new load
    /// run's built-in `LoadConfig::default()` concurrency/total, and the
    /// already-resolved [`Config::max_body_bytes`]/[`Config::timeout`]. An
    /// absent `[advanced]` table yields those defaults verbatim, so the
    /// session behaves exactly as before M8.3 until a value is explicitly
    /// set (via the Advanced settings UI, hand-edited `config.toml`, or a
    /// future M8.5 settings panel).
    pub fn advanced_limits(&self) -> ResolvedAdvancedLimits {
        let load_defaults = crate::load::LoadConfig::default();
        let section = self.advanced.unwrap_or_default();
        ResolvedAdvancedLimits {
            concurrency: section.concurrency.unwrap_or(load_defaults.concurrency),
            total: section.total.unwrap_or(load_defaults.total),
            body_cap_bytes: section.body_cap_bytes.unwrap_or(self.max_body_bytes()),
            timeout_secs: section.timeout_secs.unwrap_or(self.timeout().as_secs()),
        }
    }

    /// The resolved URL-edit mode. An unknown value is a hard error (fail loud,
    /// like every other config knob).
    pub fn url_edit(&self) -> Result<UrlEditMode, ConfigError> {
        match self.url_edit.as_deref() {
            None | Some("inline") => Ok(UrlEditMode::Inline),
            Some("popup") => Ok(UrlEditMode::Popup),
            Some(other) => Err(ConfigError::BadValue {
                key: "url_edit".to_owned(),
                value: other.to_owned(),
                expected: "one of: inline, popup",
            }),
        }
    }

    /// The resolved save-time secret policy. An unknown value is a hard error
    /// (fail loud, like every other config knob).
    pub fn secret_policy(&self) -> Result<crate::secrets::SecretPolicy, ConfigError> {
        use crate::secrets::SecretPolicy;
        match self.secret_policy.as_deref() {
            None | Some("strict") => Ok(SecretPolicy::Strict),
            Some("warn") => Ok(SecretPolicy::Warn),
            Some(other) => Err(ConfigError::BadValue {
                key: "secret_policy".to_owned(),
                value: other.to_owned(),
                expected: "one of: strict, warn",
            }),
        }
    }

    /// The resolved cross-origin redirect policy. An unknown value is a hard
    /// error (fail loud, like every other config knob).
    pub fn redirect(&self) -> Result<RedirectPolicy, ConfigError> {
        match self.redirect.as_deref() {
            None | Some("strip") => Ok(RedirectPolicy::Strip),
            Some("strict") => Ok(RedirectPolicy::Strict),
            Some("follow-all") => Ok(RedirectPolicy::FollowAll),
            Some(other) => Err(ConfigError::BadValue {
                key: "redirect".to_owned(),
                value: other.to_owned(),
                expected: "one of: strict, strip, follow-all",
            }),
        }
    }

    /// The resolved global proxy URL (verbatim). Cross-source precedence
    /// (CLI > workspace > global > env) is the caller's job — see the binary's
    /// `install_runtime`.
    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    /// The resolved insecure-TLS launch default.
    pub fn insecure(&self) -> bool {
        self.insecure
    }

    /// The resolved cookie-jar launch default.
    pub fn cookies(&self) -> bool {
        self.cookies
    }
}

/// Whether a proxy URL string carries userinfo (`user:pass@host` or `user@host`).
///
/// Security predicate: a proxy with embedded credentials is allowed at runtime
/// from CLI/env/the Options overlay (masked in any UI display), but must **never**
/// be written to a synced workspace/config file — see
/// [`crate::persistence::save_workspace_manifest_checked`], which refuses rather
/// than silently stripping the credentials. Scheme-less proxies (`user:pass@host:3128`,
/// curl's `-x` form) are normalised with an `http://` prefix before parsing; an
/// unparseable candidate falls back to a literal `@` check so the refusal fails
/// safe (a suspicious string is treated as credential-bearing).
pub fn proxy_has_credentials(proxy: &str) -> bool {
    let candidate = if proxy.contains("://") {
        proxy.to_owned()
    } else {
        format!("http://{proxy}")
    };
    match reqwest::Url::parse(&candidate) {
        Ok(url) => !url.username().is_empty() || url.password().is_some(),
        Err(_) => proxy.contains('@'),
    }
}

/// Masks any userinfo (`user:pass@`) in a proxy URL for display, so a proxy that
/// carries credentials at runtime never shows them on screen, in a log, or in an
/// error/import note. Returns the input unchanged when there is no userinfo. Only
/// the authority's userinfo is touched (the scheme + host + port survive).
pub fn mask_proxy(proxy: &str) -> String {
    let (scheme, rest) = match proxy.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), proxy),
    };
    match rest.split_once('@') {
        Some((_creds, host)) => format!("{scheme}***@{host}"),
        None => proxy.to_owned(),
    }
}

/// Returns the path of the global config file (`<config_dir>/churl/config.toml`),
/// or `None` when the platform config directory cannot be determined.
pub fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("churl").join("config.toml"))
}

/// Loads a [`Config`] from `path`. A missing file is not an error and yields
/// [`Config::default`].
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(err) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source: err,
            });
        }
    };
    let mut config: Config = toml_edit::de::from_str(&text).map_err(|err| ConfigError::Parse {
        path: path.to_owned(),
        source: err,
    })?;
    config.split_key_overlays()?;
    Ok(config)
}

/// The environment variable that pins the global config file to an explicit
/// path, overriding platform discovery. Deterministic on every OS — unlike
/// `dirs::config_dir()`, which reads `%APPDATA%` from the process token on
/// Windows (ignoring `HOME`/`XDG_CONFIG_HOME`), so it is the only portable way
/// for tests/CI to point churl at a config they control on all three platforms.
pub const CONFIG_PATH_ENV: &str = "CHURL_CONFIG";

/// Loads the global config. Discovery order: an explicit [`CONFIG_PATH_ENV`]
/// override wins; otherwise the platform path from [`global_config_path`].
/// Yields [`Config::default`] when neither is set or the file is missing.
pub fn load_global_config() -> Result<Config, ConfigError> {
    if let Some(path) = std::env::var_os(CONFIG_PATH_ENV).filter(|v| !v.is_empty()) {
        return load_config(Path::new(&path));
    }
    match global_config_path() {
        Some(path) => load_config(&path),
        None => Ok(Config::default()),
    }
}

/// Case-insensitive substrings that flag a *name* (variable / header / query
/// key) as secret-looking. Every marker is audited to stay low-false-positive as
/// a substring of a real name:
///
/// - `token`/`secret`/`password`/`passwd`/`bearer`/`credential`/`authorization`:
///   original audited set — a name containing any of these is overwhelmingly a
///   credential. `token` already covers `apitoken`/`access_token`/`id_token`, so
///   those are not listed separately.
/// - `api_key`/`apikey`/`api-key`: the three spellings of an API key name.
/// - `private_key`/`privatekey`: PEM/SSH private-key material.
/// - `passphrase`: a passphrase is a secret by definition; the `pass` root would
///   over-match (`passenger`, `bypass`), so the whole word is used.
/// - `access_key`/`secret_key`/`client_secret`: cloud/OAuth credential names
///   (AWS access/secret keys, OAuth client secret). `secret_key` is redundant
///   with `secret` but kept for readable intent; the substring match makes it a
///   no-op either way.
/// - `signature`: request-signing material (HMAC/AWS SigV4 `X-Signature`) is a
///   credential; `signature` as a *name* substring rarely collides with
///   non-secret fields.
/// - `pat`: a personal access token, but only as a standalone marker would it
///   over-match (`path`, `update`, `compatible`), so it is deliberately NOT
///   included — `token` already catches the spelled-out forms.
///
/// Deliberately excluded (too noisy as a substring of a name): `auth` (matches
/// `author`/`authority`), `session`, `cookie`, `key` (matches `keyword`,
/// `monkey`). `Authorization`/`Cookie` headers are caught by name at the header
/// layer without adding these broad substrings here.
const SECRET_NAME_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "bearer",
    "private_key",
    "privatekey",
    "credential",
    "access_key",
    "secret_key",
    "client_secret",
    "signature",
];

/// Returns `true` when `name` looks like it names a secret (case-insensitive
/// substring match on markers such as `token`, `secret`, `password`, `api_key`, …).
pub fn looks_like_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_NAME_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Returns `true` when `value` is a single well-formed `{{...}}` template
/// placeholder rather than a literal. The placeholder shape also covers future
/// env references such as `{{env:FOO}}` (the resolver handles them; until then
/// placeholders are sent verbatim).
///
/// A well-formed placeholder is exactly one `{{ name }}` run: the value (after an
/// outer trim) starts with `{{`, ends with `}}`, and the inner token — trimmed of
/// surrounding whitespace — is a single non-empty run of `[A-Za-z0-9_.:-]` with no
/// internal whitespace and no further `{{`/`}}`. This rejects the junk the old
/// loose bracket check accepted (`{{}}`, `{{a b}}`, `{{a}} {{b}}`, `lit}}` /
/// `{{lit` fragments), so those no longer masquerade as placeholders and slip a
/// literal secret past the save gate.
pub fn is_template_placeholder(value: &str) -> bool {
    let trimmed = value.trim();
    let Some(inner) = trimmed
        .strip_prefix("{{")
        .and_then(|rest| rest.strip_suffix("}}"))
    else {
        return false;
    };
    let name = inner.trim();
    // A second `{`/`}` inside means more than one token (or a nested/torn brace),
    // e.g. `{{a}}{{b}}` or `{{{x}}`. The inner token must carry no braces at all.
    if name.is_empty() || name.contains(['{', '}']) {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// Finds secret-named auth fields on `endpoint` whose value is a literal rather
/// than a `{{...}}` template placeholder ([`is_template_placeholder`]).
///
/// Secret-named fields: `Basic.password` and `Bearer.token` always (their names
/// are the markers); `ApiKey.value` only when the api-key *name* looks secret
/// ([`looks_like_secret_name`], e.g. `X-Api-Key`). Returns field paths such as
/// `"auth.password"`; an empty vec means the endpoint is clean. Loads never
/// refuse — only churl-initiated writes are gated (see
/// [`crate::persistence::save_endpoint`] / [`crate::persistence::endpoint_to_toml`]).
pub fn auth_secret_violations(endpoint: &crate::model::Endpoint) -> Vec<String> {
    use crate::model::Auth;
    let mut violations = Vec::new();
    match &endpoint.request.auth {
        Some(Auth::Basic { password, .. }) if !is_template_placeholder(password) => {
            violations.push("auth.password".to_owned());
        }
        Some(Auth::Bearer { token }) if !is_template_placeholder(token) => {
            violations.push("auth.token".to_owned());
        }
        Some(Auth::ApiKey { name, value, .. })
            if looks_like_secret_name(name) && !is_template_placeholder(value) =>
        {
            violations.push("auth.value".to_owned());
        }
        _ => {}
    }
    violations
}

/// Returns the secret-named literal variables in a flat `name → value` map,
/// prefixing each offending name with `prefix` (e.g. `"vars"` or a profile name).
/// A `{{...}}` placeholder value is never a violation.
fn secret_var_violations<'a>(
    prefix: &str,
    vars: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Vec<String> {
    vars.into_iter()
        .filter(|(name, value)| looks_like_secret_name(name) && !is_template_placeholder(value))
        .map(|(name, _)| format!("{prefix}.{name}"))
        .collect()
}

/// Finds secret-looking literal variables anywhere in a workspace manifest: the
/// workspace-level `[vars]` table (prefixed `"vars"`) and every profile's vars
/// (prefixed by the profile name). A value that is a `{{...}}` template
/// placeholder ([`is_template_placeholder`]) is never a violation.
///
/// Returns `"vars.<var>"` / `"<profile>.<var>"` strings; an empty vec means the
/// workspace is clean. Secrets belong in the process environment, never in synced
/// workspace files. Loads never refuse — only churl-initiated writes are gated
/// (see [`crate::persistence::save_workspace_manifest`]).
pub fn secret_violations(ws: &Workspace) -> Vec<String> {
    let mut violations = secret_var_violations("vars", &ws.vars);
    for profile in &ws.profiles {
        violations.extend(secret_var_violations(&profile.name, &profile.vars));
    }
    violations
}

/// Finds secret-looking literal variables in a collection's `folder.toml`
/// `[vars]` table (prefixed `"vars"`). Mirrors [`secret_violations`] for the
/// collection scope; gates [`crate::persistence::save_collection_meta`].
pub fn collection_secret_violations(meta: &crate::model::CollectionMeta) -> Vec<String> {
    secret_var_violations("vars", &meta.vars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Profile;
    use std::collections::BTreeMap;

    #[test]
    fn missing_config_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_config(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn config_parses_theme() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"gruvbox\"\n").unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.theme.as_deref(), Some("gruvbox"));
    }

    #[test]
    fn config_parses_keys_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[keys]\n\"ctrl-p\" = \"open-palette\"\nq = \"quit\"\n",
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(
            config.keys.get("ctrl-p").map(String::as_str),
            Some("open-palette")
        );
        assert_eq!(config.keys.get("q").map(String::as_str), Some("quit"));
    }

    #[test]
    fn config_parses_theme_colors_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "theme = \"light\"\n[theme_colors]\ntitle = \"red\"\nselection = \"#112233\"\n",
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.theme.as_deref(), Some("light"));
        assert_eq!(
            config.theme_colors.get("title").map(String::as_str),
            Some("red")
        );
        assert_eq!(
            config.theme_colors.get("selection").map(String::as_str),
            Some("#112233")
        );
    }

    #[test]
    fn config_splits_keys_overlays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            concat!(
                "[keys]\nq = \"quit\"\n\"ctrl-p\" = \"open-palette\"\n\n",
                "[keys.request]\n\"]\" = \"tab-next\"\n\n",
                "[keys.explorer]\nn = \"new-endpoint\"\n",
            ),
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        // Flat bindings land in `keys`.
        assert_eq!(config.keys.get("q").map(String::as_str), Some("quit"));
        assert_eq!(
            config.keys.get("ctrl-p").map(String::as_str),
            Some("open-palette")
        );
        // Nested tables land in `key_overlays`, keyed by pane name.
        assert_eq!(
            config
                .key_overlays
                .get("request")
                .and_then(|t| t.get("]"))
                .map(String::as_str),
            Some("tab-next")
        );
        assert_eq!(
            config
                .key_overlays
                .get("explorer")
                .and_then(|t| t.get("n"))
                .map(String::as_str),
            Some("new-endpoint")
        );
        assert!(
            !config.keys.contains_key("request"),
            "overlay not in flat keys"
        );
    }

    #[test]
    fn config_parses_leader_submenu_tables() {
        // Real TOML: `[keys.leader]` mixes a flat root bind with two submenu
        // sub-tables. It parses cleanly and the submenu binds flatten into dotted
        // `key_overlays` keys.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            concat!(
                "[keys.leader]\n",
                "x = \"quit\"\n\n",
                "[keys.leader.sequences]\n",
                "z = \"run-sequence\"\n\n",
                "[keys.leader.load]\n",
                "z = \"load-runner-pick\"\n",
            ),
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        // The flat root bind lands in the `"leader"` overlay table.
        assert_eq!(
            config
                .key_overlays
                .get("leader")
                .and_then(|t| t.get("x"))
                .map(String::as_str),
            Some("quit")
        );
        // Submenu sub-tables flatten into dotted overlay keys.
        assert_eq!(
            config
                .key_overlays
                .get("leader.sequences")
                .and_then(|t| t.get("z"))
                .map(String::as_str),
            Some("run-sequence")
        );
        assert_eq!(
            config
                .key_overlays
                .get("leader.load")
                .and_then(|t| t.get("z"))
                .map(String::as_str),
            Some("load-runner-pick")
        );
    }

    #[test]
    fn config_leader_submenu_only_no_root_binds_parses() {
        // A `[keys.leader.sequences]` table WITHOUT a preceding scalar under
        // `[keys.leader]` still parses (regression: the leader overlay is created
        // even when it has no flat members).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[keys.leader.sequences]\nz = \"run-sequence\"\n").unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(
            config
                .key_overlays
                .get("leader.sequences")
                .and_then(|t| t.get("z"))
                .map(String::as_str),
            Some("run-sequence")
        );
    }

    #[test]
    fn config_non_leader_subtable_errors() {
        // A sub-table nested under a non-leader pane is a hard structural error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[keys.request.nope]\nz = \"tab-next\"\n").unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::BadKeys(_)),
            "expected BadKeys, got {err:?}"
        );
    }

    #[test]
    fn config_no_leader_tables_still_loads() {
        // An unrelated valid config with NO leader tables loads cleanly (guards
        // against the recursive KeyEntry change breaking ordinary configs).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            concat!(
                "theme = \"dark\"\n",
                "[keys]\nq = \"quit\"\n\n",
                "[keys.request]\n\"]\" = \"tab-next\"\n",
            ),
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.keys.get("q").map(String::as_str), Some("quit"));
        assert!(
            config.key_overlays.keys().all(|k| !k.starts_with("leader")),
            "no leader overlays for a leader-free config"
        );
    }

    #[test]
    fn config_keys_default_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"gruvbox\"\n").unwrap();
        let config = load_config(&path).unwrap();
        assert!(config.keys.is_empty());
    }

    #[test]
    fn config_parses_timeout_and_body_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "timeout_secs = 90\nmax_body_bytes = 1048576\n").unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.timeout_secs, Some(90));
        assert_eq!(config.timeout(), Duration::from_secs(90));
        assert_eq!(config.max_body_bytes, Some(1_048_576));
        assert_eq!(config.max_body_bytes(), 1_048_576);
    }

    #[test]
    fn config_timeout_and_body_cap_defaults() {
        let config = Config::default();
        assert_eq!(config.timeout(), Duration::from_secs(30));
        assert_eq!(config.max_body_bytes(), 10 * 1024 * 1024);
        assert_eq!(config.timeout(), crate::http::DEFAULT_TIMEOUT);
        assert_eq!(config.max_body_bytes(), crate::http::DEFAULT_MAX_BODY_BYTES);
    }

    #[test]
    fn config_url_edit_default_and_values() {
        let config = Config::default();
        assert_eq!(config.url_edit().unwrap(), UrlEditMode::Inline);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "url_edit = \"popup\"\n").unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.url_edit().unwrap(), UrlEditMode::Popup);

        std::fs::write(&path, "url_edit = \"inline\"\n").unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.url_edit().unwrap(), UrlEditMode::Inline);
    }

    #[test]
    fn config_url_edit_unknown_value_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "url_edit = \"nope\"\n").unwrap();
        let config = load_config(&path).unwrap();
        let err = config.url_edit().unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        assert!(err.to_string().contains("url_edit"), "{err}");
    }

    #[test]
    fn config_secret_policy_default_and_values() {
        use crate::secrets::SecretPolicy;
        // Absent → strict (the safe default).
        assert_eq!(
            Config::default().secret_policy().unwrap(),
            SecretPolicy::Strict
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "secret_policy = \"warn\"\n").unwrap();
        assert_eq!(
            load_config(&path).unwrap().secret_policy().unwrap(),
            SecretPolicy::Warn
        );
        std::fs::write(&path, "secret_policy = \"strict\"\n").unwrap();
        assert_eq!(
            load_config(&path).unwrap().secret_policy().unwrap(),
            SecretPolicy::Strict
        );
    }

    #[test]
    fn config_secret_policy_unknown_value_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "secret_policy = \"nope\"\n").unwrap();
        let config = load_config(&path).unwrap();
        let err = config.secret_policy().unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        assert!(err.to_string().contains("secret_policy"), "{err}");
    }

    #[test]
    fn config_redirect_default_and_values() {
        // Absent → strip (the safe default).
        assert_eq!(Config::default().redirect().unwrap(), RedirectPolicy::Strip);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "redirect = \"strict\"\n").unwrap();
        assert_eq!(
            load_config(&path).unwrap().redirect().unwrap(),
            RedirectPolicy::Strict
        );
        std::fs::write(&path, "redirect = \"strip\"\n").unwrap();
        assert_eq!(
            load_config(&path).unwrap().redirect().unwrap(),
            RedirectPolicy::Strip
        );
        std::fs::write(&path, "redirect = \"follow-all\"\n").unwrap();
        assert_eq!(
            load_config(&path).unwrap().redirect().unwrap(),
            RedirectPolicy::FollowAll
        );
    }

    #[test]
    fn config_redirect_unknown_value_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "redirect = \"nope\"\n").unwrap();
        let config = load_config(&path).unwrap();
        let err = config.redirect().unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        assert!(err.to_string().contains("redirect"), "{err}");
    }

    #[test]
    fn config_parses_proxy_insecure_cookies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "proxy = \"http://proxy.local:3128\"\ninsecure = true\ncookies = true\n",
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.proxy(), Some("http://proxy.local:3128"));
        assert!(config.insecure());
        assert!(config.cookies());
    }

    #[test]
    fn config_proxy_insecure_cookies_defaults() {
        let config = Config::default();
        assert_eq!(config.proxy(), None);
        assert!(!config.insecure());
        assert!(!config.cookies());
    }

    #[test]
    fn proxy_credential_detection() {
        // Credentialed proxies (with or without scheme) are detected.
        for creds in [
            "http://user:pass@proxy.local:3128",
            "https://user:pass@proxy.local",
            "user:pass@proxy.local:3128", // scheme-less (curl -x form)
            "http://user@proxy.local",    // username only
        ] {
            assert!(
                proxy_has_credentials(creds),
                "{creds} should be credentialed"
            );
        }
        // Credential-free proxies are allowed to persist.
        for clean in [
            "http://proxy.local:3128",
            "https://proxy.example.com",
            "proxy.local:3128",
        ] {
            assert!(!proxy_has_credentials(clean), "{clean} should be clean");
        }
    }

    #[test]
    fn config_load_caps_default_when_absent() {
        let config = Config::default();
        assert_eq!(config.load_caps(), crate::load::LoadCaps::default());
    }

    #[test]
    fn config_parses_load_caps_partial_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Override only two knobs; the rest keep their defaults.
        std::fs::write(&path, "[load]\nwarn_total = 25\nmax_concurrency = 64\n").unwrap();
        let config = load_config(&path).unwrap();
        let caps = config.load_caps();
        let defaults = crate::load::LoadCaps::default();
        assert_eq!(caps.warn_total, 25);
        assert_eq!(caps.max_concurrency, 64);
        assert_eq!(caps.warn_concurrency, defaults.warn_concurrency);
        assert_eq!(caps.max_total, defaults.max_total);
    }

    #[test]
    fn config_parses_load_caps_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[load]\nwarn_total = 10\nwarn_concurrency = 4\nmax_total = 500\nmax_concurrency = 32\n",
        )
        .unwrap();
        let caps = load_config(&path).unwrap().load_caps();
        assert_eq!(caps.warn_total, 10);
        assert_eq!(caps.warn_concurrency, 4);
        assert_eq!(caps.max_total, 500);
        assert_eq!(caps.max_concurrency, 32);
    }

    #[test]
    fn config_load_caps_malformed_value_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[load]\nwarn_total = \"lots\"\n").unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("config.toml"), "{err}");
    }

    #[test]
    fn config_advanced_limits_default_when_absent() {
        let config = Config::default();
        let limits = config.advanced_limits();
        let load_defaults = crate::load::LoadConfig::default();
        assert_eq!(limits.concurrency, load_defaults.concurrency);
        assert_eq!(limits.total, load_defaults.total);
        assert_eq!(limits.body_cap_bytes, crate::http::DEFAULT_MAX_BODY_BYTES);
        assert_eq!(limits.timeout_secs, crate::http::DEFAULT_TIMEOUT.as_secs());
    }

    #[test]
    fn config_parses_advanced_partial_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[advanced]\nconcurrency = 20\nbody_cap_bytes = 4096\n",
        )
        .unwrap();
        let config = load_config(&path).unwrap();
        let limits = config.advanced_limits();
        let load_defaults = crate::load::LoadConfig::default();
        assert_eq!(limits.concurrency, 20);
        assert_eq!(limits.body_cap_bytes, 4096);
        // Untouched knobs keep their existing defaults.
        assert_eq!(limits.total, load_defaults.total);
        assert_eq!(limits.timeout_secs, crate::http::DEFAULT_TIMEOUT.as_secs());
    }

    #[test]
    fn config_parses_advanced_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[advanced]\nconcurrency = 8\ntotal = 200\nbody_cap_bytes = 2048\ntimeout_secs = 5\n",
        )
        .unwrap();
        let limits = load_config(&path).unwrap().advanced_limits();
        assert_eq!(limits.concurrency, 8);
        assert_eq!(limits.total, 200);
        assert_eq!(limits.body_cap_bytes, 2048);
        assert_eq!(limits.timeout_secs, 5);
    }

    #[test]
    fn config_parses_leader_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "leader_key = \"ctrl-b\"\n").unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(config.leader_key.as_deref(), Some("ctrl-b"));
    }

    #[test]
    fn config_parse_error_carries_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = [broken\n").unwrap();
        let err = load_config(&path).unwrap_err();
        assert!(err.to_string().contains("config.toml"));
    }

    #[test]
    fn secret_name_detection() {
        for name in [
            "API_TOKEN",
            "db_password",
            "Passwd",
            "api_key",
            "ApiKey",
            "x-api-key",
            "authorization",
            "BEARER_VALUE",
            "private_key_pem",
            "aws_credentials",
            "client_secret",
        ] {
            assert!(looks_like_secret_name(name), "{name} should look secret");
        }
        for name in ["base_url", "user", "timeout_ms", "region"] {
            assert!(!looks_like_secret_name(name), "{name} should be fine");
        }
    }

    #[test]
    fn broadened_secret_markers_flagged() {
        // Markers added by the broadening audit.
        for name in [
            "passphrase",
            "PGP_PASSPHRASE",
            "access_key",
            "AWS_ACCESS_KEY_ID",
            "secret_key",
            "aws_secret_key",
            "client_secret",
            "OAUTH_CLIENT_SECRET",
            "X-Signature",
            "hmac_signature",
            "SSH_PRIVATEKEY",
        ] {
            assert!(looks_like_secret_name(name), "{name} should look secret");
        }
        // Deliberately-excluded roots must stay clean (low false positives).
        for name in [
            "author",
            "authority",
            "session_id",
            "cookie",
            "monkey",
            "path",
        ] {
            assert!(!looks_like_secret_name(name), "{name} should be fine");
        }
    }

    #[test]
    fn placeholder_tightening_rejects_junk_keeps_legit() {
        // Legit placeholders must still pass (the false-negatives we must not close).
        for good in ["{{token}}", "{{ token }}", "{{env:API_KEY}}", "{{a.b-c_d}}"] {
            assert!(is_template_placeholder(good), "{good:?} should pass");
        }
        // Junk the old loose bracket check wrongly accepted — now rejected so a
        // literal secret can't masquerade as a placeholder and skip the gate.
        for bad in [
            "{{}}",         // empty
            "{{ }}",        // whitespace-only
            "{{a b}}",      // internal space
            "{{a}} {{b}}",  // two runs
            "{{a}}{{b}}",   // adjacent runs
            "prefix {{a}}", // trailing text before
            "{{a}} suffix", // text after
            "not a var}}",  // no opening
            "{{not a var",  // no closing
            "{{{x}}",       // extra brace
        ] {
            assert!(!is_template_placeholder(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn auth_secret_violations_per_kind() {
        use crate::model::{ApiKeyPlacement, Auth, Endpoint, Method, Request};
        let endpoint = |auth: Option<Auth>| Endpoint {
            seq: 0,
            name: "t".into(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Get,
                url: "https://e.com/".into(),
                headers: Vec::new(),
                params: Vec::new(),
                body: None,
                auth,
                insecure: false,
            },
        };
        // No auth → clean.
        assert!(auth_secret_violations(&endpoint(None)).is_empty());
        // Literal password/token → violation; placeholder → clean.
        assert_eq!(
            auth_secret_violations(&endpoint(Some(Auth::Basic {
                username: "alice".into(),
                password: "hunter2".into(),
            }))),
            vec!["auth.password".to_string()]
        );
        assert!(
            auth_secret_violations(&endpoint(Some(Auth::Basic {
                username: "alice".into(),
                password: "{{password}}".into(),
            })))
            .is_empty()
        );
        assert_eq!(
            auth_secret_violations(&endpoint(Some(Auth::Bearer {
                token: "ghp_literal".into(),
            }))),
            vec!["auth.token".to_string()]
        );
        assert!(
            auth_secret_violations(&endpoint(Some(Auth::Bearer {
                token: "{{token}}".into(),
            })))
            .is_empty()
        );
        // ApiKey: only a secret-looking *name* gates the value.
        assert_eq!(
            auth_secret_violations(&endpoint(Some(Auth::ApiKey {
                name: "X-Api-Key".into(),
                value: "abc123".into(),
                placement: ApiKeyPlacement::Header,
            }))),
            vec!["auth.value".to_string()]
        );
        assert!(
            auth_secret_violations(&endpoint(Some(Auth::ApiKey {
                name: "X-Api-Key".into(),
                value: "{{api_key}}".into(),
                placement: ApiKeyPlacement::Header,
            })))
            .is_empty()
        );
        assert!(
            auth_secret_violations(&endpoint(Some(Auth::ApiKey {
                name: "tenant".into(),
                value: "acme".into(),
                placement: ApiKeyPlacement::Query,
            })))
            .is_empty(),
            "non-secret-named apikey values are allowed as literals"
        );
    }

    #[test]
    fn secret_violations_flags_literals_but_not_placeholders() {
        let ws = Workspace {
            name: "demo".into(),
            vars: BTreeMap::from([
                // workspace-level secret literal → flagged under "vars".
                ("workspace_secret".to_string(), "leaked".to_string()),
                ("base_url".to_string(), "https://example.com".to_string()),
            ]),
            profiles: vec![Profile {
                name: "prod".into(),
                vars: BTreeMap::from([
                    ("api_token".to_string(), "hunter2".to_string()),
                    ("auth_token".to_string(), "{{ AUTH_TOKEN }}".to_string()),
                    ("base_url".to_string(), "https://example.com".to_string()),
                ]),
            }],
            ..Default::default()
        };
        assert_eq!(
            secret_violations(&ws),
            vec![
                "vars.workspace_secret".to_string(),
                "prod.api_token".to_string(),
            ]
        );
    }

    #[test]
    fn collection_secret_violations_flags_literals_but_not_placeholders() {
        use crate::model::CollectionMeta;
        let meta = CollectionMeta {
            vars: BTreeMap::from([
                ("api_key".to_string(), "abc123".to_string()),
                ("token".to_string(), "{{TOKEN}}".to_string()),
                ("base_path".to_string(), "/v1".to_string()),
            ]),
            ..Default::default()
        };
        assert_eq!(
            collection_secret_violations(&meta),
            vec!["vars.api_key".to_string()]
        );
        assert!(collection_secret_violations(&CollectionMeta::default()).is_empty());
    }
}
