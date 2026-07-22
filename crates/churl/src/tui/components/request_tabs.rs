//! Request-pane tab state: which of Params / Headers / Auth / Body is active,
//! the per-tab row selection, and any in-progress field edit. This is the pure
//! state machine (no rendering, no I/O) so it is unit-testable; `request.rs`
//! renders it and `app.rs` drives the key handling against the live request.

use churl_core::model::{Body, BodyKind};

use super::line_editor::LineEditor;

/// The four selectable Body-tab kinds (M8.6) — the UI-facing superset of
/// [`BodyKind`] (which only names the three `Simple` kinds) plus `Multipart`.
/// Kept as its own type rather than folding `Multipart` into `BodyKind`
/// itself: `BodyKind` is the *simple*-body kind `content_type_for` and the
/// TUI's pre-M8.6 code reason about, and it has no `content_type_for` mapping
/// of its own (reqwest derives the multipart Content-Type) — see
/// `churl_core::model::BodyWireKind`'s doc for the same reasoning on the wire
/// side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyTypeUi {
    /// Plain text (`BodyKind::Text`).
    Text,
    /// JSON (`BodyKind::Json`).
    Json,
    /// URL-encoded form (`BodyKind::Form`).
    Form,
    /// `multipart/form-data` (`Body::Multipart`).
    Multipart,
}

impl BodyTypeUi {
    /// The four kinds, in display/cycle order.
    pub const ALL: [BodyTypeUi; 4] = [
        BodyTypeUi::Text,
        BodyTypeUi::Json,
        BodyTypeUi::Form,
        BodyTypeUi::Multipart,
    ];

    /// The short label shown in the picker / type row.
    pub fn label(self) -> &'static str {
        match self {
            BodyTypeUi::Text => "text",
            BodyTypeUi::Json => "json",
            BodyTypeUi::Form => "form",
            BodyTypeUi::Multipart => "multipart",
        }
    }

    /// Derives the current UI kind from the live body. `None` (no body yet)
    /// reads as `Text` — the same default `BodyKind`/`BodyWireKind` use when
    /// a `type` key is absent, so a fresh endpoint's Body tab starts on the
    /// same kind a bare `[request.body]` table with no `type` would load as.
    pub fn from_body(body: Option<&Body>) -> Self {
        match body {
            None => BodyTypeUi::Text,
            Some(Body::Simple {
                kind: BodyKind::Text,
                ..
            }) => BodyTypeUi::Text,
            Some(Body::Simple {
                kind: BodyKind::Json,
                ..
            }) => BodyTypeUi::Json,
            Some(Body::Simple {
                kind: BodyKind::Form,
                ..
            }) => BodyTypeUi::Form,
            Some(Body::Multipart(_)) => BodyTypeUi::Multipart,
        }
    }
}

/// The four request-pane tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestTab {
    /// URL query parameters (`Vec<Param>`).
    Params,
    /// Request headers (`Vec<Header>`).
    Headers,
    /// First-class auth (kind + fields).
    Auth,
    /// The request body (edtui editor).
    Body,
}

impl RequestTab {
    /// The four tabs in display / cycle order.
    pub const ALL: [RequestTab; 4] = [
        RequestTab::Params,
        RequestTab::Headers,
        RequestTab::Auth,
        RequestTab::Body,
    ];

    /// The short tab label.
    pub fn label(self) -> &'static str {
        match self {
            RequestTab::Params => "Params",
            RequestTab::Headers => "Headers",
            RequestTab::Auth => "Auth",
            RequestTab::Body => "Body",
        }
    }

    /// The next tab (wrapping Body→Params).
    pub fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    /// The previous tab (wrapping Params→Body).
    pub fn prev(self) -> Self {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Which field of a row is being edited (name vs value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditField {
    /// The name/key column.
    Name,
    /// The value column.
    Value,
}

/// An in-progress row field edit: a [`LineEditor`] over one field of the row at
/// `row` on the active tab. The edit is committed into the live request on
/// commit and discarded on cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldEdit {
    /// The row index being edited.
    pub row: usize,
    /// Which column is under the editor.
    pub field: EditField,
    /// The line editor holding the in-progress text.
    pub editor: LineEditor,
}

/// Request-pane tab state. Row selections persist per tab so switching away and
/// back keeps the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestTabs {
    /// The active tab.
    pub active: RequestTab,
    /// Selected row on the Params tab.
    pub params_sel: usize,
    /// Selected row on the Headers tab.
    pub headers_sel: usize,
    /// Selected row on the Auth tab.
    pub auth_sel: usize,
    /// Selected row on the Body tab when its kind is `Multipart` (M8.6): row 0
    /// is the type-selector row (mirrors the Auth tab's kind row), rows 1..N
    /// are the parts (row `i` = part `i - 1`). Unused (stays whatever it was)
    /// on a non-multipart Body — same "persists per tab" contract as the
    /// other three selections.
    pub parts_sel: usize,
    /// An in-progress row field edit, if any.
    pub editing: Option<FieldEdit>,
    /// Vertical scroll offset (first visible row index) for the active row-list
    /// tab, kept so the selection stays in the viewport when the list is taller
    /// than the pane (mirrors the explorer's `scroll_to_fit`).
    pub row_scroll: usize,
}

