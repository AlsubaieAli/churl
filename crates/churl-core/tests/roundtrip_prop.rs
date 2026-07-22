//! Property test: any `Endpoint` survives a save → load round-trip unchanged.

use churl_core::model::{
    ApiKeyPlacement, Auth, Body, BodyKind, Endpoint, Header, Method, Param, Part, PartValue,
    Request,
};
use churl_core::persistence::{load_endpoint, save_endpoint};
use proptest::prelude::*;

/// Printable ASCII, non-empty, no control characters.
fn text() -> impl Strategy<Value = String> {
    "[ -~]{1,32}"
}

fn method() -> impl Strategy<Value = Method> {
    prop::sample::select(vec![
        Method::Get,
        Method::Post,
        Method::Put,
        Method::Patch,
        Method::Delete,
        Method::Head,
        Method::Options,
    ])
}

fn body_kind() -> impl Strategy<Value = BodyKind> {
    prop::sample::select(vec![BodyKind::Text, BodyKind::Json, BodyKind::Form])
}

fn header() -> impl Strategy<Value = Header> {
    (text(), text(), any::<bool>()).prop_map(|(name, value, enabled)| Header {
        name,
        value,
        enabled,
    })
}

fn param() -> impl Strategy<Value = Param> {
    (text(), text(), any::<bool>()).prop_map(|(name, value, enabled)| Param {
        name,
        value,
        enabled,
    })
}

fn simple_body() -> impl Strategy<Value = Body> {
    (body_kind(), text()).prop_map(|(kind, content)| Body::Simple { kind, content })
}

/// M8.6: an inline-text or file-reference part value. File `path`/`filename`
/// reuse the same printable-ASCII generator as everything else here — the
/// save/load round-trip never validates or resolves a path (that's a
/// send-time-only concern, see `http::resolve_part_path`), so any string is a
/// valid persisted value.
fn part_value() -> impl Strategy<Value = PartValue> {
    prop_oneof![
        text().prop_map(PartValue::Text),
        (text(), prop::option::of(text()), prop::option::of(text())).prop_map(
            |(path, filename, mime)| PartValue::File {
                path,
                filename,
                mime,
            }
        ),
    ]
}

fn part() -> impl Strategy<Value = Part> {
    (text(), part_value()).prop_map(|(name, value)| Part { name, value })
}

fn multipart_body() -> impl Strategy<Value = Body> {
    prop::collection::vec(part(), 0..=4).prop_map(Body::Multipart)
}

fn body() -> impl Strategy<Value = Body> {
    prop_oneof![simple_body(), multipart_body()]
}

/// A `{{var}}` template placeholder — secret-valued auth fields must hold one,
/// or `save_endpoint` (correctly) refuses to write the file.
fn placeholder() -> impl Strategy<Value = String> {
    "[a-z_]{1,12}".prop_map(|name| format!("{{{{{name}}}}}"))
}

fn auth() -> impl Strategy<Value = Auth> {
    prop_oneof![
        (text(), placeholder()).prop_map(|(username, password)| Auth::Basic { username, password }),
        placeholder().prop_map(|token| Auth::Bearer { token }),
        (
            text(),
            placeholder(),
            prop::sample::select(vec![ApiKeyPlacement::Header, ApiKeyPlacement::Query]),
        )
            .prop_map(|(name, value, placement)| Auth::ApiKey {
                name,
                value,
                placement,
            }),
    ]
}

fn endpoint() -> impl Strategy<Value = Endpoint> {
    (
        any::<u32>(),
        text(),
        method(),
        text(),
        prop::collection::vec(header(), 0..=5),
        prop::collection::vec(param(), 0..=5),
        prop::option::of(body()),
        prop::option::of(auth()),
    )
        .prop_map(
            |(seq, name, method, url, headers, params, body, auth)| Endpoint {
                seq,
                name,
                assertions: Vec::new(),
                extract: std::collections::BTreeMap::new(),
                persist: Vec::new(),
                request: Request {
                    method,
                    url,
                    headers,
                    params,
                    body,
                    auth,
                    insecure: false,
                },
            },
        )
}

proptest! {
    #[test]
    fn endpoint_save_load_round_trip(original in endpoint()) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("endpoint.toml");

        // Fresh save (no merge path).
        save_endpoint(&path, &original).unwrap();
        prop_assert_eq!(&load_endpoint(&path).unwrap(), &original);

        // Save again over the existing file (merge path) — still identical.
        save_endpoint(&path, &original).unwrap();
        prop_assert_eq!(&load_endpoint(&path).unwrap(), &original);
    }
}
