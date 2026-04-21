//! Probe a specific resource/attribute in an installed schema — useful
//! for verifying hover-worthy text is actually present.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

#[tokio::main]
async fn main() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    let args: Vec<String> = std::env::args().collect();
    let (workspace, resource, attr) = match args.as_slice() {
        [_, ws, r, a] => (ws.as_str(), r.as_str(), a.as_str()),
        _ => {
            eprintln!("usage: probe <workspace> <resource_type> <attr>");
            std::process::exit(2);
        }
    };

    let terraform_dir = std::path::PathBuf::from(workspace).join(".terraform");
    let schemas = tfls_provider_protocol::fetch_schemas_from_plugins(&terraform_dir, None)
        .await
        .expect("fetch");

    println!("providers loaded: {}", schemas.provider_schemas.len());
    let mut found = false;
    for (addr, ps) in &schemas.provider_schemas {
        if let Some(s) = ps.resource_schemas.get(resource) {
            found = true;
            println!("\n{resource} in {addr}");
            if let Some(a) = s.block.attributes.get(attr) {
                println!("  {attr}:");
                println!(
                    "    required={} optional={} computed={} deprecated={}",
                    a.required, a.optional, a.computed, a.deprecated
                );
                println!("    description: {:?}", a.description);
            } else {
                println!("  attribute `{attr}` not found; available:");
                let mut names: Vec<&String> = s.block.attributes.keys().collect();
                names.sort();
                for n in names.iter().take(20) {
                    println!("    {n}");
                }
            }
        }
    }
    if !found {
        println!("resource `{resource}` not found in any loaded provider");
    }
}
