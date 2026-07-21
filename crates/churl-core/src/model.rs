use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::assert::Assertion;

/// HTTP request method. Covers the standard REST methods; CONNECT/TRACE can be
/// added later if needed.
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

impl Method {
    /// Every method, in cycle order (GET→POST→PUT→PATCH→DELETE→HEAD→OPTIONS).
    pub const ALL: [Method; 7] = [
        Method::Get,
        Method::Post,
        Method::Put,
        Method::Patch,
        Method::Delete,
        Method::Head,
        Method::Options,
    ];

    /// The next method in cycle order, wrapping OPTIONS→GET.
    pub fn cycle(self) -> Self {
        let idx = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }
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
#[non_exhaustive]
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

/// Serde default for the `enabled` flag on [`Header`] and [`Param`].
fn default_true() -> bool {
    true
}

/// Used to omit `enabled = true` from serialized output.
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

/// Where an [`Auth::ApiKey`] credential is placed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyPlacement {
    /// Sent as a request header (the default).
    #[default]
    Header,
    /// Appended to the URL query string.
    Query,
}

/// Returns whether a placement is the default; used to omit `placement = "header"`
/// from serialized output.
fn is_default_placement(placement: &ApiKeyPlacement) -> bool {
    *placement == ApiKeyPlacement::default()
}

/// Returns whether an ordering key is the default `0`; used to omit `seq = 0`
/// from serialized [`CollectionMeta`] so a var-less collection keeps a minimal
/// (or absent) `folder.toml`.
fn is_zero_seq(seq: &u32) -> bool {
    *seq == 0
}

/// Auth on a request. Persisted as an internally-tagged `[request.auth]` table
/// (`type = "basic" | "bearer" | "apikey"`).
///
/// Secret-valued fields (`password`, `token`, and secret-named api-key values)
/// must hold `{{var}}` placeholders in workspace files, never literals — see
/// [`crate::config::auth_secret_violations`]. The wire effect of each kind is
/// resolved exclusively by [`crate::auth::apply_auth`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Auth {
    /// `type = "basic"` — user/pass, sent as `Authorization: Basic <base64>`.
    Basic {
        /// Basic-auth username (not treated as a secret).
        username: String,
        /// Basic-auth password; a `{{var}}` placeholder in workspace files.
        password: String,
    },
    /// `type = "bearer"` — sent as `Authorization: Bearer <token>`.
    Bearer {
        /// Bearer token; a `{{var}}` placeholder in workspace files.
        token: String,
    },
    /// `type = "apikey"` — arbitrary name/value pair, header or query placement.
    #[serde(rename = "apikey")]
    ApiKey {
        /// Header or query-parameter name, e.g. `X-Api-Key`.
        name: String,
        /// Credential value; a `{{var}}` placeholder when `name` looks secret.
        value: String,
        /// Wire placement; defaults to [`ApiKeyPlacement::Header`] and is omitted
        /// from serialized output when default.
        #[serde(default, skip_serializing_if = "is_default_placement")]
        placement: ApiKeyPlacement,
    },
}

/// A request body: either a simple raw-content body ([`BodyKind`]-tagged text /
/// json / form) or a `multipart/form-data` body made of named [`Part`]s.
///
/// Wire shape (TOML, `[request.body]`): `Simple` serializes exactly as the
/// pre-M8.6 `{ type, content }` shape (`type` one of `text`/`json`/`form`,
/// defaulting to `text` when the key is absent) — every existing endpoint
/// round-trips byte-identical. `Multipart` is `type = "multipart"` plus a
/// `[[request.body.part]]` array (see [`Part`]). The wire adapter lives in
/// [`BodyWire`]/[`PartWire`]; this is the same "extend the existing serde
/// representation" contract [`Auth`] uses for its own `type`-tagged shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "BodyWire", into = "BodyWire")]
pub enum Body {
    /// A raw-content body: text, JSON, or URL-encoded form.
    Simple {
        /// Body kind; stored under the TOML key `type`. Defaults to [`BodyKind::Text`].
        kind: BodyKind,
        /// Raw body content; may contain `{{var}}` template placeholders.
        content: String,
    },
    /// A `multipart/form-data` body: named parts, each inline text or a file
    /// reference resolved and read from disk at send time (never at import or
    /// save — see [`Part`]).
    Multipart(Vec<Part>),
}

