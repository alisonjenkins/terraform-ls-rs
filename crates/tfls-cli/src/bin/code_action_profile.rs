//! Profiling driver for `code_action` — populates a synthetic
//! workspace, fires N invocations, and exits. Designed to be
//! attached to under `samply` / `perf` to surface hot frames.
//!
//! ```bash
//! cargo build --release -p tfls-cli --bin tfls-code-action-profile
//! samply record ./target/release/tfls-code-action-profile
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Instant;

use tfls_lsp::handlers;
use tfls_lsp::Backend;
use tfls_state::{DocumentState, JobQueue, StateStore};
use tower_lsp::lsp_types::{
    CodeActionContext, CodeActionParams, PartialResultParams, Position, Range,
    TextDocumentIdentifier, Url, WorkDoneProgressParams,
};
use tower_lsp::LspService;

/// Build a synthetic 500-block fixture mirroring the
/// `code_action_deprecation` bench's `large` case so flame
/// data lines up with bench numbers.
fn fixture_src(n: usize) -> String {
    let mut src = String::with_capacity(n * 200);
    src.push_str("terraform { required_version = \">= 1.5\" }\n");
    for i in 0..n {
        src.push_str(&format!(
            "resource \"null_resource\" \"r{i}\" {{\n  triggers = {{ k = \"v{i}\" }}\n}}\n"
        ));
        for j in 0..5 {
            src.push_str(&format!(
                "output \"o_nr_{i}_{j}\" {{ value = null_resource.r{i}.triggers }}\n"
            ));
        }
    }
    for i in 0..n {
        src.push_str(&format!(
            "data \"template_file\" \"t{i}\" {{\n  template = \"hi\"\n  vars = {{ k = \"v{i}\" }}\n}}\n"
        ));
        for j in 0..5 {
            src.push_str(&format!(
                "output \"o_tf_{i}_{j}\" {{ value = data.template_file.t{i}.rendered }}\n"
            ));
        }
    }
    src
}

#[tokio::main]
async fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    let state = Arc::new(StateStore::new());
    let uri = Url::parse("file:///profile/main.tf").expect("url");
    let src = fixture_src(n);
    state.upsert_document(DocumentState::new(uri.clone(), &src, 1));

    let jobs = Arc::new(JobQueue::new());
    let (service, _) = LspService::new(Backend::new);
    let backend =
        Backend::with_shared_state(service.inner().client.clone(), Arc::clone(&state), jobs);

    let params = CodeActionParams {
        text_document: TextDocumentIdentifier { uri },
        range: Range {
            start: Position::new(2, 0),
            end: Position::new(2, 0),
        },
        context: CodeActionContext::default(),
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };

    eprintln!("warming up...");
    for _ in 0..3 {
        let _ = handlers::code_action::code_action(&backend, params.clone()).await;
    }

    eprintln!("running {iters} iterations against {n}-block fixture");
    let t0 = Instant::now();
    for _ in 0..iters {
        let _ = handlers::code_action::code_action(&backend, params.clone()).await;
    }
    let elapsed = t0.elapsed();
    eprintln!(
        "{iters} calls in {:.2?} ({:.2?} avg)",
        elapsed,
        elapsed / (iters as u32)
    );
}
