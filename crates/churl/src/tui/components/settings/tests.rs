//! Unit tests for the Settings panel's key handling: category-menu nav,
//! panel/list/edit level transitions, every ported Network/Debug control still
//! emitting its old outcome, the new Request/Load/Appearance controls, and the
//! proxy masking helpers (including the security corner — credentials must
//! never render in plaintext).

use churl_core::config::{RedirectPolicy, UrlEditMode};
use churl_core::cookies::{CookieView, SameSite};
use churl_core::load::LoadCaps;
use churl_core::secrets::SecretPolicy;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn default_limits() -> ResolvedAdvancedLimits {
    churl_core::config::Config::default().advanced_limits()
}

fn snapshot(debug_enabled: bool, cookies: Vec<CookieView>) -> SettingsSnapshot {
    SettingsSnapshot {
        redirect: RedirectPolicy::default(),
        url_edit: UrlEditMode::default(),
        secret_policy: SecretPolicy::default(),
        proxy: None,
        insecure: false,
        cookies_enabled: !cookies.is_empty(),
        cookies,
        load_caps: LoadCaps::default(),
        theme_name: DEFAULT_THEME_NAME.to_owned(),
        leader_key: DEFAULT_LEADER_KEY.to_owned(),
        debug_enabled,
        advanced: default_limits(),
        persisted: churl_core::config::ResolvedSettings::default(),
        touched: std::collections::HashSet::new(),
    }
}

fn state_no_cookies() -> SettingsState {
    SettingsState::new(snapshot(false, Vec::new()))
}

fn state_with_cookies() -> SettingsState {
    SettingsState::new(snapshot(
        false,
        vec![
            CookieView {
                domain: "a.example".into(),
                name: "sid".into(),
                value: "secret".into(),
                path: "/".into(),
                secure: true,
                same_site: Some(SameSite::Strict),
            },
            CookieView {
                domain: "b.example".into(),
                name: "tok".into(),
                value: "xyz".into(),
                path: "/api".into(),
                secure: false,
                same_site: None,
            },
        ],
    ))
}

/// A state with debug capture on, so the Debug category is reachable.
fn state_debug_on() -> SettingsState {
    SettingsState::new(snapshot(true, Vec::new()))
}

/// Drives a state into the Network category's Cookies row, panel level.
fn goto_network_cookies(s: &mut SettingsState) {
    s.handle_key(key(KeyCode::Char('j'))); // Request -> Network
    s.handle_key(key(KeyCode::Enter)); // enter panel
    s.handle_key(key(KeyCode::Char('j'))); // Proxy -> Tls
    s.handle_key(key(KeyCode::Char('j'))); // Tls -> Cookies
}

// ---- Level 1: category menu ----

#[test]
fn menu_starts_on_request_and_cycles_categories() {
    let mut s = state_no_cookies();
    assert_eq!(s.category, SettingsCategory::Request);
    assert_eq!(s.level, SettingsLevel::Menu);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.category, SettingsCategory::Network);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.category, SettingsCategory::Load);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.category, SettingsCategory::Appearance);
    // Debug is off — the menu wraps straight back to Request, skipping Debug.
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(
        s.category,
        SettingsCategory::Request,
        "Debug is functionally absent with debug off"
    );
}

#[test]
fn menu_reaches_debug_category_when_debug_on() {
    let mut s = state_debug_on();
    for _ in 0..4 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    assert_eq!(s.category, SettingsCategory::Debug);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.category, SettingsCategory::Request, "wraps");
    s.handle_key(key(KeyCode::Char('k')));
    assert_eq!(s.category, SettingsCategory::Debug, "wraps backward too");
}

#[test]
fn enter_opens_the_panel_at_rows_focus() {
    let mut s = state_no_cookies();
    assert_eq!(s.handle_key(key(KeyCode::Enter)), SettingsOutcome::Consumed);
    assert_eq!(s.level, SettingsLevel::Panel);
    assert_eq!(s.focus, PanelFocus::Rows);
    assert_eq!(s.category, SettingsCategory::Request);
}

#[test]
fn esc_at_menu_closes() {
    let mut s = state_no_cookies();
    assert_eq!(s.handle_key(key(KeyCode::Esc)), SettingsOutcome::Close);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('q'))),
        SettingsOutcome::Close
    );
}

// ---- Level 1 <-> 2: panel back-navigation ----

