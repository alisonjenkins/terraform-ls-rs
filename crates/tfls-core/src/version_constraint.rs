//! Parser and semantic evaluator for Terraform-style version
//! constraints — the grammar used in `required_version`, provider
//! `version` inside `required_providers`, and module `version`.
//!
//! Grammar (case-insensitive whitespace):
//!
//! ```text
//! constraints = constraint ("," constraint)*
//! constraint  = operator? version
//! operator    = "=" | "!=" | ">" | ">=" | "<" | "<=" | "~>"
//! version     = DIGIT+ ("." DIGIT+)* ("-" IDENT)? ("+" IDENT)?
//! ```
//!
//! When the operator is omitted the constraint is an exact match
//! (same as `=`). Multiple constraints separated by commas are ANDed
//! together.
//!
//! The module exposes:
//! - `parse` — split a constraint string into `Constraint`s with
//!   structured errors reported at byte spans within the input.
//! - `cursor_slot` — tells completion what the user is mid-typing
//!   (operator, version, or mid-constraint continuation).
//! - `satisfies_all` — evaluates a candidate version against a
//!   parsed constraint list (AND).
//! - `ConstraintOp::short_description` / `::long_description` — shared
//!   copy used by completion item `detail` / `documentation`.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstraintOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    /// Terraform's "pessimistic" constraint `~>`. Allows updates to
    /// the *last* specified segment only.
    Pessimistic,
}

impl ConstraintOp {
    /// Token the user types.
    pub fn token(self) -> &'static str {
        match self {
            ConstraintOp::Eq => "=",
            ConstraintOp::Ne => "!=",
            ConstraintOp::Gt => ">",
            ConstraintOp::Gte => ">=",
            ConstraintOp::Lt => "<",
            ConstraintOp::Lte => "<=",
            ConstraintOp::Pessimistic => "~>",
        }
    }

    /// One-liner for completion-item `detail`.
    pub fn short_description(self) -> &'static str {
        match self {
            ConstraintOp::Eq => "exact version match",
            ConstraintOp::Ne => "exclude a specific version",
            ConstraintOp::Gt => "strictly greater than",
            ConstraintOp::Gte => "greater than or equal to",
            ConstraintOp::Lt => "strictly less than",
            ConstraintOp::Lte => "less than or equal to",
            ConstraintOp::Pessimistic => "pessimistic / allow-last-segment bumps",
        }
    }

    /// Markdown shown in the completion item's hover documentation.
    pub fn long_description(self) -> &'static str {
        match self {
            ConstraintOp::Eq => {
                "Allows only the exact version specified. Same as writing the version with no operator.\n\nExample: `= 1.2.3`."
            }
            ConstraintOp::Ne => {
                "Allows anything *except* this exact version. Useful for blocking a known-broken release while keeping the rest of a range.\n\nExample: `>= 1.0, != 1.2.4`."
            }
            ConstraintOp::Gt => {
                "Allows any version strictly newer than the one specified.\n\nExample: `> 1.2.3` matches `1.2.4`, `1.3.0`, `2.0.0` but not `1.2.3`."
            }
            ConstraintOp::Gte => {
                "Allows the specified version and anything newer. The most common floor.\n\nExample: `>= 1.2.3`."
            }
            ConstraintOp::Lt => {
                "Allows any version strictly older than the one specified. Typically used as an upper bound.\n\nExample: `< 2.0.0`."
            }
            ConstraintOp::Lte => {
                "Allows the specified version and anything older.\n\nExample: `<= 1.9.8`."
            }
            ConstraintOp::Pessimistic => {
                "Matches versions in a range that allows updates only to the *last* specified segment.\n\n`~> 1.2.3` is equivalent to `>= 1.2.3, < 1.3.0` — patch updates only.\n\n`~> 1.2` is equivalent to `>= 1.2, < 2.0` — minor updates.\n\nThe idiomatic way to pin \"compatible with\" a specific release."
            }
        }
    }
}

