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

/// M8.5.2: `l`/`Tab`/`→` on the Cookies row with an EMPTY jar used to fall
/// through to a silent no-op — the footer still advertised `l` with nothing
/// happening. It must speak up instead, and name the actual add keybind.
#[test]
fn empty_cookie_list_l_shows_an_actionable_message() {
    let mut s = state_no_cookies();
    goto_network_cookies(&mut s);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('l'))),
        SettingsOutcome::Consumed
    );
    assert_eq!(
        s.focus,
        PanelFocus::Rows,
        "still no descent — the jar is empty"
    );
    assert_eq!(
        s.message.as_deref(),
        Some("no cookies in the jar — press a to add one")
    );
}

/// Same message for `Tab` and `Right`, the other two keys the guarded-descent
/// arm accepts.
#[test]
fn empty_cookie_list_tab_and_right_show_the_same_message() {
    for code in [KeyCode::Tab, KeyCode::Right] {
        let mut s = state_no_cookies();
        goto_network_cookies(&mut s);
        s.handle_key(key(code));
        assert_eq!(
            s.message.as_deref(),
            Some("no cookies in the jar — press a to add one")
        );
    }
}

/// A non-empty jar still descends normally — the message is an EMPTY-jar-only
/// path, not a general replacement of the descend behaviour.
#[test]
fn nonempty_cookie_list_l_still_descends_without_a_message() {
    let mut s = state_with_cookies();
    goto_network_cookies(&mut s);
    s.handle_key(key(KeyCode::Char('l')));
    assert_eq!(s.focus, PanelFocus::CookieList);
    assert!(s.message.is_none());
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

// ---- Request category: `J`/`K` quick-adjust (M8.5.2) ----

#[test]
fn request_timeout_quick_adjust_steps_by_one_second_and_clamps_at_one() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Timeout row (first)
    let start = s.advanced.timeout_secs;
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::TimeoutSecs,
            value: start + 1,
        },
        "J must NOT open the editor — it applies directly"
    );
    assert!(s.editing.is_none());
    s.advanced.timeout_secs = 1;
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::TimeoutSecs,
            value: 1,
        },
        "K must clamp at the same positive-whole-number floor commit_edit enforces"
    );
}

#[test]
fn request_max_body_quick_adjust_steps_by_one_mb() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('j'))); // -> MaxBodyBytes row
    let start = s.advanced.body_cap_bytes;
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::BodyCapBytes,
            value: start + MB,
        }
    );
    s.advanced.body_cap_bytes = start;
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::BodyCapBytes,
            value: start - MB,
        }
    );
}

#[test]
fn request_redirect_quick_adjust_cycles_both_directions() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..2 {
        s.handle_key(key(KeyCode::Char('j'))); // -> Redirect row
    }
    assert_eq!(s.request_row, RequestRow::Redirect);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyRedirect(RedirectPolicy::Strict)
    );
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ApplyRedirect(RedirectPolicy::Strip),
        "K undoes the J that preceded it"
    );
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ApplyRedirect(RedirectPolicy::FollowAll),
        "K alone cycles backward"
    );
}

#[test]
fn request_url_edit_and_secret_policy_quick_adjust_toggle_either_direction() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter));
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Char('j'))); // -> UrlEdit row
    }
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ApplyUrlEdit(UrlEditMode::Popup),
        "a 2-state toggle flips on K too, not just J"
    );
    s.handle_key(key(KeyCode::Char('j'))); // -> SecretPolicy row
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
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

#[test]
fn load_cap_quick_adjust_steps_by_family_and_clamps_at_one() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j')));
    s.handle_key(key(KeyCode::Char('j'))); // -> Load
    s.handle_key(key(KeyCode::Enter)); // -> Panel, WarnTotal row (a "total": step 10)
    assert_eq!(s.load_row, LoadRow::WarnTotal);
    let start = s.load_row.get(&s.load_caps);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyLoadCap {
            field: LoadRow::WarnTotal,
            value: start + 10,
        }
    );
    s.handle_key(key(KeyCode::Char('j'))); // -> WarnConcurrency (step 1)
    assert_eq!(s.load_row, LoadRow::WarnConcurrency);
    let start = s.load_row.get(&s.load_caps);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyLoadCap {
            field: LoadRow::WarnConcurrency,
            value: start + 1,
        }
    );
    // Clamp: decrementing to/through zero stays at 1.
    s.load_caps.warn_concurrency = 1;
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ApplyLoadCap {
            field: LoadRow::WarnConcurrency,
            value: 1,
        }
    );
}

