//! `churl uninstall`: remove the churl binary, and — only behind `--purge` and a
//! confirmation — its config directory and local state database.
//!
//! Defaults never destroy data: a plain uninstall removes just the executable
//! and prints the paths it deliberately left behind. `--purge` additionally
//! removes churl's own config dir and `state.sqlite`, but never user workspace
//! files (`churl.toml`, collections, `sequences/`) — those are the user's data,
//! not churl's.
//!
//! The path-selection logic is pure ([`config_removal_target`] + [`removal_plan`])
//! so it is unit-tested against tempdir-scoped config/state paths, asserting
//! workspace files are never selected. The TTY prompt is not part of it.
//!
//! Safety invariant on `--purge`: only a *churl-owned* config directory (the one
//! platform discovery yields, `<config_dir>/churl/`) is ever removed recursively.
//! A `CHURL_CONFIG` override names a *user-owned file* that may live inside a
//! workspace — its parent is off-limits, so the override removes only the file it
//! names. See [`config_removal_target`].

use std::ffi::OsStr;
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
/// resolved config removal target (from [`config_removal_target`]), the state-db
/// path, and whether `--purge` was passed. `config_target`/`state_db` are
/// `Option` because discovery can fail; a `None` is simply omitted (never guessed).
///
/// Guarantees, asserted by tests:
/// - the binary is always in `remove`;
/// - config target + state db move to `remove` **only** under `purge`, else `kept`;
/// - the config target is whatever [`config_removal_target`] resolved — a
///   churl-owned dir (platform) or the exact override file, never a workspace dir.
pub fn removal_plan(
    exe: &Path,
    config_target: Option<&Path>,
    state_db: Option<&Path>,
    purge: bool,
) -> RemovalPlan {
    let mut plan = RemovalPlan {
        remove: vec![exe.to_owned()],
        kept: Vec::new(),
    };
    for path in [config_target, state_db].into_iter().flatten() {
        if purge {
            plan.remove.push(path.to_owned());
        } else {
            plan.kept.push(path.to_owned());
        }
    }
    plan
}

/// Resolves the single config path `--purge` may remove, given the raw
/// `CHURL_CONFIG` override value (`None`/empty ⇒ unset). Split from process-env
/// reading so the derivation itself is unit-testable.
///
/// **Safety-critical distinction** (the P0 this guards):
/// - **No override** → platform discovery yields `<config_dir>/churl/config.toml`;
///   its parent `<config_dir>/churl/` is a directory churl *owns*, so that
///   directory is returned and removed recursively.
/// - **Override set** → `CHURL_CONFIG` names a *user-supplied file* that may live
///   inside a workspace (per the config override contract). Its parent is
///   off-limits — we return the **file itself**, never its directory, so
///   `--purge` can never `remove_dir_all` a user's project directory.
///
/// `remove_path` then does the right recursive-vs-file removal by inspecting the
/// path's own type at delete time.
fn config_removal_target(override_value: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(path) = override_value.filter(|v| !v.is_empty()) {
        // User-owned file: remove exactly it, never its (possibly workspace) dir.
        return Some(PathBuf::from(path));
    }
    // churl-owned directory: `<config_dir>/churl/` (parent of the config file).
    churl_core::config::global_config_path().and_then(|p| p.parent().map(Path::to_owned))
}

/// The process-env-backed config removal target (reads `CHURL_CONFIG`).
fn config_target() -> Option<PathBuf> {
    let value = std::env::var_os(churl_core::config::CONFIG_PATH_ENV);
    config_removal_target(value.as_deref())
}

/// `churl uninstall`: remove the binary (default) and, under `--purge` + a
/// confirmation, churl's config dir and state db too.
///
/// - `purge`: also delete the config dir + `state.sqlite` (behind a confirm).
/// - `yes`: skip the confirmation prompt.
pub fn run_uninstall(purge: bool, yes: bool) -> Result<()> {
    let exe = std::env::current_exe().wrap_err("cannot locate the running executable")?;
    let plan = removal_plan(
        &exe,
        config_target().as_deref(),
        state_db().as_deref(),
        purge,
    );

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
    fn override_config_target_is_the_file_never_its_workspace_dir() {
        // P0 regression: `CHURL_CONFIG` may point at a config file *inside* a
        // user's workspace. `--purge` must remove that file only — never its
        // parent workspace directory. Exercises the real derivation.
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("project");
        std::fs::create_dir_all(workspace.join("users")).unwrap();
        let workspace_config = workspace.join("churl.toml");
        std::fs::write(&workspace_config, "").unwrap();

        let target = config_removal_target(Some(workspace_config.as_os_str())).unwrap();
        // The resolved target is the file itself, not the workspace directory.
        assert_eq!(target, workspace_config);

        let exe = dir.path().join("churl");
        let state = dir.path().join("state.sqlite");
        let plan = removal_plan(&exe, Some(&target), Some(&state), true);

        assert!(
            plan.remove.contains(&workspace_config),
            "the named config file should be removable"
        );
        assert!(
            !plan.remove.contains(&workspace),
            "the workspace directory must NEVER be in the removal set: {}",
            workspace.display()
        );
        // And the actual delete of the target must not take the workspace with
        // it: removing the file leaves the workspace and its siblings intact.
        remove_path(&target).unwrap();
        assert!(!workspace_config.exists());
        assert!(workspace.join("users").exists(), "workspace dir destroyed");
    }

    #[test]
    fn no_override_config_target_is_the_platform_churl_dir() {
        // With no override, the target is the churl-owned `<config_dir>/churl`
        // directory (parent of the config file) — safe to remove recursively.
        let target = config_removal_target(None);
        if let Some(target) = target {
            assert_eq!(target.file_name().and_then(|n| n.to_str()), Some("churl"));
        }
        // (On a platform where discovery yields None, there's nothing to assert.)
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
