//! Global churl configuration and workspace secrets enforcement.
//!
//! The global config lives at `<config_dir>/churl/config.toml` (e.g.
//! `~/.config/churl/config.toml` on Linux). Workspace files (`churl.toml`, endpoint
//! files) must never contain secrets — see [`secret_violations`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Table};

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
    /// The config file exists but its content is not valid TOML at all (parsed
    /// as a raw [`toml_edit::DocumentMut`] for an in-place update — see
    /// [`save_defaults`] — rather than deserialized into [`Config`]).
    #[error("failed to parse existing config {path} for update: {source}")]
    ParseDocument {
        /// Path of the file that failed to parse.
        path: PathBuf,
        /// Underlying TOML document-parse error.
        source: toml_edit::TomlError,
    },
    /// The updated config could not be written back to disk.
    #[error("failed to write config {path}: {source}")]
    Write {
        /// Path of the file that failed to write.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The platform config directory could not be determined (see
    /// [`global_config_path`]), so there is nowhere to write a default config
    /// without an explicit [`CONFIG_PATH_ENV`] override.
    #[error(
        "could not determine the global config directory; set {CONFIG_PATH_ENV} to an \
         explicit path"
    )]
    NoConfigDir,
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

/// Every resolved value the settings panel (M8.5) manages, mirroring a
/// resolved [`Config`] knob — the panel's *working copy* / dirty-indicator
/// baseline, not the raw optional-override shape [`Config`] itself uses for
/// partial files, and **not** what gets persisted (see [`SettingsDefaults`]
/// for the sparse, touched-only shape [`save_defaults`] actually writes).
///
/// Deliberately **not** `Config` (or a wrapper around it): the panel reads/
/// compares base knobs only — `[keys]` / `[theme_colors]` / `raw_keys` are
/// owned by other subsystems and must never be touched by a settings-panel
/// save. Using a dedicated struct with no fields for them makes that
/// structurally impossible (there is nothing here to mistakenly serialize),
/// rather than relying on every future call site to remember to skip them on
/// a shared `Config`.
///
/// Two uses: [`Self::from_config`] resolves a loaded [`Config`] into this
/// shape as the panel's dirty-indicator baseline (`persisted`, re-read from
/// disk on every open/refresh); [`Self::default`] is the built-in baseline a
/// fresh install compares against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSettings {
    /// The colour theme name (`"dark"` | `"light"`); compared against the
    /// built-in default `"dark"`.
    pub theme: String,
    /// The leader key combination string; compared against the built-in
    /// default `"space"`.
    pub leader_key: String,
    /// Per-request timeout in seconds; compared against
    /// [`crate::http::DEFAULT_TIMEOUT`].
    pub timeout_secs: u64,
    /// Response body-size cap in bytes; compared against
    /// [`crate::http::DEFAULT_MAX_BODY_BYTES`].
    pub max_body_bytes: u64,
    /// What the URL bar's `i`/`Enter` opens.
    pub url_edit: UrlEditMode,
    /// Save-time secret policy.
    pub secret_policy: crate::secrets::SecretPolicy,
    /// Cross-origin redirect policy.
    pub redirect: RedirectPolicy,
    /// Global HTTP/HTTPS proxy URL, or `None` when unset.
    pub proxy: Option<String>,
    /// Whether TLS verification is off by default.
    pub insecure: bool,
    /// Whether the persistent cookie jar is on by default.
    pub cookies: bool,
    /// Master debug toggle.
    pub debug: bool,
    /// Concurrent-load guardrail caps; each field compared against
    /// [`crate::load::LoadCaps::default`].
    pub load_caps: crate::load::LoadCaps,
    /// Debug-gated advanced-limit overrides; each field compared against its
    /// existing default (see [`Config::advanced_limits`]).
    pub advanced: ResolvedAdvancedLimits,
}

impl Default for ResolvedSettings {
    /// The built-in defaults, expressed the same way [`Config::default`]'s own
    /// resolvers would.
    fn default() -> Self {
        let config = Config::default();
        Self {
            theme: "dark".to_owned(),
            leader_key: "space".to_owned(),
            timeout_secs: config.timeout().as_secs(),
            max_body_bytes: config.max_body_bytes(),
            url_edit: UrlEditMode::default(),
            secret_policy: crate::secrets::SecretPolicy::default(),
            redirect: RedirectPolicy::default(),
            proxy: None,
            insecure: false,
            cookies: false,
            debug: false,
            load_caps: config.load_caps(),
            advanced: config.advanced_limits(),
        }
    }
}

