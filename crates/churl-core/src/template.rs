//! `{{var}}` template resolution: the single seam every substitution flows
//! through (the plugin-guardrail — plugin template *functions* will extend the
//! lookup here, not scattered call sites).
//!
//! A [`Resolver`] holds an ordered list of [`Scope`]s (highest precedence first)
//! and falls through to the process environment last. [`Resolver::substitute`]
//! replaces every `{{name}}` occurrence in a string; an unresolved placeholder is
//! left **verbatim** (consistent with the send-verbatim behaviour — no error).
//! [`Resolver::substitute_request`] applies the same substitution across a
//! [`Request`]'s templatable fields.
//!
//! Placeholder syntax matches [`crate::config::is_template_placeholder`]: `{{`,
//! the name, `}}`, with inner whitespace trimmed; name characters are
//! `[A-Za-z0-9_.-]`. No nesting, no functions (functions are not yet supported).

use std::collections::BTreeMap;

use crate::model::{Auth, Request};

/// One named lookup layer inside a [`Resolver`]: a flat variable map. Scopes are
/// ordered by precedence in the resolver (earlier scopes win).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// Diagnostic scope name (`"cli"`, `"profile"`, `"collection"`, `"workspace"`).
    pub name: &'static str,
    /// Variable name → value map for this scope.
    pub vars: BTreeMap<String, String>,
}

impl Scope {
    /// Builds a named scope from a variable map.
    pub fn new(name: &'static str, vars: BTreeMap<String, String>) -> Self {
        Self { name, vars }
    }
}

/// Resolves `{{var}}` placeholders over an ordered scope list, with the process
/// environment as the implicit final fallback.
#[derive(Debug, Clone, Default)]
pub struct Resolver {
    scopes: Vec<Scope>,
}

impl Resolver {
    /// Builds a resolver from scopes ordered highest → lowest precedence. The
    /// process environment is consulted last, after every scope, and is never
    /// snapshotted (looked up live per name).
    pub fn new(scopes: Vec<Scope>) -> Self {
        Self { scopes }
    }

    /// Resolves a single variable name: the first scope that defines it wins;
    /// otherwise the process environment (`std::env::var`) is consulted; `None`
    /// when nothing resolves it.
    pub fn resolve(&self, name: &str) -> Option<String> {
        for scope in &self.scopes {
            if let Some(value) = scope.vars.get(name) {
                return Some(value.clone());
            }
        }
        std::env::var(name).ok()
    }

    /// Substitutes every `{{name}}` placeholder in `input`. Unresolved
    /// placeholders (and malformed `{{` runs) are left verbatim.
    pub fn substitute(&self, input: &str) -> String {
        substitute_with(input, |name| self.resolve(name))
    }

    /// Substitutes placeholders across a request's templatable fields in place:
    /// `url`, every header *value*, every param *value*, the body content, and
    /// all auth string fields (basic username + password, bearer token, apikey
    /// name + value). Header and param *names* are never substituted.
    pub fn substitute_request(&self, req: &mut Request) {
        req.url = self.substitute(&req.url);
        for header in &mut req.headers {
            header.value = self.substitute(&header.value);
        }
        for param in &mut req.params {
            param.value = self.substitute(&param.value);
        }
        if let Some(body) = req.body.as_mut() {
            body.content = self.substitute(&body.content);
        }
        if let Some(auth) = req.auth.as_mut() {
            match auth {
                Auth::Basic { username, password } => {
                    *username = self.substitute(username);
                    *password = self.substitute(password);
                }
                Auth::Bearer { token } => {
                    *token = self.substitute(token);
                }
                Auth::ApiKey { name, value, .. } => {
                    *name = self.substitute(name);
                    *value = self.substitute(value);
                }
            }
        }
    }
}

/// Names of every well-formed `{{name}}` placeholder still present in a
/// substituted [`Request`], deduplicated and sorted — empty when the request is
/// fully resolved.
///
/// This is the "fail loud" seam: run it *after* [`Resolver::substitute_request`].
/// Any name it returns is a variable no scope (nor the process env) resolved, so
/// the literal `{{name}}` would otherwise ship on the wire (a leaked `{{token}}`
/// header is silently wrong). Call sites should refuse the send and surface the
/// names. It reuses the SAME delimiter scan as substitution ([`parse_placeholder`]),
/// so what it flags is exactly what substitution would have replaced — a malformed
/// brace run (`{{ }}`, `{{a b}}`, an unclosed `{{`) is literal text at both stages,
/// never flagged. Fields scanned mirror [`Resolver::substitute_request`] exactly:
/// `url`, every header *value*, every param *value*, the body content, and all
/// auth string fields (basic username + password, bearer token, apikey name +
/// value). Header and param *names* are not substituted, so they are not scanned.
pub fn unresolved_placeholders(req: &Request) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push_from = |s: &str| collect_placeholder_names(s, &mut names);

    push_from(&req.url);
    for header in &req.headers {
        push_from(&header.value);
    }
    for param in &req.params {
        push_from(&param.value);
    }
    if let Some(body) = req.body.as_ref() {
        push_from(&body.content);
    }
    if let Some(auth) = req.auth.as_ref() {
        match auth {
            Auth::Basic { username, password } => {
                push_from(username);
                push_from(password);
            }
            Auth::Bearer { token } => push_from(token),
            Auth::ApiKey { name, value, .. } => {
                push_from(name);
                push_from(value);
            }
        }
    }

    names.sort();
    names.dedup();
    names
}

