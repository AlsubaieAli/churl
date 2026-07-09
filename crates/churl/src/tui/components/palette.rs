//! Command palette overlay (`:`): a [`PickerState`] over a **curated** list of
//! context-free commands; accepting an entry runs its [`Action`].
//!
//! The palette used to build over `Action::all()`, which surfaced pane-contextual
//! motions (Up/Down/Collapse/Expand/Tab*/Row*/…) that no-op or misbehave when
//! dispatched from an overlay (owner drive-test, 2026-07-06). It is now an
//! explicit allowlist of commands meaningful from anywhere; new Actions do **not**
//! auto-appear — inclusion is a deliberate per-command decision (DECISIONS.md).

use crate::tui::events::Action;

use super::picker::PickerState;

/// The curated palette commands, in display order: `(label, action)`. Only
/// context-free commands belong here; navigation/motion/tab/row/method/edit
/// actions stay keymap-only.
pub const COMMANDS: &[(&str, Action)] = &[
    ("send request", Action::Send),
    ("cancel request", Action::Cancel),
    ("save request", Action::Save),
    ("new endpoint", Action::NewEndpoint),
    ("new collection", Action::NewCollection),
    ("rename", Action::Rename),
    ("delete", Action::Delete),
    ("switch profile", Action::SwitchProfile),
    ("environments & vars", Action::OpenEnvEditor),
    ("run sequence", Action::RunSequence),
    ("add sequence", Action::EditSequence),
    ("open sequence", Action::OpenSequencePicker),
    ("load test endpoint (concurrent)", Action::OpenLoadRunner),
    ("load test (pick endpoint)", Action::OpenLoadRunnerPick),
    ("toggle response headers view", Action::ToggleHeadersView),
    ("toggle response wrap", Action::ToggleWrap),
    ("import collection (JSON)", Action::ImportCollection),
    (
        "export collection · Postman v2.1",
        Action::ExportCollectionPostman,
    ),
    (
        "export collection · churl JSON",
        Action::ExportCollectionNative,
    ),
    (
        "export workspace · Postman v2.1",
        Action::ExportWorkspacePostman,
    ),
    (
        "export workspace · churl JSON",
        Action::ExportWorkspaceNative,
    ),
    ("paste curl as new endpoint", Action::PasteCurl),
    ("copy request as curl", Action::CopyAsCurl),
    (
        "copy request as curl (resolved vars)",
        Action::CopyAsCurlResolved,
    ),
    ("toggle sequences sub-pane", Action::ToggleSequencesPane),
    ("focus explorer", Action::FocusExplorer),
    ("focus URL bar", Action::FocusUrlBar),
    ("focus request", Action::FocusRequest),
    ("focus response", Action::FocusResponse),
    ("quit", Action::Quit),
];

/// Palette overlay contents: the picker plus the action behind each item.
#[derive(Debug)]
pub struct PaletteItems {
    /// Picker over the curated command labels.
    pub picker: PickerState,
    /// Action per picker item, index-aligned with `picker.items`.
    pub actions: Vec<Action>,
}

/// Builds the command palette over the curated [`COMMANDS`] allowlist.
pub fn open() -> PaletteItems {
    let items = COMMANDS
        .iter()
        .map(|(label, _)| (*label).to_owned())
        .collect();
    let actions = COMMANDS.iter().map(|(_, action)| *action).collect();
    PaletteItems {
        picker: PickerState::new(" Commands ", items),
        actions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every curated command is a context-free action — none of the navigation /
    /// tab / row / method / inline-edit actions that only make sense from a
    /// specific pane keymap. This guards against accidentally re-adding a no-op.
    #[test]
    fn palette_excludes_context_only_actions() {
        let banned = [
            Action::Up,
            Action::Down,
            Action::Select,
            Action::Collapse,
            Action::Expand,
            Action::Top,
            Action::Bottom,
            Action::HalfPageUp,
            Action::HalfPageDown,
            Action::Jump,
            Action::OpenSearch,
            Action::OpenPalette,
            Action::TabNext,
            Action::TabPrev,
            Action::Tab1,
            Action::Tab2,
            Action::Tab3,
            Action::Tab4,
            Action::RowAdd,
            Action::RowDelete,
            Action::RowToggle,
            Action::RowEdit,
            Action::MethodCycle,
            Action::MethodMenu,
            Action::EditUrl,
            Action::FocusNext,
            Action::FocusPrev,
            // Response-pane actions that need cursor/pane context stay keymap-only.
            Action::OpenBodySearch,
            Action::SearchNext,
            Action::SearchPrev,
            Action::ToggleFold,
            Action::ToggleAllFolds,
            Action::CopyResponse,
            Action::CopyLine,
        ];
        for (_, action) in COMMANDS {
            assert!(
                !banned.contains(action),
                "{action:?} is context-only and must not be in the palette"
            );
        }
    }
}
