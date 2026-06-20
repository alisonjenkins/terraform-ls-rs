//! Persistent on-disk cache of parsed-Terraform symbol tables and
//! references.
//!
//! Each time the server starts on a workspace it has seen before, it
//! reads a sidecar file from `$XDG_CACHE_HOME/terraform-ls-rs/index-cache/`
//! containing one entry per indexed `.tf` / `.tf.json` / `.tofu` /
//! `.tofu.json` / `.tftest.hcl` / `.tofutest.hcl` file. Entries that
//! still match their on-disk file's `mtime` + size hydrate directly
//! into [`StateStore::definitions_by_name`] + `references_by_name`
//! without re-parsing; only files that have genuinely changed since
//! the last run pay the parse cost.
//!
//! **Fidelity trade-off:** a cached entry deliberately populates a
//! [`DocumentState`] with `parsed.body = None`. `compute_diagnostics`
//! skips the body-dependent rules for such a document, which means
//! workspace-view diagnostics (`:Trouble workspace_diagnostics`) for
//! unopened files are partial until the user actually opens one —
//! at which point `did_open` reparses the file in full via
//! `upsert_document(DocumentState::new(…))` and diagnostics become
//! complete. The cross-file rules (undefined-reference,
//! unused-declarations) run off the indexes that WERE hydrated, so
//! they work immediately from cache.
//!
//! **Invalidation.** File-level: `mtime_ns` + `size` must match or
//! the entry is dropped. Workspace-level: a version tag in the file
//! header; a mismatch (after a cache-format bump) invalidates the
//! whole file. No migration — re-parse is cheap.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tfls_core::SymbolTable;
use tfls_parser::Reference;
use url::Url;

use crate::document::DocumentState;
use crate::store::StateStore;

/// Bump when the cache entry shape changes incompatibly. Old
/// caches then get discarded silently on the next open.
/// v2 changed the wire shape of `SymbolTable.resources` +
/// `data_sources` + `for_each_shapes` + `data_source_for_each_shapes`
/// from a JSON object (broken: `ResourceAddress` is a struct, not a
/// string, so serde_json rejected it) to a JSON array of 2-tuples.
/// v3 added `CacheEntry.content_hash`.
/// Older caches are discarded silently on load.
const CACHE_FORMAT_VERSION: u32 = 3;

/// Deterministic content hash for cache identity. `FxHasher` is seedless,
/// so the value is stable across processes/runs (unlike `RandomState`) —
/// required for an on-disk cache to remain valid between server restarts.
fn content_hash(text: &str) -> u64 {
    use std::hash::Hasher;
    let mut hasher = rustc_hash::FxHasher::default();
    hasher.write(text.as_bytes());
    hasher.finish()
}

/// Header of the on-disk cache file. Kept separate from
/// `IndexCache` so a quick version check can avoid deserialising
/// the potentially-large `entries` vec.
#[derive(Debug, Serialize, Deserialize)]
pub struct CacheHeader {
    pub version: u32,
    pub workspace_root: PathBuf,
}

/// One file's worth of indexed state, the atomic unit of the
/// cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Absolute path on disk (the URI's path component). Used to
    /// stat the file on load + reconstruct its URI.
    pub path: PathBuf,
    /// Last-modified time in nanoseconds since UNIX epoch,
    /// extracted from `std::fs::Metadata::modified`. Mismatches
    /// invalidate the entry.
    pub mtime_ns: u128,
    /// File size in bytes. Belt-and-braces against systems where
    /// mtime resolution is too coarse (e.g. some network mounts).
    pub size: u64,
    /// Hash of the file content the cached symbols were parsed from.
    /// Closes the gap where a file is restored (git checkout, backup)
    /// with a matching mtime+size but DIFFERENT content — without this,
    /// stale symbols would hydrate and produce sticky false diagnostics
    /// surviving a restart.
    pub content_hash: u64,
    /// Symbols declared in the file at parse time.
    pub symbols: SymbolTable,
    /// References extracted from the file at parse time.
    pub references: Vec<Reference>,
}

