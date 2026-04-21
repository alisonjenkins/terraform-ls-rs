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
///
/// `enabled` is OpenTofu-only (v1.11+). It's accepted here so the
/// schema-validation pass doesn't flag it as "unknown attribute";
/// whether its use is *correct* in the current file's language
/// dialect is checked separately against the filename extension (see
/// [`tfls_diag::schema_validation`]) — `.tofu` / `.tofu.json` files
/// get it silently; `.tf` / `.tf.json` files get a warning pointing
/// out that Terraform doesn't support it.
pub fn lifecycle_attrs(kind: BlockKind) -> &'static [&'static str] {
    match kind {
        BlockKind::Resource => &[
            "create_before_destroy",
            "prevent_destroy",
            "ignore_changes",
            "replace_triggered_by",
            "enabled",
        ],
        BlockKind::Data => &["enabled"],
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

/// Whether a meta-block name is restricted to one occurrence per block.
/// `provisioner` is labelled and repeatable; everything else is single.
pub fn is_singleton_meta_block(kind: BlockKind, name: &str) -> bool {
    match kind {
        BlockKind::Resource => matches!(name, "lifecycle" | "connection"),
        BlockKind::Data => name == "lifecycle",
    }
}

/// Canonical one-paragraph description for an attribute-style
/// meta-argument (`count`, `for_each`, `provider`, `depends_on`).
/// Lifted from the Terraform / OpenTofu language reference. Used to
/// populate both hover docs and completion `documentation` popups.
/// Returns an empty string for names that aren't meta-args.
pub fn meta_attr_description(name: &str) -> &'static str {
    match name {
        "count" => "**count** — Creates that many instances of this resource.\n\n\
            Accepts a whole number; each instance is addressable as \
            `aws_foo.x[0]`, `aws_foo.x[1]`, and so on. Cannot be combined \
            with `for_each`. Use `count.index` inside the block body to \
            differentiate instances.",
        "for_each" => "**for_each** — Creates one instance of this resource per key.\n\n\
            Accepts a `map` or `set(string)`. Each instance is addressable \
            as `aws_foo.x[\"key\"]`. Use `each.key` / `each.value` inside \
            the body. Cannot be combined with `count`; prefer `for_each` \
            when the set of instances is keyed semantically (names, IDs) \
            rather than by position.",
        "provider" => "**provider** — Selects a non-default provider configuration.\n\n\
            Value is a reference like `aws.us-east-1`. Use when the module \
            declares multiple configured instances of the same provider \
            (aliases) and this resource must target one specifically.",
        "depends_on" => "**depends_on** — Explicit dependencies.\n\n\
            Accepts a list of resource/module references. Terraform normally \
            derives dependencies from expression references; use this \
            meta-argument only when a dependency exists that isn't visible \
            through expressions (e.g. IAM policies that must be applied \
            before the resource that uses them).",
        _ => "",
    }
}

/// Canonical description for a meta-block name (`lifecycle`,
/// `provisioner`, `connection`). Same semantics as
/// [`meta_attr_description`].
pub fn meta_block_description(name: &str) -> &'static str {
    match name {
        "lifecycle" => "**lifecycle** — Customise how Terraform manages the resource lifecycle.\n\n\
            Attributes: `create_before_destroy`, `prevent_destroy`, \
            `ignore_changes`, `replace_triggered_by`. Nested blocks: \
            `precondition`, `postcondition` (custom validation).",
        "provisioner" => "**provisioner** — Run a command on resource creation or destruction.\n\n\
            Takes a label naming the provisioner type (`local-exec`, \
            `remote-exec`, `file`). Use sparingly — provisioners are a \
            last resort; prefer a cloud-init, user-data, or configuration-\
            management system whenever possible. Documented as \
            \"last-resort\" by HashiCorp.",
        "connection" => "**connection** — Authentication for remote-exec / file provisioners.\n\n\
            Declares how Terraform should SSH / WinRM into the instance \
            to run `remote-exec` or copy files. Attributes include `type`, \
            `host`, `user`, `private_key`, `port`.",
        _ => "",
    }
}

