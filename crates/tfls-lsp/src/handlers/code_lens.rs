//! `textDocument/codeLens` — annotate each definition with its
//! reference count, clickable to invoke `textDocument/references`.

use lsp_types::{CodeLens, CodeLensParams, Command};
use tfls_core::{ResourceAddress, Symbol, SymbolKind, SymbolVisitor};
use tfls_state::{DocumentState, StateStore, SymbolKey};
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn code_lens(
    backend: &Backend,
    params: CodeLensParams,
) -> jsonrpc::Result<Option<Vec<CodeLens>>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    let mut out = Vec::new();
    collect(&doc, &backend.state, &mut out);
    if out.is_empty() { Ok(None) } else { Ok(Some(out)) }
}

fn collect(doc: &DocumentState, state: &StateStore, out: &mut Vec<CodeLens>) {
    let mut v = LensCollector { state, out };
    doc.symbols.for_each_symbol(&mut v);
}

struct LensCollector<'a> {
    state: &'a StateStore,
    out: &'a mut Vec<CodeLens>,
}

impl<'a> SymbolVisitor for LensCollector<'a> {
    fn visit(&mut self, sym: &Symbol) {
        // Providers have no reference-count lens today — skip
        // them rather than emitting "0 references" clutter.
        let key = match sym.kind {
            SymbolKind::Variable
            | SymbolKind::Local
            | SymbolKind::Output
            | SymbolKind::Module => SymbolKey::new(sym.kind, &sym.name),
            _ => return,
        };
        push_lens(sym, key, self.state, self.out);
    }

    fn visit_resource(&mut self, addr: &ResourceAddress, sym: &Symbol) {
        let key = SymbolKey::resource(sym.kind, &addr.resource_type, &addr.name);
        push_lens(sym, key, self.state, self.out);
    }
}

fn push_lens(sym: &Symbol, key: SymbolKey, state: &StateStore, out: &mut Vec<CodeLens>) {
    let count = state
        .references_by_name
        .get(&key)
        .map(|v| v.len())
        .unwrap_or(0);
    let label = match count {
        0 => "0 references".to_string(),
        1 => "1 reference".to_string(),
        n => format!("{n} references"),
    };
    out.push(CodeLens {
        range: sym.location.range(),
        command: Some(Command {
            title: label,
            command: "editor.action.showReferences".to_string(),
            arguments: None,
        }),
        data: None,
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::Url;

    fn uri() -> Url {
        Url::parse("file:///t.tf").expect("url")
    }

    #[test]
    fn no_references_produces_zero_count_label() {
        let state = StateStore::new();
        state.upsert_document(DocumentState::new(uri(), r#"variable "a" {}"#, 1));
        let doc = state.documents.get(&uri()).unwrap();
        let mut out = Vec::new();
        collect(&doc, &state, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].command.as_ref().expect("cmd").title,
            "0 references"
        );
    }

    #[test]
    fn counts_references_across_document() {
        let state = StateStore::new();
        state.upsert_document(DocumentState::new(
            uri(),
            r#"variable "x" {}
output "a" { value = var.x }
output "b" { value = var.x }
output "c" { value = var.x }
"#,
            1,
        ));
        let doc = state.documents.get(&uri()).unwrap();
        let mut out = Vec::new();
        collect(&doc, &state, &mut out);
        let lens = out.iter().find(|l| {
            l.command
                .as_ref()
                .map(|c| c.title.contains("references"))
                .unwrap_or(false)
        });
        let title = &lens.expect("lens").command.as_ref().unwrap().title;
        assert_eq!(title, "3 references");
    }

    #[test]
    fn singular_label_when_one_reference() {
        let state = StateStore::new();
        state.upsert_document(DocumentState::new(
            uri(),
            r#"variable "x" {}
output "a" { value = var.x }
"#,
            1,
        ));
        let doc = state.documents.get(&uri()).unwrap();
        let mut out = Vec::new();
        collect(&doc, &state, &mut out);
        let lens = out.iter().find(|l| {
            l.command
                .as_ref()
                .map(|c| c.title == "1 reference")
                .unwrap_or(false)
        });
        assert!(lens.is_some(), "expected singular form");
    }
}
