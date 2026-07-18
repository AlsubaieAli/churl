//! Response assertions: a small evaluator that checks a request's response
//! against a caller-supplied expectation, reusing the [`crate::sequence`]
//! extraction grammar as its *target* language rather than inventing a second
//! one.
//!
//! An [`Assertion`] pairs an extraction expression (`target`) with an
//! [`AssertOp`] and an optional expected `value`; [`evaluate`] runs it against
//! a [`Response`] and [`run_assertions`] does so for a whole set, producing a
//! serializable [`AssertionReport`] the CLI drops straight into the JSON
//! envelope (`docs/CLI.md`, "Assertions").
//!
//! # Human syntax
//!
//! [`Assertion::parse`] reads `"<target> <op> <value>"`, e.g. `status == 200`,
//! `header:Content-Type contains json`, `$.data.id exists`. `target` is a
//! single whitespace-free token (the extraction grammar never contains
//! spaces); `value` is everything after the operator token, spaces included —
//! so `body contains "hello world"` keeps its embedded space. `exists`/`absent`
//! take no value.

use serde::{Deserialize, Serialize};

use crate::load::{LoadStats, StatTarget};
use crate::model::Response;
use crate::sequence::{ExtractError, extract_value};

/// A comparison operator an [`Assertion`] applies to its extracted value.
/// Persisted (and re-emitted in the JSON report) as its [`AssertOp::canonical`]
/// string, never as the Rust variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertOp {
    /// `==` / `eq` — exact string equality.
    Eq,
    /// `!=` / `ne` — exact string inequality.
    Ne,
    /// `contains` — substring match.
    Contains,
    /// `exists` — the target extracts successfully (a `null` leaf or missing
    /// key/header does not count as existing).
    Exists,
    /// `absent` — the target's extraction fails with a not-found reason (a
    /// missing header/key/index, or a `null` leaf).
    Absent,
    /// `<` — numeric less-than.
    Lt,
    /// `>` — numeric greater-than.
    Gt,
    /// `<=` — numeric less-than-or-equal.
    Le,
    /// `>=` — numeric greater-than-or-equal.
    Ge,
}

impl AssertOp {
    /// The canonical on-the-wire token for this operator — used for
    /// persistence, the JSON report's `op` field, and error messages. Every
    /// operator's canonical token is also a valid *parse* token (round-trips).
    pub const fn canonical(self) -> &'static str {
        match self {
            AssertOp::Eq => "==",
            AssertOp::Ne => "!=",
            AssertOp::Contains => "contains",
            AssertOp::Exists => "exists",
            AssertOp::Absent => "absent",
            AssertOp::Lt => "<",
            AssertOp::Gt => ">",
            AssertOp::Le => "<=",
            AssertOp::Ge => ">=",
        }
    }

    /// Whether this operator requires an expected `value` (every operator
    /// except [`AssertOp::Exists`]/[`AssertOp::Absent`]).
    pub const fn needs_value(self) -> bool {
        !matches!(self, AssertOp::Exists | AssertOp::Absent)
    }

    /// Parses one whitespace-delimited operator token, accepting both the
    /// symbolic and word aliases (`==`/`eq`, `!=`/`ne`). Longest-match order
    /// doesn't matter here since tokens are compared whole (never a substring
    /// scan), so `<`/`<=` and `>`/`>=` never collide.
    fn from_token(token: &str) -> Option<Self> {
        match token {
            "==" | "eq" => Some(AssertOp::Eq),
            "!=" | "ne" => Some(AssertOp::Ne),
            "contains" => Some(AssertOp::Contains),
            "exists" => Some(AssertOp::Exists),
            "absent" => Some(AssertOp::Absent),
            "<" => Some(AssertOp::Lt),
            ">" => Some(AssertOp::Gt),
            "<=" => Some(AssertOp::Le),
            ">=" => Some(AssertOp::Ge),
            _ => None,
        }
    }
}

impl std::fmt::Display for AssertOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.canonical())
    }
}

impl std::str::FromStr for AssertOp {
    type Err = AssertParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // No enclosing assertion string in this bare context (only reached via
        // (de)serializing a standalone `AssertOp`, e.g. loading a persisted
        // `[[assertions]]` table's `op` key) — `expr` is empty rather than
        // fabricated.
        Self::from_token(s).ok_or_else(|| AssertParseError::UnknownOp {
            expr: String::new(),
            token: s.to_owned(),
        })
    }
}

