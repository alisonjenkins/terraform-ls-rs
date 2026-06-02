//! Filesystem watcher producing workspace change events.
//!
//! Wraps `notify-debouncer-full` and forwards events as
//! [`WorkspaceEvent`]s via a tokio mpsc channel. The watcher owns
//! a `RecommendedWatcher` under the hood; dropping the returned
//! [`WorkspaceWatcher`] stops all watching.
//!
//! # macOS lock-file workaround
//!
//! On macOS, `notify`'s FSEvents backend has been observed to
//! silently drop `.terraform.lock.hcl` modify events when tfls
//! runs as a piped-stdio subprocess (eg. spawned by an LSP
//! client / `lspmux` / a probe binary). The streams created by
//! `FSEventStreamCreate` succeed and start, but the callback
//! never fires for subsequent file modifications — confirmed via
//! [`tfls-mux-lock-probe --direct`]. Both the legacy
//! `FSEventStreamScheduleWithRunLoop` API (what notify uses) and
//! the modern `FSEventStreamSetDispatchQueue` API exhibit the
//! same dead-callback pattern; even `notify::PollWatcher` fails
//! in the same process state, so the bug is below the
//! file-watching layer (see `/tmp/notify-research.md`).
//!
//! Empirical workaround: spawn a dedicated [`spawn_lock_file_poller`]
//! `std::thread` that walks the watch root for
//! `.terraform.lock.hcl` files every [`LOCK_POLL_INTERVAL`] and
//! emits [`WorkspaceEvent::LockFileChanged`] when their mtime
//! changes. Cheap (one stat per lock file per second) and
//! belt-and-braces against any remaining FSEvents oddity.
//!
//! Linux's inotify backend doesn't have the bug — the poller
//! still runs there, but it's redundant; events arrive via
//! notify within the debounce window.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebouncedEvent, RecommendedCache, new_debouncer};
use tokio::sync::mpsc;

use crate::discovery::is_ignored_dir;
use crate::error::WalkerError;

/// How often the dedicated lock-file mtime poller scans the watch
/// root for `.terraform.lock.hcl` changes. Cheap (one stat per
/// lock file per tick) so a low cadence is fine; users only run
/// `terraform init` interactively, so 1s latency is invisible.
const LOCK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A high-level workspace change event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceEvent {
    /// A Terraform file has been created or updated on disk.
    FileChanged(PathBuf),
    /// A Terraform file was removed.
    FileRemoved(PathBuf),
    /// A `.terraform.lock.hcl` file was created, updated, or
    /// removed in the given module directory. Carries the
    /// containing dir, not the file path itself, so the
    /// consumer can hand it straight to
    /// `StateStore::invalidate_lock`.
    LockFileChanged(PathBuf),
}

/// Handle to a running workspace watcher. Drop it to stop watching.
pub struct WorkspaceWatcher {
    // Keep the debouncer alive so its thread keeps running.
    _debouncer: notify_debouncer_full::Debouncer<RecommendedWatcher, RecommendedCache>,
    pub events: mpsc::UnboundedReceiver<WorkspaceEvent>,
    // Tells the lock-file poller thread to exit on drop.
    poller_stop: Arc<AtomicBool>,
}

impl Drop for WorkspaceWatcher {
    fn drop(&mut self) {
        self.poller_stop.store(true, Ordering::Release);
    }
}

/// Start a debounced recursive watch on `root`. Events for
/// `.tf`/`.tf.json` files (create/modify/remove) and
/// `.terraform.lock.hcl` files are forwarded; everything else is
/// filtered out.
pub fn watch_workspace(
    root: &Path,
    debounce: Duration,
) -> Result<WorkspaceWatcher, WalkerError> {
    let (tx, rx) = mpsc::unbounded_channel();

    let tx_for_notify = tx.clone();
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
                    let _ = tx_for_notify.send(ev);
                }
            }
        },
    )
    .map_err(WalkerError::Watcher)?;

    debouncer
        .watch(root, RecursiveMode::Recursive)
        .map_err(WalkerError::Watcher)?;

    let poller_stop = Arc::new(AtomicBool::new(false));
    spawn_lock_file_poller(root.to_path_buf(), tx, Arc::clone(&poller_stop));

    Ok(WorkspaceWatcher {
        _debouncer: debouncer,
        events: rx,
        poller_stop,
    })
}

