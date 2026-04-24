//! Construct the LSP server capabilities we advertise.

use tower_lsp::lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionProviderCapability, CodeLensOptions,
    CompletionOptions, DeclarationCapability, DocumentLinkOptions,
    DocumentOnTypeFormattingOptions, ExecuteCommandOptions, FoldingRangeProviderCapability,
    HoverProviderCapability, OneOf, RenameOptions, SelectionRangeProviderCapability,
    SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions,
    SemanticTokensServerCapabilities, ServerCapabilities, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, WorkDoneProgressOptions,
};

use crate::handlers::semantic_tokens::SEMANTIC_TOKEN_TYPES;

pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(
            TextDocumentSyncKind::INCREMENTAL,
        )),
        definition_provider: Some(OneOf::Left(true)),
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string(), "\"".to_string()]),
            resolve_provider: Some(false),
            ..Default::default()
        }),
        semantic_tokens_provider: Some(
            SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                work_done_progress_options: WorkDoneProgressOptions::default(),
                legend: SemanticTokensLegend {
                    token_types: SEMANTIC_TOKEN_TYPES.to_vec(),
                    token_modifiers: Vec::new(),
                },
                range: Some(true),
                full: Some(SemanticTokensFullOptions::Bool(true)),
            }),
        ),
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
            first_trigger_character: "}".to_string(),
            more_trigger_character: None,
        }),
        document_link_provider: Some(DocumentLinkOptions {
            resolve_provider: Some(false),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
            work_done_progress_options: WorkDoneProgressOptions::default(),
            resolve_provider: Some(false),
        })),
        code_lens_provider: Some(CodeLensOptions {
            resolve_provider: Some(false),
        }),
        inlay_hint_provider: Some(OneOf::Left(true)),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: Some(vec![",".to_string()]),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        })),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: crate::handlers::commands::COMMANDS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            work_done_progress_options: WorkDoneProgressOptions::default(),
        }),
        // Pull diagnostics deliberately NOT advertised. Neovim 0.11+
        // (and other clients that follow LSP 3.17 to the letter) put
        // push-mode `publishDiagnostics` and pull-mode
        // `textDocument/diagnostic` into separate
        // `vim.diagnostic` namespaces and render the union of both —
        // see `runtime/lua/vim/lsp/diagnostic.lua` lines 188-230 in
        // the upstream nvim source. The auto-pull autocmd that
        // `_enable` (line 436) installs only fires for the buffer
        // where the edit happened, so a pull populates `main.tf`'s
        // pull namespace once on didOpen and then never refreshes
        // when a peer file is edited. Our cross-file pushes update
        // the push namespace correctly but the stale pull entries
        // survive in the union — the user-visible "fix didn't take"
        // bug.
        //
        // Switching to push-only routes every diagnostic through one
        // namespace. The bulk-scan + `publish_peer_diagnostics`
        // pipeline already covers every workspace file, so workspace
        // views (Trouble's `workspace_diagnostics` etc.) keep
        // populating from the same push stream.
        diagnostic_provider: None,
        experimental: Some(serde_json::json!({
            "terraform-ls": {
                "searchDocs": { "version": 1 },
                "getDoc":     { "version": 1 },
                "getSnippet": { "version": 1 }
            }
        })),
        ..Default::default()
    }
}