/// Full cache payload. Serialised as JSON under the XDG cache
/// directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexCache {
    pub header: CacheHeader,
    pub entries: Vec<CacheEntry>,
}

impl IndexCache {
    /// Load the cache for `workspace_root`, or return `None` if no
    /// cache exists or the existing file has a version mismatch /
    /// corrupt payload. Doesn't stat the workspace's files — only
    /// reads the sidecar; individual entries are validated
    /// against the filesystem at [`hydrate_into_store`].
    pub fn load(workspace_root: &Path) -> Option<Self> {
        let path = cache_path_for(workspace_root)?;
        let data = std::fs::read(&path).ok()?;
        let cache: IndexCache = serde_json::from_slice(&data).ok()?;
        if cache.header.version != CACHE_FORMAT_VERSION {
            tracing::info!(
                path = %path.display(),
                cached_version = cache.header.version,
                expected = CACHE_FORMAT_VERSION,
                "index cache: version mismatch — discarding"
            );
            return None;
        }
        // The filename is a hash of the workspace root, so finding a cache
        // at this path normally implies the same root. Verify it anyway:
        // the header stores the root precisely so a (vanishingly rare) hash
        // collision can't hydrate a DIFFERENT workspace's index into this
        // one. The data is already on hand; the check is free.
        if cache.header.workspace_root != workspace_root {
            tracing::warn!(
                path = %path.display(),
                stored = %cache.header.workspace_root.display(),
                requested = %workspace_root.display(),
                "index cache: workspace_root mismatch (hash collision?) — discarding"
            );
            return None;
        }
        Some(cache)
    }

    /// Write the cache to disk. Swallows errors (cache is a
    /// best-effort optimisation; failing to save should never
    /// crash the server) but traces them at `warn` level.
    pub fn save(&self, workspace_root: &Path) {
        let Some(path) = cache_path_for(workspace_root) else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(
                    error = %e,
                    dir = %parent.display(),
                    "index cache: mkdir failed"
                );
                return;
            }
            // Best-effort: clear temp files orphaned by a write that was
            // killed between `write` and `rename`. Bounds the slow disk
            // leak those would otherwise cause (IDX-4).
            if let Some(base) = path.file_name().and_then(|s| s.to_str()) {
                sweep_stale_temps(parent, base);
            }
        }
        let data = match serde_json::to_vec(self) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "index cache: serialise failed");
                return;
            }
        };
        if let Err(e) = atomic_write(&path, &data) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "index cache: write failed"
            );
        } else {
            tracing::info!(
                path = %path.display(),
                entries = self.entries.len(),
                "index cache: saved"
            );
        }
    }

    /// Build a cache from the current contents of `state`,
    /// scoped to files whose path starts with `workspace_root`.
    /// Iterates `state.documents` rather than re-walking disk —
    /// we cache what we've actually indexed, not what we think
    /// we'll find.
    pub fn capture(state: &StateStore, workspace_root: &Path) -> Self {
        let mut entries = Vec::new();
        for doc_ref in state.documents.iter() {
            let Ok(path) = doc_ref.key().to_file_path() else {
                continue;
            };
            if !path.starts_with(workspace_root) {
                continue;
            }
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            let Some(mtime_ns) = metadata_mtime_ns(&metadata) else {
                continue;
            };
            entries.push(CacheEntry {
                path: path.clone(),
                mtime_ns,
                size: metadata.len(),
                // Hash the in-memory text these symbols were parsed from
                // (not the disk file) so an unsaved-buffer/disk divergence
                // is caught on hydrate too.
                content_hash: content_hash(&doc_ref.text()),
                symbols: doc_ref.symbols.clone(),
                references: doc_ref.references.clone(),
            });
        }
        IndexCache {
            header: CacheHeader {
                version: CACHE_FORMAT_VERSION,
                workspace_root: workspace_root.to_path_buf(),
            },
            entries,
        }
    }

    /// For every entry whose on-disk file matches cached
    /// `mtime_ns` + `size`, reconstruct a minimal
    /// [`DocumentState`] (no parsed AST body; symbols +
    /// references hydrated from cache) and upsert it into the
    /// store. Returns the number of entries that hydrated.
    ///
    /// Stale entries (file changed on disk, missing, or
    /// unreadable) are silently skipped — the bulk scan will
    /// re-parse them in the normal flow.
    pub fn hydrate_into_store(&self, state: &StateStore) -> usize {
        let mut hydrated = 0;
        for entry in &self.entries {
            let Ok(metadata) = std::fs::metadata(&entry.path) else {
                continue;
            };
            let Some(mtime_ns) = metadata_mtime_ns(&metadata) else {
                continue;
            };
            if mtime_ns != entry.mtime_ns || metadata.len() != entry.size {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&entry.path) else {
                continue;
            };
            // Final identity check: even with matching mtime+size, the
            // content can differ (a file restored from git/backup with the
            // same mtime+size). A hash mismatch means the cached symbols
            // are stale — skip and let the bulk scan re-parse.
            if content_hash(&text) != entry.content_hash {
                continue;
            }
            let Ok(uri) = Url::from_file_path(&entry.path) else {
                continue;
            };
            // Skip if this doc is already loaded (e.g. a bulk scan or a
            // prior entry put it there) — don't redo the work or overwrite
            // a fresher parse.
            if state.documents.contains_key(&uri) {
                continue;
            }
            let doc = DocumentState::hydrated_from_cache(
                uri,
                &text,
                entry.symbols.clone(),
                entry.references.clone(),
            );
            // Cache hydration runs in the background at startup, so a
            // `did_open` can race it: the `contains_key` check above is NOT
            // atomic with the insert. Route through the open-guarded upsert
            // (the store's documented contract for every disk-driven path)
            // so a buffer opened in the gap is never clobbered with this
            // stale version-0 cached snapshot.
            if state.upsert_document_unless_open(doc) {
                hydrated += 1;
            }
        }
        hydrated
    }
}

