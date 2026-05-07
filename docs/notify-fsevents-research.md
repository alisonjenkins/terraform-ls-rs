# notify-rs on macOS under tokio + tower-lsp + piped stdin: research notes

## Summary (ranked by likelihood of being the root cause)

1. **`tokio::io::stdin` blocking-thread reader does NOT explain it on its own**, but in combination with **path canonicalization at `watch()` time vs. event-time path resolution** in `notify`'s FSEvents backend it produces this exact failure mode. The smoking gun is that the diagnostic watcher placed in `main()` BEFORE `rt.block_on(...)` fires once for an initial Create event but then goes silent — that pattern matches "stream is alive, runloop thread is alive, but path-match filter is dropping every subsequent event because the canonicalized watch root doesn't match the event paths the kernel is delivering". See hypothesis (B) below — `kFSEventStreamCreateFlagWatchRoot` + nix-shell `/private/tmp` + later directory creation under the watch root is the canonical trigger.
2. **`notify` 7.0 still uses `FSEventStreamScheduleWithRunLoop` on a dedicated `std::thread`, not `FSEventStreamSetDispatchQueue`.** That thread runs `CFRunLoop::run()` and is independent of tokio. So tokio's multi-thread runtime cannot directly starve it. BUT — the runloop thread will exit silently if `FSEventStreamStart` returns false (rare) or if any of the upstream `cf::*` constants the lazy-static init reads returns null. This has been seen on certain code-signing / sandbox / bind-mount setups; nix-shell on macOS is not one of them, but worth ruling out via `dtrace` of `FSEventStreamCreate` return value.
3. **macOS FSEvents has a documented "tail-of-write" pathology** (issue #240) where streams created with `kFSEventStreamEventIdSinceNow` may take up to ~10 s to flush the first batch unless the underlying file is closed-and-reopened. The user's "diagnostic watcher fires once for Create then stops" is consistent with this batched-coalesce behaviour, NOT with the runloop being dead.
4. **`notify`'s scope filter silently drops events when the watched-path string isn't a `starts_with` prefix of the event path.** Confirmed by reading `fsevent.rs::callback_impl`. Combined with the runloop's path canonicalization happening in `append_path()` (`path.canonicalize()` on the watch root only) but NOT on a freshly mkdir'd subtree, AND with `kFSEventStreamCreateFlagWatchRoot` in the flag set, this can produce "watch is alive, callback fires, all events filtered out" on macOS more often than is documented.
5. **PollWatcher being equally dead is suspicious.** PollWatcher is a fully-`std::thread`-driven recursive `metadata()` poller — it has zero kernel-event dependency. If both backends are silent, the problem is almost certainly in either (a) the *channel* between notify's callback and the user's tokio mpsc (i.e. tokio can't wake a receiver because its IO driver is blocked on something else — see hypothesis E), or (b) notify's directory walk on PollWatcher hitting an early-return condition. PollWatcher has known issues with mid-watch deletes (#406) and "file not found" exceptions during stat (#581). Worth instrumenting PollWatcher with `tracing` via `with_compare_contents(false)` and a lower poll interval to see whether its scan loop is actually running.

## Hypothesis-by-hypothesis evidence

### A — tokio kqueue IO driver vs FSEvents CFRunLoop

**Verdict: not directly the cause.** notify's FSEvents backend runs on a separate `std::thread::spawn` named `notify-rs fsevents loop` with its own `CFRunLoop::current()` — it does not share any kqueue fd with tokio's mio-based IO driver. The runloop thread is independent. The thread name is observable in `lldb`/`sample`, so quickly confirmable.

- `notify/src/fsevent.rs` `run()` — spawns `std::thread::Builder::new().name("notify-rs fsevents loop")` then `CFRunLoop::run()`. https://github.com/notify-rs/notify/blob/main/notify/src/fsevent.rs
- mio's macOS selector uses `kqueue(2)` for fd-readiness only; it has no relationship to FSEvents (which is FSEvents framework on top of kernel notifications, not surfaced via kqueue).

### B — Path canonicalization + `kFSEventStreamCreateFlagWatchRoot` mismatch (most-likely macOS-specific failure mode)

**Verdict: the strongest explanatory candidate.** `notify` canonicalizes the watch root via `path.canonicalize()` in `append_path()` and stores it in `recursive_info`. The callback (`callback_impl`) filters incoming event paths by `starts_with` against that map. FSEvents reports paths the kernel knows them by (post-symlink-resolution, e.g. `/private/tmp/...`); if the runtime watch root canonicalized to something that diverges from FSEvents' kernel-form path AT EVENT TIME (subdir created later), the filter drops every event.

The user's repro path is `/private/tmp/nix-shell.XXX/...` which already looks canonical, but on macOS `realpath()` historically failed for paths with PR-restricted bits or paths that traverse a per-process firmlink. See [andreyvit/FSEventsFix](https://github.com/andreyvit/FSEventsFix) — a Mac OS X library that works around a long-standing realpath() bug that prevents FSEvents API from monitoring certain folders. The "bug exists" pattern is well-documented; nix-shell creates per-invocation tmp dirs, which can hit edge cases where realpath() on a parent vs. child diverges.

- notify path canonicalization site: `notify/src/fsevent.rs::append_path` calls `path.to_path_buf().canonicalize()`.
- Issue [#447](https://github.com/notify-rs/notify/issues/447) (compilation/CI) and [#412](https://github.com/notify-rs/notify/issues/412) (large-scale dropped events, labelled `B-upstream B-wontfix` — confirms the maintainers consider event-loss with FSEvents an OS-layer problem).
- Issue [#240](https://github.com/notify-rs/notify/issues/240) — "Events not delivered until a file is closed on macOS" — directly matches the "fires once on Create then stops" observation.

**Concrete diagnostic step:** in the failing case, log the canonicalized path your `debouncer.watch(root, ...)` actually stored vs. the path FSEvents would deliver for a child file. The simplest test is to hold the diagnostic watcher rooted at `/private/tmp/nix-shell.XXX` AND a sibling watcher rooted at `realpath(/private/tmp/nix-shell.XXX)` and see whether the second one fires.

### C — PollWatcher dying

Issue [#387](https://github.com/notify-rs/notify/issues/387) is the closest match: "Watcher never triggers on file writes for RecommendedWatcher or PollWatcher" — same symptom (both backends dead). No resolution. Issue [#576](https://github.com/notify-rs/notify/issues/576) reports the documented example doesn't fire on macOS 13.6 inside a sync `main()` — the suspected cause there is the program exiting before events flush, but it shows that "happy-path code that works in tests doesn't work in real binaries" is a recurring theme.

PollWatcher reliability gaps with documented fixes:
- PR [#406](https://github.com/notify-rs/notify/pull/406) — "fix: PollWatcher panic after delete-and-recreate"
- PR [#409](https://github.com/notify-rs/notify/pull/409) — "refactor: PollWatcher"
- Issue [#581](https://github.com/notify-rs/notify/issues/581) — "PollWatcher: ignore IO 'file not found' exceptions when accessing entry metadata"

If a panic happens inside the poll thread on a transient stat failure, the thread dies silently (default `JoinHandle` is dropped on the watcher struct so no panic propagation). Worth wrapping the poll callback in `std::panic::catch_unwind` instrumentation, or checking with `dtrace -n 'rust$target_pid:::panic'`.

### D — Crossbeam vs tokio scheduler interaction

Issue [#380](https://github.com/notify-rs/notify/issues/380) — "Crossbeam breaks tokio::spawn" — closed via PR #425 (milestone 5.0.0). Reporter said tokio tasks failed to schedule because crossbeam's thread-local storage was clashing with tokio's scheduler. This was supposedly fixed pre-5.0; you're on 7.0 so it shouldn't apply. But: notify-debouncer-full 0.4 still owns its own internal thread with a crossbeam channel. If that thread dies (panic in debounce timer), debouncer goes silent. Issue [#205](https://github.com/notify-rs/notify/issues/205) — "Thread watching debounced events hanging" — mutex deadlock between event thread and debounce timer thread under high load — is open.

### E — tokio mpsc back-pressure / `tokio::io::stdin` blocking thread

`tokio::io::stdin()` is documented as "implemented by using an ordinary blocking read on a separate thread, and it is impossible to cancel that read" ([tokio docs](https://docs.rs/tokio/latest/tokio/io/fn.stdin.html), issue [#2466](https://github.com/tokio-rs/tokio/issues/2466), issue [#709](https://github.com/tokio-rs/tokio/issues/709)). So on macOS your tower-lsp main loop holds *one* dedicated tokio worker thread blocked in `read(2)` on the inherited stdin pipe fd.

If your code uses `tokio::sync::mpsc::unbounded_channel` (as `WorkspaceWatcher` does), receiving requires a runtime poll. If notify's callback fires on its own runloop thread and `tx_for_notify.send(ev)` succeeds (unbounded never blocks), the event is in the queue. If the *receiver* never gets polled because the only worker thread that *would* poll it is the same one tower-lsp's `Server::serve` is monopolising via `tokio::io::stdin` (single-thread runtime) — events appear silently dropped. **You're on `multi_thread`, so this should be impossible**, but worth confirming with `worker_threads(N)` set to ≥4 explicitly. Issue [#3120](https://github.com/tokio-rs/tokio/issues/3120) — "Tasks not scheduled to thread blocked in rt.block_on" — is the canonical example.

### F — `FSEventStreamSetDispatchQueue` migration

`FSEventStreamScheduleWithRunLoop` is deprecated in macOS 13+. notify-rs has not migrated yet (still on the deprecated API per main `fsevent.rs` as of search date). Other ecosystems made the move (gradle/native-platform issue #315; fsnotify/fsevents Go #59). Migrating to `FSEventStreamSetDispatchQueue` would let FSEvents dispatch to a libdispatch concurrent queue rather than a CFRunLoop on a dedicated thread — empirically more reliable in subprocess-launched-under-spawned-shell environments.

## Concrete event-driven workarounds (ranked by minimal-deps preference)

### Option 1 — `kqueue` crate (single-file fast path for `.terraform.lock.hcl`)

[`kqueue` crate](https://docs.rs/kqueue/latest/kqueue/) — pure rust BSD/macOS kqueue wrapper, MIT, ~129 commits, https://gitlab.com/rust-kqueue/rust-kqueue.

- **Pros:** purely event-driven, no polling thread, no FSEvents involvement (so doesn't share notify's CFRunLoop bug surface). Zero extra dependencies of significance. Very small API.
- **Cons:** consumes one fd per watched file (notify-rs issue #596 — "Too many open files" with kqueue at scale). For our use case — one `.terraform.lock.hcl` per module, typically <50 modules — this is fine, but you'd need to re-register fds when the file is delete-and-recreated (`terraform init` does this on lock rotation). PR/wrap that with a small reaper thread that consumes `EVFILT_VNODE` events and re-`open()`s the file when `NOTE_DELETE | NOTE_RENAME` arrives.
- **Integration:** spawn one `std::thread` per process (NOT per file) running `Watcher::iter()`; forward to `tokio::sync::mpsc::UnboundedSender`. The blocking thread doesn't go through tokio's blocking pool, avoiding any interaction with `enable_all` / mio.

### Option 2 — Manual `FSEventStreamSetDispatchQueue` via `fsevent-sys` + `dispatch2`

Skip notify-rs entirely for the lock file. Use [`fsevent-sys`](https://docs.rs/fsevent-sys) raw bindings + [`dispatch2`](https://docs.rs/dispatch2) and call `FSEventStreamSetDispatchQueue` with a libdispatch concurrent queue. Let GCD manage the thread.

- **Pros:** modern API; immune to CFRunLoop weirdness; minimal deps (`fsevent-sys` is tiny, `dispatch2` is tiny).
- **Cons:** macOS-specific (Linux fallback still needed); ~150 LOC of unsafe FFI to write yourself.

### Option 3 — `notify::Config::with_compare_contents(false).with_poll_interval(...)` PollWatcher with PANIC-CAPTURE wrapper

Keep the rest of notify, but force PollWatcher and wrap the callback in `std::panic::catch_unwind`. If issue (C) is real, you'll see the panic in logs and can fix it. Polling at 250 ms (fast enough for `.terraform.lock.hcl`, slow enough that recursive 10k-file workspaces aren't a problem because the lock file is at the module root, not the workspace root) is a middle ground.

- **Pros:** Stays inside notify's API; no new deps.
- **Cons:** Still polling — exactly what you said you want to remove. But it's notify's polling, not your bespoke 1s thread, so it composes with the rest of the watcher pipeline cleanly.

### Option 4 — Migrate macOS to a `dispatch_source_t` per file (libdispatch native)

`DISPATCH_SOURCE_TYPE_VNODE` directly from `dispatch2`. One `dispatch_source_t` per `.terraform.lock.hcl`. macOS's preferred file-watching API for single files (Apple's own Xcode and Spotlight use this).

- **Pros:** Native, supported, recommended; no thread to manage.
- **Cons:** Same fd-per-file constraint as kqueue (it's kqueue underneath); macOS-specific fallback story needed for Linux (inotify direct).

### Recommendation

**Start with Option 1.** Keep notify-debouncer-full for the recursive `.tf` walk (where it works), and drop your 1-second polling thread in favour of a single `std::thread` running a `kqueue::Watcher` registered against every `.terraform.lock.hcl` discovered at startup, with a re-registration callback when `NOTE_DELETE | NOTE_RENAME` fires (since `terraform init` writes to a temp file and renames). That gives you event-driven on macOS for the lock file (~10 ms latency) without touching the broken FSEvents-recursive path.

If you want to fix the *root cause* and make notify recursive watching reliable on macOS under tokio multi-thread + piped stdin, **also** instrument the diagnostic to capture: (a) the exact path string stored in `recursive_info` after `debouncer.watch()`, (b) the exact path string in the first event you observe (CFRunLoop fires once then dies), and (c) whether `dtrace -n 'objc$target:::FSEventStreamStart:return'` shows a non-zero return at any point after tokio starts. That triplet identifies which of (B), (C), (E) is hitting you.

## Open questions / didn't surface in searches

- No GitHub issue in `notify-rs/notify` directly matches "tokio multi-thread + piped stdin + tower-lsp = FSEvents callback dies after first event". Closest match is #387 (both backends dead, no resolution).
- Could not find evidence that tokio's IO driver shares kqueue state with FSEvents. notify's CFRunLoop thread is genuinely independent; if it's not running, that's a notify bug, not a tokio bug.
- `andreyvit/FSEventsFix` proves "realpath bug breaks FSEvents on certain paths" is a recurring class of macOS bug — but it's described for older OS versions; whether macOS 25.4 (Darwin) still has any residual realpath/firmlink edge cases that bite nix-shell tmp dirs would need an empirical test (dtrace on `realpath` syscall during `debouncer.watch()`).
- `notify` 8.0 / `notify-debouncer-full` 1.0 (if released) may already migrate to `FSEventStreamSetDispatchQueue`. Worth checking the changelog before doing manual FFI work.

## Source references

- notify FSEvents backend: https://github.com/notify-rs/notify/blob/main/notify/src/fsevent.rs
- notify CHANGELOG: https://github.com/notify-rs/notify/blob/main/CHANGELOG.md
- notify issue #240 (events not delivered until close): https://github.com/notify-rs/notify/issues/240
- notify issue #387 (RecommendedWatcher + PollWatcher both never trigger): https://github.com/notify-rs/notify/issues/387
- notify issue #412 (large scale dropped events, B-upstream B-wontfix): https://github.com/notify-rs/notify/issues/412
- notify issue #576 (sync main on macOS 13.6 — no events): https://github.com/notify-rs/notify/issues/576
- notify issue #205 (debouncer thread hanging): https://github.com/notify-rs/notify/issues/205
- notify issue #380 (crossbeam vs tokio::spawn, closed PR #425): https://github.com/notify-rs/notify/issues/380
- notify issue #289 (crossbeam channel, closed): https://github.com/notify-rs/notify/issues/289
- notify issue #596 (kqueue "too many open files"): https://github.com/notify-rs/notify/issues/596
- notify PollWatcher fixes: https://github.com/notify-rs/notify/pull/406, https://github.com/notify-rs/notify/pull/409, https://github.com/notify-rs/notify/issues/581
- tokio issue #2466 (stdin can block shutdown): https://github.com/tokio-rs/tokio/issues/2466
- tokio issue #709 (stdin actually blocks): https://github.com/tokio-rs/tokio/issues/709
- tokio issue #3120 (tasks not scheduled to thread blocked in `rt.block_on`): https://github.com/tokio-rs/tokio/issues/3120
- mio issue #1171 (perf when polling stdin on macOS): https://github.com/tokio-rs/mio/issues/1171
- mio issue #1377 (polling /dev/tty on macOS): https://github.com/tokio-rs/mio/issues/1377
- FSEventsFix realpath bug workaround: https://github.com/andreyvit/FSEventsFix
- gradle/native-platform #315 — `FSEventStreamSetDispatchQueue` migration: https://github.com/gradle/native-platform/issues/315
- fsnotify/fsevents #59 — same on Go side: https://github.com/fsnotify/fsevents/issues/59
- kqueue rust crate: https://docs.rs/kqueue/, https://gitlab.com/rust-kqueue/rust-kqueue
- watchexec macOS FSEvents limitations: https://watchexec.github.io/docs/macos-fsevents.html
- Apple FSEventStreamCreate: https://developer.apple.com/documentation/coreservices/1443980-fseventstreamcreate
- Apple FSEventStreamSetDispatchQueue: https://developer.apple.com/documentation/coreservices/1444164-fseventstreamsetdispatchqueue
- Apple Kernel Queues alternative: https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/KernelQueues/KernelQueues.html
- tokio::io::stdin docs: https://docs.rs/tokio/latest/tokio/io/fn.stdin.html

## Round 2: Recursive watching options

### Headline finding

**`fsevent-sys` 5.2.0 (released 2025-11-17) DOES expose `FSEventStreamSetDispatchQueue`** taking `&dispatch2::DispatchQueue` — the modern API is one Rust dependency away. The 5.x line dropped its `core-foundation` 0.9 dep and migrated to `core-foundation` 0.10 + `dispatch2` 0.3 (default-features-off, alloc only — tiny). No CFRunLoop required. This invalidates the prior assumption that we'd need 150 LOC of unsafe FFI.

Verified at `https://github.com/octplane/fsevent-rust/blob/main/fsevent-sys/src/fsevent.rs` line 121:
```rust
pub fn FSEventStreamSetDispatchQueue(stream_ref: FSEventStreamRef, q: &DispatchQueue);
```

`FSEventStreamScheduleWithRunLoop` is also still exported (line 116), so callers pick.

### Crate landscape

| Crate                | Repo                                           | Last release        | Uses dispatch queue?      | Recursive | Notes                                                                                                                                       |
|----------------------|------------------------------------------------|---------------------|---------------------------|-----------|---------------------------------------------------------------------------------------------------------------------------------------------|
| `fsevent-sys` 5.2.0  | octplane/fsevent-rust                          | 2025-11-17          | **Exposes the FFI**       | (raw FFI; FSEvents is recursive by API design) | Tiny: deps = `dispatch2` + `core-foundation` 0.10. Maintained (PRs merged Oct/Nov 2025). MIT.                                                |
| `fsevent` 2.3.0      | octplane/fsevent-rust                          | 2025-11-17          | No (high-level wrapper, RunLoop) | Yes      | Higher-level convenience over `fsevent-sys`. Doesn't actually use SetDispatchQueue itself but underlying sys crate does.                     |
| `fsevent-stream` 0.2.3 | PhotonQuantum/fsevent-stream                 | 2024-06-28 (last commit) | **No** — uses `CFRunLoop::run_current()` on a spawned thread (`stream.rs:230-243`) | Yes      | Streams API, tokio + async-std features. core-foundation 0.9 — stale dep. Open dependabot PRs since 2024-10. Effectively unmaintained.       |
| `notify` 9.0.0-rc.4  | notify-rs/notify                               | 2026-05-02 (rc)     | **No** — still `FSEventStreamScheduleWithRunLoop` (deprecated annotation in source) | Yes      | rc.1 (#726, 2026-01-12) only swapped `fsevent-sys` for `objc2-core-foundation` + `objc2-core-services`; **canonicalize() + starts_with filter unchanged**. No open PR migrating to dispatch queue.  |
| `e-dant/watcher` (`wtr-watcher` 0.14.5) | e-dant/watcher    | active             | **Yes — uses dispatch**   | Yes      | C++ core via `cc` + `bindgen`. Pulls a C++ toolchain into the build. Heavy footprint vs. ~250 LOC pure-Rust we could write.                  |
| `watchman_client` (Meta) | facebook/watchman                          | active             | N/A — IPC client          | Yes (Watchman daemon does it) | Adds runtime dependency on `watchman` daemon. Not a fit unless we want users to install Watchman.                                            |

### Question 2: canonicalize hypothesis confirmed

**Yes — directly confirmed by issue #23** ("OSX - Watching 3 or more directories fails", since-closed). The reporter shows the assertion failure verbatim:

> `assertion failed: (left == right) (left: "/private/var/folders/...")`

Test failures `watch_dir_recommended` / `watch_single_file_recommended` show `notify` storing a canonicalized `/private/var/...` watch root while events arrive against `/var/...` (or vice versa) — exactly the divergence we hypothesised. Issue #301 (foreign-exception SIGABRT) and #240 (events not delivered until close) reference the same `/private/var` symlink path-resolution rabbit hole.

Re-reading `notify/src/fsevent.rs` `append_path` (still in the 9.0-rc.4 source):
```rust
let canonical_path = path.to_path_buf().canonicalize()?;
// ...
self.recursive_info.insert(canonical_path, WatchInfo { ... });
```
And the callback filter: `if path.starts_with(p)`. No reconciliation between the canonical form and the kernel's event-time form. **No PR strips or reworks this filter.** Open PRs #717 / #632 add *user-facing* path filters but leave the canonicalize+starts_with bug intact.

### Question 3: existing dispatch-queue wrapper template

No public crate, gist, or blog post wraps `fsevent-sys` + `dispatch2` in the "events to a channel" shape we want. Closest relatives all use CFRunLoop (`fsevent`, `fsevent-stream`, `notify`). `e-dant/watcher` uses libdispatch on macOS but it's C++ underneath. We will write the wrapper ourselves — `fsevent-sys` 5.2.0 carrying the `SetDispatchQueue` symbol and `dispatch2::DispatchQueue` exposing `global()` + label'd serial queues makes the actual wrapper ~80–120 LOC of `unsafe`, not 150+, since we skip CFRunLoop entirely.

### Question 4: in-flight notify migration?

**None.** Searched open PRs (#722, #717, #676, #632, #590) and open issues with FSEvents-related text — zero address `FSEventStreamSetDispatchQueue`. Issue #208 ("fsevent hangs on Mac during shutdown", open since 2019, matklad) is the canonical CFRunLoop-shutdown bug; comments propose loop observers as the fix, no one has prototyped dispatch-queue migration. **No upstream patch to override.**

### Question 5: dtrace / realpath red herring?

Inconclusive from public sources. Apple's developer forum and Stack Overflow lack a definitive "realpath() returns different strings at different points in time" post for modern (macOS 12+) firmlinks. The `andreyvit/FSEventsFix` workaround is for older OS versions. **However**, even *without* the realpath-divergence pathology, `notify`'s starts_with filter is fragile: any difference in trailing slash, case-folding (HFS+), or firmlink resolution between watch-time and event-time silently drops the event. The bug is structural — relying on string-prefix equivalence of two independently-resolved paths — not just a realpath edge case.

### Recommendation

**Write a small bespoke macOS adapter** behind a trait shared with `notify::INotifyWatcher` (Linux). Concrete shape:

```rust
// crates/tfls-watcher-fsevents/src/lib.rs (new crate or module in tfls-walker)
use fsevent_sys::{self as fs, core_foundation as cf};
use dispatch2::{DispatchQueue, DispatchQueueAttr};

pub struct FsEventsWatcher {
    stream: fs::FSEventStreamRef,   // RAII via Drop -> Stop+Invalidate+Release
    _queue: DispatchQueue,           // keep alive
    tx: tokio::sync::mpsc::UnboundedSender<RawEvent>,
}

impl FsEventsWatcher {
    pub fn new_recursive(roots: &[&Path], tx: ...) -> io::Result<Self> {
        let queue = DispatchQueue::new("dev.tfls.fsevents", DispatchQueueAttr::SERIAL);
        // Build CFArray of CFString roots — DO NOT canonicalize.
        // FSEvents accepts the user-facing path; the kernel resolves it at watch time
        // and event delivery uses the same internal form. Filtering happens at
        // the kernel layer when kFSEventStreamCreateFlagWatchRoot is set.
        let stream = unsafe { fs::FSEventStreamCreate(/* ... */ kFSEventStreamCreateFlagWatchRoot | kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer, ...) };
        unsafe { fs::FSEventStreamSetDispatchQueue(stream, &queue) };  // ← the modern API
        unsafe { fs::FSEventStreamStart(stream) };
        Ok(Self { stream, _queue: queue, tx })
    }
}

impl Drop for FsEventsWatcher {
    fn drop(&mut self) {
        unsafe { fs::FSEventStreamStop(self.stream); fs::FSEventStreamInvalidate(self.stream); fs::FSEventStreamRelease(self.stream); }
        // _queue dropped after — its async work has been drained by Stop+Invalidate.
    }
}
```

Key design choices vs. notify:
1. **No `canonicalize()`** — pass user's path verbatim to FSEvents; the kernel does the resolution and uses the resolved form for both the watch and event delivery, so paths are inherently consistent.
2. **No `starts_with` filter** — FSEvents already scopes events to the watched roots. If we get an event we didn't ask for, it's a kernel bug (it isn't). Forward everything to the channel.
3. **`SetDispatchQueue`** with our own serial queue (or `DispatchQueue::global()`) — GCD owns the thread. No CFRunLoop, no dedicated `std::thread`, no risk of the runloop thread silently dying because tokio is monopolising things. The callback runs on a GCD-managed thread which posts to a `tokio::sync::mpsc::UnboundedSender` (lock-free, doesn't block GCD).
4. **Linux fallback** — keep using `notify`'s `INotifyWatcher` directly (skip the RecommendedWatcher façade). `inotify` backend doesn't have the canonicalize bug.

LOC estimate: ~250 in the macOS module + ~50 in the trait/shim + ~80 for the existing inotify path = ~380 LOC total replacing the current `notify-debouncer-full` + `notify` dependency. Drops one transitive dependency (`crossbeam-channel`) and removes the debouncer-thread / mutex-deadlock surface area (#205, #208).

If we want to ship faster: depend on `fsevent-sys = "5.2"` directly, skip writing our own debouncer (a 50-line tokio time-window collector suffices for our use case — coalesce events on a 100 ms window, dedupe by path, forward).

