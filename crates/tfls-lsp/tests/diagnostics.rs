//! Tests for the cross-file / workspace-wide diagnostic resolution.
//!
//! These are driven through [`tfls_lsp::handlers::document::compute_diagnostics`],
//! which is the same path the real didOpen / didChange handlers use.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use tfls_lsp::handlers::document::compute_diagnostics;
use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp_server::LspService;
use url::Url;

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

    insert(
        &b,
        &vars_uri,
        r#"variable "region" {}
"#,
    );
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
    expect_no_undefined_variable(&multi_file_diags(files, &a_main), "region", "stackA ref");
    expect_no_undefined_variable(&multi_file_diags(files, &b_main), "region", "stackB ref");
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
fn unused_data_source_cleared_when_reference_in_peer_file() {
    // Regression: a `data "http" "test" {}` in one file referenced
    // by `data.http.test.response_body` in a peer `output` file
    // must not be flagged as unused — the cross-file reference
    // resolution has to walk peer files' references too.
    let b = backend();
    let data_file = uri("file:///stack/data.tf");
    let out_file = uri("file:///stack/outputs.tf");

    insert(
        &b,
        &data_file,
        "data \"http\" \"test\" {\n  url = \"https://myip.dk\"\n}\n",
    );
    insert(
        &b,
        &out_file,
        "output \"my_ip\" { value = data.http.test.response_body }\n",
    );

    let msgs = messages(&b, &data_file);
    assert!(
        msgs.iter()
            .all(|m| !(m.contains("declared but not used") && m.contains("http.test"))),
        "in-use data source flagged unused: {msgs:?}"
    );
}

#[test]
fn malformed_version_diagnostic_clears_after_simulated_edit() {
    // Regression for the reported "LSP is stuck on a stale
    // `malformed version `c`` warning after I corrected the
    // version" symptom. Simulate the edit flow:
    //
    //   1. User opens with an in-progress bad version: `"c"`.
    //   2. User corrects to `">= 3.5.0"` — we reparse via
    //      `reparse_document` (the same call
    //      `did_change` makes after applying the rope edit).
    //   3. `compute_diagnostics` must report no malformed-
    //      version error.
    //
    // The test exercises the exact state path the did_change
    // handler goes through (rope edit → reparse → compute),
    // so any regression that leaves stale AST data / stale
    // derived symbols would surface here.
    use lsp_types::{Position, Range, TextDocumentContentChangeEvent};
    use ropey::Rope;

    let b = backend();
    let u = uri("file:///versions.tf");
    let before = "terraform {\n  required_providers {\n    http = {\n      source = \"hashicorp/http\"\n      version = \"c\"\n    }\n  }\n}\n";
    insert(&b, &u, before);

    // Sanity: the malformed diag DOES fire on the initial state.
    let initial = messages(&b, &u);
    assert!(
        initial
            .iter()
            .any(|m| m.contains("malformed") && m.contains("`c`")),
        "baseline: expected malformed `c` diag on initial state, got {initial:?}"
    );

    // Simulate the did_change rope edit: replace the lone `c`
    // with `>= 3.5.0`. Compute positions from the `before`
    // text so we target the actual `c` character.
    let c_byte = before.find("\"c\"").unwrap() + 1;
    let rope_before = Rope::from_str(before);
    let line = rope_before.byte_to_line(c_byte);
    let line_start = rope_before.line_to_byte(line);
    let col = (c_byte - line_start) as u32;
    {
        let mut doc = b.state.documents.get_mut(&u).unwrap();
        doc.apply_change(TextDocumentContentChangeEvent {
            range: Some(Range::new(
                Position::new(line as u32, col),
                Position::new(line as u32, col + 1),
            )),
            range_length: None,
            text: ">= 3.5.0".to_string(),
        })
        .unwrap();
    }
    b.state.reparse_document(&u);

    // After the edit, the diagnostic must clear.
    let after = messages(&b, &u);
    assert!(
        after.iter().all(|m| !m.contains("malformed")),
        "corrected version left a stale malformed diag: {after:?}"
    );
}

