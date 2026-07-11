//! In-app sequence editor (`Mode::SequenceEditor`, M7.4 §4): a modal to edit a
//! sequence's steps, per-step extraction rules, and `on_error` policy, saved
//! through the format-preserving `save_sequence` seam.
//!
//! Reuses the M7.3 env-editor patterns: an ordered working copy, derived dirty
//! state, a discard-on-close guard, and a shared [`LineEditor`] for field edits.
//! Adding a step picks from the workspace's endpoints via a small self-contained
//! substring picker (no cross-modal dependency on the app's fuzzy overlay).

use std::collections::BTreeMap;
use std::path::PathBuf;

use churl_core::model::{OnError, Sequence, SequenceStep};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::line_editor::LineEditor;
use super::prompt;
use crate::tui::theme::Theme;

/// A step in the editor's working copy: an endpoint path plus ordered extraction
/// rules (name → expression). Kept as a `Vec` (not a `BTreeMap`) for insertion
/// order + in-place rename UX; the `BTreeMap` is rebuilt only on save.
///
/// `persist` is a parallel `bool` per rule (index-aligned with `rules`): `true`
/// means the rule's captured value flows into the in-memory Session scope
/// (surviving the run for standalone requests); `false` (the default) is today's
/// Run-only ephemeral behaviour. It is seeded from the step's `persist` name list
/// on load and rebuilt back into it on save (note #6).
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepEdit {
    endpoint: String,
    rules: Vec<(String, String)>,
    persist: Vec<bool>,
}

/// Which column has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The step list.
    Steps,
    /// The selected step's extraction rules.
    Rules,
}

/// What field a [`LineEditor`] is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditTarget {
    RuleName,
    RuleExpr,
}

/// An in-progress field edit.
#[derive(Debug)]
struct FieldEdit {
    target: EditTarget,
    editor: LineEditor,
    /// The rule index being edited.
    rule: usize,
    /// The pre-edit value, restored on cancel.
    original: String,
    /// True when this edit is for a freshly added rule (dropped whole on cancel).
    fresh: bool,
}

/// The self-contained add-step endpoint picker (substring filter, no fuzzy dep).
#[derive(Debug)]
struct AddStepPicker {
    query: LineEditor,
    /// Indices into [`SequenceEditorState::endpoints`] matching the query.
    filtered: Vec<usize>,
    selected: usize,
}

/// What a key press asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorOutcome {
    /// Handled internally.
    Consumed,
    /// Save the sequence (stay open).
    Save,
    /// Save then close.
    SaveAndClose,
    /// Close (discard already resolved).
    Close,
}

/// The sequence editor's state.
#[derive(Debug)]
pub struct SequenceEditorState {
    /// The sequence's `seq` ordering key (preserved on save).
    seq: u32,
    /// Sequence name (shown in the title; edited via rename at the app level).
    name: String,
    /// The sequence file on disk (already created).
    path: PathBuf,
    on_error: OnError,
    steps: Vec<StepEdit>,
    selected_step: usize,
    focus: Focus,
    selected_rule: usize,
    edit: Option<FieldEdit>,
    picker: Option<AddStepPicker>,
    /// Available endpoint paths (workspace-relative) for the add-step picker.
    endpoints: Vec<String>,
    /// True while the discard-changes confirm is showing.
    pending_close: bool,
    /// Snapshot for dirty derivation.
    snapshot: (OnError, Vec<StepEdit>),
}

impl SequenceEditorState {
    /// Builds the editor from a loaded sequence and the workspace's endpoint
    /// paths (for the add-step picker).
    pub fn new(name: String, path: PathBuf, sequence: &Sequence, endpoints: Vec<String>) -> Self {
        let steps: Vec<StepEdit> = churl_core::sequence::ordered_steps(sequence)
            .into_iter()
            .map(|step| {
                let rules: Vec<(String, String)> = step
                    .extract
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                // Seed each rule's target from the step's `persist` name list: a
                // rule whose name is listed is a Session target, else Run-only.
                let persist: Vec<bool> = rules
                    .iter()
                    .map(|(name, _)| step.persist.iter().any(|p| p == name))
                    .collect();
                StepEdit {
                    endpoint: step.endpoint.clone(),
                    rules,
                    persist,
                }
            })
            .collect();
        let snapshot = (sequence.on_error, steps.clone());
        Self {
            seq: sequence.seq,
            name,
            path,
            on_error: sequence.on_error,
            steps,
            selected_step: 0,
            focus: Focus::Steps,
            selected_rule: 0,
            edit: None,
            picker: None,
            endpoints,
            pending_close: false,
            snapshot,
        }
    }

