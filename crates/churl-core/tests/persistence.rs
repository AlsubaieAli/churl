//! Integration tests for `churl-core` persistence: comment preservation, workspace
//! manifest round-trips with secrets enforcement, and lazy collection loading.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use churl_core::model::{
    ApiKeyPlacement, Auth, Body, BodyKind, CollectionMeta, Endpoint, Header, Method, Param,
    Profile, Request, Workspace,
};
use churl_core::persistence::endpoint_to_toml;
use churl_core::persistence::{
    Collection, OpenWorkspace, PersistenceError, create_collection, create_endpoint,
    create_sequence, delete_collection, delete_endpoint, delete_sequence, load_collection_meta,
    load_endpoint, load_workspace_manifest, rename_collection, rename_endpoint,
    save_collection_meta, save_endpoint, save_endpoint_checked, save_workspace_manifest,
};
use churl_core::secrets::SecretPolicy;

/// Comment-bearing endpoint fixtures: (name, contents, every comment that must survive).
const FIXTURES: &[(&str, &str, &[&str])] = &[
    (
        "basic.toml",
        include_str!("fixtures/basic.toml"),
        &[
            "# My favourite endpoint.",
            "# Handle with care.",
            "# human-readable name",
            "# only reads",
            "# prod URL",
        ],
    ),
    (
        "headers_body.toml",
        include_str!("fixtures/headers_body.toml"),
        &[
            "# Endpoint: create user.",
            "# auth goes first",
            "# toggled off while testing",
            "# the payload",
        ],
    ),
    (
        "params.toml",
        include_str!("fixtures/params.toml"),
        &[
            "# Search endpoint.",
            "# seq is deliberately high: sorts last.",
            "# query params below",
            "# limit results",
        ],
    ),
    (
        "auth.toml",
        include_str!("fixtures/auth.toml"),
        &[
            "# Endpoint with first-class auth.",
            "# credentials come from the profile, never this file",
            "# basic | bearer | apikey",
            "# placeholder, resolved in M6",
        ],
    ),
];

#[test]
fn unchanged_round_trip_is_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    for (name, contents, _) in FIXTURES {
        let path = dir.path().join(name);
        fs::write(&path, contents).unwrap();

        let endpoint = load_endpoint(&path).unwrap();
        save_endpoint(&path, &endpoint).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(&after, contents, "{name}: unchanged save must be a no-op");
    }
}

#[test]
fn mutated_round_trip_keeps_all_comments() {
    let dir = tempfile::tempdir().unwrap();
    for (name, contents, comments) in FIXTURES {
        let path = dir.path().join(name);
        fs::write(&path, contents).unwrap();

        let mut endpoint = load_endpoint(&path).unwrap();
        endpoint.request.url = "https://staging.example.com/changed".to_owned();
        save_endpoint(&path, &endpoint).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("https://staging.example.com/changed"),
            "{name}: url must be updated"
        );
        for comment in *comments {
            assert!(
                after.contains(comment),
                "{name}: comment {comment:?} lost after mutating url:\n{after}"
            );
        }
        // The mutated file must still parse back to the mutated value.
        assert_eq!(load_endpoint(&path).unwrap(), endpoint);
    }
}

#[test]
fn insecure_flag_is_serde_default_and_omitted_when_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ep.toml");

    // Backward compatibility: an endpoint file predating the field (no `insecure`
    // key) loads as secure — no migration needed.
    fs::write(
        &path,
        "seq = 0\nname = \"legacy\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/x\"\n",
    )
    .unwrap();
    let legacy = load_endpoint(&path).unwrap();
    assert!(!legacy.request.insecure, "absent key defaults to secure");

    // A secure endpoint stays byte-minimal: the field is omitted when false.
    save_endpoint(&path, &legacy).unwrap();
    let secure_toml = fs::read_to_string(&path).unwrap();
    assert!(
        !secure_toml.contains("insecure"),
        "insecure = false must be omitted:\n{secure_toml}"
    );

    // Opting in serializes the key and round-trips.
    let mut opted = legacy;
    opted.request.insecure = true;
    save_endpoint(&path, &opted).unwrap();
    let insecure_toml = fs::read_to_string(&path).unwrap();
    assert!(
        insecure_toml.contains("insecure = true"),
        "the opt-in must serialize:\n{insecure_toml}"
    );
    assert!(load_endpoint(&path).unwrap().request.insecure);
}

#[test]
fn headers_render_as_array_of_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.toml");
    let endpoint = Endpoint {
        seq: 3,
        name: "fresh".into(),
        assertions: Vec::new(),
        extract: std::collections::BTreeMap::new(),
        persist: Vec::new(),
        request: Request {
            method: Method::Post,
            url: "https://api.example.com".into(),
            headers: vec![Header {
                name: "Accept".into(),
                value: "application/json".into(),
                enabled: true,
            }],
            params: vec![Param {
                name: "page".into(),
                value: "1".into(),
                enabled: false,
            }],
            body: Some(Body {
                kind: BodyKind::Json,
                content: "{}".into(),
            }),
            auth: None,
            insecure: false,
        },
    };
    save_endpoint(&path, &endpoint).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("[[request.headers]]"),
        "headers must be array-of-tables:\n{text}"
    );
    assert!(
        text.contains("[[request.params]]"),
        "params must be array-of-tables:\n{text}"
    );
    assert!(!text.contains("headers = ["), "no inline arrays:\n{text}");
    assert_eq!(load_endpoint(&path).unwrap(), endpoint);
}