/// Every operator in declaration order — handy for completion dispatch.
pub const ALL_OPERATORS: &[ConstraintOp] = &[
    ConstraintOp::Gte,
    ConstraintOp::Pessimistic,
    ConstraintOp::Eq,
    ConstraintOp::Ne,
    ConstraintOp::Gt,
    ConstraintOp::Lt,
    ConstraintOp::Lte,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    pub op: ConstraintOp,
    /// Raw version token as typed — `1.2.3`, `1.2.3-rc1`, `1.2`.
    pub version: String,
    /// Byte span within the source string (for diagnostic ranges).
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintError {
    pub span: Range<usize>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub constraints: Vec<Constraint>,
    pub errors: Vec<ConstraintError>,
}

/// Tells the completion dispatcher what to offer at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorSlot {
    /// At the start of a (new) constraint — no operator typed yet.
    AtOperator,
    /// Operator typed, waiting for a version.
    AfterOperator(ConstraintOp),
    /// User is typing inside a version token. `partial` is the text
    /// from the end of the operator (or start of the piece) up to
    /// the cursor.
    InsideVersion { op: ConstraintOp, partial: String },
    /// Cursor is after a complete constraint, on whitespace before
    /// a potential comma. Offer a comma-continuation hint.
    Trailing,
}

// -------------------------------------------------------------------------
//  Parsing
// -------------------------------------------------------------------------

/// Parse a constraint string. Partial success: best-effort, recovering
/// past per-piece errors so every constraint is inspected.
pub fn parse(input: &str) -> Parsed {
    let mut constraints = Vec::new();
    let mut errors = Vec::new();
    for piece in split_commas(input) {
        match parse_piece(input, piece.clone()) {
            Ok(c) => constraints.push(c),
            Err(e) => errors.push(e),
        }
    }
    Parsed { constraints, errors }
}

/// Iterator of `Range<usize>` covering each comma-separated piece's
/// span within `input`, excluding the commas themselves. Empty pieces
/// are yielded as zero-length ranges so trailing/leading commas can be
/// flagged.
fn split_commas(input: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in input.bytes().enumerate() {
        if b == b',' {
            out.push(start..i);
            start = i + 1;
        }
    }
    out.push(start..input.len());
    out
}

fn parse_piece(source: &str, span: Range<usize>) -> Result<Constraint, ConstraintError> {
    let raw = &source[span.clone()];
    // Skip leading whitespace.
    let trimmed_start = raw.bytes().take_while(|b| b.is_ascii_whitespace()).count();
    let trimmed_end_rev = raw
        .bytes()
        .rev()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    let content = &raw[trimmed_start..raw.len() - trimmed_end_rev];
    let content_start = span.start + trimmed_start;
    if content.is_empty() {
        return Err(ConstraintError {
            span: span.clone(),
            message: "empty constraint — remove stray comma or fill in a version".to_string(),
        });
    }

    // Longest-match operator scan.
    let (op, op_len) = detect_operator(content);
    let after_op = &content[op_len..];
    let version_start_rel = op_len + after_op.bytes().take_while(|b| b.is_ascii_whitespace()).count();
    let version_text = content[version_start_rel..].trim_end();
    let version_abs_start = content_start + version_start_rel;

    // Catch "unknown operator" cases: leading char is not alphanumeric,
    // not a valid operator char, and not something we recognised.
    if op_len == 0 && starts_with_operator_char(content) {
        return Err(ConstraintError {
            span: content_start..content_start + non_version_prefix_len(content),
            message: format!(
                "unknown operator `{}`",
                &content[..non_version_prefix_len(content)]
            ),
        });
    }

    if version_text.is_empty() {
        return Err(ConstraintError {
            span: content_start..content_start + content.len(),
            message: format!("operator `{}` is missing a version", op.token()),
        });
    }

    if let Err(err_in_version) = validate_version(version_text) {
        return Err(ConstraintError {
            span: version_abs_start..version_abs_start + version_text.len(),
            message: format!("malformed version `{version_text}`: {err_in_version}"),
        });
    }

    Ok(Constraint {
        op,
        version: version_text.to_string(),
        span: content_start..version_abs_start + version_text.len(),
    })
}

