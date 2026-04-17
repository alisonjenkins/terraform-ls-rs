//! `textDocument/documentLink` — produce clickable links from
//! `resource`/`data` block type labels to their provider's registry
//! documentation page.
//!
//! Link targets only point at known providers. If we don't have a
//! schema installed for a given type, we skip the link rather than
//! guess.

use lsp_types::{DocumentLink, DocumentLinkParams, Url};
use tfls_core::ProviderAddress;
use tower_lsp::jsonrpc;

use crate::backend::Backend;

pub async fn document_link(
    backend: &Backend,
    params: DocumentLinkParams,
) -> jsonrpc::Result<Option<Vec<DocumentLink>>> {
    let uri = params.text_document.uri;
    let Some(doc) = backend.state.documents.get(&uri) else {
        return Ok(None);
    };

    let mut out = Vec::new();

    for (addr, sym) in &doc.symbols.resources {
        if let Some(provider) = backend
            .state
            .find_resource_schema(&addr.resource_type)
            .and_then(|p| find_provider_address(&backend.state.schemas, &p))
        {
            if let Some(link) = registry_link(&provider, "resources", &addr.resource_type) {
                out.push(DocumentLink {
                    range: sym.location.range(),
                    target: Some(link),
                    tooltip: Some(format!("Open `{}` docs on registry.terraform.io", addr.resource_type)),
                    data: None,
                });
            }
        }
    }
    for (addr, sym) in &doc.symbols.data_sources {
        if let Some(provider) = backend
            .state
            .find_data_source_schema(&addr.resource_type)
            .and_then(|p| find_provider_address(&backend.state.schemas, &p))
        {
            if let Some(link) = registry_link(&provider, "data-sources", &addr.resource_type) {
                out.push(DocumentLink {
                    range: sym.location.range(),
                    target: Some(link),
                    tooltip: Some(format!("Open `{}` docs on registry.terraform.io", addr.resource_type)),
                    data: None,
                });
            }
        }
    }

    if out.is_empty() { Ok(None) } else { Ok(Some(out)) }
}

/// Find the [`ProviderAddress`] whose schema `Arc` equals the one we
/// looked up. Compares by pointer identity to avoid cloning the whole
/// schema.
fn find_provider_address(
    schemas: &dashmap::DashMap<ProviderAddress, std::sync::Arc<tfls_schema::ProviderSchema>>,
    needle: &std::sync::Arc<tfls_schema::ProviderSchema>,
) -> Option<ProviderAddress> {
    schemas
        .iter()
        .find(|e| std::sync::Arc::ptr_eq(e.value(), needle))
        .map(|e| e.key().clone())
}

fn registry_link(provider: &ProviderAddress, kind: &str, type_name: &str) -> Option<Url> {
    // Only registry.terraform.io is supported for now.
    if provider.hostname != "registry.terraform.io" {
        return None;
    }
    Url::parse(&format!(
        "https://registry.terraform.io/providers/{}/{}/latest/docs/{}/{}",
        provider.namespace, provider.r#type, kind, type_name
    ))
    .ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn registry_link_for_resource() {
        let p = ProviderAddress::hashicorp("aws");
        let link = registry_link(&p, "resources", "aws_instance").expect("link");
        assert_eq!(
            link.as_str(),
            "https://registry.terraform.io/providers/hashicorp/aws/latest/docs/resources/aws_instance"
        );
    }

    #[test]
    fn registry_link_for_data_source() {
        let p = ProviderAddress::hashicorp("aws");
        let link = registry_link(&p, "data-sources", "aws_ami").expect("link");
        assert!(link.as_str().ends_with("/docs/data-sources/aws_ami"));
    }

    #[test]
    fn non_public_registry_returns_none() {
        let p = ProviderAddress::new("private.example.com", "acme", "widget");
        assert!(registry_link(&p, "resources", "acme_widget").is_none());
    }
}