#[test]
fn assertions_render_as_array_of_tables_and_round_trip() {
    use churl_core::assert::{AssertOp, Assertion};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("asserted.toml");
    let endpoint = Endpoint {
        seq: 0,
        name: "asserted".into(),
        extract: std::collections::BTreeMap::new(),
        persist: Vec::new(),
        assertions: vec![
            Assertion {
                target: "status".into(),
                op: AssertOp::Eq,
                value: Some("200".into()),
            },
            Assertion {
                target: "$.data.id".into(),
                op: AssertOp::Exists,
                value: None,
            },
        ],
        request: Request {
            method: Method::Get,
            url: "https://api.example.com".into(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
            insecure: false,
        },
    };
    save_endpoint(&path, &endpoint).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("[[assertions]]"),
        "assertions must be array-of-tables:\n{text}"
    );
    assert!(
        text.contains(r#"op = "==""#),
        "op must serialize canonically:\n{text}"
    );
    assert!(
        text.contains(r#"op = "exists""#),
        "exists must serialize canonically, with no `value` key:\n{text}"
    );
    assert_eq!(load_endpoint(&path).unwrap(), endpoint);

    // An assertion-free endpoint stays byte-minimal: the key is omitted entirely.
    let bare = Endpoint {
        assertions: Vec::new(),
        extract: std::collections::BTreeMap::new(),
        persist: Vec::new(),
        ..endpoint_with_headers("bare", 0)
    };
    let bare_path = dir.path().join("bare.toml");
    save_endpoint(&bare_path, &bare).unwrap();
    let bare_text = fs::read_to_string(&bare_path).unwrap();
    assert!(
        !bare_text.contains("assertions"),
        "empty assertions must be omitted:\n{bare_text}"
    );
}

#[test]
fn endpoint_extract_and_persist_round_trip_and_back_compat() {
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("login.toml");

    // U3: an endpoint carrying its own capture rules. Two rules, one persisted.
    let mut extract = BTreeMap::new();
    extract.insert("token".to_owned(), "$.data.token".into());
    extract.insert("request_id".to_owned(), "header X-Request-Id".into());
    let endpoint = Endpoint {
        seq: 0,
        name: "login".into(),
        assertions: Vec::new(),
        extract,
        persist: vec!["token".to_owned()],
        request: Request {
            method: Method::Post,
            url: "https://api.example.com/login".into(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
            insecure: false,
        },
    };
    save_endpoint(&path, &endpoint).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("[extract]"),
        "extract rules must render as a table:\n{text}"
    );
    assert!(
        text.contains("persist = [\"token\"]") || text.contains("persist = [ \"token\" ]"),
        "persist must render as an array of the persisted rule name:\n{text}"
    );
    // The captured VALUE is never written to disk — only the rule expression and
    // the persisted rule NAME (R3 secret posture).
    assert_eq!(
        load_endpoint(&path).unwrap(),
        endpoint,
        "extract/persist must round-trip losslessly"
    );

    // Back-compat: a legacy endpoint file WITHOUT extract/persist parses (fields
    // default to empty) and, re-serialized, stays byte-minimal — neither key
    // appears, so existing endpoint TOML round-trips unchanged.
    let legacy = "\
seq = 0
name = \"legacy\"

[request]
method = \"GET\"
url = \"https://api.example.com/legacy\"
";
    let legacy_path = dir.path().join("legacy.toml");
    fs::write(&legacy_path, legacy).unwrap();
    let parsed = load_endpoint(&legacy_path).unwrap();
    assert!(
        parsed.extract.is_empty(),
        "absent [extract] must default empty"
    );
    assert!(
        parsed.persist.is_empty(),
        "absent persist must default empty"
    );
    save_endpoint(&legacy_path, &parsed).unwrap();
    let round = fs::read_to_string(&legacy_path).unwrap();
    assert!(
        !round.contains("extract") && !round.contains("persist"),
        "empty extract/persist must be omitted — legacy TOML stays unchanged:\n{round}"
    );
}

// ---- R1 D3: array-of-tables comment preservation on length change ---------

/// Helper: an endpoint with `n` headers `H0..H{n-1}` (all enabled), no auth.
fn endpoint_with_headers(name: &str, n: usize) -> Endpoint {
    Endpoint {
        seq: 0,
        name: name.into(),
        assertions: Vec::new(),
        extract: std::collections::BTreeMap::new(),
        persist: Vec::new(),
        request: Request {
            method: Method::Get,
            url: "https://api.example.com".into(),
            headers: (0..n)
                .map(|i| Header {
                    name: format!("H{i}"),
                    value: format!("v{i}"),
                    enabled: true,
                })
                .collect(),
            params: Vec::new(),
            body: None,
            auth: None,
            insecure: false,
        },
    }
}

/// A `# comment` on a surviving `[[request.headers]]` entry must byte-survive a
/// shrink (3→2) AND a grow (2→3). Before R1 D3 the array-of-tables merge fell
/// through to a wholesale clone on any length change, discarding all decor.
#[test]
fn array_of_tables_comment_survives_length_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("headers.toml");

    // Start from a real 3-header save, then hand-annotate the MIDDLE header table
    // with a comment (simulating a user's hand edit).
    save_endpoint(&path, &endpoint_with_headers("hdrs", 3)).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    // Inject a comment line just above the second `[[request.headers]]` block.
    let annotated = {
        let mut blocks = text.match_indices("[[request.headers]]");
        let _first = blocks.next().unwrap();
        let (second_at, _) = blocks.next().expect("three header blocks");
        let mut s = String::with_capacity(text.len() + 32);
        s.push_str(&text[..second_at]);
        s.push_str("# keep-me: the middle header\n");
        s.push_str(&text[second_at..]);
        s
    };
    fs::write(&path, &annotated).unwrap();
    assert!(fs::read_to_string(&path).unwrap().contains("# keep-me"));

    // Shrink 3 → 2: drop the LAST header. The middle survivor's comment must stay.
    let mut ep = load_endpoint(&path).unwrap();
    assert_eq!(ep.request.headers.len(), 3);
    ep.request.headers.pop(); // now H0, H1
    save_endpoint(&path, &ep).unwrap();
    let after_shrink = fs::read_to_string(&path).unwrap();
    assert!(
        after_shrink.contains("# keep-me: the middle header"),
        "comment on surviving header lost on shrink:\n{after_shrink}"
    );
    assert_eq!(after_shrink.matches("[[request.headers]]").count(), 2);
    // Data is correct after the shrink.
    assert_eq!(load_endpoint(&path).unwrap(), ep);

    // Grow 2 → 3: append a new header. Survivors' comment still preserved.
    ep.request.headers.push(Header {
        name: "H2b".into(),
        value: "v2b".into(),
        enabled: true,
    });
    save_endpoint(&path, &ep).unwrap();
    let after_grow = fs::read_to_string(&path).unwrap();
    assert!(
        after_grow.contains("# keep-me: the middle header"),
        "comment lost on grow:\n{after_grow}"
    );
    assert_eq!(after_grow.matches("[[request.headers]]").count(), 3);
    assert!(
        after_grow.contains("H2b"),
        "new header appended:\n{after_grow}"
    );
    assert_eq!(load_endpoint(&path).unwrap(), ep);
}

#[test]
fn auth_merge_add_change_kind_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth-merge.toml");
    fs::write(&path, include_str!("fixtures/basic.toml")).unwrap();
    let mut endpoint = load_endpoint(&path).unwrap();
    assert!(endpoint.request.auth.is_none());

    // Auth added → a [request.auth] table appears; existing comments survive.
    endpoint.request.auth = Some(Auth::Basic {
        username: "alice".into(),
        password: "{{password}}".into(),
    });
    save_endpoint(&path, &endpoint).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[request.auth]"), "{text}");
    assert!(text.contains("# My favourite endpoint."), "{text}");
    assert_eq!(load_endpoint(&path).unwrap(), endpoint);

    // Kind changed basic → bearer: stale username/password keys must be dropped.
    endpoint.request.auth = Some(Auth::Bearer {
        token: "{{token}}".into(),
    });
    save_endpoint(&path, &endpoint).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(r#"type = "bearer""#), "{text}");
    assert!(
        !text.contains("username") && !text.contains("password"),
        "stale basic keys must be removed on kind change:\n{text}"
    );
    assert_eq!(load_endpoint(&path).unwrap(), endpoint);

    // Auth removed → the table disappears.
    endpoint.request.auth = None;
    save_endpoint(&path, &endpoint).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("[request.auth]"), "{text}");
    assert_eq!(load_endpoint(&path).unwrap(), endpoint);
}

/// An endpoint whose bearer token is a literal secret, not a placeholder.
fn literal_secret_endpoint() -> Endpoint {
    Endpoint {
        seq: 0,
        name: "leaky".into(),
        assertions: Vec::new(),
        extract: std::collections::BTreeMap::new(),
        persist: Vec::new(),
        request: Request {
            method: Method::Get,
            url: "https://api.example.com/x".into(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: Some(Auth::Bearer {
                token: "ghp_definitely_a_literal".into(),
            }),
            insecure: false,
        },
    }
}

#[test]
fn save_endpoint_refuses_literal_secret_auth() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leaky.toml");
    // A brand-new file has no baseline, so a name-anchored literal is NEW and
    // refuses under the strict default.
    let err = save_endpoint(&path, &literal_secret_endpoint()).unwrap_err();
    match err {
        PersistenceError::SecretsRefused { locations } => {
            assert_eq!(locations, vec!["auth.token".to_string()]);
        }
        other => panic!("expected SecretsRefused, got {other:?}"),
    }
    assert!(!path.exists(), "refused save must not write the file");
}

