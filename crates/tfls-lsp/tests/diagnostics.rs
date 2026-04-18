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
