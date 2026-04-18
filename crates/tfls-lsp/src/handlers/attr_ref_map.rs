//! Dynamic attribute → resource type resolver for context-aware value
//! completions.
//!
//! Instead of maintaining a giant static mapping table for every AWS
//! and AzureRM resource, we resolve references dynamically using the
//! loaded provider schemas: extract a candidate target type from the
//! attribute name's naming convention, then check if that type actually
//! exists in the schema. A small override table handles the irregular
//! cases that don't follow conventions.

use std::collections::HashSet;

/// Given a resource type, attribute name, and the set of all known
/// resource types from loaded schemas, return the resource type that
/// this attribute likely references.
pub fn referenced_resource_type(
    resource_type: &str,
    attr_name: &str,
    known_types: &HashSet<String>,
) -> Option<String> {
    // 1. Check explicit overrides for irregular naming.
    if let Some(target) = explicit_override(resource_type, attr_name) {
        if known_types.contains(target) {
            return Some(target.to_string());
        }
    }
    // 2. Dynamic resolution using naming conventions + schema validation.
    dynamic_resolve(resource_type, attr_name, known_types)
}

/// Infer the output attribute suffix to use for the reference.
/// `security_group_id` → `.id`, `role_arn` → `.arn`, etc.
pub fn output_attribute(attr_name: &str) -> &'static str {
    if attr_name.ends_with("_arn") || attr_name.ends_with("_arns") || attr_name == "arn" {
        ".arn"
    } else if attr_name.ends_with("_name") {
        ".name"
    } else {
        ".id"
    }
}

/// Dynamic resolution: extract a candidate resource type from the
/// attribute name's suffix pattern and validate it against the schema.
fn dynamic_resolve(
    resource_type: &str,
    attr_name: &str,
    known_types: &HashSet<String>,
) -> Option<String> {
    let provider = provider_prefix(resource_type)?;

    // Normalise: strip plural `s` suffix.
    let attr = attr_name
        .strip_suffix("_ids")
        .or_else(|| attr_name.strip_suffix("_arns"))
        .map(|base| format!("{base}_id"))
        .unwrap_or_else(|| attr_name.to_string());

    // Extract the base name by stripping known suffixes.
    let base = attr
        .strip_suffix("_id")
        .or_else(|| attr.strip_suffix("_arn"))
        .or_else(|| attr.strip_suffix("_name"))
        .or_else(|| attr.strip_suffix("_key"));
    let base = base?;

    if base.is_empty() {
        return None;
    }

    // Try the full base first: `security_group_id` → `aws_security_group`.
    let candidate = format!("{provider}_{base}");
    if known_types.contains(&candidate) {
        return Some(candidate);
    }

    // Progressive prefix stripping: `vpc_security_group_id` →
    // try `aws_vpc_security_group` (tried above), then `aws_security_group`.
    let mut remaining = base;
    while let Some(pos) = remaining.find('_') {
        remaining = &remaining[pos + 1..];
        if remaining.is_empty() {
            break;
        }
        let candidate = format!("{provider}_{remaining}");
        if known_types.contains(&candidate) {
            return Some(candidate);
        }
    }

    None
}

/// Explicit overrides for attributes that don't follow naming conventions.
fn explicit_override(resource_type: &str, attr_name: &str) -> Option<&'static str> {
    // Normalise plural attrs.
    let attr = attr_name
        .strip_suffix("_ids")
        .or_else(|| attr_name.strip_suffix("_arns"))
        .unwrap_or(attr_name);

    match (resource_type, attr) {
        // --- AWS: irregular names ---
        (_, "ami") => Some("aws_ami"),
        (_, "gateway_id") => Some("aws_internet_gateway"),
        (_, "allocation_id") => Some("aws_eip"),
        (_, "role") if resource_type.starts_with("aws_iam_") => Some("aws_iam_role"),
        (_, "bucket") if resource_type.starts_with("aws_s3_") => Some("aws_s3_bucket"),
        (_, "cluster") if resource_type.starts_with("aws_ecs_") => Some("aws_ecs_cluster"),
        (_, "task_definition") if resource_type.starts_with("aws_ecs_") => {
            Some("aws_ecs_task_definition")
        }
        (_, "load_balancer_arn") => Some("aws_lb"),
        (_, "target_group_arn") => Some("aws_lb_target_group"),
        (_, "topic_arn") => Some("aws_sns_topic"),
        (_, "key_name") => Some("aws_key_pair"),
        (_, "placement_group") => Some("aws_placement_group"),
        (_, "iam_instance_profile") => Some("aws_iam_instance_profile"),
        (_, "log_group_name") => Some("aws_cloudwatch_log_group"),
        (_, "function_name") if resource_type.starts_with("aws_lambda_") => {
            Some("aws_lambda_function")
        }

        // --- AzureRM: irregular names ---
        (_, "resource_group_name") => Some("azurerm_resource_group"),
        (_, "public_ip_address_id") => Some("azurerm_public_ip"),
        (_, "managed_disk_id") => Some("azurerm_managed_disk"),

        _ => None,
    }
}