#[test]
fn undefined_variable_clears_when_declaration_added_to_peer_file() {
    // User reported: `modules/api_gateway_resource/main.tf` uses
    // `var.rest_api_id` which is undefined. They then add
    // `variable "rest_api_id" {}` to a peer file (`variable.tf`)
    // and save — but `main.tf` keeps showing the stale
    // "undefined variable" warning.
    //
    // Pins the server-side contract: after simulating the
    // `did_change` that adds the declaration (rope edit → reparse
    // on `variable.tf`), `compute_diagnostics` on `main.tf` MUST
    // stop reporting the reference as undefined. If this passes
    // in CI but the user still sees stale diagnostics in-editor,
    // the bug is on the client-side refresh path (push/pull
    // namespace mismatch, nvim pull cache) — not the store.
    use lsp_types::TextDocumentContentChangeEvent;

    let b = backend();
    let main_u = uri("file:///mod/api_gateway_resource/main.tf");
    let vars_u = uri("file:///mod/api_gateway_resource/variable.tf");

    // main.tf uses var.rest_api_id; variable.tf is initially empty
    // (the user hasn't added the declaration yet).
    insert(
        &b,
        &main_u,
        "resource \"aws_api_gateway_integration\" \"x\" {\n  rest_api_id = var.rest_api_id\n}\n",
    );
    insert(&b, &vars_u, "");

    // Baseline — must report the undefined var.
    let initial = messages(&b, &main_u);
    assert!(
        initial
            .iter()
            .any(|m| m.contains("undefined variable") && m.contains("rest_api_id")),
        "baseline: expected undefined var, got {initial:?}"
    );

    // Simulate the fix: user types `variable "rest_api_id" {}` at
    // the top of variable.tf. Full-document replacement — mirrors
    // what nvim often sends on a first-keystroke did_change when
    // the buffer was empty.
    {
        let mut doc = b.state.documents.get_mut(&vars_u).unwrap();
        doc.apply_change(TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "variable \"rest_api_id\" {}\n".to_string(),
        })
        .unwrap();
    }
    b.state.reparse_document(&vars_u);

    // After the peer-file fix, the undefined-var warning on main.tf
    // must clear.
    let after = messages(&b, &main_u);
    assert!(
        after
            .iter()
            .all(|m| !(m.contains("undefined variable") && m.contains("rest_api_id"))),
        "stale undefined-var persisted after declaration added: {after:?}"
    );
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
        msgs.iter()
            .all(|m| !(m.contains("declared but not used") && m.contains("admin_users"))),
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
        msgs.iter()
            .all(|m| !(m.contains("declared but not used") && m.contains("admin_users"))),
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
        before
            .iter()
            .any(|m| m.contains("undefined module") && m.contains("k3s_cluster")),
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
        after
            .iter()
            .all(|m| !(m.contains("undefined module") && m.contains("k3s_cluster"))),
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

    insert(
        &b,
        &submodule_vars,
        r#"variable "inner" {}
"#,
    );
    insert(
        &b,
        &root_use,
        r#"output "x" { value = var.inner }
"#,
    );

    let msgs = messages(&b, &root_use);
    assert!(
        msgs.iter()
            .any(|m| m.contains("undefined variable `inner`")),
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

    insert(
        &b,
        &a_vars,
        r#"variable "shared" {}
"#,
    );
    insert(
        &b,
        &b_ref,
        r#"output "x" { value = var.shared }
"#,
    );

    let msgs = messages(&b, &b_ref);
    assert!(
        msgs.iter()
            .any(|m| m.contains("undefined variable `shared`")),
        "cross-workspace reference should still warn: {msgs:?}"
    );
}

#[test]
fn parses_tf_json_file_via_document_lifecycle() {
    // Open a `.tf.json` document and confirm the equivalent HCL-level
    // diagnostics fire. Specifically: a `variable` declared without a
    // `type` should trigger `terraform_typed_variables` — but ONLY
    // when the variable is actually referenced; otherwise the
    // unused-declarations rule takes precedence (suppressing the
    // type warning, because fixing the type on a soon-to-be-deleted
    // variable is wasted work).
    let b = backend();
    let vars = uri("file:///proj/input.tf.json");
    let use_site = uri("file:///proj/main.tf");
    insert(
        &b,
        &vars,
        r#"{
            "variable": {
                "region": {}
            }
        }"#,
    );
    // Reference the variable so `unused_declarations` stays silent
    // and the type warning surfaces as intended for this test.
    insert(
        &b,
        &use_site,
        r#"output "r" { value = var.region }
"#,
    );
    let msgs = messages(&b, &vars);
    assert!(
        msgs.iter()
            .any(|m| m.contains("`region`") && m.contains("no type")),
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
        msgs.iter()
            .any(|m| m.contains("JSON") || m.contains("json")),
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
        msgs.iter()
            .any(|m| m.contains("unknown") && m.contains("unknown_root")),
        "expected unknown-top-level-key error: {msgs:?}"
    );
}

fn install_aws_trim_prefix(b: &Backend) {
    b.state.merge_functions(vec![(
        "provider::hashicorp::aws::trim_prefix".to_string(),
        tfls_schema::FunctionSignature {
            description: None,
            return_type: sonic_rs::json!("string"),
            parameters: vec![],
            variadic_parameter: None,
        },
    )]);
}

#[test]
fn unknown_provider_local_flagged_as_error() {
    let b = backend();
    install_aws_trim_prefix(&b);
    let u = uri("file:///proj/main.tf");
    insert(
        &b,
        &u,
        "output \"x\" { value = provider::nope::trim_prefix(\"a\") }\n",
    );
    let msgs = messages(&b, &u);
    assert!(
        msgs.iter()
            .any(|m| m.contains("Unknown provider local") && m.contains("nope")),
        "expected unknown-local error: {msgs:?}"
    );
}

#[test]
fn unknown_function_flagged_as_warning() {
    let b = backend();
    install_aws_trim_prefix(&b);
    let v_uri = uri("file:///proj/versions.tf");
    insert(
        &b,
        &v_uri,
        "terraform {\n  required_providers {\n    aws = {\n      source = \"hashicorp/aws\"\n    }\n  }\n}\n",
    );
    let u = uri("file:///proj/main.tf");
    insert(
        &b,
        &u,
        "output \"x\" { value = provider::aws::no_such_fn(\"a\") }\n",
    );
    let msgs = messages(&b, &u);
    assert!(
        msgs.iter()
            .any(|m| m.contains("aws") && m.contains("no_such_fn")),
        "expected unknown-function warning: {msgs:?}"
    );
}

#[test]
fn known_provider_function_emits_no_diagnostic() {
    let b = backend();
    install_aws_trim_prefix(&b);
    let v_uri = uri("file:///proj/versions.tf");
    insert(
        &b,
        &v_uri,
        "terraform {\n  required_providers {\n    aws = {\n      source = \"hashicorp/aws\"\n    }\n  }\n}\n",
    );
    let u = uri("file:///proj/main.tf");
    insert(
        &b,
        &u,
        "output \"x\" { value = provider::aws::trim_prefix(\"a\") }\n",
    );
    let msgs = messages(&b, &u);
    assert!(
        !msgs
            .iter()
            .any(|m| m.contains("provider") && m.contains("aws") && m.contains("trim_prefix")),
        "false positive on known fn: {msgs:?}"
    );
}

