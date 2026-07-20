//! Render/draw fns for the Settings panel: the modal frame, the category menu,
//! each category's rows, the cookie/advanced lists, and the footer hints. Kept
//! a child module of `settings` so it keeps full access to the state's private
//! fields with no visibility widening.

use churl_core::config::{RedirectPolicy, UrlEditMode};
use churl_core::cookies::SameSite;
use churl_core::secrets::SecretPolicy;
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::{
    AdvancedField, AppearanceRow, CookieFormField, DebugRow, LoadRow, NetworkRow, PanelFocus,
    RequestRow, SettingKey, SettingsCategory, SettingsLevel, SettingsState, format_body_cap,
    mask_proxy, mask_proxy_password,
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
    // The compact key-hint line lives on the block's bottom-right border
    // (M8.5.2) — freeing the footer's hint row for a per-setting description
    // (see `render_footer`/`setting_description`). Computed once here, before
    // the level/focus-specific body renders, since it depends on the SAME
    // state those renders read.
    let hint = footer_hint(state);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(title)
        .title_style(theme.title)
        .title_bottom(Line::from(format!(" {hint} ")).right_aligned());
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

/// Renders the category menu (level 1). The key hints live on the modal's
/// border (see `footer_hint`) — this footer is just the live message row.
fn render_menu(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let [list_area, msg_row] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

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
}

/// Renders the open category panel (level 2): its rows, plus a detail area
/// (cookie list / advanced list) when relevant, plus the footer.
fn render_panel(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    // The cookie form takes over the whole panel body — a 6-field form has no
    // natural "rows + detail" split, and it's a modal sub-task within the
    // modal panel, not another row list.
    if state.cookie_form.is_some() {
        let [body, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(2)]).areas(area);
        render_cookie_form(frame, body, state, theme);
        render_footer(frame, footer, state, theme);
        return;
    }

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
    .unwrap_or_else(|| format_body_cap(state.advanced.body_cap_bytes));

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
    let leader_display = if state.capturing_leader_key {
        "press a key… (tab: type a combo instead, esc: cancel)".to_owned()
    } else {
        editing_text(state, super::EditTarget::LeaderKey)
            .unwrap_or_else(|| state.leader_key.clone())
    };
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
            let value =
                editing_text(state, super::EditTarget::Advanced(field)).unwrap_or_else(|| {
                    let raw = field.get(&state.advanced);
                    if field == AdvancedField::BodyCapBytes {
                        format_body_cap(raw)
                    } else {
                        raw.to_string()
                    }
                });
            let differs = field.get(&state.advanced) != field.get(&state.persisted.advanced);
            let is_dirty = dirty(state, field.setting_key(), differs);
            row_line(field.label(), Span::raw(value), selected, is_dirty, theme)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_area);
}

