//! Walk an hcl-edit [`Body`] to extract top-level Terraform symbols.
//!
//! Terraform source files consist of blocks like `resource`, `variable`,
//! `output`, `data`, `module`, `provider`, `locals`, `terraform`. This
//! module maps those block shapes into our domain [`SymbolTable`].

use hcl_edit::Ident;
use hcl_edit::repr::{Decorated, Span};
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::{Range, Url};
use ropey::Rope;
use tfls_core::{ResourceAddress, Symbol, SymbolKind, SymbolLocation, SymbolTable};

use crate::position::hcl_span_to_lsp_range;

/// Extract the symbol table from a parsed [`Body`].
///
/// Spans from hcl-edit are converted to LSP ranges via `rope`. Blocks
/// whose spans cannot be resolved are skipped (they shouldn't appear in
/// practice since the parser assigns spans to everything).
pub fn extract_symbols(body: &Body, uri: &Url, rope: &Rope) -> SymbolTable {
    let mut table = SymbolTable::new();

    for structure in body.iter() {
        let block = match structure.as_block() {
            Some(b) => b,
            None => continue,
        };
        let ident = block.ident.as_str();
        match ident {
            "variable" => {
                let type_expr = block.body.iter().find_map(|structure| {
                    let attr = structure.as_attribute()?;
                    if attr.key.as_str() == "type" {
                        Some(tfls_core::parse_type_expr(&attr.value))
                    } else {
                        None
                    }
                });
                insert_labeled(block, uri, rope, SymbolKind::Variable, |sym, name| {
                    if let Some(ty) = type_expr.clone() {
                        table.variable_types.insert(name.clone(), ty);
                    }
                    table.variables.insert(name, sym);
                });
            }
            "output" => insert_labeled(block, uri, rope, SymbolKind::Output, |sym, name| {
                table.outputs.insert(name, sym);
            }),
            "module" => {
                let source = string_attribute(block, "source");
                insert_labeled(block, uri, rope, SymbolKind::Module, |sym, name| {
                    if let Some(src) = source.clone() {
                        table.module_sources.insert(name.clone(), src);
                    }
                    table.modules.insert(name, sym);
                });
            }
            "provider" => insert_labeled(block, uri, rope, SymbolKind::Provider, |sym, name| {
                table.providers.insert(name, sym);
            }),
            "resource" => insert_two_labeled(block, uri, rope, SymbolKind::Resource, |sym, t, n| {
                table.resources.insert(ResourceAddress::new(t, n), sym);
            }),
            "data" => insert_two_labeled(block, uri, rope, SymbolKind::DataSource, |sym, t, n| {
                table.data_sources.insert(ResourceAddress::new(t, n), sym);
            }),
            "locals" => extract_locals(block, uri, rope, &mut table),
            "terraform" => {
                let Some(name_range) = block
                    .ident
                    .span()
                    .and_then(|s| hcl_span_to_lsp_range(rope, s).ok())
                else {
                    continue;
                };
                if let Some(sym) = build_symbol(
                    "terraform",
                    SymbolKind::TerraformBlock,
                    block,
                    uri,
                    rope,
                    name_range,
                    None,
                    None,
                ) {
                    table
                        .providers
                        .entry("_terraform".to_string())
                        .or_insert(sym);
                }
            }
            _ => {}
        }
    }

    table
}

fn label_str(label: &BlockLabel) -> &str {
    match label {
        BlockLabel::String(s) => s.value().as_str(),
        BlockLabel::Ident(i) => i.as_str(),
    }
}

fn label_span(label: &BlockLabel) -> Option<std::ops::Range<usize>> {
    match label {
        BlockLabel::String(s) => s.span(),
        BlockLabel::Ident(i) => i.span(),
    }
}

fn label_range(label: &BlockLabel, rope: &Rope) -> Option<Range> {
    hcl_span_to_lsp_range(rope, label_span(label)?).ok()
}

fn insert_labeled(
    block: &Block,
    uri: &Url,
    rope: &Rope,
    kind: SymbolKind,
    mut insert: impl FnMut(Symbol, String),
) {
    let Some(label) = block.labels.first() else {
        return;
    };
    let Some(name_range) = label_range(label, rope) else {
        return;
    };
    let name_owned = label_str(label).to_string();
    let doc = string_attribute(block, "description");
    if let Some(sym) = build_symbol(&name_owned, kind, block, uri, rope, name_range, None, doc) {
        insert(sym, name_owned);
    }
}

/// Read a plain-string attribute value (e.g. `description = "…"` or
/// `source = "./foo"`) from a block body. Returns `None` when the
/// attribute is missing or its value isn't a simple string literal.
fn string_attribute(block: &Block, key: &str) -> Option<String> {
    for structure in block.body.iter() {
        let Some(attr) = structure.as_attribute() else {
            continue;
        };
        if attr.key.as_str() != key {
            continue;
        }
        if let hcl_edit::expr::Expression::String(s) = &attr.value {
            return Some(s.value().to_string());
        }
    }
    None
}

