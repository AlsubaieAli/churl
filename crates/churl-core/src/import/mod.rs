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
//! Auth remap (M5): `-u user:pass` becomes first-class [`Auth::Basic`] and an
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
pub struct ImportResult {
    /// The imported endpoint (`seq` is 0; the name derives from the URL).
    pub endpoint: Endpoint,
    /// Human-readable warnings, one per accepted-but-ignored/remapped flag.
    pub warnings: Vec<String>,
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

/// Parses a curl command into an [`Endpoint`]. A leading `curl` token is
/// accepted and stripped; its absence is fine too.
pub fn import_curl(command: &str) -> Result<ImportResult, ImportError> {
    let tokens = shlex::split(command).ok_or(ImportError::Tokenize)?;
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
    url: Option<String>,
    warnings: Vec<String>,
}

type Args = std::iter::Peekable<std::vec::IntoIter<String>>;

impl Parser {
    fn set_url(&mut self, value: String) -> Result<(), ImportError> {
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
            Some(Body { kind, content })
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
                name: derive_name(&url),
                request: Request {
                    method,
                    url,
                    headers: self.headers,
                    params: Vec::new(), // query string stays in the URL
                    body,
                    auth: self.auth,
                },
            },
            warnings: self.warnings,
        })
    }
}

/// Derives a [`BodyKind`] for `-d` data: JSON when the trimmed content starts
/// with `{`/`[` or an explicit JSON `Content-Type` is present; form when it
/// looks like `k=v&k2=v2` and no non-form `Content-Type` says otherwise; text
/// otherwise.
fn derive_body_kind(content: &str, headers: &[Header]) -> BodyKind {
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

/// Derives an endpoint name from the URL: the last non-empty path segment,
/// else the host, sanitised to a filename-safe slug (`"endpoint"` as the last
/// resort).
fn derive_name(url: &str) -> String {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let without_query = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let mut segments = without_query.split('/');
    let host = segments.next().unwrap_or("");
    let slug = match segments.rev().find(|segment| !segment.is_empty()) {
        Some(last) => slugify(last),
        None => String::new(),
    };
    let slug = if slug.is_empty() { slugify(host) } else { slug };
    if slug.is_empty() {
        "endpoint".to_owned()
    } else {
        slug
    }
}

/// Lower-cases and keeps `[a-z0-9._-]`; every other run of characters becomes
/// a single `-`. Leading/trailing dashes are trimmed.
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_dash = false;
    for c in input.to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(command: &str) -> ImportResult {
        import_curl(command).unwrap_or_else(|err| panic!("import failed for {command:?}: {err}"))
    }

    #[test]
    fn plain_url_is_a_get() {
        let result = import("curl https://example.com/health");
        assert_eq!(result.endpoint.request.method, Method::Get);
        assert_eq!(result.endpoint.request.url, "https://example.com/health");
        assert!(result.endpoint.request.headers.is_empty());
        assert!(result.endpoint.request.body.is_none());
        assert!(result.warnings.is_empty());
        assert_eq!(result.endpoint.name, "health");
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
        assert_eq!(body.content, "a=1&b=2&c=3");
        assert_eq!(body.kind, BodyKind::Form);
    }

    #[test]
    fn json_content_derives_json_kind() {
        let body = import(r#"curl -d '{"a": 1}' https://e.com/f"#)
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(body.kind, BodyKind::Json);
        let array = import("curl -d '[1, 2]' https://e.com/f")
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(array.kind, BodyKind::Json);
    }

    #[test]
    fn json_content_type_header_forces_json_kind() {
        let body = import("curl -H 'Content-Type: application/json' -d 'a=1' https://e.com/f")
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(body.kind, BodyKind::Json);
    }

    #[test]
    fn non_form_content_type_prevents_form_kind() {
        let body = import("curl -H 'Content-Type: text/csv' -d 'a=1' https://e.com/f")
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(body.kind, BodyKind::Text);
    }

    #[test]
    fn free_text_body_is_text_kind() {
        let body = import("curl -d 'hello world' https://e.com/f")
            .endpoint
            .request
            .body
            .unwrap();
        assert_eq!(body.kind, BodyKind::Text);
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
        assert_eq!(request.body.as_ref().unwrap().kind, BodyKind::Json);
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
        // M4-era Basic base64 export and a lowercase "bearer" scheme (the
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
        // M4-era Authorization: Basic header.
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
    fn insecure_and_compressed_warn() {
        let result = import("curl -k --compressed https://e.com/x");
        assert!(result.warnings.iter().any(|w| w.contains("TLS")));
        assert!(result.warnings.iter().any(|w| w.contains("compression")));
    }

    #[test]
    fn insecure_inside_cluster_warns() {
        let result = import("curl -sk https://e.com/x");
        assert!(result.warnings.iter().any(|w| w.contains("TLS")));
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
        assert_eq!(derive_name("https://api.github.com/user/repos"), "repos");
        assert_eq!(derive_name("https://example.com/"), "example.com");
        assert_eq!(derive_name("https://example.com"), "example.com");
        assert_eq!(derive_name("https://e.com/a/b?x=1"), "b");
        assert_eq!(
            derive_name("https://e.com/Users And Groups/"),
            "users-and-groups"
        );
        assert_eq!(derive_name("https://e.com/{{id}}"), "id");
        assert_eq!(derive_name(""), "endpoint");
    }
}