#[test]
fn esc_at_panel_rows_backs_to_menu_keeping_category() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j'))); // -> Network
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    assert_eq!(s.level, SettingsLevel::Panel);
    assert_eq!(
        s.handle_key(key(KeyCode::Esc)),
        SettingsOutcome::Consumed,
        "esc backs up one level, not a full close"
    );
    assert_eq!(s.level, SettingsLevel::Menu);
    assert_eq!(
        s.category,
        SettingsCategory::Network,
        "the menu cursor stays on the category just left"
    );
}

#[test]
fn q_closes_from_inside_a_panel() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter)); // -> Panel (Request)
    assert_eq!(
        s.handle_key(key(KeyCode::Char('q'))),
        SettingsOutcome::Close
    );
}

// ---- Network category (ported Options-overlay behaviour) ----

#[test]
fn network_row_navigation_clamps_at_ends() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j'))); // -> Network
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    assert_eq!(s.network_row, NetworkRow::Proxy);
    s.handle_key(key(KeyCode::Char('k')));
    assert_eq!(s.network_row, NetworkRow::Proxy, "up at top stays");
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.network_row, NetworkRow::Tls);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.network_row, NetworkRow::Cookies);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(
        s.network_row,
        NetworkRow::Cookies,
        "down at bottom stays — clamped, ported verbatim from the old flat rows"
    );
}

#[test]
fn proxy_edit_emits_apply_with_typed_value() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j'))); // -> Network
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Proxy row
    assert_eq!(s.handle_key(key(KeyCode::Enter)), SettingsOutcome::Consumed);
    assert!(s.editing.is_some());
    for c in "http://p:3128".chars() {
        assert_eq!(
            s.handle_key(key(KeyCode::Char(c))),
            SettingsOutcome::Consumed
        );
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyProxy(Some("http://p:3128".to_owned()))
    );
    assert!(s.editing.is_none(), "commit ends the edit");
}

#[test]
fn empty_proxy_edit_clears_it() {
    let mut s = SettingsState::new(SettingsSnapshot {
        proxy: Some("http://old:1".into()),
        ..snapshot(false, Vec::new())
    });
    s.handle_key(key(KeyCode::Char('j'))); // Request -> Network
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Proxy row
    s.handle_key(key(KeyCode::Enter)); // open editor seeded with the old value
    for _ in 0..30 {
        s.handle_key(key(KeyCode::Backspace));
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyProxy(None),
        "an empty proxy clears it (env-proxy fallback)"
    );
}

#[test]
fn proxy_edit_esc_cancels_without_outcome() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('x')));
    assert_eq!(s.handle_key(key(KeyCode::Esc)), SettingsOutcome::Consumed);
    assert!(s.editing.is_none());
}

#[test]
fn tls_row_enter_emits_toggle_insecure() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('j'))); // -> Tls row
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ToggleInsecure
    );
}

#[test]
fn cookies_row_enter_emits_toggle_cookies() {
    let mut s = state_no_cookies();
    goto_network_cookies(&mut s);
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ToggleCookies
    );
}

#[test]
fn cookie_list_delete_and_clear() {
    let mut s = state_with_cookies();
    goto_network_cookies(&mut s);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('l'))),
        SettingsOutcome::Consumed
    );
    assert_eq!(s.focus, PanelFocus::CookieList);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('d'))),
        SettingsOutcome::DeleteCookie {
            domain: "a.example".into(),
            name: "sid".into(),
        }
    );
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(
        s.handle_key(key(KeyCode::Char('x'))),
        SettingsOutcome::ClearCookies
    );
}

#[test]
fn cannot_enter_empty_cookie_list() {
    let mut s = state_no_cookies();
    goto_network_cookies(&mut s);
    s.handle_key(key(KeyCode::Char('l')));
    assert_eq!(s.focus, PanelFocus::Rows, "no descent into an empty list");
}

#[test]
fn cookie_list_esc_backs_to_rows_not_close() {
    let mut s = state_with_cookies();
    goto_network_cookies(&mut s);
    s.handle_key(key(KeyCode::Char('l')));
    assert_eq!(s.focus, PanelFocus::CookieList);
    assert_eq!(
        s.handle_key(key(KeyCode::Esc)),
        SettingsOutcome::Consumed,
        "esc from a list is one level back, not a close"
    );
    assert_eq!(s.focus, PanelFocus::Rows);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('q'))),
        SettingsOutcome::Close
    );
}

