//! `churl uninstall`: remove the churl binary, and — only behind `--purge` and a
//! confirmation — its config directory and local state database.
//!
//! Defaults never destroy data: a plain uninstall removes just the executable
//! and prints the paths it deliberately left behind. `--purge` additionally
//! removes churl's own config dir and `state.sqlite`, but never user workspace
//! files (`churl.toml`, collections, `sequences/`) — those are the user's data,
//! not churl's.
//!
//! The path-selection logic is a pure function ([`removal_plan`]) so it is
//! unit-tested against tempdir-scoped config/state paths, asserting workspace
//! files are never selected. The TTY prompt is not part of that function.

use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::{Context, eyre};

/// The set of paths an uninstall would touch, split by intent so the caller can
/// print what stays vs. what goes. Pure data — no I/O.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemovalPlan {
    /// Paths to delete: always the binary; under `--purge` also the config dir
    /// and state db when they are known.
    pub remove: Vec<PathBuf>,
    /// Paths deliberately left in place (printed for the user). Populated only
    /// on a non-purge run, where config/state are known but kept.
    pub kept: Vec<PathBuf>,
}

/// Computes which paths an uninstall removes, given the running executable, the
/// churl config directory, the state-db path, and whether `--purge` was passed.
/// `config_dir`/`state_db` are `Option` because platform discovery can fail;
/// a `None` is simply omitted (never guessed).
///
/// Guarantees, asserted by tests:
/// - the binary is always in `remove`;
/// - config dir + state db move to `remove` **only** under `purge`, else `kept`;
/// - nothing outside these churl-owned paths is ever selected (workspace files
///   are never passed in, so they can never appear).
pub fn removal_plan(
    exe: &Path,
    config_dir: Option<&Path>,
    state_db: Option<&Path>,
    purge: bool,
) -> RemovalPlan {
    let mut plan = RemovalPlan {
        remove: vec![exe.to_owned()],
        kept: Vec::new(),
    };
    for path in [config_dir, state_db].into_iter().flatten() {
        if purge {
            plan.remove.push(path.to_owned());
        } else {
            plan.kept.push(path.to_owned());
        }
    }
    plan
}

/// The churl config *directory* (parent of the config file), honouring the
/// `CHURL_CONFIG` override exactly as [`churl_core::config::load_global_config`]
/// does. `None` when neither the override nor platform discovery yields a path.
fn config_dir() -> Option<PathBuf> {
    if let Some(path) =
        std::env::var_os(churl_core::config::CONFIG_PATH_ENV).filter(|v| !v.is_empty())
    {
        // The override names the config *file*; its parent is the dir churl owns.
        return Path::new(&path).parent().map(Path::to_owned);
    }
    churl_core::config::global_config_path().and_then(|p| p.parent().map(Path::to_owned))
}

/// `churl uninstall`: remove the binary (default) and, under `--purge` + a
/// confirmation, churl's config dir and state db too.
///
/// - `purge`: also delete the config dir + `state.sqlite` (behind a confirm).
/// - `yes`: skip the confirmation prompt.
pub fn run_uninstall(purge: bool, yes: bool) -> Result<()> {
    let exe = std::env::current_exe().wrap_err("cannot locate the running executable")?;
    let plan = removal_plan(&exe, config_dir().as_deref(), state_db().as_deref(), purge);

    if purge {
        println!("The following will be permanently removed:");
        for path in &plan.remove {
            println!("  {}", path.display());
        }
        if !yes && !confirm("Remove churl and all its data?")? {
            println!("uninstall cancelled.");
            return Ok(());
        }
    }

    // Remove churl-owned data first, then the binary last (so a failure removing
    // data still leaves a working binary to retry with).
    let mut errors = Vec::new();
    for path in plan.remove.iter().filter(|p| **p != exe) {
        if let Err(err) = remove_path(path) {
            errors.push(format!("{}: {err}", path.display()));
        }
    }
    if let Err(err) = remove_path(&exe) {
        errors.push(format!("{}: {err}", exe.display()));
    }
    if !errors.is_empty() {
        return Err(eyre!(
            "uninstall completed with errors:\n  {}",
            errors.join("\n  ")
        ));
    }

    if !purge {
        println!("Removed churl binary at {}.", exe.display());
        if !plan.kept.is_empty() {
            println!("Left in place (remove with `churl uninstall --purge`):");
            for path in &plan.kept {
                println!("  {}", path.display());
            }
        }
    } else {
        println!("churl and its data have been removed.");
    }
    Ok(())
}

/// The local state database path (`<data_dir>/churl/state.sqlite`).
fn state_db() -> Option<PathBuf> {
    churl_core::history::default_state_path()
}

/// Removes a file or directory (recursively for a dir). A path that does not
/// exist is a success — uninstalling something already gone is not an error.
fn remove_path(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Prompts `question [y/N]` on stderr; anything but an affirmative is a decline.
fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    eprint!("{question} [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .wrap_err("failed to read confirmation")?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_plan_removes_only_binary_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("churl");
        let config = dir.path().join("config");
        let state = dir.path().join("state.sqlite");

        let plan = removal_plan(&exe, Some(&config), Some(&state), false);
        assert_eq!(plan.remove, vec![exe.clone()]);
        assert_eq!(plan.kept, vec![config, state]);
    }

    #[test]
    fn purge_plan_removes_binary_config_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("churl");
        let config = dir.path().join("config");
        let state = dir.path().join("state.sqlite");

        let plan = removal_plan(&exe, Some(&config), Some(&state), true);
        assert_eq!(plan.remove, vec![exe, config, state]);
        assert!(plan.kept.is_empty());
    }

    #[test]
    fn plan_omits_unknown_paths_never_guesses() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("churl");
        // Neither config nor state resolvable → only the binary is planned.
        let plan = removal_plan(&exe, None, None, true);
        assert_eq!(plan.remove, vec![exe]);
    }

    #[test]
    fn purge_never_selects_workspace_files() {
        // Model a workspace alongside churl's own dirs; only churl-owned paths
        // are ever passed to the plan, so workspace files can't be selected.
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("churl");
        let config = dir.path().join("config");
        let state = dir.path().join("state.sqlite");
        let workspace_manifest = dir.path().join("workspace/churl.toml");
        let collection = dir.path().join("workspace/users/list.toml");
        let sequences = dir.path().join("workspace/sequences");

        let plan = removal_plan(&exe, Some(&config), Some(&state), true);

        for hazard in [&workspace_manifest, &collection, &sequences] {
            assert!(
                !plan.remove.contains(hazard),
                "workspace file must never be removed: {}",
                hazard.display()
            );
        }
    }

    #[test]
    fn remove_path_missing_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        assert!(remove_path(&dir.path().join("nope")).is_ok());
    }

    #[test]
    fn remove_path_deletes_file_and_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();
        remove_path(&file).unwrap();
        assert!(!file.exists());

        let sub = dir.path().join("d");
        std::fs::create_dir_all(sub.join("inner")).unwrap();
        std::fs::write(sub.join("inner/f"), b"x").unwrap();
        remove_path(&sub).unwrap();
        assert!(!sub.exists());
    }
}
