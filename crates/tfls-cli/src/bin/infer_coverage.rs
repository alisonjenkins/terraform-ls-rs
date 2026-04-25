//! Variable-type inference coverage probe.
//!
//! Walks a workspace, indexes every `.tf` (including
//! `.terraform/modules/*` so external module outputs resolve),
//! optionally fetches provider schemas, runs
//! `rebuild_assigned_variable_types_for_dir` on every dir, and
//! reports per-variable inference status:
//!
//! - **match** — declared `type = …` agrees with the inferred shape.
//! - **mismatch** — both sides resolved, but disagree (often a real
//!   discrepancy in the user's HCL — e.g. caller passes a
//!   tuple-of-objects where the variable declared `list(map(string))`).
//! - **no-decl-inferred** — variable has no `type =` but inference
//!   would suggest one (`Set variable type` quick-fix targets these).
//! - **no-decl-no-inf** — variable has neither a `type =` nor any
//!   inferable signal. Code-action skips.
//! - **no-inference** — declared `type =` but no caller / default
//!   provides a signal. Often "dead" modules with no callers in
//!   the workspace.
//!
//! Used as a bug-hunting harness when investigating gaps in the
//! type-inference code-action quick-fix and the schema-aware shape
//! resolution. Output is the source of truth for the percentage
//! figures cited in commit messages and CLAUDE.md.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use hcl_edit::expr::Expression;
use lsp_types::Url;
use tfls_core::variable_type::{VariableType, parse_type_expr};
use tfls_state::{DocumentState, StateStore};
use tfls_walker::discover_terraform_files;

#[derive(Debug, Parser)]
#[command(
    name = "tfls-infer-coverage",
    about = "Report variable-type inference coverage across a workspace"
)]
struct Cli {
    /// Workspace / module directory to analyse.
    dir: PathBuf,

    /// Skip provider-schema fetch (faster, but resource / data-source
    /// attribute traversals stay as `Any`, slashing coverage).
    #[arg(long)]
    no_schemas: bool,

    /// List every variable that lacks inference (declared but no
    /// caller / default signal), with the caller expression kind
    /// when there is one. Useful for diagnosing gaps.
    #[arg(long)]
    list_gaps: bool,

    /// Dump the staged `assigned_variable_types` map for the named
    /// dir (relative to the workspace root, or absolute). Skips
    /// the coverage report.
    #[arg(long)]
    dump_dir: Option<PathBuf>,

    /// Increase logging verbosity (`-v` = debug, `-vv` = trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let workspace = cli
        .dir
        .canonicalize()
        .map_err(|e| format!("canonicalize {:?}: {e}", cli.dir))?;

    let state = StateStore::new();
    let mut files =
        discover_terraform_files(&workspace).map_err(|e| format!("discover: {e}"))?;
    let modules_root = workspace.join(".terraform/modules");
    if modules_root.is_dir() {
        for entry in std::fs::read_dir(&modules_root).map_err(|e| format!("read_dir: {e}"))? {
            let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            files.extend(walk_tf(&p).map_err(|e| format!("walk_tf {p:?}: {e}"))?);
        }
    }
    for path in &files {
        let Some(url) = path_to_url(path) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        state.upsert_document(DocumentState::new(url, &text, 0));
    }

    if !cli.no_schemas {
        let terraform_dir = workspace.join(".terraform");
        if terraform_dir.is_dir() {
            match tfls_provider_protocol::fetch_schemas_from_plugins_raw(&terraform_dir, None)
                .await
            {
                Ok(raw) => {
                    let count = raw.schemas.provider_schemas.len();
                    eprintln!("loaded {count} provider schemas");
                    state.install_schemas(raw.schemas);
                }
                Err(e) => eprintln!("schema fetch failed: {e} (continuing without schemas)"),
            }
        }
    }

    let mut dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for f in &files {
        if let Some(p) = f.parent() {
            dirs.insert(p.to_path_buf());
        }
    }
    for dir in &dirs {
        tfls_lsp::indexer::rebuild_assigned_variable_types_for_dir(&state, dir);
    }