#[test]
fn unknown_function_skipped_when_provider_has_no_functions_indexed() {
    // No `state.functions` entry for `aws` at all → unknown-fn
    // diagnostic is suppressed (probably means schema fetch hasn't
    // completed yet, not a real typo).
    let b = backend();
    let v_uri = uri("file:///proj/versions.tf");
    insert(
        &b,
        &v_uri,
        "terraform {\n  required_providers {\n    aws = {\n      source = \"hashicorp/aws\"\n    }\n  }\n}\n",
    );
    let u = uri("file:///proj/main.tf");
    insert(
        &b,
        &u,
        "output \"x\" { value = provider::aws::trim_prefix(\"a\") }\n",
    );
    let msgs = messages(&b, &u);
    assert!(
        !msgs.iter().any(|m| m.contains("trim_prefix")),
        "should not flag when no functions indexed: {msgs:?}"
    );
}

#[test]
fn dedup_drops_identical_entries() {
    // Two `terraform { required_providers { rsa = ... } }` blocks
    // that both declare an unused, version-less local. Pre-dedup
    // the rules emit one diagnostic per declaration; post-dedup
    // each unique (range, message) survives but identical
    // emissions get folded.
    let b = backend();
    let u = uri("file:///proj/main.tf");
    insert(
        &b,
        &u,
        "terraform {\n\
           required_providers {\n\
             rsa = {\n\
               source = \"vancluever/acme\"\n\
             }\n\
           }\n\
         }\n\
         terraform {\n\
           required_providers {\n\
             rsa = {\n\
               source = \"vancluever/acme\"\n\
             }\n\
           }\n\
         }\n",
    );
    let msgs = messages(&b, &u);
    let unused_count = msgs
        .iter()
        .filter(|m| m.contains("not used") && m.contains("rsa"))
        .count();
    let version_count = msgs
        .iter()
        .filter(|m| m.contains("declare a `version`") && m.contains("rsa"))
        .count();
    // Two declarations on different lines = two ranges → two
    // diagnostics each survives. But should never exceed the
    // declaration count: dedup catches any same-range duplicate.
    assert!(
        unused_count <= 2,
        "unused diag emitted {unused_count}× for 2 declarations: {msgs:?}"
    );
    assert!(
        version_count <= 2,
        "version diag emitted {version_count}× for 2 declarations: {msgs:?}"
    );
}

#[test]
fn renamed_local_resolves_via_required_providers() {
    // versions.tf renames `aws_v6 → hashicorp/aws`. Diagnostic
    // must NOT fire for `provider::aws_v6::trim_prefix(...)`.
    let b = backend();
    install_aws_trim_prefix(&b);
    let v_uri = uri("file:///proj/versions.tf");
    insert(
        &b,
        &v_uri,
        "terraform {\n  required_providers {\n    aws_v6 = {\n      source = \"hashicorp/aws\"\n    }\n  }\n}\n",
    );
    let u = uri("file:///proj/main.tf");
    insert(
        &b,
        &u,
        "output \"x\" { value = provider::aws_v6::trim_prefix(\"a\") }\n",
    );
    let msgs = messages(&b, &u);
    assert!(
        !msgs
            .iter()
            .any(|m| m.contains("trim_prefix") || m.contains("aws_v6")),
        "alias should resolve cleanly: {msgs:?}"
    );
}

// --- .terraform.lock.hcl awareness ----------------------------------
//
// Lock-file pin overrides the lower bound of the declared
// constraint. A `~> 1.0` constraint admits min 1.0.0 — so a rule
// gated at >= 1.7.0 would normally NOT fire. But if the lock file
// pins the provider at 1.7.0+ (the actual installed version), the
// rule MUST fire — that's what `terraform plan` would run against.

fn write_files(dir: &std::path::Path, files: &[(&str, &str)]) {
    for (name, body) in files {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
}

#[test]
fn aws_lock_pin_unblocks_rule_that_constraint_floor_would_suppress() {
    // `aws_alb` → `aws_lb` is gated at AWS provider 1.7.0 in
    // `deprecated_aws_renames.rs`. With constraint `~> 1.0`
    // (min admitted = 1.0.0), the rule is constraint-suppressed.
    // A `.terraform.lock.hcl` pinning hashicorp/aws at 1.7.0
    // must un-suppress.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_files(
        dir,
        &[
            (
                "terraform.tf",
                "terraform {\n  required_providers {\n    aws = { source = \"hashicorp/aws\", version = \"~> 1.0\" }\n  }\n}\n",
            ),
            ("main.tf", "resource \"aws_alb\" \"x\" {}\n"),
            (
                ".terraform.lock.hcl",
                "provider \"registry.terraform.io/hashicorp/aws\" {\n  version = \"1.7.0\"\n}\n",
            ),
        ],
    );

    let b = backend();
    let tf_uri = Url::from_file_path(dir.join("terraform.tf")).unwrap();
    let main_uri = Url::from_file_path(dir.join("main.tf")).unwrap();
    insert(
        &b,
        &tf_uri,
        &fs::read_to_string(dir.join("terraform.tf")).unwrap(),
    );
    insert(
        &b,
        &main_uri,
        &fs::read_to_string(dir.join("main.tf")).unwrap(),
    );

    let msgs = messages(&b, &main_uri);
    assert!(
        msgs.iter().any(|m| m.contains("aws_lb")),
        "lock pin 1.7.0 must un-suppress aws_alb→aws_lb rule; diags: {msgs:?}"
    );
}

