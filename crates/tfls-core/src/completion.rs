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

    /// Cursor is after `var.` — expect a variable name.
    VariableRef,

    /// Cursor is after `local.` — expect a local name.
    LocalRef,

    /// Cursor is after `module.` — expect a module name.
    ModuleRef,

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
    // Find the identifier segment immediately before the cursor.
    let trimmed = before.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
    if trimmed.ends_with("var.") {
        Some(CompletionContext::VariableRef)
    } else if trimmed.ends_with("local.") {
        Some(CompletionContext::LocalRef)
    } else if trimmed.ends_with("module.") {
        Some(CompletionContext::ModuleRef)
    } else {
        None
    }
}

fn block_opener_context(before: &str) -> Option<CompletionContext> {
    // Look for `resource "` or `data "` on the current line only.
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = &before[line_start..];
    if line.trim_start().starts_with("resource ") && ends_inside_first_label(line) {
        Some(CompletionContext::ResourceType)
    } else if line.trim_start().starts_with("data ") && ends_inside_first_label(line) {
        Some(CompletionContext::DataSourceType)
    } else {
        None
    }
}

fn ends_inside_first_label(line: &str) -> bool {
    // A simple heuristic: if there's exactly one `"` after the block
    // keyword, we're inside the first label.
    let after_keyword = match line.find(' ') {
        Some(i) => &line[i..],
        None => return false,
    };
    let quote_count = after_keyword.chars().filter(|&c| c == '"').count();
    quote_count % 2 == 1
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

/// Given the text up to the `{`, figure out if it's a resource/data
/// block header and what type it declares.
fn classify_block_header(header_source: &str) -> Option<CompletionContext> {
    // Take the last line of the header (block openers live on one line).
    let line_start = header_source.rfind('\n').map_or(0, |i| i + 1);
    let line = header_source[line_start..].trim();
    let (keyword, rest) = line.split_once(char::is_whitespace)?;
    let resource_type = first_quoted_string(rest)?;
    match keyword {
        "resource" => Some(CompletionContext::ResourceBody { resource_type }),
        "data" => Some(CompletionContext::DataSourceBody { resource_type }),
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
}
