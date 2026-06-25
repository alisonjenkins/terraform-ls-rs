//! Terraform built-in named values — the language-provided reference
//! namespaces that aren't declared anywhere in configuration: `path.*`,
//! `terraform.*`, `count.*`, `each.*`, and the `self` object available
//! inside provisioners.
//!
//! Descriptions feed hover docs (and could back the existing top-level
//! completion `detail` strings) so there is a single source of truth.
//!
//! See: <https://developer.hashicorp.com/terraform/language/expressions/references>

/// The recognised head identifiers for a built-in named value.
pub const NAMED_VALUE_HEADS: &[&str] = &["path", "terraform", "count", "each", "self"];

/// True if `head` introduces a built-in named-value namespace.
pub fn is_named_value_head(head: &str) -> bool {
    NAMED_VALUE_HEADS.contains(&head)
}

/// Canonical markdown for a built-in named value.
///
/// `attr` is the segment after the head (`Some("module")` for
/// `path.module`); `None` requests the namespace overview (hovering the
/// bare `path` / `terraform` / `count` / `each` head). `self` ignores
/// `attr`. Returns an empty string for anything unrecognised.
pub fn named_value_description(head: &str, attr: Option<&str>) -> &'static str {
    match (head, attr) {
        // ---- path.* -------------------------------------------------
        ("path", None) => {
            "**path** — filesystem path namespace.\n\n\
            `path.module` (this module's directory), `path.root` (the root \
            module's directory), `path.cwd` (the process working directory). \
            Paths are returned without a trailing slash; join with `/`."
        }
        ("path", Some("module")) => {
            "**path.module** — `string`.\n\n\
            The filesystem path of the module where the expression is used. \
            The usual way to reference a file that ships alongside a module, \
            e.g. `templatefile(\"${path.module}/init.tpl\", …)`."
        }
        ("path", Some("root")) => {
            "**path.root** — `string`.\n\n\
            The filesystem path of the root module of the configuration — the \
            directory `terraform`/`tofu` was invoked from (or `-chdir`)."
        }
        ("path", Some("cwd")) => {
            "**path.cwd** — `string`.\n\n\
            The filesystem path of the current working directory. With \
            `-chdir` this differs from `path.root`; otherwise they match. \
            Prefer `path.root` / `path.module` unless you specifically need \
            the process CWD."
        }
        // ---- terraform.* --------------------------------------------
        ("terraform", None) => {
            "**terraform** — runtime metadata namespace.\n\n\
            `terraform.workspace` — the name of the currently selected \
            workspace. (Older `terraform.*` members are deprecated.)"
        }
        ("terraform", Some("workspace")) => {
            "**terraform.workspace** — `string`.\n\n\
            The name of the currently selected workspace, e.g. `\"default\"`. \
            Commonly used to vary names or sizing per environment: \
            `count = terraform.workspace == \"prod\" ? 5 : 1`. Avoid coupling \
            real infrastructure decisions to it where a module input would be \
            clearer."
        }
        // ---- count.* ------------------------------------------------
        ("count", None) | ("count", Some("index")) => {
            "**count.index** — `number`.\n\n\
            The distinct zero-based index of each instance created by the \
            `count` meta-argument. Only available in blocks that declare \
            `count`. For the first instance it's `0`, the second `1`, and so \
            on."
        }
        // ---- each.* -------------------------------------------------
        ("each", None) => {
            "**each** — `for_each` iteration namespace.\n\n\
            `each.key` (the map key / set value identifying this instance) and \
            `each.value` (the value for this instance). Only available in \
            blocks that declare `for_each`."
        }
        ("each", Some("key")) => {
            "**each.key** — `string`.\n\n\
            The map key (or set member) identifying the current instance of a \
            `for_each` block. For a `set(string)`, `each.key` equals \
            `each.value`."
        }
        ("each", Some("value")) => {
            "**each.value** — the per-instance value.\n\n\
            The map value (or set member) for the current instance of a \
            `for_each` block. Its type is the element type of the `for_each` \
            collection."
        }
        // ---- self ---------------------------------------------------
        ("self", _) => {
            "**self** — the current resource instance.\n\n\
            Available only inside `provisioner`, `connection`, and \
            `postcondition` blocks, where referring to the resource by its \
            own address would be circular. Access the instance's attributes \
            directly, e.g. `self.private_ip`, `self.id`."
        }
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_named_values_have_descriptions() {
        assert!(!named_value_description("path", Some("module")).is_empty());
        assert!(!named_value_description("path", Some("root")).is_empty());
        assert!(!named_value_description("path", Some("cwd")).is_empty());
        assert!(!named_value_description("terraform", Some("workspace")).is_empty());
        assert!(!named_value_description("count", Some("index")).is_empty());
        assert!(!named_value_description("each", Some("key")).is_empty());
        assert!(!named_value_description("each", Some("value")).is_empty());
        assert!(!named_value_description("self", None).is_empty());
    }

    #[test]
    fn bare_heads_give_overviews() {
        assert!(!named_value_description("path", None).is_empty());
        assert!(!named_value_description("terraform", None).is_empty());
        assert!(!named_value_description("count", None).is_empty());
        assert!(!named_value_description("each", None).is_empty());
    }

    #[test]
    fn unknown_is_empty() {
        assert!(named_value_description("var", Some("foo")).is_empty());
        assert!(named_value_description("path", Some("bogus")).is_empty());
        assert!(!is_named_value_head("var"));
        assert!(is_named_value_head("path"));
    }
}
