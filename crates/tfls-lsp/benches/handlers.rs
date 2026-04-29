//! Handler-level benchmarks — exercise hot paths at realistic scale.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use tfls_lsp::Backend;
use tfls_lsp::handlers;
use tfls_state::{DocumentState, JobQueue, StateStore};
use tokio::runtime::Runtime;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    CodeActionContext, CodeActionParams, DocumentSymbolParams, PartialResultParams, Position,
    Range, TextDocumentIdentifier, Url, WorkDoneProgressParams, WorkspaceSymbolParams,
};

/// Build a realistic workspace with many symbols across many files.
fn populate(state: &StateStore, files: usize, per_file_vars: usize) {
    for f in 0..files {
        let uri = Url::parse(&format!("file:///f{f}.tf")).expect("url");
        let mut src = String::new();
        for v in 0..per_file_vars {
            src.push_str(&format!("variable \"v_{f}_{v}\" {{}}\n"));
        }
        src.push_str(&format!(
            "resource \"aws_instance\" \"r_{f}\" {{ ami = \"x\" }}\n"
        ));
        state.upsert_document(DocumentState::new(uri, &src, 1));
    }
}

fn backend(state: Arc<StateStore>, jobs: Arc<JobQueue>) -> Backend {
    let (service, _) = LspService::new(Backend::new);
    Backend::with_shared_state(service.inner().client.clone(), state, jobs)
}

fn bench_workspace_symbol(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");
    let state = Arc::new(StateStore::new());
    // 100 files × 100 vars = 10 000 symbols, roughly a large monorepo.
    populate(&state, 100, 100);
    let jobs = Arc::new(JobQueue::new());
    let backend = backend(Arc::clone(&state), Arc::clone(&jobs));

    let mut group = c.benchmark_group("workspace_symbol");
    group.bench_function("10k_symbols_exact_match", |b| {
        b.iter(|| {
            rt.block_on(async {
                let _ = handlers::symbols::workspace_symbol(
                    &backend,
                    WorkspaceSymbolParams {
                        query: "v_50_50".to_string(),
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    },
                )
                .await;
            });
        });
    });
    group.bench_function("10k_symbols_fuzzy", |b| {
        b.iter(|| {
            rt.block_on(async {
                let _ = handlers::symbols::workspace_symbol(
                    &backend,
                    WorkspaceSymbolParams {
                        query: "v".to_string(),
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    },
                )
                .await;
            });
        });
    });
    group.finish();
}

fn bench_document_symbol(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");
    let state = Arc::new(StateStore::new());
    populate(&state, 1, 500);
    let jobs = Arc::new(JobQueue::new());
    let backend = backend(Arc::clone(&state), Arc::clone(&jobs));
    let uri = Url::parse("file:///f0.tf").expect("url");

    c.bench_function("document_symbol_500_symbols", |b| {
        b.iter(|| {
            rt.block_on(async {
                let _ = handlers::symbols::document_symbol(
                    &backend,
                    DocumentSymbolParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    },
                )
                .await;
            });
        });
    });
}

fn bench_enclosing_call(c: &mut Criterion) {
    use tfls_lsp::handlers::signature_help::enclosing_call;

    // A realistic 200-line body with function calls sprinkled throughout.
    let mut src = String::new();
    for _ in 0..200 {
        src.push_str("locals { x = format(\"%s-%d\", var.name, length([1,2,3])) }\n");
    }
    // Cursor at the end of a middle line, inside a nested call.
    let target = src.len() / 2;

    c.bench_function("signature_help_enclosing_call_200_lines", |b| {
        b.iter(|| {
            let _ = enclosing_call(&src, target);
        });
    });
}

/// Populate a single .tf file in `dir` with a configurable mix of
/// deprecated blocks + reference sites. Used to stress the per-doc
/// scans + reference walkers in the code-action handler.
fn populate_deprecation_workload(
    state: &StateStore,
    dir: &str,
    null_resources: usize,
    template_files: usize,
    refs_per_block: usize,
) -> Url {
    let mut src = String::new();
    src.push_str("terraform { required_version = \">= 1.5\" }\n");

    // null_resource blocks + N refs each.
    for i in 0..null_resources {
        src.push_str(&format!(
            "resource \"null_resource\" \"r{i}\" {{\n  triggers = {{ k = \"v{i}\" }}\n}}\n"
        ));
        for j in 0..refs_per_block {
            src.push_str(&format!(
                "output \"o_nr_{i}_{j}\" {{ value = null_resource.r{i}.triggers }}\n"
            ));
        }
    }

    // template_file blocks + N refs each.
    for i in 0..template_files {
        src.push_str(&format!(
            "data \"template_file\" \"t{i}\" {{\n  template = \"hi ${{name}}\"\n  vars = {{ name = \"x{i}\" }}\n}}\n"
        ));
        for j in 0..refs_per_block {
            src.push_str(&format!(
                "output \"o_tf_{i}_{j}\" {{ value = data.template_file.t{i}.rendered }}\n"
            ));
        }
    }

    let uri = Url::parse(&format!("file:///{dir}/main.tf")).expect("url");
    state.upsert_document(DocumentState::new(uri.clone(), &src, 1));
    uri
}

