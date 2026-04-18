//! Terraform meta-arguments — language-level constructs valid in every
//! `resource` and `data` block regardless of provider schema.
//!
//! See: <https://developer.hashicorp.com/terraform/language/meta-arguments>

/// Which kind of top-level block we're describing. `data` blocks accept a
/// slightly narrower set of meta-constructs than `resource` blocks
/// (notably no `provisioner`/`connection` and a much smaller `lifecycle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Resource,
    Data,
}

/// Attribute-style meta-arguments. Accepted in both `resource` and `data`
/// blocks. These never appear in provider schemas.
pub const META_ATTRS: &[&str] = &["count", "for_each", "provider", "depends_on"];

/// Quick membership check for [`META_ATTRS`].
pub fn is_meta_attr(name: &str) -> bool {
    META_ATTRS.contains(&name)
}

/// Block-style meta-blocks accepted in a given block kind. `data` blocks
/// don't get `provisioner`/`connection`.
pub fn meta_blocks(kind: BlockKind) -> &'static [&'static str] {
    match kind {
        BlockKind::Resource => &["lifecycle", "provisioner", "connection"],
        BlockKind::Data => &["lifecycle"],
    }
}

/// Attributes allowed directly inside a `lifecycle { ... }` block. Data
/// sources have no meta-attrs here — only the `postcondition` sub-block
/// (Terraform 1.2+).
pub fn lifecycle_attrs(kind: BlockKind) -> &'static [&'static str] {
    match kind {
        BlockKind::Resource => &[
            "create_before_destroy",
            "prevent_destroy",
            "ignore_changes",
            "replace_triggered_by",
        ],
        BlockKind::Data => &[],
    }
}

/// Sub-blocks allowed inside a `lifecycle { ... }` block.
pub fn lifecycle_blocks(kind: BlockKind) -> &'static [&'static str] {
    match kind {
        BlockKind::Resource => &["precondition", "postcondition"],
        BlockKind::Data => &["postcondition"],
    }
}

/// Attributes allowed inside `precondition`/`postcondition` blocks.
pub const CONDITION_ATTRS: &[&str] = &["condition", "error_message"];
