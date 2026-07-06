//! Explorer pane: workspace tree navigation (collection → endpoint).
//!
//! Collections are listed at startup (a cheap directory scan) but endpoint files
//! are parsed lazily on first expand, keeping the cold-start budget intact.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use churl_core::model::Endpoint;
use churl_core::persistence::{Collection, OpenWorkspace, PersistenceError, load_collection_meta};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use super::jump::JumpState;
use crate::tui::theme::Theme;

/// What kind of row a flattened explorer row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A collection directory (container row).
    Collection,
    /// An endpoint file (leaf row).
    Endpoint,
}

/// One visible row of the flattened explorer tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Nesting depth (collections 0, endpoints 1).
    pub depth: usize,
    /// Container or leaf.
    pub kind: RowKind,
    /// Display name.
    pub name: String,
    /// Whether a container row is currently expanded (always `false` for leaves).
    pub expanded: bool,
    /// Index of the owning collection in the explorer's collection list.
    pub collection: usize,
    /// Index of the endpoint within its collection, for leaf rows.
    pub endpoint: Option<usize>,
}

/// An endpoint the user selected in the explorer, ready to load into the
/// request pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedEndpoint {
    /// `collection/endpoint name` display path.
    pub display_path: String,
    /// Path of the endpoint's TOML file.
    pub file: PathBuf,
    /// Index of the owning collection (for looking up its `folder.toml` vars).
    pub collection: usize,
    /// The parsed endpoint.
    pub endpoint: Endpoint,
}

/// A collection plus its lazily loaded endpoints (`None` until first expand) and
/// its parsed `folder.toml` variables (`None` until first requested).
#[derive(Debug)]
struct CollectionNode {
    collection: Collection,
    endpoints: Option<Vec<(PathBuf, Endpoint)>>,
    /// Cached collection-level template vars from `folder.toml`; loaded lazily.
    vars: Option<BTreeMap<String, String>>,
}

impl CollectionNode {
    /// Parses the collection's endpoint files if not already loaded.
    fn load(&mut self) -> Result<&[(PathBuf, Endpoint)], PersistenceError> {
        if self.endpoints.is_none() {
            self.endpoints = Some(self.collection.endpoints()?);
        }
        Ok(self.endpoints.as_deref().expect("just loaded"))
    }

    /// Parses (and caches) the collection's `folder.toml` `[vars]`. A missing
    /// file yields an empty map. Parse failures are swallowed to an empty map —
    /// var resolution must never break sending (unresolved `{{var}}`s stay
    /// verbatim); a malformed `folder.toml` surfaces when the collection is
    /// expanded and its endpoints are parsed.
    fn vars(&mut self) -> &BTreeMap<String, String> {
        if self.vars.is_none() {
            let vars = load_collection_meta(&self.collection.path)
                .map(|meta| meta.vars)
                .unwrap_or_default();
            self.vars = Some(vars);
        }
        self.vars.as_ref().expect("just loaded")
    }
}

/// Navigation state of the explorer pane.
#[derive(Debug)]
pub struct ExplorerState {
    collections: Vec<CollectionNode>,
    expanded: HashSet<usize>,
    /// Cursor position as an index into [`ExplorerState::rows`].
    pub cursor: usize,
    /// Index of the first visible row; kept so the cursor stays in the viewport
    /// even when the tree is taller than the pane (adjusted by [`scroll_to_fit`]).
    ///
    /// [`scroll_to_fit`]: ExplorerState::scroll_to_fit
    scroll: usize,
}

impl ExplorerState {
    /// Builds the explorer for a workspace (or an empty one when `None`).
    /// Lists collection directories only; no endpoint file is parsed.
    pub fn new(workspace: Option<&OpenWorkspace>) -> Result<Self, PersistenceError> {
        let collections = match workspace {
            Some(ws) => ws
                .collections()?
                .into_iter()
                .map(|collection| CollectionNode {
                    collection,
                    endpoints: None,
                    vars: None,
                })
                .collect(),
            None => Vec::new(),
        };
        Ok(Self {
            collections,
            expanded: HashSet::new(),
            cursor: 0,
            scroll: 0,
        })
    }

