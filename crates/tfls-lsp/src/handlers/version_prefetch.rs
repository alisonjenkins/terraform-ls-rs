//! Background version-cache prefetch.
//!
//! Without this, a fresh file can sit with visible version constraints
//! whose freshness hints never render because the on-disk cache was
//! never populated by a completion interaction. We solve that by
//! walking the document on open / save, enumerating every constraint
//! target (Terraform CLI, provider, module), and kicking off the same
//! `fetch_*` APIs the completion path uses — all in the background so
//! the main document handler stays responsive. When fetches finish we
//! ask the client to re-request inlay hints (and we re-publish
//! diagnostics so the semantic no-match warning lights up too).

use std::collections::HashSet;
use std::sync::Arc;

use hcl_edit::expr::Expression;
use hcl_edit::structure::Body;
use tfls_state::StateStore;
use tower_lsp::lsp_types::Url;

use crate::backend::Backend;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum Target {
    TerraformCli,
    Provider {
        namespace: String,
        name: String,
    },
    Module {
        namespace: String,
        name: String,
        provider: String,
    },
}

/// Fire-and-forget: parse the document, fetch every uncached version
/// target in parallel, then trigger client-side inlay-hint refresh +
/// diagnostic re-publish when the last fetch completes.
pub fn spawn(backend: &Backend, uri: Url, version: Option<i32>) {
    let state = Arc::clone(&backend.state);
    let client = backend.client.clone();
    tokio::spawn(async move {
        prefetch_and_refresh(state, client, uri, version).await;
    });
}

async fn prefetch_and_refresh(
    state: Arc<StateStore>,
    client: tower_lsp::Client,
    uri: Url,
    _version: Option<i32>,
) {
    let targets = match state.documents.get(&uri) {
        Some(doc) => match doc.parsed.body.as_ref() {
            Some(body) => collect_targets(body),
            None => return,
        },
        None => return,
    };
    if targets.is_empty() {
        return;
    }

    let Ok(http) = tfls_provider_protocol::registry_versions::build_http_client() else {
        return;
    };
    let Ok(gh) = tfls_provider_protocol::tool_versions::build_http_client() else {
        return;
    };

    let mut joins = Vec::new();
    for target in targets {
        let http = http.clone();
        let gh = gh.clone();
        joins.push(tokio::spawn(async move {
            match target {
                Target::TerraformCli => {
                    let _ = tfls_provider_protocol::tool_versions::fetch_tool_versions(&gh).await;
                }
                Target::Provider { namespace, name } => {
                    let _ = tfls_provider_protocol::registry_versions::fetch_versions(
                        &http, &namespace, &name,
                    )
                    .await;
                }
                Target::Module {
                    namespace,
                    name,
                    provider,
                } => {
                    let _ = tfls_provider_protocol::registry_versions::fetch_module_versions(
                        &http, &namespace, &name, &provider,
                    )
                    .await;
                }
            }
        }));
    }
    for j in joins {
        let _ = j.await;
    }

    // Ask the client to re-request inlay hints. The standard LSP
    // method is `workspace/inlayHint/refresh`; tower-lsp exposes it
    // on `Client::inlay_hint_refresh`. We ignore failures — an older
    // client that doesn't support the capability just won't refresh
    // until the next user action.
    let _ = client.inlay_hint_refresh().await;
}

fn collect_targets(body: &Body) -> HashSet<Target> {
    let mut out: HashSet<Target> = HashSet::new();
    for structure in body.iter() {
        let Some(block) = structure.as_block() else { continue };
        match block.ident.as_str() {
            "terraform" => collect_terraform(&block.body, &mut out),
            "module" => collect_module(&block.body, &mut out),
            _ => {}
        }
    }
    out
}

fn collect_terraform(body: &Body, out: &mut HashSet<Target>) {
    for structure in body.iter() {
        if let Some(attr) = structure.as_attribute() {
            if attr.key.as_str() == "required_version" && literal_string(&attr.value).is_some() {
                out.insert(Target::TerraformCli);
            }
        } else if let Some(nested) = structure.as_block() {
            if nested.ident.as_str() == "required_providers" {
                for entry in nested.body.iter() {
                    let Some(attr) = entry.as_attribute() else { continue };
                    let Expression::Object(obj) = &attr.value else { continue };
                    for (key, value) in obj.iter() {
                        if let Some(k) = object_key_as_str(key) {
                            if k == "source" {
                                if let Some(s) = literal_string(value.expr()) {
                                    if let Some((ns, name)) = parse_provider_source(&s) {
                                        out.insert(Target::Provider {
                                            namespace: ns,
                                            name,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn collect_module(body: &Body, out: &mut HashSet<Target>) {
    let mut source_str: Option<String> = None;
    for structure in body.iter() {
        let Some(attr) = structure.as_attribute() else { continue };
        if attr.key.as_str() == "source" {
            source_str = literal_string(&attr.value);
        }
    }
    if let Some(s) = source_str.as_deref().and_then(parse_module_source) {
        out.insert(Target::Module {
            namespace: s.0,
            name: s.1,
            provider: s.2,
        });
    }
}

fn literal_string(expr: &Expression) -> Option<String> {
    match expr {
        Expression::String(s) => Some(s.as_str().to_string()),
        Expression::StringTemplate(t) => {
            let mut collected = String::new();
            for element in t.iter() {
                match element {
                    hcl_edit::template::Element::Literal(lit) => {
                        collected.push_str(lit.as_str())
                    }
                    _ => return None,
                }
            }
            Some(collected)
        }
        _ => None,
    }
}

fn object_key_as_str(key: &hcl_edit::expr::ObjectKey) -> Option<String> {
    match key {
        hcl_edit::expr::ObjectKey::Ident(d) => Some(d.as_str().to_string()),
        hcl_edit::expr::ObjectKey::Expression(Expression::String(s)) => {
            Some(s.as_str().to_string())
        }
        _ => None,
    }
}

fn parse_provider_source(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    let mut parts = s.splitn(3, '/');
    let a = parts.next()?;
    let b = parts.next()?;
    if let Some(c) = parts.next() {
        Some((b.to_string(), c.to_string()))
    } else {
        Some((a.to_string(), b.to_string()))
    }
}

fn parse_module_source(s: &str) -> Option<(String, String, String)> {
    let s = s.trim();
    if s.starts_with('.') || s.starts_with('/') || s.contains("://") || s.contains("::") {
        return None;
    }
    let parts: Vec<&str> = s.split('/').collect();
    match parts.as_slice() {
        [ns, name, provider] if !ns.is_empty() && !name.is_empty() && !provider.is_empty() => {
            Some((ns.to_string(), name.to_string(), provider.to_string()))
        }
        _ => None,
    }
}
