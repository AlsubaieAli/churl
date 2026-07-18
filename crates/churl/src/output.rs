//! M8.2 machine-output contract: the frozen JSON envelope every headless
//! subcommand (`run`, `send`, `import`) emits on `--json`, plus the closed
//! `ErrorKind` slug set and its 1:1 exit-code mapping.
//!
//! **The envelope is a compatibility surface — treat it like an on-disk
//! format.** [`SCHEMA_VERSION`] bumps ONLY on a breaking change (a field
//! removed, renamed, or its meaning changed); adding a new optional field or a
//! new [`ErrorKind`] variant never bumps it. `ok` always mirrors the process
//! exit code exactly (`ok: true` ⟺ exit 0). `data` is `null` on any hard
//! failure. `error.kind` is the stable slug an agent branches on;
//! `error.message` is free text for humans and must never be parsed.
//!
//! Exit codes (frozen forever): `0` success · `1` assertion failure (RESERVED,
//! unused until M8.4) · `2` usage error — owned entirely by clap's own parser
//! (missing/conflicting args, unknown flags to `churl` itself); this module
//! never constructs an envelope for a band-2 failure, matching "clap default —
//! don't remap" · `3` workspace/resolution error · `4` request/transport error
//! · `5` input/import error.
//!
//! stdout carries ONLY the envelope in `--json` mode — every log, warning, and
//! human nicety goes to stderr (see each subcommand's human-mode printing).

use serde::Serialize;

/// Current envelope schema version. See the module docs for the bump rule.
pub const SCHEMA_VERSION: u32 = 1;

/// The one JSON object a `--json` invocation prints to stdout.
#[derive(Debug, Serialize)]
pub struct Envelope<'a, T: Serialize> {
    pub schema_version: u32,
    pub ok: bool,
    pub command: &'a str,
    pub data: Option<T>,
    pub error: Option<EnvelopeError>,
}

/// The `error` object of a failed envelope.
#[derive(Debug, Serialize)]
pub struct EnvelopeError {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Closed set of stable failure slugs. Agents branch on `kind`, never on
/// `message`. Each variant maps 1:1 to an exit-code band via
/// [`ErrorKind::exit_code`]; new variants may be added later (additive, no
/// schema bump) but existing ones never change their band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    // --- band 3: workspace/resolution error ---
    /// No `churl.toml` workspace found at the current directory (or an
    /// ancestor scan is not performed — churl workspaces are cwd-rooted).
    NoWorkspace,
    /// A `collection/.../endpoint name` path did not resolve to an endpoint
    /// file in the open workspace.
    EndpointNotFound,
    /// A `run-seq <name>` argument did not resolve to a `sequences/<name>.toml`
    /// file in the open workspace (M8.4.1). Additive per the module docs'
    /// "new `ErrorKind` variants are additive" rule.
    SequenceNotFound,
    /// The resolved request still carries one or more `{{var}}` placeholders
    /// after substitution — refused rather than shipping a literal `{{...}}`.
    UnresolvedVar,
    /// `--profile NAME` named a profile the workspace manifest does not define.
    UnknownProfile,
    /// The global config file could not be loaded or parsed (unreadable,
    /// malformed TOML, or an invalid knob value such as `redirect`), or the
    /// current working directory could not be determined. Pre-flight resolution
    /// that fails before the request is even shaped — grouped with the other
    /// resolution failures in band 3.
    ConfigError,

    // --- band 4: request/transport error (from `churl_core::http::HttpError`) ---
    /// The request URL could not be parsed.
    InvalidUrl,
    /// The request timed out.
    Timeout,
    /// The request failed for any other transport reason (DNS, connect, TLS,
    /// protocol).
    TransportError,

    // --- band 5: input/import error ---
    /// The given input could not be parsed as a curl command (includes: no
    /// input given on a non-piped stdin, tokenize failure, missing/duplicate
    /// URL, unknown flag, unsupported construct, invalid `-X` method).
    NotACurlCommand,
    /// The curl command parsed, but writing the resulting endpoint failed
    /// (e.g. a newly-authored literal secret was refused, or a disk error).
    ImportWriteFailed,
    /// A `--assert` flag did not parse (unknown operator, missing value for a
    /// value-op, empty target) — a usage/input mistake, not a request
    /// failure, hence band 5 alongside the other input-shape errors. Additive
    /// (M8.4): a schema-compatible new closed-enum variant, per the module
    /// docs' "new `ErrorKind` variants are additive" rule.
    InvalidAssertion,
    /// A `churl load` run's `--total`/`--concurrency` exceeded the `[load]`
    /// hard cap (`LoadCaps`) — refused pre-flight before any request is fired,
    /// mirroring the TUI's own hard ceiling. A usage/input mistake (the caller
    /// asked for more than the configured maximum), hence band 5. Additive
    /// (M8.4.2). Raise `[load] max_total`/`max_concurrency` to allow it.
    LoadCapExceeded,
}

impl ErrorKind {
    /// The exit-code band this `kind` belongs to (frozen — see module docs).
    pub const fn exit_code(self) -> i32 {
        match self {
            ErrorKind::NoWorkspace
            | ErrorKind::EndpointNotFound
            | ErrorKind::SequenceNotFound
            | ErrorKind::UnresolvedVar
            | ErrorKind::UnknownProfile
            | ErrorKind::ConfigError => 3,
            ErrorKind::InvalidUrl | ErrorKind::Timeout | ErrorKind::TransportError => 4,
            ErrorKind::NotACurlCommand
            | ErrorKind::ImportWriteFailed
            | ErrorKind::InvalidAssertion
            | ErrorKind::LoadCapExceeded => 5,
        }
    }
}

