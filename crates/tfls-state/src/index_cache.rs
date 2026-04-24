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

use lsp_types::Url;
use serde::{Deserialize, Serialize};
use tfls_core::SymbolTable;
use tfls_parser::Reference;

use crate::document::DocumentState;
use crate::store::StateStore;

/// Bump when the cache entry shape changes incompatibly. Old
/// caches then get discarded silently on the next open.
/// v2 changed the wire shape of `SymbolTable.resources` +
/// `data_sources` + `for_each_shapes` + `data_source_for_each_shapes`
/// from a JSON object (broken: `ResourceAddress` is a struct, not a
/// string, so serde_json rejected it) to a JSON array of 2-tuples.
/// v1 caches are discarded silently on load.
const CACHE_FORMAT_VERSION: u32 = 2;

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
            let Ok(uri) = Url::from_file_path(&entry.path) else {
                continue;
            };
            // Skip if the editor has already upserted this doc
            // (e.g. `did_open` fired for it before the cache
            // load finished) — don't clobber live state.
            if state.documents.contains_key(&uri) {
                continue;
            }
            let doc = DocumentState::hydrated_from_cache(
                uri,
                &text,
                entry.symbols.clone(),
                entry.references.clone(),
            );
            state.upsert_document(doc);
            hydrated += 1;
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
        return Some(
            PathBuf::from(home)
                .join(".cache")
                .join("terraform-ls-rs"),
        );
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
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("cache"),
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
        let prev = std::env::var_os("XDG_CACHE_HOME");
        unsafe { std::env::set_var("XDG_CACHE_HOME", xdg.path()); }

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
