//! Save-time secret detection and the grandfather/warn decision.
//!
//! A churl workspace is a git repo users commit, so literal secrets must never be
//! *authored* into a synced TOML file. This module scans the content a save would
//! write, classifies each finding by how reliable the signal is, and — comparing
//! against the on-disk baseline — decides which findings refuse the write and
//! which merely warn.
//!
//! Two axes drive the decision:
//!
//! - **Severity** ([`Severity`]) — how a *fresh* finding is treated under the
//!   strict policy. [`Severity::Block`] is name-anchored (a secret-named field
//!   carrying a literal): reliable enough to refuse a new write. [`Severity::Warn`]
//!   is value-only (a secret-*shaped* value under an innocent name, or a request
//!   body): too noisy to block, so it only warns.
//! - **Novelty** — keyed by a finding's [`location`](SecretFinding::location)
//!   string (field path / var name). A location that was *already* a violation in
//!   the baseline is pre-existing (grandfathered → warn); a location clean or
//!   absent in the baseline is new (this save introduced it). A brand-new file
//!   has no baseline, so every finding is new.
//!
//! The policy knob ([`SecretPolicy`]) selects the regime: [`SecretPolicy::Strict`]
//! (default) blocks new name-anchored findings and warns on the rest;
//! [`SecretPolicy::Warn`] warns on everything and blocks nothing.
//!
//! Detection lives here in churl-core (pure, TUI-free); rendering the resulting
//! `!` markers / warning text is the TUI's job. **Sending is never gated** — this
//! is a save-time concern only.

use crate::config::{is_template_placeholder, looks_like_secret_name};
use crate::model::{Auth, CollectionMeta, Endpoint, Workspace};
use crate::template::contains_placeholder;

/// How a *newly-introduced* secret finding is treated under [`SecretPolicy::Strict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Name-anchored and reliable: a secret-*named* location carrying a literal.
    /// A new one refuses the write under strict; a pre-existing one is grandfathered.
    Block,
    /// Value-only and noisy: a secret-*shaped* value under an innocent name, or a
    /// request body. Always warns, never blocks — even when new under strict.
    Warn,
}

/// One secret detected in content a save would write. The `location` string is
/// both the human-facing field path (`"auth.password"`, `"headers.Authorization"`,
/// `"vars.api_key"`) *and* the novelty key: a finding is pre-existing iff its
/// location also appears in the baseline scan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecretFinding {
    /// Field path / var name — stable across saves so novelty compares correctly.
    pub location: String,
    /// How a fresh finding at this location is treated under strict.
    pub severity: Severity,
}

impl SecretFinding {
    fn block(location: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            severity: Severity::Block,
        }
    }

    fn warn(location: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            severity: Severity::Warn,
        }
    }
}

/// The workspace secret policy, resolved from `secret_policy` in the global
/// config. Default (and the safe choice) is [`SecretPolicy::Strict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecretPolicy {
    /// Block a *new* name-anchored literal; warn on value-only findings and on all
    /// pre-existing (grandfathered) findings.
    #[default]
    Strict,
    /// Warn on everything, block nothing — the opt-in escape hatch.
    Warn,
}

/// The outcome of [`decide`]: the findings that refuse the save, and the findings
/// that only warn. A save proceeds iff [`refusals`](Self::refusals) is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecretDecision {
    /// Findings that refuse the write (new + [`Severity::Block`] under strict).
    /// Empty means the save may proceed.
    pub refusals: Vec<SecretFinding>,
    /// Findings that permit the write but should surface a warning / `!` marker
    /// (grandfathered pre-existing, value-only, or everything under warn policy).
    pub warnings: Vec<SecretFinding>,
}

impl SecretDecision {
    /// Whether the save is refused (has at least one refusing finding).
    pub fn is_refused(&self) -> bool {
        !self.refusals.is_empty()
    }

    /// The refusing findings' locations, for an error message.
    pub fn refusal_locations(&self) -> Vec<String> {
        self.refusals.iter().map(|f| f.location.clone()).collect()
    }

    /// The warning findings' locations, for a `!` marker message.
    pub fn warning_locations(&self) -> Vec<String> {
        self.warnings.iter().map(|f| f.location.clone()).collect()
    }
}

