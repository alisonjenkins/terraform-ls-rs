//! Integration tests for the one-call workspace loader (`workspace::load`)
//! and the parallel whole-workspace lint (`workspace::lint_all`).
//!
//! Fixture: `tests/fixtures/basic/` — two files, one undefined reference
//! (`local.missing`) and one unused declaration (`variable "unused"`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use tfls_engine::pipeline::compute_diagnostics;
use tfls_engine::workspace::{lint_all, load, LoadOptions, SchemaSource};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/basic")
}

fn sort_key(d: &lsp_types::Diagnostic) -> (u32, u32, String) {
    let code = match &d.code {
        Some(lsp_types::NumberOrString::String(s)) => s.clone(),
        Some(lsp_types::NumberOrString::Number(n)) => n.to_string(),
        None => String::new(),
    };
    (d.range.start.line, d.range.start.character, code)
}

#[tokio::test]
async fn load_populates_state_from_fixture() {
    let opts = LoadOptions {
        schemas: SchemaSource::None,
    };
    let loaded = load(&fixture_dir(), &opts)
        .await
        .expect("load must succeed");

    assert_eq!(loaded.file_count, 2, "fixture has main.tf + versions.tf");
    assert_eq!(loaded.dirs.len(), 1, "fixture is a single module dir");
}

#[tokio::test]
async fn lint_all_finds_undefined_reference_and_unused_declaration() {
    let opts = LoadOptions {
        schemas: SchemaSource::None,
    };
    let loaded = load(&fixture_dir(), &opts)
        .await
        .expect("load must succeed");

    let results = lint_all(&loaded.state);
    let all_diags: Vec<&lsp_types::Diagnostic> =
        results.iter().flat_map(|(_, diags)| diags.iter()).collect();

    assert!(
        all_diags.iter().any(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    "terraform_undefined_reference".to_string(),
                ))
                && d.message.contains("missing")
        }),
        "expected an undefined-reference diagnostic mentioning `missing`: {all_diags:#?}"
    );

    assert!(
        all_diags.iter().any(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    "terraform_unused_declarations".to_string(),
                ))
                && d.message.contains("unused")
        }),
        "expected an unused-declarations diagnostic mentioning `unused`: {all_diags:#?}"
    );
}

#[tokio::test]
async fn lint_all_matches_compute_diagnostics_per_uri() {
    let opts = LoadOptions {
        schemas: SchemaSource::None,
    };
    let loaded = load(&fixture_dir(), &opts)
        .await
        .expect("load must succeed");

    let results = lint_all(&loaded.state);
    assert!(!results.is_empty(), "fixture must yield at least one doc");

    for (uri, diags) in &results {
        let mut expected = compute_diagnostics(&loaded.state, uri);
        expected.sort_by_key(sort_key);

        let mut actual = diags.clone();
        actual.sort_by_key(sort_key);

        assert_eq!(
            actual, expected,
            "lint_all diagnostics for {uri} must match compute_diagnostics"
        );
    }
}
