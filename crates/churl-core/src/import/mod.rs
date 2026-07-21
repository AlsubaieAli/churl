//! curl command import: parse a `curl …` invocation into an [`Endpoint`].
//!
//! Tokenisation is shell-accurate via `shlex`; the flag map is hand-rolled and
//! deliberately strict — any flag outside the supported set is a hard
//! [`ImportError::UnknownFlag`], never silently dropped (flag policy pinned in
//! DECISIONS.md). File payloads (`@file`) and multipart (`-F`) are recognised
//! and rejected as [`ImportError::Unsupported`]; churl never reads files during
//! an import. The query string stays in the URL — import does not explode it
//! into [`crate::model::Param`]s (lossless and simple).
//!
//! Auth remap: `-u user:pass` becomes first-class [`Auth::Basic`] and an
//! `Authorization: Bearer …` header becomes [`Auth::Bearer`]; literal secret
//! values are replaced with `{{password}}`/`{{token}}` placeholders (no secrets
//! in workspace files — stdout and `--out` both end up on disk). With multiple
//! auth sources, the first one in the command takes the first-class slot and
//! the rest stay plain headers, with a warning.

use crate::model::{Auth, Body, BodyKind, Endpoint, Header, Method, Request};

mod auth;
mod flags;

/// A successfully imported endpoint plus non-fatal warnings (flags accepted but
/// ignored or remapped, e.g. `--compressed`, `-k`, `-o`, `-u`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportResult {
    /// The imported endpoint (`seq` is 0; the name derives from the URL).
    pub endpoint: Endpoint,
    /// Human-readable warnings, one per accepted-but-ignored/remapped flag.
    pub warnings: Vec<String>,
    /// Secrets extracted while remapping auth to a `{{name}}` placeholder, as
    /// `(var_name, real_value)` — e.g. `("token", "v4.public…")` for a Bearer
    /// header, `("password", "s3cr3t")` for `-u user:pass`. A caller with a live
    /// session (the TUI) captures these into RAM-only Session vars so the
    /// placeholder resolves; the workspace files still hold only the placeholder.
    /// Empty when nothing real was extracted (e.g. the token was already a
    /// `{{…}}` placeholder, or `-u` had no password).
    pub captured_secrets: Vec<(String, String)>,
}

/// Error importing a curl command.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImportError {
    /// The command could not be tokenised (unbalanced quotes, trailing `\`).
    #[error("failed to tokenize curl command (unbalanced quotes?)")]
    Tokenize,
    /// No URL (positional or `--url`) was found.
    #[error("no URL found in curl command")]
    MissingUrl,
    /// More than one URL was given.
    #[error("multiple URLs in curl command: {0:?} and {1:?}")]
    MultipleUrls(String, String),
    /// A flag outside the supported set was encountered.
    #[error("unknown flag: {0}")]
    UnknownFlag(String),
    /// A value-taking flag appeared at the end of the arguments.
    #[error("flag {0} is missing its value")]
    MissingValue(String),
    /// A recognised but unsupported construct (`-F` multipart, `@file` bodies).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The `-X`/`--request` value is not a known HTTP method.
    #[error("invalid HTTP method: {0:?}")]
    InvalidMethod(String),
}

/// Parses a curl command STRING into an [`Endpoint`]. Strips bash
/// line-continuations, then shell-tokenises (`shlex`) and hands off to
/// [`import_curl_tokens`]. A leading `curl` token is accepted and stripped; its
/// absence is fine too. Use this for a single pasted/typed command; when the
/// shell has ALREADY tokenised the command (a var-arg CLI invocation), call
/// [`import_curl_tokens`] directly so the tokens are not re-split.
pub fn import_curl(command: &str) -> Result<ImportResult, ImportError> {
    let command = normalize_continuations(command);
    let tokens = shlex::split(&command).ok_or(ImportError::Tokenize)?;
    import_curl_tokens(tokens)
}

/// Parses PRE-TOKENISED curl args (already shell-split — e.g. the trailing
/// var-args of `churl import curl 'url' -H '…'`) into an [`Endpoint`]. Shares the
/// whole flag/URL walk with [`import_curl`]; the only difference is the input is
/// not re-tokenised (no `shlex`, no continuation stripping — the shell did that).
/// A leading `curl` token is accepted and stripped. `set_url` still glob-unescapes
/// `\[\]` etc., which the shell's single quotes leave intact.
pub fn import_curl_tokens<I>(tokens: I) -> Result<ImportResult, ImportError>
where
    I: IntoIterator<Item = String>,
{
    // Collect so `args` is the concrete `Args` type the flag handlers expect.
    let tokens: Vec<String> = tokens.into_iter().collect();
    let mut args = tokens.into_iter().peekable();
    if args.peek().map(String::as_str) == Some("curl") {
        args.next();
    }

    let mut parser = Parser::default();
    while let Some(token) = args.next() {
        if let Some(rest) = token.strip_prefix("--") {
            if rest.is_empty() {
                return Err(ImportError::UnknownFlag(token));
            }
            let (name, inline_value) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (rest, None),
            };
            parser.long_flag(name, inline_value, &mut args)?;
        } else if token.starts_with('-') && token.len() > 1 {
            parser.short_cluster(&token, &mut args)?;
        } else {
            parser.set_url(token)?;
        }
    }
    parser.finish()
}