/// Classifies each scanned finding into [`refusals`](SecretDecision::refusals) /
/// [`warnings`](SecretDecision::warnings) by novelty (against `baseline`) and
/// `policy`. `new` is the scan of the content being saved; `baseline` is the scan
/// of the same file's current on-disk content (empty for a brand-new file).
///
/// A finding refuses the save exactly when: the policy is
/// [`SecretPolicy::Strict`], the finding is [`Severity::Block`], **and** its
/// location was not already a violation in the baseline. Everything else warns.
pub fn decide(
    new: &[SecretFinding],
    baseline: &[SecretFinding],
    policy: SecretPolicy,
) -> SecretDecision {
    let mut decision = SecretDecision::default();
    for finding in new {
        let pre_existing = baseline.iter().any(|b| b.location == finding.location);
        let blocks =
            policy == SecretPolicy::Strict && finding.severity == Severity::Block && !pre_existing;
        if blocks {
            decision.refusals.push(finding.clone());
        } else {
            decision.warnings.push(finding.clone());
        }
    }
    decision
}

// --- Value-shape heuristic ---

/// Returns `true` when `value` *looks like* a secret from its shape alone,
/// independent of the name it sits under. Deliberately high-confidence /
/// low-false-positive — a value-only signal only ever warns, but a noisy detector
/// would still be annoying:
///
/// - Vendor-prefixed tokens: `sk-`/`pk-` (Stripe-style), `ghp_`/`gho_`/`ghu_`/
///   `ghs_`/`ghr_` (GitHub PATs), `xox` + one of `bpoas` (Slack), `AKIA` (AWS
///   access key id) — each a well-known credential prefix on a token body.
/// - JWTs: three base64url segments joined by `.`, first segment starting `eyJ`
///   (the `{"` header preamble).
/// - Long high-entropy runs: a single unbroken run of ≥32 chars that is entirely
///   base64/hex alphabet *and* mixes cases or includes digits (a pure lowercase
///   word like a 40-char sentence-free identifier still needs case/digit mixing
///   to trip, keeping ordinary long slugs out).
///
/// A `{{placeholder}}` is never secret-shaped (checked by callers before this,
/// but re-guarded here for safety).
pub fn looks_like_secret_value(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() || is_template_placeholder(v) {
        return false;
    }
    if has_vendor_prefix(v) || is_jwt(v) || has_high_entropy_run(v) {
        return true;
    }
    false
}

/// Well-known credential prefixes on a plausible token body (≥16 chars total).
fn has_vendor_prefix(v: &str) -> bool {
    const PREFIXES: &[&str] = &["sk-", "pk-", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "AKIA"];
    if v.len() >= 16 && PREFIXES.iter().any(|p| v.starts_with(p)) && v.chars().all(is_token_char) {
        return true;
    }
    // Slack tokens: `xox` + a kind letter + `-` … .
    if v.len() >= 16
        && v.starts_with("xox")
        && v.as_bytes().get(3).is_some_and(|b| b"bpoas".contains(b))
    {
        return true;
    }
    false
}

/// A three-segment JWT: `eyJ<base64url>.<base64url>.<base64url>`.
fn is_jwt(v: &str) -> bool {
    let segments: Vec<&str> = v.split('.').collect();
    segments.len() == 3
        && v.starts_with("eyJ")
        && segments
            .iter()
            .all(|s| s.len() >= 4 && s.chars().all(is_base64url_char))
}

/// A single unbroken run of ≥32 base64/hex chars that mixes cases or includes
/// digits — high-confidence random material, not a plain long word.
fn has_high_entropy_run(v: &str) -> bool {
    // The whole trimmed value must be one contiguous token (no spaces): a run
    // embedded in a sentence is not scanned here (bodies warn wholesale elsewhere).
    if v.len() < 32 || !v.chars().all(is_base64url_char) {
        return false;
    }
    let has_upper = v.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = v.chars().any(|c| c.is_ascii_lowercase());
    let has_digit = v.chars().any(|c| c.is_ascii_digit());
    (has_upper && has_lower) || has_digit
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

fn is_base64url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '=')
}

// --- Scans ---

