//! Jump-mode: single-char labels over the pane regions (à la EasyMotion /
//! Helix `gw`). Entering jump-mode assigns one mnemonic label to each of the
//! five focusable regions — the endpoints tree, the sequences sub-pane, the URL
//! bar, the request editor and the response viewer — and pressing a label
//! focuses that region. It is an overlay-level mode: it consumes every key
//! (routing precedence slot 1, alongside Search/Palette).
//!
//! `f`-jump is **pane-only** (M7.10 stage B, owner decision): it labels no
//! endpoint rows. Row-precision navigation is the leader pickers' job —
//! `<leader>f` (endpoints) and `<leader>s f` (sequences).

use super::super::app::Pane;

/// What a jump label points at. Five regions, no rows (M7.10 stage B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpTarget {
    /// Focus one of the four top-level [`Pane`]s.
    Pane(Pane),
    /// Focus the left column and switch it to the sequences sub-pane. Modelled
    /// off the [`Pane`] axis because the sequences sub-pane lives *inside*
    /// `Pane::Explorer` (see [`super::super::app::LeftPane`]).
    Sequences,
}

/// Fixed mnemonic labels for the five regions, in assignment order:
/// `e`ndpoints/explorer, `s`equences, `u`rl bar, `r`equest, res`p`onse.
///
/// `s` moved off Response (M7.10 stage B — it now mnemonically labels the new
/// **s**equences region), and Response took `p` (res**p**onse) so all five
/// labels stay distinct single keys.
pub const PANE_LABELS: &[(char, JumpTarget)] = &[
    ('e', JumpTarget::Pane(Pane::Explorer)),
    ('s', JumpTarget::Sequences),
    ('u', JumpTarget::Pane(Pane::UrlBar)),
    ('r', JumpTarget::Pane(Pane::Request)),
    ('p', JumpTarget::Pane(Pane::Response)),
];

/// Active jump-mode state: the region labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpState {
    /// `(label char, target)` pairs, in assignment order.
    pub labels: Vec<(char, JumpTarget)>,
}

impl Default for JumpState {
    fn default() -> Self {
        Self::new()
    }
}

impl JumpState {
    /// Builds the fixed five-region label set. Pane-only — no row labels.
    pub fn new() -> Self {
        Self {
            labels: PANE_LABELS.to_vec(),
        }
    }

    /// Resolves a pressed character to its target, if any.
    pub fn target_for(&self, c: char) -> Option<JumpTarget> {
        self.labels
            .iter()
            .find(|(label, _)| *label == c)
            .map(|(_, target)| *target)
    }

    /// The label char assigned to a pane, if any.
    pub fn label_for_pane(&self, pane: Pane) -> Option<char> {
        self.labels.iter().find_map(|(label, target)| {
            matches!(target, JumpTarget::Pane(p) if *p == pane).then_some(*label)
        })
    }

    /// The label char assigned to the sequences sub-pane.
    pub fn label_for_sequences(&self) -> Option<char> {
        self.labels
            .iter()
            .find_map(|(label, target)| matches!(target, JumpTarget::Sequences).then_some(*label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_the_five_regions() {
        let state = JumpState::new();
        assert_eq!(state.labels.len(), 5);
        assert_eq!(state.label_for_pane(Pane::Explorer), Some('e'));
        assert_eq!(state.label_for_sequences(), Some('s'));
        assert_eq!(state.label_for_pane(Pane::UrlBar), Some('u'));
        assert_eq!(state.label_for_pane(Pane::Request), Some('r'));
        assert_eq!(state.label_for_pane(Pane::Response), Some('p'));
    }

    #[test]
    fn all_five_labels_are_distinct() {
        let state = JumpState::new();
        let mut chars: Vec<char> = state.labels.iter().map(|(c, _)| *c).collect();
        chars.sort_unstable();
        chars.dedup();
        assert_eq!(chars.len(), 5, "the five region labels must be distinct");
    }

    #[test]
    fn labels_resolve_back_to_their_targets() {
        let state = JumpState::new();
        assert_eq!(
            state.target_for('e'),
            Some(JumpTarget::Pane(Pane::Explorer))
        );
        assert_eq!(state.target_for('s'), Some(JumpTarget::Sequences));
        assert_eq!(
            state.target_for('p'),
            Some(JumpTarget::Pane(Pane::Response))
        );
        // An unassigned char resolves to nothing.
        assert_eq!(state.target_for('Z'), None);
        // Row-label alphabet chars no longer resolve (pane-only, zero rows).
        assert_eq!(state.target_for('a'), None);
        assert_eq!(state.target_for('d'), None);
    }
}
