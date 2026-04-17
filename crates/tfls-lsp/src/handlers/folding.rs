//! `textDocument/foldingRange` — emit a fold for every block and
//! nested block in the parsed body.
//!
//! `textDocument/selectionRange` — given cursor positions, walk the
//! AST outward and return the chain of enclosing ranges.

use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, Body};
use lsp_types::{
    FoldingRange, FoldingRangeKind, FoldingRangeParams, Position, Range, SelectionRange,
    SelectionRangeParams,
};
use ropey::Rope;
use tfls_parser::hcl_span_to_lsp_range;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn folding_range(
    backend: &Backend,
    params: FoldingRangeParams,
) -> jsonrpc::Result<Option<Vec<FoldingRange>>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(body) = doc.parsed.body.as_ref() else {
        return Ok(None);
    };

    let mut out = Vec::new();
    collect_block_folds(body, &doc.rope, &mut out);
    if out.is_empty() { Ok(None) } else { Ok(Some(out)) }
}

fn collect_block_folds(body: &Body, rope: &Rope, out: &mut Vec<FoldingRange>) {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        if let Some(range) = block
            .span()
            .and_then(|s| hcl_span_to_lsp_range(rope, s).ok())
        {
            // Only fold multi-line blocks; a single-line block has
            // nothing to hide.
            if range.end.line > range.start.line {
                out.push(FoldingRange {
                    start_line: range.start.line,
                    start_character: Some(range.start.character),
                    end_line: range.end.line,
                    end_character: Some(range.end.character),
                    kind: Some(FoldingRangeKind::Region),
                    collapsed_text: None,
                });
            }
        }
        collect_block_folds(&block.body, rope, out);
    }
}

pub async fn selection_range(
    backend: &Backend,
    params: SelectionRangeParams,
) -> jsonrpc::Result<Option<Vec<SelectionRange>>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(body) = doc.parsed.body.as_ref() else {
        return Ok(None);
    };

    let mut out = Vec::new();
    for pos in params.positions {
        out.push(build_selection_chain(body, &doc.rope, pos));
    }
    Ok(Some(out))
}

fn build_selection_chain(body: &Body, rope: &Rope, pos: Position) -> SelectionRange {
    // Gather all enclosing block ranges from outermost to innermost.
    let mut ancestors: Vec<Range> = Vec::new();
    visit_chain(body, rope, pos, &mut ancestors);

    // Also add a single-character range at the cursor as the innermost
    // fallback, so clients always have at least one level.
    let leaf_range = Range {
        start: pos,
        end: pos,
    };

    // Build from innermost to outermost.
    let mut chain: Option<Box<SelectionRange>> = None;
    for range in ancestors.into_iter().rev() {
        chain = Some(Box::new(SelectionRange {
            range,
            parent: chain,
        }));
    }

    SelectionRange {
        range: leaf_range,
        parent: chain,
    }
}

fn visit_chain(body: &Body, rope: &Rope, pos: Position, out: &mut Vec<Range>) {
    for structure in body.iter() {
        let Some(block) = structure.as_block() else {
            continue;
        };
        let Some(range) = block
            .span()
            .and_then(|s| hcl_span_to_lsp_range(rope, s).ok())
        else {
            continue;
        };
        if !contains(&range, pos) {
            continue;
        }
        out.push(range);
        // Recurse to find inner containers.
        visit_chain(&block.body, rope, pos, out);
        return; // At most one matching block per level.
    }

    // Also include matching attribute ranges as the innermost container.
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if let Some(range) = attr
                .span()
                .and_then(|s| hcl_span_to_lsp_range(rope, s).ok())
            {
                if contains(&range, pos) {
                    out.push(range);
                    return;
                }
            }
        }
    }
    let _block: &Block; // silence unused-import for visual reader
}

fn contains(range: &Range, pos: Position) -> bool {
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    after_start && before_end
}
