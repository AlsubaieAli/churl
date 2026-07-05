//! Integration tests for `churl-core` persistence: comment preservation, workspace
//! manifest round-trips with secrets enforcement, and lazy collection loading.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use churl_core::model::{
    ApiKeyPlacement, Auth, Body, BodyKind, Endpoint, Header, Method, Param, Profile, Request,
    Workspace,
};
use churl_core::persistence::endpoint_to_toml;
use churl_core::persistence::{
    Collection, OpenWorkspace, PersistenceError, load_endpoint, load_workspace_manifest,
    save_endpoint, save_workspace_manifest,
};

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
fn headers_render_as_array_of_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.toml");
    let endpoint = Endpoint {
        seq: 3,
        name: "fresh".into(),
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
        request: Request {
            method: Method::Get,
            url: "https://api.example.com/x".into(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: Some(Auth::Bearer {
                token: "ghp_definitely_a_literal".into(),
            }),
        },
    }
}

#[test]
fn save_endpoint_refuses_literal_secret_auth() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leaky.toml");
    let err = save_endpoint(&path, &literal_secret_endpoint()).unwrap_err();
    match err {
        PersistenceError::SecretsInAuth { names } => {
            assert_eq!(names, vec!["auth.token".to_string()]);
        }
        other => panic!("expected SecretsInAuth, got {other:?}"),
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
    }
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
    let err = save_workspace_manifest(dir.path(), &ws).unwrap_err();
    match err {
        PersistenceError::SecretsInManifest { names } => {
            assert_eq!(names, vec!["prod.api_token".to_string()]);
        }
        other => panic!("expected SecretsInManifest, got {other:?}"),
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
