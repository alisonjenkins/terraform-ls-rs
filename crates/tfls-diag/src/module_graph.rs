//! Narrow trait the cross-file (Pass 2) diagnostic walkers consume.
//! `tfls-diag` stays free of LSP / state-store dependencies — the
//! caller in `tfls-lsp` builds an adapter over [`StateStore`] that
//! provides the four pieces of info these rules need.

use std::collections::HashSet;

pub trait ModuleGraphLookup {
    /// True if `var.<name>` / `local.<name>` / `module.<name>` /
    /// `data.<type>.<name>` appears anywhere in the same module as
    /// the document being linted. Used to drive the "unused
    /// declaration" rule.
    fn variable_is_referenced(&self, name: &str) -> bool;
    fn local_is_referenced(&self, name: &str) -> bool;
    fn data_source_is_referenced(&self, type_name: &str, name: &str) -> bool;

    /// Set of provider local names that are actually in use in the
    /// module — derived from resource/data block types and from
    /// explicit `provider = foo.east` meta-arguments. Used to drive
    /// the "unused required_providers" rule.
    fn used_provider_locals(&self) -> HashSet<String>;

    /// Which of the standard filenames exist in the module directory
    /// (`main.tf`, `variables.tf`, `outputs.tf`, …). Used for the
    /// `standard_module_structure` rule.
    fn present_files(&self) -> HashSet<String>;

    /// True if this module is a "root" — i.e. not consumed by any
    /// `module { source = "..." }` block elsewhere in the workspace.
    /// Tflint only applies `unused_declarations` to root modules,
    /// since a reusable module's variables are intentionally
    /// "unused" from the module's own point of view.
    fn is_root_module(&self) -> bool;
}
