//! Auth-kind dispatch: resolve an [`Auth`] config into its wire effect.
//!
//! THE single dispatch point for auth kinds (plugin-readiness guardrail):
//! every auth kind resolves to an [`AuthWire`] effect in
//! [`apply_auth`]'s one `match`. [`crate::http::execute`] applies effects and
//! never matches on [`Auth`] — a future plugin-provided auth kind slots into
//! this match, not into scattered call sites.
//!
//! No `{{var}}` resolution happens here; placeholder values are passed through
//! verbatim like everywhere else until the template resolver runs.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::model::{ApiKeyPlacement, Auth};

/// The wire effect of an auth config: exactly one header or one query pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthWire {
    /// Inject a request header (skipped when an enabled user header with the
    /// same name exists — the user's header always wins).
    Header {
        /// Header name, e.g. `Authorization`.
        name: String,
        /// Header value, e.g. `Bearer {{token}}`.
        value: String,
    },
    /// Append a query pair to the request URL (after enabled params).
    Query {
        /// Query-parameter name.
        name: String,
        /// Query-parameter value.
        value: String,
    },
}

/// Resolves `auth` to its [`AuthWire`] effect — the one `match` on auth kinds.
pub fn apply_auth(auth: &Auth) -> AuthWire {
    match auth {
        Auth::Basic { username, password } => AuthWire::Header {
            name: "Authorization".to_owned(),
            value: format!("Basic {}", BASE64.encode(format!("{username}:{password}"))),
        },
        Auth::Bearer { token } => AuthWire::Header {
            name: "Authorization".to_owned(),
            value: format!("Bearer {token}"),
        },
        Auth::ApiKey {
            name,
            value,
            placement,
        } => match placement {
            ApiKeyPlacement::Header => AuthWire::Header {
                name: name.clone(),
                value: value.clone(),
            },
            ApiKeyPlacement::Query => AuthWire::Query {
                name: name.clone(),
                value: value.clone(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_encodes_base64_authorization() {
        let wire = apply_auth(&Auth::Basic {
            username: "alice".into(),
            password: "s3cr3t".into(),
        });
        assert_eq!(
            wire,
            AuthWire::Header {
                name: "Authorization".into(),
                // base64("alice:s3cr3t")
                value: "Basic YWxpY2U6czNjcjN0".into(),
            }
        );
    }

    #[test]
    fn bearer_prefixes_token() {
        let wire = apply_auth(&Auth::Bearer {
            token: "{{token}}".into(),
        });
        assert_eq!(
            wire,
            AuthWire::Header {
                name: "Authorization".into(),
                value: "Bearer {{token}}".into(),
            }
        );
    }

    #[test]
    fn apikey_placement_selects_header_or_query() {
        let header = apply_auth(&Auth::ApiKey {
            name: "X-Api-Key".into(),
            value: "{{k}}".into(),
            placement: ApiKeyPlacement::Header,
        });
        assert_eq!(
            header,
            AuthWire::Header {
                name: "X-Api-Key".into(),
                value: "{{k}}".into(),
            }
        );

        let query = apply_auth(&Auth::ApiKey {
            name: "api_key".into(),
            value: "{{k}}".into(),
            placement: ApiKeyPlacement::Query,
        });
        assert_eq!(
            query,
            AuthWire::Query {
                name: "api_key".into(),
                value: "{{k}}".into(),
            }
        );
    }
}
