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

// --- Variable-scope test matrix -----------------------------------
//
// These tests pin the end-to-end `compute_diagnostics` behaviour for
// `var.X` references across the scoping edge cases that production
// modules actually hit. Regressions in reference extraction,
// `is_defined_in_module`, or path comparison surface here.
//
// Each test uses `multi_file_diags` to drop several `.tf` files into
// the store and return the diagnostics for one of them. The helper
// keeps the assertion shape small so new cases are cheap to add.

fn multi_file_diags(files: &[(&Url, &str)], target: &Url) -> Vec<String> {
    let b = backend();
    for (u, src) in files {
        insert(&b, u, src);
    }
    messages(&b, target)
}

fn expect_no_undefined_variable(msgs: &[String], name: &str, context: &str) {
    assert!(
        msgs.iter()
            .all(|m| !(m.contains("undefined variable") && m.contains(name))),
        "{context}: `{name}` flagged as undefined; diagnostics were: {msgs:?}"
    );
}

fn expect_undefined_variable(msgs: &[String], name: &str, context: &str) {
    assert!(
        msgs.iter()
            .any(|m| m.contains("undefined variable") && m.contains(name)),
        "{context}: `{name}` should be undefined but wasn't; diagnostics were: {msgs:?}"
    );
}

#[test]
fn scope_var_ref_inside_module_block_same_dir_decl_resolves() {
    // Case 2 from the matrix: reference inside a module block's
    // body, declaration in a peer file. Bug the user hit.
    let vars = uri("file:///stack/variables.tf");
    let call = uri("file:///stack/k3s_cluster.tf");
    let msgs = multi_file_diags(
        &[
            (&vars, "variable \"account_number\" {}\n"),
            (
                &call,
                "module \"k3s_cluster\" {\n  source         = \"./modules/k3s\"\n  account_number = var.account_number\n}\n",
            ),
        ],
        &call,
    );
    expect_no_undefined_variable(&msgs, "account_number", "module-block ref + peer decl");
}

#[test]
fn scope_var_ref_inside_module_block_child_only_decl_still_warns() {
    // Case 3: the declaration exists only in a CHILD module,
    // not in the caller's scope. The `var.X` at the caller
    // resolves to the caller's scope — child-module vars are
    // invisible from there — so this must still warn.
    let call = uri("file:///stack/k3s_cluster.tf");
    let child_vars = uri("file:///stack/modules/k3s/variables.tf");
    let msgs = multi_file_diags(
        &[
            (&child_vars, "variable \"account_number\" {}\n"),
            (
                &call,
                "module \"k3s_cluster\" {\n  source         = \"./modules/k3s\"\n  account_number = var.account_number\n}\n",
            ),
        ],
        &call,
    );
    expect_undefined_variable(&msgs, "account_number", "child-only decl, caller ref");
}

#[test]
fn scope_var_shadowing_root_and_child_declaration_resolves_at_root() {
    // Case 4: both the stack root AND a child module declare
    // the same variable name. A reference at the root resolves
    // to the root's declaration — the child's copy is not in
    // scope for the root body.
    let root_vars = uri("file:///stack/variables.tf");
    let call = uri("file:///stack/k3s_cluster.tf");
    let child_vars = uri("file:///stack/modules/k3s/variables.tf");
    let msgs = multi_file_diags(
        &[
            (&root_vars, "variable \"account_number\" {}\n"),
            (&child_vars, "variable \"account_number\" {}\n"),
            (
                &call,
                "module \"k3s_cluster\" {\n  source         = \"./modules/k3s\"\n  account_number = var.account_number\n}\n",
            ),
        ],
        &call,
    );
    expect_no_undefined_variable(&msgs, "account_number", "root+child shadow");
}