    /// The file path this editor saves to.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The sequence's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the working copy differs from the snapshot.
    pub fn is_dirty(&self) -> bool {
        (self.on_error, &self.steps) != (self.snapshot.0, &self.snapshot.1)
    }

    /// Validates and rebuilds the [`Sequence`] for saving, renumbering step `seq`
    /// by position. Refuses (naming the step + var) when a step has two extraction
    /// rules with the same trimmed non-empty name — collecting into a `BTreeMap`
    /// would silently collapse them last-wins and lose a visible row (mirrors the
    /// M7.3 duplicate-var-name gate). Empty-named rules are dropped from the saved
    /// output (they bind no variable); [`mark_saved`] purges them from the working
    /// copy so dirty reconciles.
    pub fn to_sequence_checked(&self) -> Result<Sequence, String> {
        for (si, step) in self.steps.iter().enumerate() {
            let mut seen = std::collections::HashSet::new();
            for (name, _) in &step.rules {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !seen.insert(trimmed.to_owned()) {
                    return Err(format!(
                        "duplicate rule name '{trimmed}' in step {}",
                        si + 1
                    ));
                }
            }
        }
        Ok(Sequence {
            seq: self.seq,
            name: self.name.clone(),
            on_error: self.on_error,
            steps: self
                .steps
                .iter()
                .enumerate()
                .map(|(i, step)| SequenceStep {
                    seq: i as u32,
                    endpoint: step.endpoint.clone(),
                    extract: step
                        .rules
                        .iter()
                        .filter(|(name, _)| !name.trim().is_empty())
                        .map(|(name, expr)| (name.clone(), expr.clone()))
                        .collect::<BTreeMap<_, _>>(),
                    // The Session-target rule names (trimmed, non-empty). Only the
                    // names are persisted — never the captured value, which stays
                    // in-memory only (note #6 security invariant).
                    persist: step
                        .rules
                        .iter()
                        .zip(step.persist.iter())
                        .filter(|((name, _), on)| **on && !name.trim().is_empty())
                        .map(|((name, _), _)| name.clone())
                        .collect(),
                })
                .collect(),
        })
    }

    /// Refreshes the snapshot after a successful save (clears dirty). Purges
    /// empty-named rules from the working copy first, so what stays on screen
    /// matches exactly what was written to disk (no dirty-after-save mismatch).
    pub fn mark_saved(&mut self) {
        for step in &mut self.steps {
            // Purge empty-named rules from both the rules list and its parallel
            // `persist` flags in lockstep so the two stay index-aligned.
            let mut i = 0;
            step.rules.retain(|(name, _)| {
                let keep = !name.trim().is_empty();
                if !keep && i < step.persist.len() {
                    step.persist.remove(i);
                } else {
                    i += 1;
                }
                keep
            });
        }
        self.selected_rule = self
            .selected_rule
            .min(self.current_rules_len().saturating_sub(1));
        self.snapshot = (self.on_error, self.steps.clone());
    }

    /// Routes a key, returning what (if any) app action to take.
    pub fn handle_key(&mut self, key: KeyEvent) -> EditorOutcome {
        // Sub-overlays intercept first.
        if self.pending_close {
            return self.handle_close_confirm(key);
        }
        if self.picker.is_some() {
            self.handle_picker_key(key);
            return EditorOutcome::Consumed;
        }
        if self.edit.is_some() {
            self.handle_edit_key(key);
            return EditorOutcome::Consumed;
        }
        match key.code {
            KeyCode::Char('w') => {
                return if self.is_dirty() {
                    EditorOutcome::Save
                } else {
                    EditorOutcome::Consumed
                };
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                return if self.is_dirty() {
                    self.pending_close = true;
                    EditorOutcome::Consumed
                } else {
                    EditorOutcome::Close
                };
            }
            _ => {}
        }
        match self.focus {
            Focus::Steps => self.handle_steps_key(key),
            Focus::Rules => self.handle_rules_key(key),
        }
        EditorOutcome::Consumed
    }

