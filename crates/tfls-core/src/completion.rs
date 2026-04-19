//! Cursor-context classification for completion.
//!
//! Rather than mining the hcl-edit AST (which may not parse cleanly
//! mid-keystroke), we inspect the text around the cursor to decide
//! what kind of completion should be offered. This keeps completion
//! responsive even while the user is typing.

/// What the user is likely trying to type at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    /// Cursor is at the top level of the document, likely typing a
    /// block identifier like `resource`, `variable`, `module`, etc.
    TopLevel,

    /// Cursor follows `resource "` — expect a resource type name.
    ResourceType,

    /// Cursor follows `data "` — expect a data source type name.
    DataSourceType,

    /// Cursor is inside a `resource "<type>" "<name>" { ... }` block
    /// body and is likely typing an attribute name.
    ResourceBody { resource_type: String },

    /// Cursor is inside a `data "<type>" "<name>" { ... }` block body.
    DataSourceBody { resource_type: String },

    /// Cursor is inside a `module "<name>" { ... }` block body — expect
    /// one of the child module's input variable names.
    ModuleBody { name: String },

    /// Cursor is after `module.<name>.` — expect an output name from
    /// the referenced child module.
    ModuleAttr { module_name: String },

    /// Cursor is after `var.` — expect a variable name.
    VariableRef,

    /// Cursor is after `local.` — expect a local name.
    LocalRef,

    /// Cursor is after `module.` — expect a module name.
    ModuleRef,

    /// Cursor is after `var.NAME.` (and possibly more `.field` steps)
    /// — expect a field on a variable's object type.
    VariableAttrRef { path: Vec<String> },

    /// Cursor is after `<resource_type>.` — expect a name of a
    /// declared resource of that type.
    ResourceRef { resource_type: String },

    /// Cursor is after `<resource_type>.<name>.` — expect an attribute
    /// of that resource from the provider schema.
    ResourceAttr { resource_type: String, name: String },

    /// Cursor is after `data.<type>.` — expect a data-source name.
    DataSourceRef { resource_type: String },

    /// Cursor is after `data.<type>.<name>.` — expect an attribute of
    /// that data source from the provider schema.
    DataSourceAttr { resource_type: String, name: String },

    /// Cursor is inside a bracket indexer like `<ref>["key1"][|"]` —
    /// expect map keys (or tuple indices) drawn from the static shape
    /// of the root reference, navigated through the collected path.
    IndexKeyRef {
        root: IndexRootRef,
        path: Vec<PathStep>,
    },

    /// Cursor is at an attribute value inside a resource/data block,
    /// and we know which attribute it is. Used for context-aware
    /// reference suggestions (e.g. `security_group_id =` → suggest
    /// `aws_security_group` resources).
    AttributeValue {
        resource_type: String,
        attr_name: String,
    },

    /// Cursor is in an expression context where a function call could
    /// start — offer function names.
    FunctionCall,

    /// Unknown — no specific hints available.
    Unknown,
}

/// Which kind of reference a bracket indexer is rooted at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexRootRef {
    Resource { resource_type: String, name: String },
    DataSource { resource_type: String, name: String },
    Module { module_name: String },
    Variable { name: String },
    Local { name: String },
}

/// One path step inside a traversal — either a `["literal"]` bracket
/// lookup or a `.ident` dot lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathStep {
    Bracket(String),
    Attr(String),
}

/// Classify the context at a given byte offset in the source.
pub fn classify_context(source: &str, byte_offset: usize) -> CompletionContext {
    if byte_offset > source.len() {
        return CompletionContext::Unknown;
    }
    let before = &source[..byte_offset];

    // Bracket-index path wins first — `var.x["key"][` etc.
    if let Some(ctx) = bracket_index_context(before) {
        return ctx;
    }

    // Reference prefixes take priority.
    if let Some(ctx) = reference_prefix_context(before) {
        return ctx;
    }

    // Expression position where a function call could start.
    if let Some(ctx) = expression_context(before) {
        return ctx;
    }

    // `resource "` / `data "` opener on the current logical "statement".
    if let Some(ctx) = block_opener_context(before) {
        return ctx;
    }

    // Inside a resource/data block body?
    if let Some(ctx) = enclosing_block_context(before) {
        return ctx;
    }

    // Top-level if we're at the start of a line after whitespace/newlines.
    if is_top_level(before) {
        return CompletionContext::TopLevel;
    }

    CompletionContext::Unknown
}