/// Renders the cookie list beneath Network's Cookies row (domain · name ·
/// masked value · Secure · SameSite). Ported verbatim from the old Options
/// overlay, then extended (M8.5.1) with the Secure/SameSite columns.
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
            "(no cookies yet — they accumulate as you send requests, or a to add one)"
        } else {
            "(cookie jar is off — enable it above, or a to add one anyway)"
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
            let secure_mark = if c.secure { "✓" } else { "✗" };
            let text = format!(
                "{domain:<name_col$}  {name}  ••••••  secure:{secure_mark}  samesite:{same_site}",
                domain = c.domain,
                name = c.name,
                same_site = same_site_label(c.same_site),
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

/// Display label for a `SameSite` value — `—` for an absent attribute (not to
/// be confused with the RFC `SameSite=None` value, labeled `None`).
fn same_site_label(same_site: Option<SameSite>) -> &'static str {
    match same_site {
        None => "—",
        Some(SameSite::Strict) => "Strict",
        Some(SameSite::Lax) => "Lax",
        Some(SameSite::None) => "None",
    }
}

/// Renders the cookie add/edit form: one line per field, the focused field
/// marked like every other row list, an inline editor for the field
/// currently being typed into (mirrors [`editing_text`]'s shape but scoped to
/// the form's own `editing`), Secure as a checkbox, SameSite as its label.
/// The value field stays masked even while being edited — same stance as the
/// Proxy row's password segment (`mask_proxy_password`): a secret is never
/// shown in plaintext, including mid-type.
fn render_cookie_form(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let Some(form) = &state.cookie_form else {
        return;
    };
    let title = if form.editing_existing.is_some() {
        " Edit cookie "
    } else {
        " Add cookie "
    };
    let block = Block::bordered()
        .border_style(theme.border_focused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let editing_field = form.editing.as_ref().map(|_| form.focus);
    let field_line = |field: CookieFormField, value: Span<'static>| {
        row_line(field.label(), value, form.focus == field, false, theme)
    };

    let text_value = |field: CookieFormField| -> Span<'static> {
        if editing_field == Some(field) {
            let editor = form.editing.as_ref().expect("editing_field implies Some");
            Span::raw(format!("{}█", editor.text()))
        } else {
            Span::raw(form.text(field).to_owned())
        }
    };
    let masked_value = || -> Span<'static> {
        if editing_field == Some(CookieFormField::Value) {
            let editor = form.editing.as_ref().expect("editing_field implies Some");
            Span::raw(if editor.text().is_empty() {
                "█".to_owned()
            } else {
                "••••••█".to_owned()
            })
        } else if form.value.is_empty() {
            Span::raw("(empty)".to_owned())
        } else {
            Span::raw("••••••".to_owned())
        }
    };

    let lines = vec![
        field_line(CookieFormField::Domain, text_value(CookieFormField::Domain)),
        field_line(CookieFormField::Name, text_value(CookieFormField::Name)),
        field_line(CookieFormField::Value, masked_value()),
        field_line(CookieFormField::Path, text_value(CookieFormField::Path)),
        field_line(
            CookieFormField::Secure,
            Span::raw(if form.secure { "[x]" } else { "[ ]" }),
        ),
        field_line(
            CookieFormField::SameSite,
            Span::raw(same_site_label(form.same_site)),
        ),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The compact key-hint line for the modal's bottom-right border (M8.5.2:
/// moved off the footer to free it for a per-setting description — see
/// `render_footer`/`setting_description`). Covers both nav levels and every
/// panel focus/edit/capture/form sub-state, so the border always shows
/// exactly what the current keys do.
fn footer_hint(state: &SettingsState) -> String {
    match state.level {
        SettingsLevel::Menu => "j/k move · enter/l open · s save · q close".to_owned(),
        SettingsLevel::Panel => {
            if let Some(form) = &state.cookie_form {
                if form.editing.is_some() {
                    "enter apply · esc cancel".to_owned()
                } else {
                    "j/k move · enter edit/toggle/cycle · s save cookie · esc cancel".to_owned()
                }
            } else if state.capturing_leader_key {
                "press any key · tab type a combo · esc cancel".to_owned()
            } else if state.editing.is_some() {
                "enter apply · esc cancel".to_owned()
            } else {
                match state.focus {
                    PanelFocus::Rows if state.category == SettingsCategory::Network => {
                        "j/k move · J/K adjust · enter edit/toggle · a add · l cookies · s save \
                         · esc menu · q close"
                            .to_owned()
                    }
                    PanelFocus::Rows => "j/k move · J/K adjust · enter edit/toggle/cycle · s \
                                          save · esc menu · q close"
                        .to_owned(),
                    PanelFocus::CookieList => {
                        "j/k move · a add · e edit · d delete · x clear · s save · h/esc back \
                         · q close"
                            .to_owned()
                    }
                    PanelFocus::AdvancedList => {
                        "j/k move · J/K adjust · enter edit · s save · h/esc back · q close"
                            .to_owned()
                    }
                }
            }
        }
    }
}

/// A one-line description of the currently-hovered setting — what it does,
/// plus anything useful a value alone doesn't convey (units, "applies on
/// next launch", a security note). `None` when nothing meaningful is hovered
/// (the category menu, a form/list with its own detail, or mid-edit/-capture
/// where the description would just repeat what's already on screen).
fn setting_description(state: &SettingsState) -> Option<&'static str> {
    if state.cookie_form.is_some() || state.editing.is_some() || state.capturing_leader_key {
        return None;
    }
    let SettingsLevel::Panel = state.level else {
        return None;
    };
    match (state.category, state.focus) {
        (SettingsCategory::Request, PanelFocus::Rows) => Some(match state.request_row {
            RequestRow::Timeout => "How long to wait for a response before giving up, in seconds.",
            RequestRow::MaxBodyBytes => {
                "Maximum response body size to read; larger bodies are truncated. Shown/entered \
                 in MB or KB (e.g. 10MB) — stored as bytes."
            }
            RequestRow::Redirect => {
                "Cross-origin redirect handling: strip drops auth headers on a cross-origin hop \
                 (default), strict stops at the first cross-origin hop, follow-all never drops \
                 headers."
            }
            RequestRow::UrlEdit => {
                "What the URL bar's i/Enter opens: inline (type in place) or popup (a full-screen editor)."
            }
            RequestRow::SecretPolicy => {
                "How a save reacts to a literal secret found in the request: strict blocks it, \
                 warn lets it through with a warning."
            }
        }),
        (SettingsCategory::Network, PanelFocus::Rows) => Some(match state.network_row {
            NetworkRow::Proxy => {
                "An HTTP(S) proxy used for every request this session. Empty falls back to the \
                 HTTP(S)_PROXY environment variable."
            }
            NetworkRow::Tls => {
                "Certificate verification for HTTPS requests. Turning this OFF accepts ANY \
                 certificate — only for trusted, controlled environments."
            }
            NetworkRow::Cookies => {
                "Automatic cookie capture and replay for this session, backed by the on-disk \
                 jar below."
            }
        }),
        (SettingsCategory::Load, PanelFocus::Rows) => Some(match state.load_row {
            LoadRow::WarnTotal => {
                "Total request count above which starting a load run asks for confirmation first."
            }
            LoadRow::WarnConcurrency => {
                "Concurrent request count above which starting a load run asks for confirmation \
                 first."
            }
            LoadRow::MaxTotal => {
                "Hard ceiling on total requests — a load run above this is refused outright."
            }
            LoadRow::MaxConcurrency => {
                "Hard ceiling on concurrent requests — a load run above this is refused outright."
            }
        }),
        (SettingsCategory::Appearance, PanelFocus::Rows) => Some(match state.appearance_row {
            AppearanceRow::Theme => "The color theme (dark or light).",
            AppearanceRow::LeaderKey => {
                "The leader key that opens the which-key menu. Applies on next launch, not this \
                 session."
            }
        }),
        (SettingsCategory::Debug, PanelFocus::Rows) => Some(match state.debug_row {
            DebugRow::DebugToggle => {
                "Captures request/response traffic for the debug Inspector this session."
            }
            DebugRow::Advanced => {
                "Override the default load concurrency/total and the request timeout/body cap."
            }
        }),
        (SettingsCategory::Debug, PanelFocus::AdvancedList) => Some(match state.advanced_field {
            AdvancedField::Concurrency => "Default concurrent requests for a new load run.",
            AdvancedField::Total => "Default total requests for a new load run.",
            AdvancedField::BodyCapBytes => {
                "Maximum response body size to read; larger bodies are truncated."
            }
            AdvancedField::TimeoutSecs => {
                "How long to wait for a response before giving up, in seconds."
            }
        }),
        _ => None,
    }
}

/// Renders the footer: a live message row, then a one-line description of
/// the currently-hovered setting (freed up by moving the key hints to the
/// modal's bottom-right border — see `footer_hint`).
fn render_footer(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let [msg_row, desc_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let top = match &state.message {
        Some(msg) => Line::styled(format!(" {msg}"), theme.status_error),
        None => Line::from(""),
    };
    frame.render_widget(Paragraph::new(top), msg_row);

    let desc = setting_description(state).unwrap_or("");
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {desc}"), theme.statusline)),
        desc_row,
    );
}
