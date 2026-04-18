//! Semantic tokens — highlights block keywords, resource/data type
//! names, variable/local/module reference roots, and attribute keys
//! using symbol and reference tables maintained on the document.

use lsp_types::{
    SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensParams,
    SemanticTokensResult, SemanticTokensRangeParams, SemanticTokensRangeResult,
};
use tfls_parser::ReferenceKind;
use tfls_state::DocumentState;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

/// Token type legend — index maps into [`SEMANTIC_TOKEN_TYPES`].
pub const SEMANTIC_TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,   // 0
    SemanticTokenType::TYPE,      // 1
    SemanticTokenType::VARIABLE,  // 2
    SemanticTokenType::PROPERTY,  // 3
    SemanticTokenType::NAMESPACE, // 4
];

#[allow(dead_code)]
const KEYWORD: u32 = 0;
const TYPE: u32 = 1;
const VARIABLE: u32 = 2;
#[allow(dead_code)]
const PROPERTY: u32 = 3;
const NAMESPACE: u32 = 4;

pub async fn semantic_tokens_full(
    backend: &Backend,
    params: SemanticTokensParams,
) -> jsonrpc::Result<Option<SemanticTokensResult>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    let data = encode_tokens(&doc);
    Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
        result_id: None,
        data,
    })))
}

pub async fn semantic_tokens_range(
    backend: &Backend,
    params: SemanticTokensRangeParams,
) -> jsonrpc::Result<Option<SemanticTokensRangeResult>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    // Phase 3: return full-file tokens for range requests. A future
    // iteration can filter by the requested range for efficiency.
    let data = encode_tokens(&doc);
    Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
        result_id: None,
        data,
    })))
}

/// Build the LSP-encoded token sequence (delta line/char, length, type, mods).
fn encode_tokens(doc: &DocumentState) -> Vec<SemanticToken> {
    let mut raw: Vec<RawToken> = Vec::new();

    // Definitions → TYPE for resources/data, VARIABLE for variables/
    // locals/outputs, NAMESPACE for modules. Token ranges come from the
    // symbol's `name_range` — the actual label or attribute key position
    // in source — not from the whole-block location.
    for sym in doc.symbols.variables.values() {
        push_label_token(&mut raw, sym.name_range, VARIABLE);
    }
    for sym in doc.symbols.locals.values() {
        push_label_token(&mut raw, sym.name_range, VARIABLE);
    }
    for sym in doc.symbols.outputs.values() {
        push_label_token(&mut raw, sym.name_range, VARIABLE);
    }
    for sym in doc.symbols.resources.values() {
        push_label_token(&mut raw, sym.name_range, TYPE);
    }
    for sym in doc.symbols.data_sources.values() {
        push_label_token(&mut raw, sym.name_range, TYPE);
    }
    for sym in doc.symbols.modules.values() {
        push_label_token(&mut raw, sym.name_range, NAMESPACE);
    }

    // References get highlighted by their prefix kind.
    for r in &doc.references {
        let type_id = match &r.kind {
            ReferenceKind::Variable { .. } | ReferenceKind::Local { .. } => VARIABLE,
            ReferenceKind::Module { .. } => NAMESPACE,
            ReferenceKind::Resource { .. } | ReferenceKind::DataSource { .. } => TYPE,
        };
        raw.push(RawToken::from_range(
            r.location.range(),
            type_id,
            reference_width(r),
        ));
    }

    // Top-level block identifiers are keywords — derive from block locations.
    // (Already highlighted by the symbol pass; nothing extra needed here.)

    // Finally encode relative deltas.
    raw.sort_by(|a, b| a.line.cmp(&b.line).then(a.character.cmp(&b.character)));
    encode_delta(&raw)
}

/// Push a single-line semantic token derived from a label range.
/// Ranges that span multiple lines (unexpected for block labels) are
/// dropped to avoid highlighting past the end of the label line.
fn push_label_token(raw: &mut Vec<RawToken>, range: lsp_types::Range, type_id: u32) {
    if range.start.line != range.end.line {
        return;
    }
    let length = range.end.character.saturating_sub(range.start.character);
    if length == 0 {
        return;
    }
    raw.push(RawToken {
        line: range.start.line,
        character: range.start.character,
        length,
        type_id,
        modifiers: 0,
    });
}

fn reference_width(r: &tfls_parser::Reference) -> usize {
    // Use the range width directly.
    let range = r.location.range();
    if range.start.line == range.end.line {
        range.end.character.saturating_sub(range.start.character) as usize
    } else {
        // Cross-line; use a conservative 0 to avoid highlighting too much.
        0
    }
}

#[derive(Debug, Clone, Copy)]
struct RawToken {
    line: u32,
    character: u32,
    length: u32,
    type_id: u32,
    modifiers: u32,
}

impl RawToken {
    fn from_range(range: lsp_types::Range, type_id: u32, length: usize) -> Self {
        Self {
            line: range.start.line,
            character: range.start.character,
            length: length as u32,
            type_id,
            modifiers: 0,
        }
    }
}

fn encode_delta(tokens: &[RawToken]) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(tokens.len());
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;
    for t in tokens {
        let delta_line = t.line.saturating_sub(prev_line);
        let delta_char = if delta_line == 0 {
            t.character.saturating_sub(prev_char)
        } else {
            t.character
        };
        out.push(SemanticToken {
            delta_line,
            delta_start: delta_char,
            length: t.length,
            token_type: t.type_id,
            token_modifiers_bitset: t.modifiers,
        });
        prev_line = t.line;
        prev_char = t.character;
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn delta_encoding_handles_same_line() {
        let raw = vec![
            RawToken {
                line: 0,
                character: 2,
                length: 3,
                type_id: 0,
                modifiers: 0,
            },
            RawToken {
                line: 0,
                character: 10,
                length: 4,
                type_id: 1,
                modifiers: 0,
            },
        ];
        let enc = encode_delta(&raw);
        assert_eq!(enc[0].delta_line, 0);
        assert_eq!(enc[0].delta_start, 2);
        assert_eq!(enc[1].delta_line, 0);
        assert_eq!(enc[1].delta_start, 8); // 10 - 2
    }

    #[test]
    fn delta_encoding_resets_character_on_line_change() {
        let raw = vec![
            RawToken {
                line: 0,
                character: 5,
                length: 3,
                type_id: 0,
                modifiers: 0,
            },
            RawToken {
                line: 3,
                character: 1,
                length: 2,
                type_id: 0,
                modifiers: 0,
            },
        ];
        let enc = encode_delta(&raw);
        assert_eq!(enc[1].delta_line, 3);
        assert_eq!(enc[1].delta_start, 1);
    }
}