impl ResolvedSettings {
    /// Builds a [`ResolvedSettings`] from a loaded [`Config`]'s RESOLVED
    /// values — e.g. so a live session's working copy can be compared
    /// against what is currently on disk (the M8.5 settings panel's
    /// best-effort dirty indicator). Fails on the same malformed-value cases
    /// [`Config`]'s own resolvers do (an unknown `url_edit`/`secret_policy`/
    /// `redirect` string) — the caller decides how to degrade (the panel
    /// treats a failure as "nothing to compare against" rather than blocking).
    pub fn from_config(config: &Config) -> Result<Self, ConfigError> {
        Ok(Self {
            theme: config.theme.clone().unwrap_or_else(|| "dark".to_owned()),
            leader_key: config
                .leader_key
                .clone()
                .unwrap_or_else(|| "space".to_owned()),
            timeout_secs: config.timeout().as_secs(),
            max_body_bytes: config.max_body_bytes(),
            url_edit: config.url_edit()?,
            secret_policy: config.secret_policy()?,
            redirect: config.redirect()?,
            proxy: config.proxy().map(str::to_owned),
            insecure: config.insecure(),
            cookies: config.cookies(),
            debug: config.debug,
            load_caps: config.load_caps(),
            advanced: config.advanced_limits(),
        })
    }
}

/// A SPARSE set of settings-panel edits: `Some` on a field means the user
/// touched that knob in the panel this session and it should be persisted;
/// `None` means the knob was never edited in the panel and [`save_defaults`]
/// must leave its on-disk key completely untouched — even if the *session's*
/// resolved value differs from disk (e.g. `-k`, a workspace override, or an
/// already-saved global default). This is what closes the M8.5 footgun where
/// Save used to snapshot the whole EFFECTIVE session (CLI/workspace-forced
/// values included) and silently write them as new global defaults.
///
/// `proxy` is `Option<Option<String>>` because the underlying knob is itself
/// optional: `None` (outer) = proxy untouched this session, do not read or
/// write the key at all; `Some(None)` = the user cleared the proxy field in
/// the panel, persist that as "no proxy" (remove the key); `Some(Some(url))`
/// = the user set/edited a proxy value, persist it (unless it carries
/// credentials — [`save_defaults`] skips just that key in that case, see
/// [`SaveOutcome::proxy_skipped`]).
///
/// `timeout_secs`/`max_body_bytes` cover BOTH the top-level knob and the
/// `[advanced]` timeout/body-cap override — they are the exact same live
/// session field (see the Settings panel's `RequestRow` doc), so one touched
/// flag and one value covers both; there is no separate
/// `advanced_timeout`/`advanced_body_cap` field here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsDefaults {
    /// The colour theme name, if edited this session.
    pub theme: Option<String>,
    /// The leader key combination string, if edited this session.
    pub leader_key: Option<String>,
    /// Per-request timeout in seconds, if edited this session (Request's
    /// Timeout row OR Debug's Advanced `timeout` field — same live value).
    pub timeout_secs: Option<u64>,
    /// Response body-size cap in bytes, if edited this session (Request's
    /// MaxBodyBytes row OR Debug's Advanced `body cap` field — same live
    /// value).
    pub max_body_bytes: Option<u64>,
    /// URL-bar edit mode, if edited this session.
    pub url_edit: Option<UrlEditMode>,
    /// Save-time secret policy, if edited this session.
    pub secret_policy: Option<crate::secrets::SecretPolicy>,
    /// Cross-origin redirect policy, if edited this session.
    pub redirect: Option<RedirectPolicy>,
    /// The proxy edit, if any this session — see the struct doc for the
    /// nested-`Option` shape.
    pub proxy: Option<Option<String>>,
    /// Whether TLS verification was toggled this session.
    pub insecure: Option<bool>,
    /// Whether the cookie jar on/off was toggled this session.
    pub cookies: Option<bool>,
    /// Whether debug capture was toggled FROM THE PANEL this session (the
    /// global `<leader>D` shortcut shares the same underlying flag but does
    /// NOT touch this — only the panel's Debug-category row does).
    pub debug: Option<bool>,
    /// [`crate::load::LoadCaps::warn_total`], if edited this session.
    pub load_warn_total: Option<usize>,
    /// [`crate::load::LoadCaps::warn_concurrency`], if edited this session.
    pub load_warn_concurrency: Option<usize>,
    /// [`crate::load::LoadCaps::max_total`], if edited this session.
    pub load_max_total: Option<usize>,
    /// [`crate::load::LoadCaps::max_concurrency`], if edited this session.
    pub load_max_concurrency: Option<usize>,
    /// [`ResolvedAdvancedLimits::concurrency`], if edited this session (Debug
    /// category only — no top-level knob aliases this one).
    pub advanced_concurrency: Option<usize>,
    /// [`ResolvedAdvancedLimits::total`], if edited this session (Debug
    /// category only — no top-level knob aliases this one).
    pub advanced_total: Option<usize>,
}