/// Strips bash line-continuations from a multi-line curl command before
/// tokenization. Every browser's "Copy as cURL" wraps flags across lines with a
/// trailing `\`+newline; left in, `shlex` turns each ` \⏎` into a spurious empty
/// token (parsed as a second URL) or fails on a dangling `\` ("unbalanced
/// quotes"). A continuation joins the lines with nothing (bash), *except inside
/// single quotes* where `\`+newline is literal — so this scans quote state and
/// only drops a `\`+newline (LF or CRLF) when it is a real continuation. Every
/// other character (including escaped quotes `\'` / `\"` and `\\`) passes through
/// verbatim for `shlex` to tokenize, so a quoted body is never rewritten.
fn normalize_continuations(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            // Inside single quotes nothing is special but the closing quote; a
            // backslash-newline here is literal, never a continuation.
            if c == '\'' {
                in_single = false;
            }
            out.push(c);
            i += 1;
        } else if c == '\\' {
            if chars.get(i + 1) == Some(&'\n') {
                i += 2; // LF continuation — drop both.
            } else if chars.get(i + 1) == Some(&'\r') && chars.get(i + 2) == Some(&'\n') {
                i += 3; // CRLF continuation — drop all three.
            } else if let Some(&next) = chars.get(i + 1) {
                // An escape (`\'`, `\"`, `\\`, …): pass the pair through as a unit
                // so the escaped char can't be mistaken for a quote toggle.
                out.push('\\');
                out.push(next);
                i += 2;
            } else {
                out.push('\\'); // trailing lone backslash — let shlex judge it.
                i += 1;
            }
        } else if c == '\'' && !in_double {
            in_single = true;
            out.push(c);
            i += 1;
        } else {
            if c == '"' {
                in_double = !in_double;
            }
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Undoes curl's glob-escaping of `[ ] { }` in a URL. curl treats these as URL
/// globbing metacharacters, so a browser escapes them (`fields\[\]=` for the very
/// common `fields[]=` array-param form). churl does no globbing, so the literal
/// bracket/brace is what the server must receive.
fn unescape_curl_url_globs(url: &str) -> String {
    url.replace("\\[", "[")
        .replace("\\]", "]")
        .replace("\\{", "{")
        .replace("\\}", "}")
}

/// Accumulated request state while walking the argument list.
#[derive(Debug, Default)]
struct Parser {
    method: Option<Method>,
    headers: Vec<Header>,
    /// `-d`/`--data*`/`--json` values in order; joined with `&` (curl semantics).
    data_parts: Vec<String>,
    /// Set by `--json`: forces [`BodyKind::Json`] and an `Accept` header.
    json: bool,
    /// First-class auth (`-u` or a `Authorization: Bearer …` header); the slot
    /// goes to whichever auth source appears first in the command.
    auth: Option<Auth>,
    /// Set by `-k`/`--insecure`: bakes durable insecure-TLS onto the imported
    /// endpoint (a security-relevant property of the request, unlike the
    /// session-scoped proxy).
    insecure: bool,
    url: Option<String>,
    warnings: Vec<String>,
    /// Real secret values extracted while placeholdering auth, as
    /// `(var_name, real_value)`. Populated by [`Parser::add_header`] /
    /// [`Parser::add_basic_auth`]; surfaced verbatim in [`ImportResult`].
    captured_secrets: Vec<(String, String)>,
}

type Args = std::iter::Peekable<std::vec::IntoIter<String>>;

impl Parser {
    fn set_url(&mut self, value: String) -> Result<(), ImportError> {
        let value = unescape_curl_url_globs(&value);
        match &self.url {
            Some(existing) => Err(ImportError::MultipleUrls(existing.clone(), value)),
            None => {
                self.url = Some(value);
                Ok(())
            }
        }
    }

    /// Assembles the final [`Endpoint`]: joins data parts, derives the body
    /// kind and method, and names the endpoint from the URL.
    fn finish(mut self) -> Result<ImportResult, ImportError> {
        let url = self.url.ok_or(ImportError::MissingUrl)?;
        let body = if self.data_parts.is_empty() {
            None
        } else {
            let content = self.data_parts.join("&");
            let kind = if self.json {
                BodyKind::Json
            } else {
                derive_body_kind(&content, &self.headers)
            };
            Some(Body::Simple { kind, content })
        };
        if self.json
            && !self
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("accept"))
        {
            self.headers.push(Header {
                name: "Accept".to_owned(),
                value: "application/json".to_owned(),
                enabled: true,
            });
        }
        // Explicit -X wins; else a body implies POST (curl semantics); else GET.
        let method = self.method.unwrap_or(if body.is_some() {
            Method::Post
        } else {
            Method::Get
        });
        Ok(ImportResult {
            endpoint: Endpoint {
                seq: 0,
                name: derive_name(method, &url, "curl"),
                assertions: Vec::new(),
                extract: std::collections::BTreeMap::new(),
                persist: Vec::new(),
                request: Request {
                    method,
                    url,
                    headers: self.headers,
                    params: Vec::new(), // query string stays in the URL
                    body,
                    auth: self.auth,
                    insecure: self.insecure,
                },
            },
            warnings: self.warnings,
            captured_secrets: self.captured_secrets,
        })
    }
}

