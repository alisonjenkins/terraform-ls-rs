//! Curated mapping from (resource_type, attr_name) → target resource
//! type for context-aware value completions.
//!
//! Terraform provider schemas don't encode cross-resource references,
//! so we maintain an explicit table for the most common patterns plus
//! a suffix-based heuristic fallback.

/// Given a resource type and attribute name, return the resource type
/// that this attribute likely references.
pub fn referenced_resource_type(resource_type: &str, attr_name: &str) -> Option<&'static str> {
    // Check the explicit curated map first.
    if let Some(target) = explicit_lookup(resource_type, attr_name) {
        return Some(target);
    }
    // Fall back to suffix-based heuristic.
    suffix_heuristic(resource_type, attr_name)
}

/// Infer the output attribute suffix to use for the reference.
/// `security_group_id` → `.id`, `role_arn` → `.arn`, etc.
pub fn output_attribute(attr_name: &str) -> &'static str {
    if attr_name.ends_with("_arn") || attr_name == "arn" {
        ".arn"
    } else if attr_name.ends_with("_name") {
        ".name"
    } else {
        ".id"
    }
}

fn explicit_lookup(resource_type: &str, attr_name: &str) -> Option<&'static str> {
    // Normalise plural attrs: `vpc_security_group_ids` → `vpc_security_group_id`
    let attr = attr_name.strip_suffix('s').unwrap_or(attr_name);

    match (resource_type, attr) {
        // --- Security groups ---
        (_, "security_group_id") => Some("aws_security_group"),
        (_, "source_security_group_id") => Some("aws_security_group"),
        (_, "vpc_security_group_id") => Some("aws_security_group"),

        // --- Networking ---
        (_, "subnet_id") => Some("aws_subnet"),
        (_, "vpc_id") => Some("aws_vpc"),
        (_, "route_table_id") => Some("aws_route_table"),
        (_, "internet_gateway_id") => Some("aws_internet_gateway"),
        (_, "gateway_id") => Some("aws_internet_gateway"),
        (_, "nat_gateway_id") => Some("aws_nat_gateway"),
        (_, "network_interface_id") => Some("aws_network_interface"),
        (_, "eip_id") => Some("aws_eip"),
        (_, "allocation_id") => Some("aws_eip"),
        (_, "network_acl_id") => Some("aws_network_acl"),

        // --- IAM ---
        (_, "role_arn") => Some("aws_iam_role"),
        (_, "role") if resource_type.starts_with("aws_iam_") => Some("aws_iam_role"),
        (_, "policy_arn") => Some("aws_iam_policy"),
        (_, "instance_profile_arn") => Some("aws_iam_instance_profile"),
        (_, "iam_instance_profile") => Some("aws_iam_instance_profile"),

        // --- S3 ---
        (_, "bucket") if resource_type.starts_with("aws_s3_") => Some("aws_s3_bucket"),
        ("aws_s3_bucket_notification", "bucket") => Some("aws_s3_bucket"),
        ("aws_s3_bucket_policy", "bucket") => Some("aws_s3_bucket"),

        // --- Load balancers ---
        (_, "target_group_arn") => Some("aws_lb_target_group"),
        (_, "load_balancer_arn") => Some("aws_lb"),

        // --- EC2 ---
        (_, "instance_id") => Some("aws_instance"),
        (_, "ami") => Some("aws_ami"),
        (_, "key_name") => Some("aws_key_pair"),
        (_, "launch_template_id") => Some("aws_launch_template"),
        (_, "placement_group") => Some("aws_placement_group"),

        // --- KMS ---
        (_, "kms_key_id") => Some("aws_kms_key"),
        (_, "kms_key_arn") => Some("aws_kms_key"),

        // --- SNS / SQS ---
        (_, "topic_arn") => Some("aws_sns_topic"),
        (_, "queue_arn") => Some("aws_sqs_queue"),

        // --- CloudWatch ---
        (_, "log_group_name") => Some("aws_cloudwatch_log_group"),

        // --- Lambda ---
        (_, "function_name") if resource_type.starts_with("aws_lambda_") => {
            Some("aws_lambda_function")
        }

        // --- ECS ---
        (_, "cluster") if resource_type.starts_with("aws_ecs_") => Some("aws_ecs_cluster"),
        (_, "task_definition") => Some("aws_ecs_task_definition"),

        // --- ACM ---
        (_, "certificate_arn") => Some("aws_acm_certificate"),

        // --- Route53 ---
        (_, "zone_id") => Some("aws_route53_zone"),

        // --- RDS ---
        (_, "db_subnet_group_name") => Some("aws_db_subnet_group"),
        (_, "parameter_group_name") if resource_type.starts_with("aws_db_") => {
            Some("aws_db_parameter_group")
        }

        _ => None,
    }
}

/// Fallback: for `foo_id` inside an `aws_*` resource, try `aws_foo`.
fn suffix_heuristic(resource_type: &str, attr_name: &str) -> Option<&'static str> {
    // Only apply within the same provider prefix.
    let provider_prefix = resource_type.split('_').next()?;
    if provider_prefix.is_empty() {
        return None;
    }

    // Strip plural suffix first.
    let attr = attr_name.strip_suffix('s').unwrap_or(attr_name);

    if let Some(base) = attr.strip_suffix("_id") {
        // `subnet_id` → try `aws_subnet`
        let candidate = format!("{provider_prefix}_{base}");
        // We can't return a &'static str from a dynamic string, so
        // the heuristic only works for the explicit map. Return None
        // here — the explicit map above covers the common cases.
        let _ = candidate;
    }

    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn security_group_id_maps_correctly() {
        assert_eq!(
            referenced_resource_type("aws_security_group_rule", "security_group_id"),
            Some("aws_security_group")
        );
    }

    #[test]
    fn plural_attr_normalised() {
        assert_eq!(
            referenced_resource_type("aws_instance", "vpc_security_group_ids"),
            Some("aws_security_group")
        );
    }

    #[test]
    fn subnet_id_maps() {
        assert_eq!(
            referenced_resource_type("aws_instance", "subnet_id"),
            Some("aws_subnet")
        );
    }

    #[test]
    fn role_arn_maps() {
        assert_eq!(
            referenced_resource_type("aws_lambda_function", "role_arn"),
            Some("aws_iam_role")
        );
    }

    #[test]
    fn unknown_attr_returns_none() {
        assert_eq!(
            referenced_resource_type("aws_instance", "tags"),
            None
        );
    }

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
    fn s3_bucket_in_s3_context() {
        assert_eq!(
            referenced_resource_type("aws_s3_bucket_policy", "bucket"),
            Some("aws_s3_bucket")
        );
    }

    #[test]
    fn iam_role_in_iam_context() {
        assert_eq!(
            referenced_resource_type("aws_iam_role_policy_attachment", "role"),
            Some("aws_iam_role")
        );
    }
}