/// A successful payload's process exit code — almost always `0`, matching
/// the documented "`ok` mirrors the exit code" rule. [`crate::headless::ExecData`]
/// is the sole exception: a request that executed fine but whose assertions
/// failed still prints success-shaped JSON (`ok: true`, `data` populated) yet
/// exits **1** (see `docs/CLI.md`, "Assertions" and "Exit codes"). Every other
/// success payload (`ImportData`, …) uses the default.
pub trait SuccessExitCode {
    /// The process exit code for a *successful* result. Defaults to `0`.
    fn success_exit_code(&self) -> i32 {
        0
    }
}

/// A headless-subcommand failure: an [`ErrorKind`] slug, a human message, and
/// optional structured detail. Every headless command builds a
/// `Result<T, CliError>` and hands it to [`emit`] to print (JSON or human) and
/// resolve the process exit code — the single seam so every subcommand's
/// stdout/stderr/exit-code triad stays consistent with the frozen contract.
#[derive(Debug)]
pub struct CliError {
    pub kind: ErrorKind,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

impl CliError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(
        kind: ErrorKind,
        message: impl Into<String>,
        detail: serde_json::Value,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: Some(detail),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CliError {}

/// Maps a core [`churl_core::http::HttpError`] onto a [`CliError`] (band 4).
pub fn from_http_error(err: churl_core::http::HttpError) -> CliError {
    use churl_core::http::HttpError;
    match err {
        HttpError::InvalidUrl { url, reason } => {
            // Mask any embedded secret before it lands in the message + detail —
            // a malformed URL can still carry `user:pass@` / `?api_key=…`, and
            // the failure surface must not leak what the success surface masks.
            let masked = churl_core::secrets::mask_url(&url);
            CliError::with_detail(
                ErrorKind::InvalidUrl,
                format!("invalid URL {masked:?}: {reason}"),
                serde_json::json!({ "url": masked, "reason": reason }),
            )
        }
        HttpError::Timeout => CliError::new(ErrorKind::Timeout, "request timed out"),
        HttpError::Request(source) => {
            // reqwest's `Display` embeds the request URL (query string included),
            // so mask any secret in it before it reaches `message`/`detail`. A
            // connection/DNS/TLS failure is the *common* failure mode, and it
            // must not leak what the success + `InvalidUrl` surfaces already mask
            // (`?api_key=…`). reqwest strips userinfo from the URL it retains, so
            // the query is the live vector here.
            let mut message = format!("request failed: {source}");
            let detail = if let Some(u) = source.url() {
                let raw = u.as_str();
                let masked = churl_core::secrets::mask_url(raw);
                if masked != raw {
                    message = message.replace(raw, &masked);
                }
                Some(serde_json::json!({ "url": masked }))
            } else {
                None
            };
            match detail {
                Some(d) => CliError::with_detail(ErrorKind::TransportError, message, d),
                None => CliError::new(ErrorKind::TransportError, message),
            }
        }
        // `HttpError` is `#[non_exhaustive]`: any future variant still lands
        // in the transport band rather than failing to compile.
        other => CliError::new(ErrorKind::TransportError, other.to_string()),
    }
}

/// Prints the result of a headless command and returns the process exit code.
///
/// - `--json`: prints exactly one [`Envelope`] to stdout (success or failure),
///   nothing else on stdout.
/// - Human mode: calls `human_ok` on success (the subcommand's own
///   human-readable rendering); on failure prints `error: <message>` to
///   stderr. Never prints both JSON and human output for the same run.
///
/// A successful result's exit code comes from [`T::success_exit_code`]
/// ([`SuccessExitCode`]) — `0` for every payload except a `run`/`send` whose
/// assertions failed (see the trait docs).
pub fn emit<T: Serialize + SuccessExitCode>(
    command: &str,
    json: bool,
    result: Result<T, CliError>,
    human_ok: impl FnOnce(&T),
) -> i32 {
    match result {
        Ok(data) => {
            // Computed before `data` is moved into the envelope below.
            let code = data.success_exit_code();
            if json {
                print_envelope(Envelope {
                    schema_version: SCHEMA_VERSION,
                    ok: true,
                    command,
                    data: Some(data),
                    error: None,
                });
            } else {
                human_ok(&data);
            }
            code
        }
        Err(err) => emit_error(command, json, err),
    }
}

/// Prints a single failure envelope (`--json`) or `error: <message>` (human
/// mode) and returns its exit-code band. Factored out of [`emit`] so a
/// streaming command (`run-seq`) can surface a *pre-flight* failure — one that
/// occurs before its NDJSON stream starts (no workspace, sequence not found) —
/// as the very same single-object error envelope every other headless command
/// emits. A per-step failure rides the stream instead (see `crate::seq_cmd`).
pub fn emit_error(command: &str, json: bool, err: CliError) -> i32 {
    let code = err.kind.exit_code();
    if json {
        print_envelope(Envelope::<()> {
            schema_version: SCHEMA_VERSION,
            ok: false,
            command,
            data: None,
            error: Some(EnvelopeError {
                kind: err.kind,
                message: err.message,
                detail: err.detail,
            }),
        });
    } else {
        eprintln!("error: {}", err.message);
    }
    code
}

fn print_envelope<T: Serialize>(envelope: Envelope<T>) {
    match serde_json::to_string(&envelope) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            // Serialization of our own well-typed envelope should never fail;
            // if it somehow does, fail loud on stderr rather than emit a
            // truncated/invalid JSON line on stdout.
            eprintln!("error: failed to serialize output envelope: {err}");
        }
    }
}

pub fn mask_header_value(name: &str, value: &str) -> String {
    churl_core::secrets::mask_header_value(name, value)
}