/// Derives a [`BodyKind`] for `-d` data: JSON when the trimmed content starts
/// with `{`/`[` or an explicit JSON `Content-Type` is present; form when it
/// looks like `k=v&k2=v2` and no non-form `Content-Type` says otherwise; text
/// otherwise.
///
/// `pub` (M8.2): the headless `churl send` ad-hoc path reuses this exact
/// heuristic for its `-d`/`--body` flag, so curl-mnemonic and churl-native
/// sends derive the same default `Content-Type` as an imported endpoint would.
pub fn derive_body_kind(content: &str, headers: &[Header]) -> BodyKind {
    let content_type = headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.to_ascii_lowercase());
    if content_type
        .as_deref()
        .is_some_and(|value| value.contains("json"))
    {
        return BodyKind::Json;
    }
    let trimmed = content.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return BodyKind::Json;
    }
    let form_like = !content.is_empty()
        && content
            .split('&')
            .all(|pair| pair.split_once('=').is_some_and(|(key, _)| !key.is_empty()));
    let content_type_allows_form = match content_type.as_deref() {
        None => true,
        Some(value) => value.contains("application/x-www-form-urlencoded"),
    };
    if form_like && content_type_allows_form {
        return BodyKind::Form;
    }
    BodyKind::Text
}

/// Derives an endpoint name from the request `method` + URL using the U6 naming
/// standard: `<METHOD> <last ≤3 non-empty path segments> [suffix]`, space-joined.
/// The space separator is mandatory — churl addresses endpoints as
/// `collection/name`, so a `/` in the name would collide with path addressing.
/// Segments are split on `/` and re-joined with spaces, so a derived name can
/// never contain one. With no path segments the host stands in; with neither host
/// nor path, just `<METHOD>` (+ suffix). The method is upper-cased via [`Method`]'s
/// `Display`.
///
/// `suffix` is a provenance marker appended when non-empty: the curl importer
/// ([`Parser::finish`]) passes `"curl"` (owner decision — marks curl-imported);
/// the foreign interchange importer ([`crate::interchange`]) passes `""` (a
/// Postman/native import is not curl-imported, so no false marker). Sharing the
/// naming core keeps the two importers in lockstep without a second copy.
pub(crate) fn derive_name(method: Method, url: &str, suffix: &str) -> String {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let without_query = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let mut segments = without_query.split('/');
    let host = segments.next().unwrap_or("");
    // Path segments after the host, in order, dropping empties (a leading/
    // trailing or doubled `/` yields no segment).
    let path_segments: Vec<&str> = segments.filter(|segment| !segment.is_empty()).collect();
    let mut name = method.to_string();
    if path_segments.is_empty() {
        // No path — fall back to the host so the name still says *where*; a
        // host-less URL leaves just `<METHOD> curl`.
        if !host.is_empty() {
            name.push(' ');
            name.push_str(host);
        }
    } else {
        // The last ≤3 segments, kept verbatim and in path order.
        let start = path_segments.len().saturating_sub(3);
        for segment in &path_segments[start..] {
            name.push(' ');
            name.push_str(segment);
        }
    }
    if !suffix.is_empty() {
        name.push(' ');
        name.push_str(suffix);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(command: &str) -> ImportResult {
        import_curl(command).unwrap_or_else(|err| panic!("import failed for {command:?}: {err}"))
    }

    /// `send`/curl-import bodies are always `Simple` — a `Multipart` body here
    /// is a test-authoring bug, not a case these helpers need to handle.
    fn simple_kind(body: &Body) -> BodyKind {
        match body {
            Body::Simple { kind, .. } => *kind,
            Body::Multipart(_) => panic!("expected a Simple body, got Multipart"),
        }
    }

    fn simple_content(body: &Body) -> &str {
        match body {
            Body::Simple { content, .. } => content,
            Body::Multipart(_) => panic!("expected a Simple body, got Multipart"),
        }
    }

    #[test]
    fn plain_url_is_a_get() {
        let result = import("curl https://example.com/health");
        assert_eq!(result.endpoint.request.method, Method::Get);
        assert_eq!(result.endpoint.request.url, "https://example.com/health");
        assert!(result.endpoint.request.headers.is_empty());
        assert!(result.endpoint.request.body.is_none());
        assert!(result.warnings.is_empty());
        assert_eq!(result.endpoint.name, "GET health curl");
    }

    #[test]
    fn multiline_browser_copy_as_curl_imports() {
        // Chrome/Firefox "Copy as cURL": trailing `\` line-continuations plus
        // glob-escaped `[]` in an array query param. Regression for the real
        // paste that failed with "multiple URLs" (empty continuation token).
        let cmd = "curl 'https://api.example.com/v2/orders/42?format=light&fields\\[\\]=is_blocked&fields\\[\\]=branches' \\\n  -H 'accept: application/json' \\\n  -H 'accept-language: ar' \\\n  -H 'origin: https://s.example.sa'";
        let result = import(cmd);
        assert_eq!(result.endpoint.request.method, Method::Get);
        assert_eq!(
            result.endpoint.request.url,
            "https://api.example.com/v2/orders/42?format=light&fields[]=is_blocked&fields[]=branches"
        );
        assert_eq!(result.endpoint.request.headers.len(), 3);
    }

    #[test]
    fn line_continuation_does_not_produce_an_empty_url() {
        // The ` \⏎` between the URL and the first flag must not become a second
        // (empty) URL token.
        let result = import("curl 'https://x.dev/o' \\\n  -H 'accept: application/json'");
        assert_eq!(result.endpoint.request.url, "https://x.dev/o");
        assert_eq!(result.endpoint.request.headers.len(), 1);
    }

    #[test]
    fn trailing_backslash_is_tolerated() {
        // A partial paste ending mid-continuation must not fail tokenization.
        let result = import("curl 'https://x.dev/o' \\\n");
        assert_eq!(result.endpoint.request.url, "https://x.dev/o");
    }

    #[test]
    fn curl_glob_escapes_in_url_are_unescaped() {
        let result = import("curl 'https://x.dev/o?a\\[\\]=1&b\\{2\\}=3'");
        assert_eq!(result.endpoint.request.url, "https://x.dev/o?a[]=1&b{2}=3");
    }

    #[test]
    fn single_quoted_body_keeps_backslash_newline_literal() {
        // Inside single quotes, `\`+newline is literal (bash), NOT a line
        // continuation — the body must survive byte-for-byte, not be collapsed.
        let result = import("curl https://e.com/n --data-raw 'text=a\\\nb'");
        assert_eq!(
            simple_content(&result.endpoint.request.body.unwrap()),
            "text=a\\\nb"
        );
    }

    #[test]
    fn continuation_outside_single_quotes_joins_with_nothing() {
        // A continuation joins with nothing (bash), so a double-quoted value split
        // across a continuation rejoins seamlessly — no stray space injected.
        let result = import("curl https://e.com/n --data-raw \"a=1\\\nb=2\"");
        assert_eq!(
            simple_content(&result.endpoint.request.body.unwrap()),
            "a=1b=2"
        );
    }

    #[test]
    fn leading_curl_token_is_optional() {
        let with = import("curl https://example.com/a");
        let without = import("https://example.com/a");
        assert_eq!(with.endpoint, without.endpoint);
    }

    #[test]
    fn explicit_method_wins() {
        let result = import("curl -X DELETE -d payload=1 https://example.com/x");
        assert_eq!(result.endpoint.request.method, Method::Delete);
    }

    #[test]
    fn method_long_flag_and_equals_form() {
        assert_eq!(
            import("curl --request PUT https://e.com/x")
                .endpoint
                .request
                .method,
            Method::Put
        );
        assert_eq!(
            import("curl --request=PATCH https://e.com/x")
                .endpoint
                .request
                .method,
            Method::Patch
        );
    }

    #[test]
    fn attached_short_value_parses() {
        let result = import("curl -XPOST https://example.com/x");
        assert_eq!(result.endpoint.request.method, Method::Post);
    }

    #[test]
    fn invalid_method_errors() {
        assert!(matches!(
            import_curl("curl -X BREW https://e.com/"),
            Err(ImportError::InvalidMethod(m)) if m == "BREW"
        ));
    }

    #[test]
    fn header_splits_on_first_colon_and_trims() {
        let result = import("curl -H 'X-Forwarded-For: 10.0.0.1:8080' https://e.com/");
        let header = &result.endpoint.request.headers[0];
        assert_eq!(header.name, "X-Forwarded-For");
        assert_eq!(header.value, "10.0.0.1:8080");
        assert!(header.enabled);
    }

    #[test]
    fn body_implies_post() {
        let result = import("curl -d 'a=1' https://e.com/f");
        assert_eq!(result.endpoint.request.method, Method::Post);
    }

    #[test]
    fn multiple_data_parts_join_with_ampersand() {
        let result = import("curl -d a=1 --data b=2 --data-raw c=3 https://e.com/f");
        let body = result.endpoint.request.body.unwrap();
        assert_eq!(simple_content(&body), "a=1&b=2&c=3");
        assert_eq!(simple_kind(&body), BodyKind::Form);
    }

    #[test]
    fn json_content_derives_json_kind() {
        let body = import(r#"curl -d '{"a": 1}' https://e.com/f"#)
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(simple_kind(&body), BodyKind::Json);
        let array = import("curl -d '[1, 2]' https://e.com/f")
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(simple_kind(&array), BodyKind::Json);
    }

    #[test]
    fn json_content_type_header_forces_json_kind() {
        let body = import("curl -H 'Content-Type: application/json' -d 'a=1' https://e.com/f")
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(simple_kind(&body), BodyKind::Json);
    }

    #[test]
    fn non_form_content_type_prevents_form_kind() {
        let body = import("curl -H 'Content-Type: text/csv' -d 'a=1' https://e.com/f")
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(simple_kind(&body), BodyKind::Text);
    }

    #[test]
    fn free_text_body_is_text_kind() {
        let body = import("curl -d 'hello world' https://e.com/f")
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(simple_kind(&body), BodyKind::Text);
    }

    #[test]
    fn at_file_body_is_unsupported() {
        for command in [
            "curl -d @payload.json https://e.com/f",
            "curl --data-binary @dump.bin https://e.com/f",
            "curl --json @body.json https://e.com/f",
        ] {
            assert!(
                matches!(import_curl(command), Err(ImportError::Unsupported(s)) if s == "@file body"),
                "expected @file rejection for {command:?}"
            );
        }
    }

    #[test]
    fn json_flag_sets_kind_method_and_accept() {
        let result = import(r#"curl --json '{"q": true}' https://e.com/search"#);
        let request = &result.endpoint.request;
        assert_eq!(request.method, Method::Post);
        assert_eq!(simple_kind(request.body.as_ref().unwrap()), BodyKind::Json);
        assert!(
            request
                .headers
                .iter()
                .any(|h| h.name == "Accept" && h.value == "application/json")
        );
        // No explicit Content-Type header — BodyKind derives it at execute time.
        assert!(
            !request
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("content-type"))
        );
    }

    #[test]
    fn json_flag_does_not_duplicate_existing_accept() {
        let result = import(r#"curl --json '{}' -H 'Accept: text/plain' https://e.com/x"#);
        let accepts: Vec<_> = result
            .endpoint
            .request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("accept"))
            .collect();
        assert_eq!(accepts.len(), 1);
        assert_eq!(accepts[0].value, "text/plain");
    }

    #[test]
    fn multipart_is_unsupported() {
        for command in [
            "curl -F 'file=@photo.png' https://e.com/upload",
            "curl --form 'name=x' https://e.com/upload",
        ] {
            assert!(
                matches!(import_curl(command), Err(ImportError::Unsupported(s)) if s.contains("multipart")),
                "expected multipart rejection for {command:?}"
            );
        }
    }

    #[test]
    fn user_remaps_to_basic_auth_with_placeholder_password() {
        let result = import("curl -u alice:s3cr3t https://e.com/private");
        assert_eq!(
            result.endpoint.request.auth,
            Some(Auth::Basic {
                username: "alice".to_owned(),
                password: "{{password}}".to_owned(),
            })
        );
        assert!(result.endpoint.request.headers.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("{{password}}") && w.contains("no secrets")),
            "warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn user_with_placeholder_password_is_kept_verbatim() {
        let result = import("curl -u 'alice:{{admin_pass}}' https://e.com/private");
        assert_eq!(
            result.endpoint.request.auth,
            Some(Auth::Basic {
                username: "alice".to_owned(),
                password: "{{admin_pass}}".to_owned(),
            })
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }

    #[test]
    fn user_without_colon_gets_placeholder_password() {
        let result = import("curl -u alice https://e.com/private");
        assert_eq!(
            result.endpoint.request.auth,
            Some(Auth::Basic {
                username: "alice".to_owned(),
                password: "{{password}}".to_owned(),
            })
        );
        assert!(
            result.warnings.iter().any(|w| w.contains("prompt")),
            "warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn bearer_header_remaps_to_bearer_auth_with_placeholder() {
        let result = import("curl -H 'Authorization: Bearer ghp_16C7e42F' https://e.com/me");
        assert_eq!(
            result.endpoint.request.auth,
            Some(Auth::Bearer {
                token: "{{token}}".to_owned(),
            })
        );
        assert!(result.endpoint.request.headers.is_empty());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("{{token}}") && w.contains("no secrets")),
            "warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn captured_secrets_hold_the_real_bearer_and_password() {
        // Bearer header → capture ("token", real).
        let result = import("curl -H 'Authorization: Bearer ghp_16C7e42F' https://e.com/me");
        assert_eq!(
            result.captured_secrets,
            vec![("token".to_owned(), "ghp_16C7e42F".to_owned())],
            "real Bearer token captured for the session"
        );
        assert_eq!(
            result.endpoint.request.auth,
            Some(Auth::Bearer {
                token: "{{token}}".to_owned()
            }),
            "the endpoint itself keeps only the placeholder"
        );

        // -u user:pass → capture ("password", real).
        let result = import("curl -u alice:s3cr3t https://e.com/private");
        assert_eq!(
            result.captured_secrets,
            vec![("password".to_owned(), "s3cr3t".to_owned())]
        );
    }

    #[test]
    fn already_placeholder_secrets_are_not_captured() {
        // A token/password that was ALREADY a `{{…}}` placeholder has nothing real
        // to store — captured_secrets stays empty.
        let bearer = import("curl -H 'authorization: Bearer {{gh_token}}' https://e.com/me");
        assert!(
            bearer.captured_secrets.is_empty(),
            "placeholder token not captured: {:?}",
            bearer.captured_secrets
        );
        let basic = import("curl -u 'alice:{{admin_pass}}' https://e.com/private");
        assert!(
            basic.captured_secrets.is_empty(),
            "placeholder password not captured: {:?}",
            basic.captured_secrets
        );
        // `-u` with no password: placeholdered but nothing real → not captured.
        let no_pass = import("curl -u alice https://e.com/private");
        assert!(
            no_pass.captured_secrets.is_empty(),
            "absent password not captured: {:?}",
            no_pass.captured_secrets
        );
    }

    #[test]
    fn import_curl_tokens_matches_import_curl_on_the_joined_string() {
        // A pre-tokenised arg vector (as the shell hands the var-arg CLI) parses
        // identically to the same command as one shlex-split string.
        let tokens = vec![
            "curl".to_owned(),
            "https://e.com/o?a\\[\\]=1".to_owned(), // single-quoted at the shell → backslashes intact
            "-H".to_owned(),
            "Authorization: Bearer ghp_ABC".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            "-d".to_owned(),
            "x=1".to_owned(),
        ];
        let from_tokens = import_curl_tokens(tokens.clone()).unwrap();
        // The equivalent single string (each token single-quoted so shlex yields
        // exactly these tokens back).
        let joined =
            "curl 'https://e.com/o?a\\[\\]=1' -H 'Authorization: Bearer ghp_ABC' -X POST -d 'x=1'";
        let from_string = import_curl(joined).unwrap();
        assert_eq!(from_tokens, from_string);
        // Sanity: the shared URL glob-unescape still ran on the token path.
        assert_eq!(from_tokens.endpoint.request.url, "https://e.com/o?a[]=1");
        assert_eq!(
            from_tokens.captured_secrets,
            vec![("token".to_owned(), "ghp_ABC".to_owned())]
        );
    }

    #[test]
    fn bearer_header_with_placeholder_token_is_kept_verbatim() {
        // Header name matching is case-insensitive; the token is already a
        // placeholder so no warning fires.
        let result = import("curl -H 'authorization: Bearer {{gh_token}}' https://e.com/me");
        assert_eq!(
            result.endpoint.request.auth,
            Some(Auth::Bearer {
                token: "{{gh_token}}".to_owned(),
            })
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }

    #[test]
    fn other_authorization_headers_stay_plain() {
        // Basic base64 export and a lowercase "bearer" scheme (the
        // `Bearer ` prefix is matched exactly) both stay plain headers.
        for command in [
            "curl -H 'Authorization: Basic YWxpY2U6czNjcjN0' https://e.com/x",
            "curl -H 'Authorization: bearer abc' https://e.com/x",
            "curl -H 'Authorization: Digest xyz' https://e.com/x",
        ] {
            let result = import(command);
            assert_eq!(result.endpoint.request.auth, None, "{command}");
            assert_eq!(result.endpoint.request.headers.len(), 1, "{command}");
            assert_eq!(result.endpoint.request.headers[0].name, "Authorization");
            assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        }
    }

    #[test]
    fn multiple_auth_sources_keep_the_first_as_first_class() {
        // -u first: basic wins; the Bearer header stays plain.
        let result =
            import("curl -u alice:s3cr3t -H 'Authorization: Bearer {{t}}' https://e.com/x");
        assert!(matches!(
            result.endpoint.request.auth,
            Some(Auth::Basic { .. })
        ));
        assert_eq!(
            result.endpoint.request.headers[0].value, "Bearer {{t}}",
            "losing bearer stays a plain header"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("multiple auth sources") && w.contains("kept basic")),
            "warnings: {:?}",
            result.warnings
        );

        // Bearer header first: bearer wins; -u falls back to the plain
        // Authorization: Basic header.
        let result =
            import("curl -H 'Authorization: Bearer {{t}}' -u alice:s3cr3t https://e.com/x");
        assert_eq!(
            result.endpoint.request.auth,
            Some(Auth::Bearer {
                token: "{{t}}".to_owned(),
            })
        );
        assert_eq!(
            result.endpoint.request.headers[0].value,
            "Basic YWxpY2U6czNjcjN0"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("multiple auth sources") && w.contains("kept bearer")),
            "warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn silent_flags_and_cluster_are_accepted() {
        let result = import("curl -sSL -v -S --location --silent https://e.com/x");
        assert!(result.warnings.is_empty());
        assert_eq!(result.endpoint.request.method, Method::Get);
    }

    #[test]
    fn insecure_bakes_endpoint_flag_and_warns() {
        let result = import("curl -k --compressed https://e.com/x");
        // `-k` now durably bakes insecure-TLS onto the endpoint (not session-scoped)
        // and warns loudly that verification is off.
        assert!(
            result.endpoint.request.insecure,
            "-k must set the endpoint's insecure flag"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("-k") && w.to_lowercase().contains("verification")),
            "warnings: {:?}",
            result.warnings
        );
        assert!(result.warnings.iter().any(|w| w.contains("compression")));
    }

    #[test]
    fn insecure_inside_cluster_bakes_flag() {
        let result = import("curl -sk https://e.com/x");
        assert!(
            result.endpoint.request.insecure,
            "-k inside a cluster must set the endpoint's insecure flag"
        );
    }

    #[test]
    fn insecure_long_flag_bakes_flag() {
        let result = import("curl --insecure https://e.com/x");
        assert!(result.endpoint.request.insecure);
    }

    #[test]
    fn no_insecure_flag_leaves_endpoint_secure() {
        // The default must stay secure: an import without -k never opts in.
        let result = import("curl https://e.com/x");
        assert!(!result.endpoint.request.insecure);
    }

    #[test]
    fn proxy_short_and_long_flag_noted_not_persisted() {
        for command in [
            "curl -x http://proxy.local:3128 https://e.com/x",
            "curl --proxy http://proxy.local:3128 https://e.com/x",
            "curl -xhttp://proxy.local:3128 https://e.com/x",
        ] {
            let result = import(command);
            // The proxy is surfaced as a note...
            assert!(
                result
                    .warnings
                    .iter()
                    .any(|w| w.contains("proxy") && w.contains("Options overlay")),
                "warnings for {command:?}: {:?}",
                result.warnings
            );
            // ...and never baked into the endpoint (no per-endpoint proxy field).
            assert_eq!(result.endpoint.request.url, "https://e.com/x");
            assert!(result.endpoint.request.headers.is_empty());
        }
    }

    #[test]
    fn proxy_missing_value_errors() {
        assert!(matches!(
            import_curl("curl https://e.com/ -x"),
            Err(ImportError::MissingValue(f)) if f == "-x"
        ));
        assert!(matches!(
            import_curl("curl https://e.com/ --proxy"),
            Err(ImportError::MissingValue(f)) if f == "--proxy"
        ));
    }

    #[test]
    fn output_value_is_consumed_with_warning() {
        let result = import("curl -o out.json https://e.com/export");
        assert_eq!(result.endpoint.request.url, "https://e.com/export");
        assert!(result.warnings.iter().any(|w| w.contains("out.json")));
        let long = import("curl --output out.json https://e.com/export");
        assert_eq!(long.endpoint.request.url, "https://e.com/export");
    }

    #[test]
    fn unknown_flags_error() {
        assert!(matches!(
            import_curl("curl --explode https://e.com/"),
            Err(ImportError::UnknownFlag(f)) if f == "--explode"
        ));
        assert!(matches!(
            import_curl("curl -Z https://e.com/"),
            Err(ImportError::UnknownFlag(f)) if f == "-Z"
        ));
        // Bare `-` (curl's stdin marker) is not supported either.
        assert!(matches!(
            import_curl("curl -- https://e.com/"),
            Err(ImportError::UnknownFlag(_))
        ));
    }

    #[test]
    fn missing_value_at_end_errors() {
        assert!(matches!(
            import_curl("curl https://e.com/ -H"),
            Err(ImportError::MissingValue(f)) if f == "-H"
        ));
        assert!(matches!(
            import_curl("curl https://e.com/ --data"),
            Err(ImportError::MissingValue(f)) if f == "--data"
        ));
    }

    #[test]
    fn missing_url_errors() {
        assert!(matches!(
            import_curl("curl -X POST"),
            Err(ImportError::MissingUrl)
        ));
        assert!(matches!(import_curl("curl"), Err(ImportError::MissingUrl)));
    }

    #[test]
    fn second_url_errors() {
        assert!(matches!(
            import_curl("curl https://a.com/ https://b.com/"),
            Err(ImportError::MultipleUrls(a, b)) if a == "https://a.com/" && b == "https://b.com/"
        ));
        assert!(matches!(
            import_curl("curl --url https://a.com/ https://b.com/"),
            Err(ImportError::MultipleUrls(_, _))
        ));
    }

    #[test]
    fn unbalanced_quotes_fail_tokenization() {
        assert!(matches!(
            import_curl("curl 'https://e.com"),
            Err(ImportError::Tokenize)
        ));
    }

    #[test]
    fn query_string_stays_in_url() {
        let result = import("curl 'https://e.com/search?q=rust+tui&page=2'");
        assert_eq!(
            result.endpoint.request.url,
            "https://e.com/search?q=rust+tui&page=2"
        );
        assert!(result.endpoint.request.params.is_empty());
    }

    #[test]
    fn name_derivation() {
        // Curl-import naming standard (U6): `<METHOD> <last ≤3 path segs> curl`,
        // space-joined, method upper-cased, segments verbatim, trailing `curl`.

        // ≥3 path segments → keep the LAST three, in path order.
        assert_eq!(
            derive_name(Method::Get, "https://api.example.com/v1/users/42", "curl"),
            "GET v1 users 42 curl"
        );
        assert_eq!(
            derive_name(Method::Post, "https://e.com/a/b/c/d/e", "curl"),
            "POST c d e curl"
        );

        // <3 path segments → use however many exist.
        assert_eq!(
            derive_name(Method::Get, "https://api.github.com/user/repos", "curl"),
            "GET user repos curl"
        );
        assert_eq!(
            derive_name(Method::Delete, "https://e.com/a/b?x=1", "curl"),
            "DELETE a b curl"
        );

        // Exactly one path segment.
        assert_eq!(
            derive_name(Method::Get, "https://e.com/{{id}}", "curl"),
            "GET {{id}} curl"
        );
        // Spaces in a single segment survive verbatim (no slugging, no `/`).
        assert_eq!(
            derive_name(Method::Get, "https://e.com/Users And Groups/", "curl"),
            "GET Users And Groups curl"
        );

        // 0 path segments → host stands in.
        assert_eq!(
            derive_name(Method::Get, "https://example.com/", "curl"),
            "GET example.com curl"
        );
        assert_eq!(
            derive_name(Method::Get, "https://example.com", "curl"),
            "GET example.com curl"
        );

        // No host and no path → just `<METHOD> curl` (never empty / panics).
        assert_eq!(derive_name(Method::Get, "", "curl"), "GET curl");

        // An empty suffix (the interchange importer) omits the provenance token.
        assert_eq!(
            derive_name(Method::Get, "https://api.example.com/v1/users/42", ""),
            "GET v1 users 42"
        );

        // The separator is ALWAYS a space — a derived name can never contain `/`
        // (churl addresses endpoints as `collection/name`).
        assert!(!derive_name(Method::Put, "https://e.com/x/y/z", "curl").contains('/'));
    }
}
