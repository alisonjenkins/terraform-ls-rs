//! Discover deprecations declared by installed providers — fetches
//! schemas via the plugin gRPC protocol, walks every resource /
//! data source / attribute, prints the ones flagged
//! `deprecated: true` along with the registry-doc URL where
//! migration breadcrumbs typically live.
//!
//! Used to PRIORITISE which deprecations get a hand-written tier-1
//! `DeprecationRule` (rich message + auto-fix action). Tier 2
//! catches everything automatically; tier 1 needs human curation.
//!
//! ```bash
//! # Markdown report (default), one provider's deprecations:
//! cargo run --release -p tfls-cli --bin tfls-deprecation-scrape -- \
//!     ~/work/some-tf-workspace --provider aws
//!
//! # Filter to attributes only (the long tail tends to be huge):
//! cargo run --release ... -- ~/workspace --attributes-only
//!
//! # Emit Rust scaffolding for one resource (drop into tfls-diag):
//! cargo run --release ... -- ~/workspace --scaffold aws_s3_bucket_object
//!
//! # Machine-readable JSON for piping into other tools:
//! cargo run --release ... -- ~/workspace --json
//! ```
//!
//! Suppression: type names already covered by a tier-1 rule (see
//! `tfls_diag::is_hardcoded_deprecation`) are flagged in the
//! report as already-covered so the curator doesn't duplicate
//! work.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use tfls_schema::ProviderSchemas;

#[derive(Debug, Parser)]
#[command(
    name = "tfls-deprecation-scrape",
    about = "List provider-declared deprecations from installed schemas"
)]
struct Cli {
    /// Workspace directory containing `.terraform/providers/`
    /// (walks up from this dir to find the init root, like the
    /// LSP indexer).
    dir: PathBuf,

    /// Restrict output to a single provider local name (e.g.
    /// `aws`, `azurerm`, `google`).
    #[arg(long)]
    provider: Option<String>,

    /// Include attribute-level deprecations (default = blocks only;
    /// providers tend to mark dozens of attrs per release).
    #[arg(long)]
    include_attributes: bool,

    /// Skip block-level deprecations (resource / data source) —
    /// useful with `--include-attributes` to focus on the long
    /// tail.
    #[arg(long)]
    attributes_only: bool,

    /// Filter to deprecations NOT already covered by a tier-1
    /// `DeprecationRule` (per `tfls_diag::is_hardcoded_deprecation`).
    /// Directly answers "what should I tier-1 next?" — drops the
    /// already-covered noise so curators only see candidates
    /// worth attention.
    #[arg(long)]
    uncovered_only: bool,

    /// Output format.
    #[arg(long, value_enum, default_value = "markdown")]
    format: Format,

    /// Emit Rust scaffolding (a draft `DeprecationRule` const +
    /// wrapper module) for the named resource / data source. The
    /// type name should be unprefixed (e.g.
    /// `aws_s3_bucket_object`). Implies `--format=scaffold`.
    #[arg(long)]
    scaffold: Option<String>,

    /// Increase logging verbosity (`-v` = debug, `-vv` = trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Markdown,
    Json,
    Scaffold,
}

#[derive(Debug, Clone)]
struct DepBlock {
    provider: String,
    kind: BlockKind,
    type_name: String,
    description: Option<String>,
    /// `true` when `is_hardcoded_deprecation` already matches —
    /// tier-1 covers this.
    already_covered: bool,
}

#[derive(Debug, Clone)]
struct DepAttribute {
    provider: String,
    kind: BlockKind,
    type_name: String,
    attr_name: String,
    description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Resource,
    DataSource,
}

