//! Wire-level regression tests for cross-file diagnostic refresh.
//!
//! These pin the server-to-client `textDocument/publishDiagnostics`
//! stream — the protocol surface that real clients (Neovim, VS Code,
//! Helix, Trouble) actually consume. Server-side state correctness is
//! covered by `diagnostics.rs`; this file covers the wire half so a
//! regression that breaks notification delivery (a la "we recompute
//! correctly but never push to peers") is caught in CI.
//!
//! Driven through the shared [`support::TestClient`] mock client, which
//! keeps the loopback socket and captures every server→client message.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use serde_json::json;
use support::{any_message_contains, contains_undefined_var, TestClient};

#[tokio::test]
async fn peer_file_undefined_variable_clears_after_declaration_added() {
    // The user's reported bug, captured at the wire level: edit
    // variables.tf to add `variable "foo" {}`. main.tf's wire-level
    // publishDiagnostics for `var.foo` must drop on the next push.

    let mut client = TestClient::new();
    client.initialize(None).await;

    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";

    // Open main.tf with an undefined `var.foo` reference.
    client
        .did_open(main_uri, "output \"x\" { value = var.foo }\n")
        .await;
    // Open variables.tf empty (no `foo` declaration yet).
    client.did_open(vars_uri, "").await;

    // Yield so async publishes flush before asserting the baseline.
    client.settle(150).await;

    let baseline = client.last_diagnostics(main_uri).await;
    assert!(
        contains_undefined_var(&baseline, "foo"),
        "baseline: expected undefined-var on main.tf for `foo`, got {baseline:?}"
    );

    // The fix: user types the declaration into variables.tf.
    client
        .did_change_full(vars_uri, 2, "variable \"foo\" {}\n")
        .await;
    client.settle(250).await;

    let pushes = client.publishes_for(main_uri).await;
    let final_diags = pushes.last().cloned().unwrap_or_default();
    assert!(
        !contains_undefined_var(&final_diags, "foo"),
        "after peer-file fix: undefined-var on main.tf must clear; \
         all pushes for main.tf were: {pushes:?}"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn server_does_not_advertise_pull_diagnostics() {
    // Pinning the capability decision: nvim 0.11+ keeps push and pull
    // in SEPARATE namespaces and renders the union. Advertising
    // `diagnosticProvider` made nvim auto-pull on every didOpen,
    // populating the pull namespace with a one-shot snapshot that
    // never refreshed when a peer file was edited — push pushed
    // fresh data but the union still showed the stale pull entries.
    // Push is now the only channel; this test fails loudly if a
    // future commit re-advertises pull without rethinking the
    // dual-namespace problem.

    let mut client = TestClient::new();
    let body = client
        .request(
            "initialize",
            json!({ "processId": null, "rootUri": null, "capabilities": {} }),
        )
        .await;

    let caps = &body["result"]["capabilities"];
    assert!(
        caps.get("diagnosticProvider").is_none() || caps["diagnosticProvider"].is_null(),
        "`diagnosticProvider` must be absent — nvim's dual-namespace \
         render bug is the reason. caps were: {caps}"
    );

    client.shutdown().await;
}

/// User-reported scenario: root module declares `variable "envtype" {}`
/// (no type, no default); env-split tfvars in `params/{nonprod,prod}/`
/// each assign `envtype = "..."`. After `didOpen`, a `codeAction`
/// request at the variable's diagnostic range must return the
/// `Set variable type to \`string\`` quick-fix.
///
/// This reproduces the wire behaviour the user couldn't get out of
/// nvim. A passing test means the server is correct end-to-end —
/// any "no action shown in editor" is then client-side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_action_envtype_inference_via_wire() {
    use std::fs;

    let workspace = std::env::temp_dir().join(format!(
        "tfls-wire-envtype-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(workspace.join("params/nonprod")).unwrap();
    fs::create_dir_all(workspace.join("params/prod")).unwrap();

    let main_tf = workspace.join("variables.tf");
    let main_text = "variable \"envtype\" {}\noutput \"e\" { value = var.envtype }\n";
    fs::write(&main_tf, main_text).unwrap();
    fs::write(
        workspace.join("params/nonprod/params.tfvars"),
        "envtype = \"nonprod\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("params/prod/params.tfvars"),
        "envtype = \"prod\"\n",
    )
    .unwrap();

    let workspace_uri = url::Url::from_file_path(&workspace).unwrap().to_string();
    let main_uri = url::Url::from_file_path(&main_tf).unwrap().to_string();

    let mut client = TestClient::new();
    client.initialize(Some(&workspace_uri)).await;
    client.did_open(&main_uri, main_text).await;

    // ScanDirectory + rebuild fire on the worker and populate
    // `assigned_variable_types` asynchronously. Poll the code action until the
    // quick-fix shows up rather than racing a single fixed sleep — under
    // parallel test load the worker can take longer than any one timeout.
    // Poll until the SPECIFIC expected action is ready, not just any action:
    // under parallel load the worker populates incrementally, so an early
    // poll can return a placeholder action (e.g. `type = any`) before the
    // tfvars/module-caller inference finishes. Wait for the `string` variant
    // and match it by title at any index. Generous budget (exits early on
    // success); only the ceiling matters on a genuine hang.
    let wanted = |a: &serde_json::Value| {
        let t = a["title"].as_str().unwrap_or("");
        t.contains("Set variable type to `string`") && t.contains("tfvars / module callers")
    };
    let mut resp = json!(null);
    let mut found = false;
    for _ in 0..120 {
        client.settle(250).await;
        resp = client
            .code_action(
                &main_uri,
                json!({
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 22 }
                }),
                json!([{
                    "range": {
                        "start": { "line": 0, "character": 9 },
                        "end": { "line": 0, "character": 17 }
                    },
                    "severity": 2,
                    "source": "terraform-ls-rs",
                    "message": "`envtype` variable has no type"
                }]),
            )
            .await;
        let actions = resp["result"].as_array().cloned().unwrap_or_default();
        if actions.iter().any(wanted) {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "expected a `Set variable type to `string` … tfvars / module callers` action; got {resp}",
    );

    client.shutdown().await;
    fs::remove_dir_all(&workspace).ok();
}

// ── Repro: undefined-local diagnostic must refresh after the definition
// is completed via did_change (the "typed the name, never rechecked" bug).
#[tokio::test]
async fn undefined_local_clears_when_def_completed_same_file() {
    let mut client = TestClient::new();
    client.initialize(None).await;
    let u = "file:///mod/main.tf";

    // Mid-edit: the definition name is still partial (`region_short_na`),
    // while the reference is already complete (`region_short_name`).
    client
        .did_open(
            u,
            "locals {\n  region_short_na = \"x\"\n}\noutput \"o\" { value = local.region_short_name }\n",
        )
        .await;
    client.settle(150).await;
    let baseline = client.last_diagnostics(u).await;
    assert!(
        any_message_contains(&baseline, "region_short_name"),
        "baseline: mid-edit state flags the unresolved reference; got {baseline:?}"
    );

    // Finish typing the definition name → now it's defined.
    client
        .did_change_full(
            u,
            2,
            "locals {\n  region_short_name = \"x\"\n}\noutput \"o\" { value = local.region_short_name }\n",
        )
        .await;
    client.settle(250).await;
    let pushes = client.publishes_for(u).await;
    let final_diags = pushes.last().cloned().unwrap_or_default();
    assert!(
        !any_message_contains(&final_diags, "region_short_name"),
        "undefined-local must clear after the definition is completed; final: {final_diags:?}"
    );
}

// ── Repro: peer (variable) file passes through a syntax-error state while
// being typed; the consumer's undefined-var must still clear once the
// variable is valid.
#[tokio::test]
async fn consumer_clears_after_peer_var_typed_through_syntax_error() {
    let mut client = TestClient::new();
    client.initialize(None).await;
    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";

    client
        .did_open(
            main_uri,
            "output \"o\" { value = var.recovery_services_vault_keyvault.x }\n",
        )
        .await;
    client.did_open(vars_uri, "").await;
    client.settle(150).await;
    let baseline = client.last_diagnostics(main_uri).await;
    assert!(
        any_message_contains(&baseline, "recovery_services_vault_keyvault"),
        "baseline: undefined var on main; got {baseline:?}"
    );

    // Type the variable, passing through an incomplete (syntax-error) state.
    client
        .did_change_full(
            vars_uri,
            2,
            "variable \"recovery_services_vault_keyvault\" {\n  type = object({\n",
        )
        .await;
    client.settle(120).await;
    // Finish it: valid object-typed variable.
    client
        .did_change_full(
            vars_uri,
            3,
            "variable \"recovery_services_vault_keyvault\" {\n  type = object({ x = bool })\n}\n",
        )
        .await;
    client.settle(250).await;

    let pushes = client.publishes_for(main_uri).await;
    let final_diags = pushes.last().cloned().unwrap_or_default();
    assert!(
        !any_message_contains(&final_diags, "recovery_services_vault_keyvault"),
        "consumer must clear after peer var is valid; pushes for main: {pushes:?}"
    );
}

#[tokio::test]
async fn server_advertises_full_text_sync() {
    // FULL (=1), not INCREMENTAL (=2). Incremental sync was the root of a
    // class of rope desync / freeze "stuck" bugs under concurrent handlers +
    // lspmux; FULL keeps the server's rope exactly the editor's buffer. This
    // fails loudly if a future change reverts to incremental without
    // rethinking that.
    let mut client = TestClient::new();
    let body = client
        .request(
            "initialize",
            json!({ "processId": null, "rootUri": null, "capabilities": {} }),
        )
        .await;
    let sync = &body["result"]["capabilities"]["textDocumentSync"];
    // Either a bare kind `1` or `{ "change": 1, ... }`.
    let kind = sync.as_i64().or_else(|| sync["change"].as_i64());
    assert_eq!(
        kind,
        Some(1),
        "textDocumentSync must be FULL (1); got {sync}"
    );
    client.shutdown().await;
}

#[tokio::test]
async fn consumer_clears_when_definition_file_is_opened() {
    // Open a consumer referencing var.foo BEFORE the file defining it is
    // opened. Opening variables.tf must refresh the already-open consumer.
    let mut client = TestClient::new();
    client.initialize(None).await;
    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";

    client
        .did_open(main_uri, "output \"o\" { value = var.foo }\n")
        .await;
    client.settle(150).await;
    let baseline = client.last_diagnostics(main_uri).await;
    assert!(
        contains_undefined_var(&baseline, "foo"),
        "baseline: undefined foo; got {baseline:?}"
    );

    // Now open the file that defines `foo` (no edit to main.tf).
    client.did_open(vars_uri, "variable \"foo\" {}\n").await;
    client.settle(250).await;

    let pushes = client.publishes_for(main_uri).await;
    let final_diags = pushes.last().cloned().unwrap_or_default();
    assert!(
        !contains_undefined_var(&final_diags, "foo"),
        "opening the defining file must clear the consumer's undefined-var; pushes: {pushes:?}"
    );
}

#[tokio::test]
async fn removing_a_definition_via_edit_reflags_consumer() {
    let mut c = TestClient::new();
    c.initialize(None).await;
    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";
    c.did_open(main_uri, "output \"o\" { value = var.foo }\n")
        .await;
    c.did_open(vars_uri, "variable \"foo\" {}\n").await;
    c.settle(200).await;
    assert!(!contains_undefined_var(
        &c.last_diagnostics(main_uri).await,
        "foo"
    ));
    c.did_change_full(vars_uri, 2, "\n").await;
    c.settle(250).await;
    let last = c
        .publishes_for(main_uri)
        .await
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        contains_undefined_var(&last, "foo"),
        "removing the var must re-flag the consumer; got {last:?}"
    );
}

#[tokio::test]
async fn renaming_a_definition_reflags_old_name() {
    let mut c = TestClient::new();
    c.initialize(None).await;
    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";
    c.did_open(main_uri, "output \"o\" { value = var.foo }\n")
        .await;
    c.did_open(vars_uri, "variable \"foo\" {}\n").await;
    c.settle(200).await;
    assert!(!contains_undefined_var(
        &c.last_diagnostics(main_uri).await,
        "foo"
    ));
    c.did_change_full(vars_uri, 2, "variable \"bar\" {}\n")
        .await;
    c.settle(250).await;
    let last = c
        .publishes_for(main_uri)
        .await
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        contains_undefined_var(&last, "foo"),
        "rename must re-flag the old name; got {last:?}"
    );
}

#[tokio::test]
async fn cross_file_local_resolves_when_defining_file_opened() {
    let mut c = TestClient::new();
    c.initialize(None).await;
    let main_uri = "file:///mod/main.tf";
    let loc_uri = "file:///mod/locals.tf";
    c.did_open(main_uri, "output \"o\" { value = local.bar }\n")
        .await;
    c.settle(150).await;
    assert!(any_message_contains(
        &c.last_diagnostics(main_uri).await,
        "bar"
    ));
    c.did_open(loc_uri, "locals {\n  bar = 1\n}\n").await;
    c.settle(250).await;
    let last = c
        .publishes_for(main_uri)
        .await
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        !any_message_contains(&last, "bar"),
        "opening the locals file must clear the consumer; got {last:?}"
    );
}

