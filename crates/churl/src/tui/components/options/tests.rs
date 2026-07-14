//! Unit tests for the Options overlay's key handling: every branch of
//! `handle_key` is asserted to emit the right [`OptionsOutcome`], and the proxy
//! masking helpers are checked (including the security corner — credentials must
//! never render in plaintext).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use churl_core::cookies::CookieView;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn state_no_cookies() -> OptionsState {
    OptionsState::new(None, false, false, Vec::new())
}

fn state_with_cookies() -> OptionsState {
    OptionsState::new(
        None,
        false,
        true,
        vec![
            CookieView {
                domain: "a.example".into(),
                name: "sid".into(),
                value: "secret".into(),
            },
            CookieView {
                domain: "b.example".into(),
                name: "tok".into(),
                value: "xyz".into(),
            },
        ],
    )
}

#[test]
fn row_navigation_clamps_at_ends() {
    let mut s = state_no_cookies();
    assert_eq!(s.row, OptionsRow::Proxy);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('k'))),
        OptionsOutcome::Consumed
    );
    assert_eq!(s.row, OptionsRow::Proxy, "up at top stays");
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.row, OptionsRow::Tls);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.row, OptionsRow::Cookies);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.row, OptionsRow::Cookies, "down at bottom stays");
    s.handle_key(key(KeyCode::Char('k')));
    assert_eq!(s.row, OptionsRow::Tls);
}

#[test]
fn proxy_edit_emits_apply_with_typed_value() {
    let mut s = state_no_cookies();
    // Enter opens the inline editor (no outcome yet).
    assert_eq!(s.handle_key(key(KeyCode::Enter)), OptionsOutcome::Consumed);
    assert!(s.editing.is_some());
    for c in "http://p:3128".chars() {
        assert_eq!(
            s.handle_key(key(KeyCode::Char(c))),
            OptionsOutcome::Consumed
        );
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        OptionsOutcome::ApplyProxy(Some("http://p:3128".to_owned()))
    );
    assert!(s.editing.is_none(), "commit ends the edit");
}

#[test]
fn empty_proxy_edit_clears_it() {
    let mut s = OptionsState::new(Some("http://old:1".into()), false, false, Vec::new());
    s.handle_key(key(KeyCode::Enter)); // open editor seeded with the old value
    // Wipe it out.
    for _ in 0..30 {
        s.handle_key(key(KeyCode::Backspace));
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        OptionsOutcome::ApplyProxy(None),
        "an empty proxy clears it (env-proxy fallback)"
    );
}

#[test]
fn proxy_edit_esc_cancels_without_outcome() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(s.handle_key(key(KeyCode::Esc)), OptionsOutcome::Consumed);
    assert!(s.editing.is_none());
}

#[test]
fn tls_row_enter_emits_toggle_insecure() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j'))); // → TLS row
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        OptionsOutcome::ToggleInsecure
    );
}

#[test]
fn cookies_row_enter_emits_toggle_cookies() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Char('j'))); // → Cookies row
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        OptionsOutcome::ToggleCookies
    );
}

#[test]
fn cookie_list_delete_and_clear() {
    let mut s = state_with_cookies();
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Char('j'))); // → Cookies row
    // Descend into the list.
    assert_eq!(
        s.handle_key(key(KeyCode::Char('l'))),
        OptionsOutcome::Consumed
    );
    assert_eq!(s.focus, OptionsFocus::CookieList);
    // Delete the selected (first) cookie.
    assert_eq!(
        s.handle_key(key(KeyCode::Char('d'))),
        OptionsOutcome::DeleteCookie {
            domain: "a.example".into(),
            name: "sid".into(),
        }
    );
    // Move down, then clear-all.
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(
        s.handle_key(key(KeyCode::Char('x'))),
        OptionsOutcome::ClearCookies
    );
}

#[test]
fn cannot_enter_empty_cookie_list() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Char('j'))); // Cookies row, but no cookies
    s.handle_key(key(KeyCode::Char('l')));
    assert_eq!(s.focus, OptionsFocus::Rows, "no descent into an empty list");
}

#[test]
fn q_and_esc_close() {
    let mut s = state_no_cookies();
    assert_eq!(s.handle_key(key(KeyCode::Char('q'))), OptionsOutcome::Close);
    assert_eq!(s.handle_key(key(KeyCode::Esc)), OptionsOutcome::Close);
}

#[test]
fn render_cookie_list_masks_values_and_marks_selection() {
    use crate::tui::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    let mut s = state_with_cookies();
    s.row = OptionsRow::Cookies;
    s.focus = OptionsFocus::CookieList;
    s.cookie_sel = 1; // select the second cookie

    let theme = Theme::dark();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, Rect::new(0, 0, 80, 24), &s, &theme))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let text: String = (0..24)
        .map(|y| {
            (0..80)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Domains + names are shown; the values are masked (never plaintext).
    assert!(text.contains("a.example"), "{text}");
    assert!(text.contains("sid"), "{text}");
    assert!(text.contains("b.example"), "{text}");
    assert!(
        text.contains("••••••"),
        "cookie values must render masked:\n{text}"
    );
    assert!(
        !text.contains("secret"),
        "plaintext cookie value leaked:\n{text}"
    );
    assert!(
        !text.contains("xyz"),
        "plaintext cookie value leaked:\n{text}"
    );
}

#[test]
fn mask_proxy_hides_userinfo() {
    assert_eq!(
        mask_proxy("http://user:pass@proxy.local:3128"),
        "http://***@proxy.local:3128"
    );
    assert_eq!(mask_proxy("user:pass@proxy.local"), "***@proxy.local");
    // No userinfo → unchanged.
    assert_eq!(
        mask_proxy("http://proxy.local:3128"),
        "http://proxy.local:3128"
    );
}

#[test]
fn mask_proxy_password_keeps_user_visible_hides_password() {
    assert_eq!(
        mask_proxy_password("http://user:pass@proxy.local:3128"),
        "http://user:••••@proxy.local:3128"
    );
    assert_eq!(mask_proxy_password("user:pass@host"), "user:••••@host");
    // Username-only or no userinfo: nothing to mask; port colon untouched.
    assert_eq!(mask_proxy_password("http://user@host"), "http://user@host");
    assert_eq!(mask_proxy_password("http://host:8080"), "http://host:8080");
    // Never leaks a password.
    assert!(!mask_proxy_password("http://u:supersecret@h").contains("supersecret"));
}
