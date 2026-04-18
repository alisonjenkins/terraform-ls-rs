//! Priority job queue for background work.
//!
//! Producers call [`JobQueue::enqueue`] from anywhere. A single
//! consumer calls [`JobQueue::next`] to receive the next highest-
//! priority job. Jobs are deduplicated — enqueueing the same job
//! twice is a no-op, which avoids thrashing when e.g. many save
//! events fire for the same file in quick succession.

use std::collections::{BinaryHeap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use tokio::sync::Notify;

/// The kind of background work to perform.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Job {
    /// Parse a file from disk and install it in the store.
    ParseFile(PathBuf),
    /// Reparse an open document that has already been edited.
    ReparseDocument(lsp_types::Url),
    /// Fetch provider schemas via the terraform/opentofu CLI.
    FetchSchemas { working_dir: PathBuf },
    /// Fetch built-in function signatures via
    /// `<binary> metadata functions -json` and install into the store.
    FetchFunctions { binary: PathBuf },
    /// Enumerate a single directory's `.tf` files (non-recursive) and
    /// enqueue parse jobs for each. Used when the editor opens a file in
    /// a directory that hasn't been indexed yet.
    ScanDirectory(PathBuf),
}

/// Priority levels (ordered: Immediate is highest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
    Immediate = 3,
}

/// Internal heap entry.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    priority: Priority,
    /// Monotonic sequence breaks ties deterministically (FIFO within
    /// the same priority).
    seq: u64,
    job: Job,
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Higher priority first; lower seq first within same priority.
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Concurrent priority queue backed by a mutex + notify.
///
/// The mutex is tiny (only held for push/pop, never across awaits) so
/// lock contention is not a concern in practice.
#[derive(Debug, Default)]
pub struct JobQueue {
    inner: Mutex<Inner>,
    notify: Notify,
}

#[derive(Debug, Default)]
struct Inner {
    heap: BinaryHeap<Entry>,
    /// Pending jobs, used for deduplication.
    pending: HashSet<Job>,
    seq: u64,
}

impl JobQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a job. No-op if an identical job is already queued.
    pub fn enqueue(&self, job: Job, priority: Priority) {
        let should_notify = {
            let mut guard = match self.inner.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !guard.pending.insert(job.clone()) {
                return;
            }
            guard.seq += 1;
            let seq = guard.seq;
            guard.heap.push(Entry {
                priority,
                seq,
                job,
            });
            true
        };
        if should_notify {
            self.notify.notify_one();
        }
    }

    /// Take the next job if one is available, without waiting.
    pub fn try_next(&self) -> Option<Job> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = guard.heap.pop()?;
        guard.pending.remove(&entry.job);
        Some(entry.job)
    }

    /// Await the next job, blocking until one is available.
    pub async fn next(&self) -> Job {
        loop {
            if let Some(job) = self.try_next() {
                return job;
            }
            self.notify.notified().await;
        }
    }

    /// Number of jobs queued right now.
    pub fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.heap.len(),
            Err(poisoned) => poisoned.into_inner().heap.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::Url;
    use std::time::Duration;

    fn doc_job(path: &str) -> Job {
        Job::ReparseDocument(Url::parse(path).expect("url"))
    }

    #[test]
    fn deduplicates_identical_jobs() {
        let q = JobQueue::new();
        let j = doc_job("file:///a.tf");
        q.enqueue(j.clone(), Priority::Normal);
        q.enqueue(j.clone(), Priority::Normal);
        q.enqueue(j, Priority::Normal);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn pops_in_priority_order() {
        let q = JobQueue::new();
        q.enqueue(doc_job("file:///a.tf"), Priority::Low);
        q.enqueue(doc_job("file:///b.tf"), Priority::High);
        q.enqueue(doc_job("file:///c.tf"), Priority::Normal);

        let first = q.try_next().expect("first");
        assert_eq!(first, doc_job("file:///b.tf"));
        let second = q.try_next().expect("second");
        assert_eq!(second, doc_job("file:///c.tf"));
        let third = q.try_next().expect("third");
        assert_eq!(third, doc_job("file:///a.tf"));
        assert!(q.try_next().is_none());
    }

    #[test]
    fn fifo_within_same_priority() {
        let q = JobQueue::new();
        q.enqueue(doc_job("file:///a.tf"), Priority::Normal);
        q.enqueue(doc_job("file:///b.tf"), Priority::Normal);
        q.enqueue(doc_job("file:///c.tf"), Priority::Normal);
        assert_eq!(q.try_next().unwrap(), doc_job("file:///a.tf"));
        assert_eq!(q.try_next().unwrap(), doc_job("file:///b.tf"));
        assert_eq!(q.try_next().unwrap(), doc_job("file:///c.tf"));
    }

    #[test]
    fn re_enqueue_after_drain_is_accepted() {
        let q = JobQueue::new();
        let j = doc_job("file:///a.tf");
        q.enqueue(j.clone(), Priority::Normal);
        assert_eq!(q.try_next().unwrap(), j);
        q.enqueue(j.clone(), Priority::Normal);
        assert_eq!(q.try_next().unwrap(), j);
    }

    #[tokio::test]
    async fn async_next_wakes_when_job_arrives() {
        let q = std::sync::Arc::new(JobQueue::new());
        let q2 = std::sync::Arc::clone(&q);
        let handle = tokio::spawn(async move { q2.next().await });

        // Give the task a moment to park on notified().
        tokio::time::sleep(Duration::from_millis(10)).await;
        q.enqueue(doc_job("file:///a.tf"), Priority::High);

        let got = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("no timeout")
            .expect("task ok");
        assert_eq!(got, doc_job("file:///a.tf"));
    }
}
