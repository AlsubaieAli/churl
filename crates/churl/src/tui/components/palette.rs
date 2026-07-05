//! Command palette overlay (`:`): a [`PickerState`] over the static command
//! list; accepting an entry runs its [`Action`].

use crate::tui::events::Action;

use super::picker::PickerState;

/// Palette overlay contents: the picker plus the action behind each item.
#[derive(Debug)]
pub struct PaletteItems {
    /// Picker over human-readable command labels.
    pub picker: PickerState,
    /// Action per picker item, index-aligned with `picker.items`.
    pub actions: Vec<Action>,
}

/// Builds the command palette over every [`Action`].
pub fn open() -> PaletteItems {
    let actions: Vec<Action> = Action::all().collect();
    let items = actions
        .iter()
        .map(|action| action.label().to_owned())
        .collect();
    PaletteItems {
        picker: PickerState::new(" Commands ", items),
        actions,
    }
}
