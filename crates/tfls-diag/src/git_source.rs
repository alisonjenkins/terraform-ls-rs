//! Shared helpers for git module `source` strings: source classification,
//! `?ref=` extraction (value + byte span), and trailing `# <tag>` comment
//! parsing. Used by `module_pinned_source`, `module_shallow_clone`,
//! `module_mutable_ref`, `module_ref_tag_mismatch`, `module_outdated`, and the
//! code actions in `tfls-lsp`.
//!
//! Pure version/SHA reasoning lives in `tfls_core::git_ref`.

use ropey::Rope;

/// A go-getter-style git module source (`git::…`, `github.com/…`, `*.git`).
pub fn is_git_source(src: &str) -> bool {
    let trimmed = src.trim();
    trimmed.starts_with("git::")
        || trimmed.starts_with("github.com/")
        || trimmed.starts_with("bitbucket.org/")
        || trimmed.ends_with(".git")
        || trimmed.contains(".git?")
        || trimmed.contains(".git#")
}

/// The ref value pinned in a source string: `?ref=VALUE`, `&ref=VALUE`, or a
/// `#fragment`. Returns the value substring (no offsets — see [`ref_value_span`]).
pub fn extract_ref(src: &str) -> Option<&str> {
    let (s, _) = ref_value_with_span(src)?;
    Some(s)
}

/// Byte offsets `(start, end)` of the ref VALUE within `src` (the unquoted
/// source string). Lets a code action replace exactly the ref token.
pub fn ref_value_span(src: &str) -> Option<(usize, usize)> {
    let (_, span) = ref_value_with_span(src)?;
    Some(span)
}

fn ref_value_with_span(src: &str) -> Option<(&str, (usize, usize))> {
    for marker in ["?ref=", "&ref="] {
        if let Some(start) = src.find(marker) {
            let val_start = start + marker.len();
            let rest = &src[val_start..];
            let end = val_start + rest.find('&').unwrap_or(rest.len());
            return Some((&src[val_start..end], (val_start, end)));
        }
    }
    if let Some(hash) = src.find('#') {
        let val_start = hash + 1;
        return Some((&src[val_start..], (val_start, src.len())));
    }
    None
}

/// Read the tag named in a trailing `# <tag>` / `// <tag>` comment on the same
/// line as the source string. `after_quote_byte` is the byte offset just past
/// the source string's closing quote. Returns the first whitespace-delimited
/// token after the comment marker (the tag), ignoring any following prose.
pub fn trailing_comment_tag(rope: &Rope, after_quote_byte: usize) -> Option<String> {
    let line_idx = rope.try_byte_to_line(after_quote_byte).ok()?;
    let line = rope.get_line(line_idx)?;
    let line_str = line.to_string();
    // Offset of `after_quote_byte` within this line.
    let line_start_byte = rope.try_line_to_byte(line_idx).ok()?;
    let from = after_quote_byte
        .saturating_sub(line_start_byte)
        .min(line_str.len());
    let tail = &line_str[from..];
    let marker = tail.find("//").or_else(|| tail.find('#'))?;
    let after = &tail[marker..];
    let after = after.trim_start_matches('#').trim_start_matches('/');
    after.split_whitespace().next().map(|t| t.to_string())
}

/// Whether a trailing `#`/`//` comment already exists on the source line,
/// anchored after the closing quote.
pub fn has_trailing_comment(rope: &Rope, after_quote_byte: usize) -> bool {
    let Ok(line_idx) = rope.try_byte_to_line(after_quote_byte) else {
        return false;
    };
    let Some(line) = rope.get_line(line_idx) else {
        return false;
    };
    let line_str = line.to_string();
    let Ok(line_start_byte) = rope.try_line_to_byte(line_idx) else {
        return false;
    };
    let from = after_quote_byte
        .saturating_sub(line_start_byte)
        .min(line_str.len());
    let tail = &line_str[from..];
    tail.contains('#') || tail.contains("//")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn git_source_classification() {
        assert!(is_git_source("git::https://example.com/foo.git?ref=v1"));
        assert!(is_git_source("git::ssh://git@github.com/o/r//m?ref=v1"));
        assert!(is_git_source("github.com/org/repo"));
        assert!(!is_git_source("./local"));
        assert!(!is_git_source("hashicorp/consul/aws"));
    }

    #[test]
    fn ref_extraction_and_span() {
        let s = "git::ssh://git@github.com/o/r//m?ref=v1.2.3";
        assert_eq!(extract_ref(s), Some("v1.2.3"));
        let (a, b) = ref_value_span(s).unwrap();
        assert_eq!(&s[a..b], "v1.2.3");

        let s2 = "git::ssh://git@host/o/r?ref=abc123&depth=1";
        assert_eq!(extract_ref(s2), Some("abc123"));
        let (a, b) = ref_value_span(s2).unwrap();
        assert_eq!(&s2[a..b], "abc123");

        let s3 = "git::https://example.com/foo.git#deadbeef";
        assert_eq!(extract_ref(s3), Some("deadbeef"));
        let (a, b) = ref_value_span(s3).unwrap();
        assert_eq!(&s3[a..b], "deadbeef");

        assert_eq!(extract_ref("git::https://example.com/foo.git"), None);
    }

    #[test]
    fn trailing_comment_parsing() {
        let src = "  source = \"git::ssh://h/o/r?ref=abc\" # v1.2.3\n";
        let rope = Rope::from_str(src);
        // byte just after the closing quote
        let after = src.find('"').unwrap();
        let after = src[after + 1..].find('"').unwrap() + after + 2;
        assert_eq!(
            trailing_comment_tag(&rope, after),
            Some("v1.2.3".to_string())
        );
        assert!(has_trailing_comment(&rope, after));

        let src2 = "  source = \"git::ssh://h/o/r?ref=abc\"\n";
        let rope2 = Rope::from_str(src2);
        let q1 = src2.find('"').unwrap();
        let q2 = src2[q1 + 1..].find('"').unwrap() + q1 + 2;
        assert_eq!(trailing_comment_tag(&rope2, q2), None);
        assert!(!has_trailing_comment(&rope2, q2));
    }

    #[test]
    fn trailing_comment_slash_and_prose() {
        let src = "  source = \"x\" // v2.0.0 pinned for stability\n";
        let rope = Rope::from_str(src);
        let q1 = src.find('"').unwrap();
        let q2 = src[q1 + 1..].find('"').unwrap() + q1 + 2;
        assert_eq!(trailing_comment_tag(&rope, q2), Some("v2.0.0".to_string()));
    }
}
