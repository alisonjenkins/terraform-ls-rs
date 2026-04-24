//! Standalone diagnostic dumper. Loads a directory, fetches schemas,
//! runs the full `compute_diagnostics` pipeline over every `.tf` /
//! `.tf.json` file, prints results grouped by file.
//!
//! Used as a bug-hunting harness — output mirrors what `did_open`
//! would produce after indexing completes, but without the LSP
//! client round-trip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use lsp_types::{DiagnosticSeverity, Url};
use tfls_lsp::handlers::document::compute_diagnostics;
use tfls_state::{DocumentState, StateStore};
use tfls_walker::discover_terraform_files;

#[derive(Debug, Parser)]
#[command(
    name = "tfls-diag-dump",
    about = "Dump diagnostics for every .tf file in a directory"
)]
struct Cli {
    /// Workspace/module directory to analyse.
    dir: PathBuf,

    /// Skip provider-schema fetch (faster, but schema-validation
    /// diagnostics will be silent).
    #[arg(long)]
    no_schemas: bool,

    /// Only print diagnostics whose severity is `Error` or `Warning`
    /// (skips Info / Hint).
    #[arg(long)]
    errors_only: bool,

    /// Filter results by substring match on diagnostic message.
    #[arg(long)]
    grep: Option<String>,

    /// Increase logging verbosity (`-v` = debug, `-vv` = trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    tfls_lsp::configure_rayon_pool();

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
    eprintln!("# workspace: {}", dir.display());

    let state = StateStore::new();

    // 1. Discover + parse + upsert every .tf / .tf.json.
    let files = discover_terraform_files(&dir)?;
    eprintln!("# discovered {} .tf / .tf.json files", files.len());
    parse_and_upsert(&state, &files);
    eprintln!("# upserted {} documents", state.documents.len());

    // 2. Install bundled functions synchronously — cheap, catches
    //    the `unknown function` family without round-tripping the CLI.
    let functions = tfls_schema::functions_cache::bundled()?;
    state.install_functions(functions);

    // 3. Schema fetch via plugin protocol. Walk up from `dir` to find
    //    `.terraform/providers/` — same logic the indexer uses on
    //    `did_open`. Diagnostics that depend on schemas (unknown
    //    attribute, deprecated, etc.) only fire once this returns.
    if !cli.no_schemas {
        if let Some(init_root) = find_terraform_init_root(&dir) {
            eprintln!("# fetching schemas from {}", init_root.display());
            let tf_dir = init_root.join(".terraform");
            match tfls_provider_protocol::fetch_schemas_from_plugins(&tf_dir, None).await {
                Ok(schemas) => {
                    let n = schemas.provider_schemas.len();
                    state.install_schemas(schemas);
                    eprintln!("# installed {n} provider schemas");
                }
                Err(e) => {
                    eprintln!("# WARNING: schema fetch failed: {e}");
                }
            }
        } else {
            eprintln!("# no .terraform/providers found — skipping schema fetch");
        }
    }

    // 4. Run diagnostics per file, grouped and sorted.
    let mut by_file: BTreeMap<String, Vec<lsp_types::Diagnostic>> = BTreeMap::new();
    let mut total = 0usize;
    for entry in state.documents.iter() {
        let uri = entry.key();
        let mut diags = compute_diagnostics(&state, uri);
        if cli.errors_only {
            diags.retain(|d| {
                matches!(
                    d.severity,
                    Some(DiagnosticSeverity::ERROR) | Some(DiagnosticSeverity::WARNING)
                )
            });
        }
        if let Some(q) = &cli.grep {
            diags.retain(|d| d.message.contains(q));
        }
        if diags.is_empty() {
            continue;
        }
        total += diags.len();
        let rel = relative_path(uri, &dir);
        by_file.insert(rel, diags);
    }

    let mut counts = SeverityCounts::default();
    for (path, diags) in &by_file {
        println!("=== {path} ({} diagnostics)", diags.len());
        let mut sorted = diags.clone();
        sorted.sort_by_key(|d| (d.range.start.line, d.range.start.character));
        for d in sorted {
            counts.tick(&d);
            let sev = severity_label(&d);
            let src = d.source.as_deref().unwrap_or("?");
            let code = d
                .code
                .as_ref()
                .map(|c| match c {
                    lsp_types::NumberOrString::Number(n) => n.to_string(),
                    lsp_types::NumberOrString::String(s) => s.clone(),
                })
                .unwrap_or_default();
            println!(
                "  {}:{}:{}  {}  [{src}{}{code}]  {}",
                path,
                d.range.start.line + 1,
                d.range.start.character + 1,
                sev,
                if code.is_empty() { "" } else { "/" },
                d.message,
            );
        }
        println!();
    }

    eprintln!(
        "# totals: {total} diagnostics across {} files — {} err, {} warn, {} info, {} hint",
        by_file.len(),
        counts.err,
        counts.warn,
        counts.info,
        counts.hint,
    );

    Ok(())
}

fn parse_and_upsert(state: &StateStore, files: &[PathBuf]) {
    for path in files {
        let Ok(url) = Url::from_file_path(path) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        state.upsert_document(DocumentState::new(url, &text, 0));
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

fn relative_path(uri: &Url, root: &Path) -> String {
    let Ok(path) = uri.to_file_path() else {
        return uri.to_string();
    };
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn severity_label(d: &lsp_types::Diagnostic) -> &'static str {
    match d.severity {
        Some(DiagnosticSeverity::ERROR) => "ERROR  ",
        Some(DiagnosticSeverity::WARNING) => "WARN   ",
        Some(DiagnosticSeverity::INFORMATION) => "INFO   ",
        Some(DiagnosticSeverity::HINT) => "HINT   ",
        _ => "?      ",
    }
}

#[derive(Default)]
struct SeverityCounts {
    err: usize,
    warn: usize,
    info: usize,
    hint: usize,
}

impl SeverityCounts {
    fn tick(&mut self, d: &lsp_types::Diagnostic) {
        match d.severity {
            Some(DiagnosticSeverity::ERROR) => self.err += 1,
            Some(DiagnosticSeverity::WARNING) => self.warn += 1,
            Some(DiagnosticSeverity::INFORMATION) => self.info += 1,
            Some(DiagnosticSeverity::HINT) => self.hint += 1,
            _ => {}
        }
    }
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbosity {
            0 => "warn",
            1 => "info,tfls=debug",
            _ => "debug,tfls=trace",
        };
        EnvFilter::new(level)
    });

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
