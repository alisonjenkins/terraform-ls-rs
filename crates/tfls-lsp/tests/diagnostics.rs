//! Tests for the cross-file / workspace-wide diagnostic resolution.
//!
//! These are driven through [`tfls_lsp::handlers::document::compute_diagnostics`],
//! which is the same path the real didOpen / didChange handlers use.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_lsp::handlers::document::compute_diagnostics;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::Url;

fn uri(path: &str) -> Url {
    Url::parse(path).expect("valid url")
}

fn backend() -> Backend {
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    )
}

fn insert(backend: &Backend, uri: &Url, src: &str) {
    backend
        .state
        .upsert_document(DocumentState::new(uri.clone(), src, 1));
}

fn messages(backend: &Backend, uri: &Url) -> Vec<String> {
    compute_diagnostics(&backend.state, uri)
        .into_iter()
        .map(|d| d.message)
        .collect()
}

#[test]
fn module_reference_resolves_across_files_in_same_directory() {
    // `k3s.tf` defines the module; `ses.tf` references it. Same directory
    // (`/project/`), so the reference must resolve.
    let b = backend();
    let def_uri = uri("file:///project/k3s.tf");
    let ref_uri = uri("file:///project/ses.tf");

    insert(
        &b,
        &def_uri,
        r#"module "k3s_cluster" { source = "./modules/k3s-cluster" }
"#,
    );
    insert(
        &b,
        &ref_uri,
        r#"output "x" { value = module.k3s_cluster.id }
"#,
    );

    let msgs = messages(&b, &ref_uri);
    assert!(
        msgs.iter().all(|m| !m.contains("undefined module")),
        "unexpected module diagnostic: {msgs:?}"
    );
}

#[test]
fn variable_reference_resolves_across_files_in_same_directory() {
    let b = backend();
    let vars_uri = uri("file:///project/variables.tf");
    let use_uri = uri("file:///project/main.tf");

    insert(&b, &vars_uri, r#"variable "region" {}
"#);
    insert(
        &b,
        &use_uri,
        r#"output "x" { value = var.region }
"#,
    );

    let msgs = messages(&b, &use_uri);
    assert!(
        msgs.iter().all(|m| !m.contains("undefined variable")),
        "unexpected variable diagnostic: {msgs:?}"
    );
}

#[test]
fn module_reference_is_false_positive_until_sibling_is_indexed() {
    // Regression: on `did_open`, only the current buffer is in the
    // store — the peer `.tf` that declares `module "k3s_cluster"`
    // hasn't been parsed yet, so the synchronous diagnostic pass
    // sees an empty peer set and emits
    // "undefined module `k3s_cluster`". Once the peer is upserted
    // (by the background indexer), the check must flip to "no
    // diagnostic", and the server nudges the client to re-pull via
    // `workspace/diagnostic/refresh`. This test pins the store-level
    // half of that handshake; the wire call is verified separately
    // by interactive smoke.
    let b = backend();
    let ref_uri = uri("file:///project/cloudflare.tf");
    let def_uri = uri("file:///project/k3s_cluster.tf");

    // Phase 1: only the referencing file is in the store. Expect the
    // false-positive "undefined module" — this is the state the user
    // sees when opening `cloudflare.tf` before the bulk scan runs.
    insert(
        &b,
        &ref_uri,
        r#"output "api" { value = module.k3s_cluster.master_eip }
"#,
    );
    let before = messages(&b, &ref_uri);
    assert!(
        before.iter().any(|m| m.contains("undefined module")
            && m.contains("k3s_cluster")),
        "expected false-positive diagnostic pre-indexing: {before:?}"
    );

    // Phase 2: the peer file is now in the store (simulating what
    // the background scan does). The false-positive must clear on
    // re-run — the check is cross-file aware, so once
    // `state.definitions_by_name` contains the module key, the
    // filter in `is_defined_in_module` passes.
    insert(
        &b,
        &def_uri,
        r#"module "k3s_cluster" { source = "./modules/k3s-cluster" }
"#,
    );
    let after = messages(&b, &ref_uri);
    assert!(
        after.iter().all(|m| !(m.contains("undefined module")
            && m.contains("k3s_cluster"))),
        "diagnostic should clear once peer indexed: {after:?}"
    );
}

#[test]
fn submodule_definitions_do_not_satisfy_parent_references() {
    // Variable is defined inside a nested module directory. The root-level
    // reference must STILL warn — sub-module definitions aren't in scope.
    let b = backend();
    let submodule_vars = uri("file:///project/modules/k/variables.tf");
    let root_use = uri("file:///project/main.tf");

    insert(&b, &submodule_vars, r#"variable "inner" {}
"#);
    insert(
        &b,
        &root_use,
        r#"output "x" { value = var.inner }
"#,
    );

    let msgs = messages(&b, &root_use);
    assert!(
        msgs.iter().any(|m| m.contains("undefined variable `inner`")),
        "expected submodule variable to still warn at root scope: {msgs:?}"
    );
}

#[test]
fn undefined_reference_still_warns_when_no_other_file_defines_it() {
    let b = backend();
    let u = uri("file:///project/only.tf");
    insert(
        &b,
        &u,
        r#"output "x" { value = var.typo }
"#,
    );

    let msgs = messages(&b, &u);
    assert!(
        msgs.iter().any(|m| m.contains("undefined variable `typo`")),
        "expected typo to still warn: {msgs:?}"
    );
}

#[test]
fn definitions_in_unrelated_workspace_dir_do_not_satisfy_references() {
    // Two unrelated root modules in the same workspace. A variable defined
    // in `/projectA/` must not cover a reference in `/projectB/`.
    let b = backend();
    let a_vars = uri("file:///projectA/variables.tf");
    let b_ref = uri("file:///projectB/main.tf");

    insert(&b, &a_vars, r#"variable "shared" {}
"#);
    insert(
        &b,
        &b_ref,
        r#"output "x" { value = var.shared }
"#,
    );

    let msgs = messages(&b, &b_ref);
    assert!(
        msgs.iter().any(|m| m.contains("undefined variable `shared`")),
        "cross-workspace reference should still warn: {msgs:?}"
    );
}

#[test]
fn parses_tf_json_file_via_document_lifecycle() {
    // Open a `.tf.json` document and confirm the equivalent HCL-level
    // diagnostics fire. Specifically: a `variable` declared without a
    // `type` should trigger `terraform_typed_variables`.
    let b = backend();
    let u = uri("file:///proj/input.tf.json");
    insert(
        &b,
        &u,
        r#"{
            "variable": {
                "region": {}
            }
        }"#,
    );
    let msgs = messages(&b, &u);
    assert!(
        msgs.iter().any(|m| m.contains("`region`") && m.contains("no type")),
        "expected `region` missing-type warning via tf.json; got {msgs:?}"
    );
}

#[test]
fn flags_malformed_tf_json() {
    let b = backend();
    let u = uri("file:///proj/broken.tf.json");
    insert(&b, &u, "{not valid json}");
    let msgs = messages(&b, &u);
    assert!(
        msgs.iter().any(|m| m.contains("JSON") || m.contains("json")),
        "expected JSON parse error: {msgs:?}"
    );
}

#[test]
fn flags_unknown_top_level_key_in_tf_json() {
    let b = backend();
    let u = uri("file:///proj/weird.tf.json");
    insert(&b, &u, r#"{ "unknown_root": {} }"#);
    let msgs = messages(&b, &u);
    assert!(
        msgs.iter().any(|m| m.contains("unknown") && m.contains("unknown_root")),
        "expected unknown-top-level-key error: {msgs:?}"
    );
}
