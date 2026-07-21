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
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use super::{
    AdvancedField, AppearanceRow, CookieFormField, DebugRow, LoadRow, NetworkRow, PanelFocus,
    RequestRow, SettingKey, SettingsCategory, SettingsLevel, SettingsState, format_body_cap,
    leader_key_eq, mask_proxy, mask_proxy_password,
};
use crate::tui::theme::Theme;

/// Panel-footer height: one status/message row plus three rows for the
/// per-setting description. Three description rows is what the longest
/// description (the redirect-policy and TLS notes) needs to wrap fully on
/// screen at the real modal width (`Percentage(70)` → ~52 usable columns on
/// an 80-col terminal), so no note is ever cut mid-sentence. The description
/// renders with word-wrap across those rows (see `render_footer`).
const FOOTER_HEIGHT: u16 = 4;

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
        // RIGHT-aligned (M8.5.3): every `footer_hint` string is kept within
        // the real modal's border width (verified at 80 cols — see
        // `footer_hint`'s doc and its regression tests), so a right-aligned
        // hint never clips at all, on either end. Right-aligned reads better
        // against the bottom-right corner convention this modal already uses
        // for the top title; a PRIOR left-aligned placement existed only to
        // dodge a since-fixed over-long hint that would have clipped the
        // flagship `j/k`/`J/K adjust` LEFT token under right-alignment.
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
            Layout::vertical([Constraint::Fill(1), Constraint::Length(FOOTER_HEIGHT)]).areas(area);
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
            Constraint::Length(FOOTER_HEIGHT),
        ])
        .areas(area)
    } else {
        let [rows_area, footer] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(FOOTER_HEIGHT)]).areas(area);
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
                // Canonical combo compare, not raw string equality — a captured
                // `"Space"` must not read dirty against the default `"space"`.
                !leader_key_eq(&state.leader_key, &state.persisted.leader_key),
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
///
/// The border is right-aligned, so an over-long hint clips on the LEFT — it
/// would eat the flagship `J/K adjust` token first. Every string here is
/// therefore kept short enough to fit the modal's inner bottom border at the
/// real width (`Percentage(70)` → ~54 columns on an 80-col terminal); the
/// full, verbose key list stays reachable via `?` help and the command
/// palette.
fn footer_hint(state: &SettingsState) -> &'static str {
    match state.level {
        SettingsLevel::Menu => "j/k move · enter open · s save · q close",
        SettingsLevel::Panel => {
            if let Some(form) = &state.cookie_form {
                if form.editing.is_some() {
                    "enter apply · esc cancel"
                } else {
                    "j/k · enter edit · s save cookie · esc cancel"
                }
            } else if state.capturing_leader_key {
                "press any key · tab to type · esc cancel"
            } else if state.editing.is_some() {
                "enter apply · esc cancel"
            } else {
                match state.focus {
                    PanelFocus::Rows if state.category == SettingsCategory::Network => {
                        "j/k · J/K adjust · enter · a add · l list · s save"
                    }
                    PanelFocus::Rows => "j/k move · J/K adjust · enter · s save · esc · q",
                    PanelFocus::CookieList => "j/k · a add · e edit · d del · x clear · esc",
                    PanelFocus::AdvancedList => "j/k · J/K adjust · enter edit · s save · esc back",
                }
            }
        }
    }
}