fn expression_context(before: &str) -> Option<CompletionContext> {
    // Strip any partial identifier the user is typing.
    let prefix = before.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == ':' || c == '.');
    let trimmed = prefix.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    // Must be inside a block (depth > 0) to be in an expression.
    let mut depth: i32 = 0;
    for c in trimmed.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = (depth - 1).max(0),
            _ => {}
        }
    }
    if depth == 0 {
        return None;
    }

    let is_expr_start = trimmed.ends_with('=')
        || trimmed.ends_with('(')
        || trimmed.ends_with(',')
        || trimmed.ends_with('?')
        || trimmed.ends_with(':')
        || trimmed.ends_with('[')
        || trimmed.ends_with('!')
        || trimmed.ends_with('+')
        || trimmed.ends_with('-')
        || trimmed.ends_with('*')
        || trimmed.ends_with('/')
        || trimmed.ends_with('%')
        || trimmed.ends_with("&&")
        || trimmed.ends_with("||")
        || trimmed.ends_with("${");

    if !is_expr_start {
        return None;
    }

    // If cursor is right after `=`, try to extract the attribute name
    // and enclosing resource type for context-aware value suggestions.
    if trimmed.ends_with('=') {
        if let Some(ctx) = attribute_value_context(trimmed) {
            return Some(ctx);
        }
    }

    Some(CompletionContext::FunctionCall)
}

/// When cursor is after `attr_name =`, extract the attribute name from
/// the current line and the enclosing resource/data type from the block
/// header. Returns `AttributeValue` if both are found.
fn attribute_value_context(before_eq: &str) -> Option<CompletionContext> {
    // Get the text before `=` and extract the attribute name.
    let before = before_eq.strip_suffix('=')?;
    let before = before.trim_end();

    // The attribute name is the last identifier on the line.
    let attr_name: String = before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if attr_name.is_empty() {
        return None;
    }

    // Find the enclosing resource/data block header.
    let resource_type = classify_block_header_from(before)?.1;

    Some(CompletionContext::AttributeValue {
        resource_type,
        attr_name,
    })
}