// ---- Cookie add/edit form (M8.5.1) ----

fn open_add_cookie_form(s: &mut SettingsState) {
    goto_network_cookies(s);
    s.handle_key(key(KeyCode::Char('a')));
}

fn type_text(s: &mut SettingsState, text: &str) {
    for c in text.chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
}

/// Types `text` into the currently-focused text field via Enter → type →
/// Enter (open the field's editor, type, commit), leaving focus on that field.
fn fill_focused_field(s: &mut SettingsState, text: &str) {
    s.handle_key(key(KeyCode::Enter));
    type_text(s, text);
    s.handle_key(key(KeyCode::Enter));
}

#[test]
fn cookie_add_form_fill_and_commit_emits_upsert_with_no_previous() {
    let mut s = state_no_cookies();
    open_add_cookie_form(&mut s);
    assert!(s.cookie_form.is_some(), "a opens the add form");
    assert!(
        s.cookie_form.as_ref().unwrap().editing_existing.is_none(),
        "a fresh add form has no original coordinates"
    );

    fill_focused_field(&mut s, "a.example"); // Domain
    s.handle_key(key(KeyCode::Char('j'))); // -> Name
    fill_focused_field(&mut s, "sid");
    s.handle_key(key(KeyCode::Char('j'))); // -> Value
    fill_focused_field(&mut s, "abc123");

    let outcome = s.handle_key(key(KeyCode::Char('s')));
    assert_eq!(
        outcome,
        SettingsOutcome::UpsertCookie {
            previous: None,
            domain: "a.example".into(),
            name: "sid".into(),
            value: "abc123".into(),
            path: "/".into(),
            secure: false,
            same_site: None,
        }
    );
    assert!(
        s.cookie_form.is_none(),
        "a successful submit closes the form"
    );
}

#[test]
fn cookie_edit_form_prefills_and_commit_emits_upsert_with_previous() {
    let mut s = state_with_cookies(); // a.example/sid/secret (secure, Strict) selected first
    goto_network_cookies(&mut s);
    s.handle_key(key(KeyCode::Char('l'))); // -> CookieList, selects a.example/sid
    s.handle_key(key(KeyCode::Char('e'))); // open edit form, prefilled

    {
        let form = s.cookie_form.as_ref().expect("e opens the edit form");
        assert_eq!(form.domain, "a.example");
        assert_eq!(form.name, "sid");
        assert_eq!(form.value, "secret");
        assert_eq!(form.path, "/");
        assert!(form.secure);
        assert_eq!(form.same_site, Some(SameSite::Strict));
        assert_eq!(
            form.editing_existing,
            Some(("a.example".into(), "sid".into(), "/".into())),
            "the edit form must remember the cookie's ORIGINAL coordinates"
        );
    }

    // Change only the value; domain/name/path stay as prefilled.
    s.handle_key(key(KeyCode::Char('j'))); // Domain -> Name
    s.handle_key(key(KeyCode::Char('j'))); // Name -> Value
    s.handle_key(key(KeyCode::Enter)); // begin editing Value (prefilled "secret")
    for _ in 0.."secret".len() {
        s.handle_key(key(KeyCode::Backspace));
    }
    type_text(&mut s, "newvalue");
    s.handle_key(key(KeyCode::Enter)); // commit Value

    let outcome = s.handle_key(key(KeyCode::Char('s')));
    assert_eq!(
        outcome,
        SettingsOutcome::UpsertCookie {
            previous: Some(("a.example".into(), "sid".into(), "/".into())),
            domain: "a.example".into(),
            name: "sid".into(),
            value: "newvalue".into(),
            path: "/".into(),
            secure: true,
            same_site: Some(SameSite::Strict),
        },
        "previous must be Some(original coords) even though the key didn't change \
         — the app handler decides whether an old-key delete is actually needed"
    );
}

#[test]
fn cookie_form_esc_cancels_without_emitting_anything() {
    let mut s = state_no_cookies();
    open_add_cookie_form(&mut s);
    fill_focused_field(&mut s, "a.example");

    assert_eq!(
        s.handle_key(key(KeyCode::Esc)),
        SettingsOutcome::Consumed,
        "esc cancels the form, not the whole panel"
    );
    assert!(s.cookie_form.is_none());
    assert_eq!(
        s.focus,
        PanelFocus::Rows,
        "canceling the form returns to the row list, panel stays open"
    );
}

