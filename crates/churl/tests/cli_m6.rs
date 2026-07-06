//! Integration tests for the M6 CLI surface: `churl keymaps`, `--var` parsing,
//! and `--profile` validation, driving the real binary.

use std::process::{Command, Output};

fn churl_in(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn churl")
}

/// Writes `config.toml` to every location `dirs::config_dir()` might resolve to
/// under `home`, and returns the env vars (`HOME`, `XDG_CONFIG_HOME`) that pin the
/// config discovery there — portable across Linux (XDG) and macOS (Library).
fn planted_config(
    home: &std::path::Path,
    contents: &str,
) -> Vec<(&'static str, std::path::PathBuf)> {
    let xdg = home.join(".config");
    let mac = home.join("Library").join("Application Support");
    for base in [&xdg, &mac] {
        let dir = base.join("churl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), contents).unwrap();
    }
    vec![("HOME", home.to_path_buf()), ("XDG_CONFIG_HOME", xdg)]
}

#[test]
fn keymaps_prints_default_map() {
    // Isolate config (empty) so a real user config on the test box doesn't skew
    // the map: no [keys] overrides → every action is (default).
    let dir = tempfile::tempdir().unwrap();
    let envs = planted_config(dir.path(), "");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_churl"));
    cmd.arg("keymaps");
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn churl");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Every line for a default binding is marked (default); sorted by action name.
    assert!(stdout.contains("quit"), "{stdout}");
    assert!(stdout.contains("(default)"), "{stdout}");
    // The jump action and its default `f` binding show up.
    assert!(stdout.contains("jump"), "{stdout}");
    assert!(
        stdout.contains("(unbound)"),
        "switch-profile is unbound: {stdout}"
    );
}

#[test]
fn keymaps_marks_overridden_bindings() {
    // A config with a [keys] override marks that action overridden.
    let dir = tempfile::tempdir().unwrap();
    let envs = planted_config(dir.path(), "[keys]\n\"ctrl-p\" = \"open-palette\"\n");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_churl"));
    cmd.arg("keymaps");
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn churl");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    // open-palette now has an extra binding → marked overridden.
    let palette_line = stdout
        .lines()
        .find(|l| l.starts_with("open-palette"))
        .unwrap_or_else(|| panic!("no open-palette line:\n{stdout}"));
    assert!(palette_line.contains("(overridden)"), "{palette_line}");
    assert!(
        palette_line.to_ascii_lowercase().contains("ctrl-p"),
        "{palette_line}"
    );
}

#[test]
fn bad_var_format_is_hard_error() {
    // `--var` without `=` fails before launching the TUI.
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["--var", "noequalshere"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("noequalshere"), "{stderr}");
}

#[test]
fn unknown_profile_is_hard_error() {
    // A workspace with one profile; --profile names a missing one.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("churl.toml"),
        "name = \"demo\"\n\n[[profiles]]\nname = \"dev\"\n",
    )
    .unwrap();
    let output = churl_in(dir.path(), &["--profile", "staging"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("staging"), "{stderr}");
    assert!(
        stderr.contains("dev"),
        "available profiles listed: {stderr}"
    );
}