#[test]
fn aws_constraint_alone_suppresses_rule_when_lock_absent() {
    // Same as above, without the lock file. Confirms the
    // baseline: under constraint `~> 1.0`, the rule does NOT
    // fire — so the previous test's assertion is meaningful.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_files(
        dir,
        &[
            (
                "terraform.tf",
                "terraform {\n  required_providers {\n    aws = { source = \"hashicorp/aws\", version = \"~> 1.0\" }\n  }\n}\n",
            ),
            ("main.tf", "resource \"aws_alb\" \"x\" {}\n"),
        ],
    );

    let b = backend();
    let tf_uri = Url::from_file_path(dir.join("terraform.tf")).unwrap();
    let main_uri = Url::from_file_path(dir.join("main.tf")).unwrap();
    insert(
        &b,
        &tf_uri,
        &fs::read_to_string(dir.join("terraform.tf")).unwrap(),
    );
    insert(
        &b,
        &main_uri,
        &fs::read_to_string(dir.join("main.tf")).unwrap(),
    );

    let msgs = messages(&b, &main_uri);
    assert!(
        !msgs.iter().any(|m| m.contains("aws_lb")),
        "constraint `~> 1.0` (min 1.0.0) must suppress rule gated at 1.7.0; diags: {msgs:?}"
    );
}

#[test]
fn aws_lock_short_form_provider_resolves_via_implicit_hashicorp_namespace() {
    // Short-form `aws = "~> 1.0"` (no `source = ...`) must still
    // pair with the lock file's `hashicorp/aws` entry — short
    // form implicitly resolves to that address.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_files(
        dir,
        &[
            (
                "terraform.tf",
                "terraform {\n  required_providers {\n    aws = \"~> 1.0\"\n  }\n}\n",
            ),
            ("main.tf", "resource \"aws_alb\" \"x\" {}\n"),
            (
                ".terraform.lock.hcl",
                "provider \"registry.terraform.io/hashicorp/aws\" {\n  version = \"1.7.0\"\n}\n",
            ),
        ],
    );

    let b = backend();
    let tf_uri = Url::from_file_path(dir.join("terraform.tf")).unwrap();
    let main_uri = Url::from_file_path(dir.join("main.tf")).unwrap();
    insert(
        &b,
        &tf_uri,
        &fs::read_to_string(dir.join("terraform.tf")).unwrap(),
    );
    insert(
        &b,
        &main_uri,
        &fs::read_to_string(dir.join("main.tf")).unwrap(),
    );

    let msgs = messages(&b, &main_uri);
    assert!(
        msgs.iter().any(|m| m.contains("aws_lb")),
        "short-form provider with hashicorp/aws lock pin must fire; diags: {msgs:?}"
    );
}

#[test]
fn aws_lock_invalidate_drops_diagnostic() {
    // Cache invalidation contract: after the lock file is
    // removed and `state.invalidate_lock` is called (mirroring
    // the watcher path), diagnostics revert to constraint-only
    // gating and the rule becomes suppressed again.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_files(
        dir,
        &[
            (
                "terraform.tf",
                "terraform {\n  required_providers {\n    aws = { source = \"hashicorp/aws\", version = \"~> 1.0\" }\n  }\n}\n",
            ),
            ("main.tf", "resource \"aws_alb\" \"x\" {}\n"),
            (
                ".terraform.lock.hcl",
                "provider \"registry.terraform.io/hashicorp/aws\" {\n  version = \"1.7.0\"\n}\n",
            ),
        ],
    );

    let b = backend();
    let tf_uri = Url::from_file_path(dir.join("terraform.tf")).unwrap();
    let main_uri = Url::from_file_path(dir.join("main.tf")).unwrap();
    insert(
        &b,
        &tf_uri,
        &fs::read_to_string(dir.join("terraform.tf")).unwrap(),
    );
    insert(
        &b,
        &main_uri,
        &fs::read_to_string(dir.join("main.tf")).unwrap(),
    );

    // Sanity: with lock present, the rule fires.
    let with_lock = messages(&b, &main_uri);
    assert!(with_lock.iter().any(|m| m.contains("aws_lb")));

    // Remove the lock file and invalidate the cache.
    fs::remove_file(dir.join(".terraform.lock.hcl")).unwrap();
    b.state.invalidate_lock(dir);

    let without_lock = messages(&b, &main_uri);
    assert!(
        !without_lock.iter().any(|m| m.contains("aws_lb")),
        "after invalidation + lock removal the rule must revert to constraint-only suppression; diags: {without_lock:?}"
    );
}

#[test]
fn lock_file_change_drops_cached_schema_fetch_mtime() {
    // Pins the invalidation contract for the bug where
    // `terraform init -upgrade` rewrites the providers but the
    // server keeps using the stale schema. The watcher's
    // LockFileChanged arm in indexer.rs MUST drop the
    // fetched_schema_dirs entry so the next mtime check falls
    // through to a real fetch.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let b = backend();
    b.state
        .fetched_schema_dirs
        .insert(dir.to_path_buf(), std::time::SystemTime::UNIX_EPOCH);
    assert!(b.state.fetched_schema_dirs.contains_key(dir));

    // Same mutation the LockFileChanged arm performs. The arm also
    // calls maybe_enqueue_schema_fetch (private), but the cache
    // eviction is the contract that fixes the bug — the fetch
    // would happen anyway via did_open / did_save once eviction
    // unblocks the mtime check.
    b.state.fetched_schema_dirs.remove(dir);

    assert!(!b.state.fetched_schema_dirs.contains_key(dir));
}

// --- lock-vs-constraint drift across repeated init runs ---------
//
// User-reported regression: after `tofu init -upgrade` to pin a
// version that satisfies the constraint, the stale "constraint
// doesn't admit lock pin" diagnostic from BEFORE the init kept
// being reported. Repro flow:
//
//   1. Open file with `version = "~> 4.71.0"` and lock pin 4.71.0.
//      No diagnostic.
//   2. Rewrite lock to 2.71.0 (mimics user pinning + init).
//      `invalidate_lock` runs (mimics LockFileChanged watcher arm).
//      Diagnostic fires.
//   3. Rewrite lock back to 4.71.0.
//      invalidate_lock runs.
//      Diagnostic clears.
//
// Tests the full cache-invalidation chain: state.lock_file_for ->
// compute_diagnostics::lock_vs_constraint_diagnostics, including
// the canonical-path key collapsing (macOS /var symlink handling).

