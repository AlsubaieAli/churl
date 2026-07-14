//! Render/draw fns for the Options overlay: the modal frame, the three control
//! rows, the cookie list, and the footer hints. Kept a child module of `options`
//! so it keeps full access to the state's private fields with no visibility
//! widening.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::{OptionsFocus, OptionsRow, OptionsState, mask_proxy, mask_proxy_password};
use crate::tui::theme::Theme;

/// Renders the Options overlay over `area`.
pub fn render(frame: &mut Frame, area: Rect, state: &OptionsState, theme: &Theme) {
    // A centered modal, comfortably narrower than the env editor.
    let [modal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(" Options ")
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Three control rows, the cookie list (fill), then a two-row footer.
    let [rows_area, cookies_area, footer] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Fill(1),
        Constraint::Length(2),
    ])
    .areas(inner);

    render_rows(frame, rows_area, state, theme);
    render_cookie_list(frame, cookies_area, state, theme);
    render_footer(frame, footer, state, theme);
}

/// Renders the three control rows (Proxy / TLS / Cookies).
fn render_rows(frame: &mut Frame, area: Rect, state: &OptionsState, theme: &Theme) {
    let rows_focused = state.focus == OptionsFocus::Rows;
    let sel = |row: OptionsRow| rows_focused && state.row == row;

    // Proxy row: masked value, or the inline editor's live text while editing
    // (the password segment stays masked even mid-edit — never plaintext creds).
    let proxy_display = if let Some(editor) = &state.editing {
        format!("{}█", mask_proxy_password(&editor.text()))
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

    let line = |label: &str, value: Span<'static>, selected: bool| -> Line<'static> {
        let marker = if selected { "▸ " } else { "  " };
        let l = Line::from(vec![Span::raw(format!("{marker}{label:<18}")), value]);
        if selected {
            l.style(theme.selection)
        } else {
            l
        }
    };

    // The insecure value is RED when verification is off — a loud indicator.
    let insecure_span = if state.insecure {
        Span::styled(insecure_display.to_owned(), theme.status_error)
    } else {
        Span::raw(insecure_display.to_owned())
    };

    let lines = vec![
        line("Proxy", Span::raw(proxy_display), sel(OptionsRow::Proxy)),
        line("TLS verification", insecure_span, sel(OptionsRow::Tls)),
        line(
            "Cookies",
            Span::raw(cookies_display.to_owned()),
            sel(OptionsRow::Cookies),
        ),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

/// Renders the cookie list beneath the Cookies row (domain · name · masked value).
fn render_cookie_list(frame: &mut Frame, area: Rect, state: &OptionsState, theme: &Theme) {
    let focused = state.focus == OptionsFocus::CookieList;
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

/// Renders the footer: a live message row plus the key hints for the focus state.
fn render_footer(frame: &mut Frame, area: Rect, state: &OptionsState, theme: &Theme) {
    let [msg_row, hint_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let top = match &state.message {
        Some(msg) => Line::styled(format!(" {msg}"), theme.status_error),
        None => Line::from(""),
    };
    frame.render_widget(Paragraph::new(top), msg_row);

    let hints = if state.editing.is_some() {
        "enter apply · esc cancel (empty clears the proxy)".to_owned()
    } else {
        match state.focus {
            OptionsFocus::Rows => "j/k move · enter edit/toggle · l cookies · q close".to_owned(),
            OptionsFocus::CookieList => {
                "j/k move · d delete · x clear all · h back · q close".to_owned()
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {hints}"), theme.statusline)),
        hint_row,
    );
}
