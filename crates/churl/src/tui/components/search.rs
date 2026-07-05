//! Fuzzy endpoint search overlay (`/`): a [`PickerState`] over all endpoint
//! display paths, plus the mapping back to explorer tree positions.

use churl_core::persistence::PersistenceError;

use super::explorer::ExplorerState;
use super::picker::PickerState;

/// Search overlay contents: the picker plus, for each picker item, the
/// `(collection, endpoint)` indices it jumps to.
#[derive(Debug)]
pub struct SearchItems {
    /// Picker over endpoint display paths (`collection/endpoint name`).
    pub picker: PickerState,
    /// Jump target per picker item, index-aligned with `picker.items`.
    pub targets: Vec<(usize, usize)>,
}

/// Builds the search overlay, lazily loading every collection so all endpoint
/// paths are available as the haystack.
pub fn open(explorer: &mut ExplorerState) -> Result<SearchItems, PersistenceError> {
    let all = explorer.all_endpoints()?;
    let mut items = Vec::with_capacity(all.len());
    let mut targets = Vec::with_capacity(all.len());
    for (path, collection, endpoint) in all {
        items.push(path);
        targets.push((collection, endpoint));
    }
    Ok(SearchItems {
        picker: PickerState::new(" Search endpoints ", items),
        targets,
    })
}
