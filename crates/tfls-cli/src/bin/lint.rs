//! `tfls-lint` — CI-facing linter over `tfls-engine`'s diagnostics
//! pipeline. Loads one or more workspace roots independently, runs
//! the same diagnostics every `did_open` would publish, prints them
//! as text, and exits non-zero when findings meet the configured
//! severity threshold. No transport, no LSP client — safe to drop
//! straight into a CI job.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};
use tfls_engine::workspace::{lint_all, load, LoadError, LoadOptions, SchemaOutcome, SchemaSource};
use url::Url;

#[derive(Debug, Parser)]
#[command(
    name = "tfls-lint",
    about = "Lint Terraform workspaces in CI using the tfls diagnostics engine"
)]
struct Cli {
    /// Workspace root(s) to lint. Each is loaded and indexed
    /// independently; results are concatenated and sorted by path.
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Where to get provider schemas from.
    #[arg(long, value_enum, default_value_t = SchemaSourceArg::Plugins)]
    schemas: SchemaSourceArg,

    /// Minimum severity that trips a non-zero exit. `never` always
    /// exits 0 regardless of findings.
    #[arg(long, value_enum, default_value_t = FailOn::Error)]
    fail_on: FailOn,

    /// Override a rule's severity: `<CODE>=<off|hint|info|warning|error>`.
    /// Repeatable.
    #[arg(long = "rule", value_name = "CODE=LEVEL")]
    rules: Vec<RuleOverrideArg>,

    /// Enable the opt-in tflint-style rule pack (documented-variables,
    /// documented-outputs, naming-convention, comment-syntax).
    #[arg(long)]
    style_rules: bool,

    /// Number of rayon worker threads for per-document diagnostic
    /// compute. Defaults to the engine's own sizing
    /// (`available_parallelism` minus headroom for tokio).
    #[arg(long)]
    jobs: Option<usize>,

    /// Suppress the per-file diagnostic listing; print only the
    /// summary line.
    #[arg(short, long)]
    quiet: bool,

    /// Increase logging verbosity (`-v` = debug, `-vv` = trace). Also
    /// enables the schema-outcome `#` line on stderr.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Print reported paths relative to this directory instead of
    /// each workspace root.
    #[arg(long)]
    relative_to: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SchemaSourceArg {
    Plugins,
    Bundled,
    None,
}

impl From<SchemaSourceArg> for SchemaSource {
    fn from(value: SchemaSourceArg) -> Self {
        match value {
            SchemaSourceArg::Plugins => SchemaSource::Plugins,
            SchemaSourceArg::Bundled => SchemaSource::Bundled,
            SchemaSourceArg::None => SchemaSource::None,
        }
    }
}

/// Severity threshold for `--fail-on`. Ordered loosest-to-strictest
/// so `exceeds` is a single ordinal comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum FailOn {
    Hint,
    Info,
    Warning,
    Error,
    /// Never trips regardless of findings — kept out of the ordinal
    /// chain below via an explicit early return in `exceeds`.
    Never,
}

#[derive(Debug, Clone)]
struct RuleOverrideArg {
    code: String,
    level: String,
}

impl std::str::FromStr for RuleOverrideArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (code, level) = s
            .split_once('=')
            .ok_or_else(|| format!("expected <CODE>=<LEVEL>, got '{s}'"))?;
        if code.is_empty() || level.is_empty() {
            return Err(format!("expected <CODE>=<LEVEL>, got '{s}'"));
        }
        Ok(RuleOverrideArg {
            code: code.to_string(),
            level: level.to_string(),
        })
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if let Some(n) = cli.jobs {
        // SAFETY-relevant only in the "already running" sense: this
        // runs before any rayon use, so `configure_rayon_pool` below
        // still gets to build the global pool with this override.
        std::env::set_var("TFLS_RAYON_THREADS", n.to_string());
    }
    tfls_lsp::configure_rayon_pool();

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to build tokio runtime: {e}");
            return ExitCode::from(2);
        }
    };

    rt.block_on(run(cli))
}

