//! Request pane: a tab bar (Params / Headers / Auth / Body) over the active
//! tab's content. Params/Headers/Auth are row-list editors; Body is the edtui
//! editor. Editing is driven by `app.rs`; this module only renders the state.

use churl_core::config::{is_template_placeholder, looks_like_secret_name};
use churl_core::model::{ApiKeyPlacement, Auth, Header, Param, Request};
use edtui::{EditorState, EditorTheme, EditorView};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget};

use super::jump::JumpState;
use super::request_tabs::{EditField, FieldEdit, RequestTab, RequestTabs};
use crate::tui::theme::Theme;

/// Everything [`render`] needs.
pub struct RenderCtx<'a> {
    /// The selected request, or `None` for the empty state.
    pub request: Option<&'a Request>,
    /// The edtui body editor (rendered on the Body tab).
    pub editor: &'a mut EditorState,
    /// Tab state (active tab, per-tab selection, in-progress edit). Mutable so the
    /// row-list vertical scroll offset can be adjusted to keep the selection in
    /// view (mirrors the explorer).
    pub tabs: &'a mut RequestTabs,
    /// Whether the request pane is focused.
    pub focused: bool,
    /// The colour theme.
    pub theme: &'a Theme,
    /// Jump-mode state (for the pane label).
    pub jump: Option<&'a JumpState>,
}

/// Renders the request pane: tab bar + active tab content.
pub fn render(frame: &mut Frame, area: Rect, ctx: RenderCtx) {
    let RenderCtx {
        request,
        editor,
        tabs,
        focused,
        theme,
        jump,
    } = ctx;

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

    // Tab bar on top, content below.
    let [tabbar_area, content_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(Paragraph::new(tab_bar(request, tabs, theme)), tabbar_area);

    let height = content_area.height as usize;
    match tabs.active {
        RequestTab::Params => {
            let n = request.params.len();
            let offset = tabs.scroll_to_fit(n, height);
            let mut rows = param_rows(&request.params, tabs, focused, theme);
            if offset < rows.len() {
                rows.drain(..offset);
            }
            frame.render_widget(Paragraph::new(rows), content_area);
        }
        RequestTab::Headers => {
            let n = request.headers.len();
            let offset = tabs.scroll_to_fit(n, height);
            let mut rows = header_rows(&request.headers, tabs, focused, theme);
            if offset < rows.len() {
                rows.drain(..offset);
            }
            frame.render_widget(Paragraph::new(rows), content_area);
        }
        RequestTab::Auth => {
            let rows = auth_rows(request.auth.as_ref(), tabs, focused, theme);
            frame.render_widget(Paragraph::new(rows), content_area);
        }
        RequestTab::Body => {
            let editor_theme = EditorTheme::default().base(Style::default());
            EditorView::new(editor)
                .theme(editor_theme)
                .wrap(true)
                .render(content_area, frame.buffer_mut());
        }
    }
}

/// The tab-bar line with the active tab highlighted and per-tab row counts.
fn tab_bar<'a>(request: &Request, tabs: &RequestTabs, theme: &Theme) -> Line<'a> {
    let counts = |tab: RequestTab| match tab {
        RequestTab::Params => Some(request.params.len()),
        RequestTab::Headers => Some(request.headers.len()),
        RequestTab::Auth => request.auth.as_ref().map(|_| 1),
        RequestTab::Body => None,
    };
    let mut spans: Vec<Span> = Vec::new();
    for (i, tab) in RequestTab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let label = match counts(*tab) {
            Some(n) => format!(" {}({n}) ", tab.label()),
            None => format!(" {} ", tab.label()),
        };
        let style = if *tab == tabs.active {
            theme.selection
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

/// The block-cursor rendering of an in-progress field editor's text.
fn cursor_text(edit: &FieldEdit) -> String {
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

/// Whether row `i` is the selected row on the active tab (and the pane is focused).
fn is_selected(tabs: &RequestTabs, i: usize, focused: bool) -> bool {
    focused && tabs.editing.is_none() && tabs.selection() == i
}

/// Renders the value of a row field, substituting the live editor when this
/// row/field is being edited.
fn field_display(tabs: &RequestTabs, row: usize, field: EditField, value: &str) -> String {
    match &tabs.editing {
        Some(edit) if edit.row == row && edit.field == field => cursor_text(edit),
        _ => value.to_owned(),
    }
}

/// The enabled marker glyph.
fn enabled_marker(enabled: bool) -> &'static str {
    if enabled { "✓" } else { "✗" }
}

fn param_rows<'a>(
    params: &[Param],
    tabs: &RequestTabs,
    focused: bool,
    theme: &Theme,
) -> Vec<Line<'a>> {
    if params.is_empty() {
        return vec![Line::styled(
            "  (no params — a to add)",
            theme.border_unfocused,
        )];
    }
    params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = field_display(tabs, i, EditField::Name, &p.name);
            let value = field_display(tabs, i, EditField::Value, &p.value);
            let text = format!("{} {name} = {value}", enabled_marker(p.enabled));
            row_line(text, is_selected(tabs, i, focused), p.enabled, theme)
        })
        .collect()
}

