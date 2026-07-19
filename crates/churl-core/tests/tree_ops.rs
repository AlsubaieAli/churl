//! Integration tests for the M7.12 tree CRUD surface: move / copy / duplicate of
//! endpoints and collection subtrees, group-internal reorder (with legacy
//! default-`0` seq normalization + back-compat load order), and the sequence-step
//! reference-rewrite helper on rename and move.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use churl_core::model::{Method, Request, Sequence, SequenceStep, Workspace};
use churl_core::persistence::{
    self, Collection, OpenWorkspace, PersistenceError, ReorderDir, ReorderOutcome, copy_collection,
    copy_endpoint, create_collection, create_endpoint, create_sequence, duplicate_collection,
    duplicate_endpoint, load_collection_meta, load_endpoint, load_sequence, move_collection,
    move_endpoint, rename_endpoint, reorder_collection, reorder_endpoint, retarget_sequence_steps,
    save_collection_meta, save_endpoint, save_sequence,
};

/// A fresh workspace root with a `churl.toml` manifest.
fn workspace(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    persistence::save_workspace_manifest(
        dir.path(),
        &Workspace {
            name: name.to_owned(),
            vars: BTreeMap::new(),
            profiles: Vec::new(),
            ..Default::default()
        },
    )
    .unwrap();
    dir
}

/// Writes a sequence file with the given steps (by workspace-relative endpoint
/// path) directly under `sequences/`.
fn write_sequence(root: &Path, slug: &str, steps: &[&str]) -> PathBuf {
    let dir = root.join("sequences");
    fs::create_dir_all(&dir).unwrap();
    let sequence = Sequence {
        seq: 0,
        name: slug.to_owned(),
        on_error: churl_core::model::OnError::Halt,
        steps: steps
            .iter()
            .enumerate()
            .map(|(i, ep)| SequenceStep {
                seq: i as u32,
                endpoint: (*ep).to_owned(),
                extract: BTreeMap::new(),
                persist: Vec::new(),
            })
            .collect(),
    };
    let path = dir.join(format!("{slug}.toml"));
    save_sequence(&path, &sequence).unwrap();
    path
}

/// The endpoint names in a collection, in load (sorted) order.
fn endpoint_names(dir: &Path) -> Vec<String> {
    Collection {
        name: dir.file_name().unwrap().to_string_lossy().into_owned(),
        path: dir.to_owned(),
    }
    .endpoints()
    .unwrap()
    .into_iter()
    .map(|(_, ep)| ep.name)
    .collect()
}

// --- move / copy / duplicate: endpoints ---

#[test]
fn move_endpoint_relocates_and_appends_seq() {
    let ws = workspace("w");
    let root = ws.path();
    let src = create_collection(root, "auth", root).unwrap();
    let dst = create_collection(root, "users", root).unwrap();
    // Seed the destination so the moved endpoint must APPEND (higher seq).
    create_endpoint(&dst, "existing").unwrap();
    let login = create_endpoint(&src, "login").unwrap();

    let new_path = move_endpoint(&login, &dst).unwrap();
    assert!(!login.exists(), "source endpoint removed");
    assert!(new_path.starts_with(&dst));
    // Appended last in the destination.
    assert_eq!(endpoint_names(&dst), vec!["existing", "login"]);
    assert!(endpoint_names(&src).is_empty());
    let moved = load_endpoint(&new_path).unwrap();
    assert!(moved.seq >= 1, "moved endpoint appended at destination");
}

#[test]
fn copy_endpoint_leaves_original_and_suffixes_on_collision() {
    let ws = workspace("w");
    let root = ws.path();
    let coll = create_collection(root, "auth", root).unwrap();
    let login = create_endpoint(&coll, "login").unwrap();

    // Copy into the SAME collection → filename must suffix, original stays.
    let copy = copy_endpoint(&login, &coll).unwrap();
    assert!(login.exists(), "original endpoint untouched by copy");
    assert_ne!(copy, login);
    assert_eq!(endpoint_names(&coll), vec!["login", "login"]);
}

