//! Text-based best-effort symbol extraction used when HCL parsing
//! fails entirely. A single syntax error in a variable's `type`
//! expression currently collapses the whole body to `None`, which
//! made every reference to any variable in that file light up as
//! "undefined" across the module. That's a terrible UX — one
//! typo can produce dozens of bogus squiggles.
//!
//! This scanner walks the source text and recognises the canonical
//! block-header shapes (`variable "NAME"`, `output "NAME"`,
//! `data "TYPE" "NAME"`, `module "NAME"`, `locals {`, `provider "NAME"`)
//! at depth 0 (not nested inside another block's body). Extracted
//! names get minimal [`Symbol`] entries so downstream
//! undefined-reference checks can see them. Per-attr details (type,
//! default shape, etc.) are skipped because parsing expressions
//! without `hcl-edit` is out of scope — the main point is to keep
//! the symbol table populated.

use lsp_types::{Position, Range, Url};
use ropey::Rope;
use tfls_core::{ResourceAddress, Symbol, SymbolKind, SymbolLocation, SymbolTable};

/// Scan `source` for top-level symbol declarations and populate a
/// [`SymbolTable`]. Respects string literals and both `#` and `//`
/// comments so a `variable "x"` hidden inside those doesn't get
/// extracted.
pub fn extract_symbols_fallback(source: &str, uri: &Url, rope: &Rope) -> SymbolTable {
    let mut table = SymbolTable::default();
    let bytes = source.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
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
        if in_string {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
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
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = (depth - 1).max(0);
                i += 1;
            }
            c if depth == 0 && (c.is_ascii_alphabetic() || c == b'_') => {
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let keyword = &source[start..i];
                match keyword {
                    "variable" | "output" | "module" | "provider" => {
                        if let Some((name, name_range)) = read_first_label(bytes, source, i) {
                            let sym = build_symbol(
                                match keyword {
                                    "variable" => SymbolKind::Variable,
                                    "output" => SymbolKind::Output,
                                    "module" => SymbolKind::Module,
                                    "provider" => SymbolKind::Provider,
                                    _ => unreachable!(),
                                },
                                &name,
                                uri,
                                rope,
                                &name_range,
                            );
                            match keyword {
                                "variable" => {
                                    table.variables.insert(name, sym);
                                }
                                "output" => {
                                    table.outputs.insert(name, sym);
                                }
                                "module" => {
                                    table.modules.insert(name, sym);
                                }
                                "provider" => {
                                    table.providers.insert(name, sym);
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                    "data" => {
                        if let Some((type_name, _)) = read_first_label(bytes, source, i) {
                            let after_first = skip_first_label(bytes, i);
                            if let Some((name, name_range)) =
                                read_first_label(bytes, source, after_first)
                            {
                                let sym = build_symbol(
                                    SymbolKind::DataSource,
                                    &name,
                                    uri,
                                    rope,
                                    &name_range,
                                );
                                let addr = ResourceAddress::new(&type_name, &name);
                                table.data_sources.insert(addr, sym);
                            }
                        }
                    }
                    "resource" => {
                        if let Some((type_name, _)) = read_first_label(bytes, source, i) {
                            let after_first = skip_first_label(bytes, i);
                            if let Some((name, name_range)) =
                                read_first_label(bytes, source, after_first)
                            {
                                let sym = build_symbol(
                                    SymbolKind::Resource,
                                    &name,
                                    uri,
                                    rope,
                                    &name_range,
                                );
                                let addr = ResourceAddress::new(&type_name, &name);
                                table.resources.insert(addr, sym);
                            }
                        }
                    }
                    "locals" => {
                        let mut j = i;
                        while j < bytes.len() && bytes[j] != b'{' {
                            j += 1;
                        }
                        if j < bytes.len() {
                            scan_locals_body(source, bytes, j + 1, uri, rope, &mut table);
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    table
}

fn build_symbol(
    kind: SymbolKind,
    name: &str,
    uri: &Url,
    rope: &Rope,
    name_span: &std::ops::Range<usize>,
) -> Symbol {
    let range = span_to_range(rope, name_span);
    Symbol {
        name: name.to_string(),
        kind,
        location: SymbolLocation::new(uri.clone(), range),
        name_range: range,
        detail: None,
        doc: None,
    }
}

/// After reading a block ident like `variable`, read the next
/// quoted string and return it with its byte-span within `source`.
fn read_first_label(
    bytes: &[u8],
    source: &str,
    from: usize,
) -> Option<(String, std::ops::Range<usize>)> {
    let mut i = from;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'"' {
        return None;
    }
    let start = i + 1;
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'"' => {
                let end = i;
                let name = source[start..end].to_string();
                return Some((name, start..end));
            }
            _ => i += 1,
        }
    }
    None
}

/// Skip past the first label so a second `read_first_label` call
/// lands on the next label. Used for `data "TYPE" "NAME"`.
fn skip_first_label(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'"' {
        return from;
    }
    i += 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'"' => {
                return i + 1;
            }
            _ => i += 1,
        }
    }
    from
}

/// Scan attribute assignments inside a `locals { … }` body starting
/// at byte offset `from` (just past the `{`). Depth tracking so we
/// don't pick up nested-map keys as local names.
fn scan_locals_body(
    source: &str,
    bytes: &[u8],
    from: usize,
    uri: &Url,
    rope: &Rope,
    table: &mut SymbolTable,
) {
    let mut depth: i32 = 1;
    let mut i = from;
    let mut at_key_position = true;
    let mut in_string = false;
    let mut in_line_comment = false;
    while i < bytes.len() && depth > 0 {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
                at_key_position = true;
            }
            i += 1;
            continue;
        }
        if in_string {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'{' | b'[' | b'(' => {
                depth += 1;
                i += 1;
                at_key_position = false;
            }
            b'}' | b']' | b')' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                in_string = true;
                i += 1;
            }
            b'#' => {
                in_line_comment = true;
                i += 1;
            }
            b'\n' | b',' if depth == 1 => {
                at_key_position = true;
                i += 1;
            }
            c if depth == 1 && at_key_position && (c.is_ascii_alphabetic() || c == b'_') => {
                let start = i;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let name = source[start..i].to_string();
                let mut j = i;
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    let sym = build_symbol(
                        SymbolKind::Local,
                        &name,
                        uri,
                        rope,
                        &(start..i),
                    );
                    table.locals.insert(name, sym);
                }
                at_key_position = false;
                i = j;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
            }
            _ => {
                at_key_position = false;
                i += 1;
            }
        }
    }
}

