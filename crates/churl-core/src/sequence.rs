//! Request-sequence run engine and value extraction (M7.4).
//!
//! This module is the single source of truth for *how a sequence runs*: the same
//! primitives ([`prepare_step`], [`classify_step`], [`extract_step`]) drive both
//! the wiremock-tested [`run_sequence`] convenience and the live TUI runner, so
//! their semantics can never drift.
//!
//! The resolver stays the single `{{var}}` seam (the M9 plugin guardrail): a step
//! is prepared by *prepending* an ephemeral `"extracted"` [`Scope`] to the
//! canonical scope chain — resolution is never forked. Extracted values win over
//! ambient config so a chained value is never shadowed by a same-named workspace
//! var.
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

use crate::http::{ExecuteOptions, HttpError, execute};
use crate::model::{Method, OnError, Response, Sequence, SequenceStep, Timing};
use crate::persistence::{PersistenceError, load_collection_meta, load_endpoint};
use crate::template::{Resolver, Scope};

/// Error extracting a value from a response via [`extract_value`]. Every variant
/// names the offending expression and the reason.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
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
/// ephemeral extracted scope: CLI `--var` flags, the active profile, and the
/// workspace `[vars]`. (The per-endpoint collection scope is loaded fresh in
/// [`prepare_step`] from each step's own collection.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunScopes {
    /// Highest ambient precedence: CLI `--var` overrides.
    pub cli: BTreeMap<String, String>,
    /// The active profile's vars.
    pub profile: BTreeMap<String, String>,
    /// Workspace-level `[vars]` (lowest, before process env).
    pub workspace: BTreeMap<String, String>,
}

/// A step resolved and ready to [`execute`]: the endpoint file it came from plus
/// its fully-substituted request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedStep {
    /// The endpoint file this step runs.
    pub endpoint_path: PathBuf,
    /// The request after `{{var}}` substitution (a resolved clone; disk untouched).
    pub request: crate::model::Request,
    /// The request method (mirrors `request.method` for convenience).
    pub method: Method,
    /// The resolved request URL (mirrors `request.url` for convenience).
    pub url: String,
}

/// Error preparing a sequence step (path resolution / endpoint load). The step
/// fails; the run never panics and never escapes the workspace root.
#[derive(Debug, thiserror::Error)]
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
/// that would escape it (`..` components, absolute paths, drive prefixes).
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
    Ok(root.join(rel))
}

