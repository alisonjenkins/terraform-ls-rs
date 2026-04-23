//! Runtime-flavor-aware wrapper around `tokio::task::block_in_place`.
//!
//! In production the server runs on the tokio multi-threaded
//! runtime, where `block_in_place` tells the runtime "I'm about
//! to do sync work — shed this worker thread and steal other
//! tasks onto a new one so the reactor stays responsive." That
//! is what we want for all the heavy sync hot-paths: fs walks,
//! serde_json over megabyte-scale blobs, rayon scopes.
//!
//! But `block_in_place` **panics** on a current-thread runtime,
//! which is what plain `#[tokio::test]` gives you. The codebase
//! has ~190 such tests and annotating each with
//! `flavor = "multi_thread"` would be churn. So this helper
//! falls back to a plain call under a current-thread runtime:
//! tests still pass (they have no reactor to starve anyway)
//! and production still gets the real off-reactor behaviour.

/// Run the closure, using `tokio::task::block_in_place` if the
/// current tokio runtime is multi-threaded, otherwise calling
/// it directly. See the module docs for why.
pub fn run<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        // Current-thread runtime, or no runtime at all (e.g. a
        // plain `fn` called from a sync test). Either way, just
        // run the closure synchronously.
        _ => f(),
    }
}
