//! Generic fuzzy picker overlay shared by the search (`/`) and command palette
//! (`:`) modes: a query line plus a filtered, selectable result list.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::tui::events::FuzzyFinder;
use crate::tui::theme::Theme;

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
}

/// Renders the picker as a centered modal over `area`.
pub fn render(frame: &mut Frame, area: Rect, picker: &PickerState, theme: &Theme) {
    let [modal] = Layout::horizontal([Constraint::Length(50)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Length(14)])
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
}
