//! Provider schema types, fetching, and caching.
//!
//! Phase 3: initial async CLI fetcher. Bundled and on-disk caching
//! follow in a later iteration.

pub mod error;
pub mod fetcher;
pub mod functions;
pub mod functions_cache;
pub mod types;

pub use error::SchemaError;
pub use fetcher::{SchemaFetcher, fetch_functions_from_cli, fetch_schema_from_cli};
pub use functions::{FunctionParameter, FunctionSignature, FunctionsSchema};
pub use functions_cache::{bundled as bundled_functions, load_functions};
pub use types::{
    AttributeSchema, BlockSchema, NestedBlockSchema, NestingMode, ProviderSchema, ProviderSchemas,
    Schema,
};
