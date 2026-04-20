//! Thin wrapper over the LSP 3.15+ `$/progress` flow.
//!
//! tower-lsp 0.20 doesn't yet provide a dedicated
//! `work_done_progress_create` helper (see upstream TODO in
//! `tower-lsp-0.20.0/src/service/client.rs:194`), so we drive it
//! through the generic `send_request` / `send_notification`. That
//! still produces a standards-compliant progress stream, which
//! Fidget and any other `$/progress`-aware nvim plugin pick up
//! without additional configuration.
//!
//! Usage:
//!
//! ```ignore
//! let Some(p) = ProgressReporter::begin(&client, "Indexing workspace").await else {
//!     // client didn't accept the token — just proceed silently
//! };
//! p.report(Some("parsed 50/140".into()), Some(33)).await;
//! // …
//! p.end(Some("indexed 140 files".into())).await;
//! ```

use std::sync::atomic::{AtomicU64, Ordering};

use tower_lsp::Client;
use tower_lsp::lsp_types::{
    ProgressParams, ProgressParamsValue, ProgressToken, WorkDoneProgress,
    WorkDoneProgressBegin, WorkDoneProgressCreateParams, WorkDoneProgressEnd,
    WorkDoneProgressReport, notification::Progress, request::WorkDoneProgressCreate,
};

/// Monotonic counter that keeps tokens unique across concurrent
/// reporters. We prefix with `"tfls-"` so the tokens are easy to
/// spot in LSP traces.
static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct ProgressReporter {
    client: Client,
    token: ProgressToken,
    ended: bool,
}

impl ProgressReporter {
    /// Create a new progress token on the client and send a
    /// `Begin`. Returns `None` if the client rejects
    /// `window/workDoneProgress/create` — older clients or clients
    /// that don't support progress. Callers treat `None` as "just
    /// don't report progress" rather than an error.
    pub async fn begin(client: &Client, title: impl Into<String>) -> Option<Self> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let token = ProgressToken::String(format!("tfls-{n}"));
        if client
            .send_request::<WorkDoneProgressCreate>(WorkDoneProgressCreateParams {
                token: token.clone(),
            })
            .await
            .is_err()
        {
            return None;
        }
        client
            .send_notification::<Progress>(ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: title.into(),
                        cancellable: Some(false),
                        message: None,
                        percentage: None,
                    },
                )),
            })
            .await;
        Some(Self {
            client: client.clone(),
            token,
            ended: false,
        })
    }

    /// Update the progress state. `message` replaces the previous
    /// detail line; `percentage` is ignored by clients for
    /// indeterminate work (just don't pass it in that case).
    pub async fn report(&self, message: Option<String>, percentage: Option<u32>) {
        self.client
            .send_notification::<Progress>(ProgressParams {
                token: self.token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Report(
                    WorkDoneProgressReport {
                        cancellable: Some(false),
                        message,
                        percentage,
                    },
                )),
            })
            .await;
    }

    /// Send `End` and consume the reporter. Idempotent: if already
    /// ended the second call is a no-op.
    pub async fn end(mut self, message: Option<String>) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.client
            .send_notification::<Progress>(ProgressParams {
                token: self.token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                    WorkDoneProgressEnd { message },
                )),
            })
            .await;
    }
}
