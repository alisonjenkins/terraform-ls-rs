//! Static schemas for Terraform / OpenTofu built-in blocks.
//!
//! Unlike `resource` and `data` — whose attributes come from the
//! provider schema — blocks like `terraform`, `variable`, `output`,
//! `module`, `backend "s3"`, etc. are part of the language and have
//! a fixed, hand-maintained shape. Rather than reinvent the full
//! `AttributeSchema` structure, this module exposes a lean view
//! (`BuiltinAttr` / `BuiltinBlock`) that the completion dispatcher
//! can iterate directly.
//!
//! Coverage is intentionally focused on what a user is likely to
//! type. Exotic / deprecated attributes can be added as the need
//! surfaces.

/// One attribute on a built-in block.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinAttr {
    pub name: &'static str,
    pub required: bool,
    /// Short doc string shown in the completion item `detail`.
    pub detail: &'static str,
}

/// One nested block type declared inside a built-in block.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinBlock {
    pub name: &'static str,
    pub detail: &'static str,
}

/// Schema for one built-in block — attributes + nested blocks.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinSchema {
    pub attrs: &'static [BuiltinAttr],
    pub blocks: &'static [BuiltinBlock],
}

// --- `terraform { ... }` --------------------------------------------------

pub const TERRAFORM_BLOCK: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr {
            name: "required_version",
            required: false,
            detail: "Pin the Terraform/OpenTofu CLI version, e.g. `\">= 1.6\"`",
        },
        BuiltinAttr {
            name: "experiments",
            required: false,
            detail: "Opt-in list of experimental language features",
        },
    ],
    blocks: &[
        BuiltinBlock {
            name: "required_providers",
            detail: "Pin provider sources + versions for this module",
        },
        BuiltinBlock {
            name: "backend",
            detail: "Remote state backend (e.g. `backend \"s3\" { ... }`)",
        },
        BuiltinBlock {
            name: "cloud",
            detail: "HCP Terraform / OpenTofu Cloud configuration",
        },
        BuiltinBlock {
            name: "provider_meta",
            detail: "Metadata the provider reads per-module",
        },
    ],
};

// --- `variable "x" { ... }` -----------------------------------------------

pub const VARIABLE_BLOCK: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr {
            name: "type",
            required: false,
            detail: "Type constraint (e.g. `string`, `number`, `list(string)`, `object({...})`)",
        },
        BuiltinAttr {
            name: "default",
            required: false,
            detail: "Default value — makes the variable optional",
        },
        BuiltinAttr {
            name: "description",
            required: false,
            detail: "Human-readable summary of the variable's purpose",
        },
        BuiltinAttr {
            name: "sensitive",
            required: false,
            detail: "When true, Terraform redacts the value from plan/apply output",
        },
        BuiltinAttr {
            name: "nullable",
            required: false,
            detail: "When false, the value cannot be null (default: true)",
        },
        BuiltinAttr {
            name: "ephemeral",
            required: false,
            detail: "When true, the value is not persisted to state (OpenTofu / TF 1.10+)",
        },
    ],
    blocks: &[BuiltinBlock {
        name: "validation",
        detail: "Custom condition + error_message the value must satisfy",
    }],
};

// --- `output "x" { ... }` -------------------------------------------------

pub const OUTPUT_BLOCK: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr {
            name: "value",
            required: true,
            detail: "Expression the output exports",
        },
        BuiltinAttr {
            name: "description",
            required: false,
            detail: "Human-readable summary",
        },
        BuiltinAttr {
            name: "sensitive",
            required: false,
            detail: "When true, Terraform redacts the value from plan/apply output",
        },
        BuiltinAttr {
            name: "depends_on",
            required: false,
            detail: "Explicit dependencies",
        },
        BuiltinAttr {
            name: "ephemeral",
            required: false,
            detail: "When true, the value is not persisted to state (OpenTofu / TF 1.10+)",
        },
    ],
    blocks: &[BuiltinBlock {
        name: "precondition",
        detail: "Expression that must be true before the output is evaluated",
    }],
};

// --- `module "x" { ... }` -------------------------------------------------

pub const MODULE_BLOCK_META_ATTRS: &[BuiltinAttr] = &[
    BuiltinAttr {
        name: "source",
        required: true,
        detail: "Module source — registry path, git URL, local path, etc.",
    },
    BuiltinAttr {
        name: "version",
        required: false,
        detail: "Version constraint (registry modules only)",
    },
    BuiltinAttr {
        name: "providers",
        required: false,
        detail: "Map from child-module provider keys to parent providers",
    },
];

