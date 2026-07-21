//! M8.6 multipart body-tab handlers: the Body-type picker (text/json/form/
//! multipart) and the file-picker overlay for a `Multipart` file part's path.
//! Grandchild module of `app` (see `handlers/mod.rs`'s doc) — full access to
//! `App`'s private fields/methods without visibility widening. The row-list
//! actions themselves (add/delete/toggle/edit a part) extend the SAME
//! Params/Headers row handlers in `editing.rs` (one `RequestTab` match per
//! action, not a parallel set here) — this file holds only what is genuinely
//! new: the type picker and the file picker.

use super::super::*;

impl App {
    /// Whether the active buffer's live body is `Multipart` — the gate that
    /// routes the Body tab's key handling to the row-list path (like Params/
    /// Headers/Auth) instead of edtui. `false` when nothing is loaded.
    pub(in crate::tui::app) fn body_is_multipart(&self) -> bool {
        self.live_request()
            .is_some_and(|r| matches!(r.body, Some(Body::Multipart(_))))
    }

    /// Whether Body-tab row `row` (1-based over parts, row 0 = the type row)
    /// addresses a `File`-kind part. `false` for the type row, an
    /// out-of-range row, or a non-multipart body.
    pub(in crate::tui::app) fn body_part_is_file(&self, row: usize) -> bool {
        row >= 1
            && self
                .live_request()
                .and_then(|r| match &r.body {
                    Some(Body::Multipart(parts)) => parts.get(row - 1),
                    _ => None,
                })
                .is_some_and(|part| matches!(part.value, PartValue::File { .. }))
    }

    /// Opens the Body-type picker (`<leader>b`): text / json / form /
    /// multipart. A no-op with nothing loaded.
    pub(in crate::tui::app) fn open_body_type_picker(&mut self) {
        if self.selected().is_none() {
            return;
        }
        let labels: Vec<String> = BodyTypeUi::ALL
            .iter()
            .map(|kind| kind.label().to_owned())
            .collect();
        self.picker = Some(Picker::BodyType {
            state: picker::PickerState::new(" Body type ", labels),
        });
        self.mode = Mode::Palette;
    }

    /// Applies a Body-type picker choice. Switching among Text/Json/Form
    /// preserves the current content (only `kind` changes, like flipping a
    /// tag); switching to/from Multipart necessarily discards it (different
    /// shape — mirrors `set_auth_kind`'s "swap in default-empty fields").
    /// Re-seeds the edtui buffer from the (possibly new) `Simple` content so
    /// the Body tab reflects the switch immediately. Syncs any in-progress
    /// edtui edit into the request FIRST, so typed-but-unsynced text is never
    /// silently dropped by the switch.
    pub(in crate::tui::app) fn set_body_type(&mut self, index: usize) {
        let Some(kind) = BodyTypeUi::ALL.get(index).copied() else {
            return;
        };
        self.sync_body_into_selected();
        let Some(b) = self.active_endpoint_buffer_mut() else {
            return;
        };
        let request = &mut b.endpoint.endpoint.request;
        if BodyTypeUi::from_body(request.body.as_ref()) == kind {
            return; // already this kind — nothing to convert
        }
        let carried_content = match &request.body {
            Some(Body::Simple { content, .. }) => content.clone(),
            _ => String::new(),
        };
        request.body = Some(match kind {
            BodyTypeUi::Text => Body::Simple {
                kind: BodyKind::Text,
                content: carried_content,
            },
            BodyTypeUi::Json => Body::Simple {
                kind: BodyKind::Json,
                content: carried_content,
            },
            BodyTypeUi::Form => Body::Simple {
                kind: BodyKind::Form,
                content: carried_content,
            },
            BodyTypeUi::Multipart => Body::Multipart(Vec::new()),
        });
        b.tabs.parts_sel = 0;
        let editor_text = match &request.body {
            Some(Body::Simple { content, .. }) => content.clone(),
            _ => String::new(),
        };
        b.editor = new_editor_state(&editor_text);
        b.editor_vim.reset();
    }

    /// Opens the file picker for the multipart part at `part_index` (`<Enter>`
    /// advancing past a File-part's name field — see `field_edit_advance`).
    /// Starts browsing at the part's existing path's parent directory when it
    /// resolves under the workspace root; else at the root itself. A no-op
    /// with no workspace open (a multipart file part is meaningless without
    /// one to resolve relative paths against).
    pub(in crate::tui::app) fn open_file_picker(&mut self, part_index: usize) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root().to_path_buf()) else {
            self.notify("no workspace open — cannot browse for a file");
            return;
        };
        let start_dir = self
            .active_endpoint_buffer()
            .and_then(|b| match &b.live_request().body {
                Some(Body::Multipart(parts)) => parts.get(part_index),
                _ => None,
            })
            .and_then(|part| match &part.value {
                PartValue::File { path, .. } if !path.is_empty() => {
                    let candidate = if Path::new(path).is_absolute() {
                        PathBuf::from(path)
                    } else {
                        root.join(path)
                    };
                    candidate.parent().map(Path::to_path_buf)
                }
                _ => None,
            })
            .unwrap_or_else(|| root.clone());
        self.mode = Mode::FilePicker(file_picker::FilePickerState::open(
            root, start_dir, part_index,
        ));
    }

    /// Handles one key while the file picker is open: `j`/`k`/arrows move the
    /// selection, `Enter`/`l`/right descends a directory or accepts a file,
    /// `-`/`h`/left/Backspace goes to the parent directory, `Esc` cancels
    /// (the target part is left untouched).
    pub(in crate::tui::app) fn handle_file_picker_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Mode::FilePicker(state) = &mut self.mode {
                    state.move_up();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Mode::FilePicker(state) = &mut self.mode {
                    state.move_down();
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') | KeyCode::Backspace => {
                if let Mode::FilePicker(state) = &mut self.mode {
                    state.go_up();
                }
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                self.accept_file_picker_entry();
            }
            _ => {}
        }
    }

    /// Accepts the currently-selected file-picker entry: descends into a
    /// directory, or — for a file — computes the M8.6 stored-path form and
    /// writes it into the target part, closing the picker back to Normal.
    fn accept_file_picker_entry(&mut self) {
        let Mode::FilePicker(state) = &mut self.mode else {
            return;
        };
        let Some(entry) = state.selected_entry().cloned() else {
            return;
        };
        if entry.is_dir {
            state.descend();
            return;
        }
        let Some(chosen) = state.selected_path() else {
            return;
        };
        let stored = state.stored_path(&chosen);
        let part_index = state.part_index;
        self.mode = Mode::Normal;
        if let Some(b) = self.active_endpoint_buffer_mut()
            && let Some(Body::Multipart(parts)) = b.endpoint.endpoint.request.body.as_mut()
            && let Some(part) = parts.get_mut(part_index)
        {
            part.value = PartValue::File {
                path: stored,
                filename: None,
                mime: None,
            };
        }
    }
}
