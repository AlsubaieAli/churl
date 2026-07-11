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
    assert_eq!(jbody.kind, BodyKind::Json);
    assert_eq!(jbody.content, "{\"a\":1}");
    let fbody = import.requests[1].endpoint.request.body.as_ref().unwrap();
    assert_eq!(fbody.kind, BodyKind::Form);
    assert_eq!(fbody.content, "a=1&b=2");
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
            },
        },
        Endpoint {
            seq: 1,
            name: "create user".into(),
            request: Request {
                method: Method::Post,
                url: "https://api.test/users".into(),
                headers: Vec::new(),
                params: Vec::new(),
                body: Some(Body {
                    kind: BodyKind::Json,
                    content: "{\"name\":\"Ada\"}".into(),
                }),
                auth: Some(Auth::Basic {
                    username: "alice".into(),
                    password: "{{password}}".into(),
                }),
            },
        },
        Endpoint {
            seq: 2,
            name: "form post".into(),
            request: Request {
                method: Method::Post,
                url: "https://api.test/form".into(),
                headers: Vec::new(),
                params: Vec::new(),
                body: Some(Body {
                    kind: BodyKind::Form,
                    content: "a=1&b=2".into(),
                }),
                auth: Some(Auth::ApiKey {
                    name: "X-Api-Key".into(),
                    value: "{{api_key}}".into(),
                    placement: ApiKeyPlacement::Header,
                }),
            },
        },
    ]
}

/// Structural equality on the request fields that must round-trip.
fn assert_request_eq(a: &Request, b: &Request) {
    assert_eq!(a.method, b.method, "method");
    assert_eq!(a.url, b.url, "url");
    assert_eq!(a.headers, b.headers, "headers");
    assert_eq!(a.body, b.body, "body");
    assert_eq!(a.auth, b.auth, "auth");
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
        request: Request {
            method: Method::Get,
            url: "https://e/x".into(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: Some(Auth::Bearer {
                token: "ghp_literalsecret".into(),
            }),
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
