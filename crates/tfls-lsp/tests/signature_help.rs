//! Integration tests for signatureHelp against the bundled function
//! signatures.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_schema::functions_cache;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    Position, SignatureHelpContext, SignatureHelpParams, SignatureHelpTriggerKind,
    TextDocumentIdentifier, TextDocumentPositionParams, Url, WorkDoneProgressParams,
};

fn uri(s: &str) -> Url {
    Url::parse(s).expect("url")
}

async fn backend_with_doc(u: &Url, src: &str) -> Backend {
    let (service, _) = LspService::new(Backend::new);
    let inner = service.inner();
    inner
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));

    // Install the bundled functions so signatureHelp has data.
    let schema = functions_cache::bundled().expect("bundled ok");
    inner.state.install_functions(schema);

    Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    )
}

fn params(u: &Url, pos: Position) -> SignatureHelpParams {
    SignatureHelpParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: u.clone() },
            position: pos,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        context: Some(SignatureHelpContext {
            trigger_kind: SignatureHelpTriggerKind::INVOKED,
            trigger_character: None,
            is_retrigger: false,
            active_signature_help: None,
        }),
    }
}

#[tokio::test]
async fn returns_signature_for_known_function() {
    let u = uri("file:///a.tf");
    let backend = backend_with_doc(&u, "output \"x\" { value = format(").await;

    // Cursor just after `format(`
    let line_len = "output \"x\" { value = format(".len() as u32;
    let help = tfls_lsp::handlers::signature_help::signature_help(
        &backend,
        params(&u, Position::new(0, line_len)),
    )
    .await
    .expect("ok")
    .expect("help");

    assert_eq!(help.signatures.len(), 1);
    assert!(help.signatures[0].label.starts_with("format("));
    assert_eq!(help.active_parameter, Some(0));
}

#[tokio::test]
async fn tracks_active_parameter_across_commas() {
    let u = uri("file:///b.tf");
    let src = "output \"x\" { value = format(\"%s\", ";
    let backend = backend_with_doc(&u, src).await;

    let help = tfls_lsp::handlers::signature_help::signature_help(
        &backend,
        params(&u, Position::new(0, src.len() as u32)),
    )
    .await
    .expect("ok")
    .expect("help");

    // `format` is variadic — 2nd arg should map to the variadic parameter (index 1).
    assert_eq!(help.active_parameter, Some(1));
}

#[tokio::test]
async fn returns_none_outside_any_call() {
    let u = uri("file:///c.tf");
    let backend = backend_with_doc(&u, "variable \"x\" {}\n").await;

    let help = tfls_lsp::handlers::signature_help::signature_help(
        &backend,
        params(&u, Position::new(0, 0)),
    )
    .await
    .expect("ok");
    assert!(help.is_none());
}

#[tokio::test]
async fn returns_none_for_unknown_function() {
    let u = uri("file:///d.tf");
    let src = "output \"x\" { value = notARealFn(";
    let backend = backend_with_doc(&u, src).await;

    let help = tfls_lsp::handlers::signature_help::signature_help(
        &backend,
        params(&u, Position::new(0, src.len() as u32)),
    )
    .await
    .expect("ok");
    assert!(help.is_none());
}