impl Default for RequestTabs {
    fn default() -> Self {
        Self {
            active: RequestTab::Params,
            params_sel: 0,
            headers_sel: 0,
            auth_sel: 0,
            parts_sel: 0,
            editing: None,
            row_scroll: 0,
        }
    }
}

impl RequestTabs {
    /// Selects the next tab (cancels any in-progress edit).
    pub fn tab_next(&mut self) {
        self.editing = None;
        self.active = self.active.next();
    }

    /// Selects the previous tab (cancels any in-progress edit).
    pub fn tab_prev(&mut self) {
        self.editing = None;
        self.active = self.active.prev();
    }

    /// Jumps to the tab at index `i` (0..=3); out-of-range is ignored.
    pub fn tab_jump(&mut self, i: usize) {
        if let Some(&tab) = RequestTab::ALL.get(i) {
            self.editing = None;
            self.active = tab;
        }
    }

    /// The selected-row index for the active row-list tab (Params/Headers/Auth,
    /// or Body when its kind is Multipart — see [`Self::parts_sel`]).
    pub fn selection(&self) -> usize {
        match self.active {
            RequestTab::Params => self.params_sel,
            RequestTab::Headers => self.headers_sel,
            RequestTab::Auth => self.auth_sel,
            RequestTab::Body => self.parts_sel,
        }
    }

    /// Sets the selected-row index for the active row-list tab.
    fn set_selection(&mut self, sel: usize) {
        match self.active {
            RequestTab::Params => self.params_sel = sel,
            RequestTab::Headers => self.headers_sel = sel,
            RequestTab::Auth => self.auth_sel = sel,
            RequestTab::Body => self.parts_sel = sel,
        }
    }

    /// Moves the active tab's selection up (clamped).
    pub fn move_up(&mut self) {
        let sel = self.selection().saturating_sub(1);
        self.set_selection(sel);
    }

    /// Moves the active tab's selection down, clamped to `row_count - 1`
    /// (`row_count` is the live number of rows on the active tab).
    pub fn move_down(&mut self, row_count: usize) {
        let max = row_count.saturating_sub(1);
        let sel = (self.selection() + 1).min(max);
        self.set_selection(sel);
    }

    /// Clamps the active tab's selection into `0..row_count` (call after a delete).
    pub fn clamp(&mut self, row_count: usize) {
        let max = row_count.saturating_sub(1);
        let sel = self.selection().min(max);
        self.set_selection(sel);
    }