impl BlockKind {
    fn block_kind_str(&self) -> &'static str {
        match self {
            BlockKind::Resource => "resource",
            BlockKind::DataSource => "data",
        }
    }
    fn label_word(&self) -> &'static str {
        match self {
            BlockKind::Resource => "resource",
            BlockKind::DataSource => "data source",
        }
    }
    fn registry_url_segment(&self) -> &'static str {
        match self {
            BlockKind::Resource => "resources",
            BlockKind::DataSource => "data-sources",
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let dir = cli.dir.canonicalize()?;
    let Some(init_root) = find_terraform_init_root(&dir) else {
        return Err(format!(
            "no `.terraform/providers/` found in or above {}",
            dir.display()
        )
        .into());
    };
    let tf_dir = init_root.join(".terraform");
    eprintln!("# scraping schemas from {}", tf_dir.display());

    let schemas = tfls_provider_protocol::fetch_schemas_from_plugins(&tf_dir, None).await?;
    eprintln!(
        "# loaded {} provider schemas",
        schemas.provider_schemas.len()
    );

    let (mut blocks, attributes) = collect_deprecations(&schemas, cli.provider.as_deref());

    let total_blocks = blocks.len();
    let covered_blocks = blocks.iter().filter(|b| b.already_covered).count();
    eprintln!(
        "# found {total_blocks} deprecated blocks ({covered_blocks} already covered by tier-1) + {} deprecated attributes",
        attributes.len()
    );

    if cli.uncovered_only {
        blocks.retain(|b| !b.already_covered);
        eprintln!(
            "# --uncovered-only: filtered to {} block candidate{}",
            blocks.len(),
            if blocks.len() == 1 { "" } else { "s" }
        );
    }

    if let Some(target) = cli.scaffold.as_deref() {
        let Some(b) = blocks.iter().find(|b| b.type_name == target) else {
            return Err(format!(
                "no deprecated block named `{target}` in scraped schemas \
                 (use without --scaffold to list candidates)"
            )
            .into());
        };
        emit_scaffold(b);
        return Ok(());
    }

    let show_blocks = !cli.attributes_only;
    let show_attrs = cli.include_attributes || cli.attributes_only;

    match cli.format {
        Format::Markdown => emit_markdown(&blocks, &attributes, show_blocks, show_attrs),
        Format::Json => emit_json(&blocks, &attributes, show_blocks, show_attrs),
        Format::Scaffold => {
            return Err(
                "use --scaffold <type_name> to choose a single rule".into(),
            );
        }
    }

    Ok(())
}

/// Walk every provider's resource + data-source schemas and
/// collect the `deprecated: true` flags into structured records.
fn collect_deprecations(
    schemas: &ProviderSchemas,
    only_provider_local: Option<&str>,
) -> (Vec<DepBlock>, Vec<DepAttribute>) {
    let mut blocks = Vec::new();
    let mut attributes = Vec::new();

    for (provider_addr, schema) in &schemas.provider_schemas {
        let local_name = local_provider_name(provider_addr);
        if let Some(filter) = only_provider_local {
            if local_name != filter {
                continue;
            }
        }

        for (type_name, res) in &schema.resource_schemas {
            if res.block.deprecated {
                blocks.push(DepBlock {
                    provider: local_name.to_string(),
                    kind: BlockKind::Resource,
                    type_name: type_name.clone(),
                    description: nonempty(&res.block.description),
                    already_covered: tfls_diag::is_hardcoded_deprecation(
                        "resource",
                        type_name,
                    ),
                });
            }
            for (attr_name, attr) in &res.block.attributes {
                if attr.deprecated {
                    attributes.push(DepAttribute {
                        provider: local_name.to_string(),
                        kind: BlockKind::Resource,
                        type_name: type_name.clone(),
                        attr_name: attr_name.clone(),
                        description: nonempty(&attr.description),
                    });
                }
            }
        }
        for (type_name, ds) in &schema.data_source_schemas {
            if ds.block.deprecated {
                blocks.push(DepBlock {
                    provider: local_name.to_string(),
                    kind: BlockKind::DataSource,
                    type_name: type_name.clone(),
                    description: nonempty(&ds.block.description),
                    already_covered: tfls_diag::is_hardcoded_deprecation("data", type_name),
                });
            }
            for (attr_name, attr) in &ds.block.attributes {
                if attr.deprecated {
                    attributes.push(DepAttribute {
                        provider: local_name.to_string(),
                        kind: BlockKind::DataSource,
                        type_name: type_name.clone(),
                        attr_name: attr_name.clone(),
                        description: nonempty(&attr.description),
                    });
                }
            }
        }
    }

    blocks.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then(a.kind.label_word().cmp(b.kind.label_word()))
            .then(a.type_name.cmp(&b.type_name))
    });
    attributes.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then(a.type_name.cmp(&b.type_name))
            .then(a.attr_name.cmp(&b.attr_name))
    });
    (blocks, attributes)
}