pub const MODULE_BLOCK: BuiltinSchema = BuiltinSchema {
    attrs: MODULE_BLOCK_META_ATTRS,
    blocks: &[],
};

// --- `required_providers { NAME = { ... } }` entry value ------------------

/// Attributes that live inside the object literal assigned to a
/// required_providers entry: `aws = { source = "…", version = "…" }`.
pub const REQUIRED_PROVIDER_ENTRY_ATTRS: &[BuiltinAttr] = &[
    BuiltinAttr {
        name: "source",
        required: true,
        detail: "Registry source path, e.g. `hashicorp/aws`",
    },
    BuiltinAttr {
        name: "version",
        required: false,
        detail: "Version constraint, e.g. `\"~> 5.0\"`",
    },
    BuiltinAttr {
        name: "configuration_aliases",
        required: false,
        detail: "Additional provider alias names the module can reference",
    },
];

// --- `required_providers { | }` key suggestions ---------------------------

/// Common provider local names to pre-populate in `required_providers`
/// as starter scaffolds. These are *not* exhaustive — users can type
/// any local name they like — they're just the ones most commonly
/// reached for.
pub const REQUIRED_PROVIDERS_COMMON_ENTRIES: &[(&str, &str, &str)] = &[
    // (local_name, source, hint shown in detail)
    ("aws", "hashicorp/aws", "Amazon Web Services"),
    ("azurerm", "hashicorp/azurerm", "Microsoft Azure Resource Manager"),
    ("azuread", "hashicorp/azuread", "Microsoft Azure Active Directory / Entra ID"),
    ("azapi", "azure/azapi", "Azure Resource Manager direct API"),
    ("google", "hashicorp/google", "Google Cloud Platform"),
    ("google-beta", "hashicorp/google-beta", "GCP beta features"),
    ("kubernetes", "hashicorp/kubernetes", "Kubernetes resources"),
    ("helm", "hashicorp/helm", "Helm chart releases"),
    ("github", "integrations/github", "GitHub org / repo management"),
    ("gitlab", "gitlabhq/gitlab", "GitLab administration"),
    ("cloudflare", "cloudflare/cloudflare", "Cloudflare"),
    ("datadog", "DataDog/datadog", "Datadog monitoring"),
    ("docker", "kreuzwerker/docker", "Docker"),
    ("hetznercloud", "hetznercloud/hcloud", "Hetzner Cloud"),
    ("random", "hashicorp/random", "Random values for bootstrapping"),
    ("tls", "hashicorp/tls", "TLS key / cert generation"),
    ("null", "hashicorp/null", "null_resource for glue logic"),
    ("local", "hashicorp/local", "Local files and commands"),
    ("archive", "hashicorp/archive", "Zip / tar archives for deploy bundles"),
    ("http", "hashicorp/http", "HTTP data source"),
    ("external", "hashicorp/external", "Shell out to an external program"),
    ("time", "hashicorp/time", "Time-based resources + rotations"),
];

// --- `provider "x" { ... }` -----------------------------------------------
//
// Provider configuration blocks are mostly schema-driven (each provider
// declares its own config attributes), but the base meta-arguments apply
// to all providers.

pub const PROVIDER_BLOCK_META_ATTRS: &[BuiltinAttr] = &[
    BuiltinAttr {
        name: "alias",
        required: false,
        detail: "Named alias allowing multiple configurations of the same provider",
    },
    BuiltinAttr {
        name: "version",
        required: false,
        detail: "Deprecated — use `required_providers` in the terraform block instead",
    },
];

// --- `backend "name" { ... }` ---------------------------------------------

/// Schema for one remote-state backend by name.
pub fn backend_schema(name: &str) -> Option<BuiltinSchema> {
    match name {
        "local" => Some(LOCAL_BACKEND),
        "s3" => Some(S3_BACKEND),
        "gcs" => Some(GCS_BACKEND),
        "azurerm" => Some(AZURERM_BACKEND),
        "http" => Some(HTTP_BACKEND),
        "consul" => Some(CONSUL_BACKEND),
        "remote" => Some(REMOTE_BACKEND),
        "kubernetes" => Some(KUBERNETES_BACKEND),
        "pg" => Some(PG_BACKEND),
        _ => None,
    }
}

const LOCAL_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr {
            name: "path",
            required: false,
            detail: "State file path (default: terraform.tfstate)",
        },
        BuiltinAttr {
            name: "workspace_dir",
            required: false,
            detail: "Directory for non-default workspace state files",
        },
    ],
    blocks: &[],
};