async fn run(cli: Cli) -> ExitCode {
    let config_json = build_config_json(&cli.rules, cli.style_rules);

    let mut all_entries: Vec<(String, Diagnostic)> = Vec::new();
    let mut counts = SeverityCounts::default();
    let mut files_with_diagnostics: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    for path in &cli.paths {
        let opts = LoadOptions {
            schemas: cli.schemas.into(),
        };
        let loaded = match load(path, &opts).await {
            Ok(loaded) => loaded,
            Err(e) => {
                print_error_chain(&e);
                return ExitCode::from(2);
            }
        };

        loaded.state.config.update_from_json(&config_json);

        report_schema_outcome(&loaded.schema_outcome, path, cli.verbose);

        let root = match path.canonicalize() {
            Ok(root) => root,
            Err(e) => {
                eprintln!(
                    "error: failed to canonicalise workspace root '{}': {e}",
                    path.display()
                );
                return ExitCode::from(2);
            }
        };
        let relative_to = cli.relative_to.as_deref().unwrap_or(&root);

        for (uri, diags) in lint_all(&loaded.state) {
            if diags.is_empty() {
                continue;
            }
            let rel = relative_path(&uri, relative_to);
            files_with_diagnostics.insert(rel.clone());
            for d in diags {
                counts.tick(&d);
                all_entries.push((rel.clone(), d));
            }
        }
    }

    all_entries.sort_by(|(path_a, a), (path_b, b)| {
        path_a
            .cmp(path_b)
            .then(a.range.start.line.cmp(&b.range.start.line))
            .then(a.range.start.character.cmp(&b.range.start.character))
            .then(code_of(a).cmp(&code_of(b)))
    });

    if !cli.quiet {
        for (path, d) in &all_entries {
            println!(
                "{path}:{}:{}: {} [{}] {}",
                d.range.start.line + 1,
                d.range.start.character + 1,
                severity_word(d),
                code_of(d),
                d.message,
            );
        }
    }

    eprintln!(
        "{} error(s), {} warning(s), {} info, {} hint(s) in {} file(s)",
        counts.error,
        counts.warning,
        counts.info,
        counts.hint,
        files_with_diagnostics.len(),
    );

    let diags_only: Vec<Diagnostic> = all_entries.into_iter().map(|(_, d)| d).collect();
    if exceeds(&diags_only, cli.fail_on) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// Pure threshold decision, unit-tested for every `FailOn` level
/// including the `Never`/boundary cases.
fn exceeds(diags: &[Diagnostic], fail_on: FailOn) -> bool {
    if fail_on == FailOn::Never {
        return false;
    }
    diags.iter().any(|d| severity_rank(d) >= fail_on)
}

/// Maps an LSP severity (loosest = `Hint`, missing = treated as
/// `Error` since an unset severity should never be silently ignored)
/// onto the same ordinal scale as `FailOn`'s non-`Never` variants.
fn severity_rank(d: &Diagnostic) -> FailOn {
    match d.severity {
        Some(DiagnosticSeverity::HINT) => FailOn::Hint,
        Some(DiagnosticSeverity::INFORMATION) => FailOn::Info,
        Some(DiagnosticSeverity::WARNING) => FailOn::Warning,
        Some(DiagnosticSeverity::ERROR) | None => FailOn::Error,
        Some(_) => FailOn::Error,
    }
}

fn severity_word(d: &Diagnostic) -> &'static str {
    match d.severity {
        Some(DiagnosticSeverity::HINT) => "hint",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        Some(DiagnosticSeverity::WARNING) => "warning",
        _ => "error",
    }
}

fn code_of(d: &Diagnostic) -> String {
    match &d.code {
        Some(NumberOrString::String(s)) => s.clone(),
        Some(NumberOrString::Number(n)) => n.to_string(),
        None => String::new(),
    }
}

#[derive(Default)]
struct SeverityCounts {
    error: usize,
    warning: usize,
    info: usize,
    hint: usize,
}

impl SeverityCounts {
    fn tick(&mut self, d: &Diagnostic) {
        match severity_rank(d) {
            FailOn::Error => self.error += 1,
            FailOn::Warning => self.warning += 1,
            FailOn::Info => self.info += 1,
            FailOn::Hint | FailOn::Never => self.hint += 1,
        }
    }
}

/// Builds the same JSON shape the LSP `initializationOptions` /
/// `workspace/didChangeConfiguration` accept (see CLAUDE.md's
/// "Per-rule diagnostic config"), so `--rule` and `--style-rules`
/// reuse `Config::update_from_json` verbatim.
fn build_config_json(rules: &[RuleOverrideArg], style_rules: bool) -> sonic_rs::Value {
    let mut rules_obj = sonic_rs::json!({});
    for r in rules {
        let _ = rules_obj.insert(&r.code, sonic_rs::json!(r.level.clone()));
    }
    sonic_rs::json!({
        "rules": rules_obj,
        "styleRules": style_rules,
    })
}

