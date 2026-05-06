//! Quick probe for `cached_latest_version` / `major_minor_of`.
//! Run: `cargo run -p tfls-provider-protocol --example cached_latest`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

fn main() {
    use tfls_provider_protocol::registry_versions::{cached_latest_version, major_minor_of};

    for (ns, name) in [
        ("hashicorp", "aws"),
        ("hashicorp", "azurerm"),
        ("hashicorp", "google"),
        ("hashicorp", "kubernetes"),
        ("hashicorp", "random"),
    ] {
        let latest = cached_latest_version(ns, name);
        let mm = latest.as_deref().and_then(major_minor_of);
        println!("{ns}/{name}: latest={latest:?} mm={mm:?}");
    }
}
