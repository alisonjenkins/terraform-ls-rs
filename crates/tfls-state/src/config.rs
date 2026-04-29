//! Runtime configuration, updated via
//! `workspace/didChangeConfiguration`.

use std::sync::RwLock;
use std::time::Duration;

/// Formatting style. Mirrors `tf_format::FormatStyle`; defined
/// here so the LSP-facing config layer doesn't have to depend on
/// the formatting crate. `tfls-format` maps this to the backend
/// enum at the call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FormatStyle {
    /// `terraform fmt` / `tofu fmt` parity: alignment + spacing
    /// only. The default — safe to apply to any repo.
    #[default]
    Minimal,
    /// Full opinionated formatting (alphabetisation, hoisting,
    /// expansion). Opt-in via the `formatStyle` config key.
    Opinionated,
}

impl FormatStyle {
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s {
            "minimal" | "Minimal" => Some(Self::Minimal),
            "opinionated" | "Opinionated" => Some(Self::Opinionated),
            _ => None,
        }
    }

    /// Stable single-byte tag used as a cache-invalidation key
    /// (e.g. `DocumentState::format_cache`). Two enum variants
    /// → 0 / 1; adding a new variant requires choosing a fresh
    /// tag here so existing cached entries don't collide.
    pub fn marker(self) -> u8 {
        match self {
            FormatStyle::Minimal => 0,
            FormatStyle::Opinionated => 1,
        }
    }
}

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
    /// Active formatting style. Updated live via
    /// `workspace/didChangeConfiguration` (key: `formatStyle`).
    /// Default [`FormatStyle::Minimal`].
    pub format_style: FormatStyle,
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
            format_style: FormatStyle::default(),
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
        if let Some(v) = obj.get("formatStyle").and_then(|v| v.as_str()) {
            if let Some(style) = FormatStyle::from_str_lossy(v) {
                guard.format_style = style;
            } else {
                tracing::warn!(value = v, "unknown formatStyle value — keeping previous");
            }
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

    #[test]
    fn format_style_round_trip() {
        let cell = ConfigCell::default();
        assert_eq!(cell.snapshot().format_style, FormatStyle::Minimal);

        let v: sonic_rs::Value =
            sonic_rs::from_str(r#"{"formatStyle":"opinionated"}"#).expect("parse");
        cell.update_from_json(&v);
        assert_eq!(cell.snapshot().format_style, FormatStyle::Opinionated);

        let v: sonic_rs::Value =
            sonic_rs::from_str(r#"{"formatStyle":"minimal"}"#).expect("parse");
        cell.update_from_json(&v);
        assert_eq!(cell.snapshot().format_style, FormatStyle::Minimal);
    }

    #[test]
    fn format_style_unknown_value_keeps_previous() {
        let cell = ConfigCell::default();
        let v: sonic_rs::Value =
            sonic_rs::from_str(r#"{"formatStyle":"opinionated"}"#).expect("parse");
        cell.update_from_json(&v);
        let v: sonic_rs::Value =
            sonic_rs::from_str(r#"{"formatStyle":"banana"}"#).expect("parse");
        cell.update_from_json(&v);
        assert_eq!(cell.snapshot().format_style, FormatStyle::Opinionated);
    }
}

