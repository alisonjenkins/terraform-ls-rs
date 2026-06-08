//! Resolve git module refs via `git ls-remote` — host-agnostic, honoring the
//! user's own git/ssh credentials (private repos work, no API tokens). One
//! `git ls-remote --tags` per repo builds a `{tag → commit-sha}` map that
//! powers tag→sha, sha→tag, and newer-version queries. Results are cached on
//! disk (24h TTL + stale fallback), mirroring `registry_versions.rs`.
//!
//! Pure version/SHA reasoning lives in `tfls_core::git_ref`. The `git` binary
//! used here is the system `git` (not `cliBinary`, which is tofu/terraform);
//! `cli_enabled` is only the on/off gate.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tfls_core::git_ref::{looks_like_commit_sha, parse_version_core, sha_matches};

use crate::ProtocolError;

const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(20);
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// A repo's tags as `{name → commit-sha}` (commit, i.e. peeled for annotated
/// tags). Serialized to the on-disk cache.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoTags {
    pub map: Vec<TagEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagEntry {
    pub name: String,
    pub commit_sha: String,
}

/// Runs `git ls-remote <url> <args...>`, returning stdout. Injectable so tests
/// never shell out.
#[async_trait::async_trait]
pub trait LsRemoteRunner: Send + Sync {
    async fn ls_remote(&self, url: &str, args: &[&str]) -> Result<String, ProtocolError>;
}

/// Real runner: the system `git` binary.
pub struct GitCli;