/// Appends the *trimmed* name of every well-formed `{{name}}` placeholder in
/// `input` to `names`, using the same scan as [`substitute_with`] so the two can
/// never disagree about what is a placeholder. Duplicates are left for the caller
/// to dedup.
fn collect_placeholder_names(input: &str, names: &mut Vec<String>) {
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if bytes[i] == b'{'
            && i + 1 < input.len()
            && bytes[i + 1] == b'{'
            && let Some((name, end)) = parse_placeholder(input, i)
        {
            names.push(name.trim().to_string());
            i = end;
            continue;
        }
        let ch = input[i..].chars().next().expect("index on char boundary");
        i += ch.len_utf8();
    }
}

/// Returns `true` for the characters allowed in a placeholder name
/// (`[A-Za-z0-9_.-]`).
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Core delimiter scan: replaces each well-formed `{{name}}` in `input` with
/// `lookup(name)` when it resolves, leaving everything else (including
/// unresolved or malformed placeholders) verbatim. Shared by [`Resolver`] so the
/// scan logic lives in exactly one place.
fn substitute_with(input: &str, mut lookup: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if bytes[i] == b'{'
            && i + 1 < input.len()
            && bytes[i + 1] == b'{'
            && let Some((name, end)) = parse_placeholder(input, i)
        {
            match lookup(name.trim()) {
                Some(value) => out.push_str(&value),
                None => out.push_str(&input[i..end]),
            }
            i = end;
            continue;
        }
        // Not a placeholder start: copy this char verbatim.
        let ch = input[i..].chars().next().expect("index on char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parses a `{{ name }}` placeholder starting at byte index `start` (which points
/// at the first `{`). Returns the inner name slice (untrimmed) and the byte index
/// just past the closing `}}`, or `None` when the run is not a well-formed
/// placeholder (empty name, illegal name char, or no closing `}}`).
fn parse_placeholder(input: &str, start: usize) -> Option<(&str, usize)> {
    let inner_start = start + 2; // skip "{{"
    let rest = &input[inner_start..];
    let close = rest.find("}}")?;
    let name = &rest[..close];
    let trimmed = name.trim();
    if trimmed.is_empty() || !trimmed.chars().all(is_name_char) {
        return None;
    }
    Some((name, inner_start + close + 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiKeyPlacement, Body, BodyKind, Header, Method, Param};

    fn scope(name: &'static str, pairs: &[(&str, &str)]) -> Scope {
        Scope::new(
            name,
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn each_scope_beats_the_ones_below() {
        // cli > profile > collection > workspace, all defining `x`.
        let resolver = Resolver::new(vec![
            scope("cli", &[("x", "cli")]),
            scope("profile", &[("x", "profile"), ("y", "profile")]),
            scope("collection", &[("x", "collection"), ("z", "collection")]),
            scope("workspace", &[("x", "workspace"), ("w", "workspace")]),
        ]);
        assert_eq!(resolver.resolve("x").as_deref(), Some("cli"));
        assert_eq!(resolver.resolve("y").as_deref(), Some("profile"));
        assert_eq!(resolver.resolve("z").as_deref(), Some("collection"));
        assert_eq!(resolver.resolve("w").as_deref(), Some("workspace"));
    }

    #[test]
    fn session_scope_beats_profile_and_below() {
        // Note #6: the standalone chain is `session > cli > profile > … > env`.
        // A var defined in both a profile and the session resolves to the session.
        let resolver = Resolver::new(vec![
            scope("session", &[("token", "session")]),
            scope("cli", &[("token", "cli")]),
            scope("profile", &[("token", "profile")]),
            scope("workspace", &[("token", "workspace")]),
        ]);
        assert_eq!(resolver.resolve("token").as_deref(), Some("session"));
        // With no session value, cli wins (session sits above cli but is empty).
        let resolver = Resolver::new(vec![
            scope("session", &[]),
            scope("cli", &[("token", "cli")]),
            scope("profile", &[("token", "profile")]),
        ]);
        assert_eq!(resolver.resolve("token").as_deref(), Some("cli"));
    }

    #[test]
    fn env_is_the_last_fallback() {
        // SAFETY: single-threaded test; unique var name.
        unsafe { std::env::set_var("CHURL_TEST_ENV_VAR", "from-env") };
        let resolver = Resolver::new(vec![scope("workspace", &[("other", "v")])]);
        assert_eq!(
            resolver.resolve("CHURL_TEST_ENV_VAR").as_deref(),
            Some("from-env")
        );
        // A scope value wins over env.
        let resolver = Resolver::new(vec![scope("cli", &[("CHURL_TEST_ENV_VAR", "scoped")])]);
        assert_eq!(
            resolver.resolve("CHURL_TEST_ENV_VAR").as_deref(),
            Some("scoped")
        );
        unsafe { std::env::remove_var("CHURL_TEST_ENV_VAR") };
    }

    #[test]
    fn unresolved_placeholder_left_verbatim() {
        let resolver = Resolver::new(vec![scope("workspace", &[("known", "v")])]);
        assert_eq!(resolver.substitute("{{unknown}}"), "{{unknown}}");
        assert_eq!(
            resolver.substitute("a {{known}} {{missing}} z"),
            "a v {{missing}} z"
        );
    }

    #[test]
    fn multiple_occurrences_in_one_string() {
        let resolver = Resolver::new(vec![scope("workspace", &[("h", "example.com")])]);
        assert_eq!(
            resolver.substitute("https://{{h}}/a and https://{{h}}/b"),
            "https://example.com/a and https://example.com/b"
        );
    }

    #[test]
    fn inner_whitespace_is_trimmed() {
        let resolver = Resolver::new(vec![scope("workspace", &[("base", "X")])]);
        assert_eq!(resolver.substitute("{{ base }}"), "X");
        assert_eq!(resolver.substitute("{{base}}"), "X");
    }

    #[test]
    fn malformed_runs_are_left_verbatim() {
        let resolver = Resolver::new(vec![scope("workspace", &[("x", "1")])]);
        // No closing braces.
        assert_eq!(resolver.substitute("{{x"), "{{x");
        // Empty name.
        assert_eq!(resolver.substitute("{{}}"), "{{}}");
        // Illegal char in name.
        assert_eq!(resolver.substitute("{{a b}}"), "{{a b}}");
        // A lone brace pair is untouched.
        assert_eq!(resolver.substitute("{ {x} }"), "{ {x} }");
    }

    #[test]
    fn substitute_request_hits_all_fields_but_not_names() {
        let resolver = Resolver::new(vec![scope(
            "workspace",
            &[
                ("host", "api.test"),
                ("hv", "app/json"),
                ("pv", "42"),
                ("body", "payload"),
                ("user", "alice"),
                ("pass", "s3cr3t"),
                ("hname", "SHOULD_NOT_APPEAR"),
            ],
        )]);
        let mut req = Request {
            method: Method::Post,
            url: "https://{{host}}/x".into(),
            headers: vec![Header {
                name: "{{hname}}".into(),
                value: "{{hv}}".into(),
                enabled: true,
            }],
            params: vec![Param {
                name: "{{hname}}".into(),
                value: "{{pv}}".into(),
                enabled: true,
            }],
            body: Some(Body {
                kind: BodyKind::Text,
                content: "{{body}}".into(),
            }),
            auth: Some(Auth::Basic {
                username: "{{user}}".into(),
                password: "{{pass}}".into(),
            }),
        };
        resolver.substitute_request(&mut req);
        assert_eq!(req.url, "https://api.test/x");
        // Names untouched, values substituted.
        assert_eq!(req.headers[0].name, "{{hname}}");
        assert_eq!(req.headers[0].value, "app/json");
        assert_eq!(req.params[0].name, "{{hname}}");
        assert_eq!(req.params[0].value, "42");
        assert_eq!(req.body.unwrap().content, "payload");
        match req.auth.unwrap() {
            Auth::Basic { username, password } => {
                assert_eq!(username, "alice");
                assert_eq!(password, "s3cr3t");
            }
            _ => panic!("wrong auth kind"),
        }
    }

    /// A helper: a plain request whose only templatable content is `url`.
    fn req_with_url(url: &str) -> Request {
        Request {
            method: Method::Get,
            url: url.into(),
            headers: vec![],
            params: vec![],
            body: None,
            auth: None,
        }
    }

    #[test]
    fn unresolved_none_when_fully_resolved() {
        // A request with no placeholders left is clean.
        let req = req_with_url("https://api.test/users/42");
        assert!(unresolved_placeholders(&req).is_empty());
    }

    #[test]
    fn unresolved_reports_names_across_every_field() {
        // A leftover placeholder in each templatable field is reported once, sorted,
        // deduped; header/param NAMES are not scanned (they are never substituted).
        let req = Request {
            method: Method::Post,
            url: "https://{{host}}/x".into(),
            headers: vec![Header {
                name: "{{hname_ignored}}".into(),
                value: "{{hval}}".into(),
                enabled: true,
            }],
            params: vec![Param {
                name: "{{pname_ignored}}".into(),
                value: "{{pval}}".into(),
                enabled: true,
            }],
            body: Some(Body {
                kind: BodyKind::Json,
                // `host` again — proves dedup across fields.
                content: "{\"a\": \"{{bodyvar}}\", \"b\": \"{{host}}\"}".into(),
            }),
            auth: Some(Auth::Basic {
                username: "{{user}}".into(),
                password: "{{pass}}".into(),
            }),
        };
        assert_eq!(
            unresolved_placeholders(&req),
            vec![
                "bodyvar".to_string(),
                "host".to_string(),
                "hval".to_string(),
                "pass".to_string(),
                "pval".to_string(),
                "user".to_string(),
            ]
        );
    }

    #[test]
    fn unresolved_reports_bearer_and_apikey_fields() {
        let bearer = Request {
            auth: Some(Auth::Bearer {
                token: "{{token}}".into(),
            }),
            ..req_with_url("https://api.test")
        };
        assert_eq!(unresolved_placeholders(&bearer), vec!["token".to_string()]);

        let apikey = Request {
            auth: Some(Auth::ApiKey {
                name: "{{keyname}}".into(),
                value: "{{keyval}}".into(),
                placement: ApiKeyPlacement::Header,
            }),
            ..req_with_url("https://api.test")
        };
        assert_eq!(
            unresolved_placeholders(&apikey),
            vec!["keyname".to_string(), "keyval".to_string()]
        );
    }

    #[test]
    fn unresolved_ignores_escaped_literal_double_braces() {
        // No escape convention exists in churl: a malformed brace run is literal
        // text at BOTH substitution and detection. None of these are well-formed
        // placeholder names, so `substitute` leaves them verbatim AND
        // `unresolved_placeholders` must NOT flag them.
        let cases = [
            "{{}}",    // empty name
            "{{a b}}", // illegal char (space) in name
            "{{x",     // unclosed
            "{ {x} }", // lone braces, not doubled
            "literal {{ not a name! }} text",
            "{{good.name-1_2}}", // <- this ONE is well-formed, flagged below
        ];
        for case in &cases[..cases.len() - 1] {
            let req = req_with_url(case);
            assert!(
                unresolved_placeholders(&req).is_empty(),
                "malformed/literal run should not be flagged: {case:?}"
            );
        }
        // Sanity: a genuinely well-formed unresolved name IS flagged (the escaping
        // negatives above are meaningful, not vacuous).
        let req = req_with_url("{{good.name-1_2}}");
        assert_eq!(
            unresolved_placeholders(&req),
            vec!["good.name-1_2".to_string()]
        );
    }

    #[test]
    fn unresolved_matches_what_substitution_would_replace() {
        // The detector and the substituter agree: after substituting with a resolver
        // that knows `host`, only the still-unknown `missing` remains — and that is
        // exactly what the detector reports.
        let resolver = Resolver::new(vec![scope("workspace", &[("host", "api.test")])]);
        let mut req = req_with_url("https://{{host}}/{{missing}}");
        resolver.substitute_request(&mut req);
        assert_eq!(req.url, "https://api.test/{{missing}}");
        assert_eq!(unresolved_placeholders(&req), vec!["missing".to_string()]);
    }

    #[test]
    fn substitute_request_hits_bearer_and_apikey() {
        let resolver = Resolver::new(vec![scope(
            "workspace",
            &[("tok", "TOKEN"), ("kn", "X-Key"), ("kv", "VALUE")],
        )]);
        let mut bearer = Request {
            method: Method::Get,
            url: "u".into(),
            headers: vec![],
            params: vec![],
            body: None,
            auth: Some(Auth::Bearer {
                token: "{{tok}}".into(),
            }),
        };
        resolver.substitute_request(&mut bearer);
        assert_eq!(
            bearer.auth.unwrap(),
            Auth::Bearer {
                token: "TOKEN".into()
            }
        );

        let mut apikey = Request {
            method: Method::Get,
            url: "u".into(),
            headers: vec![],
            params: vec![],
            body: None,
            auth: Some(Auth::ApiKey {
                name: "{{kn}}".into(),
                value: "{{kv}}".into(),
                placement: ApiKeyPlacement::Header,
            }),
        };
        resolver.substitute_request(&mut apikey);
        assert_eq!(
            apikey.auth.unwrap(),
            Auth::ApiKey {
                name: "X-Key".into(),
                value: "VALUE".into(),
                placement: ApiKeyPlacement::Header,
            }
        );
    }
}
