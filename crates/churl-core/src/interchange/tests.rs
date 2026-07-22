use super::*;

#[test]
fn imports_name_and_flat_requests() {
    let json = r#"{
            "info": { "name": "My API", "schema": "…v2.1.0…" },
            "item": [
                { "name": "list users", "request": { "method": "GET", "url": { "raw": "https://api.test/users" } } },
                { "name": "create", "request": { "method": "POST", "url": "https://api.test/users" } }
            ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    assert_eq!(import.name, "My API");
    assert_eq!(import.requests.len(), 2);
    assert_eq!(import.requests[0].endpoint.name, "list users");
    assert_eq!(import.requests[0].endpoint.request.method, Method::Get);
    assert_eq!(
        import.requests[0].endpoint.request.url,
        "https://api.test/users"
    );
    // Bare-string url form is accepted.
    assert_eq!(
        import.requests[1].endpoint.request.url,
        "https://api.test/users"
    );
    assert!(import.requests[0].folder_path.is_empty());
}

#[test]
fn unnamed_item_uses_the_shared_u6_derive_name() {
    // U6: the interchange importer no longer has its own name deriver — an item
    // with no `name` falls back to the SAME `<METHOD> <≤3 path segs>` scheme the
    // curl importer uses (one shared `derive_name`, threading the method). It
    // passes an empty suffix, so a Postman/native import gets NO `curl` provenance
    // token (that marker is reserved for actual curl imports).
    let json = r#"{
            "info": { "name": "My API", "schema": "…v2.1.0…" },
            "item": [
                { "request": { "method": "POST", "url": { "raw": "https://api.test/v1/users/42" } } },
                { "request": "https://api.test/health" }
            ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    assert_eq!(import.requests.len(), 2);
    // Method threaded through: POST, last ≤3 path segments, NO `curl` marker.
    assert_eq!(import.requests[0].endpoint.name, "POST v1 users 42");
    // A bare-URL item defaults to GET (matching the importer) → GET scheme.
    assert_eq!(import.requests[1].endpoint.name, "GET health");
    // The separator is always a space — never a `/` (path-addressing safety).
    assert!(!import.requests[0].endpoint.name.contains('/'));
}

#[test]
fn imports_nested_folders_into_folder_path() {
    let json = r#"{
            "info": { "name": "C" },
            "item": [
                { "name": "outer", "item": [
                    { "name": "inner", "item": [
                        { "name": "deep req", "request": { "method": "GET", "url": { "raw": "https://e/x" } } }
                    ] }
                ] }
            ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    assert_eq!(import.requests.len(), 1);
    assert_eq!(import.requests[0].folder_path, vec!["outer", "inner"]);
}

#[test]
fn imports_headers_with_disabled_flag() {
    let json = r#"{
            "item": [ { "name": "r", "request": {
                "method": "GET",
                "url": { "raw": "https://e/x" },
                "header": [
                    { "key": "Accept", "value": "application/json" },
                    { "key": "X-Debug", "value": "1", "disabled": true }
                ]
            } } ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    let headers = &import.requests[0].endpoint.request.headers;
    assert_eq!(headers.len(), 2);
    assert!(headers[0].enabled);
    assert!(!headers[1].enabled);
}

#[test]
fn imports_raw_json_and_urlencoded_bodies() {
    let json = r#"{
            "item": [
                { "name": "j", "request": { "method": "POST", "url": { "raw": "https://e/j" },
                    "body": { "mode": "raw", "raw": "{\"a\":1}", "options": { "raw": { "language": "json" } } } } },
                { "name": "f", "request": { "method": "POST", "url": { "raw": "https://e/f" },
                    "body": { "mode": "urlencoded", "urlencoded": [ { "key": "a", "value": "1" }, { "key": "b", "value": "2" } ] } } }
            ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    let jbody = import.requests[0].endpoint.request.body.as_ref().unwrap();
    assert_eq!(
        jbody,
        &Body::Simple {
            kind: BodyKind::Json,
            content: "{\"a\":1}".into()
        }
    );
    let fbody = import.requests[1].endpoint.request.body.as_ref().unwrap();
    assert_eq!(
        fbody,
        &Body::Simple {
            kind: BodyKind::Form,
            content: "a=1&b=2".into()
        }
    );
}

#[test]
fn unsupported_body_mode_warns_and_drops_body() {
    let json = r#"{
            "item": [ { "name": "u", "request": { "method": "POST", "url": { "raw": "https://e/u" },
                "body": { "mode": "formdata", "formdata": [] } } } ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    assert!(import.requests[0].endpoint.request.body.is_none());
    assert!(
        import.warnings.iter().any(|w| w.contains("formdata")),
        "{:?}",
        import.warnings
    );
}

#[test]
fn imports_each_auth_kind_with_secret_placeholders() {
    let json = r#"{
            "item": [
                { "name": "b", "request": { "method": "GET", "url": { "raw": "https://e/b" },
                    "auth": { "type": "basic", "basic": [ { "key": "username", "value": "alice" }, { "key": "password", "value": "s3cr3t" } ] } } },
                { "name": "t", "request": { "method": "GET", "url": { "raw": "https://e/t" },
                    "auth": { "type": "bearer", "bearer": [ { "key": "token", "value": "ghp_literal" } ] } } },
                { "name": "k", "request": { "method": "GET", "url": { "raw": "https://e/k" },
                    "auth": { "type": "apikey", "apikey": [ { "key": "key", "value": "X-Api-Key" }, { "key": "value", "value": "abc123" }, { "key": "in", "value": "header" } ] } } }
            ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    assert_eq!(
        import.requests[0].endpoint.request.auth,
        Some(Auth::Basic {
            username: "alice".into(),
            password: "{{password}}".into(),
        })
    );
    assert_eq!(
        import.requests[1].endpoint.request.auth,
        Some(Auth::Bearer {
            token: "{{token}}".into(),
        })
    );
    assert_eq!(
        import.requests[2].endpoint.request.auth,
        Some(Auth::ApiKey {
            name: "X-Api-Key".into(),
            value: "{{api_key}}".into(),
            placement: ApiKeyPlacement::Header,
        })
    );
    // Every placeholder-ized secret raised a warning.
    assert!(
        import
            .warnings
            .iter()
            .filter(|w| w.contains("placeholder"))
            .count()
            >= 3
    );
}

#[test]
fn keeps_placeholder_auth_verbatim() {
    let json = r#"{
            "item": [ { "name": "b", "request": { "method": "GET", "url": { "raw": "https://e/b" },
                "auth": { "type": "bearer", "bearer": [ { "key": "token", "value": "{{gh_token}}" } ] } } } ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    assert_eq!(
        import.requests[0].endpoint.request.auth,
        Some(Auth::Bearer {
            token: "{{gh_token}}".into(),
        })
    );
    assert!(!import.warnings.iter().any(|w| w.contains("placeholder")));
}

#[test]
fn collection_variables_warn_not_imported() {
    let json = r#"{
            "info": { "name": "C" },
            "variable": [ { "key": "base_url", "value": "https://e" } ],
            "item": []
        }"#;
    let import = import_postman_v21(json).unwrap();
    assert!(import.warnings.iter().any(|w| w.contains("variable")));
}

#[test]
fn var_placeholder_in_url_survives() {
    let json = r#"{
            "item": [ { "name": "r", "request": { "method": "GET", "url": { "raw": "https://{{host}}/x" } } } ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    assert_eq!(
        import.requests[0].endpoint.request.url,
        "https://{{host}}/x"
    );
}

#[test]
fn rejects_non_collection_json() {
    assert!(matches!(
        import_postman_v21(r#"{ "foo": 1 }"#),
        Err(InterchangeError::UnsupportedSchema(_))
    ));
    assert!(matches!(
        import_postman_v21("not json"),
        Err(InterchangeError::Json(_))
    ));
}

fn sample_endpoints() -> Vec<Endpoint> {
    vec![
        Endpoint {
            seq: 0,
            name: "get users".into(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Get,
                url: "https://api.test/users?page=2".into(),
                headers: vec![
                    crate::model::Header {
                        name: "Accept".into(),
                        value: "application/json".into(),
                        enabled: true,
                    },
                    crate::model::Header {
                        name: "X-Debug".into(),
                        value: "1".into(),
                        enabled: false,
                    },
                ],
                params: Vec::new(),
                body: None,
                auth: Some(Auth::Bearer {
                    token: "{{token}}".into(),
                }),
                insecure: false,
            },
        },
        Endpoint {
            seq: 1,
            name: "create user".into(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Post,
                url: "https://api.test/users".into(),
                headers: Vec::new(),
                params: Vec::new(),
                body: Some(Body::Simple {
                    kind: BodyKind::Json,
                    content: "{\"name\":\"Ada\"}".into(),
                }),
                auth: Some(Auth::Basic {
                    username: "alice".into(),
                    password: "{{password}}".into(),
                }),
                insecure: false,
            },
        },
        Endpoint {
            seq: 2,
            name: "form post".into(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Post,
                url: "https://api.test/form".into(),
                headers: Vec::new(),
                params: Vec::new(),
                body: Some(Body::Simple {
                    kind: BodyKind::Form,
                    content: "a=1&b=2".into(),
                }),
                auth: Some(Auth::ApiKey {
                    name: "X-Api-Key".into(),
                    value: "{{api_key}}".into(),
                    placement: ApiKeyPlacement::Header,
                }),
                insecure: false,
            },
        },
    ]
}

/// Structural equality on the request fields that must round-trip.
fn assert_request_eq(a: &Request, b: &Request) {
    assert_eq!(a.method, b.method, "method");
    assert_eq!(a.url, b.url, "url");
    assert_eq!(a.headers, b.headers, "headers");
    assert_eq!(a.params, b.params, "params");
    assert_eq!(a.body, b.body, "body");
    assert_eq!(a.auth, b.auth, "auth");
    assert_eq!(a.insecure, b.insecure, "insecure");
}

#[test]
fn postman_round_trip_preserves_requests() {
    let endpoints = sample_endpoints();
    let json = export_collection("My API", &endpoints, JsonDialect::Postman).unwrap();
    let import = import_postman_v21(&json).unwrap();
    assert_eq!(import.name, "My API");
    assert_eq!(import.requests.len(), endpoints.len());
    for (original, imported) in endpoints.iter().zip(&import.requests) {
        assert_eq!(imported.endpoint.name, original.name);
        assert_request_eq(&imported.endpoint.request, &original.request);
    }
}

#[test]
fn native_export_is_valid_json_with_endpoints() {
    let endpoints = sample_endpoints();
    let json = export_collection("My API", &endpoints, JsonDialect::Native).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["churl_version"], json!(CHURL_NATIVE_VERSION));
    assert_eq!(value["collections"][0]["endpoints"][0]["name"], "get users");
    assert_eq!(
        value["collections"][0]["endpoints"][1]["request"]["method"],
        "POST"
    );
}

#[test]
fn export_refuses_literal_secret_auth() {
    let endpoints = vec![Endpoint {
        seq: 0,
        name: "leaky".into(),
        assertions: Vec::new(),
        extract: std::collections::BTreeMap::new(),
        persist: Vec::new(),
        request: Request {
            method: Method::Get,
            url: "https://e/x".into(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: Some(Auth::Bearer {
                token: "ghp_literalsecret".into(),
            }),
            insecure: false,
        },
    }];
    for dialect in [JsonDialect::Postman, JsonDialect::Native] {
        let err = export_collection("c", &endpoints, dialect).unwrap_err();
        assert!(
            matches!(err, InterchangeError::Secrets { .. }),
            "{err:?} for {dialect:?}"
        );
    }
}

#[test]
fn write_import_flattens_folders_into_collection_dirs() {
    let json = r#"{
            "info": { "name": "My API" },
            "item": [
                { "name": "root req", "request": { "method": "GET", "url": { "raw": "https://e/r" } } },
                { "name": "outer", "item": [
                    { "name": "inner", "item": [
                        { "name": "deep", "request": { "method": "POST", "url": { "raw": "https://e/d" } } }
                    ] }
                ] }
            ]
        }"#;
    let import = import_postman_v21(json).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let summary = write_import(dir.path(), &import).unwrap();
    assert_eq!(summary.endpoints, 2);
    assert_eq!(summary.collections, 2);
    // Root-level request → a collection named after the import.
    assert!(dir.path().join("my-api").join("root-req.toml").exists());
    // Nested folders flatten via " / " → slugified "outer-inner".
    let nested = dir.path().join("outer-inner").join("deep.toml");
    assert!(nested.exists(), "missing {}", nested.display());
}

#[test]
fn write_import_bootstraps_manifest_in_bare_dir() {
    let json = r#"{ "info": { "name": "My API" },
            "item": [ { "name": "list", "request": { "method": "GET", "url": { "raw": "https://e/l" } } } ] }"#;
    let import = import_postman_v21(json).unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Bare dir: no manifest yet, so the TUI would open an empty workspace.
    assert!(persistence::load_workspace_manifest(dir.path()).is_err());
    write_import(dir.path(), &import).unwrap();
    // A manifest now exists → the launched TUI can open + display the import.
    let ws = persistence::load_workspace_manifest(dir.path()).expect("manifest was bootstrapped");
    assert_eq!(ws.name, "My API");
    // A second import into the now-established workspace keeps the manifest.
    let other = import_postman_v21(r#"{ "info": { "name": "Renamed" }, "item": [] }"#).unwrap();
    write_import(dir.path(), &other).unwrap();
    assert_eq!(
        persistence::load_workspace_manifest(dir.path())
            .unwrap()
            .name,
        "My API",
        "an existing manifest is preserved, not overwritten"
    );
}

#[test]
fn write_import_warns_on_collection_slug_collision() {
    // Two folder names that slugify to the same directory ("a-b").
    let json = r#"{ "info": { "name": "root" },
            "item": [
                { "name": "A B", "item": [ { "name": "one", "request": { "method": "GET", "url": { "raw": "https://e/1" } } } ] },
                { "name": "a-b", "item": [ { "name": "two", "request": { "method": "GET", "url": { "raw": "https://e/2" } } } ] }
            ] }"#;
    let import = import_postman_v21(json).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let summary = write_import(dir.path(), &import).unwrap();
    assert_eq!(summary.endpoints, 2);
    assert_eq!(
        summary.collections, 1,
        "distinct names collided into one dir"
    );
    assert!(
        summary.warnings.iter().any(|w| w.contains("collision")),
        "expected a collision warning, got {:?}",
        summary.warnings
    );
}

#[test]
fn structured_url_without_raw_warns_and_imports_empty() {
    let json = r#"{ "info": { "name": "x" },
            "item": [ { "name": "q", "request": { "method": "GET", "url": { "host": ["e"], "path": ["p"] } } } ] }"#;
    let import = import_postman_v21(json).unwrap();
    assert_eq!(import.requests[0].endpoint.request.url, "");
    assert!(
        import.warnings.iter().any(|w| w.contains("url.raw")),
        "expected a url.raw warning, got {:?}",
        import.warnings
    );
}

// --- churl-native import + auto-detect dispatch ---

/// A varied source workspace laid out on disk: two collections covering every
/// body kind, every auth kind (`{{var}}` placeholders — export refuses literal
/// secrets), headers, and query params. Returns `(root, source endpoints per
/// collection in document order)`, where the source endpoints are read *back
/// from disk* so they are the exact bytes the exporter will see.
fn seed_native_source(dir: &Path) -> Vec<(String, Vec<Endpoint>)> {
    use crate::model::{Header, Param};

    let groups: Vec<(&str, Vec<Endpoint>)> = vec![
        (
            "Users",
            vec![
                Endpoint {
                    seq: 0,
                    name: "list users".into(),
                    assertions: Vec::new(),
                    extract: std::collections::BTreeMap::new(),
                    persist: Vec::new(),
                    request: Request {
                        method: Method::Get,
                        url: "https://api.test/users".into(),
                        headers: vec![
                            Header {
                                name: "Accept".into(),
                                value: "application/json".into(),
                                enabled: true,
                            },
                            Header {
                                name: "X-Debug".into(),
                                value: "1".into(),
                                enabled: false,
                            },
                        ],
                        params: vec![
                            Param {
                                name: "page".into(),
                                value: "2".into(),
                                enabled: true,
                            },
                            Param {
                                name: "archived".into(),
                                value: "true".into(),
                                enabled: false,
                            },
                        ],
                        body: None,
                        auth: Some(Auth::Bearer {
                            token: "{{token}}".into(),
                        }),
                        // Durable per-endpoint insecure-TLS: must survive the
                        // native export→import round-trip below.
                        insecure: true,
                    },
                },
                Endpoint {
                    seq: 1,
                    name: "create user".into(),
                    assertions: Vec::new(),
                    extract: std::collections::BTreeMap::new(),
                    persist: Vec::new(),
                    request: Request {
                        method: Method::Post,
                        url: "https://api.test/users".into(),
                        headers: Vec::new(),
                        params: Vec::new(),
                        body: Some(Body::Simple {
                            kind: BodyKind::Json,
                            content: "{\"name\":\"Ada\"}".into(),
                        }),
                        auth: Some(Auth::Basic {
                            username: "alice".into(),
                            password: "{{password}}".into(),
                        }),
                        insecure: false,
                    },
                },
            ],
        ),
        (
            "Misc",
            vec![
                Endpoint {
                    seq: 0,
                    name: "form post".into(),
                    assertions: Vec::new(),
                    extract: std::collections::BTreeMap::new(),
                    persist: Vec::new(),
                    request: Request {
                        method: Method::Put,
                        url: "https://api.test/form".into(),
                        headers: Vec::new(),
                        params: Vec::new(),
                        body: Some(Body::Simple {
                            kind: BodyKind::Form,
                            content: "a=1&b=2".into(),
                        }),
                        auth: Some(Auth::ApiKey {
                            name: "X-Api-Key".into(),
                            value: "{{api_key}}".into(),
                            placement: ApiKeyPlacement::Header,
                        }),
                        insecure: false,
                    },
                },
                Endpoint {
                    seq: 1,
                    name: "text ping".into(),
                    assertions: Vec::new(),
                    extract: std::collections::BTreeMap::new(),
                    persist: Vec::new(),
                    request: Request {
                        method: Method::Delete,
                        url: "https://api.test/ping".into(),
                        headers: Vec::new(),
                        params: Vec::new(),
                        body: Some(Body::Simple {
                            kind: BodyKind::Text,
                            content: "ping".into(),
                        }),
                        auth: Some(Auth::ApiKey {
                            name: "token".into(),
                            value: "{{api_key}}".into(),
                            placement: ApiKeyPlacement::Query,
                        }),
                        insecure: false,
                    },
                },
            ],
        ),
    ];

    persistence::save_workspace_manifest(
        dir,
        &Workspace {
            name: "Source WS".into(),
            vars: std::collections::BTreeMap::new(),
            profiles: Vec::new(),
            ..Default::default()
        },
    )
    .unwrap();

    let mut reloaded = Vec::new();
    for (cname, endpoints) in &groups {
        let cdir = ensure_collection(dir, cname).unwrap();
        for endpoint in endpoints {
            let path = persistence::create_endpoint(&cdir, &endpoint.name).unwrap();
            let mut ep = endpoint.clone();
            ep.seq = load_seq(&path);
            persistence::save_endpoint(&path, &ep).unwrap();
        }
        // Read back what actually landed on disk — this is the source of truth.
        let loaded = OpenWorkspace::open(dir)
            .unwrap()
            .collections()
            .unwrap()
            .into_iter()
            .find(|c| c.name == persistence::slug_of(cname))
            .expect("collection dir")
            .endpoints()
            .unwrap()
            .into_iter()
            .map(|(_, ep)| ep)
            .collect::<Vec<_>>();
        reloaded.push((persistence::slug_of(cname), loaded));
    }
    reloaded
}

#[test]
fn native_round_trip_preserves_endpoints_across_collections() {
    // Source workspace on disk → export_workspace(Native) → import_json →
    // write_import into a fresh root → reload. The endpoint set (per collection,
    // in document order) must survive byte-for-byte on every round-tripped field.
    let src = tempfile::tempdir().unwrap();
    let source = seed_native_source(src.path());

    let ws = OpenWorkspace::open(src.path()).unwrap();
    let json = export_workspace(&ws, JsonDialect::Native).unwrap();

    let import = import_json(&json).unwrap();
    assert!(
        import.warnings.is_empty(),
        "clean native round-trip should warn about nothing, got {:?}",
        import.warnings
    );
    // Every request is tagged with its (slugified) collection name as folder_path.
    let total_source: usize = source.iter().map(|(_, e)| e.len()).sum();
    assert_eq!(import.requests.len(), total_source);

    let dst = tempfile::tempdir().unwrap();
    write_import(dst.path(), &import).unwrap();

    // Reload the target and compare collection-by-collection, order preserved.
    let target_ws = OpenWorkspace::open(dst.path()).unwrap();
    let mut target: std::collections::BTreeMap<String, Vec<Endpoint>> =
        std::collections::BTreeMap::new();
    for collection in target_ws.collections().unwrap() {
        let eps = collection
            .endpoints()
            .unwrap()
            .into_iter()
            .map(|(_, ep)| ep)
            .collect::<Vec<_>>();
        target.insert(collection.name, eps);
    }

    for (cname, src_eps) in &source {
        let tgt_eps = target
            .get(cname)
            .unwrap_or_else(|| panic!("missing collection {cname:?} after round-trip"));
        assert_eq!(
            tgt_eps.len(),
            src_eps.len(),
            "endpoint count for collection {cname:?}"
        );
        for (orig, round) in src_eps.iter().zip(tgt_eps) {
            assert_eq!(round.name, orig.name, "name in {cname:?}");
            assert_request_eq(&round.request, &orig.request);
        }
    }
}

#[test]
fn native_import_skips_bad_endpoint_with_warning() {
    // One deserializable endpoint and one garbage entry: the garbage is dropped
    // with a warning; the good one still imports (liberal, like the Postman path).
    let json = r#"{
            "churl_version": 1,
            "name": "N",
            "collections": [
                { "name": "c", "endpoints": [
                    { "name": "ok", "request": { "method": "GET", "url": "https://e/x" } },
                    { "name": "bad", "request": { "method": 42 } }
                ] }
            ]
        }"#;
    let import = import_churl_native(json).unwrap();
    assert_eq!(import.requests.len(), 1);
    assert_eq!(import.requests[0].endpoint.name, "ok");
    assert_eq!(import.requests[0].folder_path, vec!["c".to_owned()]);
    assert!(
        import
            .warnings
            .iter()
            .any(|w| w.contains("skipped endpoint")),
        "{:?}",
        import.warnings
    );
}

#[test]
fn native_import_blank_name_falls_back_to_imported() {
    let json = r#"{ "churl_version": 1, "collections": [] }"#;
    let import = import_churl_native(json).unwrap();
    assert_eq!(import.name, "Imported");
    assert!(import.requests.is_empty());
}

#[test]
fn native_import_rejects_missing_version() {
    let err = import_churl_native(r#"{ "name": "x", "collections": [] }"#).unwrap_err();
    match err {
        InterchangeError::UnsupportedSchema(msg) => assert!(msg.contains("churl_version"), "{msg}"),
        other => panic!("expected UnsupportedSchema, got {other:?}"),
    }
}

#[test]
fn native_import_rejects_non_integer_version() {
    // Present but the wrong type: the error names the real problem, not "missing".
    let err = import_churl_native(r#"{ "churl_version": "1", "collections": [] }"#).unwrap_err();
    match err {
        InterchangeError::UnsupportedSchema(msg) => {
            assert!(msg.contains("non-negative integer"), "{msg}");
        }
        other => panic!("expected UnsupportedSchema, got {other:?}"),
    }
}

#[test]
fn native_import_rejects_newer_version() {
    let json = format!(
        r#"{{ "churl_version": {}, "name": "x", "collections": [] }}"#,
        CHURL_NATIVE_VERSION + 1
    );
    let err = import_churl_native(&json).unwrap_err();
    match err {
        InterchangeError::UnsupportedSchema(msg) => {
            assert!(msg.contains("newer than this build"), "{msg}");
        }
        other => panic!("expected UnsupportedSchema, got {other:?}"),
    }
}

#[test]
fn dispatch_routes_native_and_postman_and_rejects_unknown() {
    // Native envelope → native (churl_version present).
    let native = r#"{ "churl_version": 1, "name": "N",
        "collections": [ { "name": "c", "endpoints": [
            { "name": "e", "request": { "method": "GET", "url": "https://e/x" } } ] } ] }"#;
    let import = import_json(native).unwrap();
    assert_eq!(import.name, "N");
    assert_eq!(import.requests[0].folder_path, vec!["c".to_owned()]);

    // Postman envelope → Postman (info/item present, no churl_version).
    let postman = r#"{ "info": { "name": "P" },
        "item": [ { "name": "r", "request": { "method": "GET", "url": { "raw": "https://e/x" } } } ] }"#;
    let import = import_json(postman).unwrap();
    assert_eq!(import.name, "P");
    // Postman requests carry no folder_path at the root.
    assert!(import.requests[0].folder_path.is_empty());

    // Neither envelope → clear error.
    let err = import_json(r#"{ "something": true }"#).unwrap_err();
    match err {
        InterchangeError::UnsupportedSchema(msg) => assert!(msg.contains("unrecognized"), "{msg}"),
        other => panic!("expected UnsupportedSchema, got {other:?}"),
    }

    // A newer native file errors through the dispatcher too.
    let newer = format!(r#"{{ "churl_version": {} }}"#, CHURL_NATIVE_VERSION + 1);
    assert!(matches!(
        import_json(&newer),
        Err(InterchangeError::UnsupportedSchema(_))
    ));

    // Bad JSON is still a hard JSON error.
    assert!(matches!(
        import_json("not json"),
        Err(InterchangeError::Json(_))
    ));
}
