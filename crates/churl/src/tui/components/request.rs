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
use super::tab_strip::{CHIP_OVERHEAD, chip_window};
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

    let [tabbar_area, content_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(tab_bar(
            request,
            tabs,
            theme,
            focused,
            tabbar_area.width as usize,
        )),
        tabbar_area,
    );

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

/// A one-line summary of the request pane for the collapsed (1-row) zoom stub.
/// Shown in the response pane's collapsed area when the request pane is zoomed.
pub fn collapsed_summary(
    request: Option<&Request>,
    tabs: &RequestTabs,
    theme: &Theme,
) -> Line<'static> {
    if request.is_none() {
        return Line::from("no endpoint selected");
    }
    // Collapsed = not focused, so no digit prefixes; the stub truncates at its
    // own edge (`usize::MAX` = never scroll — preserves the collapsed behaviour).
    tab_bar_line(request, tabs, theme, false, usize::MAX)
}

/// The tab-bar line with the active tab highlighted and per-tab row counts.
/// When `focused` is true, each tab is prefixed with its 1-based digit. `width`
/// is the columns the strip will occupy — when the chips overflow it, the bar
/// scrolls to keep the active chip visible (same logic as the top tab strip).
fn tab_bar<'a>(
    request: &Request,
    tabs: &RequestTabs,
    theme: &Theme,
    focused: bool,
    width: usize,
) -> Line<'a> {
    tab_bar_line(Some(request), tabs, theme, focused, width)
}

