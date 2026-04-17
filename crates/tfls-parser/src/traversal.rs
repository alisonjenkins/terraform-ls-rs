//! Walk an hcl-edit [`Body`] to extract top-level Terraform symbols.
//!
//! Terraform source files consist of blocks like `resource`, `variable`,
//! `output`, `data`, `module`, `provider`, `locals`, `terraform`. This
//! module maps those block shapes into our domain [`SymbolTable`].

use hcl_edit::Ident;
use hcl_edit::repr::{Decorated, Span};
use hcl_edit::structure::{Block, BlockLabel, Body};
use lsp_types::Url;
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
            "variable" => insert_labeled(block, uri, rope, SymbolKind::Variable, |sym, name| {
                table.variables.insert(name, sym);
            }),
            "output" => insert_labeled(block, uri, rope, SymbolKind::Output, |sym, name| {
                table.outputs.insert(name, sym);
            }),
            "module" => insert_labeled(block, uri, rope, SymbolKind::Module, |sym, name| {
                table.modules.insert(name, sym);
            }),
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
                if let Some(sym) = build_symbol(
                    "terraform",
                    SymbolKind::TerraformBlock,
                    block,
                    uri,
                    rope,
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

fn first_label(block: &Block) -> Option<&str> {
    block.labels.first().map(label_str)
}

fn label_str(label: &BlockLabel) -> &str {
    match label {
        BlockLabel::String(s) => s.value().as_str(),
        BlockLabel::Ident(i) => i.as_str(),
    }
}

fn insert_labeled(
    block: &Block,
    uri: &Url,
    rope: &Rope,
    kind: SymbolKind,
    mut insert: impl FnMut(Symbol, String),
) {
    let Some(name) = first_label(block) else {
        return;
    };
    let name_owned = name.to_string();
    if let Some(sym) = build_symbol(&name_owned, kind, block, uri, rope, None) {
        insert(sym, name_owned);
    }
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
    let detail = Some(format!("{type_}.{name}"));
    if let Some(sym) = build_symbol(&name, kind, block, uri, rope, detail) {
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
        let Some(span) = attr.span() else { continue };
        let Ok(range) = hcl_span_to_lsp_range(rope, span) else {
            continue;
        };
        let sym = Symbol {
            name: name.clone(),
            kind: SymbolKind::Local,
            location: SymbolLocation::new(uri.clone(), range),
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
    detail: Option<String>,
) -> Option<Symbol> {
    let span = block.span()?;
    let range = hcl_span_to_lsp_range(rope, span).ok()?;
    Some(Symbol {
        name: name.to_string(),
        kind,
        location: SymbolLocation::new(uri.clone(), range),
        detail,
        doc: None,
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