#[test]
fn cookie_add_form_rejects_empty_domain_and_stays_open() {
    let mut s = state_no_cookies();
    open_add_cookie_form(&mut s);
    // Leave Domain blank, fill only Name, then try to submit.
    s.handle_key(key(KeyCode::Char('j'))); // Domain -> Name
    fill_focused_field(&mut s, "sid");

    let outcome = s.handle_key(key(KeyCode::Char('s')));
    assert_eq!(
        outcome,
        SettingsOutcome::Consumed,
        "an invalid submit must not emit UpsertCookie"
    );
    assert!(
        s.cookie_form.is_some(),
        "the form stays open on validation failure — losing 5 other filled \
         fields over one bad one would be a needless retype"
    );
    assert!(s.message.is_some(), "an inline error must be shown");
}

// ---- Request category ----

#[test]
fn request_timeout_edit_emits_apply_advanced_timeout_secs() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Request/Timeout row
    assert_eq!(s.request_row, RequestRow::Timeout);
    s.handle_key(key(KeyCode::Enter)); // begin edit, seeded
    assert!(s.editing.is_some());
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Backspace));
    }
    for c in "45".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::TimeoutSecs,
            value: 45
        }
    );
}

#[test]
fn request_max_body_edit_emits_apply_advanced_body_cap() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('j'))); // -> MaxBodyBytes row
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..20 {
        s.handle_key(key(KeyCode::Backspace));
    }
    for c in "4096".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::BodyCapBytes,
            value: 4096
        }
    );
}

#[test]
fn request_redirect_cycles_through_all_values() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..2 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    assert_eq!(s.request_row, RequestRow::Redirect);
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyRedirect(RedirectPolicy::Strict)
    );
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyRedirect(RedirectPolicy::FollowAll)
    );
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyRedirect(RedirectPolicy::Strip),
        "wraps back to the default"
    );
}

#[test]
fn request_url_edit_and_secret_policy_cycle() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    assert_eq!(s.request_row, RequestRow::UrlEdit);
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyUrlEdit(UrlEditMode::Popup)
    );
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.request_row, RequestRow::SecretPolicy);
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplySecretPolicy(SecretPolicy::Warn)
    );
}

// ---- Load category ----

#[test]
fn load_cap_edit_emits_apply_load_cap() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Char('j'))); // -> Load
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    assert_eq!(s.load_row, LoadRow::WarnTotal);
    s.handle_key(key(KeyCode::Enter)); // begin edit
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Backspace));
    }
    for c in "50".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyLoadCap {
            field: LoadRow::WarnTotal,
            value: 50
        }
    );
}

#[test]
fn load_cap_edit_rejects_zero() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Backspace));
    }
    s.handle_key(key(KeyCode::Char('0')));
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::Consumed,
        "zero is rejected, not applied"
    );
    assert!(s.message.is_some());
}

// ---- Appearance category ----

#[test]
fn theme_row_cycles_dark_and_light() {
    let mut s = state_no_cookies();
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter)); // -> Panel (Appearance/Theme)
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyTheme("light".to_owned())
    );
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyTheme("dark".to_owned())
    );
}

#[test]
fn leader_key_edit_validates_and_applies() {
    let mut s = state_no_cookies();
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    s.handle_key(key(KeyCode::Char('j'))); // -> LeaderKey row
    s.handle_key(key(KeyCode::Enter)); // begin edit
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Backspace));
    }
    for c in "ctrl-b".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyLeaderKey("ctrl-b".to_owned())
    );
}

#[test]
fn leader_key_edit_rejects_bad_combo() {
    let mut s = state_no_cookies();
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Backspace));
    }
    for c in "not a real combo!!".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(s.handle_key(key(KeyCode::Enter)), SettingsOutcome::Consumed);
    assert!(s.message.is_some());
}

// ---- Debug category (ported Options-overlay Advanced behaviour) ----

#[test]
fn debug_category_unreachable_when_debug_off() {
    let mut s = state_no_cookies();
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    assert_ne!(
        s.category,
        SettingsCategory::Debug,
        "Debug never appears in the menu cycle with debug off"
    );
}

#[test]
fn debug_toggle_row_emits_toggle_debug() {
    let mut s = state_debug_on();
    for _ in 0..4 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    assert_eq!(s.category, SettingsCategory::Debug);
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    assert_eq!(s.debug_row, DebugRow::DebugToggle);
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ToggleDebug
    );
}

