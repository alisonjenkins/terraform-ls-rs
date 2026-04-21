//! Live smoke test: spawn every provider cached under a real
//! `.terraform/` tree and print a summary of what comes back.
//!
//! ```sh
//! cargo run --example fetch_local -- /path/to/terraform/workspace
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::path::PathBuf;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "debug".into()),
        )
        .with_target(true)
        .init();

    // Install default rustls provider (aws_lc_rs) before any TLS work.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    // Optional CPU profiler — set `TFLS_FLAMEGRAPH=/tmp/out.svg` to
    // capture an in-process flamegraph of everything from here to
    // program exit. Uses `pprof` (signal-based sampler), no perf
    // permissions needed. No-op when the env var is unset.
    //
    // pprof is Unix-only; on Windows the env var is silently ignored
    // and profiling isn't supported from this harness (use
    // WPR/WPA or Superluminal instead).
    #[cfg(unix)]
    let guard = std::env::var("TFLS_FLAMEGRAPH").ok().map(|_| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(997)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .expect("pprof guard")
    });
    #[cfg(not(unix))]
    let guard: Option<()> = {
        if std::env::var("TFLS_FLAMEGRAPH").is_ok() {
            eprintln!(
                "TFLS_FLAMEGRAPH is set but pprof is Unix-only; \
                 ignoring — use WPR/WPA or Superluminal on Windows."
            );
        }
        None
    };

    let workspace = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: fetch_local <workspace-dir>");
    let terraform_dir = workspace.join(".terraform");
    if !terraform_dir.is_dir() {
        eprintln!(
            "{} has no .terraform/ directory — nothing to fetch",
            workspace.display()
        );
        std::process::exit(2);
    }

    println!("Discovering providers under {}", terraform_dir.display());
    let binaries = tfls_provider_protocol::discover_providers(&terraform_dir)
        .expect("discovery");
    println!("  found {} provider binaries", binaries.len());
    for b in &binaries {
        println!(
            "    - {} v{}  ({})",
            b.full_address(),
            b.version,
            b.binary.display()
        );
    }

    println!();
    println!("Fetching schemas via plugin gRPC (one provider at a time for diag)…");
    let start = std::time::Instant::now();
    // Manually iterate so we can print the full error chain, not just
    // the outer `WARN` message that the library logs.
    for bin in &binaries {
        println!("  - {}", bin.binary.display());
        match tfls_provider_protocol::client::fetch_provider_schema(bin, None).await {
            Ok(s) => println!(
                "    ok ({} resources, {} data sources)",
                s.resource_schemas.len(),
                s.data_source_schemas.len()
            ),
            Err(e) => {
                println!("    ERR: {e}");
                let mut src: Option<&(dyn std::error::Error + 'static)> =
                    Some(&e as &(dyn std::error::Error + 'static));
                let mut depth = 0usize;
                while let Some(s) = src {
                    println!("      [{depth}] {s}");
                    src = s.source();
                    depth += 1;
                }
            }
        }
    }
    let schemas = tfls_provider_protocol::fetch_schemas_from_plugins(&terraform_dir, None)
        .await
        .expect("fetch");
    let elapsed = start.elapsed();

    println!(
        "  got {} provider schema entries in {:?}",
        schemas.provider_schemas.len(),
        elapsed,
    );
    for (addr, ps) in &schemas.provider_schemas {
        println!(
            "    - {addr}: {} resources, {} data sources",
            ps.resource_schemas.len(),
            ps.data_source_schemas.len(),
        );
    }

    // Spot-check: find one well-known AWS attribute and print its docs.
    if let Some(ps) = schemas
        .provider_schemas
        .iter()
        .find(|(k, _)| k.contains("aws"))
        .map(|(_, v)| v)
    {
        if let Some(s) = ps.resource_schemas.get("aws_ses_domain_identity") {
            println!();
            println!("Sanity check — aws_ses_domain_identity.domain:");
            if let Some(attr) = s.block.attributes.get("domain") {
                println!("  description: {:?}", attr.description);
                println!("  required: {}, optional: {}", attr.required, attr.optional);
            } else {
                println!("  (no `domain` attribute found — schema shape unexpected)");
            }
        }
    }

    // Write flamegraph if profiler was armed (Unix only).
    #[cfg(unix)]
    if let (Some(g), Ok(out)) = (guard, std::env::var("TFLS_FLAMEGRAPH")) {
        match g.report().build() {
            Ok(report) => {
                let f = std::fs::File::create(&out).expect("create flamegraph file");
                report.flamegraph(f).expect("write flamegraph");
                println!("flamegraph written to {out}");
            }
            Err(e) => eprintln!("failed to build profile report: {e}"),
        }
    }
    #[cfg(not(unix))]
    let _ = guard;
}