const S3_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr { name: "bucket", required: true, detail: "S3 bucket name" },
        BuiltinAttr { name: "key", required: true, detail: "State object key within the bucket" },
        BuiltinAttr { name: "region", required: false, detail: "AWS region (falls back to AWS_REGION env)" },
        BuiltinAttr { name: "profile", required: false, detail: "Shared-credentials profile name" },
        BuiltinAttr { name: "shared_credentials_files", required: false, detail: "Paths to shared credentials files" },
        BuiltinAttr { name: "shared_config_files", required: false, detail: "Paths to shared config files" },
        BuiltinAttr { name: "endpoint", required: false, detail: "Custom S3 endpoint (deprecated; use endpoints.s3)" },
        BuiltinAttr { name: "encrypt", required: false, detail: "Enable server-side encryption of the state object" },
        BuiltinAttr { name: "kms_key_id", required: false, detail: "KMS key ARN for SSE-KMS" },
        BuiltinAttr { name: "dynamodb_table", required: false, detail: "DynamoDB table for state locking (deprecated; use use_lockfile)" },
        BuiltinAttr { name: "use_lockfile", required: false, detail: "Use an S3-native lockfile instead of DynamoDB (TF 1.10+)" },
        BuiltinAttr { name: "workspace_key_prefix", required: false, detail: "Prefix applied to non-default workspace keys" },
        BuiltinAttr { name: "role_arn", required: false, detail: "Role to assume for state access" },
        BuiltinAttr { name: "session_name", required: false, detail: "Session name used with role_arn" },
        BuiltinAttr { name: "external_id", required: false, detail: "External ID required by the assumed role" },
        BuiltinAttr { name: "skip_credentials_validation", required: false, detail: "Skip STS GetCallerIdentity" },
        BuiltinAttr { name: "skip_region_validation", required: false, detail: "Skip validation of the region name" },
        BuiltinAttr { name: "skip_metadata_api_check", required: false, detail: "Skip the EC2 metadata API credentials probe" },
        BuiltinAttr { name: "force_path_style", required: false, detail: "Use path-style S3 addressing (legacy)" },
        BuiltinAttr { name: "use_path_style", required: false, detail: "Use path-style S3 addressing (TF 1.6+ spelling)" },
    ],
    blocks: &[
        BuiltinBlock { name: "assume_role", detail: "Nested configuration for sts:AssumeRole" },
        BuiltinBlock { name: "endpoints", detail: "Per-service endpoint overrides (s3, dynamodb, iam, sts)" },
    ],
};

const GCS_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr { name: "bucket", required: true, detail: "GCS bucket name" },
        BuiltinAttr { name: "prefix", required: false, detail: "Prefix applied inside the bucket" },
        BuiltinAttr { name: "credentials", required: false, detail: "Path to a service-account JSON file" },
        BuiltinAttr { name: "impersonate_service_account", required: false, detail: "Service account to impersonate" },
        BuiltinAttr { name: "access_token", required: false, detail: "OAuth2 access token" },
        BuiltinAttr { name: "encryption_key", required: false, detail: "Base64 CSEK for customer-supplied encryption" },
        BuiltinAttr { name: "kms_encryption_key", required: false, detail: "Cloud KMS key for server-side encryption" },
    ],
    blocks: &[],
};

const AZURERM_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr { name: "storage_account_name", required: true, detail: "Storage account holding the state" },
        BuiltinAttr { name: "container_name", required: true, detail: "Blob container name" },
        BuiltinAttr { name: "key", required: true, detail: "Blob name for the state file" },
        BuiltinAttr { name: "resource_group_name", required: false, detail: "Resource group of the storage account" },
        BuiltinAttr { name: "subscription_id", required: false, detail: "Subscription ID" },
        BuiltinAttr { name: "tenant_id", required: false, detail: "Entra ID tenant" },
        BuiltinAttr { name: "client_id", required: false, detail: "Service-principal application ID" },
        BuiltinAttr { name: "client_secret", required: false, detail: "Service-principal secret (sensitive)" },
        BuiltinAttr { name: "use_msi", required: false, detail: "Authenticate via managed identity" },
        BuiltinAttr { name: "use_oidc", required: false, detail: "Authenticate via workload-identity / OIDC" },
        BuiltinAttr { name: "use_azuread_auth", required: false, detail: "Use Entra ID for blob auth (vs storage account key)" },
        BuiltinAttr { name: "environment", required: false, detail: "Azure cloud environment (public, usgovernment, …)" },
        BuiltinAttr { name: "snapshot", required: false, detail: "Maintain a blob snapshot after every apply" },
    ],
    blocks: &[],
};

