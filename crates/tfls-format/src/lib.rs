//! HCL formatting — thin wrapper around the [`tf_format`] crate.
//!
//! tfls always invokes the backend with `FormatStyle::Minimal`,
//! the `terraform fmt` / `tofu fmt` parity mode. Source-order is
//! preserved; only spacing + `=` alignment changes are applied.
//! The opinionated reorder/hoist/expand transforms tf-format
//! offers under its default style would reshape repos that
//! haven't opted in to that style — undesirable for a language
//! server's format-on-save flow, where the user's expectation is
//! "match `terraform fmt`".
//!
//! If the backend ever needs to be swapped (e.g. for a custom
//! style or to layer additional passes), this is the single
//! place to do it — `crates/tfls-lsp/src/handlers/formatting.rs`
//! depends only on `format_source`'s signature.

pub mod error;

pub use error::FormatError;

/// Format an HCL source string using `terraform fmt`-style rules.
///
/// Returns the formatted text; propagates any error from the
/// underlying [`tf_format`] formatter (typically a parse error).
pub fn format_source(source: &str) -> Result<String, FormatError> {
    let opts = tf_format::FormatOptions::minimal();
    Ok(tf_format::format_hcl_with(source, &opts)?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn aligns_equals_signs_within_a_block() {
        // Spacing transform — exactly what `tofu fmt` would do.
        let src = "resource \"x\" \"y\" {\n  ami = \"a\"\n  instance_type = \"t\"\n}\n";
        let out = format_source(src).expect("formats");
        assert!(
            out.contains("ami           = \"a\""),
            "expected aligned `ami`, got:\n{out}"
        );
        assert!(
            out.contains("instance_type = \"t\""),
            "expected aligned `instance_type`, got:\n{out}"
        );
    }

    #[test]
    fn preserves_resource_block_order() {
        // Opinionated mode would alphabetise these; minimal mode
        // must NOT.
        let src = concat!(
            "resource \"x\" \"z\" { ami = \"z\" }\n",
            "resource \"x\" \"a\" { ami = \"a\" }\n",
        );
        let out = format_source(src).expect("formats");
        let z_pos = out
            .find("resource \"x\" \"z\"")
            .expect("z block present");
        let a_pos = out
            .find("resource \"x\" \"a\"")
            .expect("a block present");
        assert!(
            z_pos < a_pos,
            "minimal mode must keep z before a; got:\n{out}"
        );
    }

    #[test]
    fn does_not_hoist_meta_arguments() {
        // `count` written AFTER `ami`. Opinionated mode would
        // promote `count` to the top of the block; minimal mode
        // must leave it where the author put it.
        let src = "resource \"x\" \"y\" {\n  ami = \"a\"\n  count = 1\n}\n";
        let out = format_source(src).expect("formats");
        let ami_pos = out.find("ami").expect("ami present");
        let count_pos = out.find("count").expect("count present");
        assert!(
            ami_pos < count_pos,
            "minimal mode must not hoist count; got:\n{out}"
        );
    }

    #[test]
    fn idempotent_on_clean_input() {
        let src = "resource \"x\" \"y\" {\n  ami           = \"a\"\n  instance_type = \"t\"\n}\n";
        let once = format_source(src).expect("formats");
        let twice = format_source(&once).expect("formats");
        assert_eq!(once, twice, "format must be idempotent");
    }

    #[test]
    fn refuses_to_format_broken_source() {
        let src = "resource \"x\" {\n";
        let err = format_source(src).expect_err("must reject broken source");
        // Surface comes from tf-format; we only assert that a
        // failure path exists.
        let _ = err;
    }
}
