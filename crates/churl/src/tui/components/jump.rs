//! Jump-mode: single-char labels over jump targets (à la EasyMotion / Helix
//! `gw`). Entering jump-mode assigns home-row-first labels to the three panes and
//! each *visible* explorer row; pressing a label focuses that target (and, for an
//! endpoint row, selects it — same as Enter). It is an overlay-level mode: it
//! consumes every key (routing precedence slot 1, alongside Search/Palette).

use super::super::app::Pane;

/// The home-row-first label alphabet. Targets beyond its length are unlabelled
/// (visible rows only, so the panes + a screenful of rows always fit).
pub const LABELS: &[char] = &[
    'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p',
    'z', 'x', 'c', 'v', 'b', 'n', 'm',
];

/// What a jump label points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpTarget {
    /// Focus this pane.
    Pane(Pane),
    /// Focus the explorer and move its cursor to this (row) index.
    Row(usize),
}

/// Active jump-mode state: the ordered targets and their assigned label chars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpState {
    /// `(label char, target)` pairs, in assignment order.
    pub labels: Vec<(char, JumpTarget)>,
}

impl JumpState {
    /// Assigns labels to the three panes first, then to explorer rows starting
    /// at `first_row` (the scroll offset, so labels land on the viewport rather
    /// than offscreen rows at the top of a scrolled tree) up to `row_count`,
    /// capping at the available label alphabet.
    pub fn new(first_row: usize, row_count: usize) -> Self {
        let mut targets = vec![
            JumpTarget::Pane(Pane::Explorer),
            JumpTarget::Pane(Pane::UrlBar),
            JumpTarget::Pane(Pane::Request),
            JumpTarget::Pane(Pane::Response),
        ];
        for row in first_row..row_count {
            targets.push(JumpTarget::Row(row));
        }
        let labels = LABELS.iter().copied().zip(targets).collect();
        Self { labels }
    }

    /// Resolves a pressed character to its target, if any.
    pub fn target_for(&self, c: char) -> Option<JumpTarget> {
        self.labels
            .iter()
            .find(|(label, _)| *label == c)
            .map(|(_, target)| *target)
    }

    /// The label char assigned to a pane, if it fit.
    pub fn label_for_pane(&self, pane: Pane) -> Option<char> {
        self.labels.iter().find_map(|(label, target)| {
            matches!(target, JumpTarget::Pane(p) if *p == pane).then_some(*label)
        })
    }

    /// The label char assigned to a visible explorer row index, if it fit.
    pub fn label_for_row(&self, row: usize) -> Option<char> {
        self.labels.iter().find_map(|(label, target)| {
            matches!(target, JumpTarget::Row(r) if *r == row).then_some(*label)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_get_the_first_four_labels() {
        let state = JumpState::new(0, 0);
        assert_eq!(state.labels.len(), 4);
        assert_eq!(state.label_for_pane(Pane::Explorer), Some('a'));
        assert_eq!(state.label_for_pane(Pane::UrlBar), Some('s'));
        assert_eq!(state.label_for_pane(Pane::Request), Some('d'));
        assert_eq!(state.label_for_pane(Pane::Response), Some('f'));
    }

    #[test]
    fn rows_follow_the_panes() {
        let state = JumpState::new(0, 2);
        assert_eq!(state.labels.len(), 6);
        // Four panes take a/s/d/f, so rows start at 'g'.
        assert_eq!(state.label_for_row(0), Some('g'));
        assert_eq!(state.label_for_row(1), Some('h'));
        // A label resolves back to its target.
        assert_eq!(
            state.target_for('a'),
            Some(JumpTarget::Pane(Pane::Explorer))
        );
        assert_eq!(state.target_for('g'), Some(JumpTarget::Row(0)));
        // An unassigned char resolves to nothing.
        assert_eq!(state.target_for('Z'), None);
    }

    #[test]
    fn labels_start_at_the_scroll_offset() {
        // A scrolled tree: labels must land on the viewport (rows 5..), not on
        // the offscreen rows at the top.
        let state = JumpState::new(5, 10);
        assert_eq!(state.label_for_row(4), None);
        // Four panes take a/s/d/f, so the first labelled row is 'g'.
        assert_eq!(state.label_for_row(5), Some('g'));
        assert_eq!(state.label_for_row(9), Some('l'));
        assert_eq!(state.target_for('g'), Some(JumpTarget::Row(5)));
    }

    #[test]
    fn labels_are_exhausted_gracefully() {
        // More rows than the alphabet: only LABELS.len() targets get labels.
        let state = JumpState::new(0, 100);
        assert_eq!(state.labels.len(), LABELS.len());
        // The last labelled row is at index LABELS.len() - 4 - 1 (4 panes first).
        let last_row = LABELS.len() - 4 - 1;
        assert!(state.label_for_row(last_row).is_some());
        assert!(state.label_for_row(last_row + 1).is_none());
    }
}