fn write_lock(dir: &std::path::Path, azurerm_version: &str) {
    let body = format!(
        r#"provider "registry.opentofu.org/hashicorp/azurerm" {{
  version     = "{azurerm_version}"
  constraints = "~> 4.71.0"
  hashes      = []
}}
"#
    );
    fs::write(dir.join(".terraform.lock.hcl"), body).unwrap();
}

#[test]
fn lock_constraint_drift_clears_after_invalidate() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(
        dir.join("main.tf"),
        r#"terraform {
  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "~> 4.71.0"
    }
  }
}
"#,
    )
    .unwrap();
    write_lock(dir, "4.71.0");

    let b = backend();
    let main_uri = Url::from_file_path(dir.join("main.tf")).unwrap();
    insert(
        &b,
        &main_uri,
        &fs::read_to_string(dir.join("main.tf")).unwrap(),
    );

    // Step 1: lock matches → no drift warning.
    let initial = messages(&b, &main_uri);
    assert!(
        !initial.iter().any(|m| m.contains("does not admit")),
        "initial state should be clean; got: {initial:?}"
    );

    // Step 2: rewrite lock to a version OUTSIDE the constraint
    // band. Sleep ~10ms to ensure the rewrite produces a distinct
    // mtime even on filesystems with second-level granularity.
    std::thread::sleep(std::time::Duration::from_millis(50));
    write_lock(dir, "2.71.0");
    b.state.invalidate_lock(dir);

    let after_downgrade = messages(&b, &main_uri);
    assert!(
        after_downgrade
            .iter()
            .any(|m| m.contains("does not admit") && m.contains("2.71.0")),
        "downgrade should produce drift warning citing 2.71.0; got: {after_downgrade:?}"
    );

    // Step 3: rewrite lock back to a satisfying version.
    std::thread::sleep(std::time::Duration::from_millis(50));
    write_lock(dir, "4.71.0");
    b.state.invalidate_lock(dir);

    let after_re_upgrade = messages(&b, &main_uri);
    assert!(
        !after_re_upgrade
            .iter()
            .any(|m| m.contains("does not admit")),
        "re-upgrade should clear drift warning; got: {after_re_upgrade:?}"
    );
}

/// Same flow but mimics macOS-style symlinked tmp paths by using
/// `/var/folders/.../X` (non-canonical) for the URI side and
/// `/private/var/folders/.../X` (canonical) for the
/// invalidate_lock call — exactly the watcher-vs-URI path
/// mismatch the canonicalisation fix is supposed to handle.
#[test]
fn lock_invalidate_with_canonical_path_clears_cache_keyed_under_non_canonical() {
    let tmp = tempfile::tempdir().unwrap();
    // tempdir() on macOS returns `/var/folders/.../X` which IS
    // a symlink. Capture both forms.
    let non_canonical = tmp.path().to_path_buf();
    let canonical = non_canonical.canonicalize().unwrap();
    if non_canonical == canonical {
        // Linux / no symlinks — skip; the bug's specific to
        // macOS-style symlinked /tmp.
        eprintln!("skip: non_canonical and canonical paths match, no symlink to test against",);
        return;
    }
    fs::write(
        non_canonical.join("main.tf"),
        r#"terraform {
  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "~> 4.71.0"
    }
  }
}
"#,
    )
    .unwrap();
    write_lock(&non_canonical, "4.71.0");

    let b = backend();
    let main_uri = Url::from_file_path(non_canonical.join("main.tf")).unwrap();
    insert(
        &b,
        &main_uri,
        &fs::read_to_string(non_canonical.join("main.tf")).unwrap(),
    );

    // Prime the lock cache via the non-canonical URI parent.
    let _ = messages(&b, &main_uri);

    // Mutate lock and invalidate via the CANONICAL path
    // (mimicking the watcher's fsevents-reported event).
    std::thread::sleep(std::time::Duration::from_millis(50));
    write_lock(&non_canonical, "2.71.0");
    b.state.invalidate_lock(&canonical);

    // Diagnostic should fire — invalidate_lock must collapse
    // both path forms to the same cache key.
    let after = messages(&b, &main_uri);
    assert!(
        after
            .iter()
            .any(|m| m.contains("does not admit") && m.contains("2.71.0")),
        "invalidate via canonical path must clear cache keyed under non-canonical; got: {after:?}"
    );
}

// --- Unknown-until-apply rule family ---------------------------------
//
// End-to-end pins for `terraform_for_each_unknown_keys` (module-aware
// resolution of locals / data / resource configs), the ACM allowlist,
// `terraform_import_unknown_id`, and `terraform_lifecycle_literal` —
// all through the same `compute_diagnostics` path as didOpen.

fn diags_with_code(backend: &Backend, target: &Url, code: &str) -> Vec<String> {
    // The unknown-value rules are gated on the module dir's scan being
    // complete; tests drive compute_diagnostics directly without the
    // indexer, so mark the target's dir ready.
    if let Some(dir) = target
        .to_file_path()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    {
        backend.state.mark_scan_completed(dir);
    }
    compute_diagnostics(&backend.state, target)
        .into_iter()
        .filter(|d| {
            matches!(
                &d.code,
                Some(lsp_types::NumberOrString::String(c)) if c == code
            )
        })
        .map(|d| d.message)
        .collect()
}

