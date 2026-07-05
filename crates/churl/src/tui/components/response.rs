//! Response pane. M2 placeholder: shows the selected endpoint's method + URL;
//! real response rendering arrives with request execution in M3.

use churl_core::model::Request;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph};

/// Renders the response pane placeholder.
pub fn render(frame: &mut Frame, area: Rect, request: Option<&Request>, focused: bool) {
    let block = Block::bordered()
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .title(" Response ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = match request {
        Some(request) => vec![
            Line::from(format!("{} {}", request.method, request.url)),
            Line::from(""),
            Line::from("no response yet — send arrives in M3"),
        ],
        None => vec![Line::from(""), Line::from("no endpoint selected")],
    };
    frame.render_widget(Paragraph::new(lines), inner);
}