/// One named part of a [`Body::Multipart`] body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// The multipart field name; may contain `{{var}}` template placeholders.
    pub name: String,
    /// The part's value: inline text or a file reference.
    pub value: PartValue,
}

/// The value of a multipart [`Part`]: inline text, or a file read from disk at
/// send time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartValue {
    /// Inline text content; may contain `{{var}}` template placeholders and is
    /// scanned by the secret gate (unlike a file's contents, which are not).
    Text(String),
    /// A file reference: the path is stored as given (never read at import or
    /// save) and resolved + read only at send time. Relative paths resolve
    /// against the workspace root with a traversal guard; absolute paths are
    /// allowed as-is. `filename`/`mime` are optional wire overrides for the
    /// part's `Content-Disposition`/`Content-Type`; `path` may contain
    /// `{{var}}` placeholders (templated at send time, same as `filename`).
    File {
        /// The file path as given (relative-to-workspace or absolute).
        path: String,
        /// Optional `Content-Disposition` filename override; defaults to the
        /// path's basename when absent.
        filename: Option<String>,
        /// Optional `Content-Type` override for this part.
        mime: Option<String>,
    },
}

/// Serde wire shape for [`Body`] and [`Part`] — the "extend, don't fork" seam
/// that keeps `Simple` byte-identical to the pre-M8.6 `{ type, content }`
/// representation while adding a `type = "multipart"` variant. See [`Body`]'s
/// docs for the full contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BodyWire {
    #[serde(rename = "type", default)]
    kind: BodyWireKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    part: Vec<PartWire>,
}

/// The wire-level `type` tag for [`BodyWire`]: [`BodyKind`]'s three variants
/// plus `multipart`. Kept distinct from `BodyKind` (rather than adding a
/// `Multipart` variant there) because `BodyKind` is the *simple*-body kind the
/// TUI/`content_type_for` reason about — folding multipart into it would force
/// every `BodyKind` match site to handle a kind with no `content_type_for`
/// mapping of its own (reqwest derives the multipart Content-Type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BodyWireKind {
    #[default]
    Text,
    Json,
    Form,
    Multipart,
}

/// Serde wire shape for one [`Part`]: `name` plus exactly one of `text` /
/// `file`. `file` may carry optional `filename`/`mime`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartWire {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
}

impl From<Body> for BodyWire {
    fn from(body: Body) -> Self {
        match body {
            Body::Simple { kind, content } => BodyWire {
                kind: match kind {
                    BodyKind::Text => BodyWireKind::Text,
                    BodyKind::Json => BodyWireKind::Json,
                    BodyKind::Form => BodyWireKind::Form,
                },
                content: Some(content),
                part: Vec::new(),
            },
            Body::Multipart(parts) => BodyWire {
                kind: BodyWireKind::Multipart,
                content: None,
                part: parts.into_iter().map(PartWire::from).collect(),
            },
        }
    }
}

impl TryFrom<BodyWire> for Body {
    type Error = String;

    fn try_from(wire: BodyWire) -> Result<Self, Self::Error> {
        match wire.kind {
            BodyWireKind::Multipart => {
                let parts = wire
                    .part
                    .into_iter()
                    .map(Part::try_from)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Body::Multipart(parts))
            }
            simple => {
                let kind = match simple {
                    BodyWireKind::Text => BodyKind::Text,
                    BodyWireKind::Json => BodyKind::Json,
                    BodyWireKind::Form => BodyKind::Form,
                    BodyWireKind::Multipart => unreachable!("handled above"),
                };
                let content = wire
                    .content
                    .ok_or_else(|| "body is missing `content`".to_owned())?;
                Ok(Body::Simple { kind, content })
            }
        }
    }
}

impl From<Part> for PartWire {
    fn from(part: Part) -> Self {
        match part.value {
            PartValue::Text(text) => PartWire {
                name: part.name,
                text: Some(text),
                file: None,
                filename: None,
                mime: None,
            },
            PartValue::File {
                path,
                filename,
                mime,
            } => PartWire {
                name: part.name,
                text: None,
                file: Some(path),
                filename,
                mime,
            },
        }
    }
}

