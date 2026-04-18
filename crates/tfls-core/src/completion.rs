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

/// Classify the context at a given byte offset in the source.
pub fn classify_context(source: &str, byte_offset: usize) -> CompletionContext {
    if byte_offset > source.len() {
        return CompletionContext::Unknown;
    }
    let before = &source[..byte_offset];

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

fn is_builtin_prefix(s: &str) -> bool {
    matches!(
        s,
        "var" | "local" | "module" | "data" | "self" | "count" | "each" | "terraform" | "path"
    )
}

/// Walk backwards through `prefix` collecting `ident.ident.ident…`
/// segments (separated by `.`) until we hit a non-ident / non-dot
/// boundary. Returns them in source order.
fn traversal_segments_reverse(prefix: &str) -> Vec<String> {
    let mut segments: Vec<String> = Vec::new();
    let bytes = prefix.as_bytes();
    let mut end = bytes.len();
    loop {
        // Walk backwards over identifier characters.
        let mut start = end;
        while start > 0 {
            let c = bytes[start - 1] as char;
            if c.is_alphanumeric() || c == '_' {
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