/// Scans an [`Endpoint`]'s request for secret findings across every field a save
/// would persist: auth (name-anchored), header values, URL (query keys +
/// userinfo), query params, and the body (value-only). Names are never treated as
/// values; a `{{placeholder}}` value is never a finding.
pub fn scan_endpoint(ep: &Endpoint) -> Vec<SecretFinding> {
    let mut findings = Vec::new();
    let req = &ep.request;

    // Auth — name-anchored, the original reliable signal.
    match &req.auth {
        Some(Auth::Basic { password, .. }) if !is_template_placeholder(password) => {
            findings.push(SecretFinding::block("auth.password"));
        }
        Some(Auth::Bearer { token }) if !is_template_placeholder(token) => {
            findings.push(SecretFinding::block("auth.token"));
        }
        Some(Auth::ApiKey { name, value, .. })
            if looks_like_secret_name(name) && !is_template_placeholder(value) =>
        {
            findings.push(SecretFinding::block("auth.value"));
        }
        _ => {}
    }

    // Headers — a secret-named header carrying a literal is name-anchored. Header
    // values embed the credential (`Authorization: Bearer <tok>`), so a value that
    // *contains* any `{{placeholder}}` is templated and clean, not just a bare
    // whole-value placeholder.
    for header in &req.headers {
        if contains_placeholder(&header.value) {
            continue;
        }
        if looks_like_secret_name(&header.name) {
            findings.push(SecretFinding::block(format!("headers.{}", header.name)));
        } else if looks_like_secret_value(&header.value) {
            findings.push(SecretFinding::warn(format!("headers.{}", header.name)));
        }
    }

    // URL — userinfo (`user:pass@`) and secret-looking query keys are
    // name-anchored; other query values may be secret-shaped (warn).
    findings.extend(scan_url(&req.url));

    // Query params — mirror the URL query-key logic on the structured params
    // (values embed the credential, so a contained placeholder is templated).
    for param in &req.params {
        if contains_placeholder(&param.value) {
            continue;
        }
        if looks_like_secret_name(&param.name) {
            findings.push(SecretFinding::block(format!("params.{}", param.name)));
        } else if looks_like_secret_value(&param.value) {
            findings.push(SecretFinding::warn(format!("params.{}", param.name)));
        }
    }

    // Body — value-only, always a warn (no reliable name anchor).
    if let Some(body) = &req.body
        && !is_template_placeholder(&body.content)
        && looks_like_secret_value_in_text(&body.content)
    {
        findings.push(SecretFinding::warn("body"));
    }

    findings
}

/// Scans a URL string for secret material: `user:pass@` userinfo (name-anchored,
/// a literal password embedded in the URL) and query keys that look secret with a
/// literal value (name-anchored), plus secret-*shaped* query values (warn).
fn scan_url(url: &str) -> Vec<SecretFinding> {
    let mut findings = Vec::new();

    // Userinfo: `scheme://user:pass@host`. A literal password in the authority is
    // a name-anchored secret; a `{{placeholder}}` password is not.
    if let Some(authority) = url_authority(url)
        && let Some((userinfo, _)) = authority.split_once('@')
        && let Some((_, pass)) = userinfo.split_once(':')
        && !pass.is_empty()
        && !contains_placeholder(pass)
    {
        findings.push(SecretFinding::block("url.userinfo"));
    }

    // Query string: split each `key=value` pair. A value that embeds a
    // `{{placeholder}}` is templated and clean.
    if let Some((_, query)) = url.split_once('?') {
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key.is_empty() || value.is_empty() || contains_placeholder(value) {
                continue;
            }
            if looks_like_secret_name(key) {
                findings.push(SecretFinding::block(format!("url.query.{key}")));
            } else if looks_like_secret_value(value) {
                findings.push(SecretFinding::warn(format!("url.query.{key}")));
            }
        }
    }

    findings
}

/// The authority component of a URL (`//authority/...`), if present.
fn url_authority(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest)?;
    Some(match after_scheme.find(['/', '?', '#']) {
        Some(end) => &after_scheme[..end],
        None => after_scheme,
    })
}

/// The token substituted for a detected secret span when redacting text for
/// display (the M8.2 headless request-echo path). Six bullets, matching the
/// TUI's own secret mask so redaction reads consistently across surfaces.
pub const SECRET_MASK: &str = "••••••";

/// Masks a header value when its **name** is a known auth-bearing name
/// (`authorization`, `cookie`) or otherwise looks secret-named
/// ([`looks_like_secret_name`]), or when its **value** looks secret-shaped
/// ([`looks_like_secret_value`]).
///
/// Mirrors the redirect-strip dual-anchor policy (see DECISIONS.md,
/// "Cross-origin redirect policy") applied to any REQUEST-header display
/// surface — the M8.2 headless JSON envelope's echoed `request.headers`, the
/// M8.3 debug trace / Inspector, and copy-as-resolved-curl — so a resolved
/// `{{token}}`/session-captured value never round-trips back out to a
/// display surface, even though the real outgoing request sent it. The URL is
/// masked by its own twin, [`mask_url`]; a request body has no name anchor and
/// is not masked by this function.
pub fn mask_header_value(name: &str, value: &str) -> String {
    const ALWAYS_AUTH_NAMES: [&str; 2] = ["authorization", "cookie"];
    let name_hit = ALWAYS_AUTH_NAMES
        .iter()
        .any(|n| n.eq_ignore_ascii_case(name))
        || looks_like_secret_name(name);
    let value_hit = looks_like_secret_value(value);
    if name_hit || value_hit {
        SECRET_MASK.to_owned()
    } else {
        value.to_owned()
    }
}