/// What [`save_defaults`] actually did, beyond "it didn't error" — today just
/// whether a credential-bearing proxy was present and therefore skipped (see
/// [`SettingsDefaults::proxy`]'s doc and [`proxy_has_credentials`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SaveOutcome {
    /// `true` when `edits.proxy` carried embedded credentials — the proxy key
    /// was left completely untouched on disk (never written, never removed),
    /// while every other touched knob in the same call still persisted.
    pub proxy_skipped: bool,
}

/// Sets `doc[key]` to a string, or removes it when `value` is `None`. A
/// no-op when the document already holds exactly `value` (FIX 3: avoids
/// churning — and losing — that key's inline comment/decor on a write that
/// doesn't actually change it).
fn set_or_remove_str(doc: &mut toml_edit::DocumentMut, key: &str, value: Option<&str>) {
    match value {
        Some(v) => {
            if doc.get(key).and_then(|item| item.as_str()) != Some(v) {
                doc[key] = toml_edit::value(v);
            }
        }
        None => {
            doc.remove(key);
        }
    }
}

/// Sets `doc[key]` to an integer, or removes it when `value` is `None`.
/// `u64` values beyond `i64::MAX` saturate rather than panic — no real
/// timeout/body-cap/limit knob approaches that range. A no-op when the
/// document already holds exactly `value` (see [`set_or_remove_str`]'s doc).
fn set_or_remove_u64(doc: &mut toml_edit::DocumentMut, key: &str, value: Option<u64>) {
    match value {
        Some(v) => {
            let v = i64::try_from(v).unwrap_or(i64::MAX);
            if doc.get(key).and_then(|item| item.as_integer()) != Some(v) {
                doc[key] = toml_edit::value(v);
            }
        }
        None => {
            doc.remove(key);
        }
    }
}

/// Sets `doc[key]` to `true`, or removes it when `value == default` (`false`
/// for every panel-managed boolean today). A no-op when the document already
/// holds exactly `value` (see [`set_or_remove_str`]'s doc).
fn set_or_remove_bool(doc: &mut toml_edit::DocumentMut, key: &str, value: bool, default: bool) {
    if value == default {
        doc.remove(key);
    } else if doc.get(key).and_then(|item| item.as_bool()) != Some(value) {
        doc[key] = toml_edit::value(value);
    }
}

/// Sets or removes `int_key` inside the `[table_key]` sub-table, creating the
/// table on first use and removing it entirely once every managed field is
/// back at its default (an empty override table is noise, not signal). A
/// no-op when the table already holds exactly `value` at `int_key` (see
/// [`set_or_remove_str`]'s doc).
fn set_or_remove_subtable_u64(
    doc: &mut toml_edit::DocumentMut,
    table_key: &str,
    int_key: &str,
    value: Option<u64>,
) {
    match value {
        Some(v) => {
            let v = i64::try_from(v).unwrap_or(i64::MAX);
            let table = doc[table_key].or_insert(toml_edit::table());
            let unchanged = table
                .as_table()
                .and_then(|t| t.get(int_key))
                .and_then(|item| item.as_integer())
                == Some(v);
            if !unchanged {
                table[int_key] = toml_edit::value(v);
            }
        }
        None => {
            if let Some(table) = doc.get_mut(table_key).and_then(|item| item.as_table_mut()) {
                table.remove(int_key);
            }
        }
    }
    if doc
        .get(table_key)
        .and_then(|item| item.as_table())
        .is_some_and(Table::is_empty)
    {
        doc.remove(table_key);
    }
}

