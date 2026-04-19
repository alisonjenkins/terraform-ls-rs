//! terraform-ls-rs CLI entry point.

use clap::Parser;
use tfls_lsp::Backend;
use tower_lsp::{LspService, Server};

#[derive(Debug, Parser)]
#[command(name = "tfls", version, about = "High-performance Terraform language server")]
struct Cli {
    /// Increase logging verbosity.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("terraform-ls-rs starting");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::build(Backend::new)
        .custom_method("terraform-ls/searchDocs", Backend::search_docs)
        .custom_method("terraform-ls/getDoc", Backend::get_doc)
        .custom_method("terraform-ls/getSnippet", Backend::get_snippet)
        .finish();

    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbosity {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("tfls={level},tower_lsp=info"))
    });

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
