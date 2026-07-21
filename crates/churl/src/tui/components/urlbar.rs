//! URL bar: a slim focusable strip above the Request pane showing `METHOD  url`
//! plus right-aligned indicators (auth kind, placeholder count, unsaved dot).
//!
//! Focusable: it joins the Tab cycle / jump-mode, edits its URL inline
//! (via a [`LineEditor`]), and switches the method (cycle key / menu).
//!
//! [`LineEditor`]: crate::tui::components::line_editor::LineEditor

use churl_core::model::{Auth, Body, PartValue, Request};
use edtui::{EditorState, EditorTheme, EditorView};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Widget};

use super::line_editor::LineEditor;
use crate::tui::theme::Theme;

/// Height of the URL bar in rows (border top + content + border bottom).
pub const HEIGHT: u16 = 3;

/// What [`render`] needs beyond the request: focus, an optional inline editor, and
/// the derived dirty state.
pub struct UrlBarCtx<'a> {
    /// The selected request, or `None` for the empty state.
    pub request: Option<&'a Request>,
    /// Whether the URL bar pane is focused.
    pub focused: bool,
    /// The inline URL editor, present while editing.
    pub editor: Option<&'a mut LineEditor>,
    /// Whether the loaded endpoint has unsaved changes (drives the `●` dot).
    pub dirty: bool,
    /// Jump-mode label for the URL bar, if assigned.
    pub jump_label: Option<char>,
}

/// Counts the number of `{{...}}` placeholder occurrences in `s`.
fn count_placeholders(s: &str) -> usize {
    let mut count = 0;
    let mut rest = s;
    while let Some(open) = rest.find("{{") {
        rest = &rest[open + 2..];
        if let Some(close) = rest.find("}}") {
            count += 1;
            rest = &rest[close + 2..];
        } else {
            break;
        }
    }
    count
}