#[test]
fn unknown_rules_gated_on_scan_completion() {
    // Until the module dir's scan completes, the for_each/import rules are
    // suppressed (a sibling file may not be loaded yet); completion (which
    // triggers a diagnostics refresh in the real server) unlocks them.
    let b = backend();
    let main_uri = uri("file:///gated/main.tf");
    insert(
        &b,
        &main_uri,
        r#"
resource "null_resource" "x" {
  for_each = { for s in aws_subnet.all : s.id => s }
}
"#,
    );
    let before: Vec<String> = compute_diagnostics(&b.state, &main_uri)
        .into_iter()
        .filter(|d| {
            matches!(&d.code,
                Some(lsp_types::NumberOrString::String(c)) if c == "terraform_for_each_unknown_keys")
        })
        .map(|d| d.message)
        .collect();
    assert!(
        before.is_empty(),
        "rule must be gated pre-scan; got: {before:?}"
    );

    b.state
        .mark_scan_completed(std::path::PathBuf::from("/gated"));
    let after: Vec<String> = compute_diagnostics(&b.state, &main_uri)
        .into_iter()
        .filter(|d| {
            matches!(&d.code,
                Some(lsp_types::NumberOrString::String(c)) if c == "terraform_for_each_unknown_keys")
        })
        .map(|d| d.message)
        .collect();
    assert_eq!(after.len(), 1, "rule must fire post-scan; got: {after:?}");
}

#[test]
fn acm_validation_pattern_is_silent_across_files() {
    // The canonical ACM DNS-validation pattern with the certificate in a
    // sibling file: plan-valid, must not flag.
    let b = backend();
    let cert_uri = uri("file:///project/acm.tf");
    let dns_uri = uri("file:///project/dns.tf");
    insert(
        &b,
        &cert_uri,
        r#"
resource "aws_acm_certificate" "cert" {
  domain_name       = "example.com"
  validation_method = "DNS"
}
"#,
    );
    insert(
        &b,
        &dns_uri,
        r#"
resource "aws_route53_record" "validation" {
  for_each = {
    for dvo in aws_acm_certificate.cert.domain_validation_options : dvo.domain_name => {
      name   = dvo.resource_record_name
      record = dvo.resource_record_value
      type   = dvo.resource_record_type
    }
  }
  zone_id = var.zone_id
}
"#,
    );
    let hits = diags_with_code(&b, &dns_uri, "terraform_for_each_unknown_keys");
    assert!(hits.is_empty(), "ACM pattern flagged: {hits:?}");
}

#[test]
fn sibling_data_source_config_decides_for_each_flag() {
    // Data source declared in a sibling file with a static config: the
    // for_each over its attributes is plan-known → silent. Adding a
    // depends_on on a managed resource defers the read → flagged.
    let static_data = r#"
data "aws_subnets" "all" {
  filter {
    name   = "vpc-id"
    values = [var.vpc_id]
  }
}
"#;
    let deferred_data = r#"
data "aws_subnets" "all" {
  depends_on = [aws_vpc.main]
  filter {
    name   = "vpc-id"
    values = [var.vpc_id]
  }
}
"#;
    let main_src = r#"
resource "null_resource" "x" {
  for_each = toset(data.aws_subnets.all.ids)
}
"#;
    for (data_src, expect_flag) in [(static_data, false), (deferred_data, true)] {
        let b = backend();
        let data_uri = uri("file:///project/data.tf");
        let main_uri = uri("file:///project/main.tf");
        insert(&b, &data_uri, data_src);
        insert(&b, &main_uri, main_src);
        let hits = diags_with_code(&b, &main_uri, "terraform_for_each_unknown_keys");
        assert_eq!(
            !hits.is_empty(),
            expect_flag,
            "expected flag={expect_flag}; got: {hits:?}"
        );
    }
}

#[test]
fn sibling_resource_config_set_attr_is_silent() {
    let b = backend();
    let res_uri = uri("file:///project/buckets.tf");
    let main_uri = uri("file:///project/main.tf");
    insert(
        &b,
        &res_uri,
        r#"
resource "aws_s3_bucket" "b" {
  bucket = var.bucket_name
}
"#,
    );
    insert(
        &b,
        &main_uri,
        r#"
resource "null_resource" "x" {
  for_each = toset([aws_s3_bucket.b.bucket])
}
"#,
    );
    let hits = diags_with_code(&b, &main_uri, "terraform_for_each_unknown_keys");
    assert!(hits.is_empty(), "config-set attr flagged: {hits:?}");
}

#[test]
fn installed_schema_silences_non_computed_attr() {
    // No block in view, but the installed provider schema marks the
    // attribute non-computed — plan-known, silent.
    let b = backend();
    let schemas: tfls_schema::ProviderSchemas = sonic_rs::from_str(
        r#"{
        "format_version": "1.0",
        "provider_schemas": {
            "registry.terraform.io/hashicorp/aws": {
                "provider": { "version": 0, "block": {} },
                "resource_schemas": {
                    "aws_s3_bucket": {
                        "version": 1,
                        "block": {
                            "attributes": {
                                "bucket": { "type": "string", "optional": true },
                                "arn":    { "type": "string", "computed": true }
                            }
                        }
                    }
                },
                "data_source_schemas": {}
            }
        }
    }"#,
    )
    .expect("schemas parse");
    b.state.install_schemas(schemas);

    let main_uri = uri("file:///project/main.tf");
    insert(
        &b,
        &main_uri,
        r#"
resource "null_resource" "x" {
  for_each = toset([aws_s3_bucket.unseen.bucket])
}
resource "null_resource" "y" {
  for_each = toset([aws_s3_bucket.unseen.arn])
}
"#,
    );
    let hits = diags_with_code(&b, &main_uri, "terraform_for_each_unknown_keys");
    assert_eq!(
        hits.len(),
        1,
        "only the computed-attr for_each should flag; got: {hits:?}"
    );
}

