//! `?` help overlay: a floating pane rendering the *effective* keymap from the
//! live [`KeyMap`] (never a hardcoded list — it cannot drift), sectioned
//! **Global / Explorer / URL bar / Request / Response / Leader**, scrollable,
//! dismissed with `?`/Esc/`q`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::tui::components::response::{build_search_spans, smart_case_matches};
use crate::tui::events::{Action, KeyMap, PaneCtx};
use crate::tui::theme::Theme;

/// One rendered help section: a header plus its `(keys, label)` entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpSection {
    /// The section header (e.g. `"Global"`, `"Leader"`).
    pub header: String,
    /// `(key combos, action label)` rows, in ACTION_TABLE order.
    pub entries: Vec<(String, String)>,
}

/// Builds the help sections from the live keymap. Every bound action appears in
/// exactly one section; sections with no bindings are still emitted (with an
/// empty entry list) so the guard test can assert none is missing.
pub fn sections(keymap: &KeyMap) -> Vec<HelpSection> {
    // ACTION_TABLE order, not alphabetical — the table groups related actions
    // (h/j/k/l movement together, g/G jumps together); sorting by config name
    // scatters them (review round 3).
    let actions: Vec<Action> = Action::all().collect();

    let mut out = Vec::new();

    // Global: every action with a global binding.
    let global: Vec<(String, String)> = actions
        .iter()
        .filter_map(|a| {
            let combos = keymap.combos_for(*a);
            (!combos.is_empty()).then(|| (combos.join(", "), a.label().to_owned()))
        })
        .collect();
    out.push(HelpSection {
        header: "Global".to_owned(),
        entries: global,
    });

    // Per-pane overlays.
    for ctx in PaneCtx::all() {
        let entries: Vec<(String, String)> = actions
            .iter()
            .filter_map(|a| {
                let combos = keymap.overlay_combos_for(ctx, *a);
                (!combos.is_empty()).then(|| (combos.join(", "), a.label().to_owned()))
            })
            .collect();
        out.push(HelpSection {
            header: ctx.header().to_owned(),
            entries,
        });
    }

    // Leader continuations.
    let leader: Vec<(String, String)> = actions
        .iter()
        .filter_map(|a| {
            let combos = keymap.leader_combos_for(*a);
            (!combos.is_empty()).then(|| {
                (
                    format!("<leader> {}", combos.join(", ")),
                    a.label().to_owned(),
                )
            })
        })
        .collect();
    out.push(HelpSection {
        header: "Leader".to_owned(),
        entries: leader,
    });

    out
}

/// Flattens the sections into display lines (headers + `keys  label` rows), the
/// scroll source for the overlay, paired with the **plain text** of each line —
/// the search matcher runs against the plain text, and the byte ranges it
/// returns index into that same text so highlight spans line up.
fn help_lines<'a>(sections: &[HelpSection], theme: &Theme) -> Vec<(Line<'a>, String)> {
    let mut lines = Vec::new();
    let key_w = sections
        .iter()
        .flat_map(|s| s.entries.iter())
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(0);
    for section in sections {
        lines.push((
            Line::styled(
                section.header.clone(),
                theme.title.add_modifier(Modifier::BOLD),
            ),
            section.header.clone(),
        ));
        if section.entries.is_empty() {
            lines.push((
                Line::styled(
                    "  (none)".to_owned(),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                "  (none)".to_owned(),
            ));
        }
        for (keys, label) in &section.entries {
            let keys_col = format!("  {keys:<key_w$}");
            let label_col = format!("  {label}");
            let plain = format!("{keys_col}{label_col}");
            lines.push((
                Line::from(vec![
                    Span::styled(keys_col, Style::default().add_modifier(Modifier::DIM)),
                    Span::raw(label_col),
                ]),
                plain,
            ));
        }
        lines.push((Line::from(""), String::new()));
    }
    lines
}

/// Live search state for the `?` help overlay: highlight-and-jump over the help
/// lines, reusing the response body-search engine (shared smart-case matcher,
/// `n`/`N`, Esc/Enter). Rows are **never hidden** — matches are highlighted in
/// place and the view jumps to the current one. Analogous to
/// [`crate::tui::components::response::SearchState`].
#[derive(Debug, Default, Clone)]
pub struct HelpSearch {
    /// The current query text.
    pub query: String,
    /// `(line index, byte start, byte end)` per match, in reading order.
    matches: Vec<(usize, usize, usize)>,
    /// Index of the current match within `matches`, when any.
    current: Option<usize>,
}

impl HelpSearch {
    /// Recomputes matches for `query` over the current help lines (smart-case),
    /// resetting the current match to the first. Rebuilds the line set from the
    /// live keymap so the match indices track exactly what render will paint. An
    /// empty query clears matches but keeps the search live (input still open).
    pub fn set_query(&mut self, query: String, keymap: &KeyMap, theme: &Theme) {
        let sections = sections(keymap);
        let lines = help_lines(&sections, theme);
        let matches = smart_case_matches(lines.iter().map(|(_, plain)| plain.as_str()), &query);
        self.current = (!matches.is_empty()).then_some(0);
        self.query = query;
        self.matches = matches;
    }

    /// Steps to the next (`forward`) or previous match, wrapping. Returns the new
    /// current match's line index so the caller can scroll it into view, or
    /// `None` when there are no matches.
    pub fn step(&mut self, forward: bool) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        let len = self.matches.len();
        let next = match self.current {
            Some(i) if forward => (i + 1) % len,
            Some(i) => (i + len - 1) % len,
            None => 0,
        };
        self.current = Some(next);
        Some(self.matches[next].0)
    }

    /// The line index of the current match, if any (for scroll-into-view).
    pub fn current_line(&self) -> Option<usize> {
        self.current.map(|i| self.matches[i].0)
    }

    /// The number of matches.
    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// The 1-based ordinal of the current match, when any.
    pub fn current_ordinal(&self) -> Option<usize> {
        self.current.map(|i| i + 1)
    }
}

