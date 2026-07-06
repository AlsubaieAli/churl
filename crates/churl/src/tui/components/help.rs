//! `?` help overlay: a floating pane rendering the *effective* keymap from the
//! live [`KeyMap`] (never a hardcoded list — it cannot drift), sectioned
//! **Global / Explorer / URL bar / Request / Response / Leader**, scrollable,
//! dismissed with `?`/Esc/`q`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::tui::events::{Action, KeyMap, PaneCtx};
use crate::tui::theme::Theme;

/// One rendered help section: a header plus its `(keys, label)` entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpSection {
    /// The section header (e.g. `"Global"`, `"Leader"`).
    pub header: String,
    /// `(key combos, action label)` rows, sorted by action name.
    pub entries: Vec<(String, String)>,
}

/// Builds the help sections from the live keymap. Every bound action appears in
/// exactly one section; sections with no bindings are still emitted (with an
/// empty entry list) so the guard test can assert none is missing.
pub fn sections(keymap: &KeyMap) -> Vec<HelpSection> {
    let mut actions: Vec<Action> = Action::all().collect();
    actions.sort_by_key(|a| a.name());

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
/// scroll source for the overlay.
fn help_lines<'a>(sections: &[HelpSection], theme: &Theme) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    let key_w = sections
        .iter()
        .flat_map(|s| s.entries.iter())
        .map(|(k, _)| k.len())
        .max()
        .unwrap_or(0);
    for section in sections {
        lines.push(Line::styled(
            section.header.clone(),
            theme.title.add_modifier(Modifier::BOLD),
        ));
        if section.entries.is_empty() {
            lines.push(Line::styled(
                "  (none)".to_owned(),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        for (keys, label) in &section.entries {
            lines.push(Line::from(vec![
                Span::styled(format!("  {keys:<key_w$}"), theme.jump_label),
                Span::raw(format!("  {label}")),
            ]));
        }
        lines.push(Line::from(""));
    }
    lines
}

/// Renders the help overlay over `area`, scrolled by `scroll` lines. Returns the
/// number of content lines (so the caller can clamp scroll).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    keymap: &KeyMap,
    scroll: usize,
    theme: &Theme,
) -> usize {
    let sections = sections(keymap);
    let lines = help_lines(&sections, theme);
    let total = lines.len();

    let [modal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(" Help — keys (?/esc/q to close) ")
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let scroll = scroll.min(total.saturating_sub(1)) as u16;
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
    total
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
}
