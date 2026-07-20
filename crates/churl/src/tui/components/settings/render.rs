//! Render/draw fns for the Settings panel: the modal frame, the category menu,
//! each category's rows, the cookie/advanced lists, and the footer hints. Kept
//! a child module of `settings` so it keeps full access to the state's private
//! fields with no visibility widening.

use churl_core::config::{RedirectPolicy, UrlEditMode};
use churl_core::secrets::SecretPolicy;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::{
    AdvancedField, AppearanceRow, DebugRow, LoadRow, NetworkRow, PanelFocus, RequestRow,
    SettingKey, SettingsCategory, SettingsLevel, SettingsState, mask_proxy, mask_proxy_password,
};
use crate::tui::theme::Theme;

/// The dirty dot's actual predicate (FIX 2): touched AND differs from disk —
/// NOT a bare value comparison. A knob the panel merely displays (CLI `-k`,
/// a workspace override, an already-saved global default) must never show
/// dirty just because it differs from `persisted`; only an ACTUAL panel edit
/// this session can. Keeps Save and the dots perfectly consistent — a dot is
/// lit if and only if Save would write something for that knob.
fn dirty(state: &SettingsState, key: SettingKey, differs: bool) -> bool {
    state.touched.contains(&key) && differs
}

/// Renders the Settings panel over `area`.
pub fn render(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    // A centered modal, comfortably narrower than the env editor.
    let [modal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let title = match state.level {
        SettingsLevel::Menu => " Settings ".to_owned(),
        SettingsLevel::Panel => format!(" Settings · {} ", state.category.label()),
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match state.level {
        SettingsLevel::Menu => render_menu(frame, inner, state, theme),
        SettingsLevel::Panel => render_panel(frame, inner, state, theme),
    }
}

/// Renders the category menu (level 1).
fn render_menu(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let [list_area, msg_row, hint_row] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    let lines: Vec<Line> = SettingsCategory::visible(state.debug_enabled)
        .into_iter()
        .map(|category| {
            let selected = category == state.category;
            let marker = if selected { "▸ " } else { "  " };
            let l = Line::from(format!("{marker}{}", category.label()));
            if selected {
                l.style(theme.selection)
            } else {
                l
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_area);

    // A `s` (save) failure can fire from the menu level too (e.g. a
    // credentialed proxy refused on write) — shown loud, same as the panel's.
    let top = match &state.message {
        Some(msg) => Line::styled(format!(" {msg}"), theme.status_error),
        None => Line::from(""),
    };
    frame.render_widget(Paragraph::new(top), msg_row);
    frame.render_widget(
        Paragraph::new(Line::styled(
            " j/k move · enter/l open · s save · q close",
            theme.statusline,
        )),
        hint_row,
    );
}

/// Renders the open category panel (level 2): its rows, plus a detail area
/// (cookie list / advanced list) when relevant, plus the footer.
fn render_panel(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let has_detail = matches!(
        (state.category, state.focus),
        (SettingsCategory::Network, _) | (SettingsCategory::Debug, PanelFocus::AdvancedList)
    );
    let rows_height = category_rows_height(state);
    let [rows_area, detail_area, footer] = if has_detail {
        Layout::vertical([
            Constraint::Length(rows_height),
            Constraint::Fill(1),
            Constraint::Length(2),
        ])
        .areas(area)
    } else {
        let [rows_area, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(area);
        [rows_area, Rect::default(), footer]
    };

    match state.category {
        SettingsCategory::Request => render_request_rows(frame, rows_area, state, theme),
        SettingsCategory::Network => render_network_rows(frame, rows_area, state, theme),
        SettingsCategory::Load => render_load_rows(frame, rows_area, state, theme),
        SettingsCategory::Appearance => render_appearance_rows(frame, rows_area, state, theme),
        SettingsCategory::Debug => render_debug_rows(frame, rows_area, state, theme),
    }

    if state.category == SettingsCategory::Network {
        render_cookie_list(frame, detail_area, state, theme);
    } else if state.category == SettingsCategory::Debug && state.focus == PanelFocus::AdvancedList {
        render_advanced_list(frame, detail_area, state, theme);
    }

    render_footer(frame, footer, state, theme);
}

/// How many terminal rows the active category's row list needs.
fn category_rows_height(state: &SettingsState) -> u16 {
    match state.category {
        SettingsCategory::Request => 5,
        SettingsCategory::Network => 3,
        SettingsCategory::Load => LoadRow::ALL.len() as u16,
        SettingsCategory::Appearance => 2,
        SettingsCategory::Debug => 2,
    }
}

/// A single row line, shared rendering shape across every category: a
/// selection marker, a best-effort dirty dot (M8.5 Wave 3 — mirrors the URL
/// bar's `●` convention: accent-coloured, shown when the working value
/// differs from what's on disk), a fixed-width label, then the value span.
fn row_line(
    label: &str,
    value: Span<'static>,
    selected: bool,
    dirty: bool,
    theme: &Theme,
) -> Line<'static> {
    let marker = if selected { "▸ " } else { "  " };
    let dot = if dirty {
        Span::styled("●", theme.accent)
    } else {
        Span::raw(" ")
    };
    let l = Line::from(vec![
        Span::raw(marker),
        dot,
        Span::raw(format!(" {label:<18}")),
        value,
    ]);
    if selected {
        l.style(theme.selection)
    } else {
        l
    }
}

/// The live text of the open editor for `target`, with a trailing cursor
/// block — or `None` when no edit targeting `target` is open.
fn editing_text(state: &SettingsState, target: super::EditTarget) -> Option<String> {
    state
        .editing
        .as_ref()
        .filter(|(t, _)| *t == target)
        .map(|(_, editor)| format!("{}█", editor.text()))
}

fn redirect_label(policy: RedirectPolicy) -> &'static str {
    match policy {
        RedirectPolicy::Strip => "strip (default)",
        RedirectPolicy::Strict => "strict",
        RedirectPolicy::FollowAll => "follow-all",
    }
}

fn url_edit_label(mode: UrlEditMode) -> &'static str {
    match mode {
        UrlEditMode::Inline => "inline",
        UrlEditMode::Popup => "popup",
    }
}

fn secret_policy_label(policy: SecretPolicy) -> &'static str {
    match policy {
        SecretPolicy::Strict => "strict",
        SecretPolicy::Warn => "warn",
    }
}

fn render_request_rows(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let sel = |row: RequestRow| state.focus == PanelFocus::Rows && state.request_row == row;
    let timeout_display = editing_text(
        state,
        super::EditTarget::Advanced(AdvancedField::TimeoutSecs),
    )
    .unwrap_or_else(|| format!("{}", state.advanced.timeout_secs));
    let body_display = editing_text(
        state,
        super::EditTarget::Advanced(AdvancedField::BodyCapBytes),
    )
    .unwrap_or_else(|| format!("{}", state.advanced.body_cap_bytes));

    let lines = vec![
        row_line(
            RequestRow::Timeout.label(),
            Span::raw(timeout_display),
            sel(RequestRow::Timeout),
            dirty(
                state,
                SettingKey::Timeout,
                state.advanced.timeout_secs != state.persisted.timeout_secs,
            ),
            theme,
        ),
        row_line(
            RequestRow::MaxBodyBytes.label(),
            Span::raw(body_display),
            sel(RequestRow::MaxBodyBytes),
            dirty(
                state,
                SettingKey::MaxBodyBytes,
                state.advanced.body_cap_bytes != state.persisted.max_body_bytes,
            ),
            theme,
        ),
        row_line(
            RequestRow::Redirect.label(),
            Span::raw(redirect_label(state.redirect)),
            sel(RequestRow::Redirect),
            dirty(
                state,
                SettingKey::Redirect,
                state.redirect != state.persisted.redirect,
            ),
            theme,
        ),
        row_line(
            RequestRow::UrlEdit.label(),
            Span::raw(url_edit_label(state.url_edit)),
            sel(RequestRow::UrlEdit),
            dirty(
                state,
                SettingKey::UrlEdit,
                state.url_edit != state.persisted.url_edit,
            ),
            theme,
        ),
        row_line(
            RequestRow::SecretPolicy.label(),
            Span::raw(secret_policy_label(state.secret_policy)),
            sel(RequestRow::SecretPolicy),
            dirty(
                state,
                SettingKey::SecretPolicy,
                state.secret_policy != state.persisted.secret_policy,
            ),
            theme,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_network_rows(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let rows_focused = state.focus == PanelFocus::Rows;
    let sel = |row: NetworkRow| rows_focused && state.network_row == row;

    let proxy_display = if let Some(text) = editing_text(state, super::EditTarget::Proxy) {
        // The password segment stays masked even mid-edit.
        let masked = mask_proxy_password(text.trim_end_matches('█'));
        format!("{masked}█")
    } else {
        match &state.proxy {
            Some(p) => mask_proxy(p),
            None => "(none — uses HTTP(S)_PROXY env)".to_owned(),
        }
    };
    let insecure_display = if state.insecure {
        "OFF — certificates NOT verified"
    } else {
        "on"
    };
    let cookies_display = if state.cookies_enabled { "on" } else { "off" };
    let insecure_span = if state.insecure {
        Span::styled(insecure_display.to_owned(), theme.status_error)
    } else {
        Span::raw(insecure_display.to_owned())
    };

    let lines = vec![
        row_line(
            NetworkRow::Proxy.label(),
            Span::raw(proxy_display),
            sel(NetworkRow::Proxy),
            dirty(
                state,
                SettingKey::Proxy,
                state.proxy != state.persisted.proxy,
            ),
            theme,
        ),
        row_line(
            NetworkRow::Tls.label(),
            insecure_span,
            sel(NetworkRow::Tls),
            dirty(
                state,
                SettingKey::Insecure,
                state.insecure != state.persisted.insecure,
            ),
            theme,
        ),
        row_line(
            NetworkRow::Cookies.label(),
            Span::raw(cookies_display.to_owned()),
            sel(NetworkRow::Cookies),
            dirty(
                state,
                SettingKey::Cookies,
                state.cookies_enabled != state.persisted.cookies,
            ),
            theme,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_load_rows(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let sel = |row: LoadRow| state.focus == PanelFocus::Rows && state.load_row == row;
    let lines: Vec<Line> = LoadRow::ALL
        .iter()
        .map(|&row| {
            let display = editing_text(state, super::EditTarget::Load(row))
                .unwrap_or_else(|| row.get(&state.load_caps).to_string());
            let differs = row.get(&state.load_caps) != row.get(&state.persisted.load_caps);
            let is_dirty = dirty(state, row.setting_key(), differs);
            row_line(row.label(), Span::raw(display), sel(row), is_dirty, theme)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_appearance_rows(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let sel = |row: AppearanceRow| state.focus == PanelFocus::Rows && state.appearance_row == row;
    let leader_display = editing_text(state, super::EditTarget::LeaderKey)
        .unwrap_or_else(|| state.leader_key.clone());
    let lines = vec![
        row_line(
            AppearanceRow::Theme.label(),
            Span::raw(state.theme_name.clone()),
            sel(AppearanceRow::Theme),
            dirty(
                state,
                SettingKey::Theme,
                state.theme_name != state.persisted.theme,
            ),
            theme,
        ),
        row_line(
            AppearanceRow::LeaderKey.label(),
            Span::raw(leader_display),
            sel(AppearanceRow::LeaderKey),
            dirty(
                state,
                SettingKey::LeaderKey,
                state.leader_key != state.persisted.leader_key,
            ),
            theme,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_debug_rows(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let sel = |row: DebugRow| state.focus == PanelFocus::Rows && state.debug_row == row;
    let debug_display = if state.debug_enabled { "on" } else { "off" };
    let lines = vec![
        row_line(
            DebugRow::DebugToggle.label(),
            Span::raw(debug_display.to_owned()),
            sel(DebugRow::DebugToggle),
            dirty(
                state,
                SettingKey::Debug,
                state.debug_enabled != state.persisted.debug,
            ),
            theme,
        ),
        row_line(
            DebugRow::Advanced.label(),
            Span::raw("↳ concurrency/total/body-cap/timeout"),
            sel(DebugRow::Advanced),
            false,
            theme,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

/// Renders the Advanced field list beneath Debug's Advanced row: the four
/// override knobs, their current resolved values, and the in-progress edit
/// when one is open. Ported verbatim from the old Options overlay.
fn render_advanced_list(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let focused = state.focus == PanelFocus::AdvancedList;
    let border = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let block = Block::bordered()
        .border_style(border)
        .title(" Advanced limits (debug-gated) ")
        .title_style(theme.title);
    let list_area = block.inner(area);
    frame.render_widget(block, area);
    if list_area.width == 0 || list_area.height == 0 {
        return;
    }

    let lines: Vec<Line> = AdvancedField::ALL
        .iter()
        .map(|&field| {
            let selected = focused && state.advanced_field == field;
            let value = editing_text(state, super::EditTarget::Advanced(field))
                .unwrap_or_else(|| field.get(&state.advanced).to_string());
            let differs = field.get(&state.advanced) != field.get(&state.persisted.advanced);
            let is_dirty = dirty(state, field.setting_key(), differs);
            row_line(field.label(), Span::raw(value), selected, is_dirty, theme)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// Renders the cookie list beneath Network's Cookies row (domain · name ·
/// masked value). Ported verbatim from the old Options overlay.
fn render_cookie_list(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let focused = state.focus == PanelFocus::CookieList;
    let border = if focused {
        theme.border_focused
    } else {
        theme.border_unfocused
    };
    let block = Block::bordered()
        .border_style(border)
        .title(" Cookies ")
        .title_style(theme.title);
    let list_area = block.inner(area);
    frame.render_widget(block, area);
    if list_area.width == 0 || list_area.height == 0 {
        return;
    }

    if state.cookies.is_empty() {
        let hint = if state.cookies_enabled {
            "(no cookies yet — they accumulate as you send requests)"
        } else {
            "(cookie jar is off — enable it on the Cookies row)"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(hint, theme.auth_mask)),
            list_area,
        );
        return;
    }

    let name_col = state
        .cookies
        .iter()
        .map(|c| c.domain.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(6, 28);

    let visible = list_area.height as usize;
    let offset = state.cookie_sel.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = state
        .cookies
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, c)| {
            let selected = i == state.cookie_sel && focused;
            // Cookie values are credential-shaped — always masked in the TUI.
            let text = format!(
                "{domain:<name_col$}  {name}  ••••••",
                domain = c.domain,
                name = c.name,
            );
            let l = Line::from(text);
            if selected {
                l.style(theme.selection)
            } else {
                l
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// Renders the footer: a live message row plus the key hints for the current
/// level/focus.
fn render_footer(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let [msg_row, hint_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let top = match &state.message {
        Some(msg) => Line::styled(format!(" {msg}"), theme.status_error),
        None => Line::from(""),
    };
    frame.render_widget(Paragraph::new(top), msg_row);

    let hints = if state.editing.is_some() {
        "enter apply · esc cancel".to_owned()
    } else {
        match state.focus {
            PanelFocus::Rows if state.category == SettingsCategory::Network => {
                "j/k move · enter edit/toggle · l cookies · s save · esc menu · q close".to_owned()
            }
            PanelFocus::Rows => {
                "j/k move · enter edit/toggle/cycle · s save · esc menu · q close".to_owned()
            }
            PanelFocus::CookieList => {
                "j/k move · d delete · x clear all · s save · h/esc back · q close".to_owned()
            }
            PanelFocus::AdvancedList => {
                "j/k move · enter edit · s save · h/esc back · q close".to_owned()
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {hints}"), theme.statusline)),
        hint_row,
    );
}
