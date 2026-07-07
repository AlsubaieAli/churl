//! Jump-mode: single-char labels over jump targets (à la EasyMotion / Helix
//! `gw`). Entering jump-mode assigns mnemonic labels to the four panes
//! (`e`xplorer, `u`rl bar, `r`equest, re`s`ponse) and home-row-first labels to
//! each *visible* explorer row; pressing a label focuses that target (and, for an
//! endpoint row, selects it — same as Enter). It is an overlay-level mode: it
//! consumes every key (routing precedence slot 1, alongside Search/Palette).

use super::super::app::Pane;

/// Fixed mnemonic labels for the panes: `e`xplorer, `u`rl bar, `r`equest,
/// re`s`ponse (owner-chosen, review round 3).
pub const PANE_LABELS: &[(char, Pane)] = &[
    ('e', Pane::Explorer),
    ('u', Pane::UrlBar),
    ('r', Pane::Request),
    ('s', Pane::Response),
];

/// The home-row-first row-label alphabet — the full alphabet minus the pane
/// mnemonics. Targets beyond its length are unlabelled (visible rows only, so
/// the panes + a screenful of rows always fit).
pub const LABELS: &[char] = &[
    'a', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'q', 'w', 't', 'y', 'i', 'o', 'p', 'z', 'x', 'c', 'v',
    'b', 'n', 'm',
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
    /// Assigns the fixed pane mnemonics, then row labels to explorer rows
    /// starting at `first_row` (the scroll offset, so labels land on the
    /// viewport rather than offscreen rows at the top of a scrolled tree) up to
    /// `row_count`, capping at the available row-label alphabet.
    pub fn new(first_row: usize, row_count: usize) -> Self {
        let mut labels: Vec<(char, JumpTarget)> = PANE_LABELS
            .iter()
            .map(|&(c, pane)| (c, JumpTarget::Pane(pane)))
            .collect();
        let rows = (first_row..row_count).map(JumpTarget::Row);
        labels.extend(LABELS.iter().copied().zip(rows));
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
    fn panes_get_mnemonic_labels() {
        let state = JumpState::new(0, 0);
        assert_eq!(state.labels.len(), 4);
        assert_eq!(state.label_for_pane(Pane::Explorer), Some('e'));
        assert_eq!(state.label_for_pane(Pane::UrlBar), Some('u'));
        assert_eq!(state.label_for_pane(Pane::Request), Some('r'));
        assert_eq!(state.label_for_pane(Pane::Response), Some('s'));
    }

    #[test]
    fn row_alphabet_excludes_pane_mnemonics() {
        for (c, _) in PANE_LABELS {
            assert!(
                !LABELS.contains(c),
                "{c:?} is both a pane mnemonic and a row label"
            );
        }
    }

    #[test]
    fn rows_follow_the_panes() {
        let state = JumpState::new(0, 2);
        assert_eq!(state.labels.len(), 6);
        // Rows use the row alphabet from its start.
        assert_eq!(state.label_for_row(0), Some('a'));
        assert_eq!(state.label_for_row(1), Some('d'));
        // A label resolves back to its target.
        assert_eq!(
            state.target_for('e'),
            Some(JumpTarget::Pane(Pane::Explorer))
        );
        assert_eq!(state.target_for('a'), Some(JumpTarget::Row(0)));
        // An unassigned char resolves to nothing.
        assert_eq!(state.target_for('Z'), None);
    }

    #[test]
    fn labels_start_at_the_scroll_offset() {
        // A scrolled tree: labels must land on the viewport (rows 5..), not on
        // the offscreen rows at the top.
        let state = JumpState::new(5, 10);
        assert_eq!(state.label_for_row(4), None);
        assert_eq!(state.label_for_row(5), Some('a'));
        assert_eq!(state.label_for_row(9), Some('h'));
        assert_eq!(state.target_for('a'), Some(JumpTarget::Row(5)));
    }

    #[test]
    fn labels_are_exhausted_gracefully() {
        // More rows than the alphabet: only the labelled targets are kept.
        let state = JumpState::new(0, 100);
        assert_eq!(state.labels.len(), PANE_LABELS.len() + LABELS.len());
        let last_row = LABELS.len() - 1;
        assert!(state.label_for_row(last_row).is_some());
        assert!(state.label_for_row(last_row + 1).is_none());
    }
}
