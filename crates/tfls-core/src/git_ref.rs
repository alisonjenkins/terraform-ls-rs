//! Pure helpers for reasoning about git module refs: commit-SHA
//! detection, tag-namespace splitting (for monorepo per-module tags), and
//! newer-version computation. Kept in `tfls-core` so both `tfls-diag`
//! (diagnostics) and `tfls-provider-protocol` (the `git ls-remote` resolver)
//! can share them without depending on each other.

/// True iff `r` is an immutable commit SHA: between 7 and 64 ascii-hex chars.
/// Full sha1 is 40, sha256 is 64; abbreviations down to 7 are accepted as
/// "immutable enough" (git's conventional minimum). Anything shorter, or with
/// non-hex characters (tags like `v1.2.3`, branches like `main`), is treated as
/// a mutable ref.
pub fn looks_like_commit_sha(r: &str) -> bool {
    (7..=64).contains(&r.len()) && r.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether a `git ls-remote` commit SHA (always full) matches a `pinned` ref.
/// Exact when `pinned` is a full SHA; prefix-match when `pinned` is an
/// abbreviation (≥7 hex). Case-insensitive.
pub fn sha_matches(full: &str, pinned: &str) -> bool {
    let full = full.to_ascii_lowercase();
    let pinned = pinned.to_ascii_lowercase();
    if !looks_like_commit_sha(&pinned) {
        return false;
    }
    if pinned.len() >= 40 {
        full == pinned
    } else {
        full.starts_with(&pinned)
    }
}

/// Parse a tag's version core into a `semver::Version`. Strips an optional
/// leading `v`/`V`; tolerates a missing patch (`1.2` → `1.2.0`).
pub fn parse_version_core(core: &str) -> Option<semver::Version> {
    let s = core.strip_prefix(['v', 'V']).unwrap_or(core);
    if let Ok(v) = semver::Version::parse(s) {
        return Some(v);
    }
    // Tolerate `X.Y` (no patch): append `.0` and retry, but only when the
    // string is purely `digits.digits` (avoid mangling pre-release/build).
    if s.split('.').count() == 2
        && s.split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        if let Ok(v) = semver::Version::parse(&format!("{s}.0")) {
            return Some(v);
        }
    }
    None
}

/// Split a tag into `(prefix, version-core)` where the core is the trailing
/// semver and the prefix is the module namespace (everything before it). The
/// leftmost split whose remainder parses as a version wins, so the prefix is
/// the full namespace:
///   `modules/vpc/v1.2.3` → ("modules/vpc/", "v1.2.3")
///   `vpc-v1.2.3`         → ("vpc-", "v1.2.3")
///   `v1.2.3`             → ("", "v1.2.3")
///   `1.2.3`              → ("", "1.2.3")
/// Returns `None` if no suffix of the tag parses as a version.
pub fn tag_namespace(tag: &str) -> Option<(&str, &str)> {
    for (i, _) in tag.char_indices() {
        let rest = &tag[i..];
        if parse_version_core(rest).is_some() {
            return Some((&tag[..i], rest));
        }
    }
    None
}

