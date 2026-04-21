//! Text-based best-effort reference extraction for documents
//! whose HCL body failed to parse. Mirrors
//! [`crate::fallback_symbols`] for the reference side.
//!
//! When `hcl-edit` bails on a file (single unclosed brace, typo
//! in an expression) the AST-based [`crate::extract_references`]
//! returns nothing for that file. Every `var.X` / `local.X` /
//! `module.X` / `TYPE.NAME` reference it contained disappears
//! from the workspace index — and rules like
//! `unused_declarations` end up flagging in-use variables as
//! unused because the reference-bearing file is temporarily
//! invisible. This fallback walks the text looking for the
//! canonical Terraform traversal prefixes and emits minimal
//! [`Reference`] entries so downstream rules can still see a
//! partial-parse file's usage.
//!
//! The scan is deliberately coarse:
//!
//! - Respects string literals and both `#` and `//` /
//!   `/* */` comments so a mention of `var.x` inside a comment
//!   or a bare string doesn't produce a false reference.
//! - Looks INSIDE string interpolations (`${ … }`) — a
//!   `"prefix-${var.env}"` expression has `var.env` counted as
//!   a use.
//! - Recognises only the prefix traversal shapes
//!   `classify_traversal` in [`crate::references`] models:
//!   `var.NAME`, `local.NAME`, `module.NAME`,
//!   `data.TYPE.NAME`, and `TYPE.NAME` where `TYPE` contains
//!   an underscore (the resource-type heuristic).
//!
//! Locations are approximate — the emitted range covers the
//! whole `prefix.name[.name]` byte span, which is what the
//! AST extractor does too.

use lsp_types::{Range, Url};
use ropey::Rope;
use tfls_core::SymbolLocation;

use crate::position::byte_offset_to_lsp_position;
use crate::references::{Reference, ReferenceKind};