// ---- Network category: `J`/`K` quick-adjust (M8.5.2) ----

#[test]
fn network_tls_and_cookies_quick_adjust_toggle() {
    let mut s = state_no_cookies();
    goto_network_cookies(&mut s); // -> Panel, Cookies row (via Proxy -> Tls -> Cookies)
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ToggleCookies
    );
    s.handle_key(key(KeyCode::Char('k'))); // Cookies -> Tls
    assert_eq!(s.network_row, NetworkRow::Tls);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ToggleInsecure
    );
}

/// Proxy is free text — `J`/`K` has nothing to quick-adjust there.
#[test]
fn network_proxy_quick_adjust_is_a_no_op() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j'))); // -> Network
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Proxy row
    assert_eq!(s.network_row, NetworkRow::Proxy);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::Consumed
    );
    assert!(s.editing.is_none(), "must not open the proxy editor either");
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

/// A 2-state toggle: `J` and `K` both flip it, without opening anything.
#[test]
fn theme_row_quick_adjust_toggles_on_either_key() {
    let mut s = state_no_cookies();
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter)); // -> Panel (Appearance/Theme)
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyTheme("light".to_owned())
    );
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ApplyTheme("dark".to_owned())
    );
}

/// LeaderKey has no quick-adjust — free text, not a number/enum/toggle — so
/// `J`/`K` must be a no-op there rather than entering capture mode.
#[test]
fn leader_key_row_quick_adjust_is_a_no_op() {
    let mut s = state_no_cookies();
    goto_appearance_leader_key(&mut s);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::Consumed
    );
    assert!(!s.capturing_leader_key);
    assert!(s.editing.is_none());
}

/// Drives a fresh state to the Appearance category's LeaderKey row, panel
/// level.
fn goto_appearance_leader_key(s: &mut SettingsState) {
    for _ in 0..3 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter)); // -> Panel (Appearance/Theme)
    s.handle_key(key(KeyCode::Char('j'))); // -> LeaderKey row
}

#[test]
fn leader_key_enter_opens_capture_mode_not_the_editor() {
    let mut s = state_no_cookies();
    goto_appearance_leader_key(&mut s);
    assert_eq!(s.handle_key(key(KeyCode::Enter)), SettingsOutcome::Consumed);
    assert!(
        s.capturing_leader_key,
        "Enter must start capture mode, not open the free-type editor directly"
    );
    assert!(s.editing.is_none());
}

/// The primary path: a captured `KeyEvent` (with modifiers) normalizes
/// straight to its combo string and registers — no typing involved.
#[test]
fn leader_key_capture_registers_a_modifier_chord() {
    let mut s = state_no_cookies();
    goto_appearance_leader_key(&mut s);
    s.handle_key(key(KeyCode::Enter)); // -> capture mode
    let ctrl_b = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
    // crokey's `Display` capitalizes the modifier prefix (`Ctrl-`, same as
    // the `Shift-d` shown in the leader which-key popup) but leaves the base
    // key's own case alone — this is the SAME normalized string
    // `KeyCombination::from_str` accepts back, so it round-trips.
    assert_eq!(
        s.handle_key(ctrl_b),
        SettingsOutcome::ApplyLeaderKey("Ctrl-b".to_owned())
    );
    assert!(
        !s.capturing_leader_key,
        "capture mode ends once a key is registered"
    );
}

/// Esc cancels the capture prompt with no change and no outcome — the
/// leader-key value (and, downstream, its touched state) is untouched.
#[test]
fn leader_key_capture_esc_cancels_without_change() {
    let mut s = state_no_cookies();
    goto_appearance_leader_key(&mut s);
    let before = s.leader_key.clone();
    s.handle_key(key(KeyCode::Enter)); // -> capture mode
    assert_eq!(s.handle_key(key(KeyCode::Esc)), SettingsOutcome::Consumed);
    assert!(!s.capturing_leader_key, "esc must end capture mode");
    assert!(s.editing.is_none());
    assert_eq!(
        s.leader_key, before,
        "esc must leave the leader key unchanged"
    );
}

