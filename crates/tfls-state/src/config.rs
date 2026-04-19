//! Runtime configuration, updated via
//! `workspace/didChangeConfiguration`.

use std::sync::RwLock;
use std::time::Duration;

/// All tunable runtime settings in one place. Clone is cheap —
/// settings are scalars or short strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Max wall-clock time for an invocation of
    /// `tofu providers schema -json` or `tofu metadata functions -json`.
    pub cli_timeout: Duration,
    /// Debounce period for the notify-based file watcher.
    pub watch_debounce: Duration,
    /// Whether the server should even try to call the CLI. If
    /// disabled, schemas and functions always come from the bundled
    /// fallback.
    pub cli_enabled: bool,
    /// CLI binary name to resolve from PATH (default: "tofu").
    pub cli_binary: String,
    /// Number of days past which an exact-pinned version is flagged
    /// stale by the inlay-hint formatter. Default 180 (~6 months).
    pub stale_version_days: u32,
    /// Enable the opt-in tflint "style pack" rules:
    /// `terraform_documented_variables`, `terraform_documented_outputs`,
    /// `terraform_naming_convention`, `terraform_comment_syntax`.
    /// Default `false` — matches tflint's own recommended preset
    /// (these rules live in the `all` preset only).
    pub style_rules: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cli_timeout: Duration::from_secs(60),
            watch_debounce: Duration::from_millis(150),
            cli_enabled: true,
            cli_binary: "tofu".to_string(),
            stale_version_days: 180,
            style_rules: false,
        }
    }
}

/// Wrapper around `RwLock<Config>` with copy-on-read semantics. Reads
/// are cheap (clone a small struct); writes are rare.
#[derive(Debug, Default)]
pub struct ConfigCell {
    inner: RwLock<Config>,
}

impl ConfigCell {
    pub fn new(config: Config) -> Self {
        Self {
            inner: RwLock::new(config),
        }
    }

    /// Snapshot the config. Clones the small struct so callers can
    /// drop the lock immediately.
    pub fn snapshot(&self) -> Config {
        match self.inner.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Apply a partial update from the client's settings JSON. Silent
    /// on unrecognised keys — keeps us forward-compatible.
    pub fn update_from_json(&self, value: &sonic_rs::Value) {
        use sonic_rs::JsonValueTrait;

        let Ok(mut guard) = self.inner.write() else {
            return;
        };

        // Accept both `{"terraform-ls-rs": {...}}` and flat objects.
        let obj: &sonic_rs::Value =
            value.get("terraform-ls-rs").unwrap_or(value);

        if let Some(v) = obj.get("cliEnabled").and_then(|v| v.as_bool()) {
            guard.cli_enabled = v;
        }
        if let Some(v) = obj.get("cliBinary").and_then(|v| v.as_str()) {
            guard.cli_binary = v.to_string();
        }
        if let Some(v) = obj.get("cliTimeoutSecs").and_then(|v| v.as_u64()) {
            guard.cli_timeout = Duration::from_secs(v);
        }
        if let Some(v) = obj.get("watchDebounceMs").and_then(|v| v.as_u64()) {
            guard.watch_debounce = Duration::from_millis(v);
        }
        if let Some(v) = obj.get("staleVersionDays").and_then(|v| v.as_u64()) {
            // Clamp to u32 range. 0 means "never flag as stale".
            guard.stale_version_days = v.try_into().unwrap_or(u32::MAX);
        }
        if let Some(v) = obj.get("styleRules").and_then(|v| v.as_bool()) {
            guard.style_rules = v;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let c = Config::default();
        assert_eq!(c.cli_timeout, Duration::from_secs(60));
        assert_eq!(c.cli_binary, "tofu");
        assert!(c.cli_enabled);
    }

    #[test]
    fn update_applies_known_keys() {
        let cell = ConfigCell::default();
        let value: sonic_rs::Value = sonic_rs::from_str(
            r#"{"terraform-ls-rs": {
                "cliEnabled": false,
                "cliBinary": "terraform",
                "cliTimeoutSecs": 30,
                "watchDebounceMs": 250
            }}"#,
        )
        .expect("parse");
        cell.update_from_json(&value);
        let snap = cell.snapshot();
        assert!(!snap.cli_enabled);
        assert_eq!(snap.cli_binary, "terraform");
        assert_eq!(snap.cli_timeout, Duration::from_secs(30));
        assert_eq!(snap.watch_debounce, Duration::from_millis(250));
    }

    #[test]
    fn update_accepts_flat_object_without_prefix() {
        let cell = ConfigCell::default();
        let value: sonic_rs::Value =
            sonic_rs::from_str(r#"{"cliTimeoutSecs": 5}"#).expect("parse");
        cell.update_from_json(&value);
        assert_eq!(cell.snapshot().cli_timeout, Duration::from_secs(5));
    }

    #[test]
    fn update_ignores_unknown_keys() {
        let cell = ConfigCell::default();
        let value: sonic_rs::Value =
            sonic_rs::from_str(r#"{"madeUpSetting": 42}"#).expect("parse");
        cell.update_from_json(&value);
        assert_eq!(cell.snapshot(), Config::default());
    }
}