impl TryFrom<PartWire> for Part {
    type Error = String;

    fn try_from(wire: PartWire) -> Result<Self, Self::Error> {
        match (wire.text, wire.file) {
            (Some(_), Some(_)) => Err(format!(
                "part {:?} sets both `text` and `file` — exactly one is required",
                wire.name
            )),
            (Some(text), None) => Ok(Part {
                name: wire.name,
                value: PartValue::Text(text),
            }),
            (None, Some(path)) => Ok(Part {
                name: wire.name,
                value: PartValue::File {
                    path,
                    filename: wire.filename,
                    mime: wire.mime,
                },
            }),
            (None, None) => Err(format!(
                "part {:?} sets neither `text` nor `file` — exactly one is required",
                wire.name
            )),
        }
    }
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
    /// Optional first-class auth; omitted from serialized output when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// Durable per-endpoint insecure-TLS opt-in: when `true`, this endpoint's
    /// request goes out with certificate *and* hostname verification off, even
    /// while the session as a whole verifies. The effective insecure for a send is
    /// `endpoint.request.insecure || session_insecure`, so a sibling secure
    /// endpoint in the same session still verifies. Serde-default keeps existing
    /// endpoint files (which lack the key) verifying, and it is omitted from
    /// serialized output when `false` so a secure endpoint stays byte-minimal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
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
    /// Assertions to run against this endpoint's response on `churl run`
    /// (M8.4), persisted as `[[assertions]]`. `churl run` evaluates the
    /// persisted set followed by any CLI `--assert` flags (append); `churl
    /// send` has no persisted endpoint, so it only ever sees CLI flags.
    /// Omitted from serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<Assertion>,
    /// Endpoint-level extraction rules `variable name → expression` (U3),
    /// persisted as `[extract]`. After a **standalone TUI send** completes, each
    /// rule is run against the response via [`crate::sequence::extract_value`] —
    /// the SAME engine sequence steps use — and every rule whose name is listed
    /// in `persist` writes its captured value into the in-memory Session scope, so
    /// a login endpoint can capture a token that a later standalone request reuses
    /// without a sequence. Same field shape/semantics as [`SequenceStep::extract`].
    ///
    /// TUI-only by design: headless `send`/`run` never carry a Session scope across
    /// processes, so endpoint-level extraction is inert there and the frozen M8.2
    /// headless output contract is unaffected. Omitted from serialized output when
    /// empty, so an existing endpoint file without `[extract]` round-trips
    /// byte-identically.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extract: BTreeMap<String, String>,
    /// Names of `extract` rules whose captured value is written into the in-memory
    /// Session store after a standalone TUI send. Rules not listed are inert for a
    /// standalone send (no sequence run to chain them into). Same shape/semantics
    /// as [`SequenceStep::persist`]: the Session store is process-lifetime,
    /// in-memory, and NEVER written to disk — only the rule *name* is persisted
    /// here, never the captured value. Omitted from serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub persist: Vec<String>,
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

/// The **root collection's** metadata: the parsed form of a workspace's
/// `churl.toml`. The workspace root *is* the root collection (M7.9) — there is no
/// separate workspace tier. `churl.toml` therefore carries the root collection's
/// name, its `[vars]` (the lowest collection scope), and the global `[[profiles]]`
/// (a root-only role, like the `sequences/` store). Sub-collections carry only a
/// `folder.toml` ([`CollectionMeta`], vars only — no name, no profiles).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Root-collection name.
    pub name: String,
    /// Root-collection template variables (shared defaults) under a `[vars]`
    /// table; omitted from serialized output when empty. The **outermost**
    /// collection scope in the [`crate::template::Resolver`] chain — the root of
    /// every endpoint's ancestor-chain lookup (env aside). Values must never
    /// contain literal secrets — see [`crate::config::secret_violations`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
    /// Named variable profiles; a global, root-only role (per-collection profiles
    /// are out of scope). Omitted from serialized output when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
    /// Optional per-workspace HTTP/HTTPS proxy URL, applied to every request in
    /// this workspace (overrides the global config proxy; overridden by CLI
    /// `--proxy`). Persisted, so it **must** be credential-free — the save path
    /// refuses a `user:pass@` proxy rather than silently stripping it (see
    /// [`crate::config::proxy_has_credentials`]). Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Whether this workspace opts into the persistent cookie jar (default off).
    /// Seeds the session cookie toggle at open; overrides the global default
    /// upward. Omitted from serialized output when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cookies: bool,
}

