//! `tower-lsp` backend for terraform-ls-rs.

pub mod backend;
pub mod capabilities;
pub mod error;
pub mod handlers;
pub mod indexer;
pub mod progress;

pub use backend::Backend;
pub use error::LspError;

/// Size rayon's global thread pool so background parallel
/// work (the bulk workspace scan's parse + diagnostic-compute
/// passes) leaves headroom for the tokio runtime's LSP
/// handlers. Without this, `rayon::par_iter` saturates all
/// CPU cores during indexing and the async handlers (did_open,
/// did_change, hover, completion, pull diagnostics) queue up
/// waiting for CPU — the "LSP feels slow during indexing"
/// symptom.
///
/// Policy: reserve 2 cores for tokio workers, give everything
/// else to rayon. On a 2-core machine we clamp to 2 rayon
/// threads (floor of 1 is useless; 2 lets rayon still
/// parallelise).
///
/// Respects `TFLS_RAYON_THREADS` env override for users who
/// want to tune explicitly. Idempotent-ish: calling twice is
/// a hard error from rayon — it logs a warning and continues.
///
/// Call once at server startup, BEFORE the tokio runtime
/// dispatches any request that uses rayon.
pub fn configure_rayon_pool() {
    let total = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let from_env = std::env::var("TFLS_RAYON_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok());
    let target = match from_env {
        Some(n) if n >= 1 => n,
        _ => {
            // Leave 2 cores for tokio; floor at 2 so single-
            // and dual-core machines aren't starved of rayon
            // parallelism entirely.
            std::cmp::max(2, total.saturating_sub(2))
        }
    };
    match rayon::ThreadPoolBuilder::new()
        .num_threads(target)
        .thread_name(|i| format!("tfls-rayon-{i}"))
        .build_global()
    {
        Ok(()) => {
            tracing::info!(
                rayon_threads = target,
                total_cpus = total,
                "rayon pool sized for indexing headroom"
            );
        }
        Err(e) => {
            // Rayon only lets you set the global pool once.
            // Subsequent calls return an error; that's not
            // fatal — the pool already exists.
            tracing::warn!(
                error = %e,
                "rayon global pool already configured; leaving as-is"
            );
        }
    }
}
