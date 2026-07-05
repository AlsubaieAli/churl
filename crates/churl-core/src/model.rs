use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// HTTP request method.
///
/// Covers the standard methods used in REST APIs. Additional methods (e.g. CONNECT, TRACE)
/// can be added in later milestones if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
        };
        f.write_str(s)
    }
}

/// Error returned when a string cannot be parsed as an HTTP [`Method`].
#[derive(Debug, thiserror::Error)]
#[error("unknown HTTP method: {0:?}")]
pub struct ParseMethodError(String);

impl std::str::FromStr for Method {
    type Err = ParseMethodError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "GET" => Ok(Method::Get),
            "POST" => Ok(Method::Post),
            "PUT" => Ok(Method::Put),
            "PATCH" => Ok(Method::Patch),
            "DELETE" => Ok(Method::Delete),
            "HEAD" => Ok(Method::Head),
            "OPTIONS" => Ok(Method::Options),
            _ => Err(ParseMethodError(s.to_owned())),
        }
    }
}

impl Serialize for Method {
    /// Serializes as the upper-case method string (e.g. `"GET"`), matching [`Method::to_string`].
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Method {
    /// Deserializes from a method string via [`Method::from_str`] (case-insensitive).
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Returns `true`; serde default for the `enabled` flag on [`Header`] and [`Param`].
fn default_true() -> bool {
    true
}

/// Returns whether a bool is `true`; used to omit `enabled = true` from serialized output.
fn is_true(b: &bool) -> bool {
    *b
}

/// A single HTTP header line on a request or response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// Header name, e.g. `Content-Type`.
    pub name: String,
    /// Header value; may contain `{{var}}` template placeholders.
    pub value: String,
    /// Whether the header is sent. Defaults to `true` and is omitted from serialized
    /// output when true, so only disabled headers carry the flag on disk.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

/// A single URL query parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    /// Parameter name.
    pub name: String,
    /// Parameter value; may contain `{{var}}` template placeholders.
    pub value: String,
    /// Whether the parameter is sent. Defaults to `true` and is omitted from serialized
    /// output when true, so only disabled parameters carry the flag on disk.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
}

/// The kind of a request [`Body`], controlling content type and rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyKind {
    /// Plain text body.
    #[default]
    Text,
    /// JSON body.
    Json,
    /// URL-encoded form body.
    Form,
}

/// A request body: raw content plus its [`BodyKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    /// Body kind; stored under the TOML key `type`. Defaults to [`BodyKind::Text`].
    #[serde(rename = "type", default)]
    pub kind: BodyKind,
    /// Raw body content; may contain `{{var}}` template placeholders.
    pub content: String,
}

/// An HTTP request definition: everything needed to execute a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// HTTP method.
    pub method: Method,
    /// Target URL; may contain `{{var}}` template placeholders.
    pub url: String,
    /// Request headers; omitted from serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
    /// URL query parameters; omitted from serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Param>,
    /// Optional request body; omitted from serialized output when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
}

/// A saved endpoint: one `.toml` file inside a collection directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Explicit ordering key within a collection (lower sorts first). Defaults to `0`
    /// when missing so hand-written files stay minimal.
    #[serde(default)]
    pub seq: u32,
    /// Human-readable endpoint name shown in the explorer.
    pub name: String,
    /// The request this endpoint executes.
    pub request: Request,
}

/// A named set of template variables, selectable at request time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Profile name, e.g. `dev` or `prod`.
    pub name: String,
    /// Variable name → value map used for `{{var}}` substitution. Values must never
    /// contain secrets — see [`crate::config::secret_violations`].
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

/// A workspace manifest: the parsed form of a workspace's `churl.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Workspace name.
    pub name: String,
    /// Named variable profiles; omitted from serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
}

/// An executed HTTP response. Runtime-only: never persisted to TOML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<Header>,
    /// Raw response body bytes. When `truncated` is set, this holds exactly the
    /// first `max_body_bytes` of the wire body (see [`crate::http::ExecuteOptions`]).
    pub body: Vec<u8>,
    /// Whether the body was cut off at the configured size cap.
    pub truncated: bool,
    /// Coarse request timing.
    pub timing: Timing,
}

/// Coarse timing for an executed request. Runtime-only: never persisted to TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Time to establish the connection, when measurable.
    pub connect: Option<Duration>,
    /// Total wall-clock time from send to last byte.
    pub total: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn method_round_trip() {
        let methods = [
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Patch,
            Method::Delete,
            Method::Head,
            Method::Options,
        ];
        for method in methods {
            let displayed = method.to_string();
            let parsed = Method::from_str(&displayed)
                .unwrap_or_else(|_| panic!("failed to parse back: {displayed}"));
            assert_eq!(method, parsed, "round-trip failed for {method}");
        }
    }

    #[test]
    fn method_parse_case_insensitive() {
        assert_eq!(Method::from_str("get").unwrap(), Method::Get);
        assert_eq!(Method::from_str("Post").unwrap(), Method::Post);
    }

    #[test]
    fn method_parse_unknown_errors() {
        assert!(Method::from_str("CONNECT").is_err());
    }

    #[test]
    fn method_serde_round_trip() {
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            method: Method,
        }
        let toml = toml_edit::ser::to_string(&Wrapper {
            method: Method::Delete,
        })
        .unwrap();
        assert_eq!(toml.trim(), r#"method = "DELETE""#);
        let back: Wrapper = toml_edit::de::from_str(&toml).unwrap();
        assert_eq!(back.method, Method::Delete);
    }

    #[test]
    fn header_enabled_defaults_true_and_is_skipped() {
        let header: Header = toml_edit::de::from_str("name = \"X\"\nvalue = \"1\"\n").unwrap();
        assert!(header.enabled);

        let toml = toml_edit::ser::to_string(&header).unwrap();
        assert!(!toml.contains("enabled"), "enabled=true must be omitted");

        let disabled = Header {
            enabled: false,
            ..header
        };
        let toml = toml_edit::ser::to_string(&disabled).unwrap();
        assert!(toml.contains("enabled = false"));
    }

    #[test]
    fn body_kind_lowercase_and_type_key() {
        let body: Body = toml_edit::de::from_str("type = \"json\"\ncontent = \"{}\"\n").unwrap();
        assert_eq!(body.kind, BodyKind::Json);

        let missing_type: Body = toml_edit::de::from_str("content = \"hi\"\n").unwrap();
        assert_eq!(missing_type.kind, BodyKind::Text);

        let toml = toml_edit::ser::to_string(&body).unwrap();
        assert!(toml.contains("type = \"json\""));
    }
}