fn insert_two_labeled(
    block: &Block,
    uri: &Url,
    rope: &Rope,
    kind: SymbolKind,
    mut insert: impl FnMut(Symbol, String, String),
) {
    let labels = &block.labels;
    if labels.len() < 2 {
        return;
    }
    let type_ = label_str(&labels[0]).to_string();
    let name = label_str(&labels[1]).to_string();
    // Highlight the *type* label — matches what the semantic-tokens
    // encoder emits as `TYPE` for resource/data blocks.
    let Some(name_range) = label_range(&labels[0], rope) else {
        return;
    };
    let detail = Some(format!("{type_}.{name}"));
    if let Some(sym) = build_symbol(&name, kind, block, uri, rope, name_range, detail, None) {
        insert(sym, type_, name);
    }
}

fn extract_locals(block: &Block, uri: &Url, rope: &Rope, table: &mut SymbolTable) {
    for structure in block.body.iter() {
        let attr = match structure.as_attribute() {
            Some(a) => a,
            None => continue,
        };
        let name = attr.key.as_str().to_string();
        let Some(attr_span) = attr.span() else { continue };
        let Ok(location_range) = hcl_span_to_lsp_range(rope, attr_span) else {
            continue;
        };
        let Some(key_span) = attr.key.span() else {
            continue;
        };
        let Ok(name_range) = hcl_span_to_lsp_range(rope, key_span) else {
            continue;
        };
        let sym = Symbol {
            name: name.clone(),
            kind: SymbolKind::Local,
            location: SymbolLocation::new(uri.clone(), location_range),
            name_range,
            detail: None,
            doc: None,
        };
        table.locals.insert(name, sym);
    }
}

fn build_symbol(
    name: &str,
    kind: SymbolKind,
    block: &Block,
    uri: &Url,
    rope: &Rope,
    name_range: Range,
    detail: Option<String>,
    doc: Option<String>,
) -> Option<Symbol> {
    let span = block.span()?;
    let range = hcl_span_to_lsp_range(rope, span).ok()?;
    Some(Symbol {
        name: name.to_string(),
        kind,
        location: SymbolLocation::new(uri.clone(), range),
        name_range,
        detail,
        doc,
    })
}