/// Single-doc fixture parameterised by which deprecation
/// shape it contains. Used to isolate per-emit cost.
fn populate_isolated(state: &StateStore, dir: &str, kind: &str, n: usize) -> Url {
    let mut src = String::from("terraform { required_version = \">= 1.5\" }\n");
    match kind {
        "null_resource" => {
            for i in 0..n {
                src.push_str(&format!(
                    "resource \"null_resource\" \"r{i}\" {{\n  triggers = {{ k = \"v{i}\" }}\n}}\n"
                ));
                src.push_str(&format!(
                    "output \"o{i}\" {{ value = null_resource.r{i}.triggers }}\n"
                ));
            }
        }
        "template_file" => {
            for i in 0..n {
                src.push_str(&format!(
                    "data \"template_file\" \"t{i}\" {{\n  template = \"hi\"\n  vars = {{ k = \"v{i}\" }}\n}}\n"
                ));
                src.push_str(&format!(
                    "output \"o{i}\" {{ value = data.template_file.t{i}.rendered }}\n"
                ));
            }
        }
        "plain_outputs" => {
            // No deprecations, just outputs — exercises non-
            // deprecation emit fns (unwrap, lookup, refine_any,
            // set-types, declare-undefined, move-outputs,
            // move-vars, format).
            for i in 0..n {
                src.push_str(&format!("output \"o{i}\" {{ value = \"x{i}\" }}\n"));
            }
        }
        _ => unreachable!(),
    }
    let uri = Url::parse(&format!("file:///{dir}/main.tf")).expect("url");
    state.upsert_document(DocumentState::new(uri.clone(), &src, 1));
    uri
}

/// Per-shape isolated benches — pinpoints which emit fn
/// dominates the 500-block cost.
fn bench_code_action_isolated(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("code_action_isolated");
    for kind in ["null_resource", "template_file", "plain_outputs"] {
        let state = Arc::new(StateStore::new());
        let uri = populate_isolated(&state, kind, kind, 500);
        let jobs = Arc::new(JobQueue::new());
        let backend = backend(Arc::clone(&state), Arc::clone(&jobs));

        let params = CodeActionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position::new(2, 0),
                end: Position::new(2, 0),
            },
            context: CodeActionContext::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        group.bench_function(format!("500_{kind}"), |b| {
            b.iter(|| {
                rt.block_on(async {
                    let _ = handlers::code_action::code_action(&backend, params.clone()).await;
                });
            });
        });
    }
    group.finish();
}

fn bench_code_action_deprecation_pipeline(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("code_action_deprecation");
    // Realistic large module: 100 deprecated blocks + 5 refs each
    // = 1k+ traversals to walk on every code-action invocation.
    for &(label, n_each, refs_per) in &[
        ("small/10_blocks_2_refs", 10usize, 2usize),
        ("medium/100_blocks_5_refs", 100, 5),
        ("large/500_blocks_5_refs", 500, 5),
    ] {
        let state = Arc::new(StateStore::new());
        let uri = populate_deprecation_workload(&state, label, n_each, n_each, refs_per);
        let jobs = Arc::new(JobQueue::new());
        let backend = backend(Arc::clone(&state), Arc::clone(&jobs));

        let params = CodeActionParams {
            text_document: TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position::new(2, 0),
                end: Position::new(2, 0),
            },
            context: CodeActionContext::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        group.bench_function(label, |b| {
            b.iter(|| {
                rt.block_on(async {
                    let _ = handlers::code_action::code_action(&backend, params.clone()).await;
                });
            });
        });
    }
    group.finish();
}

