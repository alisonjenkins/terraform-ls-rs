//! Debounced filesystem watcher producing workspace change events.
//!
//! Wraps `notify-debouncer-full` and forwards events as
//! [`WorkspaceEvent`]s via a tokio mpsc channel. The watcher owns a
//! `RecommendedWatcher` under the hood; dropping the returned
//! [`WorkspaceWatcher`] stops all watching.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebouncedEvent, new_debouncer};
use tokio::sync::mpsc;

use crate::discovery::is_ignored_dir;
use crate::error::WalkerError;

/// A high-level workspace change event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceEvent {
    /// A Terraform file has been created or updated on disk.
    FileChanged(PathBuf),
    /// A Terraform file was removed.
    FileRemoved(PathBuf),
}

/// Handle to a running workspace watcher. Drop it to stop watching.
pub struct WorkspaceWatcher {
    // Keep the debouncer alive so its thread keeps running.
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
    pub events: mpsc::UnboundedReceiver<WorkspaceEvent>,
}

/// Start a debounced recursive watch on `root`. Events for
/// `.tf`/`.tf.json` files (create/modify/remove) are forwarded;
/// everything else is filtered out.
pub fn watch_workspace(
    root: &Path,
    debounce: Duration,
) -> Result<WorkspaceWatcher, WalkerError> {
    let (tx, rx) = mpsc::unbounded_channel();

    let mut debouncer = new_debouncer(
        debounce,
        None,
        move |result: Result<Vec<DebouncedEvent>, Vec<notify::Error>>| {
            let events = match result {
                Ok(e) => e,
                Err(errs) => {
                    for e in errs {
                        tracing::warn!(error = %e, "file watcher error");
                    }
                    return;
                }
            };
            for de in events {
                for ev in classify(&de) {
                    let _ = tx.send(ev);
                }
            }
        },
    )
    .map_err(WalkerError::Watcher)?;

    debouncer
        .watch(root, RecursiveMode::Recursive)
        .map_err(WalkerError::Watcher)?;

    Ok(WorkspaceWatcher {
        _debouncer: debouncer,
        events: rx,
    })
}

fn classify(de: &DebouncedEvent) -> Vec<WorkspaceEvent> {
    let mut out = Vec::new();
    for path in &de.event.paths {
        if should_ignore(path) {
            continue;
        }
        if !is_terraform_file(path) {
            continue;
        }
        match de.event.kind {
            EventKind::Create(_) | EventKind::Modify(_) => {
                out.push(WorkspaceEvent::FileChanged(path.clone()));
            }
            EventKind::Remove(_) => {
                out.push(WorkspaceEvent::FileRemoved(path.clone()));
            }
            _ => {}
        }
    }
    out
}

fn should_ignore(path: &Path) -> bool {
    path.ancestors()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
        .any(is_ignored_dir)
}

fn is_terraform_file(path: &Path) -> bool {
    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => return false,
    };
    name.ends_with(".tf") || name.ends_with(".tf.json")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tfls-watcher-{suffix}-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[tokio::test]
    async fn notifies_on_tf_file_create() {
        let dir = tmp_dir("create");
        let mut watcher =
            watch_workspace(&dir, Duration::from_millis(50)).expect("watcher");

        // Give the watcher a moment to initialise before writing.
        tokio::time::sleep(Duration::from_millis(50)).await;
        fs::write(dir.join("main.tf"), "").unwrap();

        let got = tokio::time::timeout(Duration::from_secs(3), watcher.events.recv())
            .await
            .expect("timeout")
            .expect("event");
        match got {
            WorkspaceEvent::FileChanged(p) => assert!(p.ends_with("main.tf")),
            other => panic!("unexpected: {other:?}"),
        }

        fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn ignores_non_terraform_files() {
        let dir = tmp_dir("ignore");
        let mut watcher =
            watch_workspace(&dir, Duration::from_millis(50)).expect("watcher");

        tokio::time::sleep(Duration::from_millis(50)).await;
        fs::write(dir.join("README.md"), "").unwrap();

        let got = tokio::time::timeout(Duration::from_millis(600), watcher.events.recv()).await;
        assert!(got.is_err(), "should have timed out, got {got:?}");

        fs::remove_dir_all(dir).ok();
    }
}
