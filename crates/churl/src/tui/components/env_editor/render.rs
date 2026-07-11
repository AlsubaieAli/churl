//! The environments & variables editor's render/draw fns: the `render` modal
//! entry point and its private draw helpers (scope list, var rows, footer).
//! Split out of the env_editor module as a child module, so it keeps
//! full access to `EnvEditorState`'s private fields and private methods with no
//! visibility widening.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::{EnvEditorState, EnvField, EnvFieldEdit, EnvFocus, EnvScopeKind, ProfileNameTarget};
use crate::tui::components::prompt;
use crate::tui::theme::Theme;

/// Renders the environments & variables editor over `area`.
pub fn render(frame: &mut Frame, area: Rect, state: &EnvEditorState, theme: &Theme) {
    // Near-full-screen centered modal; clamps gracefully on small terminals.
    let [modal] = Layout::horizontal([Constraint::Percentage(90)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(90)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let dirty = if state.is_dirty() { " ●" } else { "" };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(format!(" Environments & Variables{dirty} "))
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Body (fill) + a two-row footer (precedence chain, then key hints).
    let [body, footer] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(inner);

    let left_width = inner.width.saturating_sub(2).clamp(1, 28);
    let [left, right] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Fill(1)]).areas(body);

    render_scope_list(frame, left, state, theme);
    render_var_rows(frame, right, state, theme);
    render_footer(frame, footer, state, theme);

    // A profile-name prompt or the discard confirm sit on top.
    if let Some(naming) = &state.naming {
        let title = match naming.target {
            ProfileNameTarget::New => "New profile",
            ProfileNameTarget::Rename(_) => "Rename profile",
        };
        prompt::render_prompt(frame, modal, title, &naming.editor, None, theme);
    } else if state.pending_close {
        prompt::render_confirm(
            frame,
            modal,
            "Unsaved changes",
            "Save changes before closing?",
            "s save · d discard · esc stay",
            theme,
        );
    }
}

/// Renders the left scope list, grouped with dim section headers.
fn render_scope_list(frame: &mut Frame, area: Rect, state: &EnvEditorState, theme: &Theme) {
    let focused = state.focus == EnvFocus::ScopeList;
    let border = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let block = Block::bordered()
        .border_style(border)
        .title(" Scopes ")
        .title_style(theme.title);
    let list_area = block.inner(area);
    frame.render_widget(block, area);
    if list_area.width == 0 || list_area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut last_group: Option<&str> = None;
    for (i, scope) in state.scopes.iter().enumerate() {
        let group = match scope.kind {
            EnvScopeKind::Workspace => "WORKSPACE",
            EnvScopeKind::Collection { .. } => "COLLECTIONS",
            EnvScopeKind::Profile => "PROFILES",
            EnvScopeKind::Session => "SESSION (read-only)",
        };
        if last_group != Some(group) {
            if last_group.is_some() {
                lines.push(Line::from(""));
            }
            lines.push(Line::styled(group.to_owned(), theme.auth_mask));
            last_group = Some(group);
        }
        // The active profile carries the `●` marker.
        let marker = if matches!(scope.kind, EnvScopeKind::Profile)
            && state.active_profile.as_deref() == Some(scope.label.as_str())
        {
            "● "
        } else {
            "  "
        };
        let label = format!("{marker}{}", scope.label);
        let line = Line::from(label);
        lines.push(if i == state.selected_scope {
            line.style(theme.selection)
        } else {
            line
        });
    }
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// Renders the right var-row list for the selected scope.
fn render_var_rows(frame: &mut Frame, area: Rect, state: &EnvEditorState, theme: &Theme) {
    let focused = state.focus == EnvFocus::VarRows;
    let border = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let scope = &state.scopes[state.selected_scope];
    let block = Block::bordered()
        .border_style(border)
        .title(format!(" {} ", scope.label))
        .title_style(theme.title);
    let rows_area = block.inner(area);
    frame.render_widget(block, area);
    if rows_area.width == 0 || rows_area.height == 0 {
        return;
    }

    if scope.vars.is_empty() {
        let empty = if matches!(scope.kind, EnvScopeKind::Session) {
            "(no session captures yet — run a sequence with a Session-target rule)"
        } else {
            "(no variables — press a to add)"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(empty, theme.auth_mask)),
            rows_area,
        );
        return;
    }

    // Align values in a column after the widest name (clamped).
    let name_col = scope
        .vars
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(6, 24);

    let visible = rows_area.height as usize;
    let offset = state.selected_row.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = scope
        .vars
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, (name, value))| {
            let selected = i == state.selected_row && focused;
            render_var_line(state, theme, i, name, value, name_col, selected)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows_area);
}

