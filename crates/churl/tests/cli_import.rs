//! Integration tests for the `churl import` subcommand, driving the real binary.

use std::process::{Command, Output};

fn churl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .output()
        .expect("failed to spawn churl")
}

fn churl_in(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn churl")
}

#[test]
fn import_stdout_prints_endpoint_toml() {
    let output = churl(&[
        "import",
        "--stdout",
        r#"curl -X POST https://api.example.com/users -H 'Content-Type: application/json' -d '{"name": "Ada"}'"#,
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#"name = "users""#), "{stdout}");
    assert!(stdout.contains(r#"method = "POST""#), "{stdout}");
    assert!(
        stdout.contains(r#"url = "https://api.example.com/users""#),
        "{stdout}"
    );
    assert!(stdout.contains("[[request.headers]]"), "{stdout}");
    assert!(stdout.contains("[request.body]"), "{stdout}");
    assert!(stdout.contains(r#"type = "json""#), "{stdout}");
}

#[test]
fn import_warnings_go_to_stderr_not_stdout() {
    let output = churl(&["import", "--stdout", "curl -k https://api.example.com/x"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("warning:"), "stderr: {stderr}");
    assert!(!stdout.contains("warning:"), "stdout: {stdout}");
}

#[test]
fn import_name_flag_overrides_derived_name() {
    // Trailing-var-arg: `--name` must precede the curl command (everything after
    // the first curl token is captured as the command).
    let output = churl(&[
        "import",
        "--stdout",
        "--name",
        "list-users",
        "curl https://api.example.com/users",
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#"name = "list-users""#), "{stdout}");
}

#[test]
fn import_out_writes_a_loadable_endpoint_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("health.toml");
    let output = churl(&[
        "import",
        "--out",
        path.to_str().unwrap(),
        "curl https://api.example.com/health",
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoint = churl_core::persistence::load_endpoint(&path).unwrap();
    assert_eq!(endpoint.name, "health");
    assert_eq!(endpoint.request.url, "https://api.example.com/health");
}

#[test]
fn import_accepts_raw_multi_token_curl_from_the_shell() {
    // The exact failing shape: a raw curl whose args the shell already tokenised.
    // `-H` etc. must be captured (trailing var-arg), not parsed as churl flags, and
    // the single-quoted URL keeps its `\[\]` glob-escapes for `set_url` to undo.
    let output = churl(&[
        "import",
        "--stdout",
        "curl",
        r"https://api.example.com/orders/42?format=light&fields\[\]=is_blocked&fields\[\]=branches",
        "-H",
        "accept: application/json",
        "-H",
        "authorization: Bearer v4.public.SHORT",
        "-H",
        "s-source: merchant-dashboard",
    ]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(
            r#"url = "https://api.example.com/orders/42?format=light&fields[]=is_blocked&fields[]=branches""#
        ),
        "brackets unescaped, all tokens parsed: {stdout}"
    );
    assert!(stdout.contains(r#"type = "bearer""#), "{stdout}");
    assert!(stdout.contains(r#"token = "{{token}}""#), "{stdout}");
    // The real token is never printed.
    assert!(
        !stdout.contains("v4.public.SHORT"),
        "secret leaked: {stdout}"
    );
}

#[test]
fn import_error_exits_5_with_message_on_stderr() {
    let output = churl(&[
        "import",
        "--stdout",
        "curl --explode https://api.example.com/x",
    ]);
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(5),
        "expected the import-error exit band"
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown flag"), "stderr: {stderr}");
}

#[test]
fn import_json_envelope_on_parse_failure() {
    let output = churl(&[
        "--json",
        "import",
        "curl --explode https://api.example.com/x",
    ]);
    assert_eq!(output.status.code(), Some(5));
    assert!(
        output.stderr.is_empty(),
        "stderr should be silent in --json mode"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON envelope");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["ok"], false);
    assert_eq!(json["command"], "import");
    assert!(json["data"].is_null());
    assert_eq!(json["error"]["kind"], "not-a-curl-command");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown flag")
    );
}

#[test]
fn import_default_writes_into_the_cwd_workspace() {
    let dir = tempfile::tempdir().unwrap();
    // `churl init` first — the new default requires an existing workspace.
    let init = churl_in(dir.path(), &["init"]);
    assert!(
        init.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let output = churl_in(
        dir.path(),
        &["import", "curl https://api.example.com/widgets"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Written straight into the workspace root (no --out/--stdout given).
    let widget = dir.path().join("widgets.toml");
    assert!(widget.exists(), "missing {}", widget.display());
    let endpoint = churl_core::persistence::load_endpoint(&widget).unwrap();
    assert_eq!(endpoint.request.url, "https://api.example.com/widgets");
}

#[test]
fn import_default_without_a_workspace_is_a_no_workspace_error() {
    let dir = tempfile::tempdir().unwrap();
    let output = churl_in(dir.path(), &["import", "curl https://api.example.com/x"]);
    assert!(!output.status.success());
    assert_eq!(
        output.status.code(),
        Some(3),
        "expected the workspace/resolution exit band"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no churl workspace"), "stderr: {stderr}");
}

/// Runs the binary with `CHURL_CONFIG` pinned to a nonexistent file so a real
/// user config never skews a `run` (redirect/timeout) invocation.
fn churl_in_isolated(dir: &std::path::Path, args: &[&str]) -> Output {
    let missing_config = std::env::temp_dir().join(format!(
        "churl-cli-import-test-nonexistent-config-{}.toml",
        std::process::id()
    ));
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .current_dir(dir)
        .env("CHURL_CONFIG", missing_config)
        .output()
        .expect("failed to spawn churl")
}

#[test]
fn import_failed_secret_save_leaves_no_orphan_endpoint() {
    // P2-4: a curl carrying a name-anchored literal secret header is refused by
    // the strict save gate. The write must be atomic — NO `<slug>.toml` may be
    // left on disk (the old create-placeholder-then-overwrite path orphaned one).
    let dir = tempfile::tempdir().unwrap();
    assert!(churl_in(dir.path(), &["init"]).status.success());

    let output = churl_in(
        dir.path(),
        &[
            "import",
            "curl https://api.example.com/secure -H 'X-Api-Key: sk-live-0123456789abcdefGHIJ'",
        ],
    );
    assert!(
        !output.status.success(),
        "a refused-secret import must fail"
    );
    assert_eq!(output.status.code(), Some(5), "import-write band");

    // The workspace must be unchanged: only churl.toml, no orphaned endpoint.
    let stray: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".toml") && n != "churl.toml")
        .collect();
    assert!(
        stray.is_empty(),
        "orphaned endpoint file(s) left behind: {stray:?}"
    );
}

#[test]
fn import_same_url_twice_yields_two_distinct_addressable_endpoints() {
    // P2-5: a second import of the same URL collides on filename; the bumped
    // file must get a distinct `name` so it stays addressable via `run`.
    let dir = tempfile::tempdir().unwrap();
    assert!(churl_in(dir.path(), &["init"]).status.success());

    // Point at a connection-refused address so `run` resolves then fails fast at
    // transport (band 4) rather than reaching the network — proving resolution
    // succeeded without a real request.
    let first = churl_in(dir.path(), &["import", "curl http://127.0.0.1:1/thing"]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = churl_in(dir.path(), &["import", "curl http://127.0.0.1:1/thing"]);
    assert!(
        second.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // Both files exist with distinct names.
    let first_file = dir.path().join("thing.toml");
    let second_file = dir.path().join("thing-2.toml");
    assert!(first_file.exists(), "first import missing");
    assert!(second_file.exists(), "collision-bumped file missing");
    let ep1 = churl_core::persistence::load_endpoint(&first_file).unwrap();
    let ep2 = churl_core::persistence::load_endpoint(&second_file).unwrap();
    assert_eq!(ep1.name, "thing");
    assert_eq!(
        ep2.name, "thing-2",
        "bumped file must carry the addressable name"
    );
    // The second import warned about the rename (stderr).
    let stderr2 = String::from_utf8(second.stderr).unwrap();
    assert!(
        stderr2.contains("collision"),
        "expected a rename warning: {stderr2}"
    );

    // `run thing-2` RESOLVES (band 4 transport, not band-3 endpoint-not-found).
    let run2 = churl_in_isolated(dir.path(), &["--json", "run", "thing-2"]);
    assert_eq!(
        run2.status.code(),
        Some(4),
        "second endpoint must resolve then fail at transport, not 404: {}",
        String::from_utf8_lossy(&run2.stdout)
    );

    // A genuinely-absent name is still endpoint-not-found (band 3) — contrast.
    let run_missing = churl_in_isolated(dir.path(), &["--json", "run", "thing-9"]);
    assert_eq!(run_missing.status.code(), Some(3));
}