#[test]
fn duplicate_endpoint_makes_a_suffixed_sibling() {
    let ws = workspace("w");
    let root = ws.path();
    let coll = create_collection(root, "auth", root).unwrap();
    let login = create_endpoint(&coll, "login").unwrap();
    let dup = duplicate_endpoint(&login).unwrap();
    assert!(login.exists());
    assert!(dup.exists());
    assert_eq!(dup.parent(), login.parent());
    assert_ne!(dup, login);
}

#[test]
fn move_endpoint_into_own_collection_is_a_noop() {
    let ws = workspace("w");
    let root = ws.path();
    let coll = create_collection(root, "auth", root).unwrap();
    let login = create_endpoint(&coll, "login").unwrap();
    let same = move_endpoint(&login, &coll).unwrap();
    assert_eq!(same, login);
    assert_eq!(endpoint_names(&coll), vec!["login"]);
}

// --- move / copy / duplicate: collections ---

#[test]
fn copy_collection_recurses_and_appends() {
    let ws = workspace("w");
    let root = ws.path();
    let src = create_collection(root, "auth", root).unwrap();
    create_endpoint(&src, "login").unwrap();
    let sub = create_collection(&src, "nested", root).unwrap();
    create_endpoint(&sub, "deep").unwrap();
    let dst = create_collection(root, "target", root).unwrap();

    let new_dir = copy_collection(&src, &dst, root).unwrap();
    assert!(src.exists(), "copy leaves the original");
    assert!(new_dir.starts_with(&dst));
    assert_eq!(endpoint_names(&new_dir), vec!["login"]);
    assert!(
        new_dir.join("nested").is_dir(),
        "subtree copied recursively"
    );
    assert_eq!(endpoint_names(&new_dir.join("nested")), vec!["deep"]);
}

#[test]
fn duplicate_collection_suffixes_the_directory() {
    let ws = workspace("w");
    let root = ws.path();
    let src = create_collection(root, "auth", root).unwrap();
    create_endpoint(&src, "login").unwrap();
    let dup = duplicate_collection(&src, root).unwrap();
    assert!(src.exists());
    assert_ne!(dup, src);
    assert_eq!(dup.parent(), src.parent());
    assert_eq!(endpoint_names(&dup), vec!["login"]);
}

#[test]
fn move_collection_into_itself_is_rejected() {
    let ws = workspace("w");
    let root = ws.path();
    let src = create_collection(root, "auth", root).unwrap();
    let sub = create_collection(&src, "nested", root).unwrap();
    let err = move_collection(&src, &sub, root).unwrap_err();
    assert!(
        matches!(err, PersistenceError::InvalidDestination { .. }),
        "moving a collection into its own descendant must be rejected, got {err:?}"
    );
    assert!(src.exists(), "rejected move leaves the source in place");
}

// --- reorder: seq-tie normalization + edges ---

#[test]
fn reorder_endpoints_normalizes_legacy_zero_seq() {
    let ws = workspace("w");
    let root = ws.path();
    let coll = create_collection(root, "c", root).unwrap();
    // Three endpoints that all default to seq 0 (legacy / hand-written): sorted by
    // filename → a, b, c.
    for name in ["a", "b", "c"] {
        let p = coll.join(format!("{name}.toml"));
        let ep = churl_core::model::Endpoint {
            seq: 0,
            name: name.to_owned(),
            assertions: Vec::new(),
            extract: std::collections::BTreeMap::new(),
            persist: Vec::new(),
            request: Request {
                method: Method::Get,
                url: String::new(),
                headers: Vec::new(),
                params: Vec::new(),
                body: None,
                auth: None,
                insecure: false,
            },
        };
        save_endpoint(&p, &ep).unwrap();
    }
    assert_eq!(endpoint_names(&coll), vec!["a", "b", "c"]);

    // Move `c` up once → b, then again → a. Ties are dense-renumbered first so the
    // swap actually reorders (a bare swap of two 0s would be a no-op).
    let c = coll.join("c.toml");
    assert_eq!(
        reorder_endpoint(&coll, &c, ReorderDir::Up).unwrap(),
        ReorderOutcome::Moved
    );
    assert_eq!(endpoint_names(&coll), vec!["a", "c", "b"]);
    reorder_endpoint(&coll, &c, ReorderDir::Up).unwrap();
    assert_eq!(endpoint_names(&coll), vec!["c", "a", "b"]);
}