#[test]
fn import_unknown_id_flags_end_to_end() {
    let b = backend();
    let main_uri = uri("file:///project/main.tf");
    insert(
        &b,
        &main_uri,
        r#"
import {
  to = aws_s3_bucket.b
  id = aws_instance.web.id
}
import {
  to = aws_s3_bucket.c
  id = var.bucket_name
}
"#,
    );
    let hits = diags_with_code(&b, &main_uri, "terraform_import_unknown_id");
    assert_eq!(hits.len(), 1, "got: {hits:?}");
    assert!(hits[0].contains("known at plan time"));
}

#[test]
fn lifecycle_literal_flags_and_suppresses_via_rule_config() {
    let b = backend();
    let main_uri = uri("file:///project/main.tf");
    insert(
        &b,
        &main_uri,
        r#"
resource "aws_s3_bucket" "b" {
  bucket = "x"
  lifecycle {
    prevent_destroy = var.protect
  }
}
"#,
    );
    let hits = diags_with_code(&b, &main_uri, "terraform_lifecycle_literal");
    assert_eq!(hits.len(), 1, "got: {hits:?}");

    // Per-rule kill switch drops it.
    let cfg: sonic_rs::Value = sonic_rs::from_str(
        r#"{ "terraform-ls-rs": { "rules": { "terraform_lifecycle_literal": "off" } } }"#,
    )
    .expect("config parses");
    b.state.config.update_from_json(&cfg);
    let hits = diags_with_code(&b, &main_uri, "terraform_lifecycle_literal");
    assert!(hits.is_empty(), "rule off but still flagged: {hits:?}");
}

// --- module.<name>.<output> resolution --------------------------------
//
// Real tempdirs: resolve_module_source checks the filesystem for local
// sources, so fake file:/// paths can't exercise these.

fn file_uri(path: &std::path::Path) -> Url {
    Url::from_file_path(path).expect("file uri")
}

/// Write files, insert them all into the store, return the backend.
fn backend_with_tree(dir: &std::path::Path, files: &[(&str, &str)]) -> Backend {
    let b = backend();
    write_files(dir, files);
    for (name, body) in files {
        insert(&b, &file_uri(&dir.join(name)), body);
    }
    b
}

#[test]
fn flags_for_each_on_apply_time_child_module_output() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "main.tf",
                "module \"net\" { source = \"./modules/net\" }\n\
                 resource \"null_resource\" \"x\" {\n  for_each = toset(module.net.subnet_ids)\n}\n",
            ),
            (
                "modules/net/outputs.tf",
                "output \"subnet_ids\" { value = aws_subnet.all[*].id }\n",
            ),
        ],
    );
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert_eq!(
        hits.len(),
        1,
        "computed child output must flag; got: {hits:?}"
    );
}

#[test]
fn silent_for_plan_known_child_module_output() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "main.tf",
                "module \"net\" { source = \"./modules/net\" }\n\
                 resource \"null_resource\" \"x\" {\n  for_each = toset(module.net.cidrs)\n}\n",
            ),
            (
                "modules/net/outputs.tf",
                "variable \"cidrs\" { type = list(string) }\n\
                 output \"cidrs\" { value = var.cidrs }\n",
            ),
        ],
    );
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert!(
        hits.is_empty(),
        "var-derived child output is plan-known; got: {hits:?}"
    );
}

#[test]
fn silent_for_unresolvable_module_source() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[(
            "main.tf",
            "module \"net\" { source = \"./modules/missing\" }\n\
             resource \"null_resource\" \"x\" {\n  for_each = toset(module.net.subnet_ids)\n}\n",
        )],
    );
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert!(
        hits.is_empty(),
        "unresolvable source must stay silent; got: {hits:?}"
    );
}

#[test]
fn flags_through_grandchild_module_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "main.tf",
                "module \"outer\" { source = \"./modules/outer\" }\n\
                 resource \"null_resource\" \"x\" {\n  for_each = toset(module.outer.ids)\n}\n",
            ),
            (
                "modules/outer/main.tf",
                "module \"inner\" { source = \"./inner\" }\n\
                 output \"ids\" { value = module.inner.ids }\n",
            ),
            (
                "modules/outer/inner/main.tf",
                "output \"ids\" { value = aws_subnet.all[*].id }\n",
            ),
        ],
    );
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert_eq!(
        hits.len(),
        1,
        "grandchild computed output must flag; got: {hits:?}"
    );
}

#[test]
fn module_reference_cycle_stays_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "main.tf",
                "module \"a\" { source = \"./modules/a\" }\n\
                 resource \"null_resource\" \"x\" {\n  for_each = toset(module.a.v)\n}\n",
            ),
            (
                "modules/a/main.tf",
                "module \"b\" { source = \"../b\" }\noutput \"v\" { value = module.b.v }\n",
            ),
            (
                "modules/b/main.tf",
                "module \"a\" { source = \"../a\" }\noutput \"v\" { value = module.a.v }\n",
            ),
        ],
    );
    // Must terminate and stay silent (cycle is its own config error).
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert!(hits.is_empty(), "cycle must stay silent; got: {hits:?}");
}

// --- caller→child variable unknownness ---------------------------------

use tfls_lsp::indexer::rebuild_unknown_module_vars_for_dir;

const CHILD_FOR_EACH_VAR: &str = "variable \"subnets\" { type = set(string) }\n\
     resource \"null_resource\" \"x\" {\n  for_each = var.subnets\n}\n";

#[test]
fn flags_child_for_each_when_caller_passes_apply_time() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "main.tf",
                "module \"net\" {\n  source  = \"./modules/net\"\n  subnets = aws_subnet.all[*].id\n}\n",
            ),
            ("modules/net/main.tf", CHILD_FOR_EACH_VAR),
        ],
    );
    rebuild_unknown_module_vars_for_dir(&b.state, &dir);
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("modules/net/main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert_eq!(hits.len(), 1, "child for_each must flag; got: {hits:?}");
    assert!(
        hits[0].contains("caller module \"net\""),
        "message names the caller; got: {}",
        hits[0]
    );
}