/// Returns `(operator, number_of_bytes_consumed)`. When no explicit
/// operator appears we treat the constraint as `=` with zero bytes
/// consumed.
fn detect_operator(s: &str) -> (ConstraintOp, usize) {
    // Order matters — longest tokens first.
    for (tok, op) in &[
        ("~>", ConstraintOp::Pessimistic),
        (">=", ConstraintOp::Gte),
        ("<=", ConstraintOp::Lte),
        ("!=", ConstraintOp::Ne),
        (">", ConstraintOp::Gt),
        ("<", ConstraintOp::Lt),
        ("=", ConstraintOp::Eq),
    ] {
        if s.starts_with(tok) {
            return (*op, tok.len());
        }
    }
    (ConstraintOp::Eq, 0)
}

fn starts_with_operator_char(s: &str) -> bool {
    matches!(s.bytes().next(), Some(b'=' | b'!' | b'<' | b'>' | b'~' | b'^' | b'&' | b'|' | b'+'))
}

fn non_version_prefix_len(s: &str) -> usize {
    s.bytes()
        .take_while(|b| !b.is_ascii_alphanumeric() && !b.is_ascii_whitespace())
        .count()
        .max(1)
}

/// Reject anything that isn't a plausible semver-ish version token.
fn validate_version(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("empty");
    }
    // Split off pre-release (`-foo`) and build metadata (`+foo`) before
    // checking the numeric core.
    let core = s.split('+').next().unwrap_or(s);
    let core = core.split('-').next().unwrap_or(core);
    if core.is_empty() {
        return Err("missing numeric core");
    }
    for part in core.split('.') {
        if part.is_empty() {
            return Err("empty segment");
        }
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err("segment must be numeric");
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------
//  Cursor slot detection (drives completion)
// -------------------------------------------------------------------------

pub fn cursor_slot(input: &str, byte_offset: usize) -> CursorSlot {
    let byte_offset = byte_offset.min(input.len());
    // Find the comma-separated piece containing `byte_offset`.
    let mut piece_start = 0usize;
    for (i, b) in input.bytes().enumerate() {
        if i >= byte_offset {
            break;
        }
        if b == b',' {
            piece_start = i + 1;
        }
    }
    let before_cursor = &input[piece_start..byte_offset];
    // Strip leading whitespace from the piece.
    let leading_ws = before_cursor
        .bytes()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    let trimmed = &before_cursor[leading_ws..];

    if trimmed.is_empty() {
        return CursorSlot::AtOperator;
    }

    let (op, op_len) = detect_operator(trimmed);
    let after_op = &trimmed[op_len..];
    // Catch `>= ` style — operator present, user now typing version.
    let (version_pre_ws, version_text_rel) = split_ws_and_rest(after_op);
    if version_text_rel.is_empty() {
        if op_len == 0 {
            return CursorSlot::AtOperator;
        }
        if version_pre_ws > 0 || op_len > 0 {
            return CursorSlot::AfterOperator(op);
        }
        return CursorSlot::AtOperator;
    }
    // Cursor inside a version token (explicit op or bare).
    CursorSlot::InsideVersion {
        op,
        partial: version_text_rel.to_string(),
    }
}

fn split_ws_and_rest(s: &str) -> (usize, &str) {
    let ws = s.bytes().take_while(|b| b.is_ascii_whitespace()).count();
    (ws, &s[ws..])
}

// -------------------------------------------------------------------------
//  Constraint evaluation
// -------------------------------------------------------------------------

/// Returns `true` iff `candidate` satisfies every constraint in the
/// list (AND semantics). Unparseable candidates never satisfy anything.
pub fn satisfies_all(constraints: &[Constraint], candidate: &str) -> bool {
    let Some(candidate_key) = version_key(candidate) else {
        return false;
    };
    constraints.iter().all(|c| satisfies_one(c, candidate, &candidate_key))
}

fn satisfies_one(c: &Constraint, candidate: &str, candidate_key: &VersionKey) -> bool {
    let Some(constraint_key) = version_key(&c.version) else {
        // Malformed constraint version — reject rather than mismatch.
        return false;
    };
    match c.op {
        ConstraintOp::Eq => candidate == c.version,
        ConstraintOp::Ne => candidate != c.version,
        ConstraintOp::Gt => candidate_key > &constraint_key,
        ConstraintOp::Gte => candidate_key >= &constraint_key,
        ConstraintOp::Lt => candidate_key < &constraint_key,
        ConstraintOp::Lte => candidate_key <= &constraint_key,
        ConstraintOp::Pessimistic => pessimistic_matches(&c.version, candidate_key),
    }
}

/// `~> A.B.C` matches versions with the same `A.B` prefix and
/// `candidate ≥ A.B.C`. `~> A.B` matches the same `A` prefix with
/// `candidate ≥ A.B`. `~> A` is equivalent to `>= A`.
fn pessimistic_matches(constraint_version: &str, candidate_key: &VersionKey) -> bool {
    let ck = match version_key(constraint_version) {
        Some(k) => k,
        None => return false,
    };
    let segments = constraint_version
        .split('-')
        .next()
        .unwrap_or(constraint_version)
        .split('+')
        .next()
        .unwrap_or(constraint_version)
        .split('.')
        .count();
    if candidate_key < &ck {
        return false;
    }
    match segments {
        0 | 1 => true, // `~> A` → no upper bound effectively
        2 => candidate_key.major == ck.major,
        _ => candidate_key.major == ck.major && candidate_key.minor == ck.minor,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct VersionKey {
    major: i64,
    minor: i64,
    patch: i64,
    /// Stable (`1`) sorts after pre-release (`0`) so `1.0.0` > `1.0.0-rc`.
    stability: i32,
    pre_id: String,
}

fn version_key(v: &str) -> Option<VersionKey> {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let core = core.split('+').next().unwrap_or(core);
    let mut parts = core.splitn(3, '.');
    let major: i64 = parts.next()?.parse().ok()?;
    let minor: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let stability = if pre.is_some() { 0 } else { 1 };
    Some(VersionKey {
        major,
        minor,
        patch,
        stability,
        pre_id: pre.unwrap_or("").to_string(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn ops(parsed: &Parsed) -> Vec<ConstraintOp> {
        parsed.constraints.iter().map(|c| c.op).collect()
    }

    #[test]
    fn parses_bare_version_as_eq() {
        let p = parse("1.2.3");
        assert!(p.errors.is_empty());
        assert_eq!(ops(&p), vec![ConstraintOp::Eq]);
        assert_eq!(p.constraints[0].version, "1.2.3");
    }

    #[test]
    fn parses_each_operator() {
        for (tok, op) in &[
            (">= 1.0", ConstraintOp::Gte),
            ("<= 2.0", ConstraintOp::Lte),
            ("> 1", ConstraintOp::Gt),
            ("< 5", ConstraintOp::Lt),
            ("!= 1.2", ConstraintOp::Ne),
            ("= 1.2", ConstraintOp::Eq),
            ("~> 1.2", ConstraintOp::Pessimistic),
        ] {
            let p = parse(tok);
            assert!(p.errors.is_empty(), "parse errors for {tok}: {:?}", p.errors);
            assert_eq!(p.constraints.len(), 1);
            assert_eq!(p.constraints[0].op, *op, "wrong op for {tok}");
        }
    }

    #[test]
    fn parses_multiple_constraints() {
        let p = parse(">= 1.0, < 2.0, != 1.5");
        assert!(p.errors.is_empty(), "got errors: {:?}", p.errors);
        assert_eq!(
            ops(&p),
            vec![ConstraintOp::Gte, ConstraintOp::Lt, ConstraintOp::Ne]
        );
    }

    #[test]
    fn reports_extra_operator_char_as_error() {
        // `>==` greedily parses as `>=` followed by `= 1.0`, which is
        // a malformed version because the stray `=` can't be a
        // version token. Either wording is a legitimate diagnostic
        // — what matters is that the user doesn't silently accept it.
        let p = parse(">== 1.0");
        assert_eq!(p.errors.len(), 1);
        assert!(
            p.errors[0].message.contains("malformed")
                || p.errors[0].message.contains("unknown operator"),
            "got {}",
            p.errors[0].message
        );
    }

    #[test]
    fn reports_leading_garbage_as_unknown_operator() {
        // Pure non-operator punctuation at the start still flags as
        // unknown operator.
        let p = parse("&& 1.0");
        assert_eq!(p.errors.len(), 1);
        assert!(
            p.errors[0].message.contains("unknown operator"),
            "got {}",
            p.errors[0].message
        );
    }

    #[test]
    fn reports_missing_version() {
        let p = parse(">=");
        assert_eq!(p.errors.len(), 1);
        assert!(p.errors[0].message.contains("missing a version"));
    }

    #[test]
    fn reports_malformed_version() {
        let p = parse("1.x");
        assert_eq!(p.errors.len(), 1);
        assert!(p.errors[0].message.contains("malformed"));
    }

    #[test]
    fn reports_trailing_comma() {
        let p = parse(">= 1.0,");
        assert_eq!(p.errors.len(), 1);
        assert!(p.errors[0].message.contains("empty constraint"));
    }

    #[test]
    fn cursor_slot_empty_is_at_operator() {
        assert!(matches!(cursor_slot("", 0), CursorSlot::AtOperator));
    }

    #[test]
    fn cursor_slot_after_comma_is_at_operator() {
        let s = ">= 1.0, ";
        assert!(matches!(cursor_slot(s, s.len()), CursorSlot::AtOperator));
    }

    #[test]
    fn cursor_slot_after_operator_space() {
        let s = ">= ";
        match cursor_slot(s, s.len()) {
            CursorSlot::AfterOperator(ConstraintOp::Gte) => (),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn cursor_slot_inside_version() {
        let s = ">= 1.2";
        match cursor_slot(s, s.len()) {
            CursorSlot::InsideVersion { op: ConstraintOp::Gte, partial } => {
                assert_eq!(partial, "1.2");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn satisfies_exact() {
        let p = parse("1.2.3");
        assert!(satisfies_all(&p.constraints, "1.2.3"));
        assert!(!satisfies_all(&p.constraints, "1.2.4"));
    }

    #[test]
    fn satisfies_inequality() {
        let p = parse("!= 1.2.3");
        assert!(!satisfies_all(&p.constraints, "1.2.3"));
        assert!(satisfies_all(&p.constraints, "1.2.4"));
    }

    #[test]
    fn satisfies_range() {
        let p = parse(">= 1.0.0, < 2.0.0");
        assert!(satisfies_all(&p.constraints, "1.5.0"));
        assert!(!satisfies_all(&p.constraints, "0.9.0"));
        assert!(!satisfies_all(&p.constraints, "2.0.0"));
    }

    #[test]
    fn pessimistic_three_segments_pins_patch_only() {
        let p = parse("~> 1.2.3");
        assert!(satisfies_all(&p.constraints, "1.2.3"));
        assert!(satisfies_all(&p.constraints, "1.2.99"));
        assert!(!satisfies_all(&p.constraints, "1.3.0"));
        assert!(!satisfies_all(&p.constraints, "1.2.2"));
    }

    #[test]
    fn pessimistic_two_segments_pins_minor_only() {
        let p = parse("~> 1.2");
        assert!(satisfies_all(&p.constraints, "1.2.0"));
        assert!(satisfies_all(&p.constraints, "1.99.0"));
        assert!(!satisfies_all(&p.constraints, "2.0.0"));
        assert!(!satisfies_all(&p.constraints, "1.1.9"));
    }

    #[test]
    fn descriptions_are_nonempty() {
        // Regression guard: the one-liner and long-form wording the UI
        // depends on must not silently go missing.
        for op in ALL_OPERATORS {
            assert!(!op.short_description().is_empty());
            assert!(!op.long_description().is_empty());
            assert!(!op.token().is_empty());
        }
        assert!(ConstraintOp::Pessimistic
            .long_description()
            .contains("patch updates"));
    }
}
