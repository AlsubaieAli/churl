//! Request pane: method + URL + headers/params (read-only in M2) above an
//! edtui editor holding the request body.

use churl_core::model::Request;
use edtui::{EditorState, EditorTheme, EditorView};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};

/// Renders the request pane. The metadata part is read-only in M2; the body is
/// a full vim-modal edtui editor.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    request: Option<&Request>,
    editor: &mut EditorState,
    focused: bool,
) {
    let block = Block::bordered()
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .title(" Request ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(request) = request else {
        frame.render_widget(
            Paragraph::new(vec![Line::from(""), Line::from("no endpoint selected")]),
            inner,
        );
        return;
    };

    let mut meta: Vec<Line> = vec![Line::from(format!("{} {}", request.method, request.url))];
    if !request.headers.is_empty() {
        meta.push(Line::from("headers:"));
        for header in &request.headers {
            let off = if header.enabled { "" } else { " (off)" };
            meta.push(Line::from(format!(
                "  {}: {}{off}",
                header.name, header.value
            )));
        }
    }
    if !request.params.is_empty() {
        meta.push(Line::from("params:"));
        for param in &request.params {
            let off = if param.enabled { "" } else { " (off)" };
            meta.push(Line::from(format!("  {}={}{off}", param.name, param.value)));
        }
    }
    meta.push(Line::from("body:"));

    let [meta_area, body_area] =
        Layout::vertical([Constraint::Length(meta.len() as u16), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(Paragraph::new(meta), meta_area);

    let theme = EditorTheme::default().base(Style::default());
    EditorView::new(editor)
        .theme(theme)
        .wrap(true)
        .render(body_area, frame.buffer_mut());
}