    /// Adjusts the scroll offset so the cursor stays within a `height`-row
    /// viewport, clamped so we never scroll past the last screenful, and returns
    /// the offset. Called by [`render`] with the pane's inner height.
    /// Index of the first visible row (the scroll offset as of the last
    /// [`scroll_to_fit`] call). Jump-mode uses it to start labelling at the
    /// viewport instead of the top of the tree.
    ///
    /// [`scroll_to_fit`]: ExplorerState::scroll_to_fit
    pub fn first_visible(&self) -> usize {
        self.scroll
    }

    pub fn scroll_to_fit(&mut self, height: usize) -> usize {
        if height == 0 {
            self.scroll = 0;
            return 0;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
        let max_scroll = self.rows().len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
        self.scroll
    }

    /// Flattens the tree into the currently visible rows.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (ci, node) in self.collections.iter().enumerate() {
            let expanded = self.expanded.contains(&ci);
            rows.push(Row {
                depth: 0,
                kind: RowKind::Collection,
                name: node.collection.name.clone(),
                expanded,
                collection: ci,
                endpoint: None,
            });
            if expanded && let Some(endpoints) = &node.endpoints {
                for (ei, (_, endpoint)) in endpoints.iter().enumerate() {
                    rows.push(Row {
                        depth: 1,
                        kind: RowKind::Endpoint,
                        name: endpoint.name.clone(),
                        expanded: false,
                        collection: ci,
                        endpoint: Some(ei),
                    });
                }
            }
        }
        rows
    }

    fn clamp_cursor(&mut self) {
        let last = self.rows().len().saturating_sub(1);
        self.cursor = self.cursor.min(last);
    }

    fn current_row(&self) -> Option<Row> {
        self.rows().into_iter().nth(self.cursor)
    }

    /// Moves the cursor up one row (clamped at the top).
    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Moves the cursor down one row (clamped at the bottom).
    pub fn move_down(&mut self) {
        self.cursor += 1;
        self.clamp_cursor();
    }

    /// Jumps to the first row.
    pub fn top(&mut self) {
        self.cursor = 0;
    }

    /// Jumps to the last row.
    pub fn bottom(&mut self) {
        self.cursor = self.rows().len().saturating_sub(1);
    }

    /// Activates the current row: toggles a collection, or returns the endpoint
    /// on a leaf row so the caller can load it into the request pane.
    pub fn select(&mut self) -> Result<Option<SelectedEndpoint>, PersistenceError> {
        let Some(row) = self.current_row() else {
            return Ok(None);
        };
        match row.kind {
            RowKind::Collection => {
                self.toggle(row.collection)?;
                Ok(None)
            }
            RowKind::Endpoint => Ok(self.selected_endpoint(&row)),
        }
    }

    /// `h`: collapse the current collection, or jump from an endpoint to its
    /// parent collection row.
    pub fn collapse(&mut self) {
        let Some(row) = self.current_row() else {
            return;
        };
        match row.kind {
            RowKind::Collection if row.expanded => {
                self.expanded.remove(&row.collection);
                self.clamp_cursor();
            }
            RowKind::Collection => {}
            RowKind::Endpoint => {
                // Parent collection row: count of visible rows before it.
                self.cursor = self
                    .rows()
                    .iter()
                    .position(|r| r.kind == RowKind::Collection && r.collection == row.collection)
                    .unwrap_or(0);
            }
        }
    }

    /// `l`: expand a collapsed collection (loading it lazily), or descend onto
    /// the first child of an expanded one.
    pub fn expand(&mut self) -> Result<(), PersistenceError> {
        let Some(row) = self.current_row() else {
            return Ok(());
        };
        if row.kind != RowKind::Collection {
            return Ok(());
        }
        if row.expanded {
            // Descend onto the first child, if the collection has one.
            if self
                .rows()
                .get(self.cursor + 1)
                .is_some_and(|next| next.kind == RowKind::Endpoint)
            {
                self.cursor += 1;
            }
        } else {
            self.collections[row.collection].load()?;
            self.expanded.insert(row.collection);
        }
        Ok(())
    }

    fn toggle(&mut self, collection: usize) -> Result<(), PersistenceError> {
        if self.expanded.contains(&collection) {
            self.expanded.remove(&collection);
            self.clamp_cursor();
        } else {
            self.collections[collection].load()?;
            self.expanded.insert(collection);
        }
        Ok(())
    }

    fn selected_endpoint(&self, row: &Row) -> Option<SelectedEndpoint> {
        let node = self.collections.get(row.collection)?;
        let (file, endpoint) = node.endpoints.as_ref()?.get(row.endpoint?)?;
        Some(SelectedEndpoint {
            display_path: format!("{}/{}", node.collection.name, endpoint.name),
            file: file.clone(),
            collection: row.collection,
            endpoint: endpoint.clone(),
        })
    }

    /// The collection-level template vars (`folder.toml` `[vars]`) for the
    /// `collection`-th collection, loaded lazily and cached. An unknown index or
    /// missing/invalid `folder.toml` yields an empty map.
    pub fn collection_vars(&mut self, collection: usize) -> BTreeMap<String, String> {
        self.collections
            .get_mut(collection)
            .map(|node| node.vars().clone())
            .unwrap_or_default()
    }

    /// Loads every collection's endpoints (for fuzzy search) and returns
    /// `(display path, collection index, endpoint index)` for each endpoint.
    pub fn all_endpoints(&mut self) -> Result<Vec<(String, usize, usize)>, PersistenceError> {
        let mut out = Vec::new();
        for (ci, node) in self.collections.iter_mut().enumerate() {
            let name = node.collection.name.clone();
            for (ei, (_, endpoint)) in node.load()?.iter().enumerate() {
                out.push((format!("{}/{}", name, endpoint.name), ci, ei));
            }
        }
        Ok(out)
    }

    // ---- M6.6 CRUD support ----

    /// Rebuilds the collection list from `workspace` while preserving the current
    /// cursor (clamped) and re-expanding collections that were expanded before.
    /// Endpoint caches are dropped (re-parsed lazily on next expand) so on-disk
    /// changes are picked up.
    pub fn reload(&mut self, workspace: Option<&OpenWorkspace>) -> Result<(), PersistenceError> {
        let expanded_names: HashSet<String> = self
            .expanded
            .iter()
            .filter_map(|&ci| self.collections.get(ci).map(|n| n.collection.name.clone()))
            .collect();
        let cursor = self.cursor;
        let rebuilt = Self::new(workspace)?;
        self.collections = rebuilt.collections;
        self.expanded.clear();
        // Re-expand collections whose names survived.
        for (ci, node) in self.collections.iter_mut().enumerate() {
            if expanded_names.contains(&node.collection.name) {
                node.load()?;
                self.expanded.insert(ci);
            }
        }
        self.cursor = cursor;
        self.clamp_cursor();
        Ok(())
    }

    /// The kind of the currently-selected row, if any.
    pub fn selected_kind(&self) -> Option<RowKind> {
        self.current_row().map(|row| row.kind)
    }

    /// The display name of the currently-selected row, if any.
    pub fn selected_name(&self) -> Option<String> {
        self.current_row().map(|row| row.name)
    }

    /// The directory of the collection relevant to the selection: the selected
    /// collection itself, or the collection owning the selected endpoint.
    pub fn selected_collection_dir(&self) -> Option<PathBuf> {
        let row = self.current_row()?;
        self.collections
            .get(row.collection)
            .map(|n| n.collection.path.clone())
    }

    /// The file path of the selected endpoint, if an endpoint row is selected.
    pub fn selected_endpoint_file(&self) -> Option<PathBuf> {
        let row = self.current_row()?;
        if row.kind != RowKind::Endpoint {
            return None;
        }
        let node = self.collections.get(row.collection)?;
        node.endpoints
            .as_ref()?
            .get(row.endpoint?)
            .map(|(path, _)| path.clone())
    }

    /// The index of the collection whose directory contains `file`, if any.
    /// Used to remap a loaded endpoint's collection index after a tree reload
    /// (name-sorted collections shift indices when siblings appear/vanish).
    pub fn collection_index_for_file(&self, file: &Path) -> Option<usize> {
        let parent = file.parent()?;
        self.collections
            .iter()
            .position(|n| n.collection.path == parent)
    }

    /// Whether the cursor is on an endpoint row backed by a *different* file than
    /// `current` (the loaded endpoint). A collection row or the same endpoint
    /// returns `false`.
    pub fn cursor_is_other_endpoint(&self, current: Option<&SelectedEndpoint>) -> bool {
        let Some(file) = self.selected_endpoint_file() else {
            return false;
        };
        match current {
            Some(selected) => selected.file != file,
            None => true,
        }
    }

    /// Replaces the cached copy of the endpoint at `path` with `endpoint` (after a
    /// save, so the tree name stays in sync). A no-op if the file isn't loaded.
    pub fn update_endpoint(&mut self, path: &Path, endpoint: Endpoint) {
        for node in &mut self.collections {
            if let Some(endpoints) = node.endpoints.as_mut()
                && let Some(slot) = endpoints.iter_mut().find(|(p, _)| p == path)
            {
                slot.1 = endpoint;
                return;
            }
        }
    }

    /// Expands the collection containing `file`, moves the cursor onto that row,
    /// and returns the endpoint for loading. Used after create/rename.
    pub fn select_file(
        &mut self,
        file: &Path,
    ) -> Result<Option<SelectedEndpoint>, PersistenceError> {
        // Find (and lazily load) the collection + endpoint index for `file`.
        let mut target: Option<(usize, usize)> = None;
        for (ci, node) in self.collections.iter_mut().enumerate() {
            let dir = node.collection.path.clone();
            if file.parent() != Some(dir.as_path()) {
                continue;
            }
            let endpoints = node.load()?;
            if let Some(ei) = endpoints.iter().position(|(p, _)| p == file) {
                target = Some((ci, ei));
                break;
            }
        }
        let Some((ci, ei)) = target else {
            return Ok(None);
        };
        self.jump_to(ci, ei)
    }

    /// Expands `collection` and moves the cursor onto its `endpoint`-th child,
    /// returning that endpoint for loading into the request pane. Used by the
    /// fuzzy search overlay to jump to a result.
    pub fn jump_to(
        &mut self,
        collection: usize,
        endpoint: usize,
    ) -> Result<Option<SelectedEndpoint>, PersistenceError> {
        if collection >= self.collections.len() {
            return Ok(None);
        }
        self.collections[collection].load()?;
        self.expanded.insert(collection);
        if let Some(pos) = self.rows().iter().position(|row| {
            row.kind == RowKind::Endpoint
                && row.collection == collection
                && row.endpoint == Some(endpoint)
        }) {
            self.cursor = pos;
        }
        let row = self.current_row();
        Ok(row.as_ref().and_then(|row| self.selected_endpoint(row)))
    }
}

