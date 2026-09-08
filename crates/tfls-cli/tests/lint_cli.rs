//! Integration tests for the `tfls-lint` binary: drives the built
//! executable as a subprocess (per the `testing` skill's "spin up
//! the real thing" guidance) against the shared engine fixture,
//! asserting on stdout/stderr text and exit code.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;
use std::process::Command;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tfls-engine/tests/fixtures/basic"
);

fn lint_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tfls-lint"))
}

/// Copies the shared engine fixture into a fresh tempdir so a test can add
/// its own `.tfls.json` without mutating the fixture other tests rely on.
fn copy_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for entry in fs::read_dir(FIXTURE).expect("read fixture dir") {
        let entry = entry.expect("dir entry");
        let src = entry.path();
        if src.is_file() {
            let dest = dir.path().join(entry.file_name());
            fs::copy(&src, &dest).expect("copy fixture file");
        }
    }
    dir
}

fn write_config(dir: &Path, contents: &str) {
    fs::write(dir.join(".tfls.json"), contents).expect("write .tfls.json");
}

#[test]
fn default_fail_on_error_is_clean_on_warnings_only_fixture() {
    let output = lint_cmd()
        .args([FIXTURE, "--schemas", "none"])
        .output()
        .expect("failed to run tfls-lint");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout:\n{stdout}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        stdout.contains("main.tf:") && stdout.contains("[terraform_undefined_reference]"),
        "stdout missing undefined-reference line:\n{stdout}"
    );
    assert!(
        stdout.contains("[terraform_unused_declarations]"),
        "stdout missing unused-declarations line:\n{stdout}"
    );
}

#[test]
fn fail_on_warning_trips_exit_1() {
    let output = lint_cmd()
        .args([FIXTURE, "--schemas", "none", "--fail-on", "warning"])
        .output()
        .expect("failed to run tfls-lint");

    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn rule_off_suppresses_unused_declarations() {
    let output = lint_cmd()
        .args([
            FIXTURE,
            "--schemas",
            "none",
            "--rule",
            "terraform_unused_declarations=off",
            "--fail-on",
            "warning",
        ])
        .output()
        .expect("failed to run tfls-lint");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("terraform_unused_declarations"),
        "unused-declarations line should be suppressed:\n{stdout}"
    );
}

#[test]
fn rule_severity_bump_to_error_trips_default_fail_on() {
    let output = lint_cmd()
        .args([
            FIXTURE,
            "--schemas",
            "none",
            "--rule",
            "terraform_undefined_reference=error",
        ])
        .output()
        .expect("failed to run tfls-lint");

    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn format_json_parses_and_leaves_stderr_empty() {
    let output = lint_cmd()
        .args([FIXTURE, "--schemas", "none", "--format", "json"])
        .output()
        .expect("failed to run tfls-lint");

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.is_empty(), "stderr should be empty:\n{stderr}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid json");
    assert_eq!(value["summary"]["warnings"], 3);
}

#[test]
fn format_sarif_parses_with_expected_tool_name() {
    let output = lint_cmd()
        .args([FIXTURE, "--schemas", "none", "--format", "sarif"])
        .output()
        .expect("failed to run tfls-lint");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid json");
    assert_eq!(value["runs"][0]["tool"]["driver"]["name"], "tfls-lint");
}

#[test]
fn format_github_lines_all_start_with_workflow_command() {
    let output = lint_cmd()
        .args([FIXTURE, "--schemas", "none", "--format", "github"])
        .output()
        .expect("failed to run tfls-lint");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "expected 3 finding lines:\n{stdout}");
    for line in lines {
        assert!(
            line.starts_with("::warning file=main.tf,"),
            "unexpected line shape: {line}"
        );
    }
}

#[test]
fn nonexistent_path_exits_2_with_error_prefix() {
    let output = lint_cmd()
        .args(["/nonexistent/path/does-not-exist", "--schemas", "none"])
        .output()
        .expect("failed to run tfls-lint");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("error:"),
        "stderr should start with 'error:':\n{stderr}"
    );
}

#[test]
fn discovered_config_file_suppresses_unused_declarations() {
    let dir = copy_fixture();
    write_config(
        dir.path(),
        r#"{"rules": {"terraform_unused_declarations": "off"}}"#,
    );

    let output = lint_cmd()
        .args([dir.path().to_str().expect("utf8 path"), "--schemas", "none"])
        .output()
        .expect("failed to run tfls-lint");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("terraform_unused_declarations"),
        "unused-declarations line should be suppressed by discovered config:\n{stdout}"
    );
}

#[test]
fn cli_flag_wins_over_discovered_config_file() {
    let dir = copy_fixture();
    write_config(
        dir.path(),
        r#"{"rules": {"terraform_unused_declarations": "off"}}"#,
    );

    let output = lint_cmd()
        .args([
            dir.path().to_str().expect("utf8 path"),
            "--schemas",
            "none",
            "--rule",
            "terraform_unused_declarations=warning",
        ])
        .output()
        .expect("failed to run tfls-lint");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("terraform_unused_declarations"),
        "flag should re-enable the rule the config file turned off:\n{stdout}"
    );
}

#[test]
fn no_config_flag_skips_discovery() {
    let dir = copy_fixture();
    write_config(
        dir.path(),
        r#"{"rules": {"terraform_unused_declarations": "off"}}"#,
    );

    let output = lint_cmd()
        .args([
            dir.path().to_str().expect("utf8 path"),
            "--schemas",
            "none",
            "--no-config",
        ])
        .output()
        .expect("failed to run tfls-lint");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("terraform_unused_declarations"),
        "--no-config should skip the discovered file entirely:\n{stdout}"
    );
}

#[test]
fn explicit_config_pointing_at_invalid_json_exits_2() {
    let dir = copy_fixture();
    let config_path = dir.path().join("bad-config.json");
    fs::write(&config_path, "[]").expect("write bad config");

    let output = lint_cmd()
        .args([
            dir.path().to_str().expect("utf8 path"),
            "--schemas",
            "none",
            "--config",
            config_path.to_str().expect("utf8 path"),
        ])
        .output()
        .expect("failed to run tfls-lint");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("error:"),
        "stderr should start with 'error:':\n{stderr}"
    );
}