#[test]
fn endpoint_to_toml_refuses_literal_secret_auth() {
    // The stdout path used by `churl import` is gated too: a redirected stdout
    // is a workspace file.
    let err = endpoint_to_toml(&literal_secret_endpoint()).unwrap_err();
    assert!(
        matches!(&err, PersistenceError::SecretsInAuth { names } if names == &["auth.token"]),
        "expected SecretsInAuth, got {err:?}"
    );

    // Placeholder values serialize fine, as an internally-tagged table.
    let mut clean = literal_secret_endpoint();
    clean.request.auth = Some(Auth::ApiKey {
        name: "X-Api-Key".into(),
        value: "{{api_key}}".into(),
        placement: ApiKeyPlacement::Query,
    });
    let toml = endpoint_to_toml(&clean).unwrap();
    assert!(toml.contains("[request.auth]"), "{toml}");
    assert!(toml.contains(r#"type = "apikey""#), "{toml}");
    assert!(toml.contains(r#"placement = "query""#), "{toml}");
}

fn demo_workspace() -> Workspace {
    Workspace {
        name: "demo".into(),
        vars: BTreeMap::new(),
        profiles: vec![
            Profile {
                name: "dev".into(),
                vars: BTreeMap::from([("base_url".to_string(), "http://localhost".to_string())]),
            },
            Profile {
                name: "prod".into(),
                vars: BTreeMap::from([
                    (
                        "base_url".to_string(),
                        "https://api.example.com".to_string(),
                    ),
                    ("api_token".to_string(), "{{API_TOKEN}}".to_string()),
                ]),
            },
        ],
        ..Default::default()
    }
}

#[test]
fn workspace_proxy_round_trips_when_credential_free() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace {
        name: "demo".into(),
        proxy: Some("http://proxy.local:3128".into()),
        cookies: true,
        ..Default::default()
    };
    save_workspace_manifest(dir.path(), &ws).unwrap();
    let loaded = load_workspace_manifest(dir.path()).unwrap();
    assert_eq!(loaded.proxy.as_deref(), Some("http://proxy.local:3128"));
    assert!(loaded.cookies);
    let toml = fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(toml.contains("proxy ="), "{toml}");
    assert!(toml.contains("cookies = true"), "{toml}");
}

#[test]
fn workspace_credentialed_proxy_is_refused_on_save() {
    // The security gate: a proxy with embedded credentials must never reach a
    // synced file — the save is refused loudly, not silently stripped.
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace {
        name: "demo".into(),
        proxy: Some("http://user:pass@proxy.local:3128".into()),
        ..Default::default()
    };
    let err = save_workspace_manifest(dir.path(), &ws).unwrap_err();
    assert!(
        matches!(err, PersistenceError::ProxyCredentialsRefused { .. }),
        "expected ProxyCredentialsRefused, got {err:?}"
    );
    // Nothing was written.
    assert!(!dir.path().join("churl.toml").exists());
}

#[test]
fn workspace_manifest_round_trip_with_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let ws = demo_workspace();
    save_workspace_manifest(dir.path(), &ws).unwrap();
    assert!(dir.path().join("churl.toml").is_file());
    assert_eq!(load_workspace_manifest(dir.path()).unwrap(), ws);
}

#[test]
fn workspace_manifest_refuses_literal_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let mut ws = demo_workspace();
    ws.profiles[1]
        .vars
        .insert("api_token".into(), "sk-live-notatemplate".into());
    // New manifest (no baseline): a name-anchored literal refuses under strict.
    let err = save_workspace_manifest(dir.path(), &ws).unwrap_err();
    match err {
        PersistenceError::SecretsRefused { locations } => {
            assert_eq!(locations, vec!["prod.api_token".to_string()]);
        }
        other => panic!("expected SecretsRefused, got {other:?}"),
    }
    assert!(
        !dir.path().join("churl.toml").exists(),
        "refused save must not write the file"
    );
}

/// Builds a workspace with two collections / three endpoints on disk.
fn build_lazy_workspace(root: &Path) {
    fs::write(root.join("churl.toml"), "name = \"lazy\"\n").unwrap();
    fs::create_dir(root.join(".git")).unwrap(); // hidden: must be skipped
    fs::write(root.join("stray.toml"), "not a collection").unwrap(); // file: skipped

    let users = root.join("users");
    fs::create_dir(&users).unwrap();
    fs::write(users.join("folder.toml"), "# collection metadata\n").unwrap();
    fs::write(
        users.join("get.toml"),
        "seq = 2\nname = \"get\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://a\"\n",
    )
    .unwrap();
    fs::write(
        users.join("create.toml"),
        "seq = 1\nname = \"create\"\n\n[request]\nmethod = \"POST\"\nurl = \"https://b\"\n",
    )
    .unwrap();

    let search = root.join("search");
    fs::create_dir(&search).unwrap();
    fs::write(
        search.join("query.toml"),
        "seq = 1\nname = \"query\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://c\"\n",
    )
    .unwrap();
}

#[test]
fn lazy_collections_list_without_parsing_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    build_lazy_workspace(dir.path());
    // Plant a malformed endpoint: collections() must still succeed because nothing
    // is parsed until endpoints() is called.
    fs::write(dir.path().join("search").join("broken.toml"), "= = =").unwrap();

    let ws = OpenWorkspace::open(dir.path()).unwrap();
    assert_eq!(ws.manifest().name, "lazy");
    assert_eq!(ws.root(), dir.path());

    let collections = ws.collections().unwrap();
    let names: Vec<&str> = collections.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["search", "users"], "sorted, hidden dirs skipped");
}

#[test]
fn endpoints_parse_on_call_and_sort_by_seq() {
    let dir = tempfile::tempdir().unwrap();
    build_lazy_workspace(dir.path());

    let ws = OpenWorkspace::open(dir.path()).unwrap();
    let collections = ws.collections().unwrap();
    let users = collections.iter().find(|c| c.name == "users").unwrap();

    let endpoints = users.endpoints().unwrap();
    let names: Vec<&str> = endpoints.iter().map(|(_, ep)| ep.name.as_str()).collect();
    assert_eq!(
        names,
        ["create", "get"],
        "sorted by seq, folder.toml excluded"
    );
    assert!(endpoints.iter().all(|(path, _)| path.is_file()));
}

#[test]
fn malformed_endpoint_error_names_the_file() {
    let dir = tempfile::tempdir().unwrap();
    build_lazy_workspace(dir.path());
    let broken = dir.path().join("users").join("broken.toml");
    fs::write(&broken, "name = [unclosed\n").unwrap();

    let collection = Collection {
        name: "users".into(),
        path: dir.path().join("users"),
    };
    let err = collection.endpoints().unwrap_err();
    assert!(
        err.to_string().contains("broken.toml"),
        "error must carry the offending path: {err}"
    );
}

// --- M7.3: crash bugfix (churl.toml in a collection dir) + load resilience. ---

#[test]
fn nested_workspace_manifest_is_ignored_not_parsed_as_endpoint() {
    // Regression: a collection directory that is itself a nested workspace (it
    // contains a `churl.toml`) must NOT have that manifest parsed as an endpoint
    // (`missing field 'request'`), which used to abort the whole TUI load.
    let dir = tempfile::tempdir().unwrap();
    build_lazy_workspace(dir.path());
    // Drop a nested manifest into the `users` collection dir.
    fs::write(
        dir.path().join("users").join("churl.toml"),
        "name = \"nested-ws\"\n",
    )
    .unwrap();

    let collection = Collection {
        name: "users".into(),
        path: dir.path().join("users"),
    };

    // Strict path: no error, manifest simply ignored (only the real endpoints).
    let endpoints = collection.endpoints().unwrap();
    let names: Vec<&str> = endpoints.iter().map(|(_, ep)| ep.name.as_str()).collect();
    assert_eq!(
        names,
        ["create", "get"],
        "churl.toml excluded like folder.toml"
    );

    // Lenient path: same endpoints, and zero warnings (the manifest is not a
    // skipped/unparseable endpoint — it is simply not an endpoint file).
    let load = collection.endpoints_lenient().unwrap();
    let lenient_names: Vec<&str> = load
        .endpoints
        .iter()
        .map(|(_, ep)| ep.name.as_str())
        .collect();
    assert_eq!(lenient_names, ["create", "get"]);
    assert!(
        load.warnings.is_empty(),
        "a nested churl.toml must not produce a warning: {:?}",
        load.warnings
    );
}