#[test]
fn reorder_reports_edges() {
    let ws = workspace("w");
    let root = ws.path();
    let coll = create_collection(root, "c", root).unwrap();
    let a = create_endpoint(&coll, "a").unwrap();
    create_endpoint(&coll, "b").unwrap();
    assert_eq!(
        reorder_endpoint(&coll, &a, ReorderDir::Up).unwrap(),
        ReorderOutcome::AlreadyFirst
    );
    // `a` at the top; moving it down works, then it's last.
    reorder_endpoint(&coll, &a, ReorderDir::Down).unwrap();
    assert_eq!(
        reorder_endpoint(&coll, &a, ReorderDir::Down).unwrap(),
        ReorderOutcome::AlreadyLast
    );
}

#[test]
fn reorder_collections_via_seq() {
    let ws = workspace("w");
    let root = ws.path();
    let a = create_collection(root, "a", root).unwrap();
    create_collection(root, "b", root).unwrap();
    let c = create_collection(root, "c", root).unwrap();
    let names = |ws: &OpenWorkspace| {
        ws.collections()
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect::<Vec<_>>()
    };
    let open = OpenWorkspace::open(root).unwrap();
    assert_eq!(names(&open), vec!["a", "b", "c"]);

    reorder_collection(&c, root, ReorderDir::Up).unwrap();
    assert_eq!(
        names(&OpenWorkspace::open(root).unwrap()),
        vec!["a", "c", "b"]
    );
    // Reordering never disturbs the reserved sequences dir or re-parents.
    reorder_collection(&a, root, ReorderDir::Down).unwrap();
    assert_eq!(
        names(&OpenWorkspace::open(root).unwrap()),
        vec!["c", "a", "b"]
    );
}

// --- back-compat: a pre-M7.12 workspace loads byte-identical ---

#[test]
fn pre_m712_workspace_keeps_alphabetical_order() {
    let ws = workspace("w");
    let root = ws.path();
    // Collections with NO folder.toml (or an all-`seq = 0` one) — exactly a
    // pre-M7.12 corpus. Loader must fall back to alphabetical, byte-identically.
    for name in ["zebra", "alpha", "mango"] {
        create_collection(root, name, root).unwrap();
    }
    // A var-only folder.toml with no seq is also legacy-shaped.
    save_collection_meta(
        &root.join("mango"),
        &churl_core::model::CollectionMeta {
            seq: 0,
            vars: BTreeMap::from([("base".into(), "/v1".into())]),
        },
    )
    .unwrap();
    let open = OpenWorkspace::open(root).unwrap();
    let names: Vec<String> = open
        .collections()
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["alpha", "mango", "zebra"]);
    // The legacy folder.toml never gained a `seq` line.
    let text = fs::read_to_string(root.join("mango/folder.toml")).unwrap();
    assert!(!text.contains("seq"), "no seq materialized: {text:?}");
    assert_eq!(load_collection_meta(&root.join("mango")).unwrap().seq, 0);
}

// --- reference rewrite: rename + move, endpoint + collection prefix ---

#[test]
fn rename_endpoint_rewrites_referencing_steps() {
    let ws = workspace("w");
    let root = ws.path();
    let coll = create_collection(root, "auth", root).unwrap();
    let login = create_endpoint(&coll, "login").unwrap();
    let seq = write_sequence(root, "flow", &["auth/login.toml", "auth/other.toml"]);

    let new_path = rename_endpoint(&login, "signin").unwrap();
    let new_rel = new_path.strip_prefix(root).unwrap();
    let rewritten = retarget_sequence_steps(root, Path::new("auth/login.toml"), new_rel).unwrap();
    assert_eq!(rewritten, 1);
    let loaded = load_sequence(&seq).unwrap();
    assert_eq!(loaded.steps[0].endpoint, "auth/signin.toml");
    assert_eq!(
        loaded.steps[1].endpoint, "auth/other.toml",
        "unrelated step untouched"
    );
}

