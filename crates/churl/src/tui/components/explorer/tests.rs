use super::*;
use std::path::Path;

/// Builds a fixture workspace: two collections, three + one endpoints.
fn fixture(root: &Path) -> OpenWorkspace {
    std::fs::write(root.join("churl.toml"), "name = \"demo\"\n").unwrap();
    let users = root.join("users");
    std::fs::create_dir(&users).unwrap();
    for (i, (file, name, method, url)) in [
        ("list.toml", "List users", "GET", "https://api.test/users"),
        ("get.toml", "Get user", "GET", "https://api.test/users/1"),
        (
            "create.toml",
            "Create user",
            "POST",
            "https://api.test/users",
        ),
    ]
    .iter()
    .enumerate()
    {
        std::fs::write(
                users.join(file),
                format!(
                    "seq = {i}\nname = \"{name}\"\n\n[request]\nmethod = \"{method}\"\nurl = \"{url}\"\n"
                ),
            )
            .unwrap();
    }
    let orders = root.join("orders");
    std::fs::create_dir(&orders).unwrap();
    std::fs::write(
            orders.join("list.toml"),
            "seq = 0\nname = \"List orders\"\n\n[request]\nmethod = \"GET\"\nurl = \"https://api.test/orders\"\n",
        )
        .unwrap();
    OpenWorkspace::open(root).unwrap()
}

fn explorer(root: &Path) -> ExplorerState {
    let ws = fixture(root);
    ExplorerState::new(Some(&ws)).unwrap()
}

#[test]
fn collapsed_tree_lists_collections_only() {
    let dir = tempfile::tempdir().unwrap();
    let state = explorer(dir.path());
    let rows = state.rows();
    assert_eq!(rows.len(), 2);
    // Collections sort by name: orders before users.
    assert_eq!(rows[0].name, "orders");
    assert_eq!(rows[1].name, "users");
    assert!(rows.iter().all(|r| r.kind == RowKind::Collection));
}

#[test]
fn expand_flattens_children_in_seq_order() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer(dir.path());
    state.move_down(); // onto "users"
    state.select().unwrap(); // expand
    let rows = state.rows();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[2].name, "List users");
    assert_eq!(rows[3].name, "Get user");
    assert_eq!(rows[4].name, "Create user");
    assert_eq!(rows[2].depth, 1);
}

#[test]
fn cursor_clamps_at_edges() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer(dir.path());
    state.move_up();
    assert_eq!(state.cursor, 0);
    for _ in 0..10 {
        state.move_down();
    }
    assert_eq!(state.cursor, 1); // last visible row while collapsed
    state.top();
    assert_eq!(state.cursor, 0);
    state.select().unwrap(); // expand "orders" (1 endpoint)
    state.bottom();
    assert_eq!(state.cursor, 2);
}

#[test]
fn expand_collapse_toggles_and_reclamps_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer(dir.path());
    state.move_down(); // "users"
    state.expand().unwrap();
    assert_eq!(state.rows().len(), 5);
    state.bottom(); // last endpoint of users
    // Collapse via h on an endpoint first jumps to the parent...
    state.collapse();
    assert_eq!(state.cursor, 1);
    assert_eq!(state.rows()[state.cursor].name, "users");
    // ...then collapses the collection.
    state.collapse();
    assert_eq!(state.rows().len(), 2);
    assert_eq!(state.cursor, 1);
}

#[test]
fn expand_on_expanded_collection_descends() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer(dir.path());
    state.expand().unwrap(); // expand "orders"
    assert_eq!(state.cursor, 0);
    state.expand().unwrap(); // descend
    assert_eq!(state.rows()[state.cursor].name, "List orders");
}

#[test]
fn select_endpoint_returns_loaded_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer(dir.path());
    state.move_down();
    state.select().unwrap(); // expand users
    state.move_down(); // "List users"
    state.move_down(); // "Get user"
    let selected = state.select().unwrap().expect("endpoint row");
    assert_eq!(selected.display_path, "users/Get user");
    assert_eq!(selected.endpoint.request.url, "https://api.test/users/1");
}

#[test]
fn all_endpoints_loads_lazily_for_search() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer(dir.path());
    let all = state.all_endpoints().unwrap();
    let paths: Vec<&str> = all.iter().map(|(p, _, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            "orders/List orders",
            "users/List users",
            "users/Get user",
            "users/Create user",
        ]
    );
}