#[test]
fn lenient_load_degrades_one_bad_file_to_a_warning() {
    // Resilience: one valid endpoint + one garbage `.toml` returns the valid
    // endpoint and exactly one warning naming the bad file; the load does not error.
    let dir = tempfile::tempdir().unwrap();
    build_lazy_workspace(dir.path());
    fs::write(
        dir.path().join("users").join("garbage.toml"),
        "seq = 0\nname = \"broken\"\n", // missing [request] table
    )
    .unwrap();

    let collection = Collection {
        name: "users".into(),
        path: dir.path().join("users"),
    };
    let load = collection.endpoints_lenient().unwrap();
    let names: Vec<&str> = load
        .endpoints
        .iter()
        .map(|(_, ep)| ep.name.as_str())
        .collect();
    assert_eq!(names, ["create", "get"], "valid endpoints still returned");
    assert_eq!(load.warnings.len(), 1, "exactly one skipped file");
    assert!(
        load.warnings[0].contains("garbage.toml"),
        "warning names the bad file: {:?}",
        load.warnings
    );
    // Strict path still fails on the same corpus (existing callers keep strictness).
    assert!(collection.endpoints().is_err());
}

#[test]
fn lenient_load_read_dir_error_is_still_hard() {
    // A missing directory (a real IO failure, not a single bad file) stays a hard
    // error — resilience covers bad files, not a vanished collection.
    let dir = tempfile::tempdir().unwrap();
    let collection = Collection {
        name: "gone".into(),
        path: dir.path().join("does-not-exist"),
    };
    assert!(collection.endpoints_lenient().is_err());
}

#[test]
fn workspace_vars_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace {
        name: "demo".into(),
        vars: BTreeMap::from([
            (
                "base_url".to_string(),
                "https://api.example.com".to_string(),
            ),
            ("api_version".to_string(), "v2".to_string()),
        ]),
        profiles: Vec::new(),
        ..Default::default()
    };
    save_workspace_manifest(dir.path(), &ws).unwrap();
    let text = fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(text.contains("[vars]"), "{text}");
    assert!(
        text.contains(r#"base_url = "https://api.example.com""#),
        "{text}"
    );
    let back = load_workspace_manifest(dir.path()).unwrap();
    assert_eq!(back, ws);
}

#[test]
fn workspace_vars_secret_literal_refused() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace {
        name: "demo".into(),
        vars: BTreeMap::from([("api_token".to_string(), "leaked".to_string())]),
        profiles: Vec::new(),
        ..Default::default()
    };
    let err = save_workspace_manifest(dir.path(), &ws).unwrap_err();
    assert!(
        matches!(err, PersistenceError::SecretsRefused { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("vars.api_token"), "{err}");
    // Nothing was written.
    assert!(!dir.path().join("churl.toml").exists());
}

// --- M7.3: delete/rename must actually prune on disk (editor correctness gate). ---
//
// The format-preserving `merge_tables` removes keys present on disk but absent
// from the saved struct (verified here end-to-end: an editor whose "delete"
// silently left the value on disk would be broken). Surviving keys keep their
// comments; a deleted `[[profiles]]` entry disappears entirely.

#[test]
fn deleting_a_workspace_var_key_removes_it_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    // Hand-written manifest with two vars and comments on the survivor.
    fs::write(
        dir.path().join("churl.toml"),
        concat!(
            "name = \"demo\"\n\n",
            "[vars]\n",
            "# the base URL, keep this\n",
            "base_url = \"https://api.example.com\"\n",
            "page_size = \"50\"\n",
        ),
    )
    .unwrap();

    let mut ws = load_workspace_manifest(dir.path()).unwrap();
    ws.vars.remove("page_size");
    save_workspace_manifest(dir.path(), &ws).unwrap();

    let text = fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(
        !text.contains("page_size"),
        "deleted var key must be gone from disk:\n{text}"
    );
    assert!(
        text.contains("base_url"),
        "sibling var must survive:\n{text}"
    );
    assert!(
        text.contains("# the base URL, keep this"),
        "surviving key keeps its comment:\n{text}"
    );
    // And it is gone semantically on reload.
    let back = load_workspace_manifest(dir.path()).unwrap();
    assert!(!back.vars.contains_key("page_size"));
    assert_eq!(
        back.vars.get("base_url").map(String::as_str),
        Some("https://api.example.com")
    );
}

#[test]
fn deleting_a_profile_removes_its_table_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace {
        name: "demo".into(),
        vars: BTreeMap::new(),
        profiles: vec![
            Profile {
                name: "dev".into(),
                vars: BTreeMap::from([("host".to_string(), "dev.example.com".to_string())]),
            },
            Profile {
                name: "prod".into(),
                vars: BTreeMap::from([("host".to_string(), "prod.example.com".to_string())]),
            },
        ],
        ..Default::default()
    };
    save_workspace_manifest(dir.path(), &ws).unwrap();

    // Delete the `prod` profile and re-save.
    let mut edited = load_workspace_manifest(dir.path()).unwrap();
    edited.profiles.retain(|p| p.name != "prod");
    save_workspace_manifest(dir.path(), &edited).unwrap();

    let text = fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(
        !text.contains("prod") && !text.contains("prod.example.com"),
        "deleted profile must be gone from disk:\n{text}"
    );
    assert!(text.contains("dev"), "surviving profile stays:\n{text}");
    let back = load_workspace_manifest(dir.path()).unwrap();
    assert_eq!(back.profiles.len(), 1);
    assert_eq!(back.profiles[0].name, "dev");
}

#[test]
fn renaming_a_profile_leaves_no_old_name_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace {
        name: "demo".into(),
        vars: BTreeMap::new(),
        profiles: vec![Profile {
            name: "staging".into(),
            vars: BTreeMap::from([("host".to_string(), "stg.example.com".to_string())]),
        }],
        ..Default::default()
    };
    save_workspace_manifest(dir.path(), &ws).unwrap();

    let mut edited = load_workspace_manifest(dir.path()).unwrap();
    edited.profiles[0].name = "prod".into();
    save_workspace_manifest(dir.path(), &edited).unwrap();

    let text = fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(
        !text.contains("staging"),
        "old profile name must be gone:\n{text}"
    );
    assert!(text.contains("prod"), "new profile name present:\n{text}");
    let back = load_workspace_manifest(dir.path()).unwrap();
    assert_eq!(back.profiles.len(), 1);
    assert_eq!(back.profiles[0].name, "prod");
    assert_eq!(
        back.profiles[0].vars.get("host").map(String::as_str),
        Some("stg.example.com")
    );
}

#[test]
fn renaming_a_var_key_leaves_no_old_name_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("churl.toml"),
        concat!("name = \"demo\"\n\n", "[vars]\n", "old_name = \"keepme\"\n",),
    )
    .unwrap();

    let mut ws = load_workspace_manifest(dir.path()).unwrap();
    let value = ws.vars.remove("old_name").unwrap();
    ws.vars.insert("new_name".to_string(), value);
    save_workspace_manifest(dir.path(), &ws).unwrap();

    let text = fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(
        !text.contains("old_name"),
        "old var name must be gone:\n{text}"
    );
    assert!(text.contains("new_name"), "renamed var present:\n{text}");
    let back = load_workspace_manifest(dir.path()).unwrap();
    assert!(!back.vars.contains_key("old_name"));
    assert_eq!(
        back.vars.get("new_name").map(String::as_str),
        Some("keepme")
    );
}