#[test]
fn move_endpoint_rewrites_referencing_steps() {
    let ws = workspace("w");
    let root = ws.path();
    let src = create_collection(root, "auth", root).unwrap();
    let dst = create_collection(root, "users", root).unwrap();
    let login = create_endpoint(&src, "login").unwrap();
    let seq = write_sequence(root, "flow", &["auth/login.toml"]);

    let new_path = move_endpoint(&login, &dst).unwrap();
    let new_rel = new_path.strip_prefix(root).unwrap();
    let n = retarget_sequence_steps(root, Path::new("auth/login.toml"), new_rel).unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        load_sequence(&seq).unwrap().steps[0].endpoint,
        "users/login.toml"
    );
}

#[test]
fn move_collection_rewrites_steps_under_prefix() {
    let ws = workspace("w");
    let root = ws.path();
    let src = create_collection(root, "auth", root).unwrap();
    create_endpoint(&src, "login").unwrap();
    create_endpoint(&src, "logout").unwrap();
    let dst = create_collection(root, "archive", root).unwrap();
    let seq = write_sequence(
        root,
        "flow",
        &["auth/login.toml", "auth/logout.toml", "other/keep.toml"],
    );

    let new_dir = move_collection(&src, &dst, root).unwrap();
    let new_rel = new_dir.strip_prefix(root).unwrap();
    let n = retarget_sequence_steps(root, Path::new("auth"), new_rel).unwrap();
    assert_eq!(
        n, 2,
        "both steps under the moved collection prefix rewritten"
    );
    let loaded = load_sequence(&seq).unwrap();
    assert_eq!(loaded.steps[0].endpoint, "archive/auth/login.toml");
    assert_eq!(loaded.steps[1].endpoint, "archive/auth/logout.toml");
    assert_eq!(
        loaded.steps[2].endpoint, "other/keep.toml",
        "unrelated step untouched"
    );
}

#[test]
fn copy_never_rewrites_references() {
    let ws = workspace("w");
    let root = ws.path();
    let src = create_collection(root, "auth", root).unwrap();
    let login = create_endpoint(&src, "login").unwrap();
    let dst = create_collection(root, "users", root).unwrap();
    let seq = write_sequence(root, "flow", &["auth/login.toml"]);
    copy_endpoint(&login, &dst).unwrap();
    // No retarget call for a copy — the original stays referenced.
    assert_eq!(
        load_sequence(&seq).unwrap().steps[0].endpoint,
        "auth/login.toml"
    );
    // And a defensive retarget with the copy's path finds nothing to rewrite.
    assert_eq!(
        retarget_sequence_steps(root, Path::new("nope.toml"), Path::new("x.toml")).unwrap(),
        0
    );
}

// --- reference rewrite must not false-match a name prefix ---

#[test]
fn retarget_does_not_match_sibling_name_prefix() {
    let ws = workspace("w");
    let root = ws.path();
    create_collection(root, "auth", root).unwrap();
    let seq = write_sequence(root, "flow", &["authz/login.toml"]);
    // Moving `auth` must NOT touch a step under `authz` (component-wise prefix).
    let n = retarget_sequence_steps(root, Path::new("auth"), Path::new("archive/auth")).unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        load_sequence(&seq).unwrap().steps[0].endpoint,
        "authz/login.toml"
    );
}

// --- sequence duplicate + reorder ---

#[test]
fn duplicate_and_reorder_sequences() {
    let ws = workspace("w");
    let root = ws.path();
    let a = create_sequence(root, "a").unwrap();
    let b = create_sequence(root, "b").unwrap();
    let dup = persistence::duplicate_sequence(&a).unwrap();
    assert!(a.exists() && dup.exists());
    assert_ne!(dup, a);

    let seq_dir = root.join("sequences");
    // Reorder `b` to the top among sequences.
    persistence::reorder_sequence(&seq_dir, &b, ReorderDir::Up).unwrap();
    let order: Vec<String> = OpenWorkspace::open(root)
        .unwrap()
        .sequences()
        .unwrap()
        .sequences
        .into_iter()
        .map(|(_, s)| s.name)
        .collect();
    assert_eq!(order.first().map(String::as_str), Some("b"));
}