impl Serialize for AssertOp {
    /// Serializes as the canonical operator string (e.g. `"=="`), mirroring
    /// [`crate::model::Method`]'s string (de)serialization.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AssertOp {
    /// Deserializes from the canonical operator string via [`AssertOp::from_str`].
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Error parsing a human assertion string via [`Assertion::parse`]. Every
/// variant names the offending input so a bad `--assert` flag or persisted
/// rule is easy to fix.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AssertParseError {
    /// The input had no target token at all (empty or whitespace-only).
    #[error("empty assertion (expected \"<target> <op> <value>\")")]
    EmptyTarget,
    /// A target was given but no operator token followed it.
    #[error("assertion {expr:?}: missing operator (expected \"<target> <op> <value>\")")]
    MissingOperator {
        /// The full input string.
        expr: String,
    },
    /// The operator token didn't match any known [`AssertOp`].
    #[error(
        "assertion {expr:?}: unknown operator {token:?} (expected one of: == eq, != ne, contains, exists, absent, <, >, <=, >=)"
    )]
    UnknownOp {
        /// The full input string, when parsed via [`Assertion::parse`] (empty
        /// when reached via a bare [`AssertOp::from_str`]).
        expr: String,
        /// The offending token.
        token: String,
    },
    /// A value-requiring operator (everything but `exists`/`absent`) had no
    /// value after it.
    #[error("assertion {expr:?}: operator {op} requires a value")]
    MissingValue {
        /// The full input string.
        expr: String,
        /// The operator's canonical string.
        op: &'static str,
    },
    /// A value-less operator (`exists`/`absent`) was followed by trailing
    /// text — rejected rather than silently ignored, so a mistyped operator
    /// (e.g. `id exists == 200`) can't degrade into an always-passing check.
    #[error("assertion {expr:?}: operator {op} takes no value (unexpected {trailing:?})")]
    UnexpectedValue {
        /// The full input string.
        expr: String,
        /// The operator's canonical string.
        op: &'static str,
        /// The unexpected trailing text after the operator.
        trailing: String,
    },
}

/// One assertion: check `target` (an extraction expression, see
/// [`crate::sequence::extract_value`]) against `op`/`value`. `value` is
/// `None` only for `Exists`/`Absent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assertion {
    /// The extraction expression to evaluate — `status`, `header:<Name>`, or a
    /// JSON path (see the [module grammar](crate::sequence) docs).
    pub target: String,
    /// The comparison operator.
    pub op: AssertOp,
    /// The expected value; `None` for `Exists`/`Absent`, `Some` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl Assertion {
    /// Parses `"<target> <op> <value>"`. `target` is the first whitespace-run
    /// token (the extraction grammar is itself whitespace-free); `op` is the
    /// next token; everything after that — spaces included — is `value`.
    /// `value` is `None` for `exists`/`absent`, and those ops reject any
    /// trailing text (rather than dropping it) so a mistyped operator can't
    /// silently become an always-passing check.
    pub fn parse(s: &str) -> Result<Assertion, AssertParseError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(AssertParseError::EmptyTarget);
        }

        let mut after_target = trimmed.splitn(2, char::is_whitespace);
        let target = after_target.next().unwrap_or("").to_owned();
        let rest = after_target.next().map(str::trim_start).unwrap_or("");
        if rest.is_empty() {
            return Err(AssertParseError::MissingOperator { expr: s.to_owned() });
        }

        let mut after_op = rest.splitn(2, char::is_whitespace);
        let op_token = after_op.next().unwrap_or("");
        let value_rest = after_op.next().map(str::trim_start).unwrap_or("");
        let op = AssertOp::from_token(op_token).ok_or_else(|| AssertParseError::UnknownOp {
            expr: s.to_owned(),
            token: op_token.to_owned(),
        })?;

        let value = if op.needs_value() {
            if value_rest.is_empty() {
                return Err(AssertParseError::MissingValue {
                    expr: s.to_owned(),
                    op: op.canonical(),
                });
            }
            Some(value_rest.to_owned())
        } else {
            if !value_rest.is_empty() {
                return Err(AssertParseError::UnexpectedValue {
                    expr: s.to_owned(),
                    op: op.canonical(),
                    trailing: value_rest.to_owned(),
                });
            }
            None
        };

        Ok(Assertion { target, op, value })
    }
}

/// The result of [`evaluate`]ing one [`Assertion`] against a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertOutcome {
    /// Whether the assertion held.
    pub pass: bool,
    /// The stringified extracted value, when extraction succeeded.
    pub actual: Option<String>,
    /// A human reason, populated whenever `pass` is `false`.
    pub error: Option<String>,
}

