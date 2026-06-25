//! Terraform type-constraint keywords — the type-system vocabulary used on
//! the right-hand side of `type =` inside `variable` blocks (and nested
//! inside the collection constructors).
//!
//! These are NOT functions and never appear in provider schemas; they're
//! language-level constructs. Descriptions are lifted from the Terraform /
//! OpenTofu type-constraints reference and feed both hover docs and the
//! `type =` completion `documentation`/`detail` popups so there is a single
//! source of truth.
//!
//! See: <https://developer.hashicorp.com/terraform/language/expressions/type-constraints>

/// Every keyword recognised in a type-constraint expression: the three
/// primitives, the `any`/`null` specials, the five collection
/// constructors, and `optional` (valid only inside `object({…})`).
pub const TYPE_CONSTRAINT_KEYWORDS: &[&str] = &[
    "string", "number", "bool", "any", "null", "list", "set", "map", "tuple", "object", "optional",
];

/// True if `name` is one of [`TYPE_CONSTRAINT_KEYWORDS`].
pub fn is_type_constraint_keyword(name: &str) -> bool {
    TYPE_CONSTRAINT_KEYWORDS.contains(&name)
}

/// Canonical markdown description for a type-constraint keyword. Returns an
/// empty string for names that aren't keywords. `optional` delegates to
/// [`optional_description`] since it carries the most nuance.
pub fn type_constraint_description(name: &str) -> &'static str {
    match name {
        "string" => {
            "**string** — primitive type.\n\n\
            A sequence of Unicode characters, e.g. `\"hello\"`. Terraform \
            automatically converts `number` and `bool` values to `string` \
            when a string is expected."
        }
        "number" => {
            "**number** — primitive type.\n\n\
            A numeric value. Represents both whole numbers like `15` and \
            fractional values like `6.28`. A string containing only digits \
            converts to `number` automatically."
        }
        "bool" => {
            "**bool** — primitive type.\n\n\
            A boolean value, either `true` or `false`. The strings `\"true\"` \
            / `\"false\"` convert to `bool`, and `bool` converts to the \
            strings `\"true\"` / `\"false\"` when a string is expected."
        }
        "any" => {
            "**any** — type placeholder.\n\n\
            `any` is not a type itself; it's a placeholder telling Terraform \
            to infer the concrete type from the value supplied at runtime. \
            Use it for genuinely pass-through values. Prefer a concrete \
            constraint where you can — it documents intent and lets Terraform \
            catch type errors earlier."
        }
        "null" => {
            "**null** — the absence of a value.\n\n\
            `null` represents \"no value\". Setting an argument to `null` is \
            the same as omitting it: the argument's default (or the \
            provider's) applies. It is a *value*, not a type constraint — \
            an attribute typed `optional(string)` resolves to `null` when \
            the caller omits it (see `optional`)."
        }
        "list" => {
            "**list(\\<type\\>)** — collection constructor.\n\n\
            An ordered sequence of values that all share `<type>`, indexed by \
            consecutive whole numbers starting at zero, e.g. \
            `list(string)` → `[\"a\", \"b\"]`. Use `set` instead when order \
            and duplicates are irrelevant.\n\n\
            ```hcl\n\
            type = list(string)\n\
            ```"
        }
        "set" => {
            "**set(\\<type\\>)** — collection constructor.\n\n\
            An unordered collection of unique values sharing `<type>`. \
            Duplicates are coalesced and ordering is not preserved, e.g. \
            `set(string)`. Convert to a `list` (with `tolist`) when you need \
            indexing or a stable order.\n\n\
            ```hcl\n\
            type = set(string)\n\
            ```"
        }
        "map" => {
            "**map(\\<type\\>)** — collection constructor.\n\n\
            A collection of values sharing `<type>`, each identified by a \
            unique string key, e.g. `map(number)` → \
            `{ a = 1, b = 2 }`. Keys are always strings. Use `object` \
            instead when the keys are fixed and each may have its own type.\n\n\
            ```hcl\n\
            type = map(string)\n\
            ```"
        }
        "tuple" => {
            "**tuple([\\<type\\>, …])** — structural constructor.\n\n\
            An ordered sequence of a fixed length where each position has its \
            own type, e.g. `tuple([string, number, bool])` matches \
            `[\"a\", 15, true]`. The supplied value must have exactly the \
            same number of elements.\n\n\
            ```hcl\n\
            type = tuple([string, number, bool])\n\
            ```"
        }
        "object" => {
            "**object({ \\<name\\> = \\<type\\>, … })** — structural constructor.\n\n\
            A record whose attributes each have their own name and type, e.g. \
            `object({ name = string, age = number })`. By default every \
            attribute is required; wrap an attribute's type in `optional(…)` \
            to let callers omit it.\n\n\
            ```hcl\n\
            type = object({\n  \
              name = string\n  \
              age  = optional(number, 0)\n\
            })\n\
            ```"
        }
        "optional" => optional_description(),
        _ => "",
    }
}

/// The rich description for `optional` — the one keyword with behaviour
/// subtle enough to mislead. Key point: an omitted optional attribute
/// becomes `null` for **every** type unless a default is supplied; there is
/// no type-specific empty default (`optional(list(string))` is `null` when
/// omitted, *not* `[]`).
pub fn optional_description() -> &'static str {
    "**optional(\\<type\\>[, \\<default\\>])** — optional object attribute.\n\n\
    Marks an attribute of an `object({…})` type as optional, so callers may \
    omit it. Valid **only** inside an `object` type constraint.\n\n\
    **When the attribute is omitted:**\n\
    - `optional(<type>)` — the value becomes **`null`** (a typed null). This \
    is true for *every* type — `optional(string)`, `optional(number)`, \
    `optional(list(string))`, `optional(object({…}))` all resolve to `null`, \
    never to `\"\"`, `0`, or `[]`.\n\
    - `optional(<type>, <default>)` — the value becomes `<default>` instead \
    of `null`. The default is converted to `<type>`, so it must be \
    convertible (e.g. the default for `optional(number, …)` must be a number).\n\n\
    A common misconception is that `optional(list(string))` defaults to an \
    empty list — it does not. Supply the default explicitly: \
    `optional(list(string), [])`.\n\n\
    ```hcl\n\
    variable \"server\" {\n  \
      type = object({\n    \
        name    = string                       # required\n    \
        port    = optional(number, 8080)       # omitted → 8080\n    \
        tags    = optional(map(string), {})    # omitted → {}\n    \
        aliases = optional(list(string))       # omitted → null (NOT [])\n  \
      })\n\
    }\n\
    ```"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_keyword_has_a_description() {
        for kw in TYPE_CONSTRAINT_KEYWORDS {
            assert!(
                !type_constraint_description(kw).is_empty(),
                "missing description for `{kw}`"
            );
            assert!(is_type_constraint_keyword(kw));
        }
    }

    #[test]
    fn unknown_keyword_is_empty() {
        assert!(type_constraint_description("resource").is_empty());
        assert!(!is_type_constraint_keyword("resource"));
    }

    #[test]
    fn optional_explains_null_when_omitted() {
        let d = optional_description();
        assert!(d.contains("optional"));
        assert!(d.contains("null"), "must explain the omitted-value is null");
        // Guards against the empty-list misconception being dropped.
        assert!(d.contains("[]"));
    }
}
