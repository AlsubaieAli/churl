//! The cookie add/edit form: a small multi-field sub-state of the Network
//! category's Cookies row (`a` add · `e` edit selected, from either the row
//! or the cookie list). Modeled illegal-states-unrepresentable the same way
//! the panel's own `EditTarget`/`LineEditor` pair is — a [`CookieForm`] can't
//! exist without exactly the fields an add/edit needs, and its own
//! `focus`/`editing` pair the same way.
//!
//! Split out of `mod.rs` as its own sibling module (not folded into `edit.rs`)
//! because it is a self-contained state machine with no cross-dependencies on
//! the rest of the panel's row/list handling — same "several clusters, no
//! cross-dependencies → sibling module" call as `edit.rs`/`render.rs` already
//! made.

use churl_core::cookies::{CookieView, SameSite};
use crossterm::event::{KeyCode, KeyEvent};

use super::{LineEditor, SettingsOutcome, SettingsState};

/// Which field of the open cookie form has focus. Domain/Name/Value/Path pair
/// with the shared [`LineEditor`] the same way the panel's other text edits
/// do; Secure/SameSite toggle/cycle in place — no text entry needed, so they
/// never open an editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieFormField {
    /// The cookie's domain (required, non-empty).
    Domain,
    /// The cookie's name (required, non-empty).
    Name,
    /// The cookie's value (credential-shaped — stays masked at rest, exactly
    /// like the cookie list; see [`super::render`]).
    Value,
    /// The cookie's path (blank defaults to `/`).
    Path,
    /// The `Secure` attribute — toggled in place.
    Secure,
    /// The `SameSite` attribute — cycled in place through absent → Strict →
    /// Lax → the RFC `SameSite=None` value → absent.
    SameSite,
}

impl CookieFormField {
    const ALL: [CookieFormField; 6] = [
        CookieFormField::Domain,
        CookieFormField::Name,
        CookieFormField::Value,
        CookieFormField::Path,
        CookieFormField::Secure,
        CookieFormField::SameSite,
    ];

    fn next(self) -> Self {
        super::cycle_next(&Self::ALL, self)
    }

    fn prev(self) -> Self {
        super::cycle_prev(&Self::ALL, self)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            CookieFormField::Domain => "Domain",
            CookieFormField::Name => "Name",
            CookieFormField::Value => "Value",
            CookieFormField::Path => "Path",
            CookieFormField::Secure => "Secure",
            CookieFormField::SameSite => "SameSite",
        }
    }
}

/// Cycles the SameSite working value through every state an edit form needs:
/// attribute absent, then each of the three RFC values, then back to absent.
fn next_same_site(current: Option<SameSite>) -> Option<SameSite> {
    const CYCLE: [Option<SameSite>; 4] = [
        None,
        Some(SameSite::Strict),
        Some(SameSite::Lax),
        Some(SameSite::None),
    ];
    super::cycle_next(&CYCLE, current)
}

/// State of an open cookie add/edit form. Held in `Option<CookieForm>` on
/// [`SettingsState`] — `None` means no form is open.
#[derive(Debug, Clone)]
pub struct CookieForm {
    pub domain: String,
    pub name: String,
    pub value: String,
    pub path: String,
    pub secure: bool,
    pub same_site: Option<SameSite>,
    pub focus: CookieFormField,
    /// The one open text edit, if any — always targets [`Self::focus`], so
    /// (like the panel's `editing`) there is no separate "which field" tag to
    /// let drift out of sync.
    pub editing: Option<LineEditor>,
    /// `Some((domain, name, path))` = the ORIGINAL coordinates of the cookie
    /// being edited (its key at open time, before any in-form change) — the
    /// app handler needs these to delete the old entry if the key changed.
    /// `None` = adding a brand-new cookie.
    pub editing_existing: Option<(String, String, String)>,
}

impl CookieForm {
    fn new_add() -> Self {
        Self {
            domain: String::new(),
            name: String::new(),
            value: String::new(),
            path: String::new(),
            secure: false,
            same_site: None,
            focus: CookieFormField::Domain,
            editing: None,
            editing_existing: None,
        }
    }

    fn new_edit(view: &CookieView) -> Self {
        Self {
            domain: view.domain.clone(),
            name: view.name.clone(),
            value: view.value.clone(),
            path: view.path.clone(),
            secure: view.secure,
            same_site: view.same_site,
            focus: CookieFormField::Domain,
            editing: None,
            editing_existing: Some((view.domain.clone(), view.name.clone(), view.path.clone())),
        }
    }

    /// The stored (last-committed) text of a text field; empty for the two
    /// toggle/cycle fields, which never have text to seed an editor with.
    pub(crate) fn text(&self, field: CookieFormField) -> &str {
        match field {
            CookieFormField::Domain => &self.domain,
            CookieFormField::Name => &self.name,
            CookieFormField::Value => &self.value,
            CookieFormField::Path => &self.path,
            CookieFormField::Secure | CookieFormField::SameSite => "",
        }
    }