/// Optional per-collection metadata: the parsed form of a **sub-collection's**
/// `folder.toml` (`persistence::FOLDER_FILENAME`, a reserved name and never
/// listed as an endpoint). Carries collection-level template variables and an
/// explicit ordering key. The root collection's meta is [`Workspace`], not this.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionMeta {
    /// Explicit ordering key among sibling collections (lower sorts first).
    /// Defaults to `0`; **omitted from serialized output when `0`** so a var-less
    /// collection keeps an empty (or absent) `folder.toml` until it is reordered.
    /// The loader sorts sibling collections by `(seq, name)`, so an all-`0` corpus
    /// (every hand-written / pre-M7.12 collection) keeps its byte-identical
    /// alphabetical order — back-compat by construction, no migration.
    #[serde(default, skip_serializing_if = "is_zero_seq")]
    pub seq: u32,
    /// Collection-level template variables under a `[vars]` table; omitted from
    /// serialized output when empty. A rung in the endpoint's ancestor-chain scope
    /// walk: it overrides its parent collections and the root, and is overridden by
    /// its own children (M7.9 inherit-and-override). Values must never contain
    /// literal secrets — see [`crate::config::collection_secret_violations`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
}

/// What a [`Sequence`] does when a step fails (transport error, HTTP status
/// ≥ 400, or an extraction failure). Persisted as `on_error = "halt" | "continue"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnError {
    /// Stop the run at the first failing step; every later step is `Skipped`.
    /// The default.
    #[default]
    Halt,
    /// Keep running later steps even after a failure.
    Continue,
}

impl OnError {
    /// Serde default for [`Sequence::on_error`] — [`OnError::Halt`].
    pub fn halt() -> Self {
        OnError::Halt
    }
}

/// A request sequence: an ordered run of existing endpoints for end-to-end API
/// testing. Persisted as one `sequences/<slug>.toml` file inside a workspace
/// (`persistence::SEQUENCES_DIRNAME`, a reserved workspace dir — never a
/// collection). Values extracted from a step's response feed later steps through
/// the `{{var}}` resolver chain (see [`crate::template`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sequence {
    /// Explicit ordering key within the `sequences/` dir (lower sorts first).
    /// Defaults to `0` so hand-written files stay minimal.
    #[serde(default)]
    pub seq: u32,
    /// Human-readable sequence name shown in the explorer.
    pub name: String,
    /// Failure policy; defaults to [`OnError::Halt`].
    #[serde(default = "OnError::halt")]
    pub on_error: OnError,
    /// Ordered steps, serialized as `[[step]]` tables; omitted from serialized
    /// output when empty.
    #[serde(default, rename = "step", skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<SequenceStep>,
}

