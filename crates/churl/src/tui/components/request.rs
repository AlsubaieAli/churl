//! Request pane: method + URL + headers/params (read-only in M2) above an
//! edtui editor holding the request body.

use churl_core::config::{is_template_placeholder, looks_like_secret_name};
use churl_core::model::{ApiKeyPlacement, Auth, Request};
use edtui::{EditorState, EditorTheme, EditorView};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};

use super::jump::JumpState;
use crate::tui::theme::Theme;

/// Renders the request pane. The metadata part is read-only in M2; the body is
/// a full vim-modal edtui editor.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    request: Option<&Request>,
    editor: &mut EditorState,
    focused: bool,
    theme: &Theme,
    jump: Option<&JumpState>,
) {
    let (border_type, border_style) = if focused {
        (BorderType::Thick, theme.border_focused)
    } else {
        (BorderType::Plain, theme.border_unfocused)
    };
    let title = match jump.and_then(|j| j.label_for_pane(super::super::app::Pane::Request)) {
        Some(label) => format!(" Request [{label}] "),
        None => " Request ".to_owned(),
    };
    let block = Block::bordered()
        .border_type(border_type)
        .border_style(border_style)
        .title(title)
        .title_style(theme.title);
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
    if let Some(auth) = &request.auth {
        meta.push(Line::from(auth_line(auth)));
    }
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

/// The read-only auth meta line. Rendering concern only: `{{...}}` placeholders
/// show verbatim; a *literal* secret value is masked as `*****` and never
/// rendered (driven by the same placeholder / secret-name checks core uses to
/// gate saves). Non-secret-named api-key values render verbatim.
fn auth_line(auth: &Auth) -> String {
    match auth {
        Auth::Basic { username, password } => {
            format!("auth: basic {username} · password {}", mask(password))
        }
        Auth::Bearer { token } => format!("auth: bearer {}", mask(token)),
        Auth::ApiKey {
            name,
            value,
            placement,
        } => {
            let shown = if looks_like_secret_name(name) {
                mask(value)
            } else {
                value
            };
            let place = match placement {
                ApiKeyPlacement::Header => "header",
                ApiKeyPlacement::Query => "query",
            };
            format!("auth: apikey {name}={shown} ({place})")
        }
    }
}

/// Masks a literal secret value; `{{...}}` placeholders pass through verbatim.
fn mask(value: &str) -> &str {
    if is_template_placeholder(value) {
        value
    } else {
        "*****"
    }
}
