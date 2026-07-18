//! Request-sequence run engine and value extraction.
//!
//! This module is the single source of truth for *how a sequence runs*: the same
//! primitives ([`prepare_step`], [`classify_step`], [`extract_step`]) drive both
//! the wiremock-tested [`run_sequence`] convenience and the live TUI runner, so
//! their semantics can never drift.
//!
//! The resolver stays the single `{{var}}` seam (the plugin guardrail): a step
//! is prepared by *prepending* an ephemeral `"extracted"` [`Scope`] and the
//! in-memory `"session"` [`Scope`] to the canonical scope chain — resolution is
//! never forked. Extracted values win over ambient config so a chained value is
//! never shadowed by a same-named workspace var; the Session scope sits
//! just below `extracted` so a previously-captured token resolves during a run
//! too, but a same-run extraction still wins.
//!
//! # Extraction expression grammar (a documented, dependency-free subset)
//!
//! [`extract_value`] intentionally supports a *small* grammar over `serde_json`
//! — no JSONPath crate:
//!
//! - `status` — the numeric HTTP status as a string, e.g. `"200"`.
//! - `header:<Name>` — a response header value, matched case-insensitively; an
//!   absent header is an error.
//! - `$.a.b`, `$.a.b[0].c`, `a.b[2]` — a JSON path (the leading `$.` is optional).
//!   The body is parsed as JSON and walked segment by segment: `.key` indexes an
//!   object, `[n]` indexes an array. The leaf is coerced to a string — a JSON
//!   string yields its value, a number/bool its display form, an object/array its
//!   compact JSON. A `null` leaf, a missing key, an out-of-range index, a type
//!   mismatch, a non-JSON body, or a malformed expression are all clear errors.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::assert::Assertion;
use crate::debug::{DebugTrace, VarStep};
use crate::http::{ExecuteOptions, HttpError, execute_traced};
use crate::model::{Method, OnError, Response, Sequence, SequenceStep, Timing};
use crate::persistence::{PersistenceError, load_collection_meta, load_endpoint};
use crate::template::{Resolver, Scope};

/// Error extracting a value from a response via [`extract_value`]. Every variant
/// names the offending expression and the reason.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtractError {
    /// A `header:<Name>` expression named a header the response does not carry.
    #[error("extract {expr:?}: response has no header {name:?}")]
    HeaderMissing {
        /// The full expression.
        expr: String,
        /// The header name that was requested.
        name: String,
    },
    /// A JSON-path expression, but the response body is not valid JSON.
    #[error("extract {expr:?}: response body is not valid JSON: {reason}")]
    NotJson {
        /// The full expression.
        expr: String,
        /// The JSON parse-error message.
        reason: String,
    },
    /// The expression could not be parsed as a supported path.
    #[error("extract {expr:?}: malformed expression: {reason}")]
    BadExpr {
        /// The full expression.
        expr: String,
        /// Why parsing failed.
        reason: String,
    },
    /// A `.key` segment named a key absent from the object at that point.
    #[error("extract {expr:?}: no such key {key:?}")]
    MissingKey {
        /// The full expression.
        expr: String,
        /// The key that was not found.
        key: String,
    },
    /// An `[n]` segment indexed past the end of the array at that point.
    #[error("extract {expr:?}: array index {index} out of range (length {len})")]
    IndexOutOfRange {
        /// The full expression.
        expr: String,
        /// The requested index.
        index: usize,
        /// The array's length.
        len: usize,
    },
    /// A segment expected an object (for `.key`) or an array (for `[n]`) but the
    /// value at that point was something else.
    #[error("extract {expr:?}: cannot apply {segment} to a {found}")]
    TypeMismatch {
        /// The full expression.
        expr: String,
        /// The segment being applied, e.g. `.name` or `[0]`.
        segment: String,
        /// The JSON type actually found, e.g. `string` or `array`.
        found: String,
    },
    /// The leaf value has no sensible string form (a JSON `null`).
    #[error("extract {expr:?}: leaf value is null (cannot extract)")]
    NullLeaf {
        /// The full expression.
        expr: String,
    },
}

