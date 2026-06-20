//! Panic-safe wrappers around `hcl-edit` parser entry points.
//!
//! `hcl-edit 0.9.6` (our pinned version) ships internal-invariant
//! `.unwrap()` calls in its parser layer:
//!
//! - `src/parser/state.rs:{41,95,109,123,151}` — `self.current.take().unwrap()`
//! - `src/parser/expr.rs:743` — `namespace.pop().unwrap()`
//! - `src/parser/template.rs:60` — `lit.span().unwrap()`
//! - `src/raw_string.rs:58-59` — explicit `panic!`
//! - `src/expr/object.rs:485,505` — `obj.remove_entry(...).unwrap()`
//!
//! On certain malformed inputs these fire as panics, propagating
//! through whatever async task happened to call `Body::parse`. We
//! can't pre-validate those inputs, so EVERY hcl-edit parser call
//! in production code goes through this module.
//!
//! Workspace lint policy denies `unwrap_used` / `expect_used` /
//! `panic` in production code; this module's `catch_unwind` is the
//! one place we accept that the third-party parser violates that
//! policy and isolate the blast radius. Once hcl-edit ships a
//! panic-free parser (track at
//! <https://github.com/martinohmann/hcl-rs>), drop the wrappers
//! and let the parser surface errors via `Result` like everything
//! else.

use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};

use hcl_edit::structure::Body;
use thiserror::Error;

/// Maximum bracket / brace / paren nesting depth we hand to hcl-edit.
///
/// hcl-edit's parser is recursive descent, so each level of `(`, `[`,
/// or `{` consumes a stack frame. `catch_unwind` (the rest of this
/// module) isolates *panics*, but a stack overflow is an uncatchable
/// `SIGSEGV`/`SIGABRT` that would take the whole server process down —
/// and the backend feeds untrusted document text through here on every
/// `did_open` / `did_change`. A pathological native-HCL input
/// (`((((…))))`, deeply nested objects/arrays) can blow the thread stack
/// before the parser even reports an error. (The `.tf.json` path doesn't
/// reach here for its initial parse — it goes through `serde_json`, which
/// already enforces its own recursion limit — so this guard targets the
/// remaining HCL vector.)
///
/// So we do one cheap O(n), allocation-free linear pre-scan for the
/// maximum nesting depth and refuse anything past this bound *before*
/// calling hcl-edit. 500 is far deeper than any hand-written HCL (real
/// configs rarely exceed single-digit nesting) yet stays well within the
/// stack: production parses run on tokio's `spawn_blocking` pool (default
/// ~2 MiB stack), which tolerates depths far past this bound, so the limit
/// never affects normal-depth inputs.
const MAX_NESTING_DEPTH: usize = 500;

/// Reported reason an hcl-edit parser entry point panicked. Carries
/// enough context (excerpt + byte count + payload message) for a
/// human reader of the journal log to identify the offending input
/// and either fix the file or file an upstream issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsePanic {
    /// Stringified panic payload — usually the message that came
    /// out of `hcl-edit`'s `.unwrap()` or `panic!`. Falls back to a
    /// placeholder when the panic carried a non-string payload.
    pub message: String,
    /// First ~256 chars of the input source, with newlines escaped
    /// for log readability. Truncated; full input lives on disk.
    pub source_excerpt: String,
    /// Total byte length of the input source.
    pub source_bytes: usize,
}

impl std::fmt::Display for ParsePanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hcl-edit parser panicked: {} (source: {} bytes; excerpt: {:?})",
            self.message, self.source_bytes, self.source_excerpt,
        )
    }
}

impl std::error::Error for ParsePanic {}