/// Persists `edits` to `path` — ONLY the knobs `edits` marks touched
/// (`Some`); every `None` field's on-disk key is left completely untouched,
/// whatever the live session's resolved value for it is. This is the FIX 1
/// contract: Save must never write a CLI/session-forced value the user never
/// actually edited in the panel — the caller (the TUI's `settings_edits`) is
/// responsible for only setting a field to `Some` when its
/// `SettingKey` was recorded touched, and clearing that touched-set after a
/// successful save so the working copy and the save target stay in sync.
///
/// Reads the existing file into a [`toml_edit::DocumentMut`] (an empty
/// document when the file is missing — a missing file is not an error,
/// mirroring [`load_config`]), sets each TOUCHED key that differs from its
/// built-in default, **removes** a touched key that now matches its default
/// (a deliberate reset), and writes the result back **atomically** (temp file
/// in the same directory, then rename over `path`; the parent directory is
/// created if missing). `[keys]` / `[theme_colors]` / `raw_keys` are never
/// read or touched — [`SettingsDefaults`] has no fields for them, so there is
/// nothing here that could.
///
/// Every OTHER key already in the file — including ones churl doesn't model,
/// and every untouched panel-managed key — survives byte-for-byte, comments
/// and all: only touched keys are set via the DOM, never a wholesale
/// re-serialize (and [`set_or_remove_str`]/[`set_or_remove_u64`]/etc. skip
/// the DOM write entirely when the value would not actually change, so even
/// a touched-but-unchanged key's decor survives).
///
/// A credential-bearing proxy ([`proxy_has_credentials`]) is never written —
/// but unlike every other knob, it does not block the rest of the save: see
/// [`SaveOutcome::proxy_skipped`] (FIX 4 — a save must not silently strip
/// credentials, but refusing the WHOLE save over one knob is worse than
/// skipping just that one).
pub fn save_defaults(edits: &SettingsDefaults, path: &Path) -> Result<SaveOutcome, ConfigError> {
    let proxy_skipped = matches!(&edits.proxy, Some(Some(p)) if proxy_has_credentials(p));

    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
            path: path.to_owned(),
            source,
        })?;
    }

    let mut doc: DocumentMut = match std::fs::read_to_string(path) {
        Ok(existing) => existing
            .parse()
            .map_err(|source| ConfigError::ParseDocument {
                path: path.to_owned(),
                source,
            })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };

    if let Some(theme) = &edits.theme {
        let v = (theme != "dark").then_some(theme.as_str());
        set_or_remove_str(&mut doc, "theme", v);
    }
    if let Some(leader_key) = &edits.leader_key {
        let v = (leader_key != "space").then_some(leader_key.as_str());
        set_or_remove_str(&mut doc, "leader_key", v);
    }
    if let Some(timeout_secs) = edits.timeout_secs {
        let v = (timeout_secs != crate::http::DEFAULT_TIMEOUT.as_secs()).then_some(timeout_secs);
        set_or_remove_u64(&mut doc, "timeout_secs", v);
    }
    if let Some(max_body_bytes) = edits.max_body_bytes {
        let v = (max_body_bytes != crate::http::DEFAULT_MAX_BODY_BYTES).then_some(max_body_bytes);
        set_or_remove_u64(&mut doc, "max_body_bytes", v);
    }
    if let Some(url_edit) = edits.url_edit {
        let v = (url_edit != UrlEditMode::default()).then_some(match url_edit {
            UrlEditMode::Inline => "inline",
            UrlEditMode::Popup => "popup",
        });
        set_or_remove_str(&mut doc, "url_edit", v);
    }
    if let Some(secret_policy) = edits.secret_policy {
        let v = (secret_policy != crate::secrets::SecretPolicy::default()).then_some(
            match secret_policy {
                crate::secrets::SecretPolicy::Strict => "strict",
                crate::secrets::SecretPolicy::Warn => "warn",
            },
        );
        set_or_remove_str(&mut doc, "secret_policy", v);
    }
    if let Some(redirect) = edits.redirect {
        let v = (redirect != RedirectPolicy::default()).then_some(match redirect {
            RedirectPolicy::Strip => "strip",
            RedirectPolicy::Strict => "strict",
            RedirectPolicy::FollowAll => "follow-all",
        });
        set_or_remove_str(&mut doc, "redirect", v);
    }
    // Proxy: untouched (`None`) leaves the key alone entirely; touched-and-
    // credentialed skips the key (never written, never removed — see
    // `proxy_skipped` above); touched-and-clean sets or clears it normally.
    if let Some(proxy) = &edits.proxy
        && !proxy_skipped
    {
        set_or_remove_str(&mut doc, "proxy", proxy.as_deref());
    }
    if let Some(insecure) = edits.insecure {
        set_or_remove_bool(&mut doc, "insecure", insecure, false);
    }
    if let Some(cookies) = edits.cookies {
        set_or_remove_bool(&mut doc, "cookies", cookies, false);
    }
    if let Some(debug) = edits.debug {
        set_or_remove_bool(&mut doc, "debug", debug, false);
    }

    let load_defaults = crate::load::LoadCaps::default();
    if let Some(v) = edits.load_warn_total {
        set_or_remove_subtable_u64(
            &mut doc,
            "load",
            "warn_total",
            (v != load_defaults.warn_total).then_some(v as u64),
        );
    }
    if let Some(v) = edits.load_warn_concurrency {
        set_or_remove_subtable_u64(
            &mut doc,
            "load",
            "warn_concurrency",
            (v != load_defaults.warn_concurrency).then_some(v as u64),
        );
    }
    if let Some(v) = edits.load_max_total {
        set_or_remove_subtable_u64(
            &mut doc,
            "load",
            "max_total",
            (v != load_defaults.max_total).then_some(v as u64),
        );
    }
    if let Some(v) = edits.load_max_concurrency {
        set_or_remove_subtable_u64(
            &mut doc,
            "load",
            "max_concurrency",
            (v != load_defaults.max_concurrency).then_some(v as u64),
        );
    }

    // `[advanced].concurrency`/`total` have no top-level alias — the ONLY
    // place they live. `[advanced].timeout_secs`/`body_cap_bytes` are NOT
    // written here at all: they would always equal the just-written top-level
    // `timeout_secs`/`max_body_bytes` (same live field, see the struct doc),
    // so an override would always immediately prune back to nothing anyway.
    let run_defaults = crate::load::LoadConfig::default();
    if let Some(v) = edits.advanced_concurrency {
        set_or_remove_subtable_u64(
            &mut doc,
            "advanced",
            "concurrency",
            (v != run_defaults.concurrency).then_some(v as u64),
        );
    }
    if let Some(v) = edits.advanced_total {
        set_or_remove_subtable_u64(
            &mut doc,
            "advanced",
            "total",
            (v != run_defaults.total).then_some(v as u64),
        );
    }

    crate::persistence::atomic_write(path, doc.to_string().as_bytes()).map_err(|source| {
        ConfigError::Write {
            path: path.to_owned(),
            source,
        }
    })?;
    Ok(SaveOutcome { proxy_skipped })
}

