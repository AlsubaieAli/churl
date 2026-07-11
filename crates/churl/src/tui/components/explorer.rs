//! Explorer pane: workspace tree navigation (collection → endpoint).
//!
//! Collections are listed at startup (a cheap directory scan) but endpoint files
//! are parsed lazily on first expand, keeping the cold-start budget intact.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use churl_core::model::{Endpoint, Sequence};
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
///
/// Sequences no longer live in the tree: they render in a dedicated
/// toggle-able sub-pane at the bottom of the explorer column, keyed off
/// [`ExplorerState::seq_cursor`] rather than tree rows.
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

/// A request sequence the user selected in the explorer, ready to run or edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedSequence {
    /// Display name.
    pub name: String,
    /// Path of the sequence's TOML file.
    pub file: PathBuf,
    /// The parsed sequence.
    pub sequence: Sequence,
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
    /// Warnings from the last lenient load (one per skipped/unparseable file),
    /// produced once when the collection is first parsed and drained by
    /// [`ExplorerState::take_warnings`] so the app can surface them.
    warnings: Vec<String>,
}

impl CollectionNode {
    /// Parses the collection's endpoint files if not already loaded, using the
    /// lenient load path: a single unparseable file is degraded to a warning
    /// (stored on the node) instead of aborting the whole load. Only a directory
    /// read error is a hard error.
    fn load(&mut self) -> Result<&[(PathBuf, Endpoint)], PersistenceError> {
        if self.endpoints.is_none() {
            let load = self.collection.endpoints_lenient()?;
            self.warnings = load.warnings;
            self.endpoints = Some(load.endpoints);
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
    /// Request sequences, loaded eagerly at open/reload (a small set).
    sequences: Vec<(PathBuf, Sequence)>,
    /// Warnings from the last lenient sequence load, drained by [`take_warnings`].
    ///
    /// [`take_warnings`]: ExplorerState::take_warnings
    sequence_warnings: Vec<String>,
    expanded: HashSet<usize>,
    /// Cursor position as an index into [`ExplorerState::rows`].
    pub cursor: usize,
    /// Index of the first visible row; kept so the cursor stays in the viewport
    /// even when the tree is taller than the pane (adjusted by [`scroll_to_fit`]).
    ///
    /// [`scroll_to_fit`]: ExplorerState::scroll_to_fit
    scroll: usize,
    /// Cursor into [`ExplorerState::sequences`] for the sequences sub-pane
    /// Independent of the tree `cursor`.
    seq_cursor: usize,
    /// First visible sequence row, keeping the sequence cursor in view when the
    /// sub-pane is shorter than the sequence list.
    seq_scroll: usize,
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
                    warnings: Vec::new(),
                })
                .collect(),
            None => Vec::new(),
        };
        // Sequences load eagerly (a small, flat set); a single unparseable file
        // degrades to a warning surfaced via `take_warnings`, never aborts.
        let (sequences, sequence_warnings) = match workspace {
            Some(ws) => {
                let load = ws.sequences()?;
                (load.sequences, load.warnings)
            }
            None => (Vec::new(), Vec::new()),
        };
        Ok(Self {
            collections,
            sequences,
            sequence_warnings,
            expanded: HashSet::new(),
            cursor: 0,
            scroll: 0,
            seq_cursor: 0,
            seq_scroll: 0,
        })
    }

    /// Index of the first visible row (the scroll offset as of the last
    /// [`scroll_to_fit`] call). Jump-mode uses it to start labelling at the
    /// viewport instead of the top of the tree.
    ///
    /// [`scroll_to_fit`]: ExplorerState::scroll_to_fit
    pub fn first_visible(&self) -> usize {
        self.scroll
    }

    /// Adjusts the scroll offset so the cursor stays within a `height`-row
    /// viewport, clamped so we never scroll past the last screenful, and returns
    /// the offset. Called by [`render`] with the pane's inner height.
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

    /// Flattens the tree into the currently visible rows (collections and, when
    /// expanded, their endpoints). Sequences are NOT tree rows —
    /// they live in the dedicated sub-pane.
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

    /// The endpoint under the tree cursor, if the cursor is on an endpoint row
    /// Read-only — unlike [`select`], it neither toggles a
    /// collection nor loads a buffer; it just peeks at what is hovered. Powers
    /// the hover-vs-selection fallback for one-shot read actions.
    pub fn hovered_endpoint(&self) -> Option<SelectedEndpoint> {
        let row = self.current_row()?;
        (row.kind == RowKind::Endpoint)
            .then(|| self.selected_endpoint(&row))
            .flatten()
    }

    /// The sequence under the sub-pane cursor, if any. Read by the run
    /// and edit paths; no tree-cursor gate — the sub-pane owns the selection.
    pub fn selected_sequence(&self) -> Option<SelectedSequence> {
        let (file, sequence) = self.sequences.get(self.seq_cursor)?;
        Some(SelectedSequence {
            name: sequence.name.clone(),
            file: file.clone(),
            sequence: sequence.clone(),
        })
    }

    /// Number of loaded sequences (drives the sub-pane + stub summary).
    pub fn sequences_len(&self) -> usize {
        self.sequences.len()
    }

    /// The sub-pane cursor into the sequence list.
    pub fn seq_cursor(&self) -> usize {
        self.seq_cursor
    }

    /// Moves the sequence sub-pane cursor up one (clamped at the top).
    pub fn seq_move_up(&mut self) {
        self.seq_cursor = self.seq_cursor.saturating_sub(1);
    }

    /// Moves the sequence sub-pane cursor down one (clamped at the bottom).
    pub fn seq_move_down(&mut self) {
        if self.seq_cursor + 1 < self.sequences.len() {
            self.seq_cursor += 1;
        }
    }

    /// Jumps the sequence cursor to the first sequence.
    pub fn seq_top(&mut self) {
        self.seq_cursor = 0;
    }

    /// Jumps the sequence cursor to the last sequence.
    pub fn seq_bottom(&mut self) {
        self.seq_cursor = self.sequences.len().saturating_sub(1);
    }

    /// Keeps the sequence cursor within a `height`-row viewport, returning the
    /// scroll offset (mirrors [`scroll_to_fit`] for the endpoints tree).
    ///
    /// [`scroll_to_fit`]: ExplorerState::scroll_to_fit
    fn seq_scroll_to_fit(&mut self, height: usize) -> usize {
        if height == 0 {
            self.seq_scroll = 0;
            return 0;
        }
        if self.seq_cursor < self.seq_scroll {
            self.seq_scroll = self.seq_cursor;
        } else if self.seq_cursor >= self.seq_scroll + height {
            self.seq_scroll = self.seq_cursor + 1 - height;
        }
        let max_scroll = self.sequences.len().saturating_sub(height);
        self.seq_scroll = self.seq_scroll.min(max_scroll);
        self.seq_scroll
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

    /// Loads every collection's endpoints and returns their file paths (for the
    /// sequence editor's add-step picker).
    pub fn all_endpoint_files(&mut self) -> Result<Vec<PathBuf>, PersistenceError> {
        let mut out = Vec::new();
        for node in &mut self.collections {
            for (path, _) in node.load()? {
                out.push(path.clone());
            }
        }
        Ok(out)
    }

    /// Every loaded sequence as `(name, file)`, in explorer order — for the
    /// `<leader>s o` "open sequence" picker.
    pub fn all_sequences(&self) -> Vec<(String, PathBuf)> {
        self.sequences
            .iter()
            .map(|(file, sequence)| (sequence.name.clone(), file.clone()))
            .collect()
    }

    /// Moves the sequence sub-pane cursor onto the sequence backed by `file`, if
    /// it is loaded. Keeps run/edit consistent with a `<leader>s o` pick — a
    /// later `<leader>s r` runs the picked sequence, not sequence #0. Returns
    /// whether a match was found.
    pub fn select_sequence_file(&mut self, file: &Path) -> bool {
        if let Some(idx) = self.sequences.iter().position(|(p, _)| p == file) {
            self.seq_cursor = idx;
            true
        } else {
            false
        }
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

    // ---- CRUD support ----

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
        self.sequences = rebuilt.sequences;
        self.sequence_warnings = rebuilt.sequence_warnings;
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
        // Keep the sequence sub-pane cursor in range after a reload changes the
        // sequence count.
        self.seq_cursor = self.seq_cursor.min(self.sequences.len().saturating_sub(1));
        self.seq_scroll = 0;
        Ok(())
    }

    /// Drains and returns all pending load warnings accumulated across collections
    /// (one per skipped/unparseable endpoint file, produced by the lenient load).
    /// The app calls this after any operation that may have parsed endpoints and
    /// surfaces the result in the message row (never silently swallowed).
    pub fn take_warnings(&mut self) -> Vec<String> {
        let mut all = Vec::new();
        for node in &mut self.collections {
            all.append(&mut node.warnings);
        }
        all.append(&mut self.sequence_warnings);
        all
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

    /// The file path backing `row`, for endpoint rows whose collection is
    /// loaded. Used by [`render`] to match the loaded-and-dirty endpoint by
    /// path (never by index — reloads shift indices).
    pub fn row_endpoint_file(&self, row: &Row) -> Option<&Path> {
        let node = self.collections.get(row.collection)?;
        node.endpoints
            .as_ref()?
            .get(row.endpoint?)
            .map(|(path, _)| path.as_path())
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
/// `dirty_files` are the files of every open buffer with unsaved changes — each
/// matching endpoint row gets an accent `●` suffix (the editor modified-file
/// convention), matched by path, cleared on save/discard/close.
#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &mut ExplorerState,
    focused: bool,
    has_ws: bool,
    theme: &Theme,
    jump: Option<&JumpState>,
    dirty_files: &[PathBuf],
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
            // Any open dirty buffer's endpoint row gets an accent ● suffix,
            // matched by file path (indices shift across reloads).
            let dirty_suffix = (row.kind == RowKind::Endpoint
                && state
                    .row_endpoint_file(row)
                    .is_some_and(|f| dirty_files.iter().any(|d| d.as_path() == f)))
            .then(|| Span::styled(" ●", theme.accent));
            // `f`-jump is pane-only now — rows carry no jump
            // labels, only the usual cursor marker.
            let cursor = if i == state.cursor { "> " } else { "  " };
            let text = format!("{cursor}{indent}{marker}{}", row.name);
            let mut spans = vec![Span::raw(text)];
            spans.extend(dirty_suffix);
            let line = Line::from(spans);
            if i == state.cursor && focused {
                line.style(theme.selection)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Renders the sequences sub-pane: a flat list of sequence names keyed
/// off [`ExplorerState::seq_cursor`], with a `Sequences` title, a thick border
/// when focused, and empty-state text when the workspace has none. Pure and
/// deterministic (snapshot-safe). Takes `&mut` to update the scroll offset.
pub fn render_sequences_pane(
    frame: &mut Frame,
    area: Rect,
    state: &mut ExplorerState,
    focused: bool,
    theme: &Theme,
    jump_label: Option<char>,
) {
    let (border_type, border_style) = if focused {
        (BorderType::Thick, theme.border_focused)
    } else {
        (BorderType::Plain, theme.border_unfocused)
    };
    let title = match jump_label {
        Some(label) => format!(" Sequences [{label}] "),
        None => " Sequences ".to_owned(),
    };
    let block = Block::bordered()
        .border_type(border_type)
        .border_style(border_style)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if state.sequences.is_empty() {
        // Informative empty state for the EXPANDED/focused pane: when
        // `f s` (or the `s` overlay) zooms in on a sequence-less workspace, tell
        // the user how to add one rather than showing a blank body. Dim-hint
        // styling (`theme.statusline`) matches the collapsed peek stub.
        let hint = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled("No sequences yet", theme.statusline)),
            Line::from(""),
            Line::from(Span::styled(
                "Press <leader>s a to add one",
                theme.statusline,
            )),
        ]);
        frame.render_widget(hint, inner);
        return;
    }

    let height = inner.height as usize;
    let offset = state.seq_scroll_to_fit(height);
    let lines: Vec<Line> = state
        .sequences
        .iter()
        .enumerate()
        .skip(offset)
        .take(height)
        .map(|(i, (_, sequence))| {
            let cursor = if i == state.seq_cursor { "> " } else { "  " };
            let line = Line::from(Span::raw(format!("{cursor}{}", sequence.name)));
            if i == state.seq_cursor && focused {
                line.style(theme.selection)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A one-line summary for the sequences sub-pane's collapsed stub (the count of
/// sequences, or an empty-state note).
pub fn sequences_stub_summary(state: &ExplorerState, theme: &Theme) -> Line<'static> {
    let text = match state.sequences.len() {
        // A zero-sequence peek shows an add affordance, not a dead "no
        // sequences" — matches the `<leader>` glyph convention used elsewhere.
        0 => "<leader>s a to add".to_owned(),
        1 => "1 sequence".to_owned(),
        n => format!("{n} sequences"),
    };
    Line::from(Span::styled(text, theme.statusline))
}

#[cfg(test)]
mod tests;
