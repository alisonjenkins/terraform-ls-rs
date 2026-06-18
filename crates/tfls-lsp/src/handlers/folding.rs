//! `textDocument/foldingRange` — emit a fold for every block and
//! nested block in the parsed body.
//!
//! `textDocument/selectionRange` — given cursor positions, walk the
//! AST outward and return the chain of enclosing ranges.

use hcl_edit::expr::Expression;
use hcl_edit::repr::Span;
use hcl_edit::structure::{Block, Body};
use lsp_types::{
    FoldingRange, FoldingRangeKind, FoldingRangeParams, Position, Range, SelectionRange,
    SelectionRangeParams,
};
use ropey::Rope;
use tfls_diag::expr_walk::for_each_expression_in;
use tfls_parser::hcl_span_to_lsp_range;
use tower_lsp_server::jsonrpc;

use crate::backend::Backend;

pub async fn folding_range(
    backend: &Backend,
    params: FoldingRangeParams,
) -> jsonrpc::Result<Option<Vec<FoldingRange>>> {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document.uri) else {
        return Ok(None);
    };
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };
    let Some(body) = doc.parsed.body.as_ref() else {
        return Ok(None);
    };

    let mut out = Vec::new();
    collect_block_folds(body, &doc.rope, &mut out);

    // Collapse folds that cover the same line range. `type = object({…})`
    // parses as a FuncCall wrapping an Object; both span identical lines, so
    // the walker emits two identical folds. Duplicate/overlapping ranges
    // confuse clients' nested-fold engines (nvim's foldexpr counts a level
    // per covering range), so keep one fold per `(start_line, end_line)`.
    let mut seen = std::collections::HashSet::new();
    out.retain(|f| seen.insert((f.start_line, f.end_line)));

    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn collect_block_folds(body: &Body, rope: &Rope, out: &mut Vec<FoldingRange>) {
    for structure in body.iter() {
        if let Some(block) = structure.as_block() {
            push_multiline_fold(block.span(), rope, out);
            collect_block_folds(&block.body, rope, out);
        } else if let Some(attr) = structure.as_attribute() {
            // Attribute values can hold multi-line containers (objects,
            // lists, heredocs, func-call args, …) that deserve their own
            // folds — including those inside a `locals` block, whose
            // entries are attributes rather than blocks.
            collect_expr_folds(&attr.value, rope, out);
        }
    }
}

/// Emit a fold for every multi-line container expression nested inside
/// `expr`. Scalar leaves (numbers, strings, idents, traversals, operators)
/// are skipped — only forms that visually enclose multiple lines fold.
fn collect_expr_folds(expr: &Expression, rope: &Rope, out: &mut Vec<FoldingRange>) {
    for_each_expression_in(expr, |e| {
        let is_container = matches!(
            e,
            Expression::Array(_)
                | Expression::Object(_)
                | Expression::HeredocTemplate(_)
                | Expression::StringTemplate(_)
                | Expression::Parenthesis(_)
                | Expression::FuncCall(_)
                | Expression::Conditional(_)
                | Expression::ForExpr(_)
        );
        if is_container {
            push_multiline_fold(e.span(), rope, out);
        }
    });
}

/// Push a `Region` fold for `span` when it covers more than one line.
fn push_multiline_fold(
    span: Option<std::ops::Range<usize>>,
    rope: &Rope,
    out: &mut Vec<FoldingRange>,
) {
    if let Some(range) = span.and_then(|s| hcl_span_to_lsp_range(rope, s).ok()) {
        // Defensively pull the fold end back off any trailing
        // whitespace-only lines the span happens to cover (trailing decor,
        // CRLF, blank lines before the next structure). A fold must never
        // hide the blank separator after a block, or folded blocks render
        // flush with no visible gap between them.
        let end_line = last_content_line(rope, range.start.line, range.end.line);

        // Only fold multi-line spans; a single-line span has nothing to hide.
        if end_line > range.start.line {
            out.push(FoldingRange {
                start_line: range.start.line,
                start_character: Some(range.start.character),
                end_line,
                end_character: None,
                kind: Some(FoldingRangeKind::Region),
                collapsed_text: None,
            });
        }
    }
}

/// Walk `end_line` back toward `start_line` past any whitespace-only lines,
/// returning the last line that carries real content. Clamps at
/// `start_line`.
fn last_content_line(rope: &Rope, start_line: u32, end_line: u32) -> u32 {
    let total = rope.len_lines() as u32;
    let mut e = end_line.min(total.saturating_sub(1));
    while e > start_line {
        let has_content = rope
            .get_line(e as usize)
            .is_some_and(|l| l.chars().any(|c| !c.is_whitespace()));
        if has_content {
            break;
        }
        e -= 1;
    }
    e
}

pub async fn selection_range(
    backend: &Backend,
    params: SelectionRangeParams,
) -> jsonrpc::Result<Option<Vec<SelectionRange>>> {
    let Some(uri) = tfls_core::uri::uri_to_url(&params.text_document.uri) else {
        return Ok(None);
    };
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