/// Canonical description for an attribute inside a `lifecycle { ... }`
/// block. `kind` is carried so we can later differentiate descriptions
/// between resource/data if they diverge — today they only overlap on
/// `enabled` (OpenTofu) so it's unused.
pub fn lifecycle_attr_description(_kind: BlockKind, name: &str) -> &'static str {
    match name {
        "create_before_destroy" => "**create_before_destroy** — Create the replacement before destroying the original.\n\n\
            `bool`. Default `false`. Enable for resources whose identity is \
            tied to an externally-referenced ID (DNS records, load balancer \
            targets) so the replacement is live before the old one goes away.",
        "prevent_destroy" => "**prevent_destroy** — Guard against accidental deletion.\n\n\
            `bool`. Default `false`. When `true`, any plan that would \
            destroy this resource aborts with an error. Remove the flag \
            or the resource to actually destroy.",
        "ignore_changes" => "**ignore_changes** — Ignore drift for named attributes.\n\n\
            `list` of attribute references (or `all`). Terraform won't plan \
            updates when these attributes change in the remote state. Use \
            for fields modified out-of-band (e.g. autoscaling desired counts \
            managed by a cloud policy).",
        "replace_triggered_by" => "**replace_triggered_by** — Force replacement when a referenced value changes.\n\n\
            `list` of resource or attribute references. When any referenced \
            value changes, this resource is destroyed and re-created instead \
            of updated in place.",
        // Intentionally terse for `enabled` — the portability nuance
        // (warning on `.tf`, silent on `.tofu`) is appended by the
        // hover renderer at a different layer, so putting it here
        // would make `.tofu` hovers duplicate a warning they've
        // suppressed.
        "enabled" => "**enabled** — Conditionally include the resource.\n\n\
            `bool`. When `false`, the resource is omitted from the plan.",
        _ => "",
    }
}

/// Canonical description for a nested block inside `lifecycle { … }`.
pub fn lifecycle_block_description(name: &str) -> &'static str {
    match name {
        "precondition" => "**precondition** — Assertion evaluated before plan/apply.\n\n\
            Attributes `condition` (bool expression) and `error_message` \
            (string). Fails the plan when `condition` evaluates `false`. \
            Use for sanity checks against input (e.g. \"the AMI exists in \
            this region\").",
        "postcondition" => "**postcondition** — Assertion evaluated after apply.\n\n\
            Same shape as `precondition`. Fails the apply when `condition` \
            is `false` after the resource is created/updated. Use for \
            invariants that can only be verified from the live resource \
            state.",
        _ => "",
    }
}

/// Canonical description for `condition` / `error_message` inside a
/// `precondition` / `postcondition` block.
pub fn condition_attr_description(name: &str) -> &'static str {
    match name {
        "condition" => "**condition** — Boolean expression that must hold.\n\n\
            Evaluated before (precondition) or after (postcondition) the \
            resource operation. `false` aborts the plan/apply with \
            `error_message`.",
        "error_message" => "**error_message** — Message shown when `condition` fails.\n\n\
            String. Displayed to the user when the assertion trips. Should \
            explain what the condition was checking and how to fix it.",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_meta_blocks_cover_resource_and_data() {
        assert!(is_singleton_meta_block(BlockKind::Resource, "lifecycle"));
        assert!(is_singleton_meta_block(BlockKind::Resource, "connection"));
        assert!(!is_singleton_meta_block(BlockKind::Resource, "provisioner"));
        assert!(is_singleton_meta_block(BlockKind::Data, "lifecycle"));
        assert!(!is_singleton_meta_block(BlockKind::Data, "provisioner"));
        assert!(!is_singleton_meta_block(BlockKind::Data, "connection"));
        assert!(!is_singleton_meta_block(BlockKind::Resource, "not_a_meta"));
    }
}