/// Extracts a single value from `response` using the [module grammar](self).
pub fn extract_value(response: &Response, expr: &str) -> Result<String, ExtractError> {
    let expr = expr.trim();

    if expr == "status" {
        return Ok(response.status.to_string());
    }

    if let Some(name) = expr.strip_prefix("header:") {
        let name = name.trim();
        return response
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.clone())
            .ok_or_else(|| ExtractError::HeaderMissing {
                expr: expr.to_owned(),
                name: name.to_owned(),
            });
    }

    // Otherwise a JSON path.
    let value: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|err| ExtractError::NotJson {
            expr: expr.to_owned(),
            reason: err.to_string(),
        })?;
    let segments = parse_path(expr).map_err(|reason| ExtractError::BadExpr {
        expr: expr.to_owned(),
        reason,
    })?;
    let leaf = walk(&value, &segments, expr)?;
    stringify_leaf(leaf, expr)
}

/// One segment of a parsed JSON path.
#[derive(Debug, PartialEq, Eq)]
enum PathSeg {
    /// An object key (`.name`).
    Key(String),
    /// An array index (`[n]`).
    Index(usize),
}

impl std::fmt::Display for PathSeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathSeg::Key(key) => write!(f, ".{key}"),
            PathSeg::Index(index) => write!(f, "[{index}]"),
        }
    }
}

/// Parses a JSON-path expression (leading `$.` optional) into segments. Never
/// panics — byte scanning only breaks on the ASCII delimiters `.` `[` `]`, so
/// every slice lands on a char boundary and unicode keys are preserved.
fn parse_path(expr: &str) -> Result<Vec<PathSeg>, String> {
    let s = expr.strip_prefix('$').unwrap_or(expr);
    let bytes = s.as_bytes();
    let mut segments = Vec::new();
    let mut i = 0;
    while i < s.len() {
        match bytes[i] {
            b'.' => {
                i += 1;
                let start = i;
                while i < s.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                    i += 1;
                }
                if i == start {
                    return Err("empty key after '.'".to_owned());
                }
                segments.push(PathSeg::Key(s[start..i].to_owned()));
            }
            b'[' => {
                i += 1;
                let start = i;
                while i < s.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i >= s.len() {
                    return Err("unclosed '['".to_owned());
                }
                let inner = &s[start..i];
                let index = inner
                    .parse::<usize>()
                    .map_err(|_| format!("invalid array index {inner:?}"))?;
                i += 1; // consume ']'
                segments.push(PathSeg::Index(index));
            }
            b']' => return Err("unexpected ']'".to_owned()),
            _ => {
                // A bare leading key with no `.`, e.g. `a.b[2]`.
                let start = i;
                while i < s.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                    i += 1;
                }
                segments.push(PathSeg::Key(s[start..i].to_owned()));
            }
        }
    }
    if segments.is_empty() {
        return Err("empty path".to_owned());
    }
    Ok(segments)
}

/// Walks `segments` over `value`, returning the leaf or a clear error.
fn walk<'a>(
    value: &'a serde_json::Value,
    segments: &[PathSeg],
    expr: &str,
) -> Result<&'a serde_json::Value, ExtractError> {
    let mut current = value;
    for segment in segments {
        current = match segment {
            PathSeg::Key(key) => match current {
                serde_json::Value::Object(map) => {
                    map.get(key).ok_or_else(|| ExtractError::MissingKey {
                        expr: expr.to_owned(),
                        key: key.clone(),
                    })?
                }
                other => {
                    return Err(ExtractError::TypeMismatch {
                        expr: expr.to_owned(),
                        segment: segment.to_string(),
                        found: json_type(other).to_owned(),
                    });
                }
            },
            PathSeg::Index(index) => match current {
                serde_json::Value::Array(items) => {
                    items
                        .get(*index)
                        .ok_or_else(|| ExtractError::IndexOutOfRange {
                            expr: expr.to_owned(),
                            index: *index,
                            len: items.len(),
                        })?
                }
                other => {
                    return Err(ExtractError::TypeMismatch {
                        expr: expr.to_owned(),
                        segment: segment.to_string(),
                        found: json_type(other).to_owned(),
                    });
                }
            },
        };
    }
    Ok(current)
}

/// Coerces a JSON leaf to its extracted string form (see the module grammar).
fn stringify_leaf(value: &serde_json::Value, expr: &str) -> Result<String, ExtractError> {
    match value {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            Ok(serde_json::to_string(value).unwrap_or_default())
        }
        serde_json::Value::Null => Err(ExtractError::NullLeaf {
            expr: expr.to_owned(),
        }),
    }
}