    fn handle_close_confirm(&mut self, key: KeyEvent) -> EditorOutcome {
        match key.code {
            KeyCode::Char('s') => EditorOutcome::SaveAndClose,
            KeyCode::Char('d') => EditorOutcome::Close,
            KeyCode::Esc | KeyCode::Char('n') => {
                self.pending_close = false;
                EditorOutcome::Consumed
            }
            _ => EditorOutcome::Consumed,
        }
    }

    fn handle_steps_key(&mut self, key: KeyEvent) {
        // Ctrl-j / Ctrl-k reorder the selected step (down / up), an alias for the
        // Shift-J/Shift-K + [ / ] bindings (owner drive-test #4 — Ctrl-j/k is the
        // reorder convention they reached for). Intercepted before the plain match
        // so it isn't swallowed by the bare `j`/`k` selection-nav arms. Note:
        // Ctrl-j (ASCII LF) is only distinct from Enter under the enhanced keyboard
        // protocol; on legacy terminals it arrives as Enter, so the portable
        // Shift-J/[ ] bindings remain the reliable path.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('j') => {
                    self.move_step(false);
                    return;
                }
                KeyCode::Char('k') => {
                    self.move_step(true);
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected_step + 1 < self.steps.len() {
                    self.selected_step += 1;
                    self.selected_rule = 0;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_step = self.selected_step.saturating_sub(1);
                self.selected_rule = 0;
            }
            KeyCode::Char('a') => self.open_add_step_picker(),
            KeyCode::Char('d') => self.delete_step(),
            // Reorder: K/J or [ / ].
            KeyCode::Char('K') | KeyCode::Char('[') => self.move_step(true),
            KeyCode::Char('J') | KeyCode::Char(']') => self.move_step(false),
            KeyCode::Char('o') => {
                self.on_error = match self.on_error {
                    OnError::Halt => OnError::Continue,
                    OnError::Continue => OnError::Halt,
                };
            }
            KeyCode::Char('l') | KeyCode::Enter | KeyCode::Tab | KeyCode::Right
                if !self.steps.is_empty() =>
            {
                self.focus = Focus::Rules;
                self.selected_rule = 0;
            }
            _ => {}
        }
    }

    fn handle_rules_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('h') | KeyCode::Esc | KeyCode::Left | KeyCode::Tab => {
                self.focus = Focus::Steps;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let n = self.current_rules_len();
                if n > 0 && self.selected_rule + 1 < n {
                    self.selected_rule += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_rule = self.selected_rule.saturating_sub(1);
            }
            KeyCode::Char('a') => self.add_rule(),
            KeyCode::Char('d') => self.delete_rule(),
            KeyCode::Char('r') => self.begin_edit(EditTarget::RuleName, false),
            // Toggle the selected rule's target Run-only ⇄ Session (note #6). `p`
            // for "persist"; free among the Rules-focus keys.
            KeyCode::Char('p') => self.toggle_persist(),
            KeyCode::Enter | KeyCode::Char('i') => self.begin_edit(EditTarget::RuleExpr, false),
            _ => {}
        }
    }

    /// Flips the selected rule's Session/Run-only target.
    fn toggle_persist(&mut self) {
        if let Some(step) = self.steps.get_mut(self.selected_step)
            && let Some(flag) = step.persist.get_mut(self.selected_rule)
        {
            *flag = !*flag;
        }
    }

    fn current_rules_len(&self) -> usize {
        self.steps
            .get(self.selected_step)
            .map(|s| s.rules.len())
            .unwrap_or(0)
    }

    fn delete_step(&mut self) {
        if self.steps.is_empty() {
            return;
        }
        self.steps.remove(self.selected_step);
        self.selected_step = self.selected_step.min(self.steps.len().saturating_sub(1));
        self.selected_rule = 0;
        if self.steps.is_empty() {
            self.focus = Focus::Steps;
        }
    }

    fn move_step(&mut self, up: bool) {
        let i = self.selected_step;
        if up && i > 0 {
            self.steps.swap(i, i - 1);
            self.selected_step -= 1;
        } else if !up && i + 1 < self.steps.len() {
            self.steps.swap(i, i + 1);
            self.selected_step += 1;
        }
    }

    fn add_rule(&mut self) {
        let Some(step) = self.steps.get_mut(self.selected_step) else {
            return;
        };
        step.rules.push((String::new(), String::new()));
        step.persist.push(false); // new rules default to Run-only
        self.selected_rule = step.rules.len() - 1;
        self.begin_edit(EditTarget::RuleName, true);
    }

    fn delete_rule(&mut self) {
        if let Some(step) = self.steps.get_mut(self.selected_step)
            && self.selected_rule < step.rules.len()
        {
            step.rules.remove(self.selected_rule);
            if self.selected_rule < step.persist.len() {
                step.persist.remove(self.selected_rule);
            }
            self.selected_rule = self.selected_rule.min(step.rules.len().saturating_sub(1));
        }
    }

    fn begin_edit(&mut self, target: EditTarget, fresh: bool) {
        let Some(step) = self.steps.get(self.selected_step) else {
            return;
        };
        let Some((name, expr)) = step.rules.get(self.selected_rule) else {
            return;
        };
        let original = match target {
            EditTarget::RuleName => name.clone(),
            EditTarget::RuleExpr => expr.clone(),
        };
        self.edit = Some(FieldEdit {
            target,
            editor: LineEditor::new(&original),
            rule: self.selected_rule,
            original,
            fresh,
        });
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        let Some(edit) = self.edit.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let value = edit.editor.text();
                let (target, rule) = (edit.target, edit.rule);
                self.edit = None;
                if let Some(step) = self.steps.get_mut(self.selected_step)
                    && let Some(slot) = step.rules.get_mut(rule)
                {
                    match target {
                        EditTarget::RuleName => slot.0 = value,
                        EditTarget::RuleExpr => slot.1 = value,
                    }
                }
                // After naming a fresh rule, chain into editing its expression.
                if target == EditTarget::RuleName {
                    self.begin_edit(EditTarget::RuleExpr, false);
                }
            }
            KeyCode::Esc => {
                let (original, rule, fresh, target) =
                    (edit.original.clone(), edit.rule, edit.fresh, edit.target);
                self.edit = None;
                if let Some(step) = self.steps.get_mut(self.selected_step) {
                    // A fresh, still-unnamed rule is dropped whole on cancel.
                    if fresh && target == EditTarget::RuleName {
                        if rule < step.rules.len() {
                            step.rules.remove(rule);
                            if rule < step.persist.len() {
                                step.persist.remove(rule);
                            }
                            self.selected_rule =
                                self.selected_rule.min(step.rules.len().saturating_sub(1));
                        }
                    } else if let Some(slot) = step.rules.get_mut(rule) {
                        match target {
                            EditTarget::RuleName => slot.0 = original,
                            EditTarget::RuleExpr => slot.1 = original,
                        }
                    }
                }
            }
            _ => {
                edit.editor.handle_key(key);
            }
        }
    }

    // ---- add-step picker ----

    fn open_add_step_picker(&mut self) {
        if self.endpoints.is_empty() {
            return;
        }
        let filtered = (0..self.endpoints.len()).collect();
        self.picker = Some(AddStepPicker {
            query: LineEditor::new(""),
            filtered,
            selected: 0,
        });
    }

    fn refilter_picker(&mut self) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let query = picker.query.text().to_lowercase();
        picker.filtered = self
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, path)| query.is_empty() || path.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        picker.selected = picker.selected.min(picker.filtered.len().saturating_sub(1));
    }

    fn handle_picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => {
                if picker.selected + 1 < picker.filtered.len() {
                    picker.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(&idx) = picker.filtered.get(picker.selected) {
                    let endpoint = self.endpoints[idx].clone();
                    self.steps.push(StepEdit {
                        endpoint,
                        rules: Vec::new(),
                        persist: Vec::new(),
                    });
                    self.selected_step = self.steps.len() - 1;
                }
                self.picker = None;
            }
            _ => {
                if picker.query.handle_key(key) {
                    self.refilter_picker();
                }
            }
        }
    }
}