    /// Adjusts `row_scroll` so the active tab's selection stays within a
    /// `height`-row viewport over `row_count` rows, and returns the offset.
    /// Mirrors `ExplorerState::scroll_to_fit`. Called by the renderer with the
    /// content pane's inner height.
    pub fn scroll_to_fit(&mut self, row_count: usize, height: usize) -> usize {
        if height == 0 {
            self.row_scroll = 0;
            return 0;
        }
        let sel = self.selection();
        if sel < self.row_scroll {
            self.row_scroll = sel;
        } else if sel >= self.row_scroll + height {
            self.row_scroll = sel + 1 - height;
        }
        let max_scroll = row_count.saturating_sub(height);
        self.row_scroll = self.row_scroll.min(max_scroll);
        self.row_scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycles_both_ways() {
        let mut tabs = RequestTabs::default();
        assert_eq!(tabs.active, RequestTab::Params);
        tabs.tab_next();
        assert_eq!(tabs.active, RequestTab::Headers);
        tabs.tab_prev();
        assert_eq!(tabs.active, RequestTab::Params);
        tabs.tab_prev();
        assert_eq!(tabs.active, RequestTab::Body, "prev wraps to Body");
        tabs.tab_next();
        assert_eq!(tabs.active, RequestTab::Params, "next wraps to Params");
    }

    #[test]
    fn direct_jump_selects_tab() {
        let mut tabs = RequestTabs::default();
        tabs.tab_jump(2);
        assert_eq!(tabs.active, RequestTab::Auth);
        tabs.tab_jump(3);
        assert_eq!(tabs.active, RequestTab::Body);
        tabs.tab_jump(9); // out of range, ignored
        assert_eq!(tabs.active, RequestTab::Body);
    }

    #[test]
    fn per_tab_selection_persists() {
        let mut tabs = RequestTabs::default();
        // Params selection.
        tabs.move_down(5);
        tabs.move_down(5);
        assert_eq!(tabs.params_sel, 2);
        // Switch to Headers — its own selection.
        tabs.tab_next();
        assert_eq!(tabs.selection(), 0);
        tabs.move_down(3);
        assert_eq!(tabs.headers_sel, 1);
        // Back to Params — the old selection is intact.
        tabs.tab_prev();
        assert_eq!(tabs.selection(), 2);
    }

    #[test]
    fn selection_clamps() {
        let mut tabs = RequestTabs::default();
        for _ in 0..10 {
            tabs.move_down(3);
        }
        assert_eq!(tabs.params_sel, 2, "clamped to row_count-1");
        tabs.move_up();
        assert_eq!(tabs.params_sel, 1);
        // After a delete leaves fewer rows, clamp pulls it in.
        tabs.params_sel = 5;
        tabs.clamp(2);
        assert_eq!(tabs.params_sel, 1);
        // Empty list clamps to 0.
        tabs.clamp(0);
        assert_eq!(tabs.params_sel, 0);
    }

    #[test]
    fn row_scroll_keeps_selection_in_view() {
        let mut tabs = RequestTabs::default();
        // 10 rows, 3-high viewport. Selection at top → no scroll.
        assert_eq!(tabs.scroll_to_fit(10, 3), 0);
        // Move the selection to row 5: viewport must scroll to include it.
        for _ in 0..5 {
            tabs.move_down(10);
        }
        let off = tabs.scroll_to_fit(10, 3);
        assert!(off <= 5 && 5 < off + 3, "row 5 must be visible: off={off}");
        // Move back to top: scroll returns to 0.
        for _ in 0..10 {
            tabs.move_up();
        }
        assert_eq!(tabs.scroll_to_fit(10, 3), 0);
        // Never scrolls past the last screenful.
        for _ in 0..10 {
            tabs.move_down(10);
        }
        let off = tabs.scroll_to_fit(10, 3);
        assert_eq!(off, 7, "max scroll = row_count - height");
        // Zero height resets.
        assert_eq!(tabs.scroll_to_fit(10, 0), 0);
    }

    #[test]
    fn switching_tab_cancels_edit() {
        let mut tabs = RequestTabs {
            editing: Some(FieldEdit {
                row: 0,
                field: EditField::Name,
                editor: LineEditor::new("x"),
            }),
            ..RequestTabs::default()
        };
        tabs.tab_next();
        assert!(tabs.editing.is_none());
    }

    #[test]
    fn body_type_ui_from_body_matches_every_variant() {
        use churl_core::model::{Part, PartValue};

        assert_eq!(BodyTypeUi::from_body(None), BodyTypeUi::Text);
        assert_eq!(
            BodyTypeUi::from_body(Some(&Body::Simple {
                kind: BodyKind::Text,
                content: String::new(),
            })),
            BodyTypeUi::Text
        );
        assert_eq!(
            BodyTypeUi::from_body(Some(&Body::Simple {
                kind: BodyKind::Json,
                content: String::new(),
            })),
            BodyTypeUi::Json
        );
        assert_eq!(
            BodyTypeUi::from_body(Some(&Body::Simple {
                kind: BodyKind::Form,
                content: String::new(),
            })),
            BodyTypeUi::Form
        );
        assert_eq!(
            BodyTypeUi::from_body(Some(&Body::Multipart(vec![Part {
                name: "f".into(),
                value: PartValue::Text("v".into()),
            }]))),
            BodyTypeUi::Multipart
        );
    }

    #[test]
    fn every_body_type_ui_has_exactly_one_label() {
        let labels: Vec<&str> = BodyTypeUi::ALL.iter().map(|k| k.label()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "labels must be unique");
    }

    #[test]
    fn body_tab_selection_routes_through_parts_sel() {
        let mut tabs = RequestTabs::default();
        tabs.tab_jump(3); // Body
        assert_eq!(tabs.active, RequestTab::Body);
        assert_eq!(tabs.selection(), 0);
        tabs.move_down(5);
        tabs.move_down(5);
        assert_eq!(tabs.parts_sel, 2);
        assert_eq!(tabs.selection(), 2);
        // Switching away and back preserves it, same as every other tab.
        tabs.tab_prev();
        tabs.tab_next();
        assert_eq!(tabs.selection(), 2);
    }
}
