//! HCL parsing wrapper around `hcl-edit`.

use hcl_edit::structure::Body;

use crate::error::ParseError;

/// Result of parsing a single `.tf` file.
#[derive(Debug)]
pub struct ParsedFile {
    /// The parsed body — present even on partial failure if recoverable.
    pub body: Option<Body>,
    /// Parse errors, if any.
    pub errors: Vec<ParseError>,
}

impl ParsedFile {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Parse an HCL source string.
///
/// On success, returns the parsed body with no errors. On failure, the body
/// may be absent and `errors` will contain one or more diagnostics.
pub fn parse_source(source: &str) -> ParsedFile {
    match source.parse::<Body>() {
        Ok(body) => ParsedFile {
            body: Some(body),
            errors: Vec::new(),
        },
        Err(e) => {
            let message = e.to_string();
            ParsedFile {
                body: None,
                errors: vec![ParseError::Syntax {
                    message,
                    source: e,
                }],
            }
        }
    }
}

/// Parse a document, auto-selecting the HCL or JSON parser based on
/// the URI extension. `.tf.json` files go through the JSON parser;
/// everything else uses the HCL parser.
pub fn parse_source_for_uri(source: &str, uri_or_path: &str) -> ParsedFile {
    if uri_or_path.ends_with(".tf.json") {
        crate::json::parse_json_source(source)
    } else {
        parse_source(source)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_source() {
        let parsed = parse_source("");
        assert!(parsed.body.is_some(), "empty source should parse to empty body");
        assert!(!parsed.has_errors());
    }

    #[test]
    fn parses_simple_resource() {
        let src = r#"
resource "aws_instance" "web" {
  ami           = "ami-123"
  instance_type = "t3.micro"
}
"#;
        let parsed = parse_source(src);
        assert!(parsed.body.is_some());
        assert!(!parsed.has_errors(), "valid HCL should parse cleanly");
    }

    #[test]
    fn parses_variable_block() {
        let src = r#"
variable "region" {
  type    = string
  default = "us-east-1"
}
"#;
        let parsed = parse_source(src);
        assert!(parsed.body.is_some());
        assert!(!parsed.has_errors());
    }

    #[test]
    fn reports_syntax_error() {
        let src = r#"
resource "aws_instance" "web" {
  ami = "unterminated
"#;
        let parsed = parse_source(src);
        assert!(parsed.has_errors(), "invalid HCL should produce errors");
    }

    #[test]
    fn syntax_error_includes_source() {
        let parsed = parse_source("resource {");
        assert!(parsed.has_errors());
        let err = &parsed.errors[0];
        match err {
            ParseError::Syntax { message, source: _ } => {
                assert!(!message.is_empty(), "error message should not be empty");
            }
            other => panic!("expected Syntax error, got {other:?}"),
        }
    }
}