/// `Tab` from capture mode falls back to the free-type editor (for a chord a
/// terminal can't emit as one `KeyEvent`), seeded with the current value —
/// exercising the SAME validate-and-apply path the editor always has.
#[test]
fn leader_key_edit_validates_and_applies() {
    let mut s = state_no_cookies();
    goto_appearance_leader_key(&mut s);
    s.handle_key(key(KeyCode::Enter)); // -> capture mode
    assert_eq!(s.handle_key(key(KeyCode::Tab)), SettingsOutcome::Consumed);
    assert!(
        !s.capturing_leader_key && s.editing.is_some(),
        "tab must switch from capture mode to the free-type editor"
    );
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
    goto_appearance_leader_key(&mut s);
    s.handle_key(key(KeyCode::Enter)); // -> capture mode
    s.handle_key(key(KeyCode::Tab)); // -> free-type editor
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
fn debug_toggle_row_quick_adjust_flips_either_direction() {
    let mut s = state_debug_on();
    for _ in 0..4 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter)); // -> Panel, DebugToggle row
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ToggleDebug
    );
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ToggleDebug
    );
}

/// The Advanced row is a submenu link, not a knob — `J`/`K` there is a no-op
/// (the actual knobs quick-adjust from inside the field list, tested below).
#[test]
fn debug_advanced_row_quick_adjust_is_a_no_op() {
    let mut s = state_debug_on();
    for _ in 0..4 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    s.handle_key(key(KeyCode::Char('j'))); // -> Advanced row
    assert_eq!(s.debug_row, DebugRow::Advanced);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::Consumed
    );
    assert_eq!(
        s.focus,
        PanelFocus::Rows,
        "must not descend into the list either"
    );
}

#[test]
fn advanced_field_quick_adjust_steps_by_family() {
    let mut s = state_debug_on();
    for _ in 0..4 {
        s.handle_key(key(KeyCode::Char('j')));
    }
    s.handle_key(key(KeyCode::Enter)); // -> Panel
    s.handle_key(key(KeyCode::Char('j'))); // -> Advanced row
    s.handle_key(key(KeyCode::Tab)); // -> AdvancedList, Concurrency field (step 1)
    assert_eq!(s.advanced_field, AdvancedField::Concurrency);
    let start = s.advanced_field.get(&s.advanced);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::Concurrency,
            value: start + 1,
        }
    );
    s.handle_key(key(KeyCode::Char('j'))); // -> Total (step 10)
    assert_eq!(s.advanced_field, AdvancedField::Total);
    let start = s.advanced_field.get(&s.advanced);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::Total,
            value: start + 10,
        }
    );
    s.handle_key(key(KeyCode::Char('j'))); // -> BodyCapBytes (step 1 MB)
    assert_eq!(s.advanced_field, AdvancedField::BodyCapBytes);
    let start = s.advanced_field.get(&s.advanced);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('J'))),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::BodyCapBytes,
            value: start + MB,
        }
    );
    s.handle_key(key(KeyCode::Char('j'))); // -> TimeoutSecs (step 1)
    assert_eq!(s.advanced_field, AdvancedField::TimeoutSecs);
    let start = s.advanced_field.get(&s.advanced);
    assert_eq!(
        s.handle_key(key(KeyCode::Char('K'))),
        SettingsOutcome::ApplyAdvanced {
            field: AdvancedField::TimeoutSecs,
            value: start.saturating_sub(1).max(1),
        }
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

/// Renders `s` to a wide-enough buffer (long descriptions must not clip) and
/// joins it into one searchable string.
fn render_text(s: &SettingsState) -> String {
    use crate::tui::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    let theme = Theme::dark();
    let backend = TestBackend::new(200, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, Rect::new(0, 0, 200, 30), s, &theme))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..30)
        .map(|y| {
            (0..200)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The footer's freed description space (M8.5.2) tracks the hovered row —
/// moving off it must drop the old text, not leave it lingering.
#[test]
fn footer_description_matches_the_hovered_row_and_changes_with_it() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Request/Timeout row

    let text = render_text(&s);
    assert!(
        text.contains("How long to wait for a response"),
        "Timeout row's description must show:\n{text}"
    );

    s.handle_key(key(KeyCode::Char('j'))); // -> MaxBodyBytes row
    let text = render_text(&s);
    assert!(
        text.contains("Maximum response body size"),
        "the description must change with the hovered row:\n{text}"
    );
    assert!(
        !text.contains("How long to wait for a response"),
        "the OLD row's description must not linger:\n{text}"
    );
}

/// A known security-sensitive knob (TLS verification off) must surface its
/// security note in the description, not just a bare "on/off" value.
#[test]
fn tls_row_description_surfaces_a_security_note() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Char('j'))); // -> Network
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Proxy row
    s.handle_key(key(KeyCode::Char('j'))); // -> Tls row
    assert_eq!(s.network_row, NetworkRow::Tls);

    let text = render_text(&s);
    assert!(
        text.contains("accepts ANY certificate"),
        "the TLS row's description must carry a security note about turning \
         verification off:\n{text}"
    );
}