pub fn extract_references_fallback(source: &str, uri: &Url, rope: &Rope) -> Vec<Reference> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    // Mode = expression (we're reading code, idents count) vs.
    // string (idents are literal text, skip unless we hit an
    // interpolation). Comments are handled as a skip-until-end
    // mode on top of both.
    let mut in_string = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    // When we entered `in_string` via an interpolation (i.e.
    // we're tracking a stack of outer `${` → `}` boundaries),
    // we temporarily allow ident scanning inside the `${ … }`
    // range without closing the string. `interp_depth` tracks
    // open `${` inside the current string.
    let mut interp_depth: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_string && interp_depth == 0 {
            // Plain string-literal bytes. Watch for escape, end
            // of string, or start of interpolation.
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
                i += 1;
                continue;
            }
            if b == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                interp_depth = 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        // Either we're outside a string, or inside an
        // interpolation within a string. Both permit ident
        // scanning. Interpolations additionally need to track
        // brace nesting so we know when to revert to string
        // mode.
        if interp_depth > 0 {
            match b {
                b'{' => {
                    interp_depth += 1;
                    i += 1;
                    continue;
                }
                b'}' => {
                    interp_depth -= 1;
                    i += 1;
                    continue;
                }
                _ => {}
            }
        }
        match b {
            b'"' if !in_string => {
                in_string = true;
                i += 1;
            }
            b'#' => {
                in_line_comment = true;
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                in_line_comment = true;
                i += 2;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                in_block_comment = true;
                i += 2;
            }
            c if is_ident_start(c) => {
                let start = i;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let first = &source[start..i];
                let mut segments: Vec<(usize, usize)> = Vec::new();
                while i < bytes.len() && bytes[i] == b'.' {
                    i += 1;
                    let seg_start = i;
                    while i < bytes.len() && is_ident_continue(bytes[i]) {
                        i += 1;
                    }
                    if seg_start == i {
                        // `.` not followed by an identifier —
                        // stop collecting. Cursor sits past the
                        // dot; outer loop will handle the next
                        // byte.
                        break;
                    }
                    segments.push((seg_start, i));
                }
                if let Some(kind) = classify(first, &segments, source) {
                    let whole_end = segments.last().map(|s| s.1).unwrap_or(start);
                    if let Some(range) = range_from_bytes(rope, start, whole_end) {
                        out.push(Reference {
                            kind,
                            location: SymbolLocation::new(uri.clone(), range),
                        });
                    }
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

fn classify(first: &str, segments: &[(usize, usize)], source: &str) -> Option<ReferenceKind> {
    let seg = |idx: usize| -> Option<&str> {
        segments.get(idx).map(|(s, e)| &source[*s..*e])
    };
    match first {
        "var" => seg(0).map(|n| ReferenceKind::Variable { name: n.to_string() }),
        "local" => seg(0).map(|n| ReferenceKind::Local { name: n.to_string() }),
        "module" => seg(0).map(|n| ReferenceKind::Module { name: n.to_string() }),
        "data" => {
            let ty = seg(0)?;
            let name = seg(1)?;
            Some(ReferenceKind::DataSource {
                resource_type: ty.to_string(),
                name: name.to_string(),
            })
        }
        // Built-in traversal prefixes that look like idents but
        // don't point at declared symbols — skip to avoid
        // polluting `references_by_name` with noise.
        "path" | "terraform" | "each" | "count" | "self" => None,
        ty if is_resource_type_heuristic(ty) => {
            let name = seg(0)?;
            Some(ReferenceKind::Resource {
                resource_type: ty.to_string(),
                name: name.to_string(),
            })
        }
        _ => None,
    }
}

/// A bare identifier with at least one underscore looks like a
/// Terraform resource type (e.g. `aws_instance`,
/// `google_sql_database`). Matches
/// [`crate::references::is_resource_type`] heuristic.
fn is_resource_type_heuristic(s: &str) -> bool {
    s.contains('_') && s.bytes().next().is_some_and(is_ident_start)
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn range_from_bytes(rope: &Rope, start_byte: usize, end_byte: usize) -> Option<Range> {
    let start = byte_offset_to_lsp_position(rope, start_byte).ok()?;
    let end = byte_offset_to_lsp_position(rope, end_byte).ok()?;
    Some(Range { start, end })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///t.tf").expect("url")
    }

    fn refs(src: &str) -> Vec<Reference> {
        let rope = Rope::from_str(src);
        extract_references_fallback(src, &uri(), &rope)
    }

    #[test]
    fn finds_var_reference() {
        let r = refs("identifiers = var.admin_users\n");
        assert!(
            r.iter()
                .any(|r| matches!(&r.kind, ReferenceKind::Variable { name } if name == "admin_users")),
            "got: {r:?}"
        );
    }

    #[test]
    fn finds_local_reference() {
        let r = refs("value = local.region\n");
        assert!(r
            .iter()
            .any(|r| matches!(&r.kind, ReferenceKind::Local { name } if name == "region")));
    }

    #[test]
    fn finds_module_reference() {
        let r = refs("value = module.network.subnet_id\n");
        assert!(r
            .iter()
            .any(|r| matches!(&r.kind, ReferenceKind::Module { name } if name == "network")));
    }

    #[test]
    fn finds_data_reference() {
        let r = refs("value = data.aws_ami.ubuntu.id\n");
        assert!(r.iter().any(|r| matches!(
            &r.kind,
            ReferenceKind::DataSource { resource_type, name }
                if resource_type == "aws_ami" && name == "ubuntu"
        )));
    }

    #[test]
    fn finds_resource_reference() {
        let r = refs("value = aws_instance.web.id\n");
        assert!(r.iter().any(|r| matches!(
            &r.kind,
            ReferenceKind::Resource { resource_type, name }
                if resource_type == "aws_instance" && name == "web"
        )));
    }

    #[test]
    fn ignores_reference_in_double_quoted_string() {
        let r = refs("description = \"remember to set var.region\"\n");
        assert!(r.is_empty(), "string-literal text should not match: {r:?}");
    }

    #[test]
    fn ignores_reference_in_line_comment() {
        let r = refs("# var.x is documented elsewhere\nuse = something\n");
        assert!(r.is_empty(), "line-comment text should not match: {r:?}");
    }

    #[test]
    fn ignores_reference_in_block_comment() {
        let r = refs("/* see var.x */\nuse = something\n");
        assert!(r.is_empty(), "block-comment text should not match: {r:?}");
    }

    #[test]
    fn finds_reference_inside_interpolation() {
        // `${var.x}` in a string must produce a reference.
        let r = refs("name = \"prefix-${var.env}\"\n");
        assert!(
            r.iter()
                .any(|r| matches!(&r.kind, ReferenceKind::Variable { name } if name == "env")),
            "interpolation ref missed: {r:?}"
        );
    }

    #[test]
    fn finds_reference_inside_nested_interpolation() {
        // `${foo(${var.x})}` — interpolation inside
        // interpolation. `${` nesting depth is tracked, so
        // var.x should still be found.
        let r = refs("name = \"${upper(var.env)}\"\n");
        assert!(r
            .iter()
            .any(|r| matches!(&r.kind, ReferenceKind::Variable { name } if name == "env")));
    }

    #[test]
    fn survives_file_with_trailing_parse_error() {
        let r = refs(
            "resource \"aws_iam_role\" \"r\" {\n  identifiers = var.admin_users\n}\n\nresource \"x\" \"y\" {\n  broken = {\n",
        );
        assert!(r
            .iter()
            .any(|r| matches!(&r.kind, ReferenceKind::Variable { name } if name == "admin_users")));
    }

    #[test]
    fn ignores_builtin_prefixes() {
        let r = refs("x = path.module\ny = terraform.workspace\nz = each.key\n");
        assert!(r.is_empty(), "builtin prefixes leaked: {r:?}");
    }
}
