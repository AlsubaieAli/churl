//! URL bar: a slim display-only strip above the Request pane showing
//! `METHOD  url` plus right-aligned indicators (auth kind, placeholder count).
//!
//! Display-only in M6.5 — not focusable/editable; that lands in M6.6 (focusable
//! bar, inline URL editing, method switching). The bar is always 3 lines tall
//! (1 border top + 1 content line + 1 border bottom).

use churl_core::model::{Auth, Request};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::tui::theme::Theme;

/// Height of the URL bar in rows (border top + content + border bottom).
pub const HEIGHT: u16 = 3;

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
        total += count_placeholders(&body.content);
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

/// Renders the URL bar. Takes the selected `Request` (or `None` for empty state).
pub fn render(frame: &mut Frame, area: Rect, request: Option<&Request>, theme: &Theme) {
    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(theme.border_unfocused)
        .title(" URL ")
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(request) = request else {
        frame.render_widget(
            Paragraph::new(Line::styled("no endpoint selected", theme.border_unfocused)),
            inner,
        );
        return;
    };

    // Build indicator spans (right-aligned, space-separated).
    let mut indicators: Vec<String> = Vec::new();
    if let Some(auth) = &request.auth {
        indicators.push(auth_indicator(auth).to_owned());
    }
    let n = request_placeholder_count(request);
    if n > 0 {
        indicators.push(format!("{{{{{n}}}}}"));
    }
    let indicator_str = if indicators.is_empty() {
        String::new()
    } else {
        indicators.join("  ")
    };

    let method_and_url = format!("{}  {}", request.method, request.url);

    // Split the inner area into left (method+url) and right (indicators).
    if indicator_str.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(method_and_url)), inner);
    } else {
        let indicator_width = indicator_str.len() as u16;
        // Reserve space for the indicator plus 1 space gap; clamp to inner width.
        let right_width = (indicator_width + 1).min(inner.width);
        let left_width = inner.width.saturating_sub(right_width);
        let [left_area, right_area] = Layout::horizontal([
            Constraint::Length(left_width),
            Constraint::Length(right_width),
        ])
        .areas(inner);

        frame.render_widget(Paragraph::new(Line::from(method_and_url)), left_area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                indicator_str,
                Style::default().fg(theme
                    .border_unfocused
                    .fg
                    .unwrap_or(ratatui::style::Color::DarkGray)),
            )])),
            right_area,
        );
    }
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
}