/// Borrow an identifier's string — convenience for external callers.
pub fn ident_str(ident: &Decorated<Ident>) -> &str {
    ident.as_str()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::parse::parse_source;

    fn uri() -> Url {
        Url::parse("file:///test.tf").expect("valid url")
    }

    fn extract(src: &str) -> SymbolTable {
        let rope = Rope::from_str(src);
        let parsed = parse_source(src);
        let body = parsed.body.expect("should parse");
        extract_symbols(&body, &uri(), &rope)
    }

    #[test]
    fn extracts_variables() {
        let table = extract(r#"variable "region" { default = "us-east-1" }"#);
        assert_eq!(table.variables.len(), 1);
        assert!(table.variables.contains_key("region"));
        let sym = &table.variables["region"];
        assert_eq!(sym.kind, SymbolKind::Variable);
        assert_eq!(sym.name, "region");
    }

    // --- name_range regressions ---------------------------------------
    //
    // Semantic tokens need the narrow range of just the label, not the
    // whole block — otherwise the highlight gets anchored to the
    // keyword and produces the visible colour-split bug users see on
    // resource/data/module names.

    fn column_span(s: &str, needle: &str) -> (u32, u32) {
        let start = s.find(needle).expect("needle not in source");
        (start as u32, (start + needle.len()) as u32)
    }

    #[test]
    fn name_range_for_variable_covers_just_the_label() {
        let src = r#"variable "region" { default = "us-east-1" }"#;
        let table = extract(src);
        let sym = &table.variables["region"];
        // Whole-block location still starts at the `v` of `variable`.
        assert_eq!(sym.location.range().start.character, 0);
        // name_range points at the `"region"` literal — anywhere inside
        // the quoted span, so long as it is *not* the block keyword.
        assert!(sym.name_range.start.character > 0,
            "expected name_range to start past the keyword, got {}", sym.name_range.start.character);
        let (lo, hi) = column_span(src, "region");
        // The range must cover the `region` text (inclusive of or
        // excluding surrounding quotes — both are legitimate spans).
        assert!(sym.name_range.start.character <= lo,
            "name_range starts at {} but `region` begins at {}", sym.name_range.start.character, lo);
        assert!(sym.name_range.end.character >= hi,
            "name_range ends at {} but `region` ends at {}", sym.name_range.end.character, hi);
    }

    #[test]
    fn name_range_for_resource_covers_the_type_label() {
        let src = r#"resource "aws_security_group_rule" "test" { }"#;
        let table = extract(src);
        let sym = &table.resources[&ResourceAddress::new("aws_security_group_rule", "test")];
        assert_eq!(sym.location.range().start.character, 0);
        let (lo, hi) = column_span(src, "aws_security_group_rule");
        assert!(sym.name_range.start.character <= lo);
        assert!(sym.name_range.end.character >= hi);
        // Crucially, the range must not extend into or past the name
        // label — that was the bug where `"test"` got mis-coloured.
        let (name_lo, _) = column_span(src, "\"test\"");
        assert!(sym.name_range.end.character <= name_lo,
            "name_range leaks past the type label into the name label");
    }

    #[test]
    fn name_range_for_data_source_covers_the_type_label() {
        let src = r#"data "aws_ami" "ubuntu" { owners = ["x"] }"#;
        let table = extract(src);
        let sym = &table.data_sources[&ResourceAddress::new("aws_ami", "ubuntu")];
        let (lo, hi) = column_span(src, "aws_ami");
        assert!(sym.name_range.start.character <= lo);
        assert!(sym.name_range.end.character >= hi);
        let (name_lo, _) = column_span(src, "\"ubuntu\"");
        assert!(sym.name_range.end.character <= name_lo);
    }

    #[test]
    fn name_range_for_module_covers_the_label() {
        let src = r#"module "network" { source = "./x" }"#;
        let table = extract(src);
        let sym = &table.modules["network"];
        assert!(sym.name_range.start.character > 0);
        let (lo, hi) = column_span(src, "network");
        assert!(sym.name_range.start.character <= lo);
        assert!(sym.name_range.end.character >= hi);
    }

    #[test]
    fn name_range_for_local_covers_the_key() {
        let src = "locals {\n  region = \"us-east-1\"\n}\n";
        let table = extract(src);
        let sym = &table.locals["region"];
        assert_eq!(sym.name_range.start.line, 1);
        // The `region` key starts at column 2 (two-space indent).
        assert_eq!(sym.name_range.start.character, 2);
        assert_eq!(sym.name_range.end.character, 2 + "region".len() as u32);
    }

    #[test]
    fn name_range_stays_on_a_single_line() {
        // Even for a multi-line block body, the name_range must not
        // leak across lines.
        let src = "resource \"aws_instance\" \"web\" {\n  ami = \"x\"\n}\n";
        let table = extract(src);
        let sym = &table.resources[&ResourceAddress::new("aws_instance", "web")];
        assert_eq!(sym.name_range.start.line, 0);
        assert_eq!(sym.name_range.end.line, 0);
    }

    #[test]
    fn extracts_outputs() {
        let table = extract(r#"output "api_url" { value = "x" }"#);
        assert_eq!(table.outputs.len(), 1);
        assert_eq!(table.outputs["api_url"].kind, SymbolKind::Output);
    }

    #[test]
    fn extracts_resources() {
        let table = extract(
            r#"
resource "aws_instance" "web" { ami = "ami-123" }
resource "aws_instance" "api" { ami = "ami-123" }
"#,
        );
        assert_eq!(table.resources.len(), 2);
        assert!(
            table
                .resources
                .contains_key(&ResourceAddress::new("aws_instance", "web"))
        );
        assert!(
            table
                .resources
                .contains_key(&ResourceAddress::new("aws_instance", "api"))
        );
    }

    #[test]
    fn extracts_data_sources() {
        let table = extract(r#"data "aws_ami" "ubuntu" { owners = ["099720109477"] }"#);
        assert_eq!(table.data_sources.len(), 1);
        let sym = &table.data_sources[&ResourceAddress::new("aws_ami", "ubuntu")];
        assert_eq!(sym.kind, SymbolKind::DataSource);
    }

    #[test]
    fn extracts_modules() {
        let table = extract(r#"module "network" { source = "./modules/network" }"#);
        assert_eq!(table.modules.len(), 1);
        assert_eq!(table.modules["network"].kind, SymbolKind::Module);
    }

    #[test]
    fn extracts_module_source() {
        let table = extract(r#"module "x" { source = "./foo" }"#);
        assert_eq!(table.module_sources.get("x"), Some(&"./foo".to_string()));
    }

    #[test]
    fn skips_module_source_when_non_string() {
        let table = extract(r#"module "x" { source = var.path }"#);
        assert!(table.module_sources.get("x").is_none());
    }

    #[test]
    fn extracts_variable_description() {
        let table = extract(r#"variable "region" { description = "AWS region" }"#);
        assert_eq!(
            table.variables["region"].doc.as_deref(),
            Some("AWS region")
        );
    }

    #[test]
    fn extracts_output_description() {
        let table = extract(
            "output \"url\" {\n  description = \"Public URL\"\n  value = \"x\"\n}\n",
        );
        assert_eq!(table.outputs["url"].doc.as_deref(), Some("Public URL"));
    }

    #[test]
    fn extracts_locals() {
        let table = extract(
            r#"
locals {
  region = "us-east-1"
  name   = "app"
}
"#,
        );
        assert_eq!(table.locals.len(), 2);
        assert!(table.locals.contains_key("region"));
        assert!(table.locals.contains_key("name"));
        assert_eq!(table.locals["region"].kind, SymbolKind::Local);
    }

    #[test]
    fn empty_body_yields_empty_table() {
        let table = extract("");
        assert!(table.is_empty());
    }

    #[test]
    fn skips_resources_missing_labels() {
        let src = r#"variable "x" { default = 1 }"#;
        let table = extract(src);
        // exactly one variable, no resources
        assert_eq!(table.variables.len(), 1);
        assert_eq!(table.resources.len(), 0);
    }
}
