//! Wire-level test for `.tfls.json` project config applied at
//! `initialize`: a repo checks in a `rules` policy and the server
//! honours it for the very first diagnostics push, before any
//! `initializationOptions` / `didChangeConfiguration` round trip.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::fs;

use serde_json::Value;
use support::TestClient;
use url::Url;

const UNUSED_VAR_MAIN_TF: &str =
    "provider \"null\" {}\n\nvariable \"unused\" {\n  type = string\n}\n";

fn has_code(diags: &[Value], code: &str) -> bool {
    diags.iter().any(|d| {
        d.get("code")
            .and_then(|c| c.as_str())
            .map(|c| c == code)
            .unwrap_or(false)
    })
}

#[tokio::test]
async fn project_config_file_suppresses_rule_at_initialize() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    fs::write(dir.join("main.tf"), UNUSED_VAR_MAIN_TF).expect("write main.tf");
    fs::write(
        dir.join(".tfls.json"),
        r#"{"rules": {"terraform_unused_declarations": "off"}}"#,
    )
    .expect("write .tfls.json");

    let root_uri = Url::from_file_path(dir).expect("file url").to_string();
    let main_uri = Url::from_file_path(dir.join("main.tf"))
        .expect("file url")
        .to_string();

    let mut client = TestClient::new();
    client.initialize(Some(&root_uri)).await;
    client.did_open(&main_uri, UNUSED_VAR_MAIN_TF).await;
    client.settle(250).await;

    let diags = client.last_diagnostics(&main_uri).await;
    assert!(
        !has_code(&diags, "terraform_unused_declarations"),
        "the checked-in .tfls.json should have suppressed \
         terraform_unused_declarations; diags: {diags:?}"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn without_project_config_file_rule_still_fires() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    fs::write(dir.join("main.tf"), UNUSED_VAR_MAIN_TF).expect("write main.tf");

    let root_uri = Url::from_file_path(dir).expect("file url").to_string();
    let main_uri = Url::from_file_path(dir.join("main.tf"))
        .expect("file url")
        .to_string();

    let mut client = TestClient::new();
    client.initialize(Some(&root_uri)).await;
    client.did_open(&main_uri, UNUSED_VAR_MAIN_TF).await;
    client.settle(250).await;

    let diags = client.last_diagnostics(&main_uri).await;
    assert!(
        has_code(&diags, "terraform_unused_declarations"),
        "positive control: without a .tfls.json the rule should fire; diags: {diags:?}"
    );

    client.shutdown().await;
}