/// The path [`save_defaults`] should target: an explicit [`CONFIG_PATH_ENV`]
/// override wins (deterministic for tests/CI, mirroring [`load_global_config`]);
/// otherwise the platform path from [`global_config_path`]. Errs when neither
/// is available — unlike a *load*, a *write* has nowhere safe to default to.
pub fn resolve_settings_path() -> Result<PathBuf, ConfigError> {
    if let Some(path) = std::env::var_os(CONFIG_PATH_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    global_config_path().ok_or(ConfigError::NoConfigDir)
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

    // ---- M8.5 settings-panel config writer (`save_defaults`) ----

    /// Serializes access to the `CHURL_CONFIG` env var across the tests below
    /// that set it — `cargo test` runs this crate's tests on multiple threads
    /// in one process, and env vars are process-global. `unwrap_or_else` on
    /// lock rather than `unwrap`: a panic inside one guarded test must not
    /// poison the mutex and cascade-fail every sibling that locks after it.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard for `CHURL_CONFIG`: sets it under [`ENV_LOCK`] on
    /// construction, clears it on drop — so a test can never leave the env
    /// var set for the next one (early return, assertion panic, or otherwise)
    /// the way a bare `set_var`/`remove_var` pair could. Mirrors the TUI
    /// crate's `SettingsEnvGuard`.
    struct ConfigPathEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl ConfigPathEnvGuard {
        fn new(path: &Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { std::env::set_var(CONFIG_PATH_ENV, path) };
            Self { _lock: lock }
        }
    }

    impl Drop for ConfigPathEnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(CONFIG_PATH_ENV) };
        }
    }

    #[test]
    fn save_defaults_round_trip_preserves_comment_and_unknown_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# a note the user left themselves\ntheme = \"light\"\nmystery_key = 42\n",
        )
        .unwrap();

        // theme touched, still "light" — must survive alongside the comment.
        let edits = SettingsDefaults {
            theme: Some("light".to_owned()),
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# a note the user left themselves"),
            "leading comment must survive:\n{text}"
        );
        assert!(
            text.contains("mystery_key = 42"),
            "a key churl doesn't model must survive:\n{text}"
        );
        assert!(text.contains("theme = \"light\""), "{text}");
    }

    /// FIX 3's exact reviewer repro: editing ONE knob must never disturb an
    /// untouched key's value OR its inline comment — with the sparse writer
    /// this now holds structurally (an untouched field is never even passed
    /// to `set_or_remove_*`), not just by the differs-check.
    #[test]
    fn save_defaults_editing_one_knob_preserves_another_untouched_keys_inline_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "timeout_secs = 90 # prod SLA\n").unwrap();

        // Only theme is touched this session — timeout_secs must never be
        // read from or written to the document at all.
        let edits = SettingsDefaults {
            theme: Some("light".to_owned()),
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("timeout_secs = 90 # prod SLA"),
            "an untouched key's value AND inline comment must survive byte-for-byte:\n{text}"
        );
        assert!(text.contains("theme = \"light\""), "{text}");
    }

    /// FIX 3's differs-check: a touched key whose new value is IDENTICAL to
    /// what's already on disk must not churn the DOM (and so must not lose
    /// that key's inline comment either) — a no-op write stays a no-op.
    #[test]
    fn save_defaults_touched_but_unchanged_value_preserves_inline_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"light\" # matches my terminal\n").unwrap();

        let edits = SettingsDefaults {
            theme: Some("light".to_owned()), // touched, but same value already on disk
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("theme = \"light\" # matches my terminal"),
            "a touched-but-unchanged value must not churn the key's decor:\n{text}"
        );
    }

    #[test]
    fn save_defaults_sets_a_non_default_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let edits = SettingsDefaults {
            timeout_secs: Some(90),
            proxy: Some(Some("http://proxy.local:3128".to_owned())),
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("timeout_secs = 90"), "{text}");
        assert!(
            text.contains("proxy = \"http://proxy.local:3128\""),
            "{text}"
        );
    }

    #[test]
    fn save_defaults_touched_and_setting_back_to_default_removes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "timeout_secs = 90\n").unwrap();

        // Touched (the user reset it in the panel) and back at the built-in
        // default — a deliberate reset prunes the stale key.
        let edits = SettingsDefaults {
            timeout_secs: Some(crate::http::DEFAULT_TIMEOUT.as_secs()),
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("timeout_secs"),
            "resetting a touched key to its default must prune the stale key:\n{text}"
        );
    }

    #[test]
    fn save_defaults_untouched_key_is_never_pruned_even_if_stale() {
        // The old whole-snapshot writer would prune a stale [load] override
        // the moment ANY save happened, even if the user never touched Load
        // this session. The sparse writer must leave it alone entirely.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[load]\nwarn_total = 25\n").unwrap();

        let edits = SettingsDefaults {
            theme: Some("light".to_owned()), // touches a DIFFERENT knob
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[load]") && text.contains("warn_total = 25"),
            "an untouched [load] override must survive a save that edits a different knob:\n{text}"
        );
    }

    #[test]
    fn save_defaults_load_subtable_partial_override_keeps_only_that_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let edits = SettingsDefaults {
            load_max_concurrency: Some(64),
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        let load = doc["load"].as_table().expect("[load] table present");
        assert_eq!(load["max_concurrency"].as_integer(), Some(64));
        assert!(
            load.get("warn_total").is_none(),
            "untouched load knobs stay absent"
        );
    }

    /// FIX 4: a panel-typed credentialed proxy must never block the rest of
    /// the save — only the proxy key is skipped (never written, never
    /// removed), while every other touched knob in the same call persists.
    #[test]
    fn save_defaults_skips_credentialed_proxy_but_persists_other_touched_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let edits = SettingsDefaults {
            proxy: Some(Some("http://user:pass@proxy.local:3128".to_owned())),
            timeout_secs: Some(90),
            ..SettingsDefaults::default()
        };
        let outcome = save_defaults(&edits, &path).unwrap();
        assert!(
            outcome.proxy_skipped,
            "a credentialed proxy must be reported as skipped"
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("timeout_secs = 90"),
            "other touched knobs must still persist:\n{text}"
        );
        assert!(
            !text.contains("proxy"),
            "the proxy key must not be written at all:\n{text}"
        );
        assert!(
            !text.contains("user:pass") && !text.contains("pass"),
            "credentials must never reach disk:\n{text}"
        );
    }

    #[test]
    fn save_defaults_never_writes_keys_or_theme_colors_tables() {
        // A settings-panel save must never touch [keys]/[theme_colors]/raw_keys —
        // SettingsDefaults structurally has no fields for them, but this asserts
        // the on-disk behaviour too: an existing [keys]/[theme_colors] table
        // survives a save byte-for-byte.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[keys]\nq = \"quit\"\n\n[theme_colors]\ntitle = \"red\"\n",
        )
        .unwrap();

        let edits = SettingsDefaults {
            timeout_secs: Some(45),
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[keys]") && text.contains("q = \"quit\""),
            "{text}"
        );
        assert!(
            text.contains("[theme_colors]") && text.contains("title = \"red\""),
            "{text}"
        );
    }

    #[test]
    fn resolve_settings_path_honors_churl_config_override() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("config.toml");
        let _env = ConfigPathEnvGuard::new(&target);
        let resolved = resolve_settings_path().unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn save_defaults_creates_missing_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        let edits = SettingsDefaults {
            timeout_secs: Some(45),
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_defaults_via_churl_config_then_load_round_trips_resolved_values() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        let _env = ConfigPathEnvGuard::new(&target);

        let edits = SettingsDefaults {
            theme: Some("light".to_owned()),
            timeout_secs: Some(77),
            max_body_bytes: Some(2_048),
            url_edit: Some(UrlEditMode::Popup),
            secret_policy: Some(crate::secrets::SecretPolicy::Warn),
            redirect: Some(RedirectPolicy::FollowAll),
            insecure: Some(true),
            cookies: Some(true),
            load_max_concurrency: Some(64),
            ..SettingsDefaults::default()
        };

        let path = resolve_settings_path().unwrap();
        assert_eq!(path, target);
        save_defaults(&edits, &path).unwrap();

        let loaded = load_global_config().unwrap();

        assert_eq!(loaded.theme.as_deref(), Some("light"));
        assert_eq!(loaded.timeout(), Duration::from_secs(77));
        assert_eq!(loaded.max_body_bytes(), 2_048);
        assert_eq!(loaded.url_edit().unwrap(), UrlEditMode::Popup);
        assert_eq!(
            loaded.secret_policy().unwrap(),
            crate::secrets::SecretPolicy::Warn
        );
        assert_eq!(loaded.redirect().unwrap(), RedirectPolicy::FollowAll);
        assert!(loaded.insecure());
        assert!(loaded.cookies());
        assert_eq!(loaded.load_caps().max_concurrency, 64);
    }

    #[test]
    fn settings_defaults_from_config_default_matches_default() {
        // A default Config resolves to the exact same ResolvedSettings as
        // ResolvedSettings::default() (both express the built-in defaults).
        assert_eq!(
            ResolvedSettings::from_config(&Config::default()).unwrap(),
            ResolvedSettings::default()
        );
    }

    #[test]
    fn settings_defaults_from_config_reflects_a_saved_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let edits = SettingsDefaults {
            theme: Some("light".to_owned()),
            timeout_secs: Some(45),
            insecure: Some(true),
            ..SettingsDefaults::default()
        };
        save_defaults(&edits, &path).unwrap();

        let loaded = load_config(&path).unwrap();
        let resolved = ResolvedSettings::from_config(&loaded).unwrap();
        assert_eq!(resolved.theme, "light");
        assert_eq!(resolved.timeout_secs, 45);
        assert!(resolved.insecure);
        // Untouched knobs stay at the built-in default.
        assert_eq!(
            resolved.max_body_bytes,
            ResolvedSettings::default().max_body_bytes
        );
    }

    #[test]
    fn settings_defaults_from_config_propagates_bad_enum_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "redirect = \"nope\"\n").unwrap();
        let loaded = load_config(&path).unwrap();
        let err = ResolvedSettings::from_config(&loaded).unwrap_err();
        assert!(matches!(err, ConfigError::BadValue { .. }));
    }
}