// --- interchange: native collection seq round-trips ---

#[test]
fn native_export_round_trips_collection_seq() {
    let ws = workspace("src");
    let root = ws.path();
    // Two collections with an explicit, non-alphabetical order (b before a).
    let a = create_collection(root, "a", root).unwrap();
    create_endpoint(&a, "ea").unwrap();
    let b = create_collection(root, "b", root).unwrap();
    create_endpoint(&b, "eb").unwrap();
    save_collection_meta(
        &b,
        &churl_core::model::CollectionMeta {
            seq: 1,
            vars: BTreeMap::new(),
        },
    )
    .unwrap();
    // Give `a` a higher seq so `b` (seq 1) sorts first.
    save_collection_meta(
        &a,
        &churl_core::model::CollectionMeta {
            seq: 2,
            vars: BTreeMap::new(),
        },
    )
    .unwrap();

    let open = OpenWorkspace::open(root).unwrap();
    assert_eq!(
        open.collections()
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect::<Vec<_>>(),
        vec!["b", "a"],
        "explicit seq orders b before a"
    );
    let json = churl_core::interchange::export_workspace(
        &open,
        churl_core::interchange::JsonDialect::Native,
    )
    .unwrap();
    assert!(
        json.contains("\"seq\""),
        "native export carries the collection seq"
    );

    // Import into a fresh workspace and confirm the order round-trips.
    let dst = tempfile::tempdir().unwrap();
    let import = churl_core::interchange::import_json(&json).unwrap();
    churl_core::interchange::write_import(dst.path(), &import).unwrap();
    let reopened = OpenWorkspace::open(dst.path()).unwrap();
    assert_eq!(
        reopened
            .collections()
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect::<Vec<_>>(),
        vec!["b", "a"],
        "collection order survives a native export/import round-trip"
    );
}

// --- failure paths: fail-loud + no orphaned partials ---

/// A failed step-rewrite surfaces the error (never a silent `Ok(0)`), so a rename
/// that can't repoint its sequences is reported as a failure, not a clean rename.
#[cfg(unix)]
#[test]
fn retarget_errors_when_sequence_save_fails() {
    use std::os::unix::fs::PermissionsExt;
    let ws = workspace("w");
    let root = ws.path();
    create_collection(root, "auth", root).unwrap();
    write_sequence(root, "flow", &["auth/login.toml"]);
    let seq_dir = root.join("sequences");
    // Read+exec but not writable: the rewrite reads flow.toml fine, but its
    // format-preserving save (temp file in the dir) fails.
    std::fs::set_permissions(&seq_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = retarget_sequence_steps(
        root,
        Path::new("auth/login.toml"),
        Path::new("auth/signin.toml"),
    );
    std::fs::set_permissions(&seq_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        result.is_err(),
        "a failed step rewrite must surface an error, not Ok(0)"
    );
}

/// A mid-copy failure leaves no partially-populated `-N` directory behind (the
/// claim-cleanup discipline the module's header promises for collections too).
#[cfg(unix)]
#[test]
fn copy_collection_cleans_up_partial_dir_on_failure() {
    use std::os::unix::fs::PermissionsExt;
    let ws = workspace("w");
    let root = ws.path();
    let src = create_collection(root, "auth", root).unwrap();
    create_endpoint(&src, "login").unwrap();
    let sub = create_collection(&src, "nested", root).unwrap();
    create_endpoint(&sub, "deep").unwrap();
    let dst = create_collection(root, "target", root).unwrap();
    // Make the nested source dir unreadable so the recursive copy fails partway
    // (after the top-level file + the empty nested dir have been created at dest).
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = copy_collection(&src, &dst, root);
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err(), "mid-copy failure surfaces an error");
    let leftovers: Vec<_> = std::fs::read_dir(&dst)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the partial copy dir was cleaned up: {leftovers:?}"
    );
}
