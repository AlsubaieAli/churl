//! The multipart file-part picker overlay (M8.6): a small directory browser
//! used to choose a file for a `Body::Multipart` file [`Part`](churl_core::model::Part).
//! Owns its own [`FilePickerState`] (per the "illegal states unrepresentable /
//! Mode owns its state" convention — see `docs/DECISIONS.md`), reached via
//! [`crate::tui::app::Mode::FilePicker`].
//!
//! Browsing is a plain flat single-directory listing (dirs first, then files,
//! both alphabetical): `Enter`/`l` on a directory descends into it, `Enter`/`l`
//! on a file accepts it, `-`/`h`/`Backspace` goes to the parent directory,
//! `Esc` cancels. Navigation is free to go anywhere on disk (including above
//! the workspace root) — [`FilePickerState::stored_path`] is what enforces the
//! M8.6 storage rule (relative-to-root when the chosen file is under root,
//! absolute otherwise), not the browser itself.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::tui::theme::Theme;

/// One entry in the current directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The bare file/directory name (no path components).
    pub name: String,
    /// Whether this entry is a directory (descends on accept) vs a file
    /// (picks on accept).
    pub is_dir: bool,
}

/// The file-picker overlay's own state — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePickerState {
    /// The workspace root: the boundary [`Self::stored_path`] measures
    /// relative-ness against. Browsing itself is not confined to it.
    pub root: PathBuf,
    /// The directory currently listed.
    pub current_dir: PathBuf,
    /// `current_dir`'s entries, dirs first then files, each group alphabetical.
    pub entries: Vec<FileEntry>,
    /// The selected row in `entries`.
    pub selected: usize,
    /// The index (in `Body::Multipart`'s part list) this picker is choosing a
    /// path FOR — accepting writes the chosen path back into this part.
    pub part_index: usize,
}

impl FilePickerState {
    /// Opens a picker rooted at `root`, initially listing `start_dir` (falls
    /// back to `root` when `start_dir` cannot be listed — e.g. a part's
    /// existing path doesn't resolve to a real directory).
    pub fn open(root: PathBuf, start_dir: PathBuf, part_index: usize) -> Self {
        let current_dir = if start_dir.is_dir() {
            start_dir
        } else {
            root.clone()
        };
        let entries = list_dir(&current_dir);
        Self {
            root,
            current_dir,
            entries,
            selected: 0,
            part_index,
        }
    }

    /// Moves the selection up (clamped).
    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Moves the selection down (clamped to the last entry).
    pub fn move_down(&mut self) {
        let max = self.entries.len().saturating_sub(1);
        self.selected = (self.selected + 1).min(max);
    }

    /// The currently selected entry, if any (an empty directory has none).
    pub fn selected_entry(&self) -> Option<&FileEntry> {
        self.entries.get(self.selected)
    }

    /// Descends into the selected entry if it is a directory; a no-op
    /// otherwise (including on an empty listing). Resets the selection.
    pub fn descend(&mut self) {
        if let Some(entry) = self.selected_entry()
            && entry.is_dir
        {
            self.current_dir = self.current_dir.join(&entry.name);
            self.entries = list_dir(&self.current_dir);
            self.selected = 0;
        }
    }

    /// Goes to the parent directory, if any. A no-op at a filesystem root
    /// (`current_dir.parent()` is `None`) — never panics, never wraps.
    pub fn go_up(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.entries = list_dir(&self.current_dir);
            self.selected = 0;
        }
    }

    /// The full path of the selected entry, if any.
    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_entry()
            .map(|entry| self.current_dir.join(&entry.name))
    }

    /// The M8.6 storage rule: `chosen` relative-to-`root` (POSIX-separated,
    /// so a saved endpoint's `file =` string is portable across OSes) when it
    /// resolves under `root`, else the absolute path string as-is. Lexical —
    /// does not touch the filesystem (both `root` and `chosen` are already
    /// resolved by the browser's own `join`s).
    pub fn stored_path(&self, chosen: &Path) -> String {
        match chosen.strip_prefix(&self.root) {
            Ok(relative) => {
                let posix: Vec<&str> = relative
                    .components()
                    .filter_map(|c| c.as_os_str().to_str())
                    .collect();
                posix.join("/")
            }
            Err(_) => chosen.to_string_lossy().into_owned(),
        }
    }
}

/// Lists `dir`'s entries: directories first, then files, each group
/// alphabetical (case-insensitive), dotfiles included (a workspace's own
/// `.git`/hidden config is legitimately sometimes the target). An unreadable
/// directory (permissions, race) returns an empty list — never panics, never
/// propagates an error the picker has no way to display mid-browse.
fn list_dir(dir: &Path) -> Vec<FileEntry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<FileEntry> = read
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().ok()?.is_dir();
            Some(FileEntry { name, is_dir })
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