/// Count total placeholders across a request's URL, header values, param values,
/// body content, and auth fields.
fn request_placeholder_count(request: &Request) -> usize {
    let mut total = count_placeholders(&request.url);
    for header in &request.headers {
        if header.enabled {
            total += count_placeholders(&header.value);
        }
    }
    for param in &request.params {
        if param.enabled {
            total += count_placeholders(&param.value);
        }
    }
    if let Some(body) = &request.body {
        match body {
            Body::Simple { content, .. } => total += count_placeholders(content),
            Body::Multipart(parts) => {
                for part in parts {
                    total += count_placeholders(&part.name);
                    match &part.value {
                        PartValue::Text(text) => total += count_placeholders(text),
                        PartValue::File { path, filename, .. } => {
                            total += count_placeholders(path);
                            if let Some(filename) = filename {
                                total += count_placeholders(filename);
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(auth) = &request.auth {
        match auth {
            Auth::Basic { username, password } => {
                total += count_placeholders(username);
                total += count_placeholders(password);
            }
            Auth::Bearer { token } => {
                total += count_placeholders(token);
            }
            Auth::ApiKey { value, .. } => {
                total += count_placeholders(value);
            }
        }
    }
    total
}

/// The short auth-kind indicator string, e.g. `auth:basic`.
fn auth_indicator(auth: &Auth) -> &'static str {
    match auth {
        Auth::Basic { .. } => "auth:basic",
        Auth::Bearer { .. } => "auth:bearer",
        Auth::ApiKey { .. } => "auth:apikey",
    }
}

/// Renders the URL bar.
pub fn render(frame: &mut Frame, area: Rect, ctx: UrlBarCtx, theme: &Theme) {
    let (border_type, border_style) = if ctx.focused {
        (BorderType::Thick, theme.border_focused)
    } else {
        (BorderType::Plain, theme.border_unfocused)
    };
    let title = match ctx.jump_label {
        Some(label) => format!(" URL [{label}] "),
        None => " URL ".to_owned(),
    };
    let block = Block::bordered()
        .border_type(border_type)
        .border_style(border_style)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(request) = ctx.request else {
        frame.render_widget(
            Paragraph::new(Line::styled("no endpoint selected", theme.border_unfocused)),
            inner,
        );
        return;
    };

    // Build indicator spans (right-aligned, space-separated). The unsaved dot
    // sits first so it stays visible when the bar is narrow; it carries the
    // theme accent (same steady accent as the statusline unsaved marker).
    let dim = Style::default().fg(theme
        .border_unfocused
        .fg
        .unwrap_or(ratatui::style::Color::DarkGray));
    let mut pieces: Vec<(String, Style)> = Vec::new();
    if ctx.dirty {
        pieces.push(("●".to_owned(), theme.accent));
    }
    // Loud per-endpoint durable insecure-TLS marker: steady error style so a saved
    // verify-off endpoint is visible at rest here, alongside the session-effective
    // statusline flag.
    if request.insecure {
        pieces.push(("⚠ insecure".to_owned(), theme.status_error));
    }
    if let Some(auth) = &request.auth {
        pieces.push((auth_indicator(auth).to_owned(), dim));
    }
    let n = request_placeholder_count(request);
    if n > 0 {
        pieces.push((format!("{{{{{n}}}}}"), dim));
    }
    let indicator_str = pieces
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join("  ");

    // Split the inner area into left (method+url) and right (indicators) first,
    // so the inline editor's horizontal viewport can follow the cursor within the
    // real available width (typing past the edge must not go blind).
    let (left_area, right_area) = if indicator_str.is_empty() {
        (inner, None)
    } else {
        let indicator_width = indicator_str.len() as u16;
        let right_width = (indicator_width + 1).min(inner.width);
        let left_width = inner.width.saturating_sub(right_width);
        let [left, right] = Layout::horizontal([
            Constraint::Length(left_width),
            Constraint::Length(right_width),
        ])
        .areas(inner);
        (left, Some(right))
    };

    // When editing, render the method + the viewport-scrolled editor text with a
    // block cursor and `…` edge indicators; otherwise the static URL.
    let method_prefix = format!("{}  ", request.method);
    let method_line = match ctx.editor {
        Some(editor) => {
            // Reserve the method prefix + a cell for each edge indicator.
            let avail = (left_area.width as usize).saturating_sub(method_prefix.len() + 2);
            let view = editor.view(avail.max(1));
            let mut spans = vec![Span::raw(method_prefix)];
            if view.clipped_left {
                spans.push(Span::raw("…"));
            }
            let chars: Vec<char> = view.text.chars().collect();
            let mut s = String::new();
            for (col, c) in chars.iter().enumerate() {
                if col == view.cursor_col {
                    s.push('█');
                }
                s.push(*c);
            }
            if view.cursor_col >= chars.len() {
                s.push('█');
            }
            spans.push(Span::raw(s));
            if view.clipped_right {
                spans.push(Span::raw("…"));
            }
            Line::from(spans)
        }
        None => Line::from(format!("{method_prefix}{}", request.url)),
    };

    frame.render_widget(Paragraph::new(method_line), left_area);
    if let Some(right_area) = right_area {
        let mut spans: Vec<Span> = Vec::new();
        for (i, (text, style)) in pieces.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(text.clone(), *style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), right_area);
    }
}

/// Renders the centered vim-popup URL editor: an edtui editor
/// seeded with the URL. edtui's own status line shows the vim mode, so the
/// footer carries only the commit/cancel hints, bottom-right (review round 3).
/// Enter commits, Esc (in normal mode) cancels.
pub fn render_popup(frame: &mut Frame, area: Rect, editor: &mut EditorState, theme: &Theme) {
    let [modal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Length(4)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(" Edit URL ")
        .title_style(theme.title)
        .title_bottom(Line::from(" enter commit · esc cancel ").right_aligned());
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    let editor_theme = EditorTheme::default().base(Style::default());
    EditorView::new(editor)
        .theme(editor_theme)
        .wrap(false)
        .render(inner, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_count_in_url() {
        assert_eq!(count_placeholders("https://{{host}}/users/{{id}}"), 2);
        assert_eq!(count_placeholders("https://example.com"), 0);
        assert_eq!(count_placeholders("{{a}}{{b}}{{c}}"), 3);
        // Unclosed brace — not a placeholder.
        assert_eq!(count_placeholders("{{unclosed"), 0);
    }

    fn render_indicators(insecure: bool) -> String {
        use churl_core::model::Method;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let request = Request {
            method: Method::Get,
            url: "https://api.test/x".to_owned(),
            headers: Vec::new(),
            params: Vec::new(),
            body: None,
            auth: None,
            insecure,
        };
        let theme = Theme::dark();
        let backend = TestBackend::new(60, HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    Rect::new(0, 0, 60, HEIGHT),
                    UrlBarCtx {
                        request: Some(&request),
                        focused: false,
                        editor: None,
                        dirty: false,
                        jump_label: None,
                    },
                    &theme,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..HEIGHT {
            for x in 0..60 {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        text
    }

    #[test]
    fn per_endpoint_insecure_shows_marker() {
        // A saved verify-off endpoint carries a visible "insecure" marker; a secure
        // one shows nothing of the sort.
        assert!(render_indicators(true).contains("insecure"));
        assert!(!render_indicators(false).contains("insecure"));
    }
}