    if let Some(d) = cli.dump_dir {
        let target = if d.is_absolute() { d } else { workspace.join(&d) };
        return dump_dir(&state, &target);
    }

    report(&workspace, &state, cli.list_gaps);
    Ok(())
}

fn dump_dir(state: &StateStore, target: &Path) -> Result<(), String> {
    println!("=== assigned_variable_types[{}] ===", target.display());
    match state.assigned_variable_types.get(target) {
        Some(entry) => {
            for (k, v) in entry.iter() {
                println!("  {k}: {v:?}");
            }
        }
        None => println!("  (not in map)"),
    }
    Ok(())
}

fn report(workspace: &Path, state: &StateStore, list_gaps: bool) {
    let mut tally: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut gaps: Vec<GapRow> = Vec::new();

    for entry in state.documents.iter() {
        let Ok(path) = entry.key().to_file_path() else {
            continue;
        };
        // Skip docs from `.terraform/` — those are external module
        // sources, not user-authored variables we're trying to
        // infer types for.
        let pstr = path.to_string_lossy();
        if pstr.contains("/.terraform/") {
            continue;
        }
        let Some(dir) = path.parent() else {
            continue;
        };
        let dir = dir.to_path_buf();
        let doc = entry.value();
        let Some(body) = doc.parsed.body.as_ref() else {
            continue;
        };
        for s in body.iter() {
            let Some(block) = s.as_block() else { continue };
            if block.ident.as_str() != "variable" {
                continue;
            }
            let label = match block.labels.first() {
                Some(hcl_edit::structure::BlockLabel::String(s)) => s.value().to_string(),
                Some(hcl_edit::structure::BlockLabel::Ident(i)) => i.as_str().to_string(),
                None => continue,
            };
            let declared: Option<VariableType> = block.body.iter().find_map(|s| {
                let attr = s.as_attribute()?;
                if attr.key.as_str() != "type" {
                    return None;
                }
                Some(parse_type_expr(&attr.value))
            });
            let from_default = doc
                .symbols
                .variable_defaults
                .get(&label)
                .filter(|t| !matches!(t, VariableType::Any))
                .filter(|t| match t {
                    VariableType::Tuple(items) => !items.is_empty(),
                    VariableType::Object(fields) => !fields.is_empty(),
                    _ => true,
                })
                .cloned();
            let from_assigned = state.merged_assigned_type(&dir, &label);
            let inferred = from_default.clone().or(from_assigned.clone());
            let key = match (declared.as_ref(), inferred.as_ref()) {
                (None, None) => "no-decl-no-inf",
                (None, Some(_)) => "no-decl-inferred",
                (Some(_), None) => "no-inference",
                (Some(d), Some(i)) => {
                    if equivalent(d, i) {
                        "match"
                    } else {
                        "mismatch"
                    }
                }
            };
            *tally.entry(key).or_insert(0) += 1;

            if list_gaps && key == "no-inference" {
                gaps.push(GapRow {
                    file: path
                        .strip_prefix(workspace)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                    name: label.clone(),
                    callers: classify_callers(state, &dir, &label),
                });
            }
        }
    }

    println!("\n=== inference coverage ===");
    let total: usize = tally.values().sum();
    for (k, v) in &tally {
        let pct = (*v as f64 / total.max(1) as f64) * 100.0;
        println!("  {k:<20} {v:>4}  ({pct:.1}%)");
    }
    println!("  {:<20} {total}", "TOTAL");

    if list_gaps && !gaps.is_empty() {
        println!("\n=== no-inference gaps ===");
        for g in &gaps {
            let label = if g.callers.is_empty() {
                "[no-caller]".to_string()
            } else {
                format!("{:?}", g.callers)
            };
            println!("  {} :: {}  {}", g.file, g.name, label);
        }
    }
}

struct GapRow {
    file: String,
    name: String,
    callers: Vec<String>,
}