/// Extract the provider prefix from a resource type name.
/// `aws_instance` → `aws`, `azurerm_subnet` → `azurerm`.
fn provider_prefix(resource_type: &str) -> Option<&str> {
    let pos = resource_type.find('_')?;
    let prefix = &resource_type[..pos];
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Build a mock set of known resource types for testing.
    fn aws_types() -> HashSet<String> {
        [
            "aws_security_group",
            "aws_subnet",
            "aws_vpc",
            "aws_instance",
            "aws_ami",
            "aws_route_table",
            "aws_internet_gateway",
            "aws_nat_gateway",
            "aws_eip",
            "aws_network_interface",
            "aws_network_acl",
            "aws_iam_role",
            "aws_iam_policy",
            "aws_iam_instance_profile",
            "aws_s3_bucket",
            "aws_lb",
            "aws_lb_target_group",
            "aws_kms_key",
            "aws_sns_topic",
            "aws_sqs_queue",
            "aws_cloudwatch_log_group",
            "aws_lambda_function",
            "aws_ecs_cluster",
            "aws_ecs_task_definition",
            "aws_acm_certificate",
            "aws_route53_zone",
            "aws_db_subnet_group",
            "aws_db_parameter_group",
            "aws_key_pair",
            "aws_placement_group",
            "aws_launch_template",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    fn azurerm_types() -> HashSet<String> {
        let mut types = aws_types();
        types.extend(
            [
                "azurerm_subnet",
                "azurerm_virtual_network",
                "azurerm_resource_group",
                "azurerm_storage_account",
                "azurerm_key_vault",
                "azurerm_key_vault_secret",
                "azurerm_network_security_group",
                "azurerm_public_ip",
                "azurerm_managed_disk",
                "azurerm_log_analytics_workspace",
                "azurerm_network_interface",
            ]
            .into_iter()
            .map(String::from),
        );
        types
    }

    // --- AWS dynamic resolution ---

    #[test]
    fn security_group_id_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_security_group_rule", "security_group_id", &types),
            Some("aws_security_group".into())
        );
    }

    #[test]
    fn plural_ids_normalised() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_instance", "vpc_security_group_ids", &types),
            Some("aws_security_group".into())
        );
    }

    #[test]
    fn subnet_id_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_instance", "subnet_id", &types),
            Some("aws_subnet".into())
        );
    }

    #[test]
    fn vpc_id_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_subnet", "vpc_id", &types),
            Some("aws_vpc".into())
        );
    }

    #[test]
    fn kms_key_id_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_ebs_volume", "kms_key_id", &types),
            Some("aws_kms_key".into())
        );
    }

    #[test]
    fn launch_template_id_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_instance", "launch_template_id", &types),
            Some("aws_launch_template".into())
        );
    }

    #[test]
    fn route_table_id_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_route", "route_table_id", &types),
            Some("aws_route_table".into())
        );
    }

    // --- AWS explicit overrides ---

    #[test]
    fn ami_override() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_instance", "ami", &types),
            Some("aws_ami".into())
        );
    }

    #[test]
    fn gateway_id_override() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_route", "gateway_id", &types),
            Some("aws_internet_gateway".into())
        );
    }

    #[test]
    fn allocation_id_override() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_nat_gateway", "allocation_id", &types),
            Some("aws_eip".into())
        );
    }

    #[test]
    fn iam_role_override() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_iam_role_policy_attachment", "role", &types),
            Some("aws_iam_role".into())
        );
    }

    #[test]
    fn s3_bucket_override() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_s3_bucket_policy", "bucket", &types),
            Some("aws_s3_bucket".into())
        );
    }

    #[test]
    fn load_balancer_arn_override() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_lb_listener", "load_balancer_arn", &types),
            Some("aws_lb".into())
        );
    }

    // --- AWS ARN suffix ---

    #[test]
    fn target_group_arn_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_lb_listener", "target_group_arn", &types),
            Some("aws_lb_target_group".into())
        );
    }

    #[test]
    fn topic_arn_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_sns_topic_subscription", "topic_arn", &types),
            Some("aws_sns_topic".into())
        );
    }

    // --- AWS name suffix ---

    #[test]
    fn db_subnet_group_name_resolves() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_db_instance", "db_subnet_group_name", &types),
            Some("aws_db_subnet_group".into())
        );
    }

    // --- AzureRM dynamic resolution ---

    #[test]
    fn azurerm_subnet_id_resolves() {
        let types = azurerm_types();
        assert_eq!(
            referenced_resource_type("azurerm_network_interface", "subnet_id", &types),
            Some("azurerm_subnet".into())
        );
    }

    #[test]
    fn azurerm_virtual_network_name_resolves() {
        let types = azurerm_types();
        assert_eq!(
            referenced_resource_type("azurerm_subnet", "virtual_network_name", &types),
            Some("azurerm_virtual_network".into())
        );
    }

    #[test]
    fn azurerm_storage_account_id_resolves() {
        let types = azurerm_types();
        assert_eq!(
            referenced_resource_type("azurerm_monitor_diagnostic_setting", "storage_account_id", &types),
            Some("azurerm_storage_account".into())
        );
    }

    #[test]
    fn azurerm_network_security_group_id_resolves() {
        let types = azurerm_types();
        assert_eq!(
            referenced_resource_type("azurerm_subnet", "network_security_group_id", &types),
            Some("azurerm_network_security_group".into())
        );
    }

    #[test]
    fn azurerm_key_vault_secret_id_resolves() {
        let types = azurerm_types();
        assert_eq!(
            referenced_resource_type("azurerm_app_service", "key_vault_secret_id", &types),
            Some("azurerm_key_vault_secret".into())
        );
    }

    #[test]
    fn azurerm_log_analytics_workspace_id_resolves() {
        let types = azurerm_types();
        assert_eq!(
            referenced_resource_type("azurerm_monitor_diagnostic_setting", "log_analytics_workspace_id", &types),
            Some("azurerm_log_analytics_workspace".into())
        );
    }

    // --- AzureRM explicit overrides ---

    #[test]
    fn azurerm_resource_group_name_override() {
        let types = azurerm_types();
        assert_eq!(
            referenced_resource_type("azurerm_virtual_network", "resource_group_name", &types),
            Some("azurerm_resource_group".into())
        );
    }

    #[test]
    fn azurerm_public_ip_address_id_override() {
        let types = azurerm_types();
        assert_eq!(
            referenced_resource_type("azurerm_lb", "public_ip_address_id", &types),
            Some("azurerm_public_ip".into())
        );
    }

    // --- Negative cases ---

    #[test]
    fn tags_returns_none() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_instance", "tags", &types),
            None
        );
    }

    #[test]
    fn unknown_type_not_in_schema_returns_none() {
        let types = aws_types();
        assert_eq!(
            referenced_resource_type("aws_instance", "nonexistent_widget_id", &types),
            None
        );
    }

    // --- Output attribute ---

    #[test]
    fn output_attr_for_id() {
        assert_eq!(output_attribute("security_group_id"), ".id");
    }

    #[test]
    fn output_attr_for_arn() {
        assert_eq!(output_attribute("role_arn"), ".arn");
    }

    #[test]
    fn output_attr_for_name() {
        assert_eq!(output_attribute("function_name"), ".name");
    }

    #[test]
    fn output_attr_for_plural_arns() {
        assert_eq!(output_attribute("policy_arns"), ".arn");
    }
}
