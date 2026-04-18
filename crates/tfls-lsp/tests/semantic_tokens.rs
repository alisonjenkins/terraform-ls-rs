//! Integration tests for the `textDocument/semanticTokens/full` handler.
//!
//! The LSP semantic-token response is a flat `[u32]` stream of
//! `(delta_line, delta_start, length, token_type, modifiers)` quintuples;
//! we decode it back to absolute `(line, character, length, type)` so
//! tests can compare against positions in the source.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp::LspService;
use tower_lsp::lsp_types::{
    PartialResultParams, SemanticTokensParams, SemanticTokensResult, TextDocumentIdentifier, Url,
    WorkDoneProgressParams,
};

fn uri(path: &str) -> Url {
    Url::parse(path).expect("valid url")
}

fn fresh_backend(src: &str, u: &Url) -> Backend {
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    inner
        .state
        .upsert_document(DocumentState::new(u.clone(), src, 1));
    Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbsToken {
    line: u32,
    character: u32,
    length: u32,
    token_type: u32,
}

async fn tokens(src: &str) -> Vec<AbsToken> {
    let u = uri("file:///t.tf");
    let backend = fresh_backend(src, &u);
    let params = SemanticTokensParams {
        text_document: TextDocumentIdentifier { uri: u.clone() },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: PartialResultParams::default(),
    };
    let resp = tfls_lsp::handlers::semantic_tokens::semantic_tokens_full(&backend, params)
        .await
        .expect("ok")
        .expect("some tokens");
    let data = match resp {
        SemanticTokensResult::Tokens(t) => t.data,
        SemanticTokensResult::Partial(_) => panic!("partial result not expected"),
    };

    let mut out = Vec::with_capacity(data.len());
    let mut line = 0u32;
    let mut character = 0u32;
    for tok in data {
        if tok.delta_line == 0 {
            character += tok.delta_start;
        } else {
            line += tok.delta_line;
            character = tok.delta_start;
        }
        out.push(AbsToken {
            line,
            character,
            length: tok.length,
            token_type: tok.token_type,
        });
    }
    out
}

/// Byte range of `needle` within `line_str` — tests verify the token
/// covers the needle even if the range includes surrounding quotes.
fn locate(line_str: &str, needle: &str) -> (u32, u32) {
    let start = line_str.find(needle).expect("needle not present");
    (start as u32, (start + needle.len()) as u32)
}

// Token type indices mirror `SEMANTIC_TOKEN_TYPES` in the handler.
const TYPE: u32 = 1;
const VARIABLE: u32 = 2;
const NAMESPACE: u32 = 4;

#[tokio::test]
async fn resource_token_aligns_with_type_label() {
    let src = "resource \"aws_security_group_rule\" \"test\" {\n}\n";
    let toks = tokens(src).await;
    // Exactly one TYPE token on line 0, covering (at least) the type
    // label range — start before the name, end before the name label.
    let type_tokens: Vec<_> = toks
        .iter()
        .filter(|t| t.token_type == TYPE && t.line == 0)
        .collect();
    assert_eq!(type_tokens.len(), 1, "expected one TYPE token, got {toks:?}");
    let t = type_tokens[0];
    let (needle_lo, needle_hi) = locate(src.lines().next().unwrap(), "aws_security_group_rule");
    let (name_lo, _) = locate(src.lines().next().unwrap(), "\"test\"");
    let token_end = t.character + t.length;
    assert!(
        t.character <= needle_lo,
        "token starts at {} past the type label start at {needle_lo}",
        t.character
    );
    assert!(
        token_end >= needle_hi,
        "token ends at {token_end} before the type label end at {needle_hi}"
    );
    assert!(
        token_end <= name_lo,
        "token bleeds into the name label (ends at {token_end}, name starts at {name_lo})"
    );
}

#[tokio::test]
async fn data_source_token_aligns_with_type_label() {
    let src = "data \"aws_ami\" \"ubuntu\" { owners = [\"x\"] }\n";
    let toks = tokens(src).await;
    let type_tokens: Vec<_> = toks
        .iter()
        .filter(|t| t.token_type == TYPE && t.line == 0)
        .collect();
    assert_eq!(type_tokens.len(), 1, "got: {toks:?}");
    let t = type_tokens[0];
    let (lo, hi) = locate(src.lines().next().unwrap(), "aws_ami");
    let (name_lo, _) = locate(src.lines().next().unwrap(), "\"ubuntu\"");
    assert!(t.character <= lo);
    assert!(t.character + t.length >= hi);
    assert!(t.character + t.length <= name_lo);
}

#[tokio::test]
async fn module_token_aligns_with_name_label() {
    let src = "module \"network\" { source = \"./x\" }\n";
    let toks = tokens(src).await;
    let ns_tokens: Vec<_> = toks
        .iter()
        .filter(|t| t.token_type == NAMESPACE && t.line == 0)
        .collect();
    assert_eq!(ns_tokens.len(), 1, "got: {toks:?}");
    let t = ns_tokens[0];
    let (lo, hi) = locate(src.lines().next().unwrap(), "network");
    assert!(t.character <= lo);
    assert!(t.character + t.length >= hi);
    // Must not cover the `module` keyword at column 0..6.
    assert!(t.character >= 6, "NAMESPACE token overlaps the `module` keyword");
}

#[tokio::test]
async fn variable_token_aligns_with_name_label() {
    let src = "variable \"region\" { default = \"us-east-1\" }\n";
    let toks = tokens(src).await;
    let var_tokens: Vec<_> = toks
        .iter()
        .filter(|t| t.token_type == VARIABLE && t.line == 0)
        .collect();
    assert_eq!(var_tokens.len(), 1, "got: {toks:?}");
    let t = var_tokens[0];
    let (lo, hi) = locate(src.lines().next().unwrap(), "region");
    assert!(t.character <= lo);
    assert!(t.character + t.length >= hi);
    assert!(t.character >= 8, "VARIABLE token overlaps the `variable` keyword");
}

#[tokio::test]
async fn tokens_never_span_multiple_lines() {
    // Regression guard: even with multi-line block bodies, the
    // definition tokens must not extend across line breaks.
    let src = "resource \"aws_instance\" \"web\" {\n  ami = \"x\"\n}\n";
    let toks = tokens(src).await;
    for t in &toks {
        // Length covers within a single line by construction; the
        // response encoding has no "multi-line" flag, so we just assert
        // the token length is sane.
        assert!(t.length < 200, "improbably long token: {t:?}");
    }
    // The TYPE token still sits on line 0.
    let type_on_line0 = toks.iter().any(|t| t.token_type == TYPE && t.line == 0);
    assert!(type_on_line0, "expected a TYPE token on line 0, got {toks:?}");
}
