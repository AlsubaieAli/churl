//! Request-pane tab state: which of Params / Headers / Auth / Body is active,
//! the per-tab row selection, and any in-progress field edit. This is the pure
//! state machine (no rendering, no I/O) so it is unit-testable; `request.rs`
//! renders it and `app.rs` drives the key handling against the live request.

use super::line_editor::LineEditor;

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
    /// An in-progress row field edit, if any.
    pub editing: Option<FieldEdit>,
}

impl Default for RequestTabs {
    fn default() -> Self {
        Self {
            active: RequestTab::Params,
            params_sel: 0,
            headers_sel: 0,
            auth_sel: 0,
            editing: None,
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

    /// The selected-row index for the active row-list tab (Params/Headers/Auth).
    /// Body has no row selection; returns 0.
    pub fn selection(&self) -> usize {
        match self.active {
            RequestTab::Params => self.params_sel,
            RequestTab::Headers => self.headers_sel,
            RequestTab::Auth => self.auth_sel,
            RequestTab::Body => 0,
        }
    }

    /// Sets the selected-row index for the active row-list tab.
    fn set_selection(&mut self, sel: usize) {
        match self.active {
            RequestTab::Params => self.params_sel = sel,
            RequestTab::Headers => self.headers_sel = sel,
            RequestTab::Auth => self.auth_sel = sel,
            RequestTab::Body => {}
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
}
