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

use support::{any_message_contains, contains_undefined_var, TestClient};

/// An unformatted file gets a `terraform_fmt` INFORMATION diagnostic;
/// editing it to/from formatted re-evaluates (the format cache clears on
/// every edit, so a change that breaks formatting is picked up).
#[tokio::test]
async fn formatting_diagnostic_tracks_edits() {
    let mut client = TestClient::new();
    client.initialize(None).await;
    let uri = "file:///mod/main.tf";

    // Open already-formatted content — no fmt diagnostic.
    client
        .did_open(uri, "variable \"x\" {\n  default = \"a\"\n}\n")
        .await;
    client.settle(150).await;
    assert!(
        !any_message_contains(&client.last_diagnostics(uri).await, "is not formatted"),
        "formatted file should have no fmt diagnostic"
    );

    // Edit into an UNFORMATTED state (`default="a"` — missing spaces).
    client
        .did_change_full(uri, 2, "variable \"x\" { default=\"a\" }\n")
        .await;
    client.settle(150).await;
    let d = client.last_diagnostics(uri).await;
    let fmt = d
        .iter()
        .find(|x| {
            x.get("message")
                .and_then(|m| m.as_str())
                .is_some_and(|m| m.contains("is not formatted"))
        })
        .expect("unformatted edit must surface a fmt diagnostic");
    assert_eq!(
        fmt.get("severity").and_then(|s| s.as_i64()),
        Some(3),
        "INFORMATION severity"
    );

    // Edit back to formatted — the diagnostic clears.
    client
        .did_change_full(uri, 3, "variable \"x\" {\n  default = \"a\"\n}\n")
        .await;
    client.settle(150).await;
    assert!(
        !any_message_contains(&client.last_diagnostics(uri).await, "is not formatted"),
        "re-formatted file should clear the fmt diagnostic"
    );

    // It is disable-able via the per-rule config.
    client
        .did_change_full(uri, 4, "variable \"x\" { default=\"a\" }\n")
        .await;
    client.settle(150).await;
    assert!(any_message_contains(
        &client.last_diagnostics(uri).await,
        "is not formatted"
    ));
    client
        .did_change_configuration(serde_json::json!({
            "terraform-ls-rs": { "rules": { "terraform_fmt": "off" } }
        }))
        .await;
    client.settle(200).await;
    assert!(
        !any_message_contains(&client.last_diagnostics(uri).await, "is not formatted"),
        "terraform_fmt: off must suppress it"
    );

    client.shutdown().await;
}

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
    assert_eq!(
        dup.get("severity").and_then(|s| s.as_i64()),
        Some(4),
        "expected HINT severity"
    );

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

/// A value-only edit to one file must NOT trigger a recompute of open
/// peers — its cross-file state (definitions, references, terraform
/// blocks) is unchanged. Conversely, adding a declaration MUST.
#[tokio::test]
async fn value_only_edit_skips_peer_recompute() {
    let mut client = TestClient::new();
    client.initialize(None).await;

    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";
    client
        .did_open(vars_uri, "variable \"foo\" { default = \"a\" }\n")
        .await;
    client.settle(100).await;
    client
        .did_open(main_uri, "output \"x\" { value = var.foo }\n")
        .await;
    client.settle(200).await;

    let before = client.publish_count(main_uri).await;

    // Edit only the default VALUE in variables.tf — `foo` is still
    // defined, no refs/terraform-blocks change. Peers must be untouched.
    client
        .did_change_full(vars_uri, 2, "variable \"foo\" { default = \"b\" }\n")
        .await;
    client.settle(200).await;
    assert_eq!(
        client.publish_count(main_uri).await,
        before,
        "value-only edit should not republish the peer main.tf"
    );

    // Now ADD a new variable — cross-file state changes, peer recomputes.
    client
        .did_change_full(
            vars_uri,
            3,
            "variable \"foo\" { default = \"b\" }\nvariable \"bar\" {}\n",
        )
        .await;
    client.settle(200).await;
    assert!(
        client.publish_count(main_uri).await > before,
        "adding a declaration should republish the peer"
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
async fn watched_file_delete_ignored_for_open_buffer() {
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

    // A watched-file DELETE arrives for variables.tf WHILE IT IS OPEN in the
    // editor (e.g. a `git checkout` of a branch lacking the file, or a
    // transient save/rename). The open buffer is authoritative — the editor
    // still has it — so the server must NOT drop it; the editor's own
    // did_close is the signal to remove an open doc. (Dropping it here used
    // to desync: the editor kept editing a doc the server had forgotten.)
    client
        .did_change_watched_files(&[(vars_uri, 3 /* Deleted */)])
        .await;
    client.settle(250).await;

    // `foo` is still declared by the open buffer → main.tf stays clean.
    let main_last = client
        .publishes_for(main_uri)
        .await
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        !contains_undefined_var(&main_last, "foo"),
        "deleting an OPEN file on disk must not drop the buffer; main.tf \
         must still resolve `var.foo`, got {main_last:?}"
    );

    // The closed-file delete path (remove + clear + refresh peers) is gated
    // on `!is_open` and remains for files that are genuinely not open.
    client.shutdown().await;
}