#[test]
fn deleting_a_collection_var_key_removes_it_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("users");
    fs::create_dir(&coll).unwrap();
    fs::write(
        coll.join("folder.toml"),
        concat!(
            "[vars]\n",
            "# keep this default\n",
            "base_path = \"/v1/users\"\n",
            "page_size = \"50\"\n",
        ),
    )
    .unwrap();

    let mut meta = load_collection_meta(&coll).unwrap();
    meta.vars.remove("page_size");
    save_collection_meta(&coll, &meta).unwrap();

    let text = fs::read_to_string(coll.join("folder.toml")).unwrap();
    assert!(
        !text.contains("page_size"),
        "deleted collection var must be gone:\n{text}"
    );
    assert!(text.contains("base_path"), "sibling survives:\n{text}");
    assert!(
        text.contains("# keep this default"),
        "surviving key keeps its comment:\n{text}"
    );
    let back = load_collection_meta(&coll).unwrap();
    assert!(!back.vars.contains_key("page_size"));
    assert_eq!(
        back.vars.get("base_path").map(String::as_str),
        Some("/v1/users")
    );
}

#[test]
fn deleting_the_last_var_removes_the_vars_table() {
    // Emptying a scope's vars must drop the whole `[vars]` table (the struct
    // skips serializing an empty map, and merge prunes the stale table).
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("churl.toml"),
        "name = \"demo\"\n\n[vars]\nonly = \"one\"\n",
    )
    .unwrap();
    let mut ws = load_workspace_manifest(dir.path()).unwrap();
    ws.vars.clear();
    save_workspace_manifest(dir.path(), &ws).unwrap();
    let text = fs::read_to_string(dir.path().join("churl.toml")).unwrap();
    assert!(
        !text.contains("[vars]") && !text.contains("only"),
        "empty vars table must be pruned:\n{text}"
    );
}

#[test]
fn collection_meta_round_trip_preserves_comments() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("users");
    fs::create_dir(&coll).unwrap();
    fs::write(
        coll.join("folder.toml"),
        include_str!("fixtures/folder.toml"),
    )
    .unwrap();

    let meta = load_collection_meta(&coll).unwrap();
    assert_eq!(
        meta.vars.get("base_path").map(String::as_str),
        Some("/v1/users")
    );
    assert_eq!(meta.vars.get("page_size").map(String::as_str), Some("50"));

    // A no-op save preserves every comment byte-for-byte.
    save_collection_meta(&coll, &meta).unwrap();
    let text = fs::read_to_string(coll.join("folder.toml")).unwrap();
    for comment in [
        "# Collection-level defaults for the users collection.",
        "# shared prefix",
        "# default pagination",
    ] {
        assert!(text.contains(comment), "comment lost: {comment}\n{text}");
    }
}

#[test]
fn missing_folder_toml_yields_default() {
    let dir = tempfile::tempdir().unwrap();
    let meta = load_collection_meta(dir.path()).unwrap();
    assert_eq!(meta, CollectionMeta::default());
    assert!(meta.vars.is_empty());
}

#[test]
fn collection_meta_secret_literal_refused() {
    let dir = tempfile::tempdir().unwrap();
    let meta = CollectionMeta {
        vars: BTreeMap::from([("secret_key".to_string(), "abc".to_string())]),
        ..Default::default()
    };
    let err = save_collection_meta(dir.path(), &meta).unwrap_err();
    assert!(
        matches!(err, PersistenceError::SecretsRefused { .. }),
        "{err}"
    );
    assert!(!dir.path().join("folder.toml").exists());
}

// ---- M6.6 CRUD seams ----

#[test]
fn create_endpoint_slug_seq_and_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("users");
    fs::create_dir(&coll).unwrap();
    // Seed two endpoints with seqs 0 and 4 so the next is 5 (plain +1 over max).
    fs::write(
        coll.join("list.toml"),
        "seq = 0\nname = \"List\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://a\"\n",
    )
    .unwrap();
    fs::write(
        coll.join("get.toml"),
        "seq = 4\nname = \"Get\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://b\"\n",
    )
    .unwrap();

    let path = create_endpoint(&coll, "Create User!").unwrap();
    // Slug: non-alphanumeric runs collapse to '-', trailing trimmed.
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        "create-user.toml"
    );
    let ep = load_endpoint(&path).unwrap();
    assert_eq!(ep.seq, 5, "seq is max(0,4)+1");
    assert_eq!(ep.name, "Create User!");
    assert_eq!(ep.request.method, Method::Get);
    assert_eq!(ep.request.url, "");
    assert!(ep.request.headers.is_empty());
    assert!(ep.request.auth.is_none());
}

#[test]
fn create_endpoint_empty_collection_starts_at_seq_zero() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("empty");
    fs::create_dir(&coll).unwrap();
    let path = create_endpoint(&coll, "First").unwrap();
    assert_eq!(load_endpoint(&path).unwrap().seq, 0);
}

#[test]
fn create_endpoint_slug_collision_suffixes() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("c");
    fs::create_dir(&coll).unwrap();
    let a = create_endpoint(&coll, "Ping").unwrap();
    let b = create_endpoint(&coll, "Ping").unwrap();
    assert_eq!(a.file_name().unwrap().to_str().unwrap(), "ping.toml");
    assert_eq!(b.file_name().unwrap().to_str().unwrap(), "ping-2.toml");
    // Two distinct files, distinct seqs.
    assert_ne!(a, b);
}

#[test]
fn create_endpoint_empty_name_errors() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("c");
    fs::create_dir(&coll).unwrap();
    let err = create_endpoint(&coll, "   ").unwrap_err();
    assert!(matches!(err, PersistenceError::EmptyName), "{err}");
}

#[test]
fn rename_endpoint_updates_name_and_file() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("c");
    fs::create_dir(&coll).unwrap();
    let path = create_endpoint(&coll, "Old Name").unwrap();
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), "old-name.toml");

    let new_path = rename_endpoint(&path, "Brand New").unwrap();
    assert_eq!(
        new_path.file_name().unwrap().to_str().unwrap(),
        "brand-new.toml"
    );
    assert!(!path.exists(), "old file gone");
    assert_eq!(load_endpoint(&new_path).unwrap().name, "Brand New");
}

#[test]
fn rename_endpoint_grandfathers_pre_existing_literal_secret() {
    // A hand-written file may already carry a literal secret. Renaming it does
    // not *author* a new secret — the value is untouched — so under the strict
    // baseline-aware gate the pre-existing secret is grandfathered and the rename
    // succeeds, rather than refusing (the old hard block).
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("c");
    fs::create_dir(&coll).unwrap();
    let path = coll.join("leaky.toml");
    fs::write(
        &path,
        concat!(
            "seq = 0\nname = \"Leaky\"\n\n[request]\nmethod = \"GET\"\n",
            "url = \"https://a\"\n\n[request.auth]\ntype = \"bearer\"\n",
            "token = \"ghp_literal_secret\"\n",
        ),
    )
    .unwrap();
    let new_path = rename_endpoint(&path, "New Name").expect("grandfathered rename succeeds");
    assert!(!path.exists(), "the old file is moved");
    assert!(new_path.exists());
    // The pre-existing secret is preserved verbatim (never scrubbed by churl).
    let text = fs::read_to_string(&new_path).unwrap();
    assert!(text.contains("ghp_literal_secret"), "{text}");
    assert!(text.contains("New Name"), "{text}");
}

#[test]
fn delete_endpoint_removes_file() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("c");
    fs::create_dir(&coll).unwrap();
    let path = create_endpoint(&coll, "Doomed").unwrap();
    assert!(path.exists());
    delete_endpoint(&path).unwrap();
    assert!(!path.exists());
}

#[test]
fn delete_sequence_removes_file() {
    let dir = tempfile::tempdir().unwrap();
    // create_sequence makes the sequences/ dir on demand under the root.
    let path = create_sequence(dir.path(), "Doomed flow").unwrap();
    assert!(path.exists());
    delete_sequence(&path).unwrap();
    assert!(!path.exists());
}