/// The JSON type name of a value, for error messages.
fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// The ambient variable scopes a sequence run resolves against, below the
/// ephemeral extracted scope: the in-memory Session captures, CLI `--var` flags,
/// the active profile, and the workspace `[vars]`. (The per-endpoint collection
/// scope is loaded fresh in [`prepare_step`] from each step's own collection.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunScopes {
    /// In-memory Session captures: values previously extracted by a
    /// Session-target rule, surviving the run. Sits just below `extracted` and
    /// above `cli` so a same-named ambient var never shadows a captured token; a
    /// same-run extraction (`extracted`) still wins. Process-lifetime, in-memory,
    /// never persisted to disk.
    pub session: BTreeMap<String, String>,
    /// CLI `--var` overrides.
    pub cli: BTreeMap<String, String>,
    /// The active profile's vars.
    pub profile: BTreeMap<String, String>,
    /// Root-collection `[vars]` — the `churl.toml` `[vars]` (M7.9: the root
    /// collection replaced the old separate workspace scope). The outermost
    /// collection rung, appended after the step's ancestor chain (before process
    /// env). The field name is kept for API stability.
    pub workspace: BTreeMap<String, String>,
}

/// A step resolved and ready to [`execute`]: the endpoint file it came from plus
/// its fully-substituted request and its persisted assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PreparedStep {
    /// The endpoint file this step runs.
    pub endpoint_path: PathBuf,
    /// The request after `{{var}}` substitution (a resolved clone; disk untouched).
    pub request: crate::model::Request,
    /// The request method (mirrors `request.method` for convenience).
    pub method: Method,
    /// The resolved request URL (mirrors `request.url` for convenience).
    pub url: String,
    /// The endpoint's persisted `[[assertions]]` (M8.4), carried through so a
    /// headless sequence run can gate each step against its own endpoint's
    /// checks without re-loading the file. The live TUI runner ignores this
    /// field (it does not yet evaluate sequence assertions).
    pub assertions: Vec<Assertion>,
}

/// Error preparing a sequence step (path resolution / endpoint load). The step
/// fails; the run never panics and never escapes the workspace root.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SequenceError {
    /// The step's `endpoint` used `..` or an absolute path — refused so a step can
    /// never escape the workspace root.
    #[error(
        "step endpoint {endpoint:?} escapes the workspace root (`..` and absolute paths are not allowed)"
    )]
    Traversal {
        /// The offending `endpoint` string.
        endpoint: String,
    },
    /// After substitution the step's request still carried one or more `{{var}}`
    /// placeholders no scope resolved — sending it would ship a literal `{{var}}`.
    /// The step fails loudly instead (honouring the sequence's `on_error`).
    #[error(
        "unresolved variable(s): {} — set them in a profile/env or via CLI",
        names.join(", ")
    )]
    Unresolved {
        /// The unresolved placeholder names (sorted, deduped).
        names: Vec<String>,
    },
    /// The endpoint file could not be loaded (missing or unparseable) or its
    /// collection metadata could not be read.
    #[error("step endpoint {endpoint:?}: {source}")]
    Endpoint {
        /// The step's `endpoint` string.
        endpoint: String,
        /// The underlying persistence error (boxed — it is large relative to the
        /// other variants).
        #[source]
        source: Box<PersistenceError>,
    },
}

/// Resolves a step's `endpoint` against the workspace root, rejecting any path
/// that would escape it. Two layers: a lexical pre-check (rejects `..`, absolute
/// paths, drive prefixes) plus a canonical containment check that also catches an
/// in-workspace symlink pointing outside the root. A missing endpoint file is
/// *not* a traversal — its deepest existing ancestor still resolves inside the
/// root, so the load fails with a not-found error, keeping the two failure kinds
/// distinct.
fn resolve_step_path(root: &Path, endpoint: &str) -> Result<PathBuf, SequenceError> {
    let rel = Path::new(endpoint);
    for component in rel.components() {
        match component {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SequenceError::Traversal {
                    endpoint: endpoint.to_owned(),
                });
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    let resolved = root.join(rel);
    if canonical_escapes_root(root, &resolved) {
        return Err(SequenceError::Traversal {
            endpoint: endpoint.to_owned(),
        });
    }
    Ok(resolved)
}

/// Whether `path` canonically resolves outside `root` (an in-workspace symlink
/// escape). Canonicalizes the deepest existing ancestor of `path` — following
/// symlinks — and re-appends the not-yet-existing tail, then checks containment.
/// Conservative: if the root itself cannot be canonicalized, returns `false`
/// (the lexical pre-check already ran).
fn canonical_escapes_root(root: &Path, path: &Path) -> bool {
    let Ok(canonical_root) = std::fs::canonicalize(root) else {
        return false;
    };
    let mut ancestor = path;
    let mut tail = PathBuf::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            let full = canonical.join(&tail);
            return !full.starts_with(&canonical_root);
        }
        let Some(name) = ancestor.file_name() else {
            // Reached a component with no name (root/prefix) that would not
            // canonicalize; treat as non-escaping (lexical check governs).
            return false;
        };
        tail = Path::new(name).join(&tail);
        match ancestor.parent() {
            Some(parent) => ancestor = parent,
            None => return false,
        }
    }
}

