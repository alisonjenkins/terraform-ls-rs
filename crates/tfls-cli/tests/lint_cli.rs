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