fn local_provider_name(addr: &str) -> &str {
    addr.rsplit('/').next().unwrap_or(addr)
}

fn nonempty(s: &Option<String>) -> Option<String> {
    s.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()).map(String::from)
}

fn registry_url(provider_addr_local: &str, kind: BlockKind, type_name: &str) -> String {
    let slug = type_name
        .strip_prefix(&format!("{provider_addr_local}_"))
        .unwrap_or(type_name);
    format!(
        "https://registry.terraform.io/providers/hashicorp/{provider_addr_local}/latest/docs/{}/{slug}",
        kind.registry_url_segment()
    )
}

fn emit_markdown(
    blocks: &[DepBlock],
    attributes: &[DepAttribute],
    show_blocks: bool,
    show_attrs: bool,
) {
    if show_blocks {
        let mut current_provider = "";
        let pending: Vec<&DepBlock> = blocks.iter().filter(|b| !b.already_covered).collect();
        let covered: Vec<&DepBlock> = blocks.iter().filter(|b| b.already_covered).collect();

        println!("# Deprecated blocks (tier-1 candidates)\n");
        if pending.is_empty() {
            println!("_No uncovered deprecated blocks._\n");
        }
        for b in &pending {
            if b.provider != current_provider {
                current_provider = &b.provider;
                println!("## `{current_provider}`\n");
            }
            let url = registry_url(&b.provider, b.kind, &b.type_name);
            println!("- **{}** `{}`", b.kind.label_word(), b.type_name);
            println!("  - docs: {url}");
            if let Some(desc) = &b.description {
                let snippet = first_paragraph(desc, 240);
                println!("  - description: > {snippet}");
            }
            println!();
        }

        if !covered.is_empty() {
            println!("\n# Deprecated blocks already covered by tier-1\n");
            for b in &covered {
                println!("- `{}.{}` (provider `{}`)", b.kind.block_kind_str(), b.type_name, b.provider);
            }
            println!();
        }
    }

    if show_attrs {
        println!("\n# Deprecated attributes (long tail)\n");
        if attributes.is_empty() {
            println!("_None._\n");
        }
        let mut current = ("", "");
        for a in attributes {
            let key = (a.provider.as_str(), a.type_name.as_str());
            if key != current {
                current = key;
                println!(
                    "\n## `{}.{}`",
                    a.kind.block_kind_str(),
                    a.type_name
                );
            }
            print!("- `{}`", a.attr_name);
            if let Some(desc) = &a.description {
                let snippet = first_paragraph(desc, 160);
                print!(" — {snippet}");
            }
            println!();
        }
    }
}