#[test]
fn scope_var_shadowing_across_unrelated_stacks_resolves_in_each() {
    // Case 5: `stackA` and `stackB` each declare their own
    // `variable "region" {}`; references in each stack must
    // resolve to their own declaration without flagging.
    let a_vars = uri("file:///stackA/variables.tf");
    let a_main = uri("file:///stackA/main.tf");
    let b_vars = uri("file:///stackB/variables.tf");
    let b_main = uri("file:///stackB/main.tf");
    let files: &[(&Url, &str)] = &[
        (&a_vars, "variable \"region\" {}\n"),
        (&a_main, "output \"r\" { value = var.region }\n"),
        (&b_vars, "variable \"region\" {}\n"),
        (&b_main, "output \"r\" { value = var.region }\n"),
    ];
    expect_no_undefined_variable(
        &multi_file_diags(files, &a_main),
        "region",
        "stackA ref",
    );
    expect_no_undefined_variable(
        &multi_file_diags(files, &b_main),
        "region",
        "stackB ref",
    );
}

#[test]
fn scope_var_ref_inside_dynamic_content_resolves_against_caller_scope() {
    // Case 6: references inside `dynamic "X" { content { foo =
    // var.Y } }` must still resolve against the caller's own
    // module scope — the `dynamic` / `content` wrapper doesn't
    // change variable visibility.
    let vars = uri("file:///stack/variables.tf");
    let main = uri("file:///stack/main.tf");
    let msgs = multi_file_diags(
        &[
            (&vars, "variable \"tags\" {}\n"),
            (
                &main,
                "resource \"aws_instance\" \"x\" {\n  dynamic \"tag\" {\n    for_each = var.tags\n    content {\n      key = var.tags\n    }\n  }\n}\n",
            ),
        ],
        &main,
    );
    expect_no_undefined_variable(&msgs, "tags", "dynamic/content ref");
}

#[test]
fn scope_var_decl_unused_check_survives_peer_parse_error() {
    // A single typo in `iam_roles.tf` (unclosed brace, for
    // example) makes `hcl-edit` bail on that file's body.
    // Without a fallback reference extractor, every reference
    // inside that file disappears from `references_by_name`,
    // and the "declared but not used" rule fires for every
    // variable the broken file was using. Reproduce:
    let vars = uri("file:///stack/variables.tf");
    let refs = uri("file:///stack/iam_roles.tf");
    let msgs = multi_file_diags(
        &[
            (&vars, "variable \"admin_users\" {}\n"),
            (
                &refs,
                // Deliberate parse error further down (unclosed
                // brace) — the reference earlier in the file
                // should still be counted as a use.
                "resource \"aws_iam_role\" \"r\" {\n  identifiers = var.admin_users\n}\n\nresource \"aws_other\" \"x\" {\n  broken = {\n",
            ),
        ],
        &vars,
    );
    assert!(
        msgs.iter().all(|m| !(m.contains("declared but not used")
            && m.contains("admin_users"))),
        "parse error in peer file must not hide references: {msgs:?}"
    );
}

#[test]
fn scope_var_decl_is_not_unused_when_ref_lives_in_peer_file() {
    // Regression: `variable "admin_users"` in `variables.tf` was
    // flagged "declared but not used" even though
    // `iam_roles.tf` (same directory) referenced
    // `var.admin_users` in multiple places. The unused-decl
    // check must consult references across every peer file in
    // the same module, not just the declaring document.
    let vars = uri("file:///stack/variables.tf");
    let refs = uri("file:///stack/iam_roles.tf");
    let msgs = multi_file_diags(
        &[
            (&vars, "variable \"admin_users\" {}\n"),
            (
                &refs,
                "resource \"aws_iam_role\" \"r\" {\n  identifiers = var.admin_users\n}\n",
            ),
        ],
        &vars,
    );
    assert!(
        msgs.iter().all(|m| !(m.contains("declared but not used")
            && m.contains("admin_users"))),
        "in-use var flagged as unused: {msgs:?}"
    );
}

#[test]
fn scope_var_ref_inside_locals_block_resolves() {
    // Case 7: references inside `locals { x = var.Y }` must
    // resolve to the module's own variable declarations.
    let vars = uri("file:///stack/variables.tf");
    let main = uri("file:///stack/main.tf");
    let msgs = multi_file_diags(
        &[
            (&vars, "variable \"region\" {}\n"),
            (&main, "locals { r = var.region }\n"),
        ],
        &main,
    );
    expect_no_undefined_variable(&msgs, "region", "locals-block ref");
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