fn header_rows<'a>(
    headers: &[Header],
    tabs: &RequestTabs,
    focused: bool,
    theme: &Theme,
) -> Vec<Line<'a>> {
    if headers.is_empty() {
        return vec![Line::styled(
            "  (no headers — a to add)",
            theme.border_unfocused,
        )];
    }
    headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let name = field_display(tabs, i, EditField::Name, &h.name);
            let value = field_display(tabs, i, EditField::Value, &h.value);
            let text = format!("{} {name}: {value}", enabled_marker(h.enabled));
            row_line(text, is_selected(tabs, i, focused), h.enabled, theme)
        })
        .collect()
}

/// The Auth tab rows: a kind row (row 0) plus the kind's fields. Secret-valued
/// fields render masked when literal (the same rule the request meta used to).
fn auth_rows<'a>(
    auth: Option<&Auth>,
    tabs: &RequestTabs,
    focused: bool,
    theme: &Theme,
) -> Vec<Line<'a>> {
    let mut rows: Vec<Line> = Vec::new();
    let kind = match auth {
        None => "None",
        Some(Auth::Basic { .. }) => "Basic",
        Some(Auth::Bearer { .. }) => "Bearer",
        Some(Auth::ApiKey { .. }) => "ApiKey",
    };
    rows.push(row_line(
        format!("kind: {kind}  (enter to change)"),
        is_selected(tabs, 0, focused),
        true,
        theme,
    ));
    match auth {
        None => {}
        Some(Auth::Basic { username, password }) => {
            let u = field_display(tabs, 1, EditField::Value, username);
            rows.push(row_line(
                format!("username: {u}"),
                is_selected(tabs, 1, focused),
                true,
                theme,
            ));
            let p = auth_value_display(tabs, 2, password, true);
            rows.push(row_line(
                format!("password: {p}"),
                is_selected(tabs, 2, focused),
                true,
                theme,
            ));
        }
        Some(Auth::Bearer { token }) => {
            let t = auth_value_display(tabs, 1, token, true);
            rows.push(row_line(
                format!("token: {t}"),
                is_selected(tabs, 1, focused),
                true,
                theme,
            ));
        }
        Some(Auth::ApiKey {
            name,
            value,
            placement,
        }) => {
            let n = field_display(tabs, 1, EditField::Value, name);
            rows.push(row_line(
                format!("name: {n}"),
                is_selected(tabs, 1, focused),
                true,
                theme,
            ));
            let v = auth_value_display(tabs, 2, value, looks_like_secret_name(name));
            rows.push(row_line(
                format!("value: {v}"),
                is_selected(tabs, 2, focused),
                true,
                theme,
            ));
            let place = match placement {
                ApiKeyPlacement::Header => "header",
                ApiKeyPlacement::Query => "query",
            };
            rows.push(row_line(
                format!("placement: {place}  (space/enter to toggle)"),
                is_selected(tabs, 3, focused),
                true,
                theme,
            ));
        }
    }
    rows
}

/// Renders a possibly-secret auth value: the live editor when being edited;
/// otherwise the literal (masked to `*****` when `secret` and not a placeholder).
fn auth_value_display(tabs: &RequestTabs, row: usize, value: &str, secret: bool) -> String {
    if let Some(edit) = &tabs.editing
        && edit.row == row
    {
        return cursor_text(edit);
    }
    if secret && !is_template_placeholder(value) && !value.is_empty() {
        "*****".to_owned()
    } else {
        value.to_owned()
    }
}

/// Wraps a row's text in the selection / dim styling.
fn row_line<'a>(text: String, selected: bool, enabled: bool, theme: &Theme) -> Line<'a> {
    let prefix = if selected { "> " } else { "  " };
    let line = Line::from(format!("{prefix}{text}"));
    if selected {
        line.style(theme.selection)
    } else if !enabled {
        line.style(Style::default().add_modifier(Modifier::DIM))
    } else {
        line
    }
}
