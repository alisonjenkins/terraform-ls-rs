//! HCL formatting — thin wrapper around the [`tf_format`] crate.
//!
//! Style is selectable at runtime via the LSP config key
//! `formatStyle` (see `tfls_state::Config::format_style`).
//! Default is [`tfls_state::FormatStyle::Minimal`] —
//! `terraform fmt` / `tofu fmt` parity. Switch to
//! `"opinionated"` via `workspace/didChangeConfiguration` for
//! tf-format's full alphabetise / hoist / expand behaviour.
//!
//! `crates/tfls-lsp/src/handlers/formatting.rs` is the only
//! caller; it pulls the live `FormatStyle` from
//! `state.config.snapshot()` and passes it in.

pub mod error;

pub use error::FormatError;

use tfls_state::FormatStyle;

/// Format an HCL source string using the given style.
///
/// Returns the formatted text; propagates any error from the
/// underlying [`tf_format`] formatter (typically a parse error).
pub fn format_source(source: &str, style: FormatStyle) -> Result<String, FormatError> {
    let opts = tf_format::FormatOptions {
        style: map_style(style),
    };
    Ok(tf_format::format_hcl_with(source, &opts)?)
}

fn map_style(style: FormatStyle) -> tf_format::FormatStyle {
    match style {
        FormatStyle::Minimal => tf_format::FormatStyle::Minimal,
        FormatStyle::Opinionated => tf_format::FormatStyle::Opinionated,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn minimal_aligns_equals_signs_within_a_block() {
        let src = "resource \"x\" \"y\" {\n  ami = \"a\"\n  instance_type = \"t\"\n}\n";
        let out = format_source(src, FormatStyle::Minimal).expect("formats");
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
    fn minimal_preserves_resource_block_order() {
        let src = concat!(
            "resource \"x\" \"z\" { ami = \"z\" }\n",
            "resource \"x\" \"a\" { ami = \"a\" }\n",
        );
        let out = format_source(src, FormatStyle::Minimal).expect("formats");
        let z_pos = out.find("resource \"x\" \"z\"").expect("z block present");
        let a_pos = out.find("resource \"x\" \"a\"").expect("a block present");
        assert!(
            z_pos < a_pos,
            "minimal mode must keep z before a; got:\n{out}"
        );
    }

    #[test]
    fn minimal_does_not_hoist_meta_arguments() {
        let src = "resource \"x\" \"y\" {\n  ami = \"a\"\n  count = 1\n}\n";
        let out = format_source(src, FormatStyle::Minimal).expect("formats");
        let ami_pos = out.find("ami").expect("ami present");
        let count_pos = out.find("count").expect("count present");
        assert!(
            ami_pos < count_pos,
            "minimal mode must not hoist count; got:\n{out}"
        );
    }

    #[test]
    fn opinionated_does_reorder_resources() {
        // Pin the runtime-toggle path: with FormatStyle::Opinionated
        // we expect tf-format's full reorder behaviour.
        let src = concat!(
            "resource \"x\" \"z\" { ami = \"z\" }\n",
            "resource \"x\" \"a\" { ami = \"a\" }\n",
        );
        let out = format_source(src, FormatStyle::Opinionated).expect("formats");
        let z_pos = out.find("resource \"x\" \"z\"").expect("z block present");
        let a_pos = out.find("resource \"x\" \"a\"").expect("a block present");
        assert!(
            a_pos < z_pos,
            "opinionated mode must alphabetise (a before z); got:\n{out}"
        );
    }

    #[test]
    fn opinionated_hoists_meta_arguments() {
        let src = "resource \"x\" \"y\" {\n  ami = \"a\"\n  count = 1\n}\n";
        let out = format_source(src, FormatStyle::Opinionated).expect("formats");
        let ami_pos = out.find("ami").expect("ami present");
        let count_pos = out.find("count").expect("count present");
        assert!(
            count_pos < ami_pos,
            "opinionated mode must hoist count; got:\n{out}"
        );
    }

    #[test]
    fn idempotent_on_clean_input_minimal() {
        let src = "resource \"x\" \"y\" {\n  ami           = \"a\"\n  instance_type = \"t\"\n}\n";
        let once = format_source(src, FormatStyle::Minimal).expect("formats");
        let twice = format_source(&once, FormatStyle::Minimal).expect("formats");
        assert_eq!(once, twice, "format must be idempotent");
    }

    #[test]
    fn refuses_to_format_broken_source() {
        let src = "resource \"x\" {\n";
        let err =
            format_source(src, FormatStyle::Minimal).expect_err("must reject broken source");
        let _ = err;
    }
}