/// Synthetic workspace exercising the block-rename path:
///   - `aws_alb` resources (Aliased migration kind → real moved.tf)
///   - `kubernetes_pod` resources (Manual migration kind → commented scaffolding)
///   - References per block to drive the ref-rewrite scan
///   - Cross-doc structure: half the blocks in `main.tf`, half in
///     `extras.tf` so the multi-scope path's per-doc cache is
///     actually exercised.
fn populate_block_rename_workload(
    state: &StateStore,
    dir: &str,
    n_aws: usize,
    n_k8s: usize,
    refs_per_block: usize,
) -> (Url, Url) {
    let mut main_src = String::new();
    main_src.push_str("terraform {\n  required_providers {\n");
    main_src.push_str("    aws = \"~> 5.0\"\n");
    main_src.push_str("    kubernetes = \"~> 2.20\"\n");
    main_src.push_str("  }\n}\n");

    for i in 0..n_aws {
        main_src.push_str(&format!(
            "resource \"aws_alb\" \"alb{i}\" {{\n  name = \"alb{i}\"\n}}\n"
        ));
        for j in 0..refs_per_block {
            main_src.push_str(&format!(
                "output \"o_alb_{i}_{j}\" {{ value = aws_alb.alb{i}.arn }}\n"
            ));
        }
    }

    let mut extras_src = String::new();
    for i in 0..n_k8s {
        extras_src.push_str(&format!(
            "resource \"kubernetes_pod\" \"p{i}\" {{\n  metadata {{\n    name = \"p{i}\"\n  }}\n}}\n"
        ));
        for j in 0..refs_per_block {
            extras_src.push_str(&format!(
                "output \"o_pod_{i}_{j}\" {{ value = kubernetes_pod.p{i}.metadata }}\n"
            ));
        }
    }

    let main_uri = Url::parse(&format!("file:///{dir}/main.tf")).expect("url");
    let extras_uri = Url::parse(&format!("file:///{dir}/extras.tf")).expect("url");
    state.upsert_document(DocumentState::new(main_uri.clone(), &main_src, 1));
    state.upsert_document(DocumentState::new(extras_uri.clone(), &extras_src, 1));
    (main_uri, extras_uri)
}

fn bench_code_action_block_rename(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("code_action_block_rename");
    // Mix of Aliased (aws_alb) + Manual (kubernetes_pod) so both
    // moved.tf code paths run. Cross-doc fixture (main.tf +
    // extras.tf) so the per-doc scan cache is non-trivial.
    for &(label, n_aws, n_k8s, refs_per) in &[
        ("small/10_each_2_refs", 10usize, 10usize, 2usize),
        ("medium/100_each_5_refs", 100, 100, 5),
        ("large/250_each_5_refs", 250, 250, 5),
    ] {
        let state = Arc::new(StateStore::new());
        let (main_uri, _extras_uri) =
            populate_block_rename_workload(&state, label, n_aws, n_k8s, refs_per);
        let jobs = Arc::new(JobQueue::new());
        let backend = backend(Arc::clone(&state), Arc::clone(&jobs));

        let params = CodeActionParams {
            text_document: TextDocumentIdentifier { uri: main_uri },
            range: Range {
                start: Position::new(2, 0),
                end: Position::new(2, 0),
            },
            context: CodeActionContext::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        group.bench_function(label, |b| {
            b.iter(|| {
                rt.block_on(async {
                    let _ = handlers::code_action::code_action(&backend, params.clone()).await;
                });
            });
        });
    }
    group.finish();
}

/// Cursor variant: cursor inside an `aws_alb` block. Exercises
/// the diag-attached / cursor path's name-filtered ref rewrite,
/// which has a different perf profile from the multi-scope
/// emit (single-block lookup + per-doc walk for name capture).
fn bench_code_action_block_rename_cursor(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");

    let mut group = c.benchmark_group("code_action_block_rename_cursor");
    for &(label, n_aws, refs_per) in &[
        ("small/10_blocks_2_refs", 10usize, 2usize),
        ("medium/100_blocks_5_refs", 100, 5),
        ("large/500_blocks_5_refs", 500, 5),
    ] {
        let state = Arc::new(StateStore::new());
        let (main_uri, _) = populate_block_rename_workload(&state, label, n_aws, 0, refs_per);
        let jobs = Arc::new(JobQueue::new());
        let backend = backend(Arc::clone(&state), Arc::clone(&jobs));

        // Cursor on the first `aws_alb` block label.
        let params = CodeActionParams {
            text_document: TextDocumentIdentifier { uri: main_uri },
            range: Range {
                // Line 5 = first resource block's header (after
                // the 5-line `terraform {}` preamble).
                start: Position::new(5, 12),
                end: Position::new(5, 12),
            },
            context: CodeActionContext::default(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        group.bench_function(label, |b| {
            b.iter(|| {
                rt.block_on(async {
                    let _ = handlers::code_action::code_action(&backend, params.clone()).await;
                });
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_workspace_symbol,
    bench_document_symbol,
    bench_enclosing_call,
    bench_code_action_deprecation_pipeline,
    bench_code_action_isolated,
    bench_code_action_block_rename,
    bench_code_action_block_rename_cursor
);
criterion_main!(benches);
