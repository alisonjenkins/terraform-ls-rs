//! Public error type for `tfls-format`.
//!
//! The crate is now a thin wrapper around [`tf_format`], so the
//! single failure mode is "the backend formatter returned an
//! error" — typically because the source didn't parse. We keep
//! a local error type so callers depend only on this crate's
//! enum, which lets us swap or layer backends in the future
//! without churning every call site.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FormatError {
    /// The underlying [`tf_format`] formatter failed. Most often
    /// an HCL parse error in the source; tf-format is panic-free
    /// (`#![deny(clippy::unwrap_used, clippy::expect_used)]`),
    /// so this is the only path through which a failure surfaces.
    #[error(transparent)]
    Backend(#[from] tf_format::error::FormatError),
}