/// Shared implementation of [`prepare_step`] / [`prepare_step_traced`]: resolves
/// the step's endpoint (rejecting traversal), loads it and its collection
/// ancestor-chain vars, builds the resolver with the **extracted scope prepended
/// (highest precedence)**, and returns the substituted request. When `sink` is
/// `Some`, a [`VarStep`] is recorded for every `{{var}}` that resolves (the
/// `run-seq -v` trace); `None` takes the exact untraced path.
///
/// The scope chain is
/// `extracted > session > cli > profile > [leaf-collection … root-collection]`
/// (then process env, via [`Resolver`]). Prepending scopes preserves the single
/// resolver seam — resolution is never forked. `session` sits below `extracted` (a
/// same-run capture still wins) and above `cli` (a captured token is not shadowed
/// by an ambient same-named var); it is the same in-memory Session store a
/// standalone send resolves against. The collection scope is the step's ancestor
/// chain walked innermost → outermost up to the workspace root (M7.9
/// inherit-and-override), mirroring the send-time [`crate::template::Resolver`];
/// `scopes.workspace` carries the root collection's `[vars]` as the outermost rung.
fn prepare_step_inner(
    workspace_root: &Path,
    step: &SequenceStep,
    extracted: &BTreeMap<String, String>,
    scopes: &RunScopes,
    sink: Option<&mut Vec<VarStep>>,
) -> Result<PreparedStep, SequenceError> {
    let path = resolve_step_path(workspace_root, &step.endpoint)?;
    let endpoint = load_endpoint(&path).map_err(|source| SequenceError::Endpoint {
        endpoint: step.endpoint.clone(),
        source: Box::new(source),
    })?;

    let mut resolver_scopes = vec![
        Scope::new("extracted", extracted.clone()),
        Scope::new("session", scopes.session.clone()),
        Scope::new("cli", scopes.cli.clone()),
        Scope::new("profile", scopes.profile.clone()),
    ];
    // Collection ancestor chain, innermost → outermost: the step's own collection
    // `folder.toml`, then each parent directory up to (but not including) the
    // workspace root. A root-level step endpoint (collection dir == root) pushes
    // none here and sees only the root vars appended below.
    let mut dir = path.parent().unwrap_or(workspace_root).to_path_buf();
    while dir != workspace_root && dir.starts_with(workspace_root) {
        let meta = load_collection_meta(&dir).map_err(|source| SequenceError::Endpoint {
            endpoint: step.endpoint.clone(),
            source: Box::new(source),
        })?;
        resolver_scopes.push(Scope::new("collection", meta.vars));
        let Some(parent) = dir.parent() else { break };
        dir = parent.to_path_buf();
    }
    // Root collection vars (outermost) — the M7.9 replacement for the old
    // standalone `workspace` scope.
    resolver_scopes.push(Scope::new("collection", scopes.workspace.clone()));
    let resolver = Resolver::new(resolver_scopes);

    let mut request = endpoint.request.clone();
    // The traced twin produces a byte-identical mutation — same scan, same
    // per-name resolution — and additionally records the resolution trail.
    match sink {
        Some(sink) => resolver.substitute_request_traced(&mut request, sink),
        None => resolver.substitute_request(&mut request),
    }
    // Fail loud: an unresolved `{{var}}` must not ship a literal placeholder on the
    // wire. The step fails here (the run's `on_error` decides halt vs continue).
    let names = crate::template::unresolved_placeholders(&request);
    if !names.is_empty() {
        return Err(SequenceError::Unresolved { names });
    }
    Ok(PreparedStep {
        endpoint_path: path,
        method: request.method,
        url: request.url.clone(),
        request,
        assertions: endpoint.assertions.clone(),
    })
}

