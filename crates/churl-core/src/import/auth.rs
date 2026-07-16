use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::config::is_template_placeholder;
use crate::model::{Auth, Header};

use super::Parser;

/// Short label for an auth kind, used in multiple-auth-source warnings.
fn auth_kind_label(auth: &Auth) -> &'static str {
    match auth {
        Auth::Basic { .. } => "basic",
        Auth::Bearer { .. } => "bearer",
        Auth::ApiKey { .. } => "apikey",
    }
}

impl Parser {
    /// Splits a `-H` value on the first `:`; the value side is trimmed. A
    /// colon-less header lands with an empty value.
    ///
    /// An `Authorization: Bearer …` header (name case-insensitive; `Bearer `
    /// prefix matched exactly) is remapped to first-class [`Auth::Bearer`],
    /// with a literal token replaced by a `{{token}}` placeholder. Any other
    /// `Authorization:` header (including `Basic <base64>`) stays a
    /// plain header.
    pub(super) fn add_header(&mut self, value: &str) {
        let (name, val) = match value.split_once(':') {
            Some((name, val)) => (name.trim(), val.trim()),
            None => (value.trim(), ""),
        };
        if name.eq_ignore_ascii_case("authorization")
            && let Some(token) = val.strip_prefix("Bearer ")
        {
            if let Some(kept) = &self.auth {
                self.warnings.push(format!(
                    "multiple auth sources; kept {} as first-class auth",
                    auth_kind_label(kept)
                ));
            } else {
                let token = if is_template_placeholder(token) {
                    token.to_owned()
                } else {
                    self.warnings.push(
                        "Bearer token replaced with {{token}} placeholder — no secrets in \
                         workspace files; supply the real value via a profile/env (M6)"
                            .to_owned(),
                    );
                    // Capture the real token so a live session can bind `{{token}}`
                    // in RAM (never written to a workspace file).
                    self.captured_secrets
                        .push(("token".to_owned(), token.to_owned()));
                    "{{token}}".to_owned()
                };
                self.auth = Some(Auth::Bearer { token });
                return;
            }
        }
        self.headers.push(Header {
            name: name.to_owned(),
            value: val.to_owned(),
            enabled: true,
        });
    }

    /// `-u user:pass` → first-class [`Auth::Basic`]. A literal password is
    /// replaced with a `{{password}}` placeholder — no secrets in workspace
    /// files; a password that is already a `{{...}}` placeholder is kept
    /// verbatim. Without a colon the whole value is the username (curl would
    /// prompt for the password). When another auth source already claimed the
    /// first-class slot, `-u` falls back to the plain
    /// `Authorization: Basic <base64>` header.
    pub(super) fn add_basic_auth(&mut self, value: &str) {
        if let Some(kept) = &self.auth {
            let label = auth_kind_label(kept);
            self.headers.push(Header {
                name: "Authorization".to_owned(),
                value: format!("Basic {}", BASE64.encode(value)),
                enabled: true,
            });
            self.warnings.push(format!(
                "multiple auth sources; kept {label} as first-class auth"
            ));
            return;
        }
        let (username, password) = match value.split_once(':') {
            Some((user, pass)) if is_template_placeholder(pass) => {
                (user.to_owned(), pass.to_owned())
            }
            Some((user, pass)) => {
                self.warnings.push(
                    "-u password replaced with {{password}} placeholder — no secrets in \
                     workspace files; supply the real value via a profile/env (M6)"
                        .to_owned(),
                );
                // Capture the real password so a live session can bind
                // `{{password}}` in RAM (never written to a workspace file).
                self.captured_secrets
                    .push(("password".to_owned(), pass.to_owned()));
                (user.to_owned(), "{{password}}".to_owned())
            }
            None => {
                self.warnings.push(
                    "-u had no password (curl would prompt); {{password}} placeholder added — \
                     supply the real value via a profile/env (M6)"
                        .to_owned(),
                );
                (value.to_owned(), "{{password}}".to_owned())
            }
        };
        self.auth = Some(Auth::Basic { username, password });
    }
}