#[test]
fn create_collection_makes_dir_without_folder_toml() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "My Orders", dir.path()).unwrap();
    assert_eq!(coll.file_name().unwrap().to_str().unwrap(), "my-orders");
    assert!(coll.is_dir());
    assert!(
        !coll.join("folder.toml").exists(),
        "folder.toml stays lazy until vars exist"
    );
}

#[test]
fn create_collection_refuses_existing() {
    let dir = tempfile::tempdir().unwrap();
    create_collection(dir.path(), "orders", dir.path()).unwrap();
    let err = create_collection(dir.path(), "orders", dir.path()).unwrap_err();
    assert!(
        matches!(err, PersistenceError::AlreadyExists { .. }),
        "{err}"
    );
}

#[test]
fn rename_collection_moves_dir_with_contents() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "old", dir.path()).unwrap();
    let ep = create_endpoint(&coll, "Ping").unwrap();
    assert!(ep.exists());

    let new_coll = rename_collection(&coll, "New Coll", dir.path()).unwrap();
    assert_eq!(new_coll.file_name().unwrap().to_str().unwrap(), "new-coll");
    assert!(!coll.exists());
    assert!(new_coll.join("ping.toml").exists(), "contents moved along");
}

#[test]
fn delete_collection_removes_dir_recursively() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "doomed", dir.path()).unwrap();
    create_endpoint(&coll, "Ping").unwrap();
    delete_collection(&coll).unwrap();
    assert!(!coll.exists());
}

// ---- R1 D1: reserved-name guards ------------------------------------------
//
// A create/rename whose name slugs to a reserved churl stem (`churl` / `folder`
// / `sequences`) must never produce an on-disk file/dir that overshadows the
// manifest, folder metadata, or sequences directory. It disambiguates like a
// name clash (`churl` -> `churl-2.toml`) while the display `name` stays intact.

/// The load-loss bug: an endpoint named `churl` slugged to `churl` and wrote
/// `churl.toml`, silently overwriting the workspace manifest. It must now land on
/// a `-2` stem, leave the manifest byte-intact, and stay loadable with its typed
/// name preserved.
#[test]
fn create_endpoint_named_churl_never_overshadows_manifest() {
    let root = tempfile::tempdir().unwrap();
    save_workspace_manifest(root.path(), &demo_workspace()).unwrap();
    let manifest = root.path().join("churl.toml");
    let manifest_before = fs::read(&manifest).unwrap();

    // The collection dir doubles as a workspace-root-shaped dir for this test:
    // creating an endpoint named "churl" inside it must not write `churl.toml`.
    let coll = create_collection(root.path(), "api", root.path()).unwrap();
    // Plant a sibling manifest inside the collection so an overwrite is visible.
    save_workspace_manifest(&coll, &demo_workspace()).unwrap();
    let coll_manifest_before = fs::read(coll.join("churl.toml")).unwrap();

    let path = create_endpoint(&coll, "churl").unwrap();
    assert_ne!(
        path.file_name().unwrap().to_str().unwrap(),
        "churl.toml",
        "an endpoint named churl must not be written as churl.toml"
    );
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        "churl-2.toml",
        "reserved slug disambiguates like a name clash"
    );

    // Both manifests survived byte-for-byte.
    assert_eq!(fs::read(&manifest).unwrap(), manifest_before);
    assert_eq!(
        fs::read(coll.join("churl.toml")).unwrap(),
        coll_manifest_before
    );

    // The endpoint round-trips with the user's typed display name.
    let ep = load_endpoint(&path).unwrap();
    assert_eq!(ep.name, "churl");
}

/// Case-folding and the other reserved words: `Churl`, `folder`, `CHURL.TOML`
/// each disambiguate; the manifest/folder stems never land verbatim.
#[test]
fn create_endpoint_reserved_variants_all_disambiguate() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "c", dir.path()).unwrap();
    for name in ["Churl", "folder", "FOLDER"] {
        let path = create_endpoint(&coll, name).unwrap();
        let stem = path
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        // The bare reserved stem never appears as the on-disk filename — it was
        // disambiguated (`churl` -> `churl-2`, `folder` -> `folder-2`, …).
        assert!(
            stem != "churl" && stem != "folder",
            "reserved name {name} landed on {stem}"
        );
        assert_ne!(path.file_name().unwrap().to_str().unwrap(), "churl.toml");
        assert_ne!(path.file_name().unwrap().to_str().unwrap(), "folder.toml");
    }
    // `CHURL.TOML` slugs to `churl-toml` (NOT reserved) — lands verbatim.
    let ok = create_endpoint(&coll, "CHURL.TOML").unwrap();
    assert_eq!(
        ok.file_name().unwrap().to_str().unwrap(),
        "churl-toml.toml",
        "a slug that merely contains a reserved word is safe"
    );
}

/// A collection named `sequences` must not become the reserved `sequences/`
/// directory (which `collections()` excludes — the collection would vanish).
#[test]
fn create_collection_named_sequences_never_shadows_sequences_dir() {
    let root = tempfile::tempdir().unwrap();
    let coll = create_collection(root.path(), "sequences", root.path()).unwrap();
    assert_ne!(
        coll.file_name().unwrap().to_str().unwrap(),
        "sequences",
        "a `sequences` collection must not shadow the reserved dir"
    );
    assert_eq!(coll.file_name().unwrap().to_str().unwrap(), "sequences-2");

    // The collection is actually visible (not excluded like the reserved dir).
    save_workspace_manifest(root.path(), &demo_workspace()).unwrap();
    let ws = OpenWorkspace::open(root.path()).unwrap();
    let names: Vec<String> = ws
        .collections()
        .unwrap()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    assert!(
        names.iter().any(|n| n == "sequences-2"),
        "collection is listed: {names:?}"
    );
}

/// M7.9: `sequences` is reserved at the ROOT level ONLY. A sub-collection
/// literally named `sequences` (created inside another collection) is written
/// VERBATIM — never bumped to `sequences-2` — matching the loader, which treats a
/// nested `sequences/` as an ordinary sub-collection.
#[test]
fn create_nested_collection_named_sequences_is_verbatim() {
    let root = tempfile::tempdir().unwrap();
    // A top-level collection to nest inside; root is the workspace root.
    let api = create_collection(root.path(), "api", root.path()).unwrap();

    // Now create a `sequences` sub-collection INSIDE `api` — parent != root, so
    // the reservation must NOT apply.
    let nested = create_collection(&api, "sequences", root.path()).unwrap();
    assert_eq!(
        nested.file_name().unwrap().to_str().unwrap(),
        "sequences",
        "a nested `sequences` collection is created verbatim (root-only reservation)"
    );
    assert_eq!(nested, api.join("sequences"));

    // And the loader agrees: `api`'s sub-collections include the verbatim
    // `sequences` (it is NOT excluded at a non-root level).
    let api_col = Collection {
        name: "api".into(),
        path: api.clone(),
    };
    let subs = api_col.sub_collections().unwrap();
    assert_eq!(
        subs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["sequences"]
    );
}

/// M7.9: renaming a SUB-collection to `sequences` keeps the verbatim name (the
/// reserved bump is root-only), while renaming a ROOT-level collection to
/// `sequences` still bumps to `sequences-2`.
#[test]
fn rename_collection_to_sequences_bumps_only_at_root() {
    let root = tempfile::tempdir().unwrap();

    // Root-level rename → bumped.
    let top = create_collection(root.path(), "top", root.path()).unwrap();
    let renamed_top = rename_collection(&top, "sequences", root.path()).unwrap();
    assert_eq!(
        renamed_top.file_name().unwrap().to_str().unwrap(),
        "sequences-2",
        "a root-level collection renamed to `sequences` is bumped"
    );

    // Nested rename → verbatim.
    let api = create_collection(root.path(), "api", root.path()).unwrap();
    let sub = create_collection(&api, "child", root.path()).unwrap();
    let renamed_sub = rename_collection(&sub, "sequences", root.path()).unwrap();
    assert_eq!(
        renamed_sub.file_name().unwrap().to_str().unwrap(),
        "sequences",
        "a sub-collection renamed to `sequences` keeps the verbatim name"
    );
}