/// Whether an [`ExtractError`] represents "the target genuinely isn't there"
/// (a missing header/key/index, or a JSON `null` leaf) as opposed to a
/// malformed assertion (bad expression syntax, or a non-JSON body for a
/// JSON-path target) — the distinction [`AssertOp::Exists`]/[`AssertOp::Absent`]
/// need. Exhaustive on purpose: a new [`ExtractError`] variant forces a
/// deliberate call on which side of this line it falls, rather than silently
/// defaulting.
fn is_not_found(err: &ExtractError) -> bool {
    match err {
        ExtractError::HeaderMissing { .. }
        | ExtractError::MissingKey { .. }
        | ExtractError::IndexOutOfRange { .. }
        | ExtractError::NullLeaf { .. } => true,
        ExtractError::NotJson { .. }
        | ExtractError::BadExpr { .. }
        | ExtractError::TypeMismatch { .. } => false,
    }
}

/// Evaluates one [`Assertion`] against `response`, running its `target`
/// through [`extract_value`] and applying `op`.
pub fn evaluate(assertion: &Assertion, response: &Response) -> AssertOutcome {
    let extraction = extract_value(response, &assertion.target);

    match assertion.op {
        AssertOp::Exists => match extraction {
            Ok(actual) => AssertOutcome {
                pass: true,
                actual: Some(actual),
                error: None,
            },
            Err(err) => AssertOutcome {
                pass: false,
                actual: None,
                error: Some(err.to_string()),
            },
        },
        AssertOp::Absent => match extraction {
            Ok(actual) => AssertOutcome {
                pass: false,
                actual: Some(actual),
                error: Some("value is present".to_owned()),
            },
            Err(err) if is_not_found(&err) => AssertOutcome {
                pass: true,
                actual: None,
                error: None,
            },
            Err(err) => AssertOutcome {
                pass: false,
                actual: None,
                error: Some(err.to_string()),
            },
        },
        op => match extraction {
            Err(err) => AssertOutcome {
                pass: false,
                actual: None,
                error: Some(err.to_string()),
            },
            Ok(actual) => {
                // `value` is always `Some` here: every op reaching this arm is
                // value-requiring (`Assertion::parse` refuses to construct one
                // without a value, and `Exists`/`Absent` are handled above).
                let expected = assertion.value.as_deref().unwrap_or_default();
                evaluate_value_op(op, &actual, expected)
            }
        },
    }
}

/// Applies a value-comparing operator (everything but `Exists`/`Absent`) to
/// an already-extracted `actual` against `expected`.
fn evaluate_value_op(op: AssertOp, actual: &str, expected: &str) -> AssertOutcome {
    match op {
        AssertOp::Eq | AssertOp::Ne | AssertOp::Contains => {
            let pass = match op {
                AssertOp::Eq => actual == expected,
                AssertOp::Ne => actual != expected,
                AssertOp::Contains => actual.contains(expected),
                _ => unreachable!(),
            };
            AssertOutcome {
                actual: Some(actual.to_owned()),
                error: if pass {
                    None
                } else {
                    Some(format!("expected {actual:?} {op} {expected:?}"))
                },
                pass,
            }
        }
        AssertOp::Lt | AssertOp::Gt | AssertOp::Le | AssertOp::Ge => {
            evaluate_numeric_op(op, actual, expected)
        }
        AssertOp::Exists | AssertOp::Absent => {
            unreachable!("Exists/Absent are handled by evaluate() before reaching here")
        }
    }
}

/// Parses both sides as `f64` and applies a numeric comparison operator; a
/// non-numeric side fails with a clear reason rather than panicking or
/// silently falling back to string comparison.
fn evaluate_numeric_op(op: AssertOp, actual: &str, expected: &str) -> AssertOutcome {
    let parsed = actual
        .trim()
        .parse::<f64>()
        .ok()
        .zip(expected.trim().parse::<f64>().ok());
    let Some((a, e)) = parsed else {
        return AssertOutcome {
            pass: false,
            actual: Some(actual.to_owned()),
            error: Some(format!(
                "non-numeric comparison: {actual:?} {op} {expected:?}"
            )),
        };
    };
    let pass = match op {
        AssertOp::Lt => a < e,
        AssertOp::Gt => a > e,
        AssertOp::Le => a <= e,
        AssertOp::Ge => a >= e,
        _ => unreachable!(),
    };
    AssertOutcome {
        actual: Some(actual.to_owned()),
        error: if pass {
            None
        } else {
            Some(format!("expected {actual} {op} {expected}"))
        },
        pass,
    }
}

