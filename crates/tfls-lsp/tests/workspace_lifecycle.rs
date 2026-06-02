//! Wire-level tests for workspace-notification diagnostic lifecycle:
//! `workspace/didChangeConfiguration` and
//! `workspace/didChangeWatchedFiles`.
//!
//! These exercise handler behaviour that has no return value to assert
//! on — the effect is a server→client `publishDiagnostics`. They use the
//! shared [`support::TestClient`] mock client to capture that wire
//! traffic, covering bugs that were previously untestable without one:
//! a live `styleRules` toggle that must republish open buffers, and a
//! watched-file deletion that must clear the deleted file's diagnostics
//! and refresh its peers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use support::{TestClient, any_message_contains, contains_undefined_var};

/// A per-rule override of `off` suppresses that rule's diagnostics; a
/// severity override remaps them — both live via didChangeConfiguration.
#[tokio::test]
async fn per_rule_override_suppresses_and_remaps() {
    let mut client = TestClient::new();
    client.initialize(None).await;

    let uri = "file:///mod/main.tf";
    client
        .did_open(uri, "variable \"x\" {}\nvariable \"x\" {}\n")
        .await;
    client.settle(150).await;

    // Baseline: duplicate-definition error fires by default.
    let baseline = client.last_diagnostics(uri).await;
    assert!(
        any_message_contains(&baseline, "duplicate variable `x`"),
        "baseline expected, got {baseline:?}"
    );

    // Remap to a hint (severity 4).
    client
        .did_change_configuration(serde_json::json!({
            "terraform-ls-rs": { "rules": { "terraform_duplicate_definition": "hint" } }
        }))
        .await;
    client.settle(200).await;
    let remapped = client.last_diagnostics(uri).await;
    let dup = remapped
        .iter()
        .find(|d| {
            d.get("message")
                .and_then(|m| m.as_str())
                .is_some_and(|m| m.contains("duplicate variable"))
        })
        .expect("duplicate diagnostic still present after remap");
    assert_eq!(dup.get("severity").and_then(|s| s.as_i64()), Some(4), "expected HINT severity");

    // Now turn it off entirely.
    client
        .did_change_configuration(serde_json::json!({
            "terraform-ls-rs": { "rules": { "terraform_duplicate_definition": "off" } }
        }))
        .await;
    client.settle(200).await;
    let off = client.last_diagnostics(uri).await;
    assert!(
        !any_message_contains(&off, "duplicate variable"),
        "rule set to off must be suppressed, got {off:?}"
    );

    client.shutdown().await;
}

/// A same-file duplicate definition is a hard `terraform validate` error;
/// the server must publish it by default (not behind any opt-in).
#[tokio::test]
async fn duplicate_variable_published_as_error() {
    let mut client = TestClient::new();
    client.initialize(None).await;

    let uri = "file:///mod/main.tf";
    client
        .did_open(uri, "variable \"region\" {}\nvariable \"region\" {}\n")
        .await;
    client.settle(150).await;

    let diags = client.last_diagnostics(uri).await;
    assert!(
        any_message_contains(&diags, "duplicate variable `region`"),
        "expected a duplicate-definition error by default, got {diags:?}"
    );
    client.shutdown().await;
}

/// A sensitive variable declared in one file leaking into a plain output
/// in another file must be flagged — exercising the module-wide
/// sensitive-variable aggregation.
#[tokio::test]
async fn sensitive_var_leaking_into_output_across_files() {
    let mut client = TestClient::new();
    client.initialize(None).await;

    let vars_uri = "file:///mod/variables.tf";
    let out_uri = "file:///mod/outputs.tf";

    client
        .did_open(vars_uri, "variable \"pw\" { sensitive = true }\n")
        .await;
    client.settle(100).await;
    client
        .did_open(out_uri, "output \"p\" { value = var.pw }\n")
        .await;
    client.settle(200).await;

    let diags = client.last_diagnostics(out_uri).await;
    assert!(
        any_message_contains(&diags, "exposes a sensitive value"),
        "expected a sensitive-output error, got {diags:?}"
    );
    client.shutdown().await;
}

/// Toggling `styleRules` on via didChangeConfiguration must recompute and
/// republish open buffers — without the user editing them. A
/// `documented_variables` (style-pack) diagnostic is the probe: it only
/// fires when styleRules is on.
#[tokio::test]
async fn style_rules_toggle_republishes_open_docs() {
    let mut client = TestClient::new();
    client.initialize(None).await;

    let uri = "file:///mod/main.tf";
    // A variable with no `description` — flagged only by the style pack.
    client.did_open(uri, "variable \"region\" {}\n").await;
    client.settle(150).await;

    // Baseline: style rules off by default, so no documentation nag.
    let baseline = client.last_diagnostics(uri).await;
    assert!(
        !any_message_contains(&baseline, "description"),
        "style diagnostic should be absent by default, got {baseline:?}"
    );
    let publishes_before = client.publish_count(uri).await;

    // Toggle styleRules on. No edit follows — the republish must be
    // driven purely by the config change.
    client
        .did_change_configuration(serde_json::json!({
            "terraform-ls-rs": { "styleRules": true }
        }))
        .await;
    client.settle(200).await;

    let after = client.publish_count(uri).await;
    assert!(
        after > publishes_before,
        "config change must trigger a republish for the open doc \
         (before={publishes_before}, after={after})"
    );
    let latest = client.last_diagnostics(uri).await;
    assert!(
        any_message_contains(&latest, "description"),
        "after enabling styleRules the documentation diagnostic must \
         appear without an edit; got {latest:?}"
    );

    client.shutdown().await;
}

/// Deleting a file via didChangeWatchedFiles must (a) clear that file's
/// own published diagnostics and (b) refresh open peers — deleting
/// variables.tf re-introduces the undefined-var diagnostic in main.tf.
#[tokio::test]
async fn watched_file_delete_clears_and_refreshes_peers() {
    let mut client = TestClient::new();
    client.initialize(None).await;

    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";

    // Open variables.tf (declaring `foo`) FIRST so it's indexed before
    // main.tf's initial diagnostic compute resolves the reference.
    client.did_open(vars_uri, "variable \"foo\" {}\n").await;
    client.settle(100).await;
    client
        .did_open(main_uri, "output \"x\" { value = var.foo }\n")
        .await;
    client.settle(200).await;

    // With variables.tf declaring `foo`, main.tf has no undefined-var.
    let baseline = client.last_diagnostics(main_uri).await;
    assert!(
        !contains_undefined_var(&baseline, "foo"),
        "baseline: `foo` is declared, main.tf should be clean, got {baseline:?}"
    );

    // User deletes variables.tf on disk; the editor reports it watched.
    client
        .did_change_watched_files(&[(vars_uri, 3 /* Deleted */)])
        .await;
    client.settle(250).await;

    // (a) the deleted file gets an empty publish (cleared in the client).
    let vars_last = client.last_diagnostics(vars_uri).await;
    assert!(
        vars_last.is_empty(),
        "deleted file's diagnostics must be cleared, got {vars_last:?}"
    );

    // (b) main.tf now references an undefined `foo` again.
    let main_pushes = client.publishes_for(main_uri).await;
    let main_last = main_pushes.last().cloned().unwrap_or_default();
    assert!(
        contains_undefined_var(&main_last, "foo"),
        "after deleting variables.tf, main.tf must re-flag undefined \
         `var.foo`; all main.tf pushes were {main_pushes:?}"
    );

    client.shutdown().await;
}
