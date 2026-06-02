//! Per-provider regression tests for the registry-doc parser.
//!
//! These run against real Markdown checked into
//! `tests/fixtures/registry_docs/` (one canonical resource per
//! provider, fetched from the Terraform Registry and frozen at a
//! known version). They cover the three big providers users hit
//! daily — `aws`, `azurerm`, `google` — and pin both:
//!
//! 1. Description coverage: known top-level attributes get a
//!    non-empty description after `parse_attribute_descriptions`.
//! 2. Enum mining: attributes whose docs spell out a valid-value
//!    list (`Valid values: …`, `can be either …`, etc.) end up
//!    with a populated `allowed_values`.
//!
//! When a future change to the parser silently drops one of
//! these, CI fails with a precise pointer at which provider's
//! Markdown shape stopped working.
//!
//! Refresh the fixtures with:
//!
//! ```bash
//! cargo run --bin tfls-doc-probe -- hashicorp/<provider>@<version> \
//!     --resource <name> --no-cache
//! cp ~/Library/Caches/terraform-ls-rs/provider-docs/hashicorp/<provider>/<version>/docs/<id>.md \
//!     crates/tfls-provider-protocol/tests/fixtures/registry_docs/<name>.md
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use tfls_provider_protocol::registry_docs::parse_attribute_descriptions;

fn assert_described<'a>(
    descs: &std::collections::HashMap<
        String,
        tfls_provider_protocol::registry_docs::ParsedAttribute,
    >,
    attr: &'a str,
    contains: &'a str,
) {
    let parsed = descs
        .get(attr)
        .unwrap_or_else(|| panic!("missing description for `{attr}`"));
    assert!(
        !parsed.description.trim().is_empty(),
        "`{attr}` parsed with empty description"
    );
    assert!(
        parsed.description.contains(contains),
        "`{attr}` description missing `{contains}`; got: {}",
        parsed.description
    );
}

fn assert_allowed_values(
    descs: &std::collections::HashMap<
        String,
        tfls_provider_protocol::registry_docs::ParsedAttribute,
    >,
    attr: &str,
    expected: &[&str],
) {
    let parsed = descs
        .get(attr)
        .unwrap_or_else(|| panic!("missing description for `{attr}`"));
    let got = parsed
        .allowed_values
        .as_deref()
        .unwrap_or_else(|| panic!("`{attr}` has no allowed_values mined; description: {}", parsed.description));
    let got: Vec<&str> = got.iter().map(String::as_str).collect();
    assert_eq!(
        got, expected,
        "`{attr}` allowed_values mismatch (description: {})",
        parsed.description
    );
}

#[test]
fn aws_instance_top_level_attrs_get_descriptions() {
    let md = include_str!("fixtures/registry_docs/aws_instance.md");
    let descs = parse_attribute_descriptions(md);

    // Sanity floor: aws_instance is a huge resource. If the
    // parser breaks, this drops to near-zero.
    assert!(
        descs.len() >= 30,
        "expected ≥30 attributes parsed from aws_instance.md, got {}",
        descs.len()
    );

    assert_described(&descs, "ami", "AMI to use for the instance");
    assert_described(&descs, "instance_type", "Instance type");
    assert_described(&descs, "monitoring", "detailed monitoring");
    assert_described(&descs, "hibernation", "support hibernation");
}

#[test]
fn aws_instance_tenancy_mines_valid_values_enum() {
    // "Valid values are `default`, `dedicated`, and `host`."
    let md = include_str!("fixtures/registry_docs/aws_instance.md");
    let descs = parse_attribute_descriptions(md);
    assert_allowed_values(&descs, "tenancy", &["default", "dedicated", "host"]);
}

#[test]
fn aws_instance_cpu_credits_mines_valid_values_enum() {
    // "Valid values include `standard` or `unlimited`."
    let md = include_str!("fixtures/registry_docs/aws_instance.md");
    let descs = parse_attribute_descriptions(md);
    assert_allowed_values(&descs, "cpu_credits", &["standard", "unlimited"]);
}

#[test]
fn azurerm_automation_runbook_top_level_attrs_get_descriptions() {
    let md = include_str!("fixtures/registry_docs/azurerm_automation_runbook.md");
    let descs = parse_attribute_descriptions(md);

    assert!(
        descs.len() >= 15,
        "expected ≥15 attributes parsed from azurerm_automation_runbook.md, got {}",
        descs.len()
    );

    // The user's reported bug: these were silently dropped before
    // the parser learned `## Arguments Reference` (plural).
    assert_described(&descs, "log_progress", "Progress log option");
    assert_described(&descs, "log_verbose", "Verbose log option");
    assert_described(&descs, "name", "name of the Runbook");
    assert_described(&descs, "automation_account_name", "automation account");
}

#[test]
fn azurerm_automation_runbook_runbook_type_mines_can_be_either_enum() {
    // "can be either `Graph`, `GraphPowerShell`, `GraphPowerShellWorkflow`,
    //  `PowerShellWorkflow`, `PowerShell`, `PowerShell72`, `Python3`,
    //  `Python2` or `Script`."
    let md = include_str!("fixtures/registry_docs/azurerm_automation_runbook.md");
    let descs = parse_attribute_descriptions(md);
    assert_allowed_values(
        &descs,
        "runbook_type",
        &[
            "Graph",
            "GraphPowerShell",
            "GraphPowerShellWorkflow",
            "PowerShellWorkflow",
            "PowerShell",
            "PowerShell72",
            "Python3",
            "Python2",
            "Script",
        ],
    );
}

#[test]
fn google_storage_bucket_top_level_attrs_get_descriptions() {
    let md = include_str!("fixtures/registry_docs/google_storage_bucket.md");
    let descs = parse_attribute_descriptions(md);

    assert!(
        descs.len() >= 30,
        "expected ≥30 attributes parsed from google_storage_bucket.md, got {}",
        descs.len()
    );

    assert_described(&descs, "name", "name of the bucket");
    assert_described(&descs, "location", "GCS location");
    assert_described(&descs, "storage_class", "Storage Class");
    assert_described(&descs, "force_destroy", "deleting a bucket");
}

#[test]
fn google_storage_bucket_storage_class_mines_supported_values() {
    // "Supported values include: `STANDARD`, `MULTI_REGIONAL`,
    //  `REGIONAL`, `NEARLINE`, `COLDLINE`, `ARCHIVE`."
    let md = include_str!("fixtures/registry_docs/google_storage_bucket.md");
    let descs = parse_attribute_descriptions(md);
    assert_allowed_values(
        &descs,
        "storage_class",
        &[
            "STANDARD",
            "MULTI_REGIONAL",
            "REGIONAL",
            "NEARLINE",
            "COLDLINE",
            "ARCHIVE",
        ],
    );
}