/// Renders the explorer pane. Pure: no I/O, deterministic for snapshots. Takes
/// `&mut` because it updates the scroll offset to keep the cursor in view.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &mut ExplorerState,
    focused: bool,
    has_ws: bool,
    theme: &Theme,
    jump: Option<&JumpState>,
) {
    let (border_type, border_style) = if focused {
        (BorderType::Thick, theme.border_focused)
    } else {
        (BorderType::Plain, theme.border_unfocused)
    };
    let title = match jump.and_then(|j| j.label_for_pane(super::super::app::Pane::Explorer)) {
        Some(label) => format!(" Explorer [{label}] "),
        None => " Explorer ".to_owned(),
    };
    let block = Block::bordered()
        .border_type(border_type)
        .border_style(border_style)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if !has_ws {
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from("no workspace"),
            Line::from(""),
            Line::from("run churl in a directory"),
            Line::from("containing churl.toml"),
        ]);
        frame.render_widget(hint, inner);
        return;
    }

    let height = inner.height as usize;
    let offset = state.scroll_to_fit(height);
    let rows = state.rows();
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, row)| {
            let marker = match row.kind {
                RowKind::Collection if row.expanded => "▾ ",
                RowKind::Collection => "▸ ",
                RowKind::Endpoint => "",
            };
            let indent = "  ".repeat(row.depth);
            // In jump-mode, overlay the row's label at the start; otherwise the
            // usual cursor marker.
            match jump.and_then(|j| j.label_for_row(i)) {
                Some(label) => Line::from(vec![
                    Span::styled(label.to_string(), theme.jump_label),
                    Span::raw(format!(" {indent}{marker}{}", row.name)),
                ]),
                None => {
                    let cursor = if i == state.cursor { "> " } else { "  " };
                    let text = format!("{cursor}{indent}{marker}{}", row.name);
                    if i == state.cursor && focused {
                        Line::from(text).style(theme.selection)
                    } else {
                        Line::from(text)
                    }
                }
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
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
}
