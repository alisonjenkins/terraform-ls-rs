//! Diagnose registry-doc enrichment for one provider.
//!
//! Hover descriptions for many providers come from the Terraform
//! Registry's hand-written Markdown rather than the gRPC schema
//! (most SDK-v2 providers ship empty per-attribute descriptions;
//! many Plugin Framework providers do too). The enrichment
//! pipeline lives in `tfls_provider_protocol::registry_docs` and
//! is best-effort — silent failures (HTTP 404, unrecognised
//! Markdown shape, etc.) only show up indirectly as missing hover
//! text. This binary surfaces the pipeline's state for one
//! provider so a "hover doesn't work for X" report is one
//! command away from a root cause.
//!
//! ```bash
//! # Inspect cache + index, then parse one resource's docs:
//! cargo run --bin tfls-doc-probe -- hashicorp/azurerm@4.50.0 \
//!     --resource azurerm_automation_runbook
//!
//! # Walk every resource in the index and report ones whose parser
//! # output is empty (the canary for "registry markdown shape we
//! # don't recognise").
//! cargo run --bin tfls-doc-probe -- hashicorp/azurerm@4.50.0 --list-uncovered
//!
//! # Force a re-fetch by purging the cache first:
//! cargo run --bin tfls-doc-probe -- hashicorp/azurerm@4.50.0 \
//!     --resource azurerm_automation_runbook --no-cache
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::ExitCode;

use clap::Parser;
use tfls_provider_protocol::registry_docs;

#[derive(Debug, Parser)]
#[command(
    name = "tfls-doc-probe",
    about = "Inspect registry-doc enrichment for one provider"
)]
struct Cli {
    /// Provider coordinate `<namespace>/<name>@<version>`. Example:
    /// `hashicorp/azurerm@4.50.0`.
    coord: String,

    /// Inspect a single resource's Markdown + parser output.
    #[arg(long)]
    resource: Option<String>,

    /// Same as `--resource` but for data sources.
    #[arg(long)]
    data_source: Option<String>,

    /// Walk every resource in the index, run the parser, and report
    /// resources whose parser output is empty. Use to scope parser
    /// fixes for unfamiliar Markdown shapes.
    #[arg(long)]
    list_uncovered: bool,

    /// Print the raw Markdown content of the targeted resource /
    /// data source before the parser output. Useful when the parser
    /// returns garbage and you need to see what it was looking at.
    #[arg(long)]
    show_markdown: bool,

    /// Bypass the on-disk cache for this run (forces a fresh
    /// fetch). The cache file is left untouched.
    #[arg(long)]
    no_cache: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some(coords) = parse_coord(&cli.coord) else {
        eprintln!(
            "error: coord must look like `<namespace>/<name>@<version>` (got `{}`)",
            cli.coord
        );
        return ExitCode::from(2);
    };

    println!(
        "provider: {}/{}@{}",
        coords.namespace, coords.name, coords.version
    );

    print_cache_summary(&coords);

    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to build HTTP client: {e}");
            return ExitCode::from(1);
        }
    };

    if cli.no_cache {
        let _ = std::fs::remove_file(registry_docs::parsed_cache_path(
            &coords.namespace,
            &coords.name,
            &coords.version,
        ));
    }

    let index =
        match registry_docs::fetch_index(&client, &coords.namespace, &coords.name, &coords.version)
            .await
        {
            Ok(i) => i,
            Err(e) => {
                eprintln!("error: fetch_index failed: {e}");
                return ExitCode::from(1);
            }
        };
    let (n_resources, n_data_sources) = count_index_categories(&index);
    println!("index: {n_resources} resources, {n_data_sources} data sources",);

    if cli.list_uncovered {
        list_uncovered(&client, &coords, &index).await;
        return ExitCode::SUCCESS;
    }

    if let Some(type_name) = cli.resource.as_deref() {
        probe_one(
            &client,
            &coords,
            &index,
            type_name,
            Kind::Resource,
            cli.show_markdown,
            cli.no_cache,
        )
        .await;
    }
    if let Some(type_name) = cli.data_source.as_deref() {
        probe_one(
            &client,
            &coords,
            &index,
            type_name,
            Kind::DataSource,
            cli.show_markdown,
            cli.no_cache,
        )
        .await;
    }

    if cli.resource.is_none() && cli.data_source.is_none() {
        println!(
            "\n(no --resource / --data-source / --list-uncovered specified; nothing more to do)"
        );
    }

    ExitCode::SUCCESS
}

fn parse_coord(s: &str) -> Option<registry_docs::ProviderCoords> {
    let (path, version) = s.rsplit_once('@')?;
    let (namespace, name) = path.split_once('/')?;
    if namespace.is_empty() || name.is_empty() || version.is_empty() {
        return None;
    }
    Some(registry_docs::ProviderCoords {
        address: format!("registry.terraform.io/{namespace}/{name}"),
        namespace: namespace.to_string(),
        name: name.to_string(),
        version: version.to_string(),
    })
}

fn print_cache_summary(coords: &registry_docs::ProviderCoords) {
    let path = registry_docs::parsed_cache_path(&coords.namespace, &coords.name, &coords.version);
    print!("parsed-descriptions cache: {} ", path.display());
    match std::fs::metadata(&path) {
        Ok(m) => {
            let size = m.len();
            println!("(present, {size} bytes)");
            // Peek at a sample by reading the JSON keys cheaply.
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    let resources = v
                        .get("resources")
                        .and_then(|r| r.as_object())
                        .map(|o| o.len())
                        .unwrap_or(0);
                    let data_sources = v
                        .get("data_sources")
                        .and_then(|r| r.as_object())
                        .map(|o| o.len())
                        .unwrap_or(0);
                    println!(
                        "  cache contents: {resources} resources, {data_sources} data sources"
                    );
                }
            }
        }
        Err(_) => println!("(absent — enrichment hasn't produced one yet)"),
    }
}

