//! Small centered modal overlays for CRUD flows: a text [`Prompt`] (a titled
//! [`LineEditor`], e.g. "New endpoint name") and a [`Confirm`] (a y/n question,
//! e.g. "Delete endpoint?"). Both are rendered as compact boxes centred over the
//! main area — the same visual family as the picker, smaller.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

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