#[test]
fn advanced_field_edit_emits_apply_advanced() {
    let mut s = state_debug_on();
    for _ in 0..4 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    s.handle_key(key(KeyCode::Char('j'))); // -> Advanced row
    s.handle_key(key(KeyCode::Tab)); // descend into the field list
    assert_eq!(s.focus, PanelFocus::AdvancedList);
    assert_eq!(s.advanced_field, AdvancedField::Concurrency);
    s.handle_key(key(KeyCode::Enter)); // begin edit, seeded with the current value
    assert!(s.editing.is_some());
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Backspace));
    }
    for c in "42".chars() {
        s.handle_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::Concurrency,
            value: 42
        }
    );
    assert!(s.editing.is_none(), "commit ends the edit");
}

#[test]
fn advanced_field_edit_rejects_zero_and_non_numeric() {
    let mut s = state_debug_on();
    s.level = SettingsLevel::Panel;
    s.category = SettingsCategory::Debug;
    s.focus = PanelFocus::AdvancedList;
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..10 {
        s.handle_key(key(KeyCode::Backspace));
    }
    s.handle_key(key(KeyCode::Char('0')));
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SettingsOutcome::Consumed,
        "zero is rejected, not applied"
    );
    assert!(s.message.is_some());
}

#[test]
fn advanced_field_navigation_cycles_all_four() {
    let mut s = state_debug_on();
    s.level = SettingsLevel::Panel;
    s.category = SettingsCategory::Debug;
    s.focus = PanelFocus::AdvancedList;
    assert_eq!(s.advanced_field, AdvancedField::Concurrency);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.advanced_field, AdvancedField::Total);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.advanced_field, AdvancedField::BodyCapBytes);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.advanced_field, AdvancedField::TimeoutSecs);
    s.handle_key(key(KeyCode::Char('j')));
    assert_eq!(s.advanced_field, AdvancedField::Concurrency, "wraps");
    s.handle_key(key(KeyCode::Char('h')));
    assert_eq!(s.focus, PanelFocus::Rows, "h backs out to the row list");
}

#[test]
fn debug_going_off_mid_session_strands_nothing() {
    let mut s = state_debug_on();
    s.level = SettingsLevel::Panel;
    s.category = SettingsCategory::Debug;
    s.focus = PanelFocus::AdvancedList;
    s.refresh(snapshot(false, Vec::new()));
    assert_eq!(s.level, SettingsLevel::Menu);
    assert_eq!(s.category, SettingsCategory::Request);
    assert_eq!(s.focus, PanelFocus::Rows);
}

// ---- rendering + masking ----

#[test]
fn render_cookie_list_masks_values_and_marks_selection() {
    use crate::tui::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    let mut s = state_with_cookies();
    s.level = SettingsLevel::Panel;
    s.category = SettingsCategory::Network;
    s.network_row = NetworkRow::Cookies;
    s.focus = PanelFocus::CookieList;
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
    // SameSite/Secure render too (M8.5.1 B1): a.example/sid is secure+Strict,
    // b.example/tok is neither.
    assert!(
        text.contains("secure:✓") && text.contains("samesite:Strict"),
        "secure cookie must show secure:✓ and samesite:Strict:\n{text}"
    );
    assert!(
        text.contains("secure:✗") && text.contains("samesite:—"),
        "non-secure/no-samesite cookie must show secure:✗ and samesite:—:\n{text}"
    );

    // A cookie that just went through the add flow and landed in the panel's
    // mirror (via `refresh`, exactly like a real add would) must stay masked
    // too — masking is a property of the render, not of which cookies
    // happened to be present when the panel opened.
    s.refresh(snapshot(
        false,
        vec![CookieView {
            domain: "new.example".into(),
            name: "added".into(),
            value: "topsecret".into(),
            path: "/".into(),
            secure: false,
            same_site: None,
        }],
    ));
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
    assert!(text.contains("new.example"), "{text}");
    assert!(text.contains("added"), "{text}");
    assert!(
        text.contains("••••••"),
        "a freshly-added cookie's value must render masked too:\n{text}"
    );
    assert!(
        !text.contains("topsecret"),
        "plaintext cookie value leaked for a freshly-added cookie:\n{text}"
    );
}