/// Renaming an endpoint *into* a reserved name is the same hazard as creating
/// one, and must disambiguate too.
#[test]
fn rename_endpoint_into_reserved_name_disambiguates() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "c", dir.path()).unwrap();
    let path = create_endpoint(&coll, "Ping").unwrap();

    let new_path = rename_endpoint(&path, "churl").unwrap();
    assert_ne!(
        new_path.file_name().unwrap().to_str().unwrap(),
        "churl.toml",
        "rename into a reserved name must not write churl.toml"
    );
    assert!(!path.exists(), "old file moved");
    let ep = load_endpoint(&new_path).unwrap();
    assert_eq!(ep.name, "churl", "display name is what the user typed");
}

/// Property-style: no user-supplied name can yield a reserved on-disk stem, for
/// either endpoints or collections.
#[test]
fn no_name_yields_a_reserved_on_disk_stem() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "c", dir.path()).unwrap();
    for name in [
        "churl",
        "Churl",
        "CHURL",
        "  churl  ",
        "folder",
        "Folder",
        "sequences",
        "Sequences",
        "churl!!!",
        "**folder**",
    ] {
        // Endpoints: never a reserved *file* stem (`churl`/`folder`). A
        // `sequences.toml` endpoint file inside a collection is harmless — only
        // the top-level `sequences/` DIRECTORY is reserved — so it is allowed.
        let ep_path = create_endpoint(&coll, name).unwrap();
        let ep_stem = ep_path
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            ep_stem != "churl" && ep_stem != "folder",
            "endpoint name {name:?} produced reserved file stem {ep_stem:?}"
        );
        // Collections.
        let root = tempfile::tempdir().unwrap();
        let cdir = create_collection(root.path(), name, root.path()).unwrap();
        let cstem = cdir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert_ne!(
            cstem, "sequences",
            "collection {name:?} shadowed sequences/"
        );
    }
}

#[test]
fn rename_endpoint_same_slug_keeps_filename() {
    // "Get Users" -> "get users": name changes, slug doesn't. The file must
    // stay put — not gain a spurious -2 collision suffix from its own path.
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("c");
    fs::create_dir(&coll).unwrap();
    let path = create_endpoint(&coll, "Get Users").unwrap();

    let new_path = rename_endpoint(&path, "get users").unwrap();
    assert_eq!(new_path, path, "same slug must not move the file");
    assert!(!coll.join("get-users-2.toml").exists());
    assert_eq!(load_endpoint(&new_path).unwrap().name, "get users");
}

// ---- Concurrent-save clobber race (atomic path claim) --------------------

/// True OS-thread concurrency: many threads race to create a new endpoint with
/// the SAME name in the SAME collection. Each must land on its own file — no two
/// share a filename, and every write survives (the atomic `create_new` claim
/// makes the loser of any name race advance to the next `-N` instead of
/// clobbering the winner).
#[test]
fn concurrent_create_endpoint_same_name_never_clobbers() {
    use std::sync::Arc;
    use std::sync::Barrier;

    let dir = tempfile::tempdir().unwrap();
    let coll = Arc::new(dir.path().join("c"));
    fs::create_dir(coll.as_path()).unwrap();

    const THREADS: usize = 12;
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let coll = Arc::clone(&coll);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                create_endpoint(&coll, "Ping").expect("create must not fail under contention")
            })
        })
        .collect();

    let paths: Vec<std::path::PathBuf> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Every returned path is distinct — no two threads claimed the same file.
    let mut names: Vec<String> = paths
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(
        names.len(),
        THREADS,
        "each concurrent create must claim a distinct filename: {names:?}"
    );

    // Every file is on disk and reads back as a `Ping` endpoint — no lost write.
    for path in &paths {
        assert!(path.exists(), "claimed file must persist: {path:?}");
        assert_eq!(load_endpoint(path).unwrap().name, "Ping");
    }

    // The corpus convention held: `ping.toml` plus `ping-2..ping-12.toml`.
    assert!(names.contains(&"ping.toml".to_owned()), "{names:?}");
    assert!(names.contains(&"ping-2.toml".to_owned()), "{names:?}");
}

/// A file placed on disk OUT OF BAND (not via churl) must be respected by the
/// next create: the atomic claim lands on `ping-2.toml` rather than trusting a
/// stale existence probe and overwriting the pre-existing `ping.toml`.
#[test]
fn create_endpoint_claims_around_out_of_band_file() {
    let dir = tempfile::tempdir().unwrap();
    let coll = dir.path().join("c");
    fs::create_dir(&coll).unwrap();
    // Drop a non-churl file at the bare slug path.
    fs::write(
        coll.join("ping.toml"),
        b"# hand-placed, not a churl endpoint\n",
    )
    .unwrap();

    let path = create_endpoint(&coll, "Ping").unwrap();
    assert_eq!(
        path.file_name().unwrap().to_str().unwrap(),
        "ping-2.toml",
        "claim must skip the pre-existing file, not clobber it"
    );
    // The out-of-band file is untouched.
    assert_eq!(
        fs::read_to_string(coll.join("ping.toml")).unwrap(),
        "# hand-placed, not a churl endpoint\n"
    );
}

/// A reserved collection slug (`sequences`) whose bumped target already exists on
/// disk out of band keeps advancing: with both `sequences-2` present, the claim
/// lands on `sequences-3`.
#[test]
fn create_collection_reserved_bump_advances_past_existing() {
    let root = tempfile::tempdir().unwrap();
    // `sequences` is reserved, so a create bumps to `-2`; pre-occupy `-2`.
    fs::create_dir(root.path().join("sequences-2")).unwrap();

    let coll = create_collection(root.path(), "sequences", root.path()).unwrap();
    assert_eq!(
        coll.file_name().unwrap().to_str().unwrap(),
        "sequences-3",
        "reserved bump must advance past an already-claimed `-2`"
    );
}

// ---- A3: widened save-gate coverage (headers / URL / body) + policy ----

/// A GET endpoint at `url` with the given headers/params/body, no auth.
fn endpoint_with(
    url: &str,
    headers: Vec<Header>,
    params: Vec<Param>,
    body: Option<Body>,
) -> Endpoint {
    Endpoint {
        seq: 0,
        name: "e".into(),
        assertions: Vec::new(),
        extract: std::collections::BTreeMap::new(),
        persist: Vec::new(),
        request: Request {
            method: Method::Get,
            url: url.into(),
            headers,
            params,
            body,
            auth: None,
            insecure: false,
        },
    }
}

#[test]
fn new_secret_header_value_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("h.toml");
    let ep = endpoint_with(
        "https://api.example.com/",
        vec![Header {
            name: "Authorization".into(),
            value: "Bearer sk-live-literal".into(),
            enabled: true,
        }],
        Vec::new(),
        None,
    );
    let err = save_endpoint(&path, &ep).unwrap_err();
    assert!(
        matches!(&err, PersistenceError::SecretsRefused { locations }
            if locations == &["headers.Authorization".to_string()]),
        "expected SecretsRefused on the header, got {err:?}"
    );
    assert!(!path.exists());
    // A templated header value (`Bearer {{token}}`) saves cleanly.
    let ep = endpoint_with(
        "https://api.example.com/",
        vec![Header {
            name: "Authorization".into(),
            value: "Bearer {{token}}".into(),
            enabled: true,
        }],
        Vec::new(),
        None,
    );
    save_endpoint(&path, &ep).expect("templated header saves");
}