/// One assertion's outcome, shaped for the JSON envelope
/// (`docs/CLI.md`, "Assertions").
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct AssertResult {
    /// The assertion's `target` expression.
    pub target: String,
    /// The operator, serialized as its canonical string.
    pub op: AssertOp,
    /// The expected value, when the operator takes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// The extracted actual value, when extraction succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    /// Whether the assertion held.
    pub pass: bool,
    /// A human reason, populated whenever `pass` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The full report over a set of assertions, dropped straight into the JSON
/// envelope's `data.assertions` field (`docs/CLI.md`).
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct AssertionReport {
    /// Whether every assertion passed (vacuously `true` for an empty set).
    pub passed: bool,
    /// Total number of assertions run.
    pub total: usize,
    /// Number that failed.
    pub failed: usize,
    /// Per-assertion results, in input order.
    pub results: Vec<AssertResult>,
}

/// Runs every assertion in `assertions` against `response`, in order,
/// producing an [`AssertionReport`]. An empty set passes vacuously.
pub fn run_assertions(assertions: &[Assertion], response: &Response) -> AssertionReport {
    let results: Vec<AssertResult> = assertions
        .iter()
        .map(|assertion| {
            let outcome = evaluate(assertion, response);
            AssertResult {
                target: assertion.target.clone(),
                op: assertion.op,
                expected: assertion.value.clone(),
                actual: outcome.actual,
                pass: outcome.pass,
                error: outcome.error,
            }
        })
        .collect();
    let failed = results.iter().filter(|r| !r.pass).count();
    AssertionReport {
        passed: failed == 0,
        total: results.len(),
        failed,
        results,
    }
}

/// A load-run assertion: a `stats.<field>` [`StatTarget`] paired with an
/// [`AssertOp`] and optional expected value. Constructed only via
/// [`parse_stats_assertion`], which validates the target names a real stat — so
/// an unresolvable `stats.foo` can never reach evaluation (illegal-states-
/// unrepresentable: holding a `StatAssertion` guarantees a known target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatAssertion {
    /// The resolved aggregate-stat target.
    pub target: StatTarget,
    /// The original `stats.<field>` token, echoed verbatim into the report's
    /// `target` field so `median`/`p50` read back exactly as the user wrote them.
    pub raw_target: String,
    /// The comparison operator.
    pub op: AssertOp,
    /// The expected value; `None` for `exists`/`absent`.
    pub value: Option<String>,
}

/// Error parsing a `churl load --assert` expression: either the shared
/// `<target> <op> <value>` grammar rejected it, or the target does not name a
/// load stat. Both are, at the CLI boundary, a band-5 `invalid-assertion`
/// usage mistake caught before any request is fired.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatAssertError {
    /// The `<target> <op> <value>` grammar itself didn't parse.
    #[error(transparent)]
    Grammar(#[from] AssertParseError),
    /// The expression parsed, but its target is not a `stats.<field>` name.
    #[error(
        "assertion {expr:?}: unknown load-stat target {target:?} (expected one of: stats.count, stats.ok, stats.failed, stats.errored, stats.success_rate, stats.error_rate, stats.p50/stats.median, stats.p95, stats.min, stats.max, stats.mean, stats.rps)"
    )]
    UnknownTarget {
        /// The full input expression.
        expr: String,
        /// The offending target token.
        target: String,
    },
}

/// Parses one `churl load --assert` expression into a [`StatAssertion`],
/// reusing the exact [`Assertion::parse`] grammar (same `<target> <op> <value>`
/// syntax and [`AssertOp`] set as a response assertion) and then validating the
/// target is a `stats.<field>` name via [`StatTarget::parse`]. The grammar is
/// shared verbatim — only the *target vocabulary* differs (aggregate stats vs.
/// a response's status/header/JSON-path), matching the "one assertion model,
/// three scopes" story (`docs/DECISIONS.md`).
pub fn parse_stats_assertion(expr: &str) -> Result<StatAssertion, StatAssertError> {
    let parsed = Assertion::parse(expr)?;
    let target =
        StatTarget::parse(&parsed.target).ok_or_else(|| StatAssertError::UnknownTarget {
            expr: expr.to_owned(),
            target: parsed.target.clone(),
        })?;
    Ok(StatAssertion {
        target,
        raw_target: parsed.target,
        op: parsed.op,
        value: parsed.value,
    })
}