/// FIX C guard: the cookie ADD/EDIT FORM must mask the value field, exactly
/// like the list does. Without this, a regression swapping the Value line's
/// `masked_value()` back to a plaintext render would ship green (the recurring
/// M8.2/M8.3/M8.5 secret-leak class). Opens an edit form prefilled with a
/// distinctive value, renders it, and asserts the plaintext is absent while
/// the mask is present.
#[test]
fn render_cookie_form_masks_the_value_field() {
    use crate::tui::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    let mut s = SettingsState::new(snapshot(
        false,
        vec![CookieView {
            domain: "a.example".into(),
            name: "sid".into(),
            value: "SUPERSECRET".into(),
            path: "/".into(),
            secure: false,
            same_site: None,
        }],
    ));
    goto_network_cookies(&mut s);
    s.handle_key(key(KeyCode::Char('l'))); // descend into the cookie list
    s.handle_key(key(KeyCode::Char('e'))); // open the edit form, prefilled
    assert!(
        s.cookie_form.is_some(),
        "the edit form must be open for this to test the form render"
    );
    assert_eq!(
        s.cookie_form.as_ref().unwrap().value,
        "SUPERSECRET",
        "the form must actually hold the secret value it is masking"
    );

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

    assert!(
        !text.contains("SUPERSECRET"),
        "the cookie form must NEVER render the value in plaintext:\n{text}"
    );
    assert!(
        text.contains("••••••"),
        "the cookie form's value field must render masked:\n{text}"
    );
}

#[test]
fn mask_proxy_hides_userinfo() {
    assert_eq!(
        mask_proxy("http://user:pass@proxy.local:3128"),
        "http://***@proxy.local:3128"
    );
    assert_eq!(mask_proxy("user:pass@proxy.local"), "***@proxy.local");
    assert_eq!(
        mask_proxy("http://proxy.local:3128"),
        "http://proxy.local:3128"
    );
}

#[test]
fn mask_proxy_password_keeps_user_visible_hides_password() {
    assert_eq!(
        mask_proxy_password("http://proxy.local"),
        "http://proxy.local"
    );
    assert_eq!(mask_proxy_password("http://user"), "http://user");
    assert_eq!(mask_proxy_password("http://user@host"), "http://user@host");
    assert_eq!(mask_proxy_password("http://user:pass"), "http://user:••••");
    assert_eq!(mask_proxy_password("user:secret"), "user:••••");
    assert_eq!(
        mask_proxy_password("http://user:pass@proxy.local:3128"),
        "http://user:••••@proxy.local:3128"
    );
    assert_eq!(mask_proxy_password("user:pass@host"), "user:••••@host");
    let masked = mask_proxy_password("http://user:pass@proxy.local:3128");
    assert!(
        masked.ends_with(":3128"),
        "the port must stay visible: {masked}"
    );
    assert!(!mask_proxy_password("http://u:p$s!w0rd@h").contains("p$s!w0rd"));
    assert!(!mask_proxy_password("http://u:p$s!w0rd").contains("p$s!w0rd"));
}

// ---- Save-as-default (M8.5 Wave 3) ----

#[test]
fn s_emits_save_defaults_from_the_menu() {
    let mut s = state_no_cookies();
    assert_eq!(
        s.handle_key(key(KeyCode::Char('s'))),
        SettingsOutcome::SaveDefaults
    );
}

#[test]
fn s_emits_save_defaults_from_a_panel() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j'))); // -> Network
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    assert_eq!(
        s.handle_key(key(KeyCode::Char('s'))),
        SettingsOutcome::SaveDefaults
    );
}

#[test]
fn s_emits_save_defaults_from_a_list() {
    let mut s = state_with_cookies();
    goto_network_cookies(&mut s);
    s.handle_key(key(KeyCode::Char('l'))); // -> CookieList
    assert_eq!(s.focus, PanelFocus::CookieList);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('s'))),
        SettingsOutcome::SaveDefaults
    );
}

#[test]
fn s_types_normally_inside_an_open_edit_rather_than_saving() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j'))); // -> Network
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Proxy row
    s.handle_key(key(KeyCode::Enter)); // open the proxy editor
    assert_eq!(
        s.handle_key(key(KeyCode::Char('s'))),
        SettingsOutcome::Consumed,
        "typing 's' while editing must type it, not trigger save"
    );
    assert_eq!(
        s.editing.as_ref().map(|(_, e)| e.text()),
        Some("s".to_owned())
    );
}