fn tab_bar_line(
    request: Option<&Request>,
    tabs: &RequestTabs,
    theme: &Theme,
    focused: bool,
    width: usize,
) -> Line<'static> {
    let counts = |tab: RequestTab| match (request, tab) {
        (Some(req), RequestTab::Params) => Some(req.params.len()),
        (Some(req), RequestTab::Headers) => Some(req.headers.len()),
        (Some(req), RequestTab::Auth) => req.auth.as_ref().map(|_| 1),
        _ => None,
    };
    // Build each tab's label first (padding, focused `[N]` prefix, `(n)` counts).
    let labels: Vec<String> = RequestTab::ALL
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let tab_label = tab.label();
            // Note #5: two-space internal padding (softer, roomier chip) now that
            // the hard `▐`/`▌` edge caps are gone; the gap between chips separates.
            if focused {
                // Prefix with 1-based digit when focused (tab-jump keys are live)
                match counts(*tab) {
                    Some(n) => format!("  [{}] {tab_label}({n})  ", i + 1),
                    None => format!("  [{}] {tab_label}  ", i + 1),
                }
            } else {
                match counts(*tab) {
                    Some(n) => format!("  {tab_label}({n})  "),
                    None => format!("  {tab_label}  "),
                }
            }
        })
        .collect();

    // Scroll to keep the ACTIVE chip fully visible when the chips overflow
    // `width`, mirroring the top tab strip: chip width = label + edges + gap.
    let active = RequestTab::ALL
        .iter()
        .position(|t| *t == tabs.active)
        .unwrap_or(0);
    let widths: Vec<usize> = labels
        .iter()
        .map(|l| l.chars().count() + CHIP_OVERHEAD)
        .collect();
    let (start, left_marker) = chip_window(&widths, active, width);

    // Each tab is a soft filled "chip" — the padded label on the chip bg — with a
    // single raw space gap between chips doing the separating (no `▐`/`▌` edge
    // caps, no `▏` bar). Active = bright `selection`; inactive =
    // the dim `tab_inactive` fill (both carry a real bg, so every chip reads as
    // filled, with stronger active-vs-inactive contrast).
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    if left_marker {
        spans.push(Span::styled("‹", theme.title));
        used += 1;
    }
    let mut clipped_right = false;
    for (i, tab) in RequestTab::ALL.iter().enumerate().skip(start) {
        // Reserve a column for the `›` marker when more tabs follow (incl. the
        // active one), so the exactly-full case still leaves room for the marker.
        let reserve = usize::from(i < RequestTab::ALL.len() - 1);
        if used + widths[i] + reserve > width {
            clipped_right = true;
            break;
        }
        // Active chip: the bright `selection` fill made BOLD for a stronger,
        // clearer active-vs-inactive contrast without touching the
        // shared `selection` slot (also used by explorer/picker rows).
        let style = if *tab == tabs.active {
            theme.selection.add_modifier(Modifier::BOLD)
        } else {
            theme.tab_inactive
        };
        spans.push(Span::styled(labels[i].clone(), style));
        spans.push(Span::raw(" "));
        used += widths[i];
    }
    if clipped_right && used < width {
        spans.push(Span::styled("›", theme.title));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Flattens a rendered tab-bar `Line` into its plain text (all span content).
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The active `Body` chip must render at every pane width — when it doesn't
    /// fit, the bar scrolls (dropping earlier chips behind a `‹` marker) so the
    /// active chip is never clipped off-screen.
    #[test]
    fn narrow_tab_bar_keeps_active_body_visible_with_marker() {
        let theme = Theme::dark();
        let mut tabs = RequestTabs::default();
        tabs.tab_jump(3); // Body
        // 20 cols cannot hold all four chips; Body is active and last.
        let line = tab_bar_line(None, &tabs, &theme, false, 20);
        let text = line_text(&line);
        assert!(text.contains("Body"), "active Body chip renders: {text:?}");
        assert!(
            text.contains('‹'),
            "clipped-left marker shows when scrolled: {text:?}"
        );
        // The active chip fits within the pane (rendered spans never exceed width).
        let painted: usize = text.chars().count();
        assert!(painted <= 20, "nothing paints past the pane: {painted}");
    }

    /// At a wide enough width all four chips render with NO edge markers — the
    /// window scroll engages only when the chips overflow.
    #[test]
    fn wide_tab_bar_shows_all_four_chips_no_markers() {
        let theme = Theme::dark();
        let tabs = RequestTabs::default(); // active = Params (first)
        let line = tab_bar_line(None, &tabs, &theme, false, 60);
        let text = line_text(&line);
        for label in ["Params", "Headers", "Auth", "Body"] {
            assert!(text.contains(label), "{label} chip renders: {text:?}");
        }
        assert!(
            !text.contains('‹') && !text.contains('›'),
            "no scroll markers when everything fits: {text:?}"
        );
    }

    /// The focused digit prefix `[N]` and `(n)` counts stay inside the chips; the
    /// active chip carries the bright `selection` bg and inactive chips the dim
    /// `tab_inactive` bg.
    #[test]
    fn chips_carry_active_and_inactive_backgrounds() {
        let theme = Theme::dark();
        let tabs = RequestTabs::default(); // active = Params
        let line = tab_bar_line(None, &tabs, &theme, true, 80);
        let text = line_text(&line);
        assert!(
            text.contains("[1]"),
            "focused digit prefix inside chip: {text:?}"
        );
        // The active (Params) chip spans use the bright selection style (bolded
        // for contrast); some inactive chip spans use the dim
        // tab_inactive style.
        let active_style = theme.selection.add_modifier(Modifier::BOLD);
        assert!(
            line.spans.iter().any(|s| s.style == active_style),
            "active chip uses the bright, bold selection bg"
        );
        assert!(
            line.spans.iter().any(|s| s.style == theme.tab_inactive),
            "inactive chips use the dim tab_inactive bg"
        );
    }

    /// The refined chips carry NO hard edge caps (`▐`/`▌`) and
    /// NO `▏` bar separator — a raw space gap separates them. Verified over the
    /// request tab bar; the top buffer strip shares the same render primitives.
    #[test]
    fn refined_chips_drop_caps_and_bar_separators() {
        let theme = Theme::dark();
        let tabs = RequestTabs::default();
        let line = tab_bar_line(None, &tabs, &theme, false, 60);
        let text = line_text(&line);
        for glyph in ['▐', '▌', '▏'] {
            assert!(
                !text.contains(glyph),
                "chip must not contain {glyph:?}: {text:?}"
            );
        }
        // A bare-space gap between chips does the separating: an inactive-styled
        // chip span is followed by an unstyled raw space span.
        let has_gap = line
            .spans
            .windows(2)
            .any(|w| w[0].style != Style::default() && w[1].content.as_ref() == " ");
        assert!(has_gap, "a raw-space gap separates chips: {text:?}");
        // The active chip fill is the bright `selection` style, bolded for a
        // stronger active-vs-inactive contrast.
        let active_style = theme.selection.add_modifier(Modifier::BOLD);
        assert!(
            line.spans.iter().any(|s| s.style == active_style),
            "active chip uses the bright, bold selection fill: {text:?}"
        );
    }
}
