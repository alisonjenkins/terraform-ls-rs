//! Diagnostics engine for terraform-ls-rs.

pub mod comment_syntax;
pub mod deprecated_index;
pub mod deprecated_interpolation;
pub mod deprecated_lookup;
pub mod deprecated_null_data_source;
pub mod deprecated_null_resource;
pub mod deprecated_template_dir;
pub mod deprecated_template_file;
pub mod deprecation_rule;
pub mod documented_outputs;
pub mod documented_variables;
pub mod empty_list_equality;
pub mod error;
pub mod expr_walk;
pub mod map_duplicate_keys;
pub mod module_graph;
pub mod module_pinned_source;
pub mod module_shallow_clone;
pub mod module_version_presence;
pub mod naming_convention;
pub mod references;
pub mod required_providers_version;
pub mod required_version_presence;
pub mod schema_validation;
pub mod standard_module_structure;
pub mod syntax;
pub mod typed_variables;
pub mod unused_declarations;
pub mod unused_required_providers;
pub mod variable_default_type;
pub mod version_constraint;
pub mod workspace_remote;

pub use comment_syntax::comment_syntax_diagnostics;
pub use deprecated_index::deprecated_index_diagnostics;
pub use deprecated_interpolation::deprecated_interpolation_diagnostics;
pub use deprecated_lookup::deprecated_lookup_diagnostics;
pub use deprecated_null_data_source::{
    deprecated_null_data_source_diagnostics,
    deprecated_null_data_source_diagnostics_for_module, supports_locals_replacement,
};
pub use deprecated_null_resource::{
    deprecated_null_resource_diagnostics, deprecated_null_resource_diagnostics_for_module,
    extract_required_version, supports_terraform_data,
};
pub use deprecated_template_dir::{
    deprecated_template_dir_diagnostics, deprecated_template_dir_diagnostics_for_module,
};
pub use deprecated_template_file::{
    deprecated_template_file_diagnostics, deprecated_template_file_diagnostics_for_module,
    supports_templatefile,
};
pub use documented_outputs::documented_outputs_diagnostics;
pub use documented_variables::documented_variables_diagnostics;
pub use empty_list_equality::empty_list_equality_diagnostics;
pub use error::DiagError;
pub use map_duplicate_keys::map_duplicate_keys_diagnostics;
pub use module_graph::ModuleGraphLookup;
pub use module_pinned_source::module_pinned_source_diagnostics;
pub use module_shallow_clone::module_shallow_clone_diagnostics;
pub use module_version_presence::module_version_presence_diagnostics;
pub use naming_convention::naming_convention_diagnostics;
pub use references::{undefined_reference_diagnostics, undefined_reference_diagnostics_for_document};
pub use required_providers_version::required_providers_version_diagnostics;
pub use required_version_presence::required_version_presence_diagnostics;
pub use schema_validation::resource_diagnostics;
pub use standard_module_structure::standard_module_structure_diagnostics;
pub use syntax::diagnostics_for_parse_errors;
pub use typed_variables::typed_variables_diagnostics;
pub use unused_declarations::unused_declarations_diagnostics;
pub use unused_required_providers::unused_required_providers_diagnostics;
pub use variable_default_type::variable_default_type_diagnostics;
pub use version_constraint::{ConstraintSource, VersionCacheLookup, constraint_diagnostics};
pub use workspace_remote::workspace_remote_diagnostics;