/// Runs every [`StatAssertion`] against a completed load run's `stats` and its
/// measured `rps`, producing the identical [`AssertionReport`] shape a response
/// assertion set does — so the `data.assertions` envelope field is the same
/// whether it came from a response (`run`/`send`/`run-seq`) or an aggregate
/// (`load`). Numeric comparisons reuse the response evaluator
/// ([`evaluate_value_op`]) verbatim; only target *resolution* differs.
pub fn run_stats_assertions(
    assertions: &[StatAssertion],
    stats: &LoadStats,
    rps: Option<f64>,
) -> AssertionReport {
    let results: Vec<AssertResult> = assertions
        .iter()
        .map(|assertion| {
            let resolved = assertion.target.resolve(stats, rps);
            let outcome = evaluate_stat(assertion.op, assertion.value.as_deref(), resolved);
            AssertResult {
                target: assertion.raw_target.clone(),
                op: assertion.op,
                expected: assertion.value.clone(),
                actual: outcome.actual,
                pass: outcome.pass,
                error: outcome.error,
            }
        })
        .collect();
    let failed = results.iter().filter(|r| !r.pass).count();
    AssertionReport {
        passed: failed == 0,
        total: results.len(),
        failed,
        results,
    }
}

/// Evaluates one operator against a `stats.<field>` value already resolved to a
/// number (`None` when the stat is undefined — a latency with no completed
/// request, or a rate over a zero-attempted run). Value-comparing operators
/// reuse the response evaluator's [`evaluate_value_op`] verbatim after
/// formatting the number; `exists`/`absent` test whether the stat is *defined*
/// (a natural fit — `stats.p95 exists` asserts at least one request completed).
fn evaluate_stat(op: AssertOp, expected: Option<&str>, resolved: Option<f64>) -> AssertOutcome {
    const UNDEFINED: &str = "stat is undefined (no completed requests)";
    match op {
        AssertOp::Exists => match resolved {
            Some(value) => AssertOutcome {
                pass: true,
                actual: Some(format_stat(value)),
                error: None,
            },
            None => AssertOutcome {
                pass: false,
                actual: None,
                error: Some(UNDEFINED.to_owned()),
            },
        },
        AssertOp::Absent => match resolved {
            None => AssertOutcome {
                pass: true,
                actual: None,
                error: None,
            },
            Some(value) => AssertOutcome {
                pass: false,
                actual: Some(format_stat(value)),
                error: Some("value is present".to_owned()),
            },
        },
        op => match resolved {
            None => AssertOutcome {
                pass: false,
                actual: None,
                error: Some(UNDEFINED.to_owned()),
            },
            // `expected` is always `Some` for a value-op (parse refuses one
            // without a value); `unwrap_or_default` keeps the type total.
            Some(value) => evaluate_value_op(op, &format_stat(value), expected.unwrap_or_default()),
        },
    }
}