fn report_schema_outcome(outcome: &SchemaOutcome, root: &Path, verbose: u8) {
    if let SchemaOutcome::FetchFailed { init_root, message } = outcome {
        eprintln!(
            "warning: schema fetch failed for {}: {message} (init root {})",
            root.display(),
            init_root.display()
        );
        return;
    }
    if verbose == 0 {
        return;
    }
    match outcome {
        SchemaOutcome::Fetched { init_root, count } => {
            eprintln!(
                "# {}: fetched {count} provider schema(s) from {}",
                root.display(),
                init_root.display()
            );
        }
        SchemaOutcome::NoInitRoot => {
            eprintln!(
                "# {}: no .terraform/providers found — schema-driven diagnostics skipped",
                root.display()
            );
        }
        SchemaOutcome::Bundled => {
            eprintln!(
                "# {}: using bundled terraform provider schema only",
                root.display()
            );
        }
        SchemaOutcome::Skipped => {
            eprintln!(
                "# {}: schema fetch skipped (--schemas none)",
                root.display()
            );
        }
        SchemaOutcome::FetchFailed { .. } => unreachable!("handled above"),
    }
}

fn print_error_chain(err: &LoadError) {
    eprint!("error: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(s) = source {
        eprint!(": {s}");
        source = s.source();
    }
    eprintln!();
}

fn relative_path(uri: &Url, root: &Path) -> String {
    let Ok(path) = uri.to_file_path() else {
        return uri.to_string();
    };
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let level = match verbosity {
            0 => "warn",
            1 => "info,tfls=debug",
            _ => "debug,tfls=trace",
        };
        EnvFilter::new(level)
    });

    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn diag(severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            severity: Some(severity),
            code: None,
            code_description: None,
            source: None,
            message: "test".to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[test]
    fn never_never_trips_regardless_of_findings() {
        let diags = vec![diag(DiagnosticSeverity::ERROR)];
        assert!(!exceeds(&diags, FailOn::Never));
    }

    #[test]
    fn empty_diagnostics_never_trips() {
        assert!(!exceeds(&[], FailOn::Error));
        assert!(!exceeds(&[], FailOn::Hint));
    }

    #[test]
    fn warning_present_fail_on_error_does_not_trip() {
        let diags = vec![diag(DiagnosticSeverity::WARNING)];
        assert!(!exceeds(&diags, FailOn::Error));
    }

    #[test]
    fn error_present_fail_on_error_trips() {
        let diags = vec![diag(DiagnosticSeverity::ERROR)];
        assert!(exceeds(&diags, FailOn::Error));
    }

    #[test]
    fn warning_present_fail_on_warning_trips() {
        let diags = vec![diag(DiagnosticSeverity::WARNING)];
        assert!(exceeds(&diags, FailOn::Warning));
    }

    #[test]
    fn info_present_fail_on_warning_does_not_trip() {
        let diags = vec![diag(DiagnosticSeverity::INFORMATION)];
        assert!(!exceeds(&diags, FailOn::Warning));
    }

    #[test]
    fn info_present_fail_on_info_trips() {
        let diags = vec![diag(DiagnosticSeverity::INFORMATION)];
        assert!(exceeds(&diags, FailOn::Info));
    }

    #[test]
    fn hint_present_fail_on_hint_trips() {
        let diags = vec![diag(DiagnosticSeverity::HINT)];
        assert!(exceeds(&diags, FailOn::Hint));
    }

    #[test]
    fn hint_present_fail_on_info_does_not_trip() {
        let diags = vec![diag(DiagnosticSeverity::HINT)];
        assert!(!exceeds(&diags, FailOn::Info));
    }

    #[test]
    fn rule_override_arg_parses_code_and_level() {
        let parsed: RuleOverrideArg = "terraform_fmt=off".parse().expect("should parse");
        assert_eq!(parsed.code, "terraform_fmt");
        assert_eq!(parsed.level, "off");
    }

    #[test]
    fn rule_override_arg_rejects_missing_equals() {
        let parsed: Result<RuleOverrideArg, String> = "terraform_fmt".parse();
        assert!(parsed.is_err());
    }
}