    fn set_text(&mut self, field: CookieFormField, text: String) {
        match field {
            CookieFormField::Domain => self.domain = text,
            CookieFormField::Name => self.name = text,
            CookieFormField::Value => self.value = text,
            CookieFormField::Path => self.path = text,
            CookieFormField::Secure | CookieFormField::SameSite => {}
        }
    }
}

impl SettingsState {
    /// Opens a blank add form (`a` from the Network category's Cookies row or
    /// its cookie list).
    pub(super) fn open_add_cookie_form(&mut self) {
        self.cookie_form = Some(CookieForm::new_add());
    }

    /// Opens an edit form prefilled from the selected cookie (`e` from the
    /// cookie list). A no-op if nothing is selected (an empty list can't be
    /// entered in the first place — see `handle_cookie_list_key`).
    pub(super) fn open_edit_cookie_form(&mut self) {
        if let Some(view) = self.cookies.get(self.cookie_sel).cloned() {
            self.cookie_form = Some(CookieForm::new_edit(&view));
        }
    }

    /// Handles one key while a cookie form is open (field nav is the only
    /// caller — checked by [`Self::handle_key`] before this is reached).
    pub(super) fn handle_cookie_form_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        if self
            .cookie_form
            .as_ref()
            .is_some_and(|f| f.editing.is_some())
        {
            return self.handle_cookie_form_edit_key(key);
        }
        // 's' (submit) and Esc (cancel the whole form) both need `&mut self`
        // for a follow-up call / to drop the form outright — handled before
        // borrowing `self.cookie_form` below so the two borrows never overlap.
        match key.code {
            KeyCode::Char('s') => return self.commit_cookie_form(),
            KeyCode::Esc => {
                self.cookie_form = None;
                return SettingsOutcome::Consumed;
            }
            _ => {}
        }
        let Some(form) = self.cookie_form.as_mut() else {
            return SettingsOutcome::Consumed;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => form.focus = form.focus.next(),
            KeyCode::Char('k') | KeyCode::Up => form.focus = form.focus.prev(),
            KeyCode::Enter | KeyCode::Char(' ') => match form.focus {
                CookieFormField::Secure => form.secure = !form.secure,
                CookieFormField::SameSite => form.same_site = next_same_site(form.same_site),
                field => {
                    let seed = form.text(field).to_owned();
                    form.editing = Some(LineEditor::new(&seed));
                }
            },
            _ => {}
        }
        SettingsOutcome::Consumed
    }

    /// Keys while a form field's text editor is open — mirrors
    /// [`Self::handle_edit_key`]'s shape (delegate to the editor first, then
    /// Enter commits / Esc cancels), scoped to the current field.
    fn handle_cookie_form_edit_key(&mut self, key: KeyEvent) -> SettingsOutcome {
        let Some(form) = self.cookie_form.as_mut() else {
            return SettingsOutcome::Consumed;
        };
        let Some(editor) = form.editing.as_mut() else {
            return SettingsOutcome::Consumed;
        };
        if editor.handle_key(key) {
            return SettingsOutcome::Consumed;
        }
        match key.code {
            KeyCode::Enter => {
                let text = form.editing.take().expect("checked Some above").text();
                let field = form.focus;
                form.set_text(field, text);
            }
            KeyCode::Esc => form.editing = None,
            _ => {}
        }
        SettingsOutcome::Consumed
    }

    /// Validates and emits the form's [`SettingsOutcome::UpsertCookie`], or
    /// sets an inline error and keeps the form open for correction. Unlike a
    /// single-field commit (which always closes its editor, success or not —
    /// see [`Self::commit_edit`]), an invalid *whole-form* submit deliberately
    /// does NOT discard the form: losing five other filled-in fields over one
    /// bad one would be a needless retype, unlike a single row's trivial
    /// re-open.
    fn commit_cookie_form(&mut self) -> SettingsOutcome {
        let Some(form) = self.cookie_form.as_ref() else {
            return SettingsOutcome::Consumed;
        };
        let domain = form.domain.trim().to_owned();
        if domain.is_empty() {
            self.message = Some("domain cannot be empty".to_owned());
            return SettingsOutcome::Consumed;
        }
        let name = form.name.trim().to_owned();
        if name.is_empty() {
            self.message = Some("name cannot be empty".to_owned());
            return SettingsOutcome::Consumed;
        }
        let path = {
            let trimmed = form.path.trim();
            if trimmed.is_empty() {
                "/".to_owned()
            } else {
                trimmed.to_owned()
            }
        };
        let value = form.value.clone();
        let secure = form.secure;
        let same_site = form.same_site;
        let previous = form.editing_existing.clone();
        self.cookie_form = None;
        SettingsOutcome::UpsertCookie {
            previous,
            domain,
            name,
            value,
            path,
            secure,
            same_site,
        }
    }
}