/// Renders the sequence editor — the Edit face of the unified sequence surface.
/// The `Ctrl-R` face-flip hint lives in the footer (see [`render_footer`]),
/// keeping the title to name + dirty marker.
pub fn render(frame: &mut Frame, area: Rect, state: &SequenceEditorState, theme: &Theme) {
    let [modal] = Layout::horizontal([Constraint::Percentage(90)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(90)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let dirty = if state.is_dirty() { " ●" } else { "" };
    let title = format!(" Sequence · {}{dirty} ", state.name);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Mode + progress row · one-line purpose hint · body · footer (key hints).
    let [header, hint, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Mode: EDIT", theme.title),
            Span::styled(format!("   {} steps", state.steps.len()), theme.statusline),
            Span::styled(
                format!(
                    "   on_error: {}",
                    match state.on_error {
                        OnError::Halt => "halt",
                        OnError::Continue => "continue",
                    }
                ),
                theme.statusline,
            ),
        ])),
        header,
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Compose an ordered chain of requests, extracting values from each response to feed the next.",
            theme.statusline,
        ))),
        hint,
    );

    let left_width = (inner.width.saturating_sub(2) / 2).clamp(1, 44);
    let [left, right] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Fill(1)]).areas(body);
    render_steps(frame, left, state, theme);
    render_rules(frame, right, state, theme);
    render_footer(frame, footer, state, theme);

    if let Some(picker) = &state.picker {
        render_picker(frame, modal, picker, &state.endpoints, theme);
    } else if state.pending_close {
        prompt::render_confirm(
            frame,
            modal,
            "Unsaved changes",
            "Save changes before closing?",
            "s save · d discard · esc stay",
            theme,
        );
    }
}

