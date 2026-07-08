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
    delete_collection, delete_endpoint, load_collection_meta, load_endpoint,
    load_workspace_manifest, rename_collection, rename_endpoint, save_collection_meta,
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
    };
    let err = save_workspace_manifest(dir.path(), &ws).unwrap_err();
    assert!(
        matches!(err, PersistenceError::SecretsInManifest { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("vars.api_token"), "{err}");
    // Nothing was written.
    assert!(!dir.path().join("churl.toml").exists());
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
    };
    let err = save_collection_meta(dir.path(), &meta).unwrap_err();
    assert!(
        matches!(err, PersistenceError::SecretsInCollection { .. }),
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
fn rename_endpoint_refuses_literal_secret_and_leaves_file() {
    // A hand-written file may carry a literal secret; renaming it saves (which
    // runs the secrets gate) and must fail before any move.
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
    let err = rename_endpoint(&path, "New Name").unwrap_err();
    assert!(
        matches!(err, PersistenceError::SecretsInAuth { .. }),
        "{err}"
    );
    assert!(path.exists(), "original file must survive a refused rename");
    assert!(!coll.join("new-name.toml").exists());
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
fn create_collection_makes_dir_without_folder_toml() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "My Orders").unwrap();
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
    create_collection(dir.path(), "orders").unwrap();
    let err = create_collection(dir.path(), "orders").unwrap_err();
    assert!(
        matches!(err, PersistenceError::AlreadyExists { .. }),
        "{err}"
    );
}

#[test]
fn rename_collection_moves_dir_with_contents() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "old").unwrap();
    let ep = create_endpoint(&coll, "Ping").unwrap();
    assert!(ep.exists());

    let new_coll = rename_collection(&coll, "New Coll").unwrap();
    assert_eq!(new_coll.file_name().unwrap().to_str().unwrap(), "new-coll");
    assert!(!coll.exists());
    assert!(new_coll.join("ping.toml").exists(), "contents moved along");
}

#[test]
fn delete_collection_removes_dir_recursively() {
    let dir = tempfile::tempdir().unwrap();
    let coll = create_collection(dir.path(), "doomed").unwrap();
    create_endpoint(&coll, "Ping").unwrap();
    delete_collection(&coll).unwrap();
    assert!(!coll.exists());
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
