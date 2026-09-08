//! Standalone diagnostic dumper. Thin CLI wrapper over
//! `tfls_engine::workspace::{load, lint_all}` — loads a directory,
//! fetches schemas, runs the full diagnostics pipeline over every
//! `.tf` / `.tf.json` file, prints results grouped by file.
//!
//! Used as a bug-hunting harness — output mirrors what `did_open`
//! would produce after indexing completes, but without the LSP
//! client round-trip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use lsp_types::DiagnosticSeverity;
use tfls_engine::workspace::{lint_all, load, LoadOptions, SchemaOutcome, SchemaSource};
use url::Url;

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
    let opts = LoadOptions {
        schemas: if cli.no_schemas {
            SchemaSource::None
        } else {
            SchemaSource::Plugins
        },
    };
    let root = cli.dir.canonicalize()?;
    let loaded = load(&cli.dir, &opts).await?;
    eprintln!("# workspace: {}", root.display());
    eprintln!("# discovered {} .tf / .tf.json files", loaded.file_count);
    eprintln!("# upserted {} documents", loaded.state.documents.len());

    match &loaded.schema_outcome {
        SchemaOutcome::Fetched { init_root, count } => {
            eprintln!("# fetching schemas from {}", init_root.display());
            eprintln!("# installed {count} provider schemas");
        }
        SchemaOutcome::FetchFailed { init_root, message } => {
            eprintln!("# fetching schemas from {}", init_root.display());
            eprintln!("# WARNING: schema fetch failed: {message}");
        }
        SchemaOutcome::NoInitRoot => {
            eprintln!("# no .terraform/providers found — skipping schema fetch");
        }
        SchemaOutcome::Bundled | SchemaOutcome::Skipped => {}
    }

    // Run diagnostics per file, grouped and sorted.
    let mut by_file: BTreeMap<String, Vec<lsp_types::Diagnostic>> = BTreeMap::new();
    let mut total = 0usize;
    for (uri, mut diags) in lint_all(&loaded.state) {
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
        let rel = relative_path(&uri, &root);
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
    use tracing_subscriber::{fmt, EnvFilter};

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
