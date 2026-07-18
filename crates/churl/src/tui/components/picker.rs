//! Generic fuzzy picker overlay shared by the search (`/`) and command palette
//! (`:`) modes: a query line plus a filtered, selectable result list.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::tui::events::FuzzyFinder;
use crate::tui::theme::Theme;

/// What routing a key into a [`PickerState`] asks the caller to do next. The
/// navigation + query editing is handled internally ([`PickerKey::Consumed`]);
/// only the two terminal signals — accept the current selection or cancel — are
/// handed back, because what "accept"/"cancel" *mean* differs per overlay (the
/// app search picker loads an endpoint, the sequence editor's add-step picker
/// appends a step). Centralising the key semantics here is what lets every
/// picker share one nav story (e.g. the Ctrl-j/k vim aliases) — a fix lands once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKey {
    /// The key was handled internally (navigation, query edit, or ignored).
    Consumed,
    /// Enter — the caller should act on the current selection.
    Accept,
    /// Esc — the caller should close/cancel the picker.
    Cancel,
}

/// State of an open picker overlay.
#[derive(Debug)]
pub struct PickerState {
    /// Overlay title (e.g. `" Search "`).
    pub title: &'static str,
    /// All candidate display strings.
    pub items: Vec<String>,
    /// Indices into `items` matching the query, best first.
    pub filtered: Vec<usize>,
    /// Current query string.
    pub query: String,
    /// Selection as an index into `filtered`.
    pub selected: usize,
}

impl PickerState {
    /// Creates a picker over `items` with an empty query (everything matches).
    pub fn new(title: &'static str, items: Vec<String>) -> Self {
        let filtered = (0..items.len()).collect();
        Self {
            title,
            items,
            filtered,
            query: String::new(),
            selected: 0,
        }
    }

    /// Appends a character to the query and refilters.
    pub fn push_char(&mut self, c: char, finder: &mut FuzzyFinder) {
        self.query.push(c);
        self.refilter(finder);
    }

    /// Appends a whole string to the query and refilters once (a pasted filter
    /// term). Empty input is a no-op that still leaves the query unchanged.
    pub fn push_str(&mut self, s: &str, finder: &mut FuzzyFinder) {
        if s.is_empty() {
            return;
        }
        self.query.push_str(s);
        self.refilter(finder);
    }

    /// Deletes the last query character and refilters.
    pub fn backspace(&mut self, finder: &mut FuzzyFinder) {
        self.query.pop();
        self.refilter(finder);
    }

    fn refilter(&mut self, finder: &mut FuzzyFinder) {
        self.filtered = finder.filter(&self.query, &self.items);
        self.selected = 0;
    }

    /// Moves the selection up (clamped).
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Moves the selection down (clamped).
    pub fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    /// Returns the item index of the current selection, if any.
    pub fn current(&self) -> Option<usize> {
        self.filtered.get(self.selected).copied()
    }

    /// Routes one key into the picker: navigation (↑/↓, plus the Ctrl-p/n and
    /// Ctrl-k/j vim aliases), query editing (printable chars + Backspace,
    /// refiltered through `finder`), and the two terminal signals — Enter →
    /// [`PickerKey::Accept`], Esc → [`PickerKey::Cancel`]. Everything the
    /// caller must interpret (what to do on accept/cancel) is returned; the
    /// rest is [`PickerKey::Consumed`]. This is the single home of picker key
    /// semantics, shared by every overlay.
    pub fn handle_key(&mut self, key: KeyEvent, finder: &mut FuzzyFinder) -> PickerKey {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return PickerKey::Cancel,
            KeyCode::Enter => return PickerKey::Accept,
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Char('p') if ctrl => self.move_up(),
            KeyCode::Char('n') if ctrl => self.move_down(),
            KeyCode::Char('k') if ctrl => self.move_up(),
            KeyCode::Char('j') if ctrl => self.move_down(),
            KeyCode::Backspace => self.backspace(finder),
            KeyCode::Char(c) if !ctrl => self.push_char(c, finder),
            _ => {}
        }
        PickerKey::Consumed
    }
}