fn render_steps(frame: &mut Frame, area: Rect, state: &SequenceEditorState, theme: &Theme) {
    let focused = state.focus == Focus::Steps && state.edit.is_none();
    let block = bordered(theme, focused, " Steps ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    if state.steps.is_empty() {
        lines.push(Line::from(Span::styled(
            "no steps — press a to add",
            theme.statusline,
        )));
    }
    for (i, step) in state.steps.iter().enumerate() {
        let marker = if i == state.selected_step { "> " } else { "  " };
        let mut line = Line::from(format!(
            "{marker}{}. {}  ({} rules)",
            i + 1,
            step.endpoint,
            step.rules.len()
        ));
        if i == state.selected_step && focused {
            line = line.style(theme.selection);
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_rules(frame: &mut Frame, area: Rect, state: &SequenceEditorState, theme: &Theme) {
    let focused = state.focus == Focus::Rules || state.edit.is_some();
    let block = bordered(theme, focused, " Extraction rules ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(step) = state.steps.get(state.selected_step) else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no step selected",
                theme.statusline,
            ))),
            inner,
        );
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    if step.rules.is_empty() {
        lines.push(Line::from(Span::styled(
            "no rules — press a to add",
            theme.statusline,
        )));
    }
    for (i, (name, expr)) in step.rules.iter().enumerate() {
        let selected = i == state.selected_rule;
        // Show the live edit buffer for the row being edited.
        let (name_s, expr_s) = match &state.edit {
            Some(edit) if edit.rule == i => match edit.target {
                EditTarget::RuleName => (edit.editor.text(), expr.clone()),
                EditTarget::RuleExpr => (name.clone(), edit.editor.text()),
            },
            _ => (name.clone(), expr.clone()),
        };
        let marker = if selected { "> " } else { "  " };
        let mut spans = vec![
            Span::raw(marker),
            Span::styled(
                if name_s.is_empty() {
                    "<name>".to_owned()
                } else {
                    name_s
                },
                theme.title,
            ),
            Span::raw(" = "),
            Span::raw(if expr_s.is_empty() {
                "<expr>".to_owned()
            } else {
                expr_s
            }),
        ];
        // A Session-target rule (note #6) shows a subordinate ` →session` marker;
        // Run-only rules show nothing (the ephemeral default). Hidden while this
        // row is being edited so the live edit buffer stays clean. On the
        // highlighted row the marker adapts (selection bg + DIM) so it stays
        // legible where a plain-dim fg would wash out (drive-test note #1).
        let persisted = step.persist.get(i).copied().unwrap_or(false);
        let being_edited = matches!(&state.edit, Some(edit) if edit.rule == i);
        let highlighted = selected && focused;
        if persisted && !being_edited {
            spans.push(Span::styled(
                " →session",
                session_marker_style(theme, highlighted),
            ));
        }
        let mut line = Line::from(spans);
        if highlighted {
            line = line.style(theme.selection);
        }
        lines.push(line);
    }
    // Guidance on the extraction grammar (owner drive-test #5 — adding a rule gave
    // no hint how to extract a value). Shown while the Rules pane is focused/edited
    // so the syntax is in view as you type. Mirrors the M7.4 grammar subset:
    // `status`, `header:<name>`, and `$.json.path` (with `[i]` array indexing).
    if focused {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "extract from response:",
            theme.statusline,
        )));
        lines.push(Line::from(Span::styled(
            "  status · header:<name> · $.json.path · $.list[0].id",
            theme.statusline,
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(frame: &mut Frame, area: Rect, state: &SequenceEditorState, theme: &Theme) {
    let hint = if state.edit.is_some() {
        "enter commit · esc cancel"
    } else if state.picker.is_some() {
        "type to filter · ↑/↓ select · enter add · esc cancel"
    } else {
        match state.focus {
            Focus::Steps => {
                "j/k step · a add · d del · K/J mv · o on-err · w save · ^R run · q close"
            }
            Focus::Rules => {
                "j/k rule · a add · enter expr · r name · p session · d del · w save · ^R run · q close"
            }
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme.statusline))),
        area,
    );
}

fn render_picker(
    frame: &mut Frame,
    area: Rect,
    picker: &AddStepPicker,
    endpoints: &[String],
    theme: &Theme,
) {
    let [pop] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [pop] = Layout::vertical([Constraint::Percentage(60)])
        .flex(Flex::Center)
        .areas(pop);
    frame.render_widget(Clear, pop);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(format!(" Add step · {} ", picker.query.text()))
        .title_style(theme.title);
    let inner = block.inner(pop);
    frame.render_widget(block, pop);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let lines: Vec<Line> = picker
        .filtered
        .iter()
        .take(inner.height as usize)
        .enumerate()
        .map(|(row, &idx)| {
            let marker = if row == picker.selected { "> " } else { "  " };
            let mut line = Line::from(format!("{marker}{}", endpoints[idx]));
            if row == picker.selected {
                line = line.style(theme.selection);
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The style for the ` →session` marker, adapting to whether its row is the
/// highlighted (selected + focused) one. On a plain row it is the subordinate
/// `session_marker` slot. On the highlighted row the marker sits on the
/// `selection` fill; the `session_marker` slot's own hue tracks that same fill
/// (cyan-on-cyan / blue-on-blue) and would wash out to fg==bg, so instead we
/// carry the selection's *own* foreground — guaranteed to contrast its own
/// background — and add `DIM` so the marker stays subordinate but legible
/// (drive-test note #1; the earlier "keep marker fg" derivation was invisible).
fn session_marker_style(theme: &Theme, highlighted: bool) -> Style {
    if highlighted {
        theme.selection.add_modifier(Modifier::DIM)
    } else {
        theme.session_marker
    }
}

fn bordered(theme: &Theme, focused: bool, title: &'static str) -> Block<'static> {
    Block::bordered()
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(if focused {
            theme.border_focused
        } else {
            theme.border_unfocused
        })
        .title(title)
        .title_style(theme.title)
}

#[cfg(test)]
mod tests;