#[test]
fn new_url_query_key_and_userinfo_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("u.toml");
    let ep = endpoint_with(
        "https://user:s3cr3tpass@api.example.com/x?api_key=abcd1234",
        Vec::new(),
        Vec::new(),
        None,
    );
    let err = save_endpoint(&path, &ep).unwrap_err();
    let PersistenceError::SecretsRefused { locations } = &err else {
        panic!("expected SecretsRefused, got {err:?}");
    };
    assert!(
        locations.contains(&"url.userinfo".to_string()),
        "{locations:?}"
    );
    assert!(
        locations.contains(&"url.query.api_key".to_string()),
        "{locations:?}"
    );
    assert!(!path.exists());
}

#[test]
fn body_secret_shaped_value_warns_not_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.toml");
    let ep = endpoint_with(
        "https://api.example.com/",
        Vec::new(),
        Vec::new(),
        Some(Body {
            kind: BodyKind::Json,
            content: r#"{"token": "ghp_0123456789abcdefABCDEF0123456789abcd"}"#.into(),
        }),
    );
    // Value-only → warns, save proceeds even under strict.
    let decision = save_endpoint_checked(&path, &ep, SecretPolicy::Strict)
        .expect("body value-only finding must not block");
    assert!(path.exists(), "the file is written");
    assert_eq!(decision.warning_locations(), vec!["body".to_string()]);
}

#[test]
fn warn_policy_never_blocks_new_name_anchored_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.toml");
    let ep = endpoint_with(
        "https://api.example.com/",
        vec![Header {
            name: "X-Api-Key".into(),
            value: "literal-value".into(),
            enabled: true,
        }],
        Vec::new(),
        None,
    );
    let decision =
        save_endpoint_checked(&path, &ep, SecretPolicy::Warn).expect("warn policy blocks nothing");
    assert!(path.exists());
    assert!(
        decision
            .warning_locations()
            .contains(&"headers.X-Api-Key".to_string()),
        "{:?}",
        decision.warning_locations()
    );
}

// ---- M7.9: recursive collection tree (workspace = root collection) ----

/// Writes an endpoint TOML into `dir` (created if needed).
fn write_endpoint(dir: &Path, file: &str, seq: u32, name: &str, url: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join(file),
        format!("seq = {seq}\nname = \"{name}\"\n\n[request]\nmethod = \"GET\"\nurl = \"{url}\"\n"),
    )
    .unwrap();
}

/// Back-compat regression: a pre-M7.9-shaped FLAT workspace (collections one
/// deep, no root endpoints) loads IDENTICALLY under the recursive model — the
/// same top-level collections, the same endpoints, and the `churl.toml` `[vars]`
/// are exactly the root collection's vars.
#[test]
fn m79_flat_workspace_loads_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(
        root.join("churl.toml"),
        "name = \"demo\"\n\n[vars]\nbase = \"https://api.example.com\"\n",
    )
    .unwrap();
    write_endpoint(
        &root.join("users"),
        "list.toml",
        0,
        "List",
        "https://x/users",
    );
    write_endpoint(
        &root.join("orders"),
        "list.toml",
        0,
        "List",
        "https://x/orders",
    );

    let ws = OpenWorkspace::open(root).unwrap();

    // Top-level collections unchanged: name-sorted, exactly the two dirs.
    let cols = ws.collections().unwrap();
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["orders", "users"]);

    // Each collection's endpoints unchanged.
    let users = cols.iter().find(|c| c.name == "users").unwrap();
    let eps = users.endpoints().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0].1.name, "List");

    // The root collection IS the workspace root; its vars are the churl.toml vars.
    let rootc = ws.root_collection();
    assert_eq!(rootc.path, root);
    assert_eq!(rootc.name, "demo");
    assert_eq!(
        ws.manifest().vars.get("base").map(String::as_str),
        Some("https://api.example.com")
    );
    // A flat workspace has no root-level endpoints.
    assert!(rootc.endpoints().unwrap().is_empty());
}

/// A root-level endpoint (an endpoint file directly under the workspace root) is
/// enumerated by the root collection's `endpoints()`, and the manifest
/// (`churl.toml`) / `folder.toml` are NOT parsed as endpoints.
#[test]
fn m79_root_endpoint_enumerated_and_manifest_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    fs::write(root.join("folder.toml"), "[vars]\nx = \"1\"\n").unwrap();
    write_endpoint(root, "ping.toml", 0, "Ping", "https://x/ping");

    let ws = OpenWorkspace::open(root).unwrap();
    let rootc = ws.root_collection();
    let eps = rootc.endpoints().unwrap();
    assert_eq!(
        eps.len(),
        1,
        "only ping.toml is an endpoint (not churl/folder)"
    );
    assert_eq!(eps[0].1.name, "Ping");
}

/// Reserved-names per level: `sequences/` is reserved AT THE ROOT ONLY. A
/// `sequences` directory NESTED under a sub-collection is an ordinary
/// sub-collection, not churl's sequence store.
#[test]
fn m79_sequences_reserved_at_root_only() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    fs::create_dir_all(root.join("sequences")).unwrap(); // root: reserved
    let api = root.join("api");
    fs::create_dir_all(api.join("sequences")).unwrap(); // nested: a real collection

    let ws = OpenWorkspace::open(root).unwrap();
    // Root: sequences/ excluded.
    let top = ws.collections().unwrap();
    assert!(top.iter().all(|c| c.name != "sequences"));
    assert!(top.iter().any(|c| c.name == "api"));

    // Nested: api/sequences IS a sub-collection.
    let api_col = top.iter().find(|c| c.name == "api").unwrap();
    let subs = api_col.sub_collections().unwrap();
    assert_eq!(
        subs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["sequences"],
        "a nested `sequences` dir is a normal sub-collection"
    );
}

/// Arbitrary-depth nesting: sub-collections recurse with no cap; each level's
/// endpoints and children resolve independently.
#[test]
fn m79_nested_collections_recurse_arbitrary_depth() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let deep = root.join("a").join("b").join("c");
    write_endpoint(&deep, "leaf.toml", 0, "Leaf", "https://x/leaf");
    write_endpoint(&root.join("a"), "mid.toml", 0, "Mid", "https://x/mid");

    let ws = OpenWorkspace::open(root).unwrap();
    let a = ws
        .collections()
        .unwrap()
        .into_iter()
        .find(|c| c.name == "a")
        .unwrap();
    assert_eq!(a.endpoints().unwrap().len(), 1); // mid.toml
    let b = a.sub_collections().unwrap().into_iter().next().unwrap();
    assert_eq!(b.name, "b");
    let c = b.sub_collections().unwrap().into_iter().next().unwrap();
    assert_eq!(c.name, "c");
    let leaf = c.endpoints().unwrap();
    assert_eq!(leaf.len(), 1);
    assert_eq!(leaf[0].1.name, "Leaf");
    assert!(c.sub_collections().unwrap().is_empty());
}

/// Per-level `folder.toml`/`churl.toml` skip: a sub-collection's own `folder.toml`
/// (and a stray nested `churl.toml`) are never listed as endpoints.
#[test]
fn m79_reserved_files_skipped_at_every_level() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let sub = root.join("api");
    write_endpoint(&sub, "get.toml", 0, "Get", "https://x/get");
    fs::write(sub.join("folder.toml"), "[vars]\nk = \"v\"\n").unwrap();
    fs::write(sub.join("churl.toml"), "name = \"nested\"\n").unwrap();

    let ws = OpenWorkspace::open(root).unwrap();
    let api = ws
        .collections()
        .unwrap()
        .into_iter()
        .find(|c| c.name == "api")
        .unwrap();
    let eps = api.endpoints().unwrap();
    assert_eq!(
        eps.len(),
        1,
        "only get.toml — folder/churl skipped at this level"
    );
    assert_eq!(eps[0].1.name, "Get");
    // The sub-collection's own folder.toml vars still load.
    assert_eq!(
        load_collection_meta(&sub)
            .unwrap()
            .vars
            .get("k")
            .map(String::as_str),
        Some("v")
    );
}