#[async_trait::async_trait]
impl LsRemoteRunner for GitCli {
    async fn ls_remote(&self, url: &str, args: &[&str]) -> Result<String, ProtocolError> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.arg("ls-remote")
            .args(args)
            .arg(url)
            .env("GIT_TERMINAL_PROMPT", "0") // fail fast instead of prompting for creds
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let fut = cmd.output();
        let out = tokio::time::timeout(LS_REMOTE_TIMEOUT, fut)
            .await
            .map_err(|_| ProtocolError::GitRef(format!("git ls-remote timed out for {url}")))?
            .map_err(|e| ProtocolError::GitRef(format!("git ls-remote failed to spawn: {e}")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(ProtocolError::GitRef(format!(
                "git ls-remote {url} failed: {}",
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Normalize a Terraform git module source into a clonable git URL: strip the
/// `git::` prefix, the `//subdir` module subpath, and the `?query`/`#fragment`.
/// Shorthand (`github.com/...`) gets `https://`; scp form (`git@host:org/repo`)
/// and explicit schemes are preserved as-written (so ssh sources resolve via
/// the user's ssh keys).
pub fn normalize_git_url(source: &str) -> Option<String> {
    let s = source.trim();
    let s = s.strip_prefix("git::").unwrap_or(s).trim();
    let s = s.split(['?', '#']).next().unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    let has_scheme = s.contains("://");
    // Strip the `//subdir` module subpath (but not the scheme's `://`).
    let s = if has_scheme {
        let scheme_end = s.find("://").map(|i| i + 3).unwrap_or(0);
        match s[scheme_end..].find("//") {
            Some(rel) => &s[..scheme_end + rel],
            None => s,
        }
    } else {
        match s.find("//") {
            Some(i) => &s[..i],
            None => s,
        }
    };
    let s = s.trim_end_matches('/');
    if s.is_empty() {
        return None;
    }
    if has_scheme {
        return Some(s.to_string());
    }
    // No scheme: scp form (`user@host:path`) is left as-is; bare shorthand
    // (`host/path`) becomes https.
    let is_scp = s
        .find(':')
        .is_some_and(|colon| s[..colon].contains('@') && !s[..colon].contains('/'));
    if is_scp {
        Some(s.to_string())
    } else {
        Some(format!("https://{s}"))
    }
}

/// Parse `git ls-remote --tags` output into a tag→commit-sha map. For annotated
/// tags the peeled `refs/tags/<name>^{}` line (the commit) overrides the tag
/// object's own sha.
pub fn parse_ls_remote_tags(stdout: &str) -> RepoTags {
    use std::collections::HashMap;
    // name -> (sha, peeled?) — peeled wins.
    let mut map: HashMap<String, (String, bool)> = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let (Some(sha), Some(refname)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Some(rest) = refname.strip_prefix("refs/tags/") else {
            continue;
        };
        let (name, peeled) = match rest.strip_suffix("^{}") {
            Some(n) => (n, true),
            None => (rest, false),
        };
        match map.get(name) {
            Some((_, true)) => {} // already have the peeled commit; keep it
            _ => {
                map.insert(name.to_string(), (sha.to_string(), peeled));
            }
        }
    }
    let mut entries: Vec<TagEntry> = map
        .into_iter()
        .map(|(name, (commit_sha, _))| TagEntry { name, commit_sha })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    RepoTags { map: entries }
}

/// All tag names in the repo.
pub fn tag_names(tags: &RepoTags) -> Vec<String> {
    tags.map.iter().map(|e| e.name.clone()).collect()
}

/// Commit SHA a tag points at.
pub fn tag_to_sha<'a>(tags: &'a RepoTags, tag: &str) -> Option<&'a str> {
    tags.map
        .iter()
        .find(|e| e.name == tag)
        .map(|e| e.commit_sha.as_str())
}

/// The tag pointing at `pinned` (exact or ≥7-hex prefix). When several tags
/// point at the same commit, the highest semver wins, else lexical-max.
pub fn sha_to_tag<'a>(tags: &'a RepoTags, pinned: &str) -> Option<&'a str> {
    if !looks_like_commit_sha(pinned) {
        return None;
    }
    let mut matches: Vec<&'a TagEntry> = tags
        .map
        .iter()
        .filter(|e| sha_matches(&e.commit_sha, pinned))
        .collect();
    if matches.is_empty() {
        return None;
    }
    matches.sort_by(
        |a, b| match (parse_version_core(&a.name), parse_version_core(&b.name)) {
            (Some(va), Some(vb)) => va.cmp(&vb),
            _ => a.name.cmp(&b.name),
        },
    );
    matches.last().map(|e| e.name.as_str())
}

// ---- async resolution (cache + runner) ----

/// List a repo's tags, cache-first. Gated on `cli_enabled`.
pub async fn list_repo_tags(source: &str, cli_enabled: bool) -> Result<RepoTags, ProtocolError> {
    if !cli_enabled {
        return Err(ProtocolError::GitRef(
            "git ref resolution disabled (cliEnabled=false)".into(),
        ));
    }
    list_repo_tags_with_runner(source, &GitCli).await
}

/// As [`list_repo_tags`] but with an injected runner (no `cli_enabled` gate —
/// the caller gates). Cache-first; falls back to stale cache on fetch failure.
pub async fn list_repo_tags_with_runner(
    source: &str,
    runner: &dyn LsRemoteRunner,
) -> Result<RepoTags, ProtocolError> {
    let url = normalize_git_url(source)
        .ok_or_else(|| ProtocolError::GitRef(format!("not a git source: {source}")))?;
    let path = cache_path(&url);

    if let Some(fresh) = read_fresh_cache(&path).await {
        return Ok(fresh);
    }
    match runner.ls_remote(&url, &["--tags"]).await {
        Ok(stdout) => {
            let tags = parse_ls_remote_tags(&stdout);
            write_cache(&path, &tags).await;
            Ok(tags)
        }
        Err(e) => {
            if let Some(stale) = read_any_cache(&path).await {
                tracing::debug!(error = %e, url, "git ls-remote failed; serving stale cache");
                Ok(stale)
            } else {
                Err(e)
            }
        }
    }
}

/// Resolve a ref name (tag or branch) to a commit SHA. Tags come from the
/// cached `--tags` map; anything not a known tag is tried as a branch tip, then
/// a peeled tag refspec.
pub async fn resolve_ref_to_sha(
    source: &str,
    ref_name: &str,
    cli_enabled: bool,
) -> Result<String, ProtocolError> {
    if !cli_enabled {
        return Err(ProtocolError::GitRef(
            "git ref resolution disabled (cliEnabled=false)".into(),
        ));
    }
    let tags = list_repo_tags(source, cli_enabled).await?;
    if let Some(sha) = tag_to_sha(&tags, ref_name) {
        return Ok(sha.to_string());
    }
    // Not a known tag — try branch tip, then a one-off peeled tag refspec.
    let url = normalize_git_url(source)
        .ok_or_else(|| ProtocolError::GitRef(format!("not a git source: {source}")))?;
    let runner = GitCli;
    for refspec in [
        format!("refs/heads/{ref_name}"),
        format!("refs/tags/{ref_name}^{{}}"),
        format!("refs/tags/{ref_name}"),
    ] {
        if let Ok(out) = runner.ls_remote(&url, &[&refspec]).await {
            if let Some(sha) = out.split_whitespace().next() {
                if looks_like_commit_sha(sha) {
                    return Ok(sha.to_string());
                }
            }
        }
    }
    Err(ProtocolError::GitRef(format!(
        "could not resolve ref `{ref_name}` for {url}"
    )))
}

/// Whether a fresh (within TTL) tag cache exists for the source's repo. Used by
/// the prefetch to skip already-warm repos.
pub fn is_repo_tags_cached(source: &str) -> bool {
    let Some(url) = normalize_git_url(source) else {
        return false;
    };
    let path = cache_path(&url);
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age <= CACHE_TTL)
        .unwrap_or(false)
}

/// Synchronous, offline cache read for diagnostics (no network, ignores TTL —
/// best-effort; the prefetch keeps it fresh).
pub fn read_cached_repo_tags(source: &str) -> Option<RepoTags> {
    let url = normalize_git_url(source)?;
    let path = cache_path(&url);
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<RepoTags>(&body).ok()
}

// ---- cache plumbing (mirrors registry_versions.rs) ----

fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("terraform-ls-rs").join("git-refs");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("terraform-ls-rs")
            .join("git-refs");
    }
    PathBuf::from("/tmp/terraform-ls-rs/git-refs")
}