/// Picks a modal size proportional to the terminal: ~70% of each dimension,
/// width clamped to [50, 120] and height to [14, area.height - 4], but never
/// exceeding `area` itself.
///
/// The upper bound is computed first and the lower bound is clamped against it,
/// so `u16::clamp` is never called with `min > max` (which would panic). On a
/// 1x1 terminal, `w_hi = 1`, so `clamp(50.min(1)=1, 1) = 1`; likewise
/// `h_hi = max(1, 1-4)=1`, so `clamp(14.min(1)=1, 1) = 1`.
fn modal_size(area: Rect) -> (u16, u16) {
    let w_hi = area.width.clamp(1, 120);
    let w_prop = (u32::from(area.width) * 70 / 100) as u16;
    let width = w_prop.clamp(50.min(w_hi), w_hi);

    let h_hi = area.height.saturating_sub(4).max(1);
    let h_prop = (u32::from(area.height) * 70 / 100) as u16;
    let height = h_prop.clamp(14.min(h_hi), h_hi);

    (width, height)
}

/// Renders the picker as a centered modal over `area`.
pub fn render(frame: &mut Frame, area: Rect, picker: &PickerState, theme: &Theme) {
    let (width, height) = modal_size(area);
    let [modal] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(picker.title)
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let [query_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(format!("> {}█", picker.query))),
        query_area,
    );

    let visible = list_area.height as usize;
    // Keep the selection in view.
    let offset = picker.selected.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = picker
        .filtered
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(pos, &item)| {
            let cursor = if pos == picker.selected { "> " } else { "  " };
            let line = Line::from(format!("{cursor}{}", picker.items[item]));
            if pos == picker.selected {
                line.style(theme.selection)
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_clamps_and_tracks_filter() {
        let mut finder = FuzzyFinder::new();
        let mut picker = PickerState::new(
            " Test ",
            vec!["alpha".into(), "beta".into(), "gamma".into()],
        );
        assert_eq!(picker.current(), Some(0));
        picker.move_up();
        assert_eq!(picker.selected, 0);
        picker.move_down();
        picker.move_down();
        picker.move_down();
        assert_eq!(picker.selected, 2);

        picker.push_char('a', &mut finder);
        assert_eq!(picker.selected, 0, "filtering resets the selection");
        picker.push_char('l', &mut finder);
        assert_eq!(picker.current(), Some(0), "only alpha matches 'al'");
        assert_eq!(picker.filtered, vec![0]);

        picker.backspace(&mut finder);
        picker.backspace(&mut finder);
        assert_eq!(picker.filtered.len(), 3, "empty query matches everything");
    }

    #[test]
    fn modal_size_is_proportional_and_clamped() {
        // Large terminal: ~70% within [50,120] / [14, h-4].
        let (w, h) = modal_size(Rect::new(0, 0, 200, 60));
        assert_eq!(w, 120, "width clamped to max 120");
        assert_eq!(h, 42, "60 * 70% = 42, under the h-4=56 ceiling");

        // Mid terminal: proportional, no clamping.
        let (w, h) = modal_size(Rect::new(0, 0, 100, 30));
        assert_eq!(w, 70);
        assert_eq!(h, 21);

        // Small terminal: floors kick in but never exceed the area.
        let (w, h) = modal_size(Rect::new(0, 0, 60, 16));
        assert_eq!(w, 50, "width floored to 50");
        assert_eq!(h, 12, "h-4 ceiling (12) beats the 14 floor");
        assert!(w <= 60 && h <= 16);
    }

    #[test]
    fn modal_size_never_panics_on_tiny_terminals() {
        // These exercise the min>max guard in clamp; a bad formula panics here.
        for (w, h) in [(0, 0), (1, 1), (2, 2), (49, 13), (50, 14), (51, 18)] {
            let (mw, mh) = modal_size(Rect::new(0, 0, w, h));
            assert!(mw <= w.max(1), "width fits area for {w}x{h}");
            assert!(mh <= h.max(1), "height fits area for {w}x{h}");
        }
    }
}