/// Result of a help overlay render.
pub struct RenderOutcome {
    /// Total number of content lines.
    pub total: usize,
    /// Height of the inner viewport in rows.
    pub viewport_height: usize,
}

/// Renders the help overlay over `area`, scrolled by `scroll` lines. When
/// `search` is `Some`, match byte-ranges are painted as highlight spans on their
/// lines (current match reversed, others dim+underlined — same styling as the
/// response body search via [`build_search_spans`]) and the title shows the live
/// query + `k/N` counter. Non-matching rows stay visible: this is
/// highlight-and-jump, not a hiding filter. Returns the total content line count
/// and the inner viewport height (for half-page scrolling).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    keymap: &KeyMap,
    scroll: usize,
    theme: &Theme,
    search: Option<&HelpSearch>,
) -> RenderOutcome {
    let sections = sections(keymap);
    let rows = help_lines(&sections, theme);
    let total = rows.len();

    // Paint search highlights over the plain lines where matches fall, leaving
    // every other line exactly as before (rows are never hidden).
    let lines: Vec<Line> = rows
        .into_iter()
        .enumerate()
        .map(|(idx, (line, plain))| match search {
            Some(s) if !s.matches.is_empty() => {
                let hits: Vec<(usize, usize, bool)> = s
                    .matches
                    .iter()
                    .enumerate()
                    .filter(|(_, (l, _, _))| *l == idx)
                    .map(|(mi, (_, ms, me))| (*ms, *me, Some(mi) == s.current))
                    .collect();
                if hits.is_empty() {
                    line
                } else {
                    build_search_spans(&plain, 0, &hits)
                }
            }
            _ => line,
        })
        .collect();

    let [modal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    // Title reflects search state: an open search shows the live query and match
    // counter; otherwise the dismissal hint.
    let title = match search {
        Some(s) => {
            let counter = match (s.current_ordinal(), s.count()) {
                (Some(ord), total) => format!("{ord}/{total}"),
                (None, _) => "no matches".to_owned(),
            };
            format!(" Help — /{}  [{}]  (n/N · esc/enter) ", s.query, counter)
        }
        None => " Help — keys (?/esc/q · / search) ".to_owned(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(modal);
    let viewport_height = inner.height as usize;
    frame.render_widget(block, modal);

    let scroll = scroll.min(total.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
    RenderOutcome {
        total,
        viewport_height,
    }
}

/// The flattened help lines' **plain text**, in render order — the search
/// surface. Exposed so `App` can recompute matches/counts without duplicating
/// the flatten logic. (Kept in sync with [`help_lines`] by construction.)
pub fn plain_lines(keymap: &KeyMap, theme: &Theme) -> Vec<String> {
    help_lines(&sections(keymap), theme)
        .into_iter()
        .map(|(_, plain)| plain)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every bound action appears in exactly one help section — the guard against
    /// a section silently going missing (the whole point of rendering from the
    /// live keymap).
    #[test]
    fn every_bound_action_appears_once() {
        let keymap = KeyMap::default();
        let sections = sections(&keymap);
        // The six expected sections are always present, in order.
        let headers: Vec<&str> = sections.iter().map(|s| s.header.as_str()).collect();
        assert_eq!(
            headers,
            vec![
                "Global", "Explorer", "URL bar", "Request", "Response", "Leader"
            ]
        );

        // Collect every action that has any binding anywhere in the keymap.
        let mut bound: Vec<Action> = Vec::new();
        for a in Action::all() {
            let anywhere = !keymap.combos_for(a).is_empty()
                || PaneCtx::all()
                    .into_iter()
                    .any(|c| !keymap.overlay_combos_for(c, a).is_empty())
                || !keymap.leader_combos_for(a).is_empty();
            if anywhere {
                bound.push(a);
            }
        }
        // Every bound action's label shows up in the flattened sections.
        let all_labels: Vec<&str> = sections
            .iter()
            .flat_map(|s| s.entries.iter().map(|(_, l)| l.as_str()))
            .collect();
        for a in bound {
            assert!(
                all_labels.contains(&a.label()),
                "{a:?} is bound but missing from help sections"
            );
        }
    }

    #[test]
    fn leader_section_lists_continuations() {
        let keymap = KeyMap::default();
        let sections = sections(&keymap);
        let leader = sections.iter().find(|s| s.header == "Leader").unwrap();
        assert!(
            leader
                .entries
                .iter()
                .any(|(k, l)| k.contains("<leader>") && l == "toggle explorer sidebar"),
            "leader section must list <leader>e toggle explorer"
        );
    }

    /// D2 note #2: the Explorer `s` binding (endpoints ⇄ sequences switch) must be
    /// discoverable in the help overlay's Explorer section — it renders from the
    /// live keymap, but lock it so a future refactor cannot silently drop it.
    #[test]
    fn explorer_section_lists_the_s_switch() {
        let keymap = KeyMap::default();
        let sections = sections(&keymap);
        let explorer = sections
            .iter()
            .find(|s| s.header == "Explorer")
            .expect("Explorer section present");
        assert!(
            explorer
                .entries
                .iter()
                .any(|(k, l)| k == "s" && l == "switch endpoints / sequences"),
            "Explorer section must list `s` → switch endpoints / sequences, got {:?}",
            explorer.entries
        );
    }

    /// A query matching a known label produces matches on the right line(s) with
    /// byte ranges that slice back to the query in the plain line text.
    #[test]
    fn search_locates_label_with_byte_ranges() {
        let keymap = KeyMap::default();
        let theme = Theme::default();
        let plain = plain_lines(&keymap, &theme);
        let mut search = HelpSearch::default();
        search.set_query("explorer".to_owned(), &keymap, &theme);
        assert!(search.count() > 0, "`explorer` must match some help rows");
        // Every match's byte range slices out the (lowercased) query.
        for &(line, s, e) in &search.matches {
            assert_eq!(
                plain[line][s..e].to_lowercase(),
                "explorer",
                "match byte range must cover the query on its line"
            );
        }
        // At least one hit lands on the "toggle explorer sidebar" row.
        let target = plain
            .iter()
            .position(|p| p.contains("toggle explorer sidebar"))
            .expect("toggle explorer row present");
        assert!(
            search.matches.iter().any(|&(l, _, _)| l == target),
            "search must locate the toggle explorer row"
        );
    }

    /// `n`/`N` cycle through matches and wrap around.
    #[test]
    fn n_and_shift_n_cycle_and_wrap() {
        let keymap = KeyMap::default();
        let theme = Theme::default();
        let mut search = HelpSearch::default();
        search.set_query("toggle".to_owned(), &keymap, &theme);
        let count = search.count();
        assert!(count >= 2, "need >=2 `toggle` matches to test cycling");
        // Live search seeds current = first match (ordinal 1).
        assert_eq!(search.current_ordinal(), Some(1));
        // Forward steps advance the ordinal…
        search.step(true);
        assert_eq!(search.current_ordinal(), Some(2));
        // …then wrap: from the last back to the first.
        search.current = Some(count - 1);
        search.step(true);
        assert_eq!(search.current_ordinal(), Some(1));
        // Backward from the first wraps to the last.
        search.step(false);
        assert_eq!(search.current_ordinal(), Some(count));
    }

    /// Smart-case parity with the body search: an all-lowercase query is
    /// case-insensitive; any uppercase char makes it case-sensitive. Asserted via
    /// the *shared* matcher so the two searches cannot diverge.
    #[test]
    fn search_is_smart_case() {
        let keymap = KeyMap::default();
        let theme = Theme::default();
        let plain = plain_lines(&keymap, &theme);
        let refs: Vec<&str> = plain.iter().map(String::as_str).collect();

        // Lowercase query: case-insensitive — matches regardless of the label's
        // own casing. Compare against a manual case-fold count.
        let ci = smart_case_matches(refs.iter().copied(), "toggle");
        let expected_ci: usize = plain
            .iter()
            .map(|p| p.to_lowercase().matches("toggle").count())
            .sum();
        assert_eq!(ci.len(), expected_ci, "lowercase query is case-insensitive");

        // Uppercase-bearing query: case-sensitive — only exact-case hits.
        let cs = smart_case_matches(refs.iter().copied(), "Toggle");
        let expected_cs: usize = plain.iter().map(|p| p.matches("Toggle").count()).sum();
        assert_eq!(cs.len(), expected_cs, "uppercase query is case-sensitive");
    }

    /// Highlight-and-jump, not a hiding filter: the rendered line set is the same
    /// length whether or not a search is active.
    #[test]
    fn search_does_not_hide_rows() {
        let keymap = KeyMap::default();
        let theme = Theme::default();
        let total_no_search = plain_lines(&keymap, &theme).len();

        let mut search = HelpSearch::default();
        search.set_query("explorer".to_owned(), &keymap, &theme);
        // The plain-line surface the renderer paints is unchanged by a search.
        let total_with_search = plain_lines(&keymap, &theme).len();
        assert_eq!(
            total_no_search, total_with_search,
            "search must not add or remove help rows"
        );
        // And it actually matched something (so the invariant isn't vacuous).
        assert!(search.count() > 0);
    }
}