/// Spawn a dedicated `std::thread` that scans `root` recursively
/// every [`LOCK_POLL_INTERVAL`] for `.terraform.lock.hcl` files
/// and emits [`WorkspaceEvent::LockFileChanged`] when their
/// mtime changes (or when a lock file appears / disappears).
fn spawn_lock_file_poller(
    root: PathBuf,
    tx: mpsc::UnboundedSender<WorkspaceEvent>,
    stop: Arc<AtomicBool>,
) {
    std::thread::Builder::new()
        .name("tfls-lock-poller".to_string())
        .spawn(move || {
            // Prime `seen` with the workspace's current lock files BEFORE
            // the loop. Otherwise the first scan classifies every
            // pre-existing `.terraform.lock.hcl` as new (`None => true`)
            // and emits a `LockFileChanged` for each — triggering an
            // expensive schema re-fetch per module on every startup of an
            // already-initialised workspace. We only want post-startup
            // changes.
            let mut seen: HashMap<PathBuf, SystemTime> = HashMap::new();
            scan_lock_files(&root, &mut seen);
            loop {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if tx.is_closed() {
                    break;
                }

                let mut current: HashMap<PathBuf, SystemTime> = HashMap::new();
                scan_lock_files(&root, &mut current);

                for (path, mtime) in &current {
                    let changed = match seen.get(path) {
                        None => true,
                        Some(t) => t != mtime,
                    };
                    if changed {
                        if let Some(parent) = path.parent() {
                            tracing::debug!(
                                dir = %parent.display(),
                                "lock-poller: emit LockFileChanged"
                            );
                            let _ = tx.send(WorkspaceEvent::LockFileChanged(
                                parent.to_path_buf(),
                            ));
                        }
                    }
                }
                for path in seen.keys() {
                    if !current.contains_key(path) {
                        if let Some(parent) = path.parent() {
                            tracing::debug!(
                                dir = %parent.display(),
                                "lock-poller: emit LockFileChanged (removed)"
                            );
                            let _ = tx.send(WorkspaceEvent::LockFileChanged(
                                parent.to_path_buf(),
                            ));
                        }
                    }
                }
                seen = current;

                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
        })
        .ok();
}

fn scan_lock_files(root: &Path, out: &mut HashMap<PathBuf, SystemTime>) {
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    if is_ignored_dir(name) {
                        continue;
                    }
                }
                stack.push(path);
            } else if is_lockfile(&path) {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        out.insert(path, mtime);
                    }
                }
            }
        }
    }
}

fn classify(de: &DebouncedEvent) -> Vec<WorkspaceEvent> {
    let mut out = Vec::new();
    for path in &de.event.paths {
        if should_ignore(path) {
            continue;
        }
        if is_lockfile(path) {
            // The lock file lives at the module root next to
            // `.terraform/`. Emit the containing dir so consumers
            // can invalidate per-module caches.
            if let Some(parent) = path.parent() {
                if matches!(
                    de.event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) {
                    out.push(WorkspaceEvent::LockFileChanged(parent.to_path_buf()));
                }
            }
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

fn is_lockfile(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()) == Some(".terraform.lock.hcl")
}

fn should_ignore(path: &Path) -> bool {
    path.ancestors()
        .filter_map(|p| p.file_name().and_then(|s| s.to_str()))
        .any(is_ignored_dir)
}

fn is_terraform_file(path: &Path) -> bool {
    crate::discovery::is_terraform_file(path)
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
    async fn lockfile_rewrite_emits_lock_file_changed() {
        // Regression: before the dedicated mtime poller existed,
        // `notify`'s FSEvents backend on macOS silently dropped
        // `.terraform.lock.hcl` modify events when tfls ran as a
        // piped-stdio subprocess. Pre-create the lock so the
        // poller's first scan records its mtime, drain initial
        // events, then rewrite and assert a `LockFileChanged`
        // event arrives within the poll cadence.
        let dir = tmp_dir("lockfile-rewrite");
        fs::write(dir.join(".terraform.lock.hcl"), "version = \"1\"\n").unwrap();

        let mut watcher =
            watch_workspace(&dir, Duration::from_millis(50)).expect("watcher");

        tokio::time::sleep(LOCK_POLL_INTERVAL + Duration::from_millis(200)).await;
        while tokio::time::timeout(Duration::from_millis(50), watcher.events.recv())
            .await
            .is_ok()
        {}

        fs::write(dir.join(".terraform.lock.hcl"), "version = \"2\"\n").unwrap();

        let got = tokio::time::timeout(LOCK_POLL_INTERVAL * 3, watcher.events.recv())
            .await
            .expect("timeout waiting for LockFileChanged")
            .expect("channel closed");
        match got {
            WorkspaceEvent::LockFileChanged(p) => {
                let canon_dir = dir.canonicalize().unwrap();
                assert!(
                    p == dir || p == canon_dir,
                    "got {p:?}, expected {dir:?} or {canon_dir:?}"
                );
            }
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

        let got =
            tokio::time::timeout(Duration::from_millis(600), watcher.events.recv()).await;
        assert!(got.is_err(), "should have timed out, got {got:?}");

        fs::remove_dir_all(dir).ok();
    }
}