/// Returns `url` with every embedded secret span replaced by [`SECRET_MASK`]:
/// the `user:PASSWORD@` userinfo password (a literal, non-placeholder password)
/// and each secret query value — a secret-*named* key's literal value, or a
/// secret-*shaped* value under any key. Non-secret spans are left byte-identical.
///
/// This is the redaction twin of [`scan_url`]: it mirrors that scanner's exact
/// detection spans, so what `scan_url` would flag as a secret is precisely what
/// this masks. It exists for echoing a *resolved* request URL back to a caller
/// (the `churl send`/`run` `--json` envelope + `-v` trace) without leaking a
/// secret that `{{var}}` substitution wrote into the URL (`user:pass@` or
/// `?api_key={{secret}}` → the real value). A `{{placeholder}}` span (never a
/// secret) and any non-secret query value are untouched.
pub fn mask_url(url: &str) -> String {
    let mut out = url.to_owned();

    // Userinfo password: `scheme://user:PASSWORD@host`. Mask a literal
    // (non-placeholder) password span, keeping the username + host visible.
    if let Some(authority) = url_authority(url)
        && let Some((userinfo, host)) = authority.split_once('@')
        && let Some((user, pass)) = userinfo.split_once(':')
        && !pass.is_empty()
        && !contains_placeholder(pass)
    {
        // `out` still equals `url` here (query masking runs after), so the
        // authority substring matches; it appears before the path, so the first
        // occurrence is the right one.
        out = out.replacen(authority, &format!("{user}:{SECRET_MASK}@{host}"), 1);
    }

    // Query string: mask each secret pair's value, preserving key order,
    // separators, and any trailing `#fragment`.
    if let Some(qpos) = out.find('?') {
        let (head, rest) = out.split_at(qpos);
        let query_and_frag = &rest[1..];
        let (query, frag) = match query_and_frag.split_once('#') {
            Some((q, f)) => (q, Some(f)),
            None => (query_and_frag, None),
        };
        let masked_query = query
            .split('&')
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                if key.is_empty() || value.is_empty() || contains_placeholder(value) {
                    pair.to_owned()
                } else if looks_like_secret_name(key) || looks_like_secret_value(value) {
                    format!("{key}={SECRET_MASK}")
                } else {
                    pair.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("&");
        let mut rebuilt = format!("{head}?{masked_query}");
        if let Some(frag) = frag {
            rebuilt.push('#');
            rebuilt.push_str(frag);
        }
        out = rebuilt;
    }

    out
}

/// Whether any whitespace-delimited token in free text is secret-shaped. Used for
/// request bodies, where there is no name anchor — a single high-confidence token
/// anywhere warns.
fn looks_like_secret_value_in_text(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | '{' | '}' | ':'))
        .any(looks_like_secret_value)
}

/// Scans a [`Workspace`]'s `[vars]` and every profile's vars. A secret-*named*
/// literal is name-anchored (block); an innocent-named secret-*shaped* value warns.
/// Locations mirror the on-disk paths (`"vars.<name>"`, `"<profile>.<name>"`) so
/// they match the env-editor's baseline scan.
pub fn scan_workspace(ws: &Workspace) -> Vec<SecretFinding> {
    let mut findings = scan_vars("vars", &ws.vars);
    for profile in &ws.profiles {
        findings.extend(scan_vars(&profile.name, &profile.vars));
    }
    findings
}

/// Scans a collection's `folder.toml` `[vars]` (prefixed `"vars"`), mirroring
/// [`scan_workspace`] for the collection scope.
pub fn scan_collection(meta: &CollectionMeta) -> Vec<SecretFinding> {
    scan_vars("vars", &meta.vars)
}

/// Scans a flat `name → value` var map: a secret-named literal blocks; an
/// innocent-named secret-shaped value warns. `{{placeholder}}` values are clean.
fn scan_vars<'a>(
    prefix: &str,
    vars: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Vec<SecretFinding> {
    let mut findings = Vec::new();
    for (name, value) in vars {
        if is_template_placeholder(value) {
            continue;
        }
        if looks_like_secret_name(name) {
            findings.push(SecretFinding::block(format!("{prefix}.{name}")));
        } else if looks_like_secret_value(value) {
            findings.push(SecretFinding::warn(format!("{prefix}.{name}")));
        }
    }
    findings
}

#[cfg(test)]
mod tests;