/// Formats a resolved stat number for comparison and the report's `actual`
/// field: an integer-valued float prints without a decimal point (`5`, not
/// `5.0`) so a count/latency reads naturally and an `== 5` check holds, while a
/// fractional value (a rate, an rps) keeps its shortest round-tripping form.
/// Numeric operators re-parse this back to `f64`, so the exact spelling only
/// matters for the string operators (`==`/`!=`/`contains`).
fn format_stat(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Header, Timing};
    use std::time::Duration;

    fn response(status: u16, headers: Vec<(&str, &str)>, body: &str) -> Response {
        Response {
            status,
            headers: headers
                .into_iter()
                .map(|(name, value)| Header {
                    name: name.to_owned(),
                    value: value.to_owned(),
                    enabled: true,
                })
                .collect(),
            body: body.as_bytes().to_vec(),
            truncated: false,
            timing: Timing {
                connect: None,
                total: Duration::from_millis(1),
            },
        }
    }

    // --- parser ---

    #[test]
    fn parse_eq_symbol_and_word() {
        assert_eq!(
            Assertion::parse("status == 200").unwrap(),
            Assertion {
                target: "status".into(),
                op: AssertOp::Eq,
                value: Some("200".into())
            }
        );
        assert_eq!(Assertion::parse("status eq 200").unwrap().op, AssertOp::Eq);
    }

    #[test]
    fn parse_every_operator() {
        let cases = [
            ("a != b", AssertOp::Ne),
            ("a ne b", AssertOp::Ne),
            ("a contains b", AssertOp::Contains),
            ("a exists", AssertOp::Exists),
            ("a absent", AssertOp::Absent),
            ("a < 1", AssertOp::Lt),
            ("a > 1", AssertOp::Gt),
            ("a <= 1", AssertOp::Le),
            ("a >= 1", AssertOp::Ge),
        ];
        for (input, expected_op) in cases {
            assert_eq!(Assertion::parse(input).unwrap().op, expected_op, "{input}");
        }
    }

    #[test]
    fn parse_value_keeps_embedded_spaces() {
        let a = Assertion::parse("$.data.msg contains hello world").unwrap();
        assert_eq!(a.value.as_deref(), Some("hello world"));
    }

    #[test]
    fn parse_exists_absent_have_no_value() {
        assert_eq!(Assertion::parse("$.data.id exists").unwrap().value, None);
        assert_eq!(Assertion::parse("$.data.id absent").unwrap().value, None);
    }

    #[test]
    fn parse_rejects_empty_target() {
        assert!(matches!(
            Assertion::parse(""),
            Err(AssertParseError::EmptyTarget)
        ));
        assert!(matches!(
            Assertion::parse("   "),
            Err(AssertParseError::EmptyTarget)
        ));
    }

    #[test]
    fn parse_rejects_missing_operator() {
        assert!(matches!(
            Assertion::parse("status"),
            Err(AssertParseError::MissingOperator { .. })
        ));
    }

    #[test]
    fn parse_rejects_unknown_operator() {
        let err = Assertion::parse("status ?? 200").unwrap_err();
        assert!(matches!(err, AssertParseError::UnknownOp { token, .. } if token == "??"));
    }

    #[test]
    fn parse_rejects_missing_value() {
        assert!(matches!(
            Assertion::parse("status =="),
            Err(AssertParseError::MissingValue { .. })
        ));
    }

    #[test]
    fn parse_rejects_trailing_text_after_value_less_op() {
        // A mistyped operator must not degrade into an always-passing check:
        // `id exists == 200` is a user who meant equality, not existence.
        let err = Assertion::parse("$.data.id exists == 200").unwrap_err();
        assert!(matches!(
            err,
            AssertParseError::UnexpectedValue { op, trailing, .. }
                if op == "exists" && trailing == "== 200"
        ));
        assert!(matches!(
            Assertion::parse("$.data.id absent junk"),
            Err(AssertParseError::UnexpectedValue { .. })
        ));
    }

    // --- evaluator: status ---

    #[test]
    fn evaluate_status_eq_pass_and_fail() {
        let resp = response(200, vec![], "");
        let pass = Assertion::parse("status == 200").unwrap();
        assert!(evaluate(&pass, &resp).pass);
        let fail = Assertion::parse("status == 500").unwrap();
        let outcome = evaluate(&fail, &resp);
        assert!(!outcome.pass);
        assert_eq!(outcome.actual.as_deref(), Some("200"));
        assert!(outcome.error.is_some());
    }

    #[test]
    fn evaluate_status_ne() {
        let resp = response(200, vec![], "");
        assert!(evaluate(&Assertion::parse("status != 500").unwrap(), &resp).pass);
        assert!(!evaluate(&Assertion::parse("status != 200").unwrap(), &resp).pass);
    }

    #[test]
    fn evaluate_status_numeric_ops() {
        let resp = response(200, vec![], "");
        assert!(evaluate(&Assertion::parse("status < 300").unwrap(), &resp).pass);
        assert!(evaluate(&Assertion::parse("status > 100").unwrap(), &resp).pass);
        assert!(evaluate(&Assertion::parse("status <= 200").unwrap(), &resp).pass);
        assert!(evaluate(&Assertion::parse("status >= 200").unwrap(), &resp).pass);
        assert!(!evaluate(&Assertion::parse("status < 100").unwrap(), &resp).pass);
    }

    // --- evaluator: header ---

    #[test]
    fn evaluate_header_case_insensitive_eq_and_contains() {
        let resp = response(200, vec![("Content-Type", "application/json")], "");
        assert!(
            evaluate(
                &Assertion::parse("header:content-type == application/json").unwrap(),
                &resp
            )
            .pass
        );
        assert!(
            evaluate(
                &Assertion::parse("header:Content-Type contains json").unwrap(),
                &resp
            )
            .pass
        );
    }

    #[test]
    fn evaluate_header_missing_fails_value_op_with_reason() {
        let resp = response(200, vec![], "");
        let outcome = evaluate(&Assertion::parse("header:X-Foo == bar").unwrap(), &resp);
        assert!(!outcome.pass);
        assert!(outcome.error.is_some());
    }

    // --- evaluator: JSON path ---

    #[test]
    fn evaluate_json_path_eq_contains_numeric() {
        let resp = response(200, vec![], r#"{"data":{"id":42,"name":"ada"}}"#);
        assert!(evaluate(&Assertion::parse("$.data.id == 42").unwrap(), &resp).pass);
        assert!(evaluate(&Assertion::parse("$.data.name contains ad").unwrap(), &resp).pass);
        assert!(evaluate(&Assertion::parse("$.data.id > 40").unwrap(), &resp).pass);
        assert!(!evaluate(&Assertion::parse("$.data.id > 100").unwrap(), &resp).pass);
    }

    #[test]
    fn evaluate_json_path_extraction_error_fails_value_op() {
        let resp = response(200, vec![], "not json");
        let outcome = evaluate(&Assertion::parse("$.data.id == 1").unwrap(), &resp);
        assert!(!outcome.pass);
        assert!(outcome.actual.is_none());
        assert!(outcome.error.is_some());
    }

    // --- evaluator: exists/absent ---

    #[test]
    fn exists_passes_when_extraction_succeeds() {
        let resp = response(200, vec![], r#"{"data":{"id":42}}"#);
        assert!(evaluate(&Assertion::parse("$.data.id exists").unwrap(), &resp).pass);
        assert!(evaluate(&Assertion::parse("status exists").unwrap(), &resp).pass);
    }

    #[test]
    fn exists_fails_on_missing_key_and_null_leaf() {
        let resp = response(200, vec![], r#"{"data":{"id":null}}"#);
        assert!(!evaluate(&Assertion::parse("$.data.missing exists").unwrap(), &resp).pass);
        assert!(!evaluate(&Assertion::parse("$.data.id exists").unwrap(), &resp).pass);
    }

    #[test]
    fn absent_passes_on_missing_key_header_and_null_leaf() {
        let resp = response(200, vec![], r#"{"data":{"id":null}}"#);
        assert!(evaluate(&Assertion::parse("$.data.missing absent").unwrap(), &resp).pass);
        assert!(evaluate(&Assertion::parse("$.data.id absent").unwrap(), &resp).pass);
        assert!(evaluate(&Assertion::parse("header:X-Nope absent").unwrap(), &resp).pass);
    }

    #[test]
    fn absent_fails_when_value_present() {
        let resp = response(200, vec![], r#"{"data":{"id":42}}"#);
        let outcome = evaluate(&Assertion::parse("$.data.id absent").unwrap(), &resp);
        assert!(!outcome.pass);
        assert_eq!(outcome.actual.as_deref(), Some("42"));
    }

    #[test]
    fn absent_fails_on_malformed_target_rather_than_passing() {
        // A non-JSON body against a JSON-path target is a malformed-assertion
        // error, not a "not found" — `absent` must not pass here (spec: only a
        // not-found kind counts).
        let resp = response(200, vec![], "not json");
        let outcome = evaluate(&Assertion::parse("$.data.id absent").unwrap(), &resp);
        assert!(!outcome.pass);
        assert!(outcome.error.is_some());
    }

    // --- run_assertions / AssertionReport ---

    #[test]
    fn run_assertions_empty_set_passes_vacuously() {
        let resp = response(200, vec![], "");
        let report = run_assertions(&[], &resp);
        assert!(report.passed);
        assert_eq!(report.total, 0);
        assert_eq!(report.failed, 0);
        assert!(report.results.is_empty());
    }

    #[test]
    fn run_assertions_mixed_pass_fail_counts() {
        let resp = response(200, vec![], r#"{"data":{"id":42}}"#);
        let assertions = vec![
            Assertion::parse("status == 200").unwrap(),
            Assertion::parse("$.data.id == 1").unwrap(),
            Assertion::parse("$.data.id exists").unwrap(),
        ];
        let report = run_assertions(&assertions, &resp);
        assert!(!report.passed);
        assert_eq!(report.total, 3);
        assert_eq!(report.failed, 1);
        assert!(!report.results[1].pass);
    }

    #[test]
    fn assert_op_canonical_round_trips_through_serde() {
        for op in [
            AssertOp::Eq,
            AssertOp::Ne,
            AssertOp::Contains,
            AssertOp::Exists,
            AssertOp::Absent,
            AssertOp::Lt,
            AssertOp::Gt,
            AssertOp::Le,
            AssertOp::Ge,
        ] {
            let json = serde_json::to_string(&op).unwrap();
            let expected = format!("{:?}", op.canonical());
            assert_eq!(json, expected);
        }
    }

    // --- load-run (aggregate stats) assertions ---

    use crate::load::{LoadStats, StatTarget};

    #[test]
    fn parse_stats_assertion_reuses_grammar_and_validates_target() {
        let a = parse_stats_assertion("stats.p95 < 500").unwrap();
        assert_eq!(a.target, StatTarget::P95);
        assert_eq!(a.raw_target, "stats.p95");
        assert_eq!(a.op, AssertOp::Lt);
        assert_eq!(a.value.as_deref(), Some("500"));
        // The `median` alias resolves to p50 but echoes the user's spelling.
        let m = parse_stats_assertion("stats.median <= 100").unwrap();
        assert_eq!(m.target, StatTarget::P50);
        assert_eq!(m.raw_target, "stats.median");
    }

    #[test]
    fn parse_stats_assertion_rejects_unknown_and_non_stats_targets() {
        assert!(matches!(
            parse_stats_assertion("stats.bogus < 1"),
            Err(StatAssertError::UnknownTarget { .. })
        ));
        // A response target (no `stats.` prefix) is not a load stat.
        assert!(matches!(
            parse_stats_assertion("status == 200"),
            Err(StatAssertError::UnknownTarget { .. })
        ));
    }

    #[test]
    fn parse_stats_assertion_surfaces_shared_grammar_errors() {
        assert!(matches!(
            parse_stats_assertion("stats.p95"),
            Err(StatAssertError::Grammar(
                AssertParseError::MissingOperator { .. }
            ))
        ));
        assert!(matches!(
            parse_stats_assertion("stats.p95 ?? 5"),
            Err(StatAssertError::Grammar(AssertParseError::UnknownOp { .. }))
        ));
    }

    #[test]
    fn run_stats_assertions_numeric_pass_and_fail() {
        // ok=3 failed=1 → attempted 4, error_rate 0.25 (exact in f64).
        let s = LoadStats {
            ok: 3,
            failed: 1,
            ..LoadStats::default()
        };
        let pass = [
            parse_stats_assertion("stats.error_rate <= 0.25").unwrap(),
            parse_stats_assertion("stats.count == 4").unwrap(),
            parse_stats_assertion("stats.ok == 3").unwrap(),
        ];
        let report = run_stats_assertions(&pass, &s, Some(10.0));
        assert!(report.passed, "{report:?}");
        assert_eq!(report.total, 3);
        assert_eq!(report.failed, 0);

        let fail = [parse_stats_assertion("stats.error_rate < 0.25").unwrap()];
        let report = run_stats_assertions(&fail, &s, Some(10.0));
        assert!(!report.passed);
        assert_eq!(report.failed, 1);
        // The report echoes the resolved rate as `actual`.
        assert_eq!(report.results[0].actual.as_deref(), Some("0.25"));
    }

    #[test]
    fn run_stats_assertions_undefined_latency_fails_value_op() {
        // All errored → no completed request → p95 is undefined.
        let s = LoadStats {
            errored: 3,
            ..LoadStats::default()
        };
        let a = [parse_stats_assertion("stats.p95 < 500").unwrap()];
        let report = run_stats_assertions(&a, &s, None);
        assert!(!report.passed);
        assert!(
            report.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("undefined")
        );
        assert!(report.results[0].actual.is_none());
    }

    #[test]
    fn run_stats_assertions_exists_absent_track_definedness() {
        let defined = LoadStats {
            ok: 1,
            p95: Some(std::time::Duration::from_millis(120)),
            ..LoadStats::default()
        };
        let a = [parse_stats_assertion("stats.p95 exists").unwrap()];
        assert!(run_stats_assertions(&a, &defined, Some(5.0)).passed);

        let undefined = LoadStats {
            errored: 1,
            ..LoadStats::default()
        };
        let a = [parse_stats_assertion("stats.p95 absent").unwrap()];
        assert!(run_stats_assertions(&a, &undefined, None).passed);
        // A count is always defined → `absent` must fail on it.
        let a = [parse_stats_assertion("stats.count absent").unwrap()];
        assert!(!run_stats_assertions(&a, &undefined, None).passed);
    }
}