/// Renders the file picker as a centered overlay: the current directory in
/// the title, the entry list (directories suffixed `/`), and a footer hint.
pub fn render(frame: &mut Frame, area: Rect, state: &FilePickerState, theme: &Theme) {
    let [modal] = Layout::horizontal([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(area);
    let [modal] = Layout::vertical([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(modal);

    frame.render_widget(Clear, modal);
    let title = format!(" Choose file — {} ", state.current_dir.display());
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(theme.border_focused)
        .title(title)
        .title_style(theme.title);
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let [list_area, footer_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);

    let mut lines: Vec<Line> = if state.entries.is_empty() {
        vec![Line::styled("  (empty directory)", theme.border_unfocused)]
    } else {
        state
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let marker = if entry.is_dir { "/" } else { "" };
                let prefix = if i == state.selected { "> " } else { "  " };
                let line = Line::from(format!("{prefix}{}{marker}", entry.name));
                if i == state.selected {
                    line.style(theme.selection)
                } else {
                    line
                }
            })
            .collect()
    };
    // Keep the selection in view within the list area's height.
    let height = list_area.height as usize;
    if height > 0 && state.selected >= height {
        let offset = state.selected + 1 - height;
        if offset < lines.len() {
            lines.drain(..offset);
        }
    }
    frame.render_widget(Paragraph::new(lines), list_area);
    frame.render_widget(
        Line::from(" Enter: open/pick · -: up · Esc: cancel")
            .style(theme.border_unfocused.add_modifier(Modifier::ITALIC)),
        footer_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scaffold() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/report.pdf"), b"x").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"x").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();
        dir
    }

    #[test]
    fn lists_dirs_before_files_alphabetically() {
        let dir = scaffold();
        let state = FilePickerState::open(dir.path().to_path_buf(), dir.path().to_path_buf(), 0);
        let names: Vec<&str> = state.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["assets", ".hidden", "readme.txt"]);
        assert!(state.entries[0].is_dir);
        assert!(!state.entries[1].is_dir);
    }

    #[test]
    fn descend_and_go_up_round_trip() {
        let dir = scaffold();
        let mut state =
            FilePickerState::open(dir.path().to_path_buf(), dir.path().to_path_buf(), 0);
        // Select "assets" (row 0) and descend.
        state.descend();
        assert_eq!(state.current_dir, dir.path().join("assets"));
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].name, "report.pdf");
        assert!(!state.entries[0].is_dir);
        // Descending on a FILE row is a no-op.
        state.descend();
        assert_eq!(state.current_dir, dir.path().join("assets"));
        // Go back up.
        state.go_up();
        assert_eq!(state.current_dir, dir.path());
    }

    #[test]
    fn go_up_at_filesystem_root_is_a_noop() {
        let mut state = FilePickerState::open(PathBuf::from("/"), PathBuf::from("/"), 0);
        state.go_up();
        assert_eq!(state.current_dir, PathBuf::from("/"));
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let dir = scaffold();
        let mut state =
            FilePickerState::open(dir.path().to_path_buf(), dir.path().to_path_buf(), 0);
        state.move_up();
        assert_eq!(state.selected, 0, "cannot go below 0");
        for _ in 0..10 {
            state.move_down();
        }
        assert_eq!(state.selected, state.entries.len() - 1);
    }

    #[test]
    fn stored_path_is_relative_under_root_else_absolute() {
        let dir = scaffold();
        let root = dir.path().to_path_buf();
        let state = FilePickerState::open(root.clone(), root.clone(), 0);
        let under_root = root.join("assets").join("report.pdf");
        assert_eq!(state.stored_path(&under_root), "assets/report.pdf");

        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("elsewhere.bin");
        assert_eq!(
            state.stored_path(&outside_file),
            outside_file.to_string_lossy()
        );
    }

    #[test]
    fn open_falls_back_to_root_when_start_dir_is_not_a_directory() {
        let dir = scaffold();
        let root = dir.path().to_path_buf();
        let missing = root.join("does/not/exist");
        let state = FilePickerState::open(root.clone(), missing, 0);
        assert_eq!(state.current_dir, root);
    }

    #[test]
    fn unreadable_or_missing_directory_lists_empty_not_panicking() {
        let entries = list_dir(Path::new("/definitely/does/not/exist/at/all"));
        assert!(entries.is_empty());
    }
}