/// A one-line description of the currently-hovered setting — what it does,
/// plus anything useful a value alone doesn't convey (units, when it takes
/// effect, a security note). `None` when nothing meaningful is hovered
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
                "Max response body to read; larger is truncated. Entered in MB/KB (e.g. 10MB), \
                 stored as bytes."
            }
            RequestRow::Redirect => {
                "Cross-origin redirect handling: strip drops auth headers, strict stops at the \
                 hop, follow-all keeps them."
            }
            RequestRow::UrlEdit => {
                "What the URL bar's i/Enter opens: inline (in place) or popup (full editor)."
            }
            RequestRow::SecretPolicy => {
                "How a save reacts to a literal secret in the request: strict blocks it, warn \
                 allows with a warning."
            }
        }),
        (SettingsCategory::Network, PanelFocus::Rows) => Some(match state.network_row {
            NetworkRow::Proxy => {
                "HTTP(S) proxy for every request this session. Empty uses the HTTP(S)_PROXY env \
                 var."
            }
            NetworkRow::Tls => {
                "Certificate verification for HTTPS. Turning this OFF accepts ANY certificate — \
                 only for trusted environments."
            }
            NetworkRow::Cookies => {
                "Automatic cookie capture and replay this session, backed by the on-disk jar \
                 below."
            }
        }),
        (SettingsCategory::Load, PanelFocus::Rows) => Some(match state.load_row {
            LoadRow::WarnTotal => "Total requests above which a load run asks to confirm first.",
            LoadRow::WarnConcurrency => {
                "Concurrent requests above which a load run asks to confirm first."
            }
            LoadRow::MaxTotal => {
                "Hard ceiling on total requests — a load run above this is refused."
            }
            LoadRow::MaxConcurrency => {
                "Hard ceiling on concurrent requests — a load run above this is refused."
            }
        }),
        (SettingsCategory::Appearance, PanelFocus::Rows) => Some(match state.appearance_row {
            AppearanceRow::Theme => "The color theme (dark or light).",
            AppearanceRow::LeaderKey => {
                "The leader key that opens the which-key menu. Live-applies on <leader>r \
                 (or restart)."
            }
        }),
        (SettingsCategory::Debug, PanelFocus::Rows) => Some(match state.debug_row {
            DebugRow::DebugToggle => {
                "Captures request/response traffic for the debug Inspector this session."
            }
            DebugRow::Advanced => {
                "Override default load concurrency/total and the request timeout/body cap."
            }
        }),
        (SettingsCategory::Debug, PanelFocus::AdvancedList) => Some(match state.advanced_field {
            AdvancedField::Concurrency => "Default concurrent requests for a new load run.",
            AdvancedField::Total => "Default total requests for a new load run.",
            AdvancedField::BodyCapBytes => "Max response body to read; larger is truncated.",
            AdvancedField::TimeoutSecs => {
                "How long to wait for a response before giving up, in seconds."
            }
        }),
        _ => None,
    }
}

/// Renders the footer: a live message row, then a WORD-WRAPPED description of
/// the currently-hovered setting, BOTTOM-ANCHORED within the remaining rows
/// (freed up by moving the key hints to the modal's bottom-right border — see
/// `footer_hint`). The description wraps rather than clipping, so the full
/// text (units, the TLS-off security note, the redirect-policy summary) is on
/// screen even at the real modal width where a single line would truncate.
/// Bottom-anchoring (M8.5.3) means a short 1/2-line description sits directly
/// above the modal's bottom border, matching the border's own key-hint line,
/// rather than floating at the top of the description area with a blank gap
/// beneath it.
fn render_footer(frame: &mut Frame, area: Rect, state: &SettingsState, theme: &Theme) {
    let [msg_row, desc_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);

    let top = match &state.message {
        Some(msg) => Line::styled(format!(" {msg}"), theme.status_error),
        None => Line::from(""),
    };
    frame.render_widget(Paragraph::new(top), msg_row);

    let desc = setting_description(state).unwrap_or("");
    let desc_line = format!(" {desc}");
    let paragraph =
        Paragraph::new(Line::styled(desc_line.clone(), theme.statusline)).wrap(Wrap { trim: true });
    // `wrapped_line_count` mirrors the SAME greedy word-wrap this Paragraph
    // renders with, so the offset always matches what actually gets drawn —
    // a 1-line description gets pushed down by (height - 1) rows, a 3-line
    // one fills the area exactly as before (pad = 0).
    let needed = wrapped_line_count(&desc_line, desc_area.width).min(desc_area.height);
    let pad = desc_area.height.saturating_sub(needed);
    let anchored = Rect {
        y: desc_area.y + pad,
        height: desc_area.height - pad,
        ..desc_area
    };
    frame.render_widget(paragraph, anchored);
}

/// Counts how many rows a `Wrap { trim: true }` render of `text` needs at
/// `width` columns — a greedy word-wrap mirroring the algorithm
/// `ratatui-widgets`' `Paragraph` itself renders with (its own `line_count`
/// is gated behind the `unstable-rendered-line-info` cargo feature, which
/// this crate does not enable). Used only to POSITION the description
/// (bottom-anchor it in [`render_footer`]) — the actual wrap/render is still
/// done by `Paragraph` itself, so any edge-case divergence here could only
/// shift the anchor by a row, never change what's drawn.
fn wrapped_line_count(text: &str, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    let width = width as usize;
    let mut lines: u16 = 0;
    let mut current_width = 0usize;
    let mut line_has_content = false;
    for word in text.split_whitespace() {
        let word_width = unicode_width::UnicodeWidthStr::width(word);
        if !line_has_content {
            // The first word on a line is always placed, even if it alone
            // overflows the width (matches WordWrapper's own overflow rule).
            current_width = word_width;
            line_has_content = true;
            lines += 1;
        } else if current_width + 1 + word_width <= width {
            current_width += 1 + word_width;
        } else {
            lines += 1;
            current_width = word_width;
        }
    }
    lines.max(1)
}