/// Given every tag in a repo and the currently pinned tag, return the tags
/// that are newer versions *within the same namespace*, highest first.
///
/// - Only tags sharing `current`'s prefix are considered (monorepo isolation).
/// - Only strictly-higher semver versions are returned.
/// - When `current` is a stable release, pre-release candidates are excluded
///   (don't suggest `v2.0.0-rc1` over `v1.9.0`); when `current` is itself a
///   pre-release, pre-releases are allowed.
///
/// Empty when `current` is unparseable or already the latest.
pub fn newer_versions(all_tags: &[String], current: &str) -> Vec<String> {
    let Some((cur_prefix, cur_core)) = tag_namespace(current) else {
        return Vec::new();
    };
    let Some(cur_ver) = parse_version_core(cur_core) else {
        return Vec::new();
    };
    let allow_pre = !cur_ver.pre.is_empty();

    let mut cands: Vec<(semver::Version, String)> = Vec::new();
    for tag in all_tags {
        let Some((p, c)) = tag_namespace(tag) else {
            continue;
        };
        if p != cur_prefix {
            continue;
        }
        let Some(v) = parse_version_core(c) else {
            continue;
        };
        if v <= cur_ver {
            continue;
        }
        if !allow_pre && !v.pre.is_empty() {
            continue;
        }
        cands.push((v, tag.clone()));
    }
    cands.sort_by(|a, b| b.0.cmp(&a.0));
    cands.dedup_by(|a, b| a.1 == b.1);
    cands.into_iter().map(|(_, t)| t).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn sha_detection() {
        assert!(looks_like_commit_sha("abc1234")); // 7 hex
        assert!(looks_like_commit_sha(&"a".repeat(40)));
        assert!(looks_like_commit_sha(&"a".repeat(64)));
        assert!(!looks_like_commit_sha("abc12")); // 5 hex, too short
        assert!(!looks_like_commit_sha("v1.2.3"));
        assert!(!looks_like_commit_sha("main"));
        assert!(!looks_like_commit_sha("deadbeefg")); // non-hex g
    }

    #[test]
    fn sha_match_exact_and_prefix() {
        let full = "9f8e7d6c5b4a3f2e1d0c9b8a7f6e5d4c3b2a1f0e";
        assert!(sha_matches(full, full));
        assert!(sha_matches(full, "9f8e7d6")); // abbrev prefix
        assert!(!sha_matches(full, "deadbee"));
        assert!(!sha_matches(full, "v1.2.3"));
    }

    #[test]
    fn namespace_splitting() {
        assert_eq!(
            tag_namespace("modules/vpc/v1.2.3"),
            Some(("modules/vpc/", "v1.2.3"))
        );
        assert_eq!(tag_namespace("vpc-v1.2.3"), Some(("vpc-", "v1.2.3")));
        assert_eq!(tag_namespace("v1.2.3"), Some(("", "v1.2.3")));
        assert_eq!(tag_namespace("1.2.3"), Some(("", "1.2.3")));
        assert_eq!(tag_namespace("not-a-version"), None);
    }

    #[test]
    fn newer_versions_same_namespace_only() {
        let all = vec![
            "modules/vpc/v1.0.0".into(),
            "modules/vpc/v1.1.0".into(),
            "modules/vpc/v2.0.0".into(),
            "modules/rds/v9.9.9".into(), // different namespace, must be ignored
        ];
        let n = newer_versions(&all, "modules/vpc/v1.0.0");
        assert_eq!(n, vec!["modules/vpc/v2.0.0", "modules/vpc/v1.1.0"]);
    }

    #[test]
    fn newer_versions_excludes_prerelease_for_stable() {
        let all = vec!["v1.0.0".into(), "v1.1.0".into(), "v2.0.0-rc1".into()];
        let n = newer_versions(&all, "v1.0.0");
        assert_eq!(n, vec!["v1.1.0"]);
    }

    #[test]
    fn newer_versions_allows_prerelease_when_current_is_prerelease() {
        let all = vec!["v2.0.0-rc1".into(), "v2.0.0-rc2".into()];
        let n = newer_versions(&all, "v2.0.0-rc1");
        assert_eq!(n, vec!["v2.0.0-rc2"]);
    }

    #[test]
    fn newer_versions_empty_when_latest() {
        let all = vec!["v1.0.0".into(), "v1.1.0".into()];
        assert!(newer_versions(&all, "v1.1.0").is_empty());
    }

    #[test]
    fn missing_patch_tolerated() {
        assert!(parse_version_core("v1.2").is_some());
        assert_eq!(newer_versions(&["v1.3".to_string()], "v1.2"), vec!["v1.3"]);
    }
}
