//! The nucleo-backed [`FuzzyFinder`] over a list of display strings. Split out of
//! the events module as a child module.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Fuzzy matcher over a list of display strings, wrapping `nucleo-matcher`
/// (the sync engine used by Helix — the threaded `nucleo` crate is overkill for
/// workspace-sized endpoint lists).
pub struct FuzzyFinder {
    matcher: Matcher,
}

impl Default for FuzzyFinder {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FuzzyFinder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuzzyFinder").finish_non_exhaustive()
    }
}

impl FuzzyFinder {
    /// Creates a finder with the default matcher configuration.
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// Returns the indices of `items` matching `query`, best score first
    /// (original order as tie-break). An empty query returns all indices in
    /// their original order.
    pub fn filter(&mut self, query: &str, items: &[String]) -> Vec<usize> {
        if query.is_empty() {
            return (0..items.len()).collect();
        }
        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                pattern
                    .score(Utf32Str::new(item, &mut buf), &mut self.matcher)
                    .map(|score| (index, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.into_iter().map(|(index, _)| index).collect()
    }
}