/// One step of a [`Sequence`]: runs an endpoint and optionally extracts values
/// from its response into named variables consumed by later steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceStep {
    /// Explicit ordering key within the sequence (lower runs first). Defaults to
    /// `0`; the in-app editor renumbers on reorder.
    #[serde(default)]
    pub seq: u32,
    /// Workspace-relative path to the endpoint file this step runs, e.g.
    /// `"auth/login.toml"`. Must stay inside the workspace root — `..` and
    /// absolute paths are rejected at run time (see
    /// [`crate::sequence::prepare_step`]).
    pub endpoint: String,
    /// Extraction rules `variable name → expression` (`[step.extract]`), each run
    /// via [`crate::sequence::extract_value`]. Omitted from serialized output when
    /// empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extract: BTreeMap<String, String>,
    /// Names of this step's extraction rules whose captured value is written into
    /// the in-memory Session scope (surviving the run for standalone requests).
    /// Rules not listed stay run-only (ephemeral). Persisted as `persist = [...]`.
    ///
    /// Backward-compatible: a sequence file without `persist` loads with an empty
    /// list, so every rule is Run-only — today's behaviour unchanged. A rule's
    /// target is Session iff its name is in `persist`. The Session store itself is
    /// process-lifetime, in-memory, and NEVER written to disk (a security feature —
    /// captured secrets never touch the filesystem); only the rule *name* list is
    /// persisted here, never the captured value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub persist: Vec<String>,
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
    fn method_cycle_wraps() {
        assert_eq!(Method::Get.cycle(), Method::Post);
        assert_eq!(Method::Post.cycle(), Method::Put);
        assert_eq!(Method::Options.cycle(), Method::Get);
        // Cycling through all seven returns to the start.
        let mut m = Method::Get;
        for _ in 0..7 {
            m = m.cycle();
        }
        assert_eq!(m, Method::Get);
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
    fn auth_toml_round_trip_per_kind() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Wrapper {
            auth: Auth,
        }
        let cases = [
            Auth::Basic {
                username: "alice".into(),
                password: "{{password}}".into(),
            },
            Auth::Bearer {
                token: "{{token}}".into(),
            },
            Auth::ApiKey {
                name: "X-Api-Key".into(),
                value: "{{api_key}}".into(),
                placement: ApiKeyPlacement::Header,
            },
            Auth::ApiKey {
                name: "api_key".into(),
                value: "{{api_key}}".into(),
                placement: ApiKeyPlacement::Query,
            },
        ];
        for auth in cases {
            let wrapper = Wrapper { auth };
            let toml = toml_edit::ser::to_string(&wrapper)
                .unwrap_or_else(|err| panic!("serialize failed for {wrapper:?}: {err}"));
            let back: Wrapper = toml_edit::de::from_str(&toml)
                .unwrap_or_else(|err| panic!("deserialize failed for {toml:?}: {err}"));
            assert_eq!(back, wrapper, "round-trip mismatch via:\n{toml}");
        }
    }

    #[test]
    fn auth_default_placement_is_skipped_on_serialize() {
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            auth: Auth,
        }
        let header = toml_edit::ser::to_string(&Wrapper {
            auth: Auth::ApiKey {
                name: "X-Api-Key".into(),
                value: "{{k}}".into(),
                placement: ApiKeyPlacement::Header,
            },
        })
        .unwrap();
        assert!(
            !header.contains("placement"),
            "default placement must be omitted:\n{header}"
        );
        assert!(header.contains(r#"type = "apikey""#), "{header}");

        let query = toml_edit::ser::to_string(&Wrapper {
            auth: Auth::ApiKey {
                name: "k".into(),
                value: "v".into(),
                placement: ApiKeyPlacement::Query,
            },
        })
        .unwrap();
        assert!(query.contains(r#"placement = "query""#), "{query}");

        // A missing placement key deserializes to the header default.
        let back: Wrapper =
            toml_edit::de::from_str("[auth]\ntype = \"apikey\"\nname = \"k\"\nvalue = \"v\"\n")
                .unwrap();
        assert_eq!(
            back.auth,
            Auth::ApiKey {
                name: "k".into(),
                value: "v".into(),
                placement: ApiKeyPlacement::Header,
            }
        );
    }

    #[test]
    fn body_kind_lowercase_and_type_key() {
        let body: Body = toml_edit::de::from_str("type = \"json\"\ncontent = \"{}\"\n").unwrap();
        assert_eq!(
            body,
            Body::Simple {
                kind: BodyKind::Json,
                content: "{}".into()
            }
        );

        let missing_type: Body = toml_edit::de::from_str("content = \"hi\"\n").unwrap();
        assert_eq!(
            missing_type,
            Body::Simple {
                kind: BodyKind::Text,
                content: "hi".into()
            }
        );

        let toml = toml_edit::ser::to_string(&body).unwrap();
        assert!(toml.contains("type = \"json\""));
    }

    /// `Simple` must serialize byte-identical to the pre-M8.6 `{ type, content }`
    /// shape so every existing endpoint round-trips unchanged — the M8.6 model
    /// split's core back-compat contract.
    #[test]
    fn body_simple_toml_shape_is_byte_identical_to_pre_multipart() {
        let body = Body::Simple {
            kind: BodyKind::Json,
            content: r#"{"name":"ada"}"#.into(),
        };
        let toml = toml_edit::ser::to_string(&body).unwrap();
        assert_eq!(toml, "type = \"json\"\ncontent = '{\"name\":\"ada\"}'\n");
    }

    #[test]
    fn body_multipart_round_trip_toml() {
        let body = Body::Multipart(vec![
            Part {
                name: "field".into(),
                value: PartValue::Text("hello".into()),
            },
            Part {
                name: "upload".into(),
                value: PartValue::File {
                    path: "assets/report.pdf".into(),
                    filename: Some("report.pdf".into()),
                    mime: Some("application/pdf".into()),
                },
            },
        ]);
        // Bare `to_string` renders `part` as an inline array of inline
        // tables (`part = [{ ... }, { ... }]`) — logically identical TOML,
        // just not the `[[request.body.part]]` block-array-of-tables shape a
        // saved workspace file actually gets. That block shape is produced
        // by `persistence::atomic::normalize_table`'s post-pass over
        // `save_value`'s freshly-serialized document — see
        // `persistence.rs`'s `multipart_body_round_trips_through_save_and_reads_as_array_of_tables`
        // for that real-path assertion. This unit test only needs the wire
        // KEYS and round-trip data fidelity, not the array syntax.
        let toml = toml_edit::ser::to_string(&body).unwrap();
        assert!(toml.contains("type = \"multipart\""), "{toml}");
        assert!(toml.contains("name = \"field\""), "{toml}");
        assert!(toml.contains("text = \"hello\""), "{toml}");
        assert!(toml.contains("name = \"upload\""), "{toml}");
        assert!(toml.contains("file = \"assets/report.pdf\""), "{toml}");
        assert!(toml.contains("filename = \"report.pdf\""), "{toml}");
        assert!(toml.contains("mime = \"application/pdf\""), "{toml}");
        // `content` never appears on a multipart body.
        assert!(!toml.contains("content"), "{toml}");

        let back: Body = toml_edit::de::from_str(&toml).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn body_multipart_minimal_text_only_part() {
        let toml = "type = \"multipart\"\n\n[[part]]\nname = \"field\"\ntext = \"v\"\n";
        let body: Body = toml_edit::de::from_str(toml).unwrap();
        assert_eq!(
            body,
            Body::Multipart(vec![Part {
                name: "field".into(),
                value: PartValue::Text("v".into()),
            }])
        );
    }

    #[test]
    fn body_part_rejects_both_text_and_file() {
        let toml = "type = \"multipart\"\n\n[[part]]\nname = \"x\"\ntext = \"a\"\nfile = \"b\"\n";
        let err = toml_edit::de::from_str::<Body>(toml).unwrap_err();
        assert!(err.to_string().contains("both"), "{err}");
    }

    #[test]
    fn body_part_rejects_neither_text_nor_file() {
        let toml = "type = \"multipart\"\n\n[[part]]\nname = \"x\"\n";
        let err = toml_edit::de::from_str::<Body>(toml).unwrap_err();
        assert!(err.to_string().contains("neither"), "{err}");
    }

    #[test]
    fn body_simple_missing_content_key_errors() {
        let err = toml_edit::de::from_str::<Body>("type = \"json\"\n").unwrap_err();
        assert!(err.to_string().contains("content"), "{err}");
    }

    #[test]
    fn body_multipart_file_part_omits_optional_filename_and_mime() {
        let body = Body::Multipart(vec![Part {
            name: "upload".into(),
            value: PartValue::File {
                path: "a.txt".into(),
                filename: None,
                mime: None,
            },
        }]);
        let toml = toml_edit::ser::to_string(&body).unwrap();
        assert!(!toml.contains("filename"), "{toml}");
        assert!(!toml.contains("mime"), "{toml}");
        let back: Body = toml_edit::de::from_str(&toml).unwrap();
        assert_eq!(back, body);
    }
}
