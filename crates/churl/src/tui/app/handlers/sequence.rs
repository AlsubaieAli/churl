//! Sequence run + editor handlers — run orchestration, the unified sequence
//! surface key routing, pickers, and the editor save/create path — extracted
//! from `app.rs` (M7.11, PR 4). Grandchild module of `app`; `impl App` here
//! keeps full access to `App`'s private fields and methods without any
//! visibility widening.

use super::super::*;

impl App {
    /// Runs the sequence under the sub-pane cursor (`<leader>s r` / palette / `r`
    /// on the sequences sub-pane). Notifies when no sequence is selected.
    pub(in crate::tui::app) fn run_selected_sequence(&mut self) {
        let Some(selected) = self.explorer.selected_sequence() else {
            self.notify("select a sequence first");
            return;
        };
        self.open_sequence_runner(selected);
    }

    /// Opens a fuzzy picker over every sequence name. `run == false` (`<leader>s o`
    /// / palette) opens the chosen sequence in the Edit face; `run == true`
    /// (`<leader>s r`) loads + runs it instead (D1 — mirrors the
    /// `load_runner_after_pick` one-shot-intent pattern).
    pub(in crate::tui::app) fn open_sequence_picker(&mut self, run: bool) {
        if self.workspace.is_none() {
            self.notify("open a workspace first");
            return;
        }
        let sequences = self.explorer.all_sequences();
        if sequences.is_empty() {
            self.notify("no sequences in this workspace");
            return;
        }
        let mut items = Vec::with_capacity(sequences.len());
        let mut choices = Vec::with_capacity(sequences.len());
        for (name, file) in sequences {
            items.push(name);
            choices.push(file);
        }
        self.sequence_choices = choices;
        self.sequence_pick_runs = run;
        let title = if run {
            " Run sequence "
        } else {
            " Open sequence "
        };
        self.picker = Some(picker::PickerState::new(title, items));
        self.mode = Mode::SequencePicker;
    }

    /// Loads the sequence at `path` and opens the RUNNER over it (D1 `<leader>s r`
    /// chooser accept path). Mirrors `open_picked_sequence` but hands to the
    /// runner instead of the editor.
    pub(in crate::tui::app) fn run_sequence_at(&mut self, path: PathBuf) {
        match persistence::load_sequence(&path) {
            Ok(sequence) => {
                self.explorer.select_sequence_file(&path);
                self.open_sequence_runner(crate::tui::components::explorer::SelectedSequence {
                    name: sequence.name.clone(),
                    file: path,
                    sequence,
                });
            }
            Err(err) => self.crud_error(err),
        }
    }

    /// Loads the sequence at `path` and opens the unified surface (Edit face).
    /// Also moves the sub-pane cursor onto the picked sequence so a subsequent
    /// `<leader>s r` runs *this* sequence, not sequence #0.
    pub(in crate::tui::app) fn open_picked_sequence(&mut self, path: PathBuf) -> Result<()> {
        match persistence::load_sequence(&path) {
            Ok(sequence) => {
                self.explorer.select_sequence_file(&path);
                self.open_sequence_editor(sequence.name.clone(), path, &sequence);
            }
            Err(err) => self.crud_error(err),
        }
        Ok(())
    }

    /// Opens the runner over `selected` and starts the run.
    pub(in crate::tui::app) fn open_sequence_runner(
        &mut self,
        selected: crate::tui::components::explorer::SelectedSequence,
    ) {
        let steps = churl_core::sequence::ordered_steps(&selected.sequence)
            .into_iter()
            .cloned()
            .collect();
        self.sequence_runner = Some(SequenceRunnerState::new(
            selected.name,
            selected.file,
            selected.sequence.on_error,
            steps,
        ));
        self.mode = Mode::Sequence;
        self.sequence_view = SeqView::Run;
        self.start_sequence_run();
    }

