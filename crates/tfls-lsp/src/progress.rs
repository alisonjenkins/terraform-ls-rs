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
//! ## Ordering model
//!
//! Every outgoing Report / End for a given token is funnelled through
//! a single per-token drain task via a `mpsc::UnboundedSender`. That
//! gives two properties Fidget depends on:
//!
//! - **Order preservation**: whoever sends a Report *before* End in
//!   program order will be observed that way on the wire. Without
//!   this, `ReportSender::send_detached` calls (spawned tokio tasks
//!   with no mutual ordering) could race `ProgressReporter::end`,
//!   letting the End land mid-stream. Fidget then sees a stray
//!   post-End Report and keeps the widget stuck at its last
//!   percentage — the "stuck at 99%" symptom.
//! - **End delivery even on panic / drop**: the `Drop` impl enqueues
//!   a terminating End if one hasn't been sent yet. Even if the
//!   scan task unwinds halfway through, Fidget gets a clean close.
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

use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
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

/// Messages pushed into a reporter's drain queue.
enum DrainMsg {
    Report {
        message: Option<String>,
        percentage: Option<u32>,
    },
    End {
        message: Option<String>,
    },
}

pub struct ProgressReporter {
    tx: UnboundedSender<DrainMsg>,
    drain_handle: Option<JoinHandle<()>>,
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

        // Drain task — single consumer, so every Report / End goes
        // out in enqueue order even if upstream senders are a mix
        // of `send_detached` (fire-and-forget) and awaited calls.
        let (tx, mut rx) = mpsc::unbounded_channel::<DrainMsg>();
        let drain_client = client.clone();
        let drain_token = token.clone();
        let drain_handle = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    DrainMsg::Report {
                        message,
                        percentage,
                    } => {
                        drain_client
                            .send_notification::<Progress>(ProgressParams {
                                token: drain_token.clone(),
                                value: ProgressParamsValue::WorkDone(
                                    WorkDoneProgress::Report(WorkDoneProgressReport {
                                        cancellable: Some(false),
                                        message,
                                        percentage,
                                    }),
                                ),
                            })
                            .await;
                    }
                    DrainMsg::End { message } => {
                        drain_client
                            .send_notification::<Progress>(ProgressParams {
                                token: drain_token.clone(),
                                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                                    WorkDoneProgressEnd { message },
                                )),
                            })
                            .await;
                        // Stop draining — any further Reports are
                        // silently dropped (protocol-invalid after
                        // End anyway).
                        break;
                    }
                }
            }
        });

        Some(Self {
            tx,
            drain_handle: Some(drain_handle),
            ended: false,
        })
    }

    /// Update the progress state. `message` replaces the previous
    /// detail line; `percentage` is ignored by clients for
    /// indeterminate work (just don't pass it in that case).
    pub async fn report(&self, message: Option<String>, percentage: Option<u32>) {
        // The channel is unbounded — send is immediate and never
        // blocks. Order is preserved because every sender funnels
        // through the same drain task.
        let _ = self.tx.send(DrainMsg::Report {
            message,
            percentage,
        });
    }

    /// Get a cheap, cloneable handle that can send `Report`
    /// notifications from either sync or async callers. Used when a
    /// sync callback (e.g. the schema-fetch per-provider hook) needs
    /// to tick the same progress token that an async orchestrator
    /// began. The sender shares the reporter's drain queue, so its
    /// Reports are guaranteed to land on the wire before the
    /// reporter's End — even when fired from detached tasks.
    pub fn sender(&self) -> ReportSender {
        ReportSender {
            tx: self.tx.clone(),
        }
    }

    /// Send `End` and consume the reporter. Idempotent: if already
    /// ended the second call is a no-op. Waits for the drain task
    /// to flush every pending Report before returning, so the caller
    /// can rely on "End has been written" once `.await` resolves.
    pub async fn end(mut self, message: Option<String>) {
        if self.ended {
            return;
        }
        self.ended = true;
        let _ = self.tx.send(DrainMsg::End { message });
        // Dropping tx lets the drain task's `recv()` return None
        // after processing End, unblocking the join below.
        //
        // We move tx out of self so Drop (which also tries to send
        // End on a tx that still exists) becomes a no-op after this
        // method returns.
        if let Some(handle) = self.drain_handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        // Normal path: `end()` was called — nothing to do.
        if self.ended {
            return;
        }
        // Panic / early-return path: enqueue a terminating End so
        // Fidget doesn't see an orphaned Begin/Report stream and
        // freeze at the last percentage. Best-effort — if the
        // channel is already closed (shutdown race) we silently
        // accept the stranded token, which is still better than
        // the hang.
        let _ = self.tx.send(DrainMsg::End {
            message: Some("cancelled".to_string()),
        });
    }
}