fn classify_callers(state: &StateStore, target_dir: &Path, var_name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let _ = BTreeSet::<String>::new();
    for c_entry in state.documents.iter() {
        let Ok(c_path) = c_entry.key().to_file_path() else {
            continue;
        };
        let Some(c_dir) = c_path.parent() else {
            continue;
        };
        let c_doc = c_entry.value();
        let Some(c_body) = c_doc.parsed.body.as_ref() else {
            continue;
        };
        for cs in c_body.iter() {
            let Some(c_block) = cs.as_block() else { continue };
            if c_block.ident.as_str() != "module" {
                continue;
            }
            let Some(c_label) = c_block.labels.first() else {
                continue;
            };
            let c_lbl = match c_label {
                hcl_edit::structure::BlockLabel::String(s) => s.value().to_string(),
                hcl_edit::structure::BlockLabel::Ident(i) => i.as_str().to_string(),
            };
            let Some(source) = c_doc.symbols.module_sources.get(&c_lbl) else {
                continue;
            };
            let Some(child_dir) =
                tfls_lsp::handlers::util::resolve_module_source(c_dir, &c_lbl, source)
            else {
                continue;
            };
            if child_dir != target_dir {
                continue;
            }
            for cb in c_block.body.iter() {
                let Some(attr) = cb.as_attribute() else { continue };
                if attr.key.as_str() != var_name {
                    continue;
                }
                out.push(classify_expr(&attr.value));
            }
        }
    }
    out
}

fn classify_expr(expr: &Expression) -> String {
    match expr {
        Expression::String(_) => "literal-string".into(),
        Expression::Number(_) => "literal-number".into(),
        Expression::Bool(_) => "literal-bool".into(),
        Expression::Array(arr) => {
            let count = arr.iter().count();
            if count == 0 {
                "empty-array".into()
            } else {
                format!("array[{count}]")
            }
        }
        Expression::Object(_) => "literal-object".into(),
        Expression::Variable(_) => "bare-variable".into(),
        Expression::Traversal(t) => match &t.expr {
            Expression::Variable(v) => match v.as_str() {
                "var" => "var.x".into(),
                "local" => "local.x".into(),
                "data" => "data.x.y.z".into(),
                "module" => "module.x.y".into(),
                "each" => "each.x".into(),
                "count" => "count.x".into(),
                name if name.contains('_') => format!("res:{name}"),
                _ => "other-traversal".into(),
            },
            _ => "complex-traversal".into(),
        },
        Expression::FuncCall(f) => format!("fn:{}", f.name.name.as_str()),
        Expression::Conditional(_) => "conditional".into(),
        Expression::ForExpr(_) => "for-expr".into(),
        Expression::Parenthesis(_) => "parenthesis".into(),
        Expression::HeredocTemplate(_) => "heredoc".into(),
        Expression::StringTemplate(_) => "string-template".into(),
        _ => "other".into(),
    }
}

fn equivalent(a: &VariableType, b: &VariableType) -> bool {
    use VariableType::*;
    match (a, b) {
        (Any, _) | (_, Any) => true,
        (Primitive(x), Primitive(y)) => x == y,
        (List(x), List(y)) | (Set(x), Set(y)) | (Map(x), Map(y)) => equivalent(x, y),
        (List(inner), Tuple(items)) | (Set(inner), Tuple(items)) => {
            items.iter().all(|t| equivalent(inner, t))
        }
        (Tuple(xs), Tuple(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| equivalent(x, y))
        }
        (Object(xs), Object(ys)) => {
            xs.len() == ys.len()
                && xs.iter().all(|(k, vx)| ys.get(k).is_some_and(|vy| equivalent(vx, vy)))
        }
        _ => false,
    }
}

fn walk_tf(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d)?;
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if matches!(name, ".git" | "examples" | "test" | "tests") {
                        continue;
                    }
                }
                stack.push(p);
            } else if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.ends_with(".tf") || name.ends_with(".tf.json") {
                    out.push(p);
                }
            }
        }
    }
    Ok(out)
}

fn path_to_url(path: &Path) -> Option<Url> {
    Url::from_file_path(path).ok()
}

fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