const HTTP_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr { name: "address", required: true, detail: "URL for GET/POST of state" },
        BuiltinAttr { name: "update_method", required: false, detail: "HTTP method for updates (default POST)" },
        BuiltinAttr { name: "lock_address", required: false, detail: "URL for state locking" },
        BuiltinAttr { name: "lock_method", required: false, detail: "HTTP method for lock (default LOCK)" },
        BuiltinAttr { name: "unlock_address", required: false, detail: "URL for unlocking" },
        BuiltinAttr { name: "unlock_method", required: false, detail: "HTTP method for unlock (default UNLOCK)" },
        BuiltinAttr { name: "username", required: false, detail: "Basic auth username" },
        BuiltinAttr { name: "password", required: false, detail: "Basic auth password" },
        BuiltinAttr { name: "retry_max", required: false, detail: "Maximum retries on HTTP errors" },
        BuiltinAttr { name: "retry_wait_min", required: false, detail: "Minimum backoff between retries (seconds)" },
        BuiltinAttr { name: "retry_wait_max", required: false, detail: "Maximum backoff between retries (seconds)" },
        BuiltinAttr { name: "skip_cert_verification", required: false, detail: "Skip TLS verification (dangerous)" },
        BuiltinAttr { name: "client_ca_certificate_pem", required: false, detail: "CA bundle for mTLS" },
        BuiltinAttr { name: "client_certificate_pem", required: false, detail: "Client cert for mTLS" },
        BuiltinAttr { name: "client_private_key_pem", required: false, detail: "Client key for mTLS" },
    ],
    blocks: &[],
};

const CONSUL_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr { name: "path", required: true, detail: "Consul KV path for state" },
        BuiltinAttr { name: "address", required: false, detail: "Consul HTTP API address" },
        BuiltinAttr { name: "scheme", required: false, detail: "http or https" },
        BuiltinAttr { name: "datacenter", required: false, detail: "Consul datacenter" },
        BuiltinAttr { name: "access_token", required: false, detail: "Consul ACL token" },
        BuiltinAttr { name: "ca_file", required: false, detail: "CA bundle for TLS" },
        BuiltinAttr { name: "cert_file", required: false, detail: "Client cert for TLS" },
        BuiltinAttr { name: "key_file", required: false, detail: "Client key for TLS" },
        BuiltinAttr { name: "http_auth", required: false, detail: "Basic auth as user:pass" },
        BuiltinAttr { name: "gzip", required: false, detail: "Compress state in the KV store" },
        BuiltinAttr { name: "lock", required: false, detail: "Enable locking via Consul sessions" },
    ],
    blocks: &[],
};

const REMOTE_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr { name: "hostname", required: false, detail: "Hostname of HCP Terraform / TFE" },
        BuiltinAttr { name: "organization", required: true, detail: "Organization name" },
        BuiltinAttr { name: "token", required: false, detail: "API token (prefer TFE_TOKEN env)" },
    ],
    blocks: &[BuiltinBlock {
        name: "workspaces",
        detail: "Workspaces to bind to (name or prefix)",
    }],
};

const KUBERNETES_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr { name: "secret_suffix", required: true, detail: "Suffix on the state Secret's name" },
        BuiltinAttr { name: "labels", required: false, detail: "Additional labels on the state Secret" },
        BuiltinAttr { name: "namespace", required: false, detail: "Kubernetes namespace" },
        BuiltinAttr { name: "in_cluster_config", required: false, detail: "Use the pod service account" },
        BuiltinAttr { name: "load_config_file", required: false, detail: "Load a kubeconfig from disk" },
        BuiltinAttr { name: "config_path", required: false, detail: "Path to kubeconfig" },
        BuiltinAttr { name: "config_context", required: false, detail: "Kubeconfig context name" },
        BuiltinAttr { name: "host", required: false, detail: "Cluster API server URL" },
        BuiltinAttr { name: "token", required: false, detail: "Bearer token" },
        BuiltinAttr { name: "insecure", required: false, detail: "Skip TLS verification" },
    ],
    blocks: &[BuiltinBlock {
        name: "exec",
        detail: "Exec-based credential plugin configuration",
    }],
};

const PG_BACKEND: BuiltinSchema = BuiltinSchema {
    attrs: &[
        BuiltinAttr { name: "conn_str", required: true, detail: "Postgres connection string" },
        BuiltinAttr { name: "schema_name", required: false, detail: "Schema holding the state table" },
        BuiltinAttr { name: "skip_schema_creation", required: false, detail: "Assume the schema already exists" },
        BuiltinAttr { name: "skip_table_creation", required: false, detail: "Assume the state table already exists" },
        BuiltinAttr { name: "skip_index_creation", required: false, detail: "Assume the supporting index already exists" },
    ],
    blocks: &[],
};