fn span_to_range(rope: &Rope, span: &std::ops::Range<usize>) -> Range {
    let start = byte_to_lsp(rope, span.start);
    let end = byte_to_lsp(rope, span.end);
    Range { start, end }
}

fn byte_to_lsp(rope: &Rope, byte: usize) -> Position {
    let clamped = byte.min(rope.len_bytes());
    let char_idx = rope.byte_to_char(clamped);
    let line = rope.char_to_line(char_idx);
    let line_start_char = rope.line_to_char(line);
    Position {
        line: line as u32,
        character: (char_idx - line_start_char) as u32,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///t.tf").expect("url")
    }

    fn extract(src: &str) -> SymbolTable {
        extract_symbols_fallback(src, &uri(), &Rope::from_str(src))
    }

    #[test]
    fn extracts_variables_even_from_broken_file() {
        // First variable has a syntax error in its type expression;
        // normal HCL parsing would produce body=None. Fallback should
        // still pick up both variable names.
        let src = r#"
variable "bad" {
  type = object({
    map(object({ x = string })
  })
}

variable "good" {
  type = string
}
"#;
        let t = extract(src);
        assert!(
            t.variables.contains_key("bad"),
            "got: {:?}",
            t.variables.keys().collect::<Vec<_>>()
        );
        assert!(t.variables.contains_key("good"));
    }

    #[test]
    fn extracts_outputs_and_modules() {
        let src = r#"
output "x" { value = 1 }
module "m" { source = "./x" }
"#;
        let t = extract(src);
        assert!(t.outputs.contains_key("x"));
        assert!(t.modules.contains_key("m"));
    }

    #[test]
    fn extracts_data_sources() {
        let src = r#"
data "aws_ami" "ubuntu" {}
"#;
        let t = extract(src);
        let addr = ResourceAddress::new("aws_ami", "ubuntu");
        assert!(t.data_sources.contains_key(&addr));
    }

    #[test]
    fn extracts_locals() {
        let src = r#"
locals {
  a = 1
  b = "hello"
  c = { nested = true }
}
"#;
        let t = extract(src);
        assert!(t.locals.contains_key("a"));
        assert!(t.locals.contains_key("b"));
        assert!(t.locals.contains_key("c"));
        assert!(!t.locals.contains_key("nested"));
    }

    #[test]
    fn skips_content_in_comments_and_strings() {
        let src = r#"
# variable "commented_out" {}
// variable "slashed_out" {}
output "with_template" {
  value = "variable \"not_a_decl\" {}"
}
"#;
        let t = extract(src);
        assert!(!t.variables.contains_key("commented_out"));
        assert!(!t.variables.contains_key("slashed_out"));
        assert!(!t.variables.contains_key("not_a_decl"));
        assert!(t.outputs.contains_key("with_template"));
    }

    #[test]
    fn ignores_block_inside_another_block() {
        let src = r#"
resource "aws_thing" "x" {
  variable = "a-value"
  something {
    variable "not_real" {}
  }
}
variable "real" {}
"#;
        let t = extract(src);
        assert!(t.variables.contains_key("real"));
        assert!(!t.variables.contains_key("not_real"));
    }
}