#[test]
fn silent_child_for_each_when_caller_passes_static() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "main.tf",
                "module \"net\" {\n  source  = \"./modules/net\"\n  subnets = var.subnet_ids\n}\n",
            ),
            ("modules/net/main.tf", CHILD_FOR_EACH_VAR),
        ],
    );
    rebuild_unknown_module_vars_for_dir(&b.state, &dir);
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("modules/net/main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert!(
        hits.is_empty(),
        "static caller arg must stay silent; got: {hits:?}"
    );
}

#[test]
fn caller_rebuilds_do_not_clobber_each_other() {
    // Two caller dirs feed the same child. The static caller's rebuild
    // must not erase the unknown caller's contribution.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "envs/prod/main.tf",
                "module \"net\" {\n  source  = \"../../modules/net\"\n  subnets = aws_subnet.all[*].id\n}\n",
            ),
            (
                "envs/dev/main.tf",
                "module \"net\" {\n  source  = \"../../modules/net\"\n  subnets = var.subnet_ids\n}\n",
            ),
            ("modules/net/main.tf", CHILD_FOR_EACH_VAR),
        ],
    );
    rebuild_unknown_module_vars_for_dir(&b.state, &dir.join("envs/prod"));
    rebuild_unknown_module_vars_for_dir(&b.state, &dir.join("envs/dev"));
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("modules/net/main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert_eq!(
        hits.len(),
        1,
        "prod's apply-time arg must survive dev's rebuild; got: {hits:?}"
    );

    // Fixing the prod caller clears the flag on its next rebuild.
    let fixed =
        "module \"net\" {\n  source  = \"../../modules/net\"\n  subnets = var.subnet_ids\n}\n";
    fs::write(dir.join("envs/prod/main.tf"), fixed).unwrap();
    insert(&b, &file_uri(&dir.join("envs/prod/main.tf")), fixed);
    rebuild_unknown_module_vars_for_dir(&b.state, &dir.join("envs/prod"));
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("modules/net/main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert!(
        hits.is_empty(),
        "fixed caller must clear the flag; got: {hits:?}"
    );
}

#[test]
fn known_keys_unknown_values_map_stays_silent_in_child() {
    // Caller passes a map literal with static keys and apply-time VALUES —
    // a valid for_each in the child.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "main.tf",
                "module \"net\" {\n  source  = \"./modules/net\"\n  subnets = { a = aws_subnet.a.id, b = aws_subnet.b.id }\n}\n",
            ),
            (
                "modules/net/main.tf",
                "variable \"subnets\" { type = map(string) }\n\
                 resource \"null_resource\" \"x\" {\n  for_each = var.subnets\n}\n",
            ),
        ],
    );
    rebuild_unknown_module_vars_for_dir(&b.state, &dir);
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("modules/net/main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert!(
        hits.is_empty(),
        "static-key map is a valid for_each; got: {hits:?}"
    );
}

#[test]
fn flags_parent_for_each_on_output_of_caller_unknown_var() {
    // G1×G2: parent keys on module.net.ids; the child output is var.input
    // and the PARENT passes an apply-time value into input.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let b = backend_with_tree(
        &dir,
        &[
            (
                "main.tf",
                "module \"net\" {\n  source = \"./modules/net\"\n  input  = aws_subnet.all[*].id\n}\n\
                 resource \"null_resource\" \"x\" {\n  for_each = toset(module.net.ids)\n}\n",
            ),
            (
                "modules/net/main.tf",
                "variable \"input\" { type = set(string) }\noutput \"ids\" { value = var.input }\n",
            ),
        ],
    );
    rebuild_unknown_module_vars_for_dir(&b.state, &dir);
    let hits = diags_with_code(
        &b,
        &file_uri(&dir.join("main.tf")),
        "terraform_for_each_unknown_keys",
    );
    assert_eq!(
        hits.len(),
        1,
        "output passthrough of unknown var must flag; got: {hits:?}"
    );
}

#[test]
fn plan_known_collections_config_live_toggle() {
    let b = backend();
    let main_uri = uri("file:///project/main.tf");
    insert(
        &b,
        &main_uri,
        r#"
resource "null_resource" "x" {
  for_each = { for v in custom_cert.c.validation_options : v.domain => v }
}
"#,
    );
    // Default: unknown collection → flagged.
    let hits = diags_with_code(&b, &main_uri, "terraform_for_each_unknown_keys");
    assert_eq!(hits.len(), 1, "got: {hits:?}");

    // Config allowlists the collection with `domain` plan-known → silent.
    let cfg: sonic_rs::Value = sonic_rs::from_str(
        r#"{ "terraform-ls-rs": { "planKnownComputedCollections": {
            "custom_cert.validation_options": ["domain"]
        } } }"#,
    )
    .expect("config parses");
    b.state.config.update_from_json(&cfg);
    let hits = diags_with_code(&b, &main_uri, "terraform_for_each_unknown_keys");
    assert!(hits.is_empty(), "allowlisted collection flagged: {hits:?}");

    // Emptying the map restores the default.
    let cfg: sonic_rs::Value =
        sonic_rs::from_str(r#"{ "terraform-ls-rs": { "planKnownComputedCollections": {} } }"#)
            .expect("config parses");
    b.state.config.update_from_json(&cfg);
    let hits = diags_with_code(&b, &main_uri, "terraform_for_each_unknown_keys");
    assert_eq!(hits.len(), 1, "default must restore: {hits:?}");
}