#[derive(Copy, Clone, Debug)]
enum Kind {
    Resource,
    DataSource,
}

async fn probe_one(
    client: &reqwest::Client,
    coords: &registry_docs::ProviderCoords,
    index: &registry_docs::ProviderDocIndex,
    type_name: &str,
    kind: Kind,
    show_markdown: bool,
    no_cache: bool,
) {
    let prefix = format!("{}_", coords.name);
    let slug = type_name.strip_prefix(&prefix).unwrap_or(type_name);
    let label = match kind {
        Kind::Resource => "resource",
        Kind::DataSource => "data source",
    };
    println!("\n--- {label} {type_name} ---");
    let id = match kind {
        Kind::Resource => index.get_resource(slug),
        Kind::DataSource => index.get_data_source(slug),
    };
    let Some(id) = id else {
        println!("not found in index (slug = `{slug}`)");
        return;
    };
    println!("doc id: {id}");

    if no_cache {
        let _ = std::fs::remove_file(registry_docs::doc_cache_path(
            &coords.namespace,
            &coords.name,
            &coords.version,
            id,
        ));
    }

    let content = match registry_docs::fetch_doc_content(
        client,
        &coords.namespace,
        &coords.name,
        &coords.version,
        id,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            println!("fetch_doc_content failed: {e}");
            return;
        }
    };
    println!("markdown: {} bytes", content.len());

    if show_markdown {
        println!("\n```markdown");
        println!("{content}");
        println!("```");
    }

    let parsed = registry_docs::parse_attribute_descriptions(&content);
    println!("parser produced {} attribute(s)", parsed.len());
    if parsed.is_empty() {
        println!("(zero attributes — likely a parser shape mismatch for this provider's docs)");
        return;
    }
    let mut entries: Vec<_> = parsed.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (name, parsed) in entries.iter().take(40) {
        let trimmed: String = parsed.description.chars().take(140).collect();
        let suffix = match &parsed.allowed_values {
            Some(vs) if !vs.is_empty() => format!(
                " [valid: {}]",
                vs.iter()
                    .map(|v| format!("`{v}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => String::new(),
        };
        println!("  - {name}: {trimmed}{suffix}");
    }
    if entries.len() > 40 {
        println!("  ... ({} more)", entries.len() - 40);
    }
}

async fn list_uncovered(
    client: &reqwest::Client,
    coords: &registry_docs::ProviderCoords,
    index: &registry_docs::ProviderDocIndex,
) {
    use futures::stream::{self, StreamExt};

    println!("\n--- uncovered scan ---");
    // Build (kind, slug, doc_id) targets from the index. We don't
    // have the schema here so we walk every entry — bounded by the
    // index size.
    let mut targets: Vec<(&'static str, String, String)> = Vec::new();
    for (key, id) in &index.entries {
        let Some((category, slug)) = key.split_once(':') else {
            continue;
        };
        let category_label = match category {
            "resources" => "resource",
            "data-sources" => "data source",
            _ => continue,
        };
        targets.push((category_label, slug.to_string(), id.clone()));
    }

    println!("scanning {} entries...", targets.len());
    let ns = coords.namespace.clone();
    let name = coords.name.clone();
    let version = coords.version.clone();
    let results: Vec<(&'static str, String, usize, Option<String>)> =
        stream::iter(targets.into_iter().map(|(kind, slug, id)| {
            let client = client.clone();
            let ns = ns.clone();
            let name = name.clone();
            let version = version.clone();
            async move {
                match registry_docs::fetch_doc_content(&client, &ns, &name, &version, &id).await {
                    Ok(content) => {
                        let parsed = registry_docs::parse_attribute_descriptions(&content);
                        (kind, slug, parsed.len(), None)
                    }
                    Err(e) => (kind, slug, 0, Some(e.to_string())),
                }
            }
        }))
        .buffer_unordered(8)
        .collect()
        .await;

    let mut empty: Vec<&(&'static str, String, usize, Option<String>)> =
        results.iter().filter(|(_, _, n, _)| *n == 0).collect();
    empty.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
    if empty.is_empty() {
        println!("all entries parsed to ≥1 attribute — registry-doc enrichment looks healthy");
        return;
    }
    println!("\nentries with zero parsed attributes:");
    for (kind, slug, _, err) in &empty {
        match err {
            Some(e) => println!("  - {kind} {slug} (fetch error: {e})"),
            None => println!("  - {kind} {slug}"),
        }
    }
    let total = results.len();
    println!(
        "\nsummary: {} / {total} entries empty ({:.1}%)",
        empty.len(),
        100.0 * empty.len() as f64 / total as f64
    );
}

fn count_index_categories(index: &registry_docs::ProviderDocIndex) -> (usize, usize) {
    let mut resources = 0usize;
    let mut data_sources = 0usize;
    for key in index.entries.keys() {
        if key.starts_with("resources:") {
            resources += 1;
        } else if key.starts_with("data-sources:") {
            data_sources += 1;
        }
    }
    (resources, data_sources)
}

fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent("terraform-ls-rs/0.1 (+tfls-doc-probe)")
        .timeout(std::time::Duration::from_secs(20))
        .build()
}
