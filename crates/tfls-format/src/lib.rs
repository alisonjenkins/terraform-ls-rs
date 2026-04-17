//! HCL formatting.
//!
//! Phase 4 formatter: parses the source via `hcl-edit` to validate
//! syntax, then applies safe text-level normalisations that match a
//! subset of `terraform fmt` behaviour without risking semantic
//! changes:
//!
//! - trims trailing whitespace on each line
//! - collapses runs of blank lines to a single blank line
//! - ensures a single trailing newline at end of file
//! - normalises the indentation of leading whitespace to use spaces

pub mod error;

pub use error::FormatError;

/// Format a Terraform source string.
///
/// Returns the formatted text if parsing succeeds; otherwise propagates
/// the parse error (refusing to touch invalid source).
pub fn format_source(source: &str) -> Result<String, FormatError> {
    // Validate via a real HCL parse so we never rewrite malformed text.
    source
        .parse::<hcl_edit::structure::Body>()
        .map_err(FormatError::Parse)?;

    Ok(apply_normalisations(source))
}

fn apply_normalisations(source: &str) -> String {
    let mut lines: Vec<String> = source
        .split('\n')
        .map(|l| l.trim_end().to_string())
        .map(|l| l.replace('\t', "  "))
        .collect();

    collapse_consecutive_blanks(&mut lines);
    trim_leading_trailing_blanks(&mut lines);

    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn collapse_consecutive_blanks(lines: &mut Vec<String>) {
    let mut i = 0;
    while i + 1 < lines.len() {
        if lines[i].is_empty() && lines[i + 1].is_empty() {
            lines.remove(i + 1);
        } else {
            i += 1;
        }
    }
}

fn trim_leading_trailing_blanks(lines: &mut Vec<String>) {
    while lines.first().map(String::is_empty).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(String::is_empty).unwrap_or(false) {
        lines.pop();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_whitespace() {
        let src = "variable \"x\" {}   \n";
        let got = format_source(src).expect("valid");
        assert_eq!(got, "variable \"x\" {}\n");
    }

    #[test]
    fn collapses_blank_lines() {
        let src = "variable \"a\" {}\n\n\n\nvariable \"b\" {}\n";
        let got = format_source(src).expect("valid");
        assert_eq!(got, "variable \"a\" {}\n\nvariable \"b\" {}\n");
    }

    #[test]
    fn expands_tabs_to_two_spaces() {
        let src = "variable \"x\" {\n\tdefault = 1\n}\n";
        let got = format_source(src).expect("valid");
        assert!(got.contains("  default = 1"));
        assert!(!got.contains('\t'));
    }

    #[test]
    fn ensures_trailing_newline() {
        let src = "variable \"x\" {}";
        let got = format_source(src).expect("valid");
        assert!(got.ends_with('\n'));
    }

    #[test]
    fn strips_leading_and_trailing_blank_lines() {
        let src = "\n\nvariable \"x\" {}\n\n\n";
        let got = format_source(src).expect("valid");
        assert_eq!(got, "variable \"x\" {}\n");
    }

    #[test]
    fn refuses_to_format_broken_source() {
        let src = "variable \"x\" { default =";
        let err = format_source(src);
        assert!(matches!(err, Err(FormatError::Parse(_))));
    }

    #[test]
    fn idempotent_on_clean_input() {
        let src = "variable \"region\" {\n  default = \"us-east-1\"\n}\n";
        let once = format_source(src).expect("valid");
        let twice = format_source(&once).expect("valid");
        assert_eq!(once, twice);
    }
}