/// Prepares a step for execution: resolves its endpoint (rejecting traversal),
/// loads it and its collection ancestor-chain vars, builds the resolver with the
/// **extracted scope prepended (highest precedence)**, and returns the
/// substituted request. See [`prepare_step_inner`] for the full scope-chain
/// semantics.
pub fn prepare_step(
    workspace_root: &Path,
    step: &SequenceStep,
    extracted: &BTreeMap<String, String>,
    scopes: &RunScopes,
) -> Result<PreparedStep, SequenceError> {
    prepare_step_inner(workspace_root, step, extracted, scopes, None)
}

/// The traced twin of [`prepare_step`]: produces a byte-identical [`PreparedStep`]
/// while additionally recording a [`VarStep`] into `sink` for every `{{var}}`
/// that resolved, in substitution order (the `run-seq -v` per-step trace). Note
/// that a sequence step's trace reflects the step's *own* resolution context —
/// including the ephemeral `extracted` scope of chained values — so it is
/// deliberately richer than a standalone `run -v` of the same endpoint would be.
pub fn prepare_step_traced(
    workspace_root: &Path,
    step: &SequenceStep,
    extracted: &BTreeMap<String, String>,
    scopes: &RunScopes,
    sink: &mut Vec<VarStep>,
) -> Result<PreparedStep, SequenceError> {
    prepare_step_inner(workspace_root, step, extracted, scopes, Some(sink))
}

/// Runs every extraction rule on a step's response, collecting `name → value`.
/// Any single rule failing fails the whole step's extraction.
pub fn extract_step(
    response: &Response,
    step: &SequenceStep,
) -> Result<BTreeMap<String, String>, ExtractError> {
    let mut out = BTreeMap::new();
    for (name, expr) in &step.extract {
        out.insert(name.clone(), extract_value(response, expr)?);
    }
    Ok(out)
}

/// The classified outcome of a single step's execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepResult {
    /// The request succeeded (status < 400) and every extraction rule passed.
    Ok {
        /// The HTTP status code.
        status: u16,
    },
    /// The request could not be sent (transport/TLS error, or a prepare failure).
    HttpError(String),
    /// The request completed with an HTTP error status (≥ 400).
    Failed {
        /// The HTTP status code.
        status: u16,
    },
    /// The request succeeded but an extraction rule failed.
    ExtractError(String),
    /// The step never ran because an earlier step halted the sequence.
    Skipped,
}

impl StepResult {
    /// Whether this result is a *failure* that triggers the `on_error` policy
    /// (transport error, HTTP error status, or extraction failure). `Skipped` is a
    /// consequence of a halt, not itself a trigger.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            StepResult::HttpError(_) | StepResult::Failed { .. } | StepResult::ExtractError(_)
        )
    }
}

/// Classifies a *successful* HTTP exchange (status ≥ 400 → `Failed`; else run the
/// step's extraction → `Ok` or `ExtractError`) and returns the values to merge.
/// This is the response-classification seam shared by [`classify_step`] and the
/// live TUI runner (whose message carries a stringified transport error, so it
/// handles the transport case itself and calls this for the `Ok` branch) — the
/// guarantee that their run semantics can't drift.
pub fn classify_response(
    response: &Response,
    step: &SequenceStep,
) -> (StepResult, BTreeMap<String, String>) {
    if response.status >= 400 {
        return (
            StepResult::Failed {
                status: response.status,
            },
            BTreeMap::new(),
        );
    }
    match extract_step(response, step) {
        Ok(extracted) => (
            StepResult::Ok {
                status: response.status,
            },
            extracted,
        ),
        Err(err) => (StepResult::ExtractError(err.to_string()), BTreeMap::new()),
    }
}

/// Classifies an execution outcome (transport error → `HttpError`; else
/// [`classify_response`]) and returns the values to merge into the accumulator
/// (empty on any failure). Used by [`run_sequence`].
pub fn classify_step(
    result: &Result<Response, HttpError>,
    step: &SequenceStep,
) -> (StepResult, BTreeMap<String, String>) {
    match result {
        Err(err) => (StepResult::HttpError(err.to_string()), BTreeMap::new()),
        Ok(response) => classify_response(response, step),
    }
}