/// Prepares a step for execution: resolves its endpoint (rejecting traversal),
/// loads it and its collection vars, builds the resolver with the **extracted
/// scope prepended (highest precedence)**, and returns the substituted request.
///
/// The scope chain is `extracted > cli > profile > collection > workspace`
/// (then process env, via [`Resolver`]). Prepending one scope preserves the
/// single resolver seam — resolution is never forked.
pub fn prepare_step(
    workspace_root: &Path,
    step: &SequenceStep,
    extracted: &BTreeMap<String, String>,
    scopes: &RunScopes,
) -> Result<PreparedStep, SequenceError> {
    let path = resolve_step_path(workspace_root, &step.endpoint)?;
    let endpoint = load_endpoint(&path).map_err(|source| SequenceError::Endpoint {
        endpoint: step.endpoint.clone(),
        source: Box::new(source),
    })?;
    let collection_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let meta = load_collection_meta(collection_dir).map_err(|source| SequenceError::Endpoint {
        endpoint: step.endpoint.clone(),
        source: Box::new(source),
    })?;

    let resolver = Resolver::new(vec![
        Scope::new("extracted", extracted.clone()),
        Scope::new("cli", scopes.cli.clone()),
        Scope::new("profile", scopes.profile.clone()),
        Scope::new("collection", meta.vars),
        Scope::new("workspace", scopes.workspace.clone()),
    ]);

    let mut request = endpoint.request.clone();
    resolver.substitute_request(&mut request);
    Ok(PreparedStep {
        endpoint_path: path,
        method: request.method,
        url: request.url.clone(),
        request,
    })
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

/// Classifies an execution outcome and, on success, runs the step's extraction.
/// This is the single classify+extract point shared by [`run_sequence`] and the
/// live TUI runner — the guarantee that their run semantics can't drift.
///
/// Returns the [`StepResult`] and the values to merge into the accumulator (empty
/// on any failure).
pub fn classify_step(
    result: &Result<Response, HttpError>,
    step: &SequenceStep,
) -> (StepResult, BTreeMap<String, String>) {
    match result {
        Err(err) => (StepResult::HttpError(err.to_string()), BTreeMap::new()),
        Ok(response) if response.status >= 400 => (
            StepResult::Failed {
                status: response.status,
            },
            BTreeMap::new(),
        ),
        Ok(response) => match extract_step(response, step) {
            Ok(extracted) => (
                StepResult::Ok {
                    status: response.status,
                },
                extracted,
            ),
            Err(err) => (StepResult::ExtractError(err.to_string()), BTreeMap::new()),
        },
    }
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
pub struct SequenceRun {
    /// Per-step outcomes.
    pub steps: Vec<StepOutcome>,
}

/// Runs a whole sequence end-to-end: for each step, prepare → [`execute`] →
/// [`classify_step`] → merge extracted values → honour `on_error`. Extracted
/// values accumulate across steps. This is the wiremock-tested convenience over
/// the same primitives the live TUI drives; it is the single source of truth for
/// run semantics.
pub async fn run_sequence(
    client: &reqwest::Client,
    workspace_root: &Path,
    sequence: &Sequence,
    scopes: &RunScopes,
    options: &ExecuteOptions,
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
                if sequence.on_error == OnError::Halt {
                    halted = true;
                }
            }
            Ok(prepared) => {
                let result = execute(client, &prepared.request, options).await;
                let timing = result.as_ref().ok().map(|response| response.timing);
                let (step_result, step_extracted) = classify_step(&result, step);
                if step_result.is_failure() && sequence.on_error == OnError::Halt {
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
mod tests {
    use super::*;
    use crate::model::{Header, Timing};
    use std::time::Duration;

    fn json_response(body: &str) -> Response {
        Response {
            status: 200,
            headers: vec![Header {
                name: "Content-Type".into(),
                value: "application/json".into(),
                enabled: true,
            }],
            body: body.as_bytes().to_vec(),
            truncated: false,
            timing: Timing {
                connect: None,
                total: Duration::from_millis(1),
            },
        }
    }

    #[test]
    fn extract_status() {
        let mut response = json_response("{}");
        response.status = 201;
        assert_eq!(extract_value(&response, "status").unwrap(), "201");
    }

    #[test]
    fn extract_header_case_insensitive() {
        let response = json_response("{}");
        assert_eq!(
            extract_value(&response, "header:content-type").unwrap(),
            "application/json"
        );
        assert_eq!(
            extract_value(&response, "header:Content-Type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn extract_missing_header_errors() {
        let response = json_response("{}");
        assert!(matches!(
            extract_value(&response, "header:X-Absent"),
            Err(ExtractError::HeaderMissing { .. })
        ));
    }

    #[test]
    fn extract_nested_path_optional_dollar() {
        let response = json_response(r#"{"data":{"token":"abc","user":{"id":7}}}"#);
        assert_eq!(extract_value(&response, "$.data.token").unwrap(), "abc");
        assert_eq!(extract_value(&response, "data.token").unwrap(), "abc");
        assert_eq!(extract_value(&response, "$.data.user.id").unwrap(), "7");
    }

    #[test]
    fn extract_array_indices() {
        let response = json_response(r#"{"items":[{"id":10},{"id":20}]}"#);
        assert_eq!(extract_value(&response, "$.items[0].id").unwrap(), "10");
        assert_eq!(extract_value(&response, "items[1].id").unwrap(), "20");
    }

    #[test]
    fn extract_typed_leaves() {
        let response = json_response(r#"{"n":42,"f":1.5,"b":true,"s":"hi","o":{"k":1},"a":[1,2]}"#);
        assert_eq!(extract_value(&response, "$.n").unwrap(), "42");
        assert_eq!(extract_value(&response, "$.f").unwrap(), "1.5");
        assert_eq!(extract_value(&response, "$.b").unwrap(), "true");
        assert_eq!(extract_value(&response, "$.s").unwrap(), "hi");
        // Object and array leaves become compact JSON.
        assert_eq!(extract_value(&response, "$.o").unwrap(), r#"{"k":1}"#);
        assert_eq!(extract_value(&response, "$.a").unwrap(), "[1,2]");
    }

    #[test]
    fn extract_null_leaf_errors() {
        let response = json_response(r#"{"x":null}"#);
        assert!(matches!(
            extract_value(&response, "$.x"),
            Err(ExtractError::NullLeaf { .. })
        ));
    }

    #[test]
    fn extract_missing_key_and_bad_index() {
        let response = json_response(r#"{"a":[1]}"#);
        assert!(matches!(
            extract_value(&response, "$.nope"),
            Err(ExtractError::MissingKey { .. })
        ));
        assert!(matches!(
            extract_value(&response, "$.a[5]"),
            Err(ExtractError::IndexOutOfRange { len: 1, .. })
        ));
    }

    #[test]
    fn extract_type_mismatch() {
        let response = json_response(r#"{"a":"str"}"#);
        // Indexing a string, or keying an array.
        assert!(matches!(
            extract_value(&response, "$.a[0]"),
            Err(ExtractError::TypeMismatch { .. })
        ));
        let response = json_response(r#"{"a":[1,2]}"#);
        assert!(matches!(
            extract_value(&response, "$.a.foo"),
            Err(ExtractError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn extract_non_json_body_errors() {
        let mut response = json_response("");
        response.body = b"<html>not json</html>".to_vec();
        assert!(matches!(
            extract_value(&response, "$.a"),
            Err(ExtractError::NotJson { .. })
        ));
    }

    #[test]
    fn extract_malformed_expr_errors() {
        let response = json_response(r#"{"a":1}"#);
        for expr in ["$.", "a..b", "a[", "a[x]", "$"] {
            assert!(
                matches!(
                    extract_value(&response, expr),
                    Err(ExtractError::BadExpr { .. })
                ),
                "expected BadExpr for {expr:?}"
            );
        }
    }

    #[test]
    fn extract_unicode_key() {
        let response = json_response(r#"{"café":{"naïve":"yes"}}"#);
        assert_eq!(extract_value(&response, "café.naïve").unwrap(), "yes");
    }

    #[test]
    fn parse_path_shapes() {
        assert_eq!(
            parse_path("$.a.b[0].c").unwrap(),
            vec![
                PathSeg::Key("a".into()),
                PathSeg::Key("b".into()),
                PathSeg::Index(0),
                PathSeg::Key("c".into()),
            ]
        );
        assert_eq!(
            parse_path("a.b[2]").unwrap(),
            vec![
                PathSeg::Key("a".into()),
                PathSeg::Key("b".into()),
                PathSeg::Index(2),
            ]
        );
    }

    #[test]
    fn resolve_step_path_rejects_traversal() {
        let root = Path::new("/ws");
        assert!(resolve_step_path(root, "../secret.toml").is_err());
        assert!(resolve_step_path(root, "a/../../secret.toml").is_err());
        assert!(resolve_step_path(root, "/etc/passwd").is_err());
        // A normal relative path is fine.
        assert_eq!(
            resolve_step_path(root, "auth/login.toml").unwrap(),
            Path::new("/ws/auth/login.toml")
        );
    }

    #[test]
    fn classify_step_variants() {
        let step = SequenceStep {
            seq: 0,
            endpoint: "e.toml".into(),
            extract: BTreeMap::new(),
        };
        // Ok
        let (result, _) = classify_step(&Ok(json_response("{}")), &step);
        assert_eq!(result, StepResult::Ok { status: 200 });
        // Failed (>= 400)
        let mut bad = json_response("{}");
        bad.status = 500;
        let (result, _) = classify_step(&Ok(bad), &step);
        assert_eq!(result, StepResult::Failed { status: 500 });
        assert!(result.is_failure());
        // HttpError
        let (result, _) = classify_step(&Err(HttpError::Timeout), &step);
        assert!(matches!(result, StepResult::HttpError(_)));
    }

    #[test]
    fn classify_step_extract_error() {
        let mut step_extract = BTreeMap::new();
        step_extract.insert("v".to_owned(), "$.missing".to_owned());
        let step = SequenceStep {
            seq: 0,
            endpoint: "e.toml".into(),
            extract: step_extract,
        };
        let (result, extracted) = classify_step(&Ok(json_response("{}")), &step);
        assert!(matches!(result, StepResult::ExtractError(_)));
        assert!(extracted.is_empty());
    }
}
