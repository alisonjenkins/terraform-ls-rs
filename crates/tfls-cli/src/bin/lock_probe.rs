//! End-to-end lock-file invalidation probe.
//!
//! Boots the SAME plumbing the real LSP server uses — `StateStore`,
//! `JobQueue`, `tfls_lsp::indexer::spawn_watcher` — against a
//! temporary workspace, simulates a sequence of `terraform init`
//! events by rewriting `.terraform.lock.hcl`, and reports what
//! `state.lock_file_for(...)` and `compute_diagnostics(...)` see
//! after each. Catches the kind of cache-key / watcher-vs-uri-path
//! / debounce mismatches that don't show up in in-process unit
//! tests but bite users when the actual notify-debouncer-full
//! crate runs.
//!
//! Usage:
//!
//!   # Run the canned regression sequence (open → upgrade →
//!   # downgrade → re-upgrade), report at each step:
//!   cargo run --bin tfls-lock-probe
//!
//!   # Override per-step wait so slow filesystems get a chance:
//!   cargo run --bin tfls-lock-probe -- --wait-ms 800
//!
//!   # Verbose: also dump the parsed lock entries seen at each step:
//!   cargo run --bin tfls-lock-probe -- --verbose

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tfls_lsp::Backend;
use tfls_state::DocumentState;
use tower_lsp_server::LspService;
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "tfls-lock-probe",
    about = "End-to-end probe for .terraform.lock.hcl invalidation"
)]
struct Cli {
    /// Milliseconds to wait between mutating the lock file and
    /// querying state. Should comfortably exceed the watcher's
    /// debounce window (default in `spawn_watcher` is ~250ms).
    #[arg(long, default_value_t = 600)]
    wait_ms: u64,

    /// Dump every parsed lock entry seen at each step.
    #[arg(long)]
    verbose: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();
    let dir = std::env::temp_dir().join(format!(
        "tfls-lock-probe-{}-{}",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    println!("workspace: {}", dir.display());
    println!(
        "workspace canonical: {}",
        dir.canonicalize().unwrap().display()
    );

    // Seed: main.tf + initial lock matching `~> 4.71.0`.
    let main_tf = dir.join("main.tf");
    std::fs::write(
        &main_tf,
        r#"terraform {
  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "~> 4.71.0"
    }
  }
}
"#,
    )
    .unwrap();
    write_lock(&dir, "4.71.0");

    // Boot a Backend the same way the LSP service would.
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let state = inner.state.clone();
    let jobs = inner.jobs.clone();
    let backend = Backend::with_shared_state(inner.client.clone(), state.clone(), jobs.clone());

    // Mirror `did_open`: insert the document into state.
    let main_uri = Url::from_file_path(&main_tf).unwrap();
    let body = std::fs::read_to_string(&main_tf).unwrap();
    backend
        .state
        .upsert_document(DocumentState::new(main_uri.clone(), &body, 1));

    // Spawn the actual watcher — same code path as `initialize` →
    // `spawn_workspace_watcher` → `spawn_watcher`.
    let watcher = tfls_lsp::indexer::spawn_watcher(
        Arc::clone(&state),
        Arc::clone(&jobs),
        dir.clone(),
        None, // no client → no inlay-hint refresh / publish-for-dir
    )
    .expect("spawn_watcher");

    // Step 0: initial state. Constraint admits 4.71.0; no warning.
    report(
        &state,
        &main_uri,
        &dir,
        "step 0: open with constraint `~> 4.71.0`, lock 4.71.0",
        cli.verbose,
    );

    // Helper: rewrite lock + wait for the watcher to debounce.
    let mutate = |label: &str, version: &str| {
        std::thread::sleep(Duration::from_millis(50));
        write_lock(&dir, version);
        std::thread::sleep(Duration::from_millis(cli.wait_ms));
        report(&state, &main_uri, &dir, label, cli.verbose);
    };

    mutate("step 1: lock → 2.71.0 (drift; warning expected)", "2.71.0");
    mutate(
        "step 2: lock → 4.71.0 (back in band; warning should clear)",
        "4.71.0",
    );
    mutate(
        "step 3: lock → 1.0.0 (drift again; warning expected)",
        "1.0.0",
    );
    mutate(
        "step 4: lock → 4.71.0 (back; warning should clear)",
        "4.71.0",
    );

    // Schema-aware version hopping. The real LSP populates
    // `state.schemas` from gRPC against the on-disk binary,
    // which we can't emulate without a real provider binary.
    // Stub by directly calling `state.install_schemas` with
    // synthetic schemas representing two provider versions:
    //
    //   * `OLD` — no `runtime_environment_name` attribute.
    //   * `NEW` — has `runtime_environment_name`.
    //
    // Pair each lock-file rewrite with the matching schema
    // install so we can verify diagnostics, attribute lookup
    // (completion / hover surface), and the drift warning all
    // see consistent state across the hop.
    println!("\n=== schema-coupled version hopping ===");
    let schemas_old = synthetic_schemas(/* with_runtime_env */ false);
    let schemas_new = synthetic_schemas(/* with_runtime_env */ true);

    let main_uri_for_runbook = main_uri.clone();
    let hop = |label: &str, lock_version: &str, install_new_schema: bool| {
        std::thread::sleep(Duration::from_millis(50));
        write_lock(&dir, lock_version);
        // Mimic what the schema fetch would do post-init.
        if install_new_schema {
            state.install_schemas(schemas_new.clone());
            state.record_installed_version(
                tfls_core::ProviderAddress::new("registry.terraform.io", "hashicorp", "azurerm"),
                lock_version.to_string(),
            );
        } else {
            state.install_schemas(schemas_old.clone());
            state.record_installed_version(
                tfls_core::ProviderAddress::new("registry.terraform.io", "hashicorp", "azurerm"),
                lock_version.to_string(),
            );
        }
        std::thread::sleep(Duration::from_millis(cli.wait_ms));
        report_full(&state, &main_uri_for_runbook, &dir, label, cli.verbose);
    };

    hop(
        "hop 1: lock 4.71.0 + new schema (with attr)",
        "4.71.0",
        true,
    );
    hop("hop 2: lock 2.71.0 + old schema (no attr)", "2.71.0", false);
    hop("hop 3: lock 4.71.0 + new schema", "4.71.0", true);
    hop("hop 4: lock 1.0.0 + old schema", "1.0.0", false);
    hop("hop 5: lock 4.71.0 + new schema (final)", "4.71.0", true);

    watcher.abort();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Synthesise a `ProviderSchemas` document with one
