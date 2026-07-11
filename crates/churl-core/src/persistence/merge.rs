//! Format-preserving TOML merge cluster extracted from `persistence.rs`:
//! the recursive `merge_*` walkers that fold a freshly serialized document into an
//! existing one, touching only changed values so all decor survives. Child module
//! of `persistence`, so it keeps full access to the parent's imports without any
//! visibility widening — pure movement, no logic changes. `merge_tables` is
//! `pub(super)` (called by `save_value` in `mod.rs`); its recursive helpers are
//! private (called only within this module).

use super::*;

/// Merges `new` into `old` in place, preserving `old`'s decor wherever the value is
/// unchanged:
///
/// - keys present in `old` but absent in `new` are removed;
/// - unchanged values are left untouched (their comments/formatting survive);
/// - changed scalar values are replaced, copying the old value's decor so inline
///   comments survive;
/// - nested tables recurse; arrays-of-tables merge element-wise (see
///   [`merge_arrays_of_tables`]) — survivors keep their decor even when the array
///   grows or shrinks; only added/removed tables change.
pub(super) fn merge_tables(old: &mut Table, new: &Table) {
    let stale: Vec<String> = old
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| !new.contains_key(key))
        .collect();
    for key in stale {
        old.remove(&key);
    }
    for (key, new_item) in new.iter() {
        match old.get_mut(key) {
            Some(old_item) => merge_items(old_item, new_item),
            None => {
                old.insert(key, new_item.clone());
            }
        }
    }
}

/// Recursive worker for [`merge_tables`].
fn merge_items(old: &mut Item, new: &Item) {
    match (old, new) {
        (Item::Table(old_table), Item::Table(new_table)) => merge_tables(old_table, new_table),
        (Item::ArrayOfTables(old_tables), Item::ArrayOfTables(new_tables)) => {
            merge_arrays_of_tables(old_tables, new_tables);
        }
        (Item::Value(old_value), Item::Value(new_value)) => {
            if !values_equal(old_value, new_value) {
                let decor = old_value.decor().clone();
                *old_value = new_value.clone();
                *old_value.decor_mut() = decor;
            }
        }
        (old, new) => *old = new.clone(),
    }
}

/// Merges two arrays-of-tables element-wise, preserving the decor
/// (comments/whitespace/order) of every surviving table — even when the length
/// changes. Overlapping indices recurse through [`merge_tables`] (so a
/// survivor keeps its `# comments`); genuinely new trailing tables are appended
/// from `new`; trailing tables that disappeared are truncated.
///
/// This replaces the old wholesale `*old = new.clone()` on any length change,
/// which discarded all decor on surviving siblings — breaking the module's
/// format-preserving promise for `[[array-of-tables]]` (e.g. `[[request.headers]]`).
fn merge_arrays_of_tables(old: &mut ArrayOfTables, new: &ArrayOfTables) {
    // 1. Recurse into the overlapping prefix — survivors keep their decor.
    let overlap = old.len().min(new.len());
    for i in 0..overlap {
        if let (Some(old_table), Some(new_table)) = (old.get_mut(i), new.get(i)) {
            merge_tables(old_table, new_table);
        }
    }
    // 2. Append tables that are new (grew).
    for i in old.len()..new.len() {
        if let Some(new_table) = new.get(i) {
            old.push(new_table.clone());
        }
    }
    // 3. Truncate tables that disappeared (shrank) — remove from the tail.
    while old.len() > new.len() {
        old.remove(old.len() - 1);
    }
}

/// Semantic value equality, ignoring decor (whitespace/comments) and string style.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Float(a), Value::Float(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Datetime(a), Value::Datetime(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(item_a, item_b)| values_equal(item_a, item_b))
        }
        (Value::InlineTable(a), Value::InlineTable(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value_a)| {
                    b.get(key)
                        .is_some_and(|value_b| values_equal(value_a, value_b))
                })
        }
        _ => false,
    }
}