/// XDG-based cache file path for `workspace_root`. Returns
/// `None` when no cache root can be determined (no
/// XDG_CACHE_HOME, no HOME).
fn cache_path_for(workspace_root: &Path) -> Option<PathBuf> {
    let root = cache_root_dir()?;
    let key = workspace_key(workspace_root);
    Some(root.join("index-cache").join(format!("{key}.json")))
}

fn cache_root_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join("terraform-ls-rs"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home).join(".cache").join("terraform-ls-rs"));
    }
    None
}

/// Hash the workspace root path into a stable ~16-char filename
/// stem. Same input → same filename; different workspaces get
/// different caches. Uses the standard `DefaultHasher` (SipHash-
/// like) — collisions are irrelevant for our per-workspace
/// isolation goal.
fn workspace_key(workspace_root: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    workspace_root.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn metadata_mtime_ns(metadata: &std::fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos())
}

/// Remove temp files (`.{base}.tmp.<pid>`) in `parent` that are clearly
/// orphaned — left behind when a previous write was killed between the
/// `write` and the `rename`. Only files older than 60s are touched: a real
/// in-flight write from a concurrent process completes in milliseconds, so
/// this never disturbs a live writer. Best-effort; ignores all errors.
fn sweep_stale_temps(parent: &Path, base: &str) {
    let prefix = format!(".{base}.tmp.");
    let Ok(read_dir) = std::fs::read_dir(parent) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in read_dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let orphaned = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() >= 60);
        if orphaned {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write `data` to `path` atomically: write to a sibling
/// temp file, then rename. Prevents a half-written cache on
/// crash/kill. On rename failure, falls back to direct write
/// (best-effort).
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return std::fs::write(path, data);
    };
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("cache"),
        std::process::id(),
    ));
    std::fs::write(&tmp, data)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::fs;

    /// Serialises tests that mutate the process-global `XDG_CACHE_HOME`
    /// env var, which would otherwise cross-contaminate under cargo's
    /// in-binary parallelism.
    static XDG_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_tf(path: &Path, src: &str) {
        fs::write(path, src).unwrap();
    }

    #[test]
    fn capture_then_hydrate_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let var_path = root.join("variables.tf");
        write_tf(&var_path, "variable \"region\" {}\n");

        // Original store — populate it via a real DocumentState.
        let store_a = StateStore::new();
        let uri = Url::from_file_path(&var_path).unwrap();
        store_a.upsert_document(DocumentState::new(
            uri.clone(),
            "variable \"region\" {}\n",
            1,
        ));

        let cache = IndexCache::capture(&store_a, &root);
        assert_eq!(cache.entries.len(), 1, "one indexed file → one entry");

        // Hydrate into a fresh store.
        let store_b = StateStore::new();
        let hydrated = cache.hydrate_into_store(&store_b);
        assert_eq!(hydrated, 1);
        let doc = store_b.documents.get(&uri).unwrap();
        assert!(
            doc.symbols.variables.contains_key("region"),
            "symbols must round-trip through the cache"
        );
    }

    #[test]
    fn hydrate_skips_files_whose_mtime_changed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let var_path = root.join("variables.tf");
        write_tf(&var_path, "variable \"region\" {}\n");

        let store_a = StateStore::new();
        let uri = Url::from_file_path(&var_path).unwrap();
        store_a.upsert_document(DocumentState::new(
            uri.clone(),
            "variable \"region\" {}\n",
            1,
        ));
        let cache = IndexCache::capture(&store_a, &root);

        // Overwrite on disk with different content. mtime will
        // bump (modern filesystems have ns-resolution; safety
        // margin below).
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_tf(&var_path, "variable \"DIFFERENT\" {}\n");

        let store_b = StateStore::new();
        let hydrated = cache.hydrate_into_store(&store_b);
        assert_eq!(
            hydrated, 0,
            "mtime mismatch must invalidate the cache entry"
        );
        assert!(
            !store_b.documents.contains_key(&uri),
            "stale cache entry must not land in the store"
        );
    }

    #[test]
    fn save_then_load_round_trip() {
        // Use a dedicated XDG cache dir for test isolation.
        let xdg = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let var_path = root.join("variables.tf");
        write_tf(&var_path, "variable \"region\" {}\n");

        // Override cache root for the duration of this test.
        let _g = XDG_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("XDG_CACHE_HOME");
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", xdg.path());
        }

        let store_a = StateStore::new();
        let uri = Url::from_file_path(&var_path).unwrap();
        store_a.upsert_document(DocumentState::new(
            uri.clone(),
            "variable \"region\" {}\n",
            1,
        ));
        let cache_a = IndexCache::capture(&store_a, &root);
        cache_a.save(&root);

        let cache_b = IndexCache::load(&root).expect("cache present");
        assert_eq!(cache_b.entries.len(), 1);
        assert_eq!(cache_b.header.version, CACHE_FORMAT_VERSION);

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
    }

    #[test]
    fn sweep_removes_only_stale_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = "abcd1234.json";
        let stale = dir.path().join(format!(".{base}.tmp.999"));
        let fresh = dir.path().join(format!(".{base}.tmp.1000"));
        let real_cache = dir.path().join(base); // not a temp — must survive
        fs::write(&stale, b"x").unwrap();
        fs::write(&fresh, b"y").unwrap();
        fs::write(&real_cache, b"z").unwrap();
        // Age the stale temp two minutes.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();

        sweep_stale_temps(dir.path(), base);

        assert!(!stale.exists(), "stale temp must be removed");
        assert!(fresh.exists(), "fresh temp (live write) must be kept");
        assert!(real_cache.exists(), "the real cache file must be untouched");
    }

    #[test]
    fn hydrate_skips_when_content_differs_despite_matching_mtime_and_size() {
        // Restore-from-backup / git-checkout: a file comes back with the
        // SAME mtime+size but DIFFERENT content. The content hash must
        // catch it so stale symbols don't hydrate (IDX-3).
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("variables.tf");
        let a = "variable \"aa\" {}\n";
        let b = "variable \"bb\" {}\n";
        assert_eq!(a.len(), b.len(), "fixtures must be the same byte length");
        write_tf(&path, a);

        let store_a = StateStore::new();
        let uri = Url::from_file_path(&path).unwrap();
        store_a.upsert_document(DocumentState::new(uri.clone(), a, 1));
        let cache = IndexCache::capture(&store_a, &root);
        let captured_mtime = fs::metadata(&path).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(10));
        write_tf(&path, b);
        // Force the mtime back to the captured value.
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(captured_mtime).unwrap();
        drop(f);

        // Sanity: mtime+size now match the cached entry; only content differs.
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), cache.entries[0].size);
        assert_eq!(metadata_mtime_ns(&meta), Some(cache.entries[0].mtime_ns));

        let store_b = StateStore::new();
        let hydrated = cache.hydrate_into_store(&store_b);
        assert_eq!(
            hydrated, 0,
            "content-hash mismatch must invalidate despite matching mtime+size"
        );
    }

    #[test]
    fn load_rejects_foreign_workspace_root() {
        // A cache file found at this workspace's path but whose header
        // names a DIFFERENT root (a hash collision) must be discarded, not
        // hydrated into the wrong workspace (IDX-2).
        let xdg = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();

        let _g = XDG_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("XDG_CACHE_HOME");
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", xdg.path());
        }

        let path = cache_path_for(&root).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let foreign = IndexCache {
            header: CacheHeader {
                version: CACHE_FORMAT_VERSION,
                workspace_root: PathBuf::from("/some/other/workspace"),
            },
            entries: vec![],
        };
        fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();

        let loaded = IndexCache::load(&root);

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_CACHE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        assert!(loaded.is_none(), "foreign workspace_root must be rejected");
    }

    #[test]
    fn hydrate_does_not_clobber_an_open_buffer_marked_during_the_race() {
        // The TOCTOU: a `did_open` calls `mark_open` then upserts. If the
        // hydrate's `contains_key` check runs in the gap (open marked, doc
        // not yet inserted), the OLD code would still upsert the stale
        // cached doc. The atomic open-guard must skip it.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let var_path = root.join("variables.tf");
        write_tf(&var_path, "variable \"region\" {}\n");

        let store_a = StateStore::new();
        let uri = Url::from_file_path(&var_path).unwrap();
        store_a.upsert_document(DocumentState::new(uri.clone(), "variable \"region\" {}\n", 1));
        let cache = IndexCache::capture(&store_a, &root);

        // Simulate the race: buffer marked open, doc not yet in `documents`.
        let store_b = StateStore::new();
        store_b.mark_open(uri.clone());
        assert!(!store_b.documents.contains_key(&uri));

        let hydrated = cache.hydrate_into_store(&store_b);
        assert_eq!(hydrated, 0, "must not hydrate over an open buffer");
        assert!(
            !store_b.documents.contains_key(&uri),
            "stale cached doc must not land in the store for an open URI"
        );
    }

    #[test]
    fn hydrate_preserves_live_documents() {
        // `did_open` may fire BEFORE cache hydration completes —
        // if an entry's URI is already live in the store, we
        // must not clobber it.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let var_path = root.join("variables.tf");
        write_tf(&var_path, "variable \"region\" {}\n");

        let store_a = StateStore::new();
        let uri = Url::from_file_path(&var_path).unwrap();
        store_a.upsert_document(DocumentState::new(
            uri.clone(),
            "variable \"region\" {}\n",
            1,
        ));
        let cache = IndexCache::capture(&store_a, &root);

        // Now simulate a live editor session with a newer
        // version of the document.
        let store_b = StateStore::new();
        store_b.upsert_document(DocumentState::new(
            uri.clone(),
            "variable \"EDITED\" {}\n",
            42,
        ));

        let hydrated = cache.hydrate_into_store(&store_b);
        assert_eq!(hydrated, 0, "live doc must not be clobbered");
        let doc = store_b.documents.get(&uri).unwrap();
        assert_eq!(doc.version, 42, "editor version preserved");
        assert!(
            doc.symbols.variables.contains_key("EDITED"),
            "editor contents preserved"
        );
    }
}
