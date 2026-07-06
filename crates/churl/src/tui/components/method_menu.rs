//! The one-key method-picker menu (`M` on the focused URL bar): a small overlay
//! listing every [`Method`] with a home-row-style single-key label. Pressing a
//! label sets the method; `Esc` cancels.

use churl_core::model::Method;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::tui::theme::Theme;

/// `(label key, method)` pairs. Labels are mnemonic where distinct
/// (g=GET, p=POST, u=PUT, t=PATCH, d=DELETE, h=HEAD, o=OPTIONS).
pub const MENU: &[(char, Method)] = &[
    ('g', Method::Get),
    ('p', Method::Post),
    ('u', Method::Put),
    ('t', Method::Patch),
    ('d', Method::Delete),
    ('h', Method::Head),
    ('o', Method::Options),
];

/// Resolves a pressed key to its method, if it labels one.
pub fn method_for(key: char) -> Option<Method> {
    MENU.iter()
        .find(|(label, _)| *label == key)
        .map(|(_, method)| *method)
}

/// Renders the method menu as a small centered overlay.
pub fn render(frame: &mut Frame, area: Rect, current: Method, theme: &Theme) {
    let height = MENU.len() as u16 + 2; // border top/bottom.
    let [modal] = Layout::horizontal([Constraint::Length(24)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(" Method ")
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let lines: Vec<Line> = MENU
        .iter()
        .map(|(label, method)| {
            let marker = if *method == current { "●" } else { " " };
            let line = Line::from(format!(" {marker} {label}  {method}"));
            if *method == current {
                line.style(theme.selection)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_resolve_to_methods() {
        assert_eq!(method_for('g'), Some(Method::Get));
        assert_eq!(method_for('p'), Some(Method::Post));
        assert_eq!(method_for('o'), Some(Method::Options));
        assert_eq!(method_for('z'), None);
    }

    #[test]
    fn every_method_has_exactly_one_label() {
        for method in Method::ALL {
            let count = MENU.iter().filter(|(_, m)| *m == method).count();
            assert_eq!(count, 1, "{method} must have exactly one label");
        }
    }
}