fn emit_json(
    blocks: &[DepBlock],
    attributes: &[DepAttribute],
    show_blocks: bool,
    show_attrs: bool,
) {
    let blocks_json: Vec<sonic_rs::Value> = if show_blocks {
        blocks
            .iter()
            .map(|b| {
                let url = registry_url(&b.provider, b.kind, &b.type_name);
                sonic_rs::json!({
                    "provider": b.provider,
                    "kind": b.kind.block_kind_str(),
                    "type_name": b.type_name,
                    "description": b.description,
                    "already_covered": b.already_covered,
                    "registry_url": url,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    let attrs_json: Vec<sonic_rs::Value> = if show_attrs {
        attributes
            .iter()
            .map(|a| {
                sonic_rs::json!({
                    "provider": a.provider,
                    "kind": a.kind.block_kind_str(),
                    "type_name": a.type_name,
                    "attribute": a.attr_name,
                    "description": a.description,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    let combined = sonic_rs::json!({
        "blocks": blocks_json,
        "attributes": attrs_json,
    });
    println!("{}", sonic_rs::to_string_pretty(&combined).unwrap_or_default());
}

/// Emit a Rust scaffolding for a tier-1 rule covering this
/// deprecated block. Curator drops the file into
/// `crates/tfls-diag/src/`, picks a provider-version threshold,
/// improves the message, and adds the label to
/// `HARDCODED_DEPRECATION_LABELS`.
fn emit_scaffold(b: &DepBlock) {
    let mod_name = format!("deprecated_{}", b.type_name);
    let url = registry_url(&b.provider, b.kind, &b.type_name);
    let kind_word = b.kind.label_word();

    println!("// File: crates/tfls-diag/src/{mod_name}.rs");
    println!("// Add to crates/tfls-diag/src/lib.rs:");
    println!("//   pub mod {mod_name};");
    println!("//   pub use {mod_name}::deprecated_{}_diagnostics{{,_for_module}};",
             b.type_name);
    println!("// Add to deprecation_rule.rs::HARDCODED_DEPRECATION_LABELS:");
    println!("//   (\"{}\", \"{}\"),", b.kind.block_kind_str(), b.type_name);
    println!();
    println!("//! `terraform_deprecated_{}` — flag uses of", b.type_name);
    println!("//! `{} \"{}\"`. Provider-version-gated.", b.kind.block_kind_str(), b.type_name);
    println!("//!");
    if let Some(desc) = &b.description {
        for line in first_paragraph(desc, 600).lines() {
            println!("//! {line}");
        }
    } else {
        println!("//! TODO: write a migration message based on the registry docs.");
    }
    println!("//!");
    println!("//! Registry docs: {url}");
    println!();
    println!("use hcl_edit::structure::Body;");
    println!("use lsp_types::Diagnostic;");
    println!("use ropey::Rope;");
    println!();
    println!("use crate::deprecation_rule::{{self, DeprecationRule, Gate}};");
    println!();
    println!("const RULE: DeprecationRule = DeprecationRule {{");
    println!("    block_kind: \"{}\",", b.kind.block_kind_str());
    println!("    label: \"{}\",", b.type_name);
    println!("    gate: Gate::ProviderVersion {{");
    println!("        provider: \"{}\",", b.provider);
    println!("        threshold: \"X.Y.Z\", // TODO: version that introduced the replacement");
    println!("    }},");
    println!("    message: \"{} `{}` is deprecated — see {url}\",",
             kind_word, b.type_name);
    println!("}};");
    println!();
    println!("pub fn deprecated_{}_diagnostics(body: &Body, rope: &Rope) -> Vec<Diagnostic> {{",
             b.type_name);
    println!("    deprecation_rule::diagnostics(&RULE, body, rope)");
    println!("}}");
    println!();
    println!("pub fn deprecated_{}_diagnostics_for_module(", b.type_name);
    println!("    body: &Body,");
    println!("    rope: &Rope,");
    println!("    supports: bool,");
    println!(") -> Vec<Diagnostic> {{");
    println!("    deprecation_rule::diagnostics_for_module(&RULE, body, rope, supports)");
    println!("}}");
}

fn first_paragraph(s: &str, max_chars: usize) -> String {
    let para = s.split("\n\n").next().unwrap_or(s).replace('\n', " ");
    if para.chars().count() <= max_chars {
        para
    } else {
        let mut out = String::new();
        for (i, ch) in para.chars().enumerate() {
            if i + 1 >= max_chars {
                out.push('…');
                break;
            }
            out.push(ch);
        }
        out
    }
}

fn find_terraform_init_root(start: &Path) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        if dir.join(".terraform").join("providers").is_dir() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt};
    let level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let _ = fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()))
        .with_writer(std::io::stderr)
        .try_init();
}