/// Walk backwards through brace-depth to find the enclosing block
/// header, returning `("resource"|"data", type_name)`.
fn classify_block_header_from(before: &str) -> Option<(String, String)> {
    let mut depth: i32 = 0;
    let bytes = before.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'}' => depth += 1,
            b'{' => {
                if depth == 0 {
                    let header = &before[..i];
                    let line_start = header.rfind('\n').map_or(0, |j| j + 1);
                    let line = header[line_start..].trim();
                    let (keyword, rest) = line.split_once(char::is_whitespace)?;
                    if keyword != "resource" && keyword != "data" {
                        return None;
                    }
                    let type_name = first_quoted_string(rest)?;
                    return Some((keyword.to_string(), type_name));
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn reference_prefix_context(before: &str) -> Option<CompletionContext> {
    // Drop the partial identifier the user is still typing, so we look
    // at segments *before* the cursor identifier.
    let trimmed = before.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
    // Fast-path: short single-segment refs (unchanged historical behaviour).
    if trimmed.ends_with("var.") {
        return Some(CompletionContext::VariableRef);
    }
    if trimmed.ends_with("local.") {
        return Some(CompletionContext::LocalRef);
    }
    if trimmed.ends_with("module.") {
        return Some(CompletionContext::ModuleRef);
    }
    // Multi-segment traversal (TYPE.NAME., data.TYPE.NAME., var.foo.bar.).
    let prefix = trimmed.strip_suffix('.')?;
    // Strip trailing `[...]` bracket groups so e.g.
    // `aws_vpc.eu-west-1["vpc"].` classifies the same as
    // `aws_vpc.eu-west-1.` — the index doesn't change the schema the
    // referenced attribute comes from.
    let prefix = strip_trailing_bracket_groups(prefix);
    let segments = traversal_segments_reverse(prefix);
    let segs: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
    match segs.as_slice() {
        ["var", rest @ ..] if !rest.is_empty() => Some(CompletionContext::VariableAttrRef {
            path: rest.iter().map(|s| (*s).to_string()).collect(),
        }),
        ["module", name] => Some(CompletionContext::ModuleAttr {
            module_name: (*name).to_string(),
        }),
        ["data", t] if !is_builtin_prefix(t) => Some(CompletionContext::DataSourceRef {
            resource_type: (*t).to_string(),
        }),
        ["data", t, n] if !is_builtin_prefix(t) => Some(CompletionContext::DataSourceAttr {
            resource_type: (*t).to_string(),
            name: (*n).to_string(),
        }),
        [t] if !is_builtin_prefix(t) => Some(CompletionContext::ResourceRef {
            resource_type: (*t).to_string(),
        }),
        [t, n] if !is_builtin_prefix(t) => Some(CompletionContext::ResourceAttr {
            resource_type: (*t).to_string(),
            name: (*n).to_string(),
        }),
        _ => None,
    }
}

/// Walk back through any balanced `[...]` groups at the end of `s`,
/// returning the prefix with them removed. Leaves the string alone if
/// it doesn't end in `]` or the brackets aren't balanced.
fn strip_trailing_bracket_groups(mut s: &str) -> &str {
    while let Some(trimmed) = s.strip_suffix(']') {
        let mut depth: i32 = 1;
        let mut cut_at: Option<usize> = None;
        let bytes = trimmed.as_bytes();
        let mut i = bytes.len();
        while i > 0 {
            i -= 1;
            match bytes[i] {
                b']' => depth += 1,
                b'[' => {
                    depth -= 1;
                    if depth == 0 {
                        cut_at = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        match cut_at {
            Some(idx) => s = &trimmed[..idx],
            None => return s,
        }
    }
    s
}

fn is_builtin_prefix(s: &str) -> bool {
    matches!(
        s,
        "var" | "local" | "module" | "data" | "self" | "count" | "each" | "terraform" | "path"
    )
}

/// Classify a cursor sitting inside a bracket indexer. Collects a
/// path from chained `["key"]` bracket lookups and optional `.ident`
/// dot steps, then walks the remaining prefix back to the root ref.
fn bracket_index_context(before: &str) -> Option<CompletionContext> {
    // 1) Strip the partial trailing key inside the *current* (unterminated) bracket.
    //    Accept alphanumeric, underscore, hyphen, and quotes (open or close).
    let stripped = before.trim_end_matches(|c: char| {
        c.is_alphanumeric() || c == '_' || c == '-' || c == '"'
    });
    // 2) The stripped text must end with `[` to indicate we're inside brackets.
    let mut cursor = stripped.strip_suffix('[')?;

    // 3) Collect completed `["literal"]` trailers (plus interspersed `.ident`).
    let mut path: Vec<PathStep> = Vec::new();
    loop {
        // Trim any trailing `.ident` — treat as an Attr step.
        if let Some(rest) = peel_attr_step(cursor) {
            let (stripped, ident) = rest;
            cursor = stripped;
            path.push(PathStep::Attr(ident));
            continue;
        }
        // Trim a full `["literal"]` bracket trailer.
        if let Some(rest) = peel_bracket_step(cursor) {
            let (stripped, key) = rest;
            cursor = stripped;
            path.push(PathStep::Bracket(key));
            continue;
        }
        break;
    }
    path.reverse();

    // 4) What's left must end with the root ref segments (no trailing dot).
    let segments = traversal_segments_reverse(cursor);
    let segs: Vec<&str> = segments.iter().map(|s| s.as_str()).collect();
    let root = match segs.as_slice() {
        ["var", name] => IndexRootRef::Variable {
            name: (*name).to_string(),
        },
        ["local", name] => IndexRootRef::Local {
            name: (*name).to_string(),
        },
        ["module", name] => IndexRootRef::Module {
            module_name: (*name).to_string(),
        },
        ["data", t, n] if !is_builtin_prefix(t) => IndexRootRef::DataSource {
            resource_type: (*t).to_string(),
            name: (*n).to_string(),
        },
        [t, n] if !is_builtin_prefix(t) => IndexRootRef::Resource {
            resource_type: (*t).to_string(),
            name: (*n).to_string(),
        },
        _ => return None,
    };
    Some(CompletionContext::IndexKeyRef { root, path })
}

/// Peel a trailing `.ident` off `s`. Returns the stripped prefix and
/// the ident, or `None` if the text doesn't end in that pattern.
fn peel_attr_step(s: &str) -> Option<(&str, String)> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut end = bytes.len();
    while end > 0 {
        let c = bytes[end - 1] as char;
        if c.is_alphanumeric() || c == '_' || c == '-' {
            end -= 1;
        } else {
            break;
        }
    }
    if end == bytes.len() {
        return None;
    }
    // Must be preceded by `.` and not part of a root-ref traversal we
    // haven't reached yet — i.e. we only peel when the preceding dot's
    // predecessor is `]` (end of a bracket step).
    if end == 0 {
        return None;
    }
    if bytes[end - 1] as char != '.' {
        return None;
    }
    if end < 2 {
        return None;
    }
    if bytes[end - 2] as char != ']' {
        return None;
    }
    let ident = s[end..].to_string();
    Some((&s[..end - 1], ident))
}

/// Peel a trailing `["literal"]` off `s`. Returns the stripped prefix
/// and the literal, or `None`.
fn peel_bracket_step(s: &str) -> Option<(&str, String)> {
    let s = s.strip_suffix(']')?;
    let s = s.strip_suffix('"')?;
    let start = s.rfind("[\"")?;
    let key = &s[start + 2..];
    Some((&s[..start], key.to_string()))
}

/// Walk backwards through `prefix` collecting `ident.ident.ident…`
/// segments (separated by `.`) until we hit a non-ident / non-dot
/// boundary. Returns them in source order.
fn traversal_segments_reverse(prefix: &str) -> Vec<String> {
    let mut segments: Vec<String> = Vec::new();
    let bytes = prefix.as_bytes();
    let mut end = bytes.len();
    loop {
        // Walk backwards over identifier characters — HCL accepts
        // hyphens in idents after the first character, so names like
        // `eu-west-1` parse as one segment.
        let mut start = end;
        while start > 0 {
            let c = bytes[start - 1] as char;
            if c.is_alphanumeric() || c == '_' || c == '-' {
                start -= 1;
            } else {
                break;
            }
        }
        if start == end {
            break;
        }
        segments.push(prefix[start..end].to_string());
        if start == 0 {
            break;
        }
        // The only acceptable separator is `.`.
        let prev = bytes[start - 1] as char;
        if prev == '.' {
            end = start - 1;
        } else {
            break;
        }
    }
    segments.reverse();
    segments
}

fn block_opener_context(before: &str) -> Option<CompletionContext> {
    // Look for `resource "` or `data "` on the current line only.
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = &before[line_start..];
    let trimmed = line.trim_start();
    let is_resource = trimmed.starts_with("resource ");
    let is_data = trimmed.starts_with("data ");
    if !is_resource && !is_data {
        return None;
    }
    match label_index_at_cursor(line) {
        // Cursor is in the *first* label — the type position. This is
        // the only place resource/data-type completions belong.
        Some(0) if is_resource => Some(CompletionContext::ResourceType),
        Some(0) => Some(CompletionContext::DataSourceType),
        // Cursor is in a later label (the name). Short-circuit to
        // Unknown so no resource-type scaffold is offered.
        Some(_) => Some(CompletionContext::Unknown),
        // Cursor is between labels / outside any label — let later
        // classifiers try.
        None => None,
    }
}

/// If the cursor sits inside a quoted label on the current line of a
/// `resource`/`data` block opener, return the 0-based index of that
/// label. Returns `None` if the cursor is between or after labels.
fn label_index_at_cursor(line: &str) -> Option<usize> {
    // Skip past the block keyword (e.g. `resource`) to where labels live.
    let after_keyword = match line.find(' ') {
        Some(i) => &line[i..],
        None => return None,
    };
    let mut in_label = false;
    let mut label_idx = 0usize;
    for c in after_keyword.chars() {
        if c == '"' {
            if in_label {
                // Closing quote — this label has ended.
                label_idx += 1;
            }
            in_label = !in_label;
        }
    }
    if in_label { Some(label_idx) } else { None }
}

fn enclosing_block_context(before: &str) -> Option<CompletionContext> {
    // Walk braces from right-to-left to find the nearest unclosed `{`.
    let mut depth: i32 = 0;
    let bytes = before.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b'}' => depth += 1,
            b'{' => {
                if depth == 0 {
                    // Found the opener for the enclosing block.
                    return classify_block_header(&before[..i]);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// Given the text up to the `{`, figure out if it's a resource/data/
/// module block header and what type or name it declares.
fn classify_block_header(header_source: &str) -> Option<CompletionContext> {
    // Take the last line of the header (block openers live on one line).
    let line_start = header_source.rfind('\n').map_or(0, |i| i + 1);
    let line = header_source[line_start..].trim();
    let (keyword, rest) = line.split_once(char::is_whitespace)?;
    let first_label = first_quoted_string(rest)?;
    match keyword {
        "resource" => Some(CompletionContext::ResourceBody {
            resource_type: first_label,
        }),
        "data" => Some(CompletionContext::DataSourceBody {
            resource_type: first_label,
        }),
        "module" => Some(CompletionContext::ModuleBody { name: first_label }),
        _ => None,
    }
}

fn first_quoted_string(s: &str) -> Option<String> {
    let first = s.find('"')?;
    let rest = &s[first + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn is_top_level(before: &str) -> bool {
    // Top-level means we're not inside any open block.
    let mut depth: i32 = 0;
    for c in before.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = (depth - 1).max(0),
            _ => {}
        }
    }
    depth == 0
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn at_end(src: &str) -> CompletionContext {
        classify_context(src, src.len())
    }

    #[test]
    fn top_level_at_start_of_empty_doc() {
        assert_eq!(at_end(""), CompletionContext::TopLevel);
    }

    #[test]
    fn top_level_between_blocks() {
        let src = "variable \"x\" {}\n";
        assert_eq!(at_end(src), CompletionContext::TopLevel);
    }

    #[test]
    fn resource_type_after_resource_quote() {
        assert_eq!(at_end("resource \""), CompletionContext::ResourceType);
    }

    #[test]
    fn data_source_type_after_data_quote() {
        assert_eq!(at_end("data \""), CompletionContext::DataSourceType);
    }

    #[test]
    fn variable_ref_after_var_dot() {
        assert_eq!(
            at_end("output \"x\" { value = var."),
            CompletionContext::VariableRef
        );
    }

    #[test]
    fn local_ref_after_local_dot() {
        assert_eq!(
            at_end("output \"x\" { value = local."),
            CompletionContext::LocalRef
        );
    }

    #[test]
    fn module_ref_after_module_dot() {
        assert_eq!(
            at_end("output \"x\" { value = module."),
            CompletionContext::ModuleRef
        );
    }

    #[test]
    fn resource_body_reports_type() {
        let src = "resource \"aws_instance\" \"web\" {\n  ";
        let got = at_end(src);
        match got {
            CompletionContext::ResourceBody { resource_type } => {
                assert_eq!(resource_type, "aws_instance");
            }
            other => panic!("expected ResourceBody, got {other:?}"),
        }
    }

    #[test]
    fn data_source_body_reports_type() {
        let src = "data \"aws_ami\" \"ubuntu\" {\n  ";
        let got = at_end(src);
        match got {
            CompletionContext::DataSourceBody { resource_type } => {
                assert_eq!(resource_type, "aws_ami");
            }
            other => panic!("expected DataSourceBody, got {other:?}"),
        }
    }

    #[test]
    fn partial_variable_ref_is_still_variable_ref() {
        assert_eq!(
            at_end("output \"x\" { value = var.reg"),
            CompletionContext::VariableRef
        );
    }

    #[test]
    fn attribute_value_after_equals_in_resource() {
        let src = "resource \"aws_instance\" \"web\" {\n  subnet_id = ";
        match at_end(src) {
            CompletionContext::AttributeValue {
                resource_type,
                attr_name,
            } => {
                assert_eq!(resource_type, "aws_instance");
                assert_eq!(attr_name, "subnet_id");
            }
            other => panic!("expected AttributeValue, got {other:?}"),
        }
    }

    #[test]
    fn attribute_value_with_partial_typing() {
        let src = "resource \"aws_instance\" \"web\" {\n  subnet_id = aws";
        match at_end(src) {
            CompletionContext::AttributeValue {
                resource_type,
                attr_name,
            } => {
                assert_eq!(resource_type, "aws_instance");
                assert_eq!(attr_name, "subnet_id");
            }
            other => panic!("expected AttributeValue, got {other:?}"),
        }
    }

    #[test]
    fn function_call_after_open_paren() {
        let src = "resource \"x\" \"y\" {\n  value = foo(";
        assert_eq!(at_end(src), CompletionContext::FunctionCall);
    }

    #[test]
    fn function_call_after_comma() {
        let src = "resource \"x\" \"y\" {\n  value = foo(a, ";
        assert_eq!(at_end(src), CompletionContext::FunctionCall);
    }

    #[test]
    fn function_call_in_interpolation() {
        let src = "resource \"x\" \"y\" {\n  value = \"${";
        assert_eq!(at_end(src), CompletionContext::FunctionCall);
    }

    #[test]
    fn function_call_partial_name_in_subexpression() {
        let src = "resource \"x\" \"y\" {\n  value = foo(for";
        assert_eq!(at_end(src), CompletionContext::FunctionCall);
    }

    #[test]
    fn out_of_bounds_offset_is_unknown() {
        assert_eq!(
            classify_context("short", 9999),
            CompletionContext::Unknown
        );
    }

    // Regression: when the cursor is inside the *second* label of a
    // `resource "TYPE" "NAME"` header (i.e. editing the name), the
    // classifier must not report `ResourceType`. Reporting `ResourceType`
    // causes the handler to emit full-scaffold snippets that splice into
    // the already-open resource block and produce malformed code.
    #[test]
    fn resource_type_partial_prefix_is_still_resource_type() {
        assert_eq!(at_end("resource \"aws_"), CompletionContext::ResourceType);
    }

    #[test]
    fn cursor_between_labels_is_not_resource_type() {
        // Cursor sits after the first label's closing quote but before the
        // second label has been opened. Quote count is even; classifier
        // should fall through.
        let ctx = at_end("resource \"aws_instance\" ");
        assert_ne!(ctx, CompletionContext::ResourceType);
    }

    #[test]
    fn cursor_in_empty_name_label_is_not_resource_type() {
        // Cursor is right after the opening quote of the *name* label.
        // Old heuristic wrongly classified this as ResourceType because
        // the raw quote count is odd.
        let ctx = at_end("resource \"aws_instance\" \"");
        assert_ne!(ctx, CompletionContext::ResourceType);
    }

    #[test]
    fn cursor_inside_name_label_is_not_resource_type() {
        // Exact screenshot repro: cursor in the second label while typing
        // a partial name.
        let ctx = at_end("resource \"aws_security_group\" \"test");
        assert_ne!(
            ctx,
            CompletionContext::ResourceType,
            "cursor in resource name label must not trigger ResourceType context"
        );
    }

    #[test]
    fn cursor_inside_data_source_name_label_is_not_data_source_type() {
        let ctx = at_end("data \"aws_ami\" \"x");
        assert_ne!(ctx, CompletionContext::DataSourceType);
    }

    // --- Reference-prefix classifier regressions ---------------------

    #[test]
    fn resource_ref_after_type_dot() {
        let ctx = at_end("output \"x\" { value = aws_iam_role.");
        assert_eq!(
            ctx,
            CompletionContext::ResourceRef {
                resource_type: "aws_iam_role".to_string()
            }
        );
    }

    #[test]
    fn resource_attr_after_type_name_dot() {
        let ctx = at_end("output \"x\" { value = aws_iam_role.role1.");
        assert_eq!(
            ctx,
            CompletionContext::ResourceAttr {
                resource_type: "aws_iam_role".to_string(),
                name: "role1".to_string()
            }
        );
    }

    #[test]
    fn data_source_ref_after_data_type_dot() {
        let ctx = at_end("output \"x\" { value = data.aws_ami.");
        assert_eq!(
            ctx,
            CompletionContext::DataSourceRef {
                resource_type: "aws_ami".to_string()
            }
        );
    }

    #[test]
    fn data_source_attr_after_data_type_name_dot() {
        let ctx = at_end("output \"x\" { value = data.aws_ami.ubuntu.");
        assert_eq!(
            ctx,
            CompletionContext::DataSourceAttr {
                resource_type: "aws_ami".to_string(),
                name: "ubuntu".to_string()
            }
        );
    }

    #[test]
    fn variable_attr_ref_single_field() {
        let ctx = at_end("output \"x\" { value = var.foo.");
        assert_eq!(
            ctx,
            CompletionContext::VariableAttrRef {
                path: vec!["foo".to_string()]
            }
        );
    }

    // --- Bracket-index classifier regressions ------------------------

    #[test]
    fn resource_index_key_after_bare_bracket() {
        let src = "output \"x\" { value = aws_vpc.eu-west-1[";
        match at_end(src) {
            CompletionContext::IndexKeyRef {
                root:
                    IndexRootRef::Resource {
                        resource_type,
                        name,
                    },
                path,
            } => {
                assert_eq!(resource_type, "aws_vpc");
                assert_eq!(name, "eu-west-1");
                assert!(path.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn variable_index_key_after_var_dot_name_bracket() {
        let ctx = at_end("output \"x\" { value = var.regions[");
        match ctx {
            CompletionContext::IndexKeyRef {
                root: IndexRootRef::Variable { name },
                path,
            } => {
                assert_eq!(name, "regions");
                assert!(path.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn local_index_key_after_local_dot_name_bracket() {
        let ctx = at_end("output \"x\" { value = local.cfg[");
        match ctx {
            CompletionContext::IndexKeyRef {
                root: IndexRootRef::Local { name },
                path,
            } => {
                assert_eq!(name, "cfg");
                assert!(path.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn module_index_key_after_module_dot_name_bracket() {
        let ctx = at_end("output \"x\" { value = module.web[");
        match ctx {
            CompletionContext::IndexKeyRef {
                root: IndexRootRef::Module { module_name },
                ..
            } => assert_eq!(module_name, "web"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn data_source_index_key_after_data_triple_bracket() {
        let ctx = at_end("output \"x\" { value = data.aws_ami.lookup[");
        match ctx {
            CompletionContext::IndexKeyRef {
                root:
                    IndexRootRef::DataSource {
                        resource_type,
                        name,
                    },
                ..
            } => {
                assert_eq!(resource_type, "aws_ami");
                assert_eq!(name, "lookup");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn index_key_with_open_quote_collects_no_partial() {
        let ctx = at_end("output \"x\" { value = var.regions[\"");
        match ctx {
            CompletionContext::IndexKeyRef {
                root: IndexRootRef::Variable { name },
                path,
            } => {
                assert_eq!(name, "regions");
                assert!(path.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn deep_bracket_path_collected() {
        let ctx = at_end(
            "output \"x\" { value = var.regions[\"eu-west-1\"][\"subnet_cidrs\"][",
        );
        match ctx {
            CompletionContext::IndexKeyRef {
                root: IndexRootRef::Variable { name },
                path,
            } => {
                assert_eq!(name, "regions");
                assert_eq!(
                    path,
                    vec![
                        PathStep::Bracket("eu-west-1".to_string()),
                        PathStep::Bracket("subnet_cidrs".to_string()),
                    ]
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn resource_attr_after_bracket_index_and_dot() {
        let ctx = at_end("output \"x\" { value = aws_vpc.eu-west-1[\"vpc\"].");
        assert_eq!(
            ctx,
            CompletionContext::ResourceAttr {
                resource_type: "aws_vpc".to_string(),
                name: "eu-west-1".to_string(),
            }
        );
    }

    #[test]
    fn resource_attr_after_bracket_index_and_partial_attr() {
        let ctx = at_end("output \"x\" { value = aws_vpc.eu-west-1[\"vpc\"].id");
        assert_eq!(
            ctx,
            CompletionContext::ResourceAttr {
                resource_type: "aws_vpc".to_string(),
                name: "eu-west-1".to_string(),
            }
        );
    }

    #[test]
    fn data_attr_after_bracket_index_and_dot() {
        let ctx = at_end("output \"x\" { value = data.aws_ami.web[\"k\"].");
        assert_eq!(
            ctx,
            CompletionContext::DataSourceAttr {
                resource_type: "aws_ami".to_string(),
                name: "web".to_string(),
            }
        );
    }

    #[test]
    fn bracket_context_does_not_steal_from_plain_dot() {
        // `var.regions.` should still route through VariableAttrRef, not
        // the bracket classifier.
        let ctx = at_end("output \"x\" { value = var.regions.");
        assert!(matches!(
            ctx,
            CompletionContext::VariableAttrRef { .. }
        ));
    }

    #[test]
    fn module_body_after_block_opener() {
        let src = "module \"web\" {\n  ";
        match at_end(src) {
            CompletionContext::ModuleBody { name } => assert_eq!(name, "web"),
            other => panic!("expected ModuleBody, got {other:?}"),
        }
    }

    #[test]
    fn module_attr_after_module_dot_name() {
        assert_eq!(
            at_end("output \"x\" { value = module.web."),
            CompletionContext::ModuleAttr {
                module_name: "web".to_string()
            }
        );
    }

    #[test]
    fn module_attr_drill_beyond_name_is_not_reclassified() {
        // `module.web.foo.` — segments ["module", "web", "foo"]
        // falls through (no `[module, name, extra]` arm).
        let ctx = at_end("output \"x\" { value = module.web.foo.");
        match ctx {
            CompletionContext::ModuleAttr { .. } | CompletionContext::ResourceAttr { .. } => {
                panic!("unexpected classification: {ctx:?}")
            }
            _ => {}
        }
    }

    #[test]
    fn variable_attr_ref_nested_field() {
        let ctx = at_end("output \"x\" { value = var.foo.bar.");
        assert_eq!(
            ctx,
            CompletionContext::VariableAttrRef {
                path: vec!["foo".to_string(), "bar".to_string()]
            }
        );
    }

    #[test]
    fn resource_body_still_works_after_complete_header() {
        // Full header on one line with cursor inside the body — must
        // resolve to ResourceBody via the brace-walking classifier,
        // *not* be shadowed by block_opener_context.
        let src = "resource \"aws_instance\" \"web\" {\n  ";
        match at_end(src) {
            CompletionContext::ResourceBody { resource_type } => {
                assert_eq!(resource_type, "aws_instance");
            }
            other => panic!("expected ResourceBody, got {other:?}"),
        }
    }
}
