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
    ReparseDocument(url::Url),
    /// Fetch provider schemas via the terraform/opentofu CLI.
    FetchSchemas { working_dir: PathBuf },
    /// Fetch built-in function signatures via
    /// `<binary> metadata functions -json` and install into the store.
    FetchFunctions { binary: PathBuf },
    /// Enumerate a single directory's `.tf` files (non-recursive) and
    /// enqueue parse jobs for each. Used when the editor opens a file in
    /// a directory that hasn't been indexed yet.
    ScanDirectory(PathBuf),
    /// Bulk workspace scan: recursively discover every `.tf` /
    /// `.tf.json` under the root, parse + publish diagnostics in
    /// parallel via rayon. Replaces the fan-out of hundreds of
    /// individual `ParseFile` jobs that used to flood the queue at
    /// initialize time and serialise through the single-task
    /// worker loop.
    BulkWorkspaceScan(PathBuf),
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
    /// Jobs dequeued but not yet marked [`JobQueue::complete`]. Tracked
    /// so the queue can distinguish "no jobs queued" from "no work left":
    /// a job is removed from the heap the instant it's popped, but the
    /// worker is still processing it (and may not have committed its
    /// results to the store yet). [`JobQueue::is_idle`] accounts for both.
    in_flight: usize,
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
            guard.heap.push(Entry { priority, seq, job });
            true
        };
        if should_notify {
            self.notify.notify_one();
        }
    }

    /// Take the next job if one is available, without waiting. The returned
    /// job counts as **in-flight** until [`complete`](Self::complete) is
    /// called for it, so the queue can report whether work is genuinely
    /// finished rather than merely dequeued.
    pub fn try_next(&self) -> Option<Job> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = guard.heap.pop()?;
        guard.pending.remove(&entry.job);
        guard.in_flight += 1;
        Some(entry.job)
    }

    /// Mark one previously-dequeued job as fully processed. The consumer
    /// calls this after a job's side effects have been committed, so that
    /// [`is_idle`](Self::is_idle) only reports `true` once no work remains.
    pub fn complete(&self) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.in_flight = guard.in_flight.saturating_sub(1);
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

    /// Number of jobs queued right now (excludes in-flight jobs).
    pub fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.heap.len(),
            Err(poisoned) => poisoned.into_inner().heap.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True when there is no queued **and** no in-flight work. Unlike
    /// [`is_empty`](Self::is_empty), this stays `false` while the consumer
    /// is still processing a dequeued job, so a waiter observing `is_idle`
    /// knows every job's side effects are committed.
    pub fn is_idle(&self) -> bool {
        match self.inner.lock() {
            Ok(g) => g.heap.is_empty() && g.in_flight == 0,
            Err(poisoned) => {
                let g = poisoned.into_inner();
                g.heap.is_empty() && g.in_flight == 0
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Duration;
    use url::Url;

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

    #[test]
    fn in_flight_job_is_not_idle_until_completed() {
        let q = JobQueue::new();
        q.enqueue(doc_job("file:///a.tf"), Priority::Normal);
        assert!(!q.is_idle(), "queued job is not idle");

        let job = q.try_next().expect("job");
        // Heap is now empty, but the job is in-flight — NOT idle. This is the
        // window the old `is_empty`-based wait raced into.
        assert!(q.is_empty(), "heap drained");
        assert!(!q.is_idle(), "in-flight job must keep the queue non-idle");

        q.complete();
        assert!(q.is_idle(), "idle once the job completes");
        let _ = job;
    }

    #[test]
    fn re_enqueue_while_in_flight_keeps_non_idle() {
        let q = JobQueue::new();
        q.enqueue(doc_job("file:///a.tf"), Priority::Normal);
        let _a = q.try_next().expect("a");
        // A second job enqueued while the first is still processing.
        q.enqueue(doc_job("file:///b.tf"), Priority::Normal);
        q.complete(); // first job done, but `b` still queued
        assert!(!q.is_idle(), "still has a queued job");
        let _b = q.try_next().expect("b");
        q.complete();
        assert!(q.is_idle());
    }

    #[test]
    fn complete_without_inflight_does_not_underflow() {
        let q = JobQueue::new();
        // Defensive: an unbalanced `complete` must not panic / wrap around.
        q.complete();
        assert!(q.is_idle());
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