fn sanitise(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn cache_path(url: &str) -> PathBuf {
    cache_root().join(sanitise(url)).join("tags.json")
}

async fn read_fresh_cache(path: &Path) -> Option<RepoTags> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    let age = SystemTime::now()
        .duration_since(meta.modified().ok()?)
        .unwrap_or(Duration::MAX);
    if age > CACHE_TTL {
        return None;
    }
    read_cache_contents(path).await
}

async fn read_any_cache(path: &Path) -> Option<RepoTags> {
    read_cache_contents(path).await
}

async fn read_cache_contents(path: &Path) -> Option<RepoTags> {
    let body = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str::<RepoTags>(&body).ok()
}

async fn write_cache(path: &Path, tags: &RepoTags) {
    if let Some(parent) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            tracing::debug!(error = %e, dir = %parent.display(), "git-refs cache dir create failed");
            return;
        }
    }
    match serde_json::to_string(tags) {
        Ok(body) => {
            if let Err(e) = tokio::fs::write(path, body).await {
                tracing::debug!(error = %e, path = %path.display(), "git-refs cache write failed");
            }
        }
        Err(e) => tracing::debug!(error = %e, "git-refs cache serialise failed"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn normalize_variants() {
        assert_eq!(
            normalize_git_url("git::ssh://git@github.com/org/repo.git//modules/vpc?ref=v1.2.3"),
            Some("ssh://git@github.com/org/repo.git".to_string())
        );
        assert_eq!(
            normalize_git_url("git::https://host/org/repo.git//mod?ref=v1"),
            Some("https://host/org/repo.git".to_string())
        );
        assert_eq!(
            normalize_git_url("github.com/org/repo//mod?ref=v1"),
            Some("https://github.com/org/repo".to_string())
        );
        assert_eq!(
            normalize_git_url("git@github.com:org/repo.git?ref=v1"),
            Some("git@github.com:org/repo.git".to_string())
        );
        assert_eq!(
            normalize_git_url("bitbucket.org/org/repo?ref=v1"),
            Some("https://bitbucket.org/org/repo".to_string())
        );
    }

    const LS: &str = "\
1111111111111111111111111111111111111111\trefs/tags/v1.0.0
2222222222222222222222222222222222222222\trefs/tags/v1.1.0
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\trefs/tags/v2.0.0
3333333333333333333333333333333333333333\trefs/tags/v2.0.0^{}
";

    #[test]
    fn parse_prefers_peeled() {
        let tags = parse_ls_remote_tags(LS);
        // v2.0.0 annotated: peeled commit 3333 overrides tag object aaaa.
        assert_eq!(
            tag_to_sha(&tags, "v2.0.0"),
            Some("3333333333333333333333333333333333333333")
        );
        assert_eq!(
            tag_to_sha(&tags, "v1.0.0"),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(tag_to_sha(&tags, "nope"), None);
    }

    #[test]
    fn reverse_lookup_and_prefix() {
        let tags = parse_ls_remote_tags(LS);
        assert_eq!(
            sha_to_tag(&tags, "1111111111111111111111111111111111111111"),
            Some("v1.0.0")
        );
        assert_eq!(sha_to_tag(&tags, "1111111"), Some("v1.0.0")); // abbrev prefix
        assert_eq!(sha_to_tag(&tags, "deadbeef"), None);
    }

    #[test]
    fn normalize_strips_subdir_and_query_for_scp() {
        // scp form with a //subdir and ?ref — preserved as scp, subdir+query gone.
        assert_eq!(
            normalize_git_url("git@github.com:org/repo.git//modules/vpc?ref=modules/vpc/v1.0.0"),
            Some("git@github.com:org/repo.git".to_string())
        );
    }
}