/// Unified error type for `hcl-edit::Body` parsing: either a normal
/// hcl-edit syntax error (returned as `Result::Err` in the upstream
/// API) or a panic ferried out via `catch_unwind`.
#[derive(Debug, Error)]
pub enum BodyParseError {
    #[error("HCL syntax error: {0}")]
    Syntax(#[source] hcl_edit::parser::Error),

    #[error(transparent)]
    Panicked(#[from] ParsePanic),
}

/// Parse `source` as a `Body`, isolating any panic from the
/// hcl-edit parser. On panic: returns `Err(BodyParseError::Panicked)`
/// and emits a structured `error!` log naming the offending input.
pub fn parse_body(source: &str) -> Result<Body, BodyParseError> {
    // Guard the recursive-descent parser against stack overflow on
    // pathologically deep nesting BEFORE handing the input to hcl-edit:
    // a stack overflow is an uncatchable SIGSEGV that `catch` below
    // cannot intercept, so it must never reach the parser.
    if let Some(depth) = excessive_nesting_depth(source) {
        return Err(BodyParseError::Panicked(ParsePanic {
            message: format!(
                "input nesting depth {depth} exceeds the safe limit of \
                 {MAX_NESTING_DEPTH}; refused before parsing to avoid a \
                 recursive-descent stack overflow",
            ),
            source_excerpt: source_excerpt(source),
            source_bytes: source.len(),
        }));
    }
    let parsed = catch(source, || source.parse::<Body>())?;
    parsed.map_err(BodyParseError::Syntax)
}

/// One linear, allocation-free pass over `source` tracking bracket /
/// brace / paren nesting depth. Returns `Some(depth)` at the first
/// point the running depth exceeds [`MAX_NESTING_DEPTH`] (so we bail
/// early on huge inputs), or `None` if the whole input stays within
/// bounds.
///
/// Deliberately coarse: it counts every `(`, `[`, `{` as an open and
/// every `)`, `]`, `}` as a close, ignoring whether they sit inside a
/// string or comment. That over-counts in rare cases, but the bound is
/// so far above legitimate HCL depth that a few stray bracket chars in
/// a string never matter — and the goal is purely to keep hcl-edit's
/// recursion off the cliff edge, not to validate syntax.
fn excessive_nesting_depth(source: &str) -> Option<usize> {
    let mut depth: usize = 0;
    for &b in source.as_bytes() {
        match b {
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > MAX_NESTING_DEPTH {
                    return Some(depth);
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

/// Generic wrapper: run `f` against `source` under `catch_unwind`,
/// converting any panic into a structured [`ParsePanic`] and
/// emitting a tracing `error!` record. Reuse this any time we add
/// a new hcl-edit parser entry point (e.g. `Expression::parse`,
/// `Template::parse`) so we never grow a parallel `catch_unwind`
/// at a call site.
pub fn catch<F, T>(source: &str, f: F) -> Result<T, ParsePanic>
where
    F: FnOnce() -> T,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(t) => Ok(t),
        Err(payload) => {
            let panic = ParsePanic {
                message: panic_payload_message(payload.as_ref()),
                source_excerpt: source_excerpt(source),
                source_bytes: source.len(),
            };
            tracing::error!(
                bytes = panic.source_bytes,
                excerpt = %panic.source_excerpt,
                message = %panic.message,
                "hcl-edit parser panicked — caller will skip this input",
            );
            Err(panic)
        }
    }
}

/// Best-effort extraction of a panic payload's message. `Box<Any>`
/// commonly carries either `&'static str` or `String`; everything
/// else degrades to `<non-string panic payload>`.
fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

/// First 256 chars of `source`, with newlines escaped for log
/// readability. Enough to triage which file (and which construct)
/// provoked the panic without dumping arbitrarily large inputs
/// into the journal.
fn source_excerpt(source: &str) -> String {
    let mut out: String = source.chars().take(256).collect();
    out = out.replace('\n', "\\n").replace('\r', "\\r");
    if source.len() > out.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parse_body_returns_ok_on_valid_hcl() {
        let body = parse_body("a = 1\n").expect("valid HCL parses");
        assert_eq!(body.iter().count(), 1);
    }

    #[test]
    fn parse_body_returns_syntax_err_on_invalid_hcl() {
        let err = parse_body("not valid {{").expect_err("malformed parses to error");
        assert!(matches!(err, BodyParseError::Syntax(_)));
    }

    #[test]
    fn catch_returns_panic_struct_on_explicit_panic() {
        let result: Result<i32, _> = catch("source body here\nmore content", || {
            panic!("boom from test")
        });
        let panic = result.expect_err("explicit panic should surface");
        assert!(panic.message.contains("boom from test"));
        assert_eq!(panic.source_bytes, "source body here\nmore content".len());
        assert!(panic.source_excerpt.contains("source body here"));
        assert!(
            !panic.source_excerpt.contains('\n'),
            "newlines should be escaped"
        );
    }

    #[test]
    fn catch_passes_value_through_on_no_panic() {
        let result = catch("ignored", || 42);
        assert_eq!(result.expect("no panic"), 42);
    }

    #[test]
    fn over_depth_input_is_refused_without_crashing() {
        // Nesting depth far past MAX_NESTING_DEPTH would risk a stack
        // overflow inside hcl-edit's recursive-descent parser (an
        // uncatchable SIGSEGV). The pre-scan must reject it cleanly as
        // a parse error, never reaching hcl-edit.
        let depth = MAX_NESTING_DEPTH + 50;
        let src = format!("a = {}{}", "(".repeat(depth), ")".repeat(depth));
        let err = parse_body(&src).expect_err("over-depth input must error");
        match err {
            BodyParseError::Panicked(p) => {
                assert!(
                    p.message.contains("nesting depth"),
                    "message should explain the depth guard: {}",
                    p.message
                );
            }
            other => panic!("expected Panicked guard error, got {other:?}"),
        }
    }

    #[test]
    fn over_depth_bracket_storm_is_refused() {
        // A payload of `[` repeated many thousands of times is the classic
        // recursive-descent stack-overflow trigger. Even unbalanced, the
        // pre-scan trips before hcl-edit recurses. (Real `.tf.json` reaches
        // serde_json, not this path; this guards the native-HCL parser.)
        let src = "[".repeat(MAX_NESTING_DEPTH * 200);
        let err = parse_body(&src).expect_err("bracket storm must error");
        assert!(matches!(err, BodyParseError::Panicked(_)));
    }

    #[test]
    fn legal_deep_nesting_under_bound_still_parses() {
        // Deeply but legally nested — well under the bound — must parse
        // exactly as before (no behaviour change for normal inputs).
        let depth = 100;
        assert!(depth < MAX_NESTING_DEPTH);
        let src = format!("a = {}1{}\n", "(".repeat(depth), ")".repeat(depth));
        let body = parse_body(&src).expect("legal deep nesting parses");
        assert_eq!(body.iter().count(), 1);
    }

    #[test]
    fn depth_scan_handles_balanced_brackets_without_tripping() {
        // Many shallow, sequential bracket pairs (high total count, low
        // depth) must NOT trip the guard — the scan tracks depth, not
        // cumulative count.
        let src = format!("a = [{}]\n", "[], ".repeat(MAX_NESTING_DEPTH * 4));
        assert!(
            excessive_nesting_depth(&src).is_none(),
            "shallow-but-many brackets must stay under the depth bound"
        );
    }

    #[test]
    fn excerpt_truncates_long_sources() {
        let big = "x".repeat(1024);
        let result: Result<(), _> = catch(&big, || panic!("trip"));
        let panic = result.unwrap_err();
        assert_eq!(panic.source_bytes, 1024);
        assert!(panic.source_excerpt.ends_with('…'));
    }
}