/// Builds one `name  value  <precedence>` row line.
fn render_var_line<'a>(
    state: &EnvEditorState,
    theme: &Theme,
    row: usize,
    name: &str,
    value: &str,
    name_col: usize,
    selected: bool,
) -> Line<'a> {
    let editing = state.editing.as_ref().filter(|e| e.row == row);
    let editing_name = editing.map(|e| e.field == EnvField::Name).unwrap_or(false);
    let editing_value = editing.map(|e| e.field == EnvField::Value).unwrap_or(false);

    let name_cell = if editing_name {
        field_with_cursor(editing.unwrap())
    } else {
        pad(name, name_col)
    };

    // An ephemeral peek reveals THIS row's resolved value in
    // place — but only when the reveal is pinned to exactly this scope+row (guarded
    // in `revealed_value`), so a stale reveal can never paint another row's secret.
    let revealed = (row == state.selected_row)
        .then(|| state.revealed_value())
        .flatten();

    // Session captures are ALWAYS masked — a captured token is a secret
    // regardless of its var name. Elsewhere, secret-named literal values
    // are masked unless a placeholder or being edited. A live peek overrides the
    // mask for its one row only.
    let value_cell = if editing_value {
        field_with_cursor(editing.unwrap())
    } else if let Some(plain) = revealed {
        plain.to_owned()
    } else if state.row_is_masked(row) {
        // Single source of truth: the display mask and the #3 copy-gate both read
        // `row_is_masked`, so they can never desync (this row renders rows of the
        // selected scope, so `row` is the same coordinate the helper indexes).
        "••••••".to_owned()
    } else {
        value.to_owned()
    };

    let tag = if editing.is_some() {
        String::new()
    } else {
        state.row_precedence_tag(name)
    };

    let mut spans = vec![
        Span::raw(pad(&name_cell, name_col)),
        Span::raw("  "),
        Span::raw(value_cell),
    ];
    // A small affordance right on the revealed row: it is a secret currently
    // visible, and how to re-mask / that it is ephemeral.
    if revealed.is_some() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "👁 revealed · y copy · any key re-masks",
            theme.status_error,
        ));
    }
    if !tag.is_empty() {
        spans.push(Span::styled(tag, theme.auth_mask));
    }
    let line = Line::from(spans);
    if selected {
        line.style(theme.selection)
    } else {
        line
    }
}

/// Renders an editing field's text with a block cursor at the caret.
fn field_with_cursor(edit: &EnvFieldEdit) -> String {
    let text = edit.editor.text();
    let cursor = edit.editor.cursor();
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i == cursor {
            out.push('█');
        }
        out.push(*c);
    }
    if cursor >= chars.len() {
        out.push('█');
    }
    out
}

/// Right-pads `text` to at least `width` display columns (char-count based;
/// good enough for the ASCII-ish var names this aligns).
fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// Renders the two-row footer: the selected row's precedence chain, then the
/// live key hints for the focused pane (or the active editor state).
fn render_footer(frame: &mut Frame, area: Rect, state: &EnvEditorState, theme: &Theme) {
    let [chain_row, hint_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    // Line 1: a live message (errors/status) wins over the precedence chain; the
    // `✓*` collection-override legend appends to the chain when relevant.
    let top = if let Some(msg) = &state.message {
        Line::styled(format!(" {msg}"), theme.status_error)
    } else if let Some(chain) = state.selected_row_chain() {
        let legend = if state.selected_row_has_collection_caveat() {
            "   * also set in a collection — overridden there per-request"
        } else {
            ""
        };
        Line::styled(format!(" {chain}{legend}"), theme.auth_mask)
    } else {
        Line::from("")
    };
    frame.render_widget(Paragraph::new(top), chain_row);

    // Line 2: key hints, built from the actual handler bindings for this state.
    let dirty = if state.is_dirty() {
        "● unsaved · "
    } else {
        ""
    };
    let hints = if state.pending_close {
        "s save · d discard · esc stay".to_owned()
    } else if state.naming.is_some() {
        "enter commit · esc cancel".to_owned()
    } else if state.editing.is_some() {
        "enter commit · tab name/value · esc cancel".to_owned()
    } else if state.selected_is_session() {
        // Read-only Session group: only view / clear / navigate.
        match state.focus {
            EnvFocus::ScopeList => {
                format!(
                    "{dirty}j/k move · l/enter view (read-only) · c clear session · w save · q close"
                )
            }
            EnvFocus::VarRows => {
                format!(
                    "{dirty}j/k move · p peek · read-only (run-populated, masked) · h scopes · q close"
                )
            }
        }
    } else {
        match state.focus {
            EnvFocus::ScopeList => format!(
                "{dirty}j/k move · l/enter edit vars · n new profile · r rename · d delete · x activate · w save · q close"
            ),
            EnvFocus::VarRows => format!(
                "{dirty}j/k move · a add · enter value · r name · d delete · p peek · h scopes · w save · q close"
            ),
        }
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {hints}"), theme.statusline)),
        hint_row,
    );
}