#[test]
fn jump_to_expands_and_selects() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer(dir.path());
    let all = state.all_endpoints().unwrap();
    let (_, ci, ei) = all
        .iter()
        .find(|(p, _, _)| p == "users/Create user")
        .unwrap();
    let selected = state.jump_to(*ci, *ei).unwrap().expect("jump target");
    assert_eq!(selected.display_path, "users/Create user");
    assert_eq!(state.rows()[state.cursor].name, "Create user");
}

#[test]
fn scroll_offset_keeps_selection_in_viewport() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer(dir.path());
    state.move_down(); // onto "users"
    state.select().unwrap(); // expand → 5 rows total
    assert_eq!(state.rows().len(), 5);

    // Cursor at the bottom, viewport only 2 rows tall: scroll must follow so
    // the selected row stays visible.
    state.bottom();
    assert_eq!(state.cursor, 4);
    let offset = state.scroll_to_fit(2);
    assert_eq!(offset, 3, "bottom row must be the last visible one");
    assert!(
        state.cursor >= offset && state.cursor < offset + 2,
        "cursor {} must be within [{offset}, {})",
        state.cursor,
        offset + 2
    );

    // Jumping back to the top pulls the offset back to zero.
    state.top();
    assert_eq!(state.scroll_to_fit(2), 0);

    // A viewport taller than the tree never scrolls.
    state.bottom();
    assert_eq!(state.scroll_to_fit(50), 0);
}

#[test]
fn empty_workspace_has_no_rows() {
    let mut state = ExplorerState::new(None).unwrap();
    assert!(state.rows().is_empty());
    state.move_down();
    state.bottom();
    assert_eq!(state.cursor, 0);
    assert!(state.select().unwrap().is_none());
}

/// A fixture with two named sequences (in `seq` order) plus the endpoint
/// collections, for the PR-2b sub-pane tests.
fn explorer_with_sequences(root: &Path) -> ExplorerState {
    let _ws = fixture(root);
    let seq_dir = root.join("sequences");
    std::fs::create_dir(&seq_dir).unwrap();
    for (i, name) in ["Login flow", "Checkout"].iter().enumerate() {
        std::fs::write(
            seq_dir.join(format!("seq{i}.toml")),
            format!("seq = {i}\nname = \"{name}\"\non_error = \"halt\"\n"),
        )
        .unwrap();
    }
    let ws = OpenWorkspace::open(root).unwrap();
    ExplorerState::new(Some(&ws)).unwrap()
}

#[test]
fn rows_never_contain_a_sequence_even_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer_with_sequences(dir.path());
    assert_eq!(state.sequences_len(), 2, "sequences load into the sub-pane");
    // Expand every collection so every possible row is materialized.
    for ci in 0..state.collections.len() {
        state.collections[ci].load().unwrap();
        state.expanded.insert(ci);
    }
    assert!(
        state
            .rows()
            .iter()
            .all(|r| matches!(r.kind, RowKind::Collection | RowKind::Endpoint)),
        "the tree lists only collections/endpoints — sequences moved to the sub-pane"
    );
}

#[test]
fn selected_sequence_reads_seq_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = explorer_with_sequences(dir.path());
    // Sequences sort by `seq`: "Login flow" (0), "Checkout" (1).
    assert_eq!(state.seq_cursor(), 0);
    assert_eq!(state.selected_sequence().unwrap().name, "Login flow");
    state.seq_move_down();
    assert_eq!(state.selected_sequence().unwrap().name, "Checkout");
    state.seq_move_down(); // clamped at the last sequence
    assert_eq!(state.seq_cursor(), 1);
    state.seq_top();
    assert_eq!(state.selected_sequence().unwrap().name, "Login flow");
    state.seq_bottom();
    assert_eq!(state.selected_sequence().unwrap().name, "Checkout");
}

#[test]
fn all_sequences_still_lists_every_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let state = explorer_with_sequences(dir.path());
    let names: Vec<String> = state.all_sequences().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["Login flow", "Checkout"]);
}

#[test]
fn no_sequences_means_none_selected() {
    let dir = tempfile::tempdir().unwrap();
    let state = explorer(dir.path());
    assert_eq!(state.sequences_len(), 0);
    assert!(state.selected_sequence().is_none());
}
