//! terraform-ls-rs CLI entry point.

use clap::Parser;
use tfls_lsp::Backend;
use tower_lsp_server::{LspService, Server};

#[derive(Debug, Parser)]
#[command(
    name = "tfls",
    version,
    about = "High-performance Terraform language server"
)]
struct Cli {
    /// Increase logging verbosity.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    // Cap rayon's global pool so the bulk workspace scan can't
    // saturate every CPU core and starve the tokio runtime's
    // LSP handlers. See `tfls_lsp::configure_rayon_pool` for
    // the policy.
    tfls_lsp::configure_rayon_pool();

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
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbosity {
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("tfls={level},tower_lsp_server=info"))
    });

    // File sink path. Defaults to `$XDG_RUNTIME_DIR/tfls.log`
    // (typically `/run/user/<uid>/tfls.log`) — accessible to the
    // user but cleared on logout. Falls back to the platform temp
    // directory (`/tmp` on unix, `%TEMP%` on Windows). Override with
    // `TFLS_LOG_FILE=…`. Critical because tfls runs under `lspmux`
    // daemon mode where stderr is detached to `/dev/null` and
    // journald never sees the trace stream.
    let log_path = resolve_log_path(
        std::env::var_os("TFLS_LOG_FILE"),
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::temp_dir(),
        std::process::id(),
    );
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        use std::sync::Mutex;
        let _ = fmt()
            .with_env_filter(filter)
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .try_init();
        return;
    }

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

/// Resolve the trace-log file path, cross-platform.
///
/// Precedence:
/// 1. `TFLS_LOG_FILE` — explicit override, used verbatim.
/// 2. `$XDG_RUNTIME_DIR/tfls.log` — the user-private runtime dir (unix,
///    mode `0700`); a stable name there keeps the log easy to find.
/// 3. `<temp_dir>/tfls-<pid>.log` — the platform temp dir (`%TEMP%` on
///    Windows, `/tmp` on unix). That dir can be shared between users, so
///    the pid in the filename avoids colliding with — or being
///    pre-created / symlinked by — another process or user.
///
/// Paths are built with `Path::join` rather than string formatting so the
/// correct separator is used on every platform.
fn resolve_log_path(
    explicit: Option<std::ffi::OsString>,
    xdg_runtime_dir: Option<std::ffi::OsString>,
    temp_dir: std::path::PathBuf,
    pid: u32,
) -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Some(explicit) = explicit {
        return PathBuf::from(explicit);
    }
    if let Some(rt) = xdg_runtime_dir.filter(|s| !s.is_empty()) {
        return PathBuf::from(rt).join("tfls.log");
    }
    temp_dir.join(format!("tfls-{pid}.log"))
}

#[cfg(test)]
mod tests {
    use super::resolve_log_path;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    #[test]
    fn explicit_override_used_verbatim() {
        let got = resolve_log_path(
            Some(OsString::from("/custom/tfls.log")),
            Some(OsString::from("/run/user/1000")),
            PathBuf::from("/tmp"),
            42,
        );
        assert_eq!(got, Path::new("/custom/tfls.log"));
    }

    #[test]
    fn prefers_xdg_runtime_dir_with_stable_name() {
        let got = resolve_log_path(
            None,
            Some(OsString::from("/run/user/1000")),
            PathBuf::from("/tmp"),
            42,
        );
        assert_eq!(got, Path::new("/run/user/1000").join("tfls.log"));
    }

    #[test]
    fn falls_back_to_temp_dir_with_pid_when_no_xdg() {
        // Windows hits this branch (no XDG_RUNTIME_DIR); the pid keeps the
        // name unique in the shared temp dir, and `join` yields the right
        // separator on each platform.
        let temp = PathBuf::from("/tmp");
        let got = resolve_log_path(None, None, temp.clone(), 4242);
        assert_eq!(got, temp.join("tfls-4242.log"));
    }

    #[test]
    fn empty_xdg_runtime_dir_falls_back_to_temp() {
        let got = resolve_log_path(None, Some(OsString::new()), PathBuf::from("/tmp"), 7);
        assert_eq!(got, Path::new("/tmp").join("tfls-7.log"));
    }
}