#[tokio::test]
async fn cross_module_reference_stays_undefined() {
    // Different directories = different modules; no false resolution.
    let mut c = TestClient::new();
    c.initialize(None).await;
    let b_main = "file:///modB/main.tf";
    c.did_open(b_main, "output \"o\" { value = var.foo }\n")
        .await;
    c.did_open("file:///modA/variables.tf", "variable \"foo\" {}\n")
        .await;
    c.settle(250).await;
    let last = c
        .publishes_for(b_main)
        .await
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        contains_undefined_var(&last, "foo"),
        "cross-module ref must stay undefined; got {last:?}"
    );
}

#[tokio::test]
async fn broken_definition_file_still_resolves_consumer() {
    let mut c = TestClient::new();
    c.initialize(None).await;
    let main_uri = "file:///mod/main.tf";
    let vars_uri = "file:///mod/variables.tf";
    c.did_open(main_uri, "output \"o\" { value = var.foo }\n")
        .await;
    c.settle(120).await;
    c.did_open(
        vars_uri,
        "variable \"foo\" {}\nresource \"x\" \"y\" {\n  bad = @@@\n}\n",
    )
    .await;
    c.settle(250).await;
    let last = c
        .publishes_for(main_uri)
        .await
        .last()
        .cloned()
        .unwrap_or_default();
    assert!(
        !contains_undefined_var(&last, "foo"),
        "broken def file must still index the var; got {last:?}"
    );
}