/// Mid-edit and mid-capture, the description row goes quiet rather than
/// showing a stale or misleading line (the editor/capture prompt already
/// occupies the row it describes).
#[test]
fn footer_description_is_blank_while_editing_or_capturing() {
    let mut s = state_no_cookies();
    s.handle_key(key(KeyCode::Enter)); // -> Panel, Timeout row
    let with_row_hovered = render_text(&s);
    assert!(with_row_hovered.contains("How long to wait for a response"));

    s.handle_key(key(KeyCode::Enter)); // begin editing Timeout
    let while_editing = render_text(&s);
    assert!(
        !while_editing.contains("How long to wait for a response"),
        "the description must not linger once the editor is open:\n{while_editing}"
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

// ---- max-body-bytes MB/KB display + parse (M8.5.2) ----

#[test]
fn format_body_cap_picks_the_largest_clean_unit() {
    assert_eq!(format_body_cap(10 * MB), "10 MB");
    assert_eq!(format_body_cap(512 * KB), "512 KB");
    assert_eq!(format_body_cap(1), "1 bytes");
    assert_eq!(format_body_cap(0), "0 bytes");
    // Not a whole multiple of either unit — falls back to bytes.
    assert_eq!(format_body_cap(MB + 1), "1048577 bytes");
}

#[test]
fn parse_body_cap_accepts_mb_kb_and_bare_bytes() {
    assert_eq!(parse_body_cap("10MB"), Some(10 * MB));
    assert_eq!(
        parse_body_cap("10mb"),
        Some(10 * MB),
        "unit is case-insensitive"
    );
    assert_eq!(
        parse_body_cap("10 MB"),
        Some(10 * MB),
        "a space before the unit is fine"
    );
    assert_eq!(parse_body_cap("512KB"), Some(512 * KB));
    assert_eq!(
        parse_body_cap("1048576"),
        Some(MB),
        "a bare number is bytes — back-compat with the old raw-bytes input"
    );
    assert_eq!(
        parse_body_cap("1 bytes"),
        Some(1),
        "format_body_cap's own bytes fallback must parse back"
    );
    assert_eq!(parse_body_cap("5 byte"), Some(5));
}

#[test]
fn parse_body_cap_rejects_garbage() {
    assert_eq!(parse_body_cap(""), None);
    assert_eq!(parse_body_cap("   "), None);
    assert_eq!(parse_body_cap("MB"), None, "a unit with no number");
    assert_eq!(parse_body_cap("ten MB"), None);
    assert_eq!(parse_body_cap("-5"), None, "no negative sizes");
    assert_eq!(parse_body_cap("5GB"), None, "GB is not a supported unit");
}

/// `format_body_cap` output always round-trips back through `parse_body_cap`
/// — the seed a user sees when opening the editor must parse as itself.
#[test]
fn body_cap_format_and_parse_round_trip() {
    for bytes in [MB, 10 * MB, KB, 512 * KB, 1, 0, MB + 1] {
        assert_eq!(
            parse_body_cap(&format_body_cap(bytes)),
            Some(bytes),
            "format_body_cap({bytes}) = {:?} must parse back to {bytes}",
            format_body_cap(bytes)
        );
    }
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
