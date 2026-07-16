//! Small centered modal overlays for CRUD flows: a text prompt (a titled
//! [`LineEditor`] for most purposes, e.g. "New endpoint name"; the new-endpoint
//! prompt instead hosts the multi-line edtui vim editor via
//! [`render_prompt_multiline`] so a pasted browser curl is editable across
//! lines) and a [`Confirm`] (a y/n question, e.g. "Delete endpoint?"). All are
//! rendered as compact boxes centred over the main area — the same visual
//! family as the picker, smaller.

use edtui::{EditorMode, EditorState, EditorTheme, EditorView};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};

use super::line_editor::LineEditor;
use crate::tui::theme::Theme;

/// Renders a text-input prompt: a titled box with the editor line and a block
/// cursor at the editor's cursor column. `hint` renders dimly under the input
/// (e.g. the required collection name for a typed-confirm delete).
pub fn render_prompt(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    editor: &LineEditor,
    hint: Option<&str>,
    theme: &Theme,
) {
    let height = if hint.is_some() { 5 } else { 4 };
    let [modal] = Layout::horizontal([Constraint::Length(50)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(format!(" {title} "))
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let text = editor.text();
    let cursor = editor.cursor();
    let chars: Vec<char> = text.chars().collect();
    let mut line = String::from("> ");
    for (i, c) in chars.iter().enumerate() {
        if i == cursor {
            line.push('█');
        }
        line.push(*c);
    }
    if cursor >= chars.len() {
        line.push('█');
    }

    let mut lines = vec![Line::from(line)];
    if let Some(hint) = hint {
        lines.push(Line::styled(hint.to_owned(), theme.auth_mask));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Renders a multi-line text-input prompt: a titled box hosting the edtui
/// vim editor (the new-endpoint paste-curl prompt), with an optional dim hint
/// under it and a mode-aware commit/cancel hint in the bottom border — Insert
/// mode edits (Enter adds a line, vim-faithful), Normal mode is where Enter
/// submits, matching [`crate::tui::app::handlers::crud::App::handle_curl_prompt_key`].
/// Taller than the single-line [`render_prompt`] so a pasted multi-line browser
/// curl is visible while editing. Reuses the same edtui `EditorView` the URL
/// popup renders.
pub fn render_prompt_multiline(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    editor: &mut EditorState,
    hint: Option<&str>,
    theme: &Theme,
) {
    let [modal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Length(12)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let footer = if editor.mode == EditorMode::Insert {
        " insert: enter=newline · esc=normal "
    } else {
        " normal: enter=submit · esc=cancel "
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(format!(" {title} "))
        .title_style(theme.title)
        .title_bottom(Line::from(footer).right_aligned());
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    // Reserve the last inner row for the hint when present; the editor fills the
    // rest.
    let (editor_area, hint_area) = match hint {
        Some(_) if inner.height > 1 => {
            let [editor_area, hint_area] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
            (editor_area, Some(hint_area))
        }
        _ => (inner, None),
    };
    let editor_theme = EditorTheme::default().base(Style::default());
    EditorView::new(editor)
        .theme(editor_theme)
        .wrap(false)
        .render(editor_area, frame.buffer_mut());
    if let Some(hint) = hint
        && let Some(hint_area) = hint_area
    {
        frame.render_widget(
            Paragraph::new(Line::styled(hint.to_owned(), theme.auth_mask)),
            hint_area,
        );
    }
}

/// Renders a confirmation: a titled box with the question and a key hint line
/// (e.g. `[y/n]` or `s save · d discard · esc stay`).
pub fn render_confirm(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    question: &str,
    hint: &str,
    theme: &Theme,
) {
    let [modal] = Layout::horizontal([Constraint::Length(50)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Length(4)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(format!(" {title} "))
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(question.to_owned()),
            Line::styled(hint.to_owned(), theme.auth_mask),
        ]),
        inner,
    );
}
