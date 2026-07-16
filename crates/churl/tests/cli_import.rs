//! Integration tests for the `churl import` subcommand, driving the real binary.

use std::process::{Command, Output};

fn churl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_churl"))
        .args(args)
        .output()
        .expect("failed to spawn churl")
}

#[test]
fn import_prints_endpoint_toml_to_stdout() {
    let output = churl(&[
        "import",
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
    let output = churl(&["import", "curl -k https://api.example.com/x"]);
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
fn import_error_exits_nonzero_with_message_on_stderr() {
    let output = churl(&["import", "curl --explode https://api.example.com/x"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown flag"), "stderr: {stderr}");
}