    /// The ambient run scopes (cli / active profile / workspace) for a sequence
    /// run, mirroring the send-time resolver's non-collection layers. The
    /// per-step collection scope is loaded inside `prepare_step`.
    pub(in crate::tui::app) fn sequence_run_scopes(&self) -> churl_core::sequence::RunScopes {
        churl_core::sequence::RunScopes {
            session: self.session_vars(),
            cli: self.cli_vars.clone(),
            profile: self.profile_vars(),
            workspace: self.workspace_vars(),
        }
    }

    /// (Re)starts the run from the top: resets rows, bumps the run generation,
    /// aborts any in-flight step, and drives the first step.
    pub(in crate::tui::app) fn start_sequence_run(&mut self) {
        if let Some(handle) = self.sequence_abort.take() {
            handle.abort();
        }
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        runner.reset_for_rerun();
        runner.run_generation += 1;
        if runner.steps.is_empty() {
            runner.finished = true;
            return;
        }
        runner.current = Some(0);
        self.drive_sequence_step(0);
    }

    /// Prepares step `index` and spawns its execution (or records a prepare
    /// failure and advances). No HTTP client (snapshot tests) leaves the step
    /// pending — deterministic and runtime-free.
    pub(in crate::tui::app) fn drive_sequence_step(&mut self, index: usize) {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            return;
        };
        let scopes = self.sequence_run_scopes();
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        let run_generation = runner.run_generation;
        let Some(row) = runner.steps.get_mut(index) else {
            return;
        };
        let step = row.step.clone();
        match churl_core::sequence::prepare_step(&root, &step, &runner.extracted, &scopes) {
            Err(err) => {
                // A prepare failure is a transport-class failure for the step.
                row.response = ResponseState::Failed {
                    error: err.to_string(),
                    meta: sequence_step_meta(&step.endpoint),
                };
                self.finish_sequence_step(index, StepResult::HttpError(err.to_string()));
            }
            Ok(prepared) => {
                row.method = prepared.method;
                row.url = prepared.url.clone();
                row.status = StepStatus::Running;
                row.response = ResponseState::InFlight {
                    started: Instant::now(),
                };
                runner.selected = index;
                runner.geometry.cursor = 0;
                runner.geometry.scroll = 0;
                // No client (snapshot tests): leave the step running, no spawn.
                let Some(client) = self.client.clone() else {
                    return;
                };
                let tx = self.tx.clone();
                let options = self.execute_options;
                let request = prepared.request;
                let handle = tokio::spawn(async move {
                    let outcome = churl_core::http::execute(&client, &request, &options)
                        .await
                        .map_err(|err| err.to_string());
                    let _ = tx
                        .send(AppMsg::SequenceStep {
                            run_generation,
                            index,
                            outcome,
                        })
                        .await;
                });
                self.sequence_abort = Some(handle.abort_handle());
            }
        }
    }

    /// Lands a completed sequence step: drops stale results, classifies with the
    /// shared core seam, merges extracted values, and advances or finishes.
    pub(in crate::tui::app) fn on_sequence_step(
        &mut self,
        run_generation: u64,
        index: usize,
        outcome: Result<Response, String>,
    ) {
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        if run_generation != runner.run_generation {
            return; // stale — a cancel or re-run superseded this step
        }
        self.sequence_abort = None;
        let step = match runner.steps.get(index) {
            Some(row) => row.step.clone(),
            None => return,
        };
        let view_gen = runner.next_view_gen();

        // Classify with the shared core seam for the success branch; a transport
        // error maps to `HttpError` exactly as `classify_step` does (the one line
        // the TUI mirrors — guarded by `sequence_transition_matches_core`).
        let (result, extracted, timing, response) = match outcome {
            Ok(response) => {
                let (result, extracted) = churl_core::sequence::classify_response(&response, &step);
                let timing = Some(response.timing.total);
                let view = ResponseView::build(&response, view_gen);
                (result, extracted, timing, ResponseState::Done { view })
            }
            Err(error) => (
                StepResult::HttpError(error.clone()),
                BTreeMap::new(),
                None,
                ResponseState::Failed {
                    error,
                    meta: sequence_step_meta(&step.endpoint),
                },
            ),
        };

        // Merge extracted values into the run-only accumulator (empty on any
        // failure). Also collect the Session-target captures (note #6): a rule
        // whose name is in this step's `persist` and that actually extracted a
        // value. `extracted` is empty on a failed extraction, so a failure never
        // writes — leaving any prior Session value intact.
        let mut session_writes: Vec<(String, String)> = Vec::new();
        for (name, value) in &extracted {
            runner.extracted.insert(name.clone(), value.clone());
            if step.persist.iter().any(|p| p == name) {
                session_writes.push((name.clone(), value.clone()));
            }
        }
        if let Some(row) = runner.steps.get_mut(index) {
            row.timing = timing;
            row.extracted = extracted;
            row.response = response;
        }
        // Write the Session captures into the current workspace's in-memory store
        // (create/overwrite — a re-login refreshes the token). Done after the
        // `runner` borrow is released. Never touches disk.
        for (name, value) in session_writes {
            self.write_session_var(name, value);
        }
        self.finish_sequence_step(index, result);
    }

    /// Applies a step's classified `result`: sets its display status, then makes
    /// the halt/advance decision through the shared `should_halt` seam — the
    /// single place the TUI mirrors core's per-step transition, so the two cannot
    /// drift. Halt marks the remaining steps `Skipped` and finishes; otherwise the
    /// run advances.
    pub(in crate::tui::app) fn finish_sequence_step(
        &mut self,
        index: usize,
        result: churl_core::sequence::StepResult,
    ) {
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        if let Some(row) = runner.steps.get_mut(index) {
            row.status = StepStatus::from_result(&result);
        }
        runner.selected = index;
        if churl_core::sequence::should_halt(&result, runner.on_error) {
            for row in runner.steps.iter_mut().skip(index + 1) {
                row.status = StepStatus::Skipped;
            }
            runner.current = None;
            runner.finished = true;
        } else {
            self.advance_sequence(index);
        }
    }

    /// Advances to the step after `index`, or finishes the run.
    pub(in crate::tui::app) fn advance_sequence(&mut self, index: usize) {
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        let next = index + 1;
        if next >= runner.steps.len() {
            runner.current = None;
            runner.finished = true;
            return;
        }
        runner.current = Some(next);
        self.drive_sequence_step(next);
    }

    /// Cancels the in-flight run: aborts the task, bumps the generation so a
    /// landed result is dropped, and marks every non-terminal step skipped.
    pub(in crate::tui::app) fn cancel_sequence_run(&mut self) {
        if let Some(handle) = self.sequence_abort.take() {
            handle.abort();
        }
        let Some(runner) = self.sequence_runner.as_mut() else {
            return;
        };
        runner.run_generation += 1;
        for row in &mut runner.steps {
            if matches!(row.status, StepStatus::Pending | StepStatus::Running) {
                row.status = StepStatus::Skipped;
                if matches!(row.response, ResponseState::InFlight { .. }) {
                    row.response = ResponseState::Cancelled;
                }
            }
        }
        runner.current = None;
        runner.finished = true;
        runner.confirming_close = false;
        self.notify("run cancelled");
    }

    /// Routes a runner Response-region key through the SAME dispatch + `response_*`
    /// handlers the main pane uses (note #2 — one code path, so the runner viewer
    /// can never drift). Returns `true` when the key resolved to a response action
    /// and was consumed; `false` when the caller must delegate to the runner for
    /// its own keys.
    ///
    /// Only fires when a runner Response region is the active response surface (so
    /// Config/Results/Steps focus is untouched, and the runner's own guard/edit
    /// sub-states — via `active_response_surface`'s `response_input_captured` gate —
    /// keep their keys). The key is looked up through the shared `[keys.response]`
    /// overlay (so remapping a Response key updates the runners too), then matched
    /// against the response action set: the overlay's parity actions PLUS the
    /// viewer cursor-nav actions (routed through the same `response_scroll` /
    /// `response_half_page` the main pane uses). Runner-owned keys (Tab/BackTab,
    /// Ctrl-R, Ctrl-C, q/Esc, and everything else) are not in the set, so they fall
    /// through to the runner unchanged.
    pub(in crate::tui::app) fn try_route_runner_response_key(&mut self, key: KeyEvent) -> bool {
        if matches!(self.active_response_surface(), ResponseSurface::Main) {
            return false;
        }
        let Some(action) = self.keymap.lookup_ctx(key, PaneCtx::Response) else {
            return false;
        };
        match action {
            // Viewer cursor nav — the SAME movement path the main Response pane
            // uses, operating on the mode-aware geometry (note #2).
            Action::Up | Action::Down | Action::Top | Action::Bottom => {
                self.response_scroll(action)
            }
            Action::HalfPageDown => self.response_half_page(true),
            Action::HalfPageUp => self.response_half_page(false),
            // Response-overlay parity actions — identical handlers to the main pane.
            Action::ToggleHeadersView => self.response_toggle_headers(),
            Action::ToggleWrap => self.response_toggle_wrap(),
            Action::TogglePretty => self.response_toggle_pretty(),
            Action::ToggleSortKeys => self.response_toggle_sort_keys(),
            Action::ToggleLineNumbers => self.response_toggle_line_numbers(),
            Action::OpenBodySearch => self.open_body_search(),
            Action::SearchNext => self.response_search_step(true),
            Action::SearchPrev => self.response_search_step(false),
            Action::ToggleFold => self.response_toggle_fold(),
            Action::ToggleAllFolds => self.response_toggle_all_folds(),
            Action::CopyResponse => self.response_copy_view(),
            Action::CopyLine => self.response_copy_line(),
            Action::ScrollBodyLeft => self.response_scroll_h(false),
            Action::ScrollBodyRight => self.response_scroll_h(true),
            // Not a response action — the runner keeps it (nav-law, run/cancel/close).
            _ => return false,
        }
        true
    }

    /// Routes a key to the unified sequence surface. `Ctrl-R` flips the Edit⇄Run
    /// face (a switcher, not a nav-law key, free in both components); otherwise
    /// the key goes to the active face's handler. Per-face `Esc`/`q` semantics
    /// stay isolated — the Run-face confirm-on-close-while-running never fires in
    /// the Edit face because Edit keys never reach the runner.
    pub(in crate::tui::app) fn handle_sequence_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Char('r') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.toggle_sequence_view();
            return Ok(());
        }
        match self.sequence_view {
            SeqView::Edit => {
                let Some(editor) = self.sequence_editor.as_mut() else {
                    self.close_sequence_surface();
                    return Ok(());
                };
                match editor.handle_key(key) {
                    EditorOutcome::Consumed => {}
                    EditorOutcome::Save => {
                        self.save_sequence_editor()?;
                    }
                    EditorOutcome::SaveAndClose => {
                        if self.save_sequence_editor()? {
                            self.close_sequence_surface();
                        }
                    }
                    EditorOutcome::Close => self.close_sequence_surface(),
                }
            }
            SeqView::Run => {
                if self.sequence_runner.is_none() {
                    self.close_sequence_surface();
                    return Ok(());
                }
                // Response-region keys route through the shared response path FIRST
                // (note #2); anything not a response action delegates to the runner.
                if self.try_route_runner_response_key(key) {
                    return Ok(());
                }
                let runner = self.sequence_runner.as_mut().expect("checked above");
                match runner.handle_key(key) {
                    RunnerOutcome::Consumed => {}
                    RunnerOutcome::Rerun => self.start_sequence_run(),
                    RunnerOutcome::Cancel => self.cancel_sequence_run(),
                    RunnerOutcome::Close => self.close_sequence_surface(),
                }
            }
        }
        Ok(())
    }

    /// Flips the sequence surface's face. Run→Edit is always safe. Edit→Run is
    /// gated on the saved sequence being the single source of truth: a DIRTY
    /// editor blocks with a notify (no auto-save, no stale run); a clean editor
    /// (re)builds the runner from the saved steps before switching. The run
    /// itself is never auto-started here — the user presses `r` in the Run face.
    pub(in crate::tui::app) fn toggle_sequence_view(&mut self) {
        match self.sequence_view {
            SeqView::Run => {
                // Run→Edit is always safe, but the editor may not exist yet: a
                // `<leader>s r` run opens the runner face WITHOUT building an editor
                // (D2 note #1). Flipping to Edit without one left the surface in a
                // dead state — Edit face, no editor — so the pane was "exited" with
                // nothing focused until the next keypress fell through to
                // close_sequence_surface. Build the editor synchronously here (from
                // the runner's saved file, the single source of truth) so focus
                // transfers into the Edit face on the flip itself.
                if self.sequence_editor.is_none() {
                    let Some(path) = self.sequence_runner.as_ref().map(|r| r.path.clone()) else {
                        // No runner either — nothing to edit; leave the surface as-is
                        // rather than stranding it in a face with no component.
                        return;
                    };
                    match persistence::load_sequence(&path) {
                        Ok(sequence) => {
                            let endpoints = self.endpoint_rel_paths();
                            self.sequence_editor = Some(SequenceEditorState::new(
                                sequence.name.clone(),
                                path,
                                &sequence,
                                endpoints,
                            ));
                        }
                        Err(err) => {
                            // Couldn't load the file to edit — stay in Run face with
                            // the error surfaced, never a focus-less dead surface.
                            self.notify(format!("cannot edit sequence: {err}"));
                            return;
                        }
                    }
                }
                self.sequence_view = SeqView::Edit;
            }
            SeqView::Edit => {
                let Some(editor) = self.sequence_editor.as_ref() else {
                    return;
                };
                if editor.is_dirty() {
                    self.notify("save (w) before running");
                    return;
                }
                let sequence = match editor.to_sequence_checked() {
                    Ok(sequence) => sequence,
                    Err(msg) => {
                        self.notify(msg);
                        return;
                    }
                };
                let name = editor.name().to_owned();
                let file = editor.path().to_owned();
                let steps = churl_core::sequence::ordered_steps(&sequence)
                    .into_iter()
                    .cloned()
                    .collect();
                // A prior run's in-flight step survives a Run→Edit flip (the abort
                // handle is kept alive). Rebuilding the runner here without aborting
                // it would ORPHAN that async step — a real POST/DELETE running to
                // completion in the background with no UI. Abort + bump the old
                // generation first (mirrors start_sequence_run/close), so a landed
                // straggler is also dropped by the generation guard.
                if let Some(handle) = self.sequence_abort.take() {
                    handle.abort();
                }
                if let Some(runner) = self.sequence_runner.as_mut() {
                    runner.run_generation += 1;
                }
                self.sequence_runner = Some(SequenceRunnerState::new(
                    name,
                    file,
                    sequence.on_error,
                    steps,
                ));
                self.sequence_view = SeqView::Run;
            }
        }
    }

    /// Closes the unified sequence surface: aborts any in-flight step, drops both
    /// component states, and returns to Normal.
    pub(in crate::tui::app) fn close_sequence_surface(&mut self) {
        if let Some(handle) = self.sequence_abort.take() {
            handle.abort();
        }
        // Bump the generation so a straggler result is dropped after close.
        if let Some(runner) = self.sequence_runner.as_mut() {
            runner.run_generation += 1;
        }
        self.sequence_runner = None;
        self.sequence_editor = None;
        self.sequence_view = SeqView::Edit;
        self.mode = Mode::Normal;
    }

    /// `<leader>a` / palette: edit the sequence under the cursor, or prompt for a
    /// name to create a new one.
    pub(in crate::tui::app) fn edit_selected_sequence(&mut self) -> Result<()> {
        if self.workspace.is_none() {
            self.notify("open a workspace first");
            return Ok(());
        }
        match self.explorer.selected_sequence() {
            Some(selected) => {
                self.open_sequence_editor(selected.name, selected.file, &selected.sequence);
                Ok(())
            }
            None => {
                self.new_sequence_prompt();
                Ok(())
            }
        }
    }

    /// Opens the "new sequence" name prompt (the `n`-on-the-sequences-sub-pane
    /// entry point, parallel to endpoints `n`=new endpoint). Guards on an open
    /// workspace like every other create path.
    pub(in crate::tui::app) fn new_sequence_prompt(&mut self) {
        if self.workspace.is_none() {
            self.notify("open a workspace first");
            return;
        }
        self.open_prompt(PromptPurpose::NewSequence, "");
    }

    /// Workspace-relative endpoint paths for the editor's add-step picker.
    pub(in crate::tui::app) fn endpoint_rel_paths(&mut self) -> Vec<String> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            return Vec::new();
        };
        self.explorer
            .all_endpoint_files()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| {
                path.strip_prefix(&root)
                    .ok()
                    .map(|rel| rel.to_string_lossy().into_owned())
            })
            .collect()
    }

    /// Opens the editor over a loaded sequence.
    pub(in crate::tui::app) fn open_sequence_editor(
        &mut self,
        name: String,
        file: PathBuf,
        sequence: &churl_core::model::Sequence,
    ) {
        let endpoints = self.endpoint_rel_paths();
        self.sequence_editor = Some(SequenceEditorState::new(name, file, sequence, endpoints));
        // A fresh open starts with no run built; the run face is entered lazily.
        self.sequence_runner = None;
        self.mode = Mode::Sequence;
        self.sequence_view = SeqView::Edit;
    }

    /// Commits the "new sequence" name prompt: creates the file and opens the
    /// editor on it.
    pub(in crate::tui::app) fn commit_new_sequence(&mut self, name: String) -> Result<()> {
        let Some(root) = self.workspace.as_ref().map(|ws| ws.root().to_owned()) else {
            self.notify("no workspace open");
            return Ok(());
        };
        match persistence::create_sequence(&root, &name) {
            Ok(path) => {
                let sequence = persistence::load_sequence(&path)?;
                self.reload_explorer()?;
                self.open_sequence_editor(sequence.name.clone(), path, &sequence);
            }
            Err(err) => self.crud_error(err),
        }
        Ok(())
    }

    /// Saves the editor's sequence through the format-preserving seam and reloads
    /// the explorer so the change is visible. Returns whether the save took.
    pub(in crate::tui::app) fn save_sequence_editor(&mut self) -> Result<bool> {
        let Some(editor) = self.sequence_editor.as_ref() else {
            return Ok(false);
        };
        let path = editor.path().to_owned();
        // Validate first (duplicate rule names refuse the whole save — nothing is
        // written and the editor stays open + dirty, mirroring the M7.3 gate).
        let sequence = match editor.to_sequence_checked() {
            Ok(sequence) => sequence,
            Err(msg) => {
                self.notify(msg);
                return Ok(false);
            }
        };
        match persistence::save_sequence(&path, &sequence) {
            Ok(()) => {
                if let Some(editor) = self.sequence_editor.as_mut() {
                    editor.mark_saved();
                }
                self.reload_explorer()?;
                self.notify("sequence saved");
                Ok(true)
            }
            Err(err) => {
                self.crud_error(err);
                Ok(false)
            }
        }
    }

    /// `d` on the sequences sub-pane: y/n confirm before deleting the hovered
    /// sequence file (parallels [`begin_delete`]'s endpoint arm — same low-friction
    /// y/n gate, since a sequence file carries no secret values). Notifies when no
    /// sequence is selected.
    pub(in crate::tui::app) fn begin_delete_sequence(&mut self) {
        if self.explorer.selected_sequence().is_none() {
            self.notify("select a sequence first");
            return;
        }
        self.mode = Mode::Confirm(ConfirmPurpose::DeleteSequence);
    }
}
