//! Wire-level tests for diagnostic-attached code-action quick-fixes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use serde_json::Value;
use support::TestClient;

/// The legacy-index deprecation diagnostic must offer a `.N` → `[N]`
/// quick-fix attached to the diagnostic.
#[tokio::test]
async fn legacy_index_offers_convert_quickfix() {
    let mut client = TestClient::new();
    client.initialize(None).await;

    let uri = "file:///mod/main.tf";
    client
        .did_open(uri, "output \"o\" {\n  value = var.list.0\n}\n")
        .await;
    client.settle(200).await;

    // Find the legacy-index diagnostic in the published set.
    let diags = client.last_diagnostics(uri).await;
    let legacy = diags
        .iter()
        .find(|d| {
            d.get("message")
                .and_then(|m| m.as_str())
                .is_some_and(|m| m.contains("legacy attribute-style index"))
        })
        .expect("legacy-index diagnostic published");

    let range = legacy.get("range").cloned().expect("range");
    let resp = client
        .code_action(uri, range, Value::Array(vec![legacy.clone()]))
        .await;

    let actions = resp["result"].as_array().cloned().unwrap_or_default();
    let convert = actions.iter().find(|a| {
        a.get("title")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.contains("Convert `.0` to `[0]`"))
    });
    assert!(convert.is_some(), "expected `.0`→`[0]` quick-fix, got {actions:?}");

    // The action carries a workspace edit replacing `.0` with `[0]`.
    let edit = &convert.unwrap()["edit"]["changes"][uri];
    assert!(
        edit.as_array()
            .and_then(|e| e.first())
            .and_then(|e| e.get("newText"))
            .and_then(|t| t.as_str())
            == Some("[0]"),
        "edit should insert `[0]`: {edit:?}"
    );

    client.shutdown().await;
}

/// `x == []` offers a `length(x) == 0` quick-fix; `x != []` offers
/// `length(x) > 0`.
#[tokio::test]
async fn empty_list_equality_offers_length_quickfix() {
    let mut client = TestClient::new();
    client.initialize(None).await;

    let uri = "file:///mod/main.tf";
    client
        .did_open(uri, "output \"o\" {\n  value = var.ids != []\n}\n")
        .await;
    client.settle(200).await;

    let diags = client.last_diagnostics(uri).await;
    let eq = diags
        .iter()
        .find(|d| {
            d.get("message")
                .and_then(|m| m.as_str())
                .is_some_and(|m| m.contains("comparing with `!= []`"))
        })
        .expect("empty-list diagnostic published");

    let range = eq.get("range").cloned().expect("range");
    let resp = client
        .code_action(uri, range, Value::Array(vec![eq.clone()]))
        .await;
    let actions = resp["result"].as_array().cloned().unwrap_or_default();
    let fix = actions.iter().find(|a| {
        a.get("title")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.contains("length(var.ids) > 0"))
    });
    assert!(fix.is_some(), "expected `length(var.ids) > 0` fix, got {actions:?}");

    client.shutdown().await;
}