/// `azurerm_automation_runbook` resource. The variant flag toggles
/// whether the resource's block exposes a
/// `runtime_environment_name` attribute — used to model the
/// "old vs new" provider versions.
fn synthetic_schemas(with_runtime_env: bool) -> tfls_schema::ProviderSchemas {
    use std::collections::HashMap;
    use tfls_schema::{AttributeSchema, BlockSchema, ProviderSchema, ProviderSchemas, Schema};
    let mut block = BlockSchema::default();
    block.attributes.insert(
        "name".to_string(),
        AttributeSchema {
            description: Some("(Required) name".to_string()),
            required: true,
            ..Default::default()
        },
    );
    block.attributes.insert(
        "location".to_string(),
        AttributeSchema {
            description: Some("(Required) location".to_string()),
            required: true,
            ..Default::default()
        },
    );
    if with_runtime_env {
        block.attributes.insert(
            "runtime_environment_name".to_string(),
            AttributeSchema {
                description: Some("(Optional) Runtime environment name".to_string()),
                optional: true,
                ..Default::default()
            },
        );
    }
    let mut resource_schemas = HashMap::new();
    resource_schemas.insert(
        "azurerm_automation_runbook".to_string(),
        Schema { version: 0, block },
    );
    let empty_schema = Schema {
        version: 0,
        block: BlockSchema::default(),
    };
    let provider_schema = ProviderSchema {
        provider: empty_schema,
        resource_schemas,
        data_source_schemas: HashMap::new(),
    };
    let mut provider_schemas = HashMap::new();
    provider_schemas.insert(
        "registry.terraform.io/hashicorp/azurerm".to_string(),
        provider_schema,
    );
    ProviderSchemas {
        format_version: "1.0".to_string(),
        provider_schemas,
    }
}

fn write_lock(dir: &std::path::Path, azurerm_version: &str) {
    let body = format!(
        r#"provider "registry.opentofu.org/hashicorp/azurerm" {{
  version     = "{azurerm_version}"
  constraints = "~> 4.71.0"
  hashes      = []
}}
"#
    );
    std::fs::write(dir.join(".terraform.lock.hcl"), body).unwrap();
}

/// Richer report that also queries the schema for
/// `azurerm_automation_runbook` and reports whether
/// `runtime_environment_name` is exposed as an attribute. This
/// is the single signal that completion / hover / schema-driven
/// diagnostics consult downstream — if it's stale, every
/// schema-aware surface lies.
fn report_full(
    state: &tfls_state::StateStore,
    uri: &Url,
    dir: &std::path::Path,
    label: &str,
    verbose: bool,
) {
    report(state, uri, dir, label, verbose);
    // Schema lookup for the resource we care about.
    let schema = state.resource_schema("azurerm_automation_runbook");
    match schema {
        Some(s) => {
            let has_runtime_env = s.block.attributes.contains_key("runtime_environment_name");
            let attr_count = s.block.attributes.len();
            println!(
                "schema: azurerm_automation_runbook present, {attr_count} attrs, \
                 runtime_environment_name = {has_runtime_env}"
            );
        }
        None => println!("schema: azurerm_automation_runbook MISSING"),
    }
    // Installed version (used by upgrade-hint / etc).
    let addr = tfls_core::ProviderAddress::new("registry.terraform.io", "hashicorp", "azurerm");
    if let Some(v) = state.installed_version(&addr) {
        println!("installed_version(azurerm) = {v}");
    } else {
        println!("installed_version(azurerm) = <unset>");
    }
}

fn report(
    state: &tfls_state::StateStore,
    uri: &Url,
    dir: &std::path::Path,
    label: &str,
    verbose: bool,
) {
    println!("\n--- {label} ---");
    let lock = state.lock_file_for(dir);
    match &lock {
        Some(l) => println!("lock_file_for(non_canonical): {} entries", l.len()),
        None => println!("lock_file_for(non_canonical): None"),
    }
    let canon = dir.canonicalize().unwrap();
    let lock_canon = state.lock_file_for(&canon);
    match &lock_canon {
        Some(l) => println!("lock_file_for(canonical):     {} entries", l.len()),
        None => println!("lock_file_for(canonical):     None"),
    }
    if verbose {
        if let Some(l) = &lock {
            for (addr, entry) in l.iter() {
                println!(
                    "  - {}/{}/{} = {}",
                    addr.hostname, addr.namespace, addr.r#type, entry.version
                );
            }
        }
    }

    let diags = tfls_lsp::handlers::document::compute_diagnostics(state, uri);
    let drift: Vec<_> = diags
        .iter()
        .filter(|d| d.message.contains("does not admit"))
        .collect();
    println!("diagnostics: {} total, {} drift", diags.len(), drift.len());
    for d in drift {
        println!("  ⚠ {}", d.message.lines().next().unwrap_or(""));
    }
}
