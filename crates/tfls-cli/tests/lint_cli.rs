//! Integration tests for the `tfls-lint` binary: drives the built
//! executable as a subprocess (per the `testing` skill's "spin up
//! the real thing" guidance) against the shared engine fixture,
//! asserting on stdout/stderr text and exit code.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../tfls-engine/tests/fixtures/basic"
);

fn lint_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tfls-lint"))
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
