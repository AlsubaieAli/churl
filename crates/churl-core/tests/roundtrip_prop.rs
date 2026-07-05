//! Property test: any `Endpoint` survives a save → load round-trip unchanged.

use churl_core::model::{Body, BodyKind, Endpoint, Header, Method, Param, Request};
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

fn body() -> impl Strategy<Value = Body> {
    (body_kind(), text()).prop_map(|(kind, content)| Body { kind, content })
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
    )
        .prop_map(|(seq, name, method, url, headers, params, body)| Endpoint {
            seq,
            name,
            request: Request {
                method,
                url,
                headers,
                params,
                body,
            },
        })
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
