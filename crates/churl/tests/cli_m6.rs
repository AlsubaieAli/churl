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
fn keymaps_prints_leader_section() {
    // The default map prints a Leader section listing the built-in continuations.
    let dir = tempfile::tempdir().unwrap();
    let envs = planted_config(dir.path(), "");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_churl"));
    cmd.arg("keymaps");
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn churl");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // A "Leader" header appears, with toggle-explorer bound to `e`.
    let leader_idx = stdout
        .find("\nLeader\n")
        .unwrap_or_else(|| panic!("no Leader section:\n{stdout}"));
    let after = &stdout[leader_idx..];
    assert!(
        after.contains("toggle-explorer"),
        "leader lists toggle-explorer: {after}"
    );
}

#[test]
fn keymaps_marks_overridden_leader_binding() {
    // A [keys.leader] override marks that action overridden in the Leader section.
    let dir = tempfile::tempdir().unwrap();
    let envs = planted_config(dir.path(), "[keys.leader]\nx = \"save\"\n");
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
    let leader_idx = stdout.find("\nLeader\n").expect("Leader section");
    let after = &stdout[leader_idx..];
    // `save` now has a leader binding → the leader `save` line is overridden.
    let save_line = after
        .lines()
        .find(|l| l.trim_start().starts_with("save"))
        .unwrap_or_else(|| panic!("no save line in leader:\n{after}"));
    assert!(save_line.contains("(overridden)"), "{save_line}");
    assert!(save_line.contains('x'), "{save_line}");
}

#[test]
fn keymaps_applies_leader_submenu_remap() {
    // A real `[keys.leader.sequences]` / `[keys.leader.load]` config must parse
    // AND the remap must reach the effective keymap. `churl keymaps` renders the
    // effective map, so a remapped submenu key shows up as a full `<prefix> <key>`
    // chord for that action (guards the whole config → KeyMap seam end to end).
    let dir = tempfile::tempdir().unwrap();
    let envs = planted_config(
        dir.path(),
        concat!(
            "[keys.leader]\n",
            "x = \"quit\"\n\n",
            "[keys.leader.sequences]\n",
            "z = \"run-sequence\"\n\n",
            "[keys.leader.load]\n",
            "z = \"load-runner-pick\"\n",
        ),
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_churl"));
    cmd.arg("keymaps");
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn churl");
    assert!(
        output.status.success(),
        "config with leader submenu tables must load; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let leader_idx = stdout.find("\nLeader\n").expect("Leader section");
    let after = &stdout[leader_idx..];
    // run-sequence is reachable only through the sequences submenu; the remap
    // adds `s z` alongside the default `s r`.
    let run_line = after
        .lines()
        .find(|l| l.trim_start().starts_with("run-sequence"))
        .unwrap_or_else(|| panic!("no run-sequence line in leader:\n{after}"));
    assert!(
        run_line.contains("s z"),
        "run-sequence must show the remapped `s z` chord: {run_line}"
    );
    // load-runner-pick gains `l z`.
    let load_line = after
        .lines()
        .find(|l| l.trim_start().starts_with("load-runner-pick"))
        .unwrap_or_else(|| panic!("no load-runner-pick line in leader:\n{after}"));
    assert!(
        load_line.contains("l z"),
        "load-runner-pick must show the remapped `l z` chord: {load_line}"
    );
}

#[test]
fn keymaps_bad_leader_submenu_action_errors() {
    // An unknown action name in a leader submenu table fails loudly at startup.
    let dir = tempfile::tempdir().unwrap();
    let envs = planted_config(dir.path(), "[keys.leader.sequences]\nz = \"explode\"\n");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_churl"));
    cmd.arg("keymaps");
    for (k, v) in &envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn churl");
    assert!(
        !output.status.success(),
        "a bad submenu action name must fail the command"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("explode"),
        "error must name the bad action: {stderr}"
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
