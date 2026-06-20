//! Wire-level regression for the "formatting reverts my edits" bug.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use serde_json::{json, Value};
use support::TestClient;

/// Apply LSP TextEdits (line/char ranges) to `text`. Edits are applied in
/// reverse document order so earlier offsets stay valid.
fn apply_edits(text: &str, mut edits: Vec<Value>) -> String {
    let line_col_to_byte = |s: &str, line: usize, col: usize| -> usize {
        let mut off = 0usize;
        for (i, l) in s.split_inclusive('\n').enumerate() {
            if i == line {
                // col is a UTF-16 offset; tests use ASCII so it's byte-equal.
                return off + col.min(l.trim_end_matches('\n').len());
            }
            off += l.len();
        }
        off // line past EOF → end
    };
    edits.sort_by(|a, b| {
        let pa = (a["range"]["start"]["line"].as_u64(), a["range"]["start"]["character"].as_u64());
        let pb = (b["range"]["start"]["line"].as_u64(), b["range"]["start"]["character"].as_u64());
        pb.cmp(&pa)
    });
    let mut s = text.to_string();
    for e in edits {
        let sl = e["range"]["start"]["line"].as_u64().unwrap() as usize;
        let sc = e["range"]["start"]["character"].as_u64().unwrap() as usize;
        let el = e["range"]["end"]["line"].as_u64().unwrap() as usize;
        let ec = e["range"]["end"]["character"].as_u64().unwrap() as usize;
        let start = line_col_to_byte(&s, sl, sc);
        let end = line_col_to_byte(&s, el, ec);
        s.replace_range(start..end, e["newText"].as_str().unwrap());
    }
    s
}

#[tokio::test]
async fn formatting_preserves_a_freshly_edited_source_line() {
    let mut c = TestClient::new();
    c.initialize(None).await;
    let uri = "file:///mod/main.tf";

    // Open a formatted module block.
    c.did_open(uri, "module \"m\" {\n  source = \"../old\"\n}\n").await;
    c.settle(80).await;

    // User repoints `source` to a new on-disk path (FULL sync = whole doc).
    let edited = "module \"m\" {\n  source = \"../new-path\"\n}\n";
    c.did_change_full(uri, 2, edited).await;
    c.settle(80).await;

    // Format-on-save fires.
    let resp = c
        .request(
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": { "tabSize": 2, "insertSpaces": true }
            }),
        )
        .await;
    let edits = resp["result"].as_array().cloned().unwrap_or_default();
    let result = apply_edits(edited, edits.clone());

    assert!(
        result.contains("\"../new-path\""),
        "formatting must NOT revert the just-edited source value; got:\n{result}"
    );
    // And the edit set must be minimal — never a whole-document replace.
    for e in &edits {
        let spans_all = e["range"]["start"]["line"].as_u64() == Some(0)
            && e["range"]["end"]["line"].as_u64().unwrap_or(0) >= 3;
        assert!(!spans_all, "formatting emitted a whole-document replace: {e}");
    }
    c.shutdown().await;
}