/// Cheap cloneable handle for sending `Report` notifications on an
/// existing progress token from sync callers.
#[derive(Clone)]
pub struct ReportSender {
    tx: UnboundedSender<DrainMsg>,
}

impl ReportSender {
    /// Send a `Report` asynchronously. Returns a future that
    /// resolves once the message has been handed off to the drain
    /// queue. The enqueue is synchronous and non-blocking; the
    /// `async` signature is kept for call-site compatibility.
    pub fn send(
        &self,
        message: Option<String>,
        percentage: Option<u32>,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let tx = self.tx.clone();
        async move {
            let _ = tx.send(DrainMsg::Report {
                message,
                percentage,
            });
        }
    }

    /// Fire-and-forget variant for sync callers — enqueues the
    /// Report into the reporter's drain queue without spawning a
    /// tokio task. Preserves strict ordering relative to `end()`
    /// because every sender shares the same mpsc.
    pub fn send_detached(&self, message: Option<String>, percentage: Option<u32>) {
        let _ = self.tx.send(DrainMsg::Report {
            message,
            percentage,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! These tests drive the drain task in isolation — the
    //! `Client`-touching half of begin/end is exercised by the
    //! integration tests.

    use super::{DrainMsg, ReportSender};
    use tokio::sync::mpsc;

    fn sender_with_collector() -> (ReportSender, mpsc::UnboundedReceiver<DrainMsg>) {
        let (tx, rx) = mpsc::unbounded_channel::<DrainMsg>();
        (ReportSender { tx }, rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_detached_enqueues_immediately_in_order() {
        // The "stuck at 99%" bug came from `send_detached` spawning
        // independent tokio tasks that raced each other and the
        // final End. This pins the replacement behaviour: every
        // `send_detached` lands on the same ordered channel before
        // the function returns.
        let (sender, mut rx) = sender_with_collector();
        sender.send_detached(Some("a".into()), Some(10));
        sender.send_detached(Some("b".into()), Some(20));
        sender.send_detached(Some("c".into()), Some(30));

        for expected in ["a", "b", "c"] {
            match rx.recv().await {
                Some(DrainMsg::Report { message, .. }) => {
                    assert_eq!(message.as_deref(), Some(expected));
                }
                other => panic!("expected Report({expected}), got {:?}", matches_tag(&other)),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn report_before_end_preserved_under_mixed_senders() {
        // Simulate the schema-fetch scenario: a background closure
        // fires several Reports via a clone, then the owning
        // reporter sends End. The End MUST arrive after every
        // Report, otherwise Fidget sticks at the last percentage.
        let (owner_tx, mut rx) = mpsc::unbounded_channel::<DrainMsg>();
        let background = ReportSender {
            tx: owner_tx.clone(),
        };
        background.send_detached(Some("1/3".into()), Some(33));
        background.send_detached(Some("2/3".into()), Some(66));
        background.send_detached(Some("3/3".into()), Some(99));
        owner_tx
            .send(DrainMsg::End {
                message: Some("done".into()),
            })
            .unwrap();

        let mut reports = 0;
        let mut saw_end = false;
        while let Some(msg) = rx.recv().await {
            match msg {
                DrainMsg::Report { .. } => {
                    assert!(!saw_end, "Report arrived after End — ordering broken");
                    reports += 1;
                }
                DrainMsg::End { .. } => saw_end = true,
            }
            if saw_end && rx.is_empty() {
                break;
            }
        }
        assert_eq!(reports, 3);
        assert!(saw_end);
    }

    fn matches_tag(msg: &Option<DrainMsg>) -> &'static str {
        match msg {
            Some(DrainMsg::Report { .. }) => "Report",
            Some(DrainMsg::End { .. }) => "End",
            None => "None",
        }
    }
}