/// Whether a completed step's `result` should halt the rest of the run under
/// `on_error`. The single halt-decision seam shared by [`run_sequence`] and the
/// live TUI driver so the halt/continue/skipped-tail behaviour cannot drift.
pub fn should_halt(result: &StepResult, on_error: OnError) -> bool {
    result.is_failure() && on_error == OnError::Halt
}

/// The steps of a sequence in run order (by `seq`, stable for ties). Shared by the
/// core runner and the TUI so both agree on ordering.
pub fn ordered_steps(sequence: &Sequence) -> Vec<&SequenceStep> {
    let mut steps: Vec<&SequenceStep> = sequence.steps.iter().collect();
    steps.sort_by_key(|step| step.seq);
    steps
}

/// The outcome of one step in a completed [`SequenceRun`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StepOutcome {
    /// The step's endpoint path string.
    pub endpoint: String,
    /// The resolved method (falls back to `GET` when the step failed to prepare).
    pub method: Method,
    /// The resolved URL (the raw endpoint string when the step failed to prepare).
    pub url: String,
    /// The classified result.
    pub result: StepResult,
    /// Values this step extracted and merged into the accumulator (empty on
    /// failure or skip).
    pub extracted: BTreeMap<String, String>,
    /// Request timing, when the request was actually sent and completed.
    pub timing: Option<Timing>,
}

/// A completed sequence run: one [`StepOutcome`] per step, in run order.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SequenceRun {
    /// Per-step outcomes.
    pub steps: Vec<StepOutcome>,
}

/// Runs a whole sequence end-to-end: for each step, prepare → [`execute_traced`]
/// → [`classify_step`] → merge extracted values → honour `on_error`. Extracted
/// values accumulate across steps. This is the wiremock-tested convenience over
/// the same primitives the live TUI drives; it is the single source of truth for
/// run semantics.
///
/// When `sink` is `Some`, every step that actually sends a request appends its
/// own [`DebugTrace`] to it, in step order (a step that fails to prepare, or
/// that is skipped after a halt, contributes no trace — there is no request to
/// trace). `sink: None` costs nothing extra per step — no `DebugTrace` is ever
/// built. Unused (always `None`) until a later wave wires the caller up; the
/// signature is frozen here so that wave is bin-only.
pub async fn run_sequence(
    client: &reqwest::Client,
    workspace_root: &Path,
    sequence: &Sequence,
    scopes: &RunScopes,
    options: &ExecuteOptions,
    mut sink: Option<&mut Vec<DebugTrace>>,
) -> SequenceRun {
    let mut extracted: BTreeMap<String, String> = BTreeMap::new();
    let mut outcomes = Vec::new();
    let mut halted = false;

    for step in ordered_steps(sequence) {
        if halted {
            outcomes.push(StepOutcome {
                endpoint: step.endpoint.clone(),
                method: Method::Get,
                url: step.endpoint.clone(),
                result: StepResult::Skipped,
                extracted: BTreeMap::new(),
                timing: None,
            });
            continue;
        }

        match prepare_step(workspace_root, step, &extracted, scopes) {
            Err(err) => {
                outcomes.push(StepOutcome {
                    endpoint: step.endpoint.clone(),
                    method: Method::Get,
                    url: step.endpoint.clone(),
                    result: StepResult::HttpError(err.to_string()),
                    extracted: BTreeMap::new(),
                    timing: None,
                });
                halted = should_halt(&StepResult::HttpError(String::new()), sequence.on_error);
            }
            Ok(prepared) => {
                let mut trace = sink
                    .is_some()
                    .then(|| DebugTrace::from_request(&prepared.request));
                let result =
                    execute_traced(client, &prepared.request, options, trace.as_mut()).await;
                if let Some(trace) = trace
                    && let Some(sink) = sink.as_deref_mut()
                {
                    sink.push(trace);
                }
                let timing = result.as_ref().ok().map(|response| response.timing);
                let (step_result, step_extracted) = classify_step(&result, step);
                if should_halt(&step_result, sequence.on_error) {
                    halted = true;
                }
                for (name, value) in &step_extracted {
                    extracted.insert(name.clone(), value.clone());
                }
                outcomes.push(StepOutcome {
                    endpoint: step.endpoint.clone(),
                    method: prepared.method,
                    url: prepared.url,
                    result: step_result,
                    extracted: step_extracted,
                    timing,
                });
            }
        }
    }

    SequenceRun { steps: outcomes }
}

#[cfg(test)]
mod tests;
