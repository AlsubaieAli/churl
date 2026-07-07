//! Integration tests for the `--import-collection` launch flag, driving the
//! real binary. The success path continues into the TUI (which cannot run under
//! a test harness), so these cover the flag surface and the fail-loud error
//! path; the on-disk write is unit-tested in `main.rs::tests`.

use std::process::{Command, Output};

fn churl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .output()
        .expect("failed to spawn churl")
}

#[test]
fn help_lists_the_import_collection_flag() {
    let output = churl(&["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("--import-collection"),
        "help missing the flag:\n{stdout}"
    );
}

#[test]
fn import_collection_missing_file_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.json");
    let output = churl(&["--import-collection", missing.to_str().unwrap()]);
    assert!(
        !output.status.success(),
        "expected a nonzero exit for a missing import file"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("import failed"),
        "stderr missing the failure message:\n{stderr}"
    );
}

#[test]
fn import_collection_bad_json_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("bad.json");
    std::fs::write(&file, "{ not valid json").unwrap();
    let output = churl(&["--import-collection", file.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("import failed"), "stderr:\n{stderr}");
}
