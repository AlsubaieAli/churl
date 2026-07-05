//! Global churl configuration and workspace secrets enforcement.
//!
//! The global config lives at `<config_dir>/churl/config.toml` (e.g.
//! `~/.config/churl/config.toml` on Linux). Workspace files (`churl.toml`, endpoint
//! files) must never contain secrets — see [`secret_violations`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Workspace;

/// Error loading a churl [`Config`] file.
#[derive(Debug, thiserror::Error)]
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
}

/// Global churl configuration. Every field is optional; a missing file means defaults.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Config {
    /// Name of the colour theme to use; `None` means the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Keybinding overrides under a flat `[keys]` table: key-combination string →
    /// action name (e.g. `"ctrl-p" = "open-palette"`). Core carries only strings;
    /// the TUI layer parses combinations and action names and rejects unknown
    /// entries loudly at startup.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, String>,
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
    toml_edit::de::from_str(&text).map_err(|err| ConfigError::Parse {
        path: path.to_owned(),
        source: err,
    })
}

/// Loads the global config from [`global_config_path`]. Yields [`Config::default`]
/// when the platform config directory is unknown or the file is missing.
pub fn load_global_config() -> Result<Config, ConfigError> {
    match global_config_path() {
        Some(path) => load_config(&path),
        None => Ok(Config::default()),
    }
}

/// Case-insensitive markers that flag a variable name as secret-looking.
const SECRET_NAME_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "bearer",
    "private_key",
    "credential",
];

/// Returns `true` when `name` looks like it names a secret (case-insensitive
/// substring match on markers such as `token`, `secret`, `password`, `api_key`, …).
pub fn looks_like_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_NAME_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Returns `true` when `value` is a `{{...}}` template placeholder rather than a literal.
fn is_template_placeholder(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("{{") && trimmed.ends_with("}}")
}

/// Finds profile variables in `ws` whose name looks secret ([`looks_like_secret_name`])
/// and whose value is a literal rather than a `{{...}}` template placeholder.
///
/// Returns `"<profile>.<var>"` strings; an empty vec means the workspace is clean.
/// Secrets belong in the process environment, never in synced workspace files.
pub fn secret_violations(ws: &Workspace) -> Vec<String> {
    let mut violations = Vec::new();
    for profile in &ws.profiles {
        for (name, value) in &profile.vars {
            if looks_like_secret_name(name) && !is_template_placeholder(value) {
                violations.push(format!("{}.{}", profile.name, name));
            }
        }
    }
    violations
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
    fn config_keys_default_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "theme = \"gruvbox\"\n").unwrap();
        let config = load_config(&path).unwrap();
        assert!(config.keys.is_empty());
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
    fn secret_violations_flags_literals_but_not_placeholders() {
        let ws = Workspace {
            name: "demo".into(),
            profiles: vec![Profile {
                name: "prod".into(),
                vars: BTreeMap::from([
                    ("api_token".to_string(), "hunter2".to_string()),
                    ("auth_token".to_string(), "{{ AUTH_TOKEN }}".to_string()),
                    ("base_url".to_string(), "https://example.com".to_string()),
                ]),
            }],
        };
        assert_eq!(secret_violations(&ws), vec!["prod.api_token".to_string()]);
    }
}
