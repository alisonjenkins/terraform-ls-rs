//! Fetch from one provider binary, dump everything rustls did.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rustls=trace,tfls_provider_protocol=debug".into()),
        )
        .with_target(true)
        .init();

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    let path = std::env::args().nth(1).expect("usage: single_fetch <path>");
    let bin = tfls_provider_protocol::ProviderBinary {
        binary: std::path::PathBuf::from(&path),
        registry_host: "registry.opentofu.org".into(),
        namespace: "hashicorp".into(),
        name: "random".into(),
        version: "3.8.1".into(),
    };

    match tfls_provider_protocol::client::fetch_provider_schema(&bin, None).await {
        Ok(s) => println!(
            "OK: {} resources, {} data sources",
            s.resource_schemas.len(),
            s.data_source_schemas.len()
        ),
        Err(e) => {
            println!("ERR: {e}");
            let mut src: Option<&(dyn std::error::Error + 'static)> =
                Some(&e as &(dyn std::error::Error + 'static));
            let mut i = 0;
            while let Some(s) = src {
                println!("  [{i}] {s}");
                src = s.source();
                i += 1;
            }
        }
    }
}
