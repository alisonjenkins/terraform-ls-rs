//! Navigation + hover probe. Loads a workspace, recursively indexes
//! every child module referenced via `module.X { source = "..." }`,
//! then answers `goto-def` or `hover` at a given cursor position.
//!
//! Lets us exercise the LSP navigation pipeline end-to-end from the
//! shell without an editor — essential for reproducing module-related
//! navigation bugs and for TDD against real workspaces.
//!
//! Examples:
//!   tfls-nav-probe ~/stack ~/stack/main.tf:12:18 goto-def
//!   tfls-nav-probe ~/stack ~/stack/main.tf:12:18 hover

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, HoverContents, HoverParams, Position,
    TextDocumentIdentifier, TextDocumentPositionParams, Url, WorkDoneProgressParams,
};
use tfls_lsp::Backend;
use tfls_lsp::handlers::{navigation, util};
use tfls_lsp::indexer;
use tfls_state::{DocumentState, StateStore};
use tfls_walker::discover_terraform_files;
use tower_lsp::LspService;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Command {
    GotoDef,
    Hover,
}

#[derive(Debug, Parser)]
#[command(
    name = "tfls-nav-probe",
    about = "Probe goto-def / hover at a cursor position"
)]
struct Cli {
    /// Workspace / module directory. Every `.tf` underneath is
    /// indexed, plus every child module referenced via `source =
    /// "./…"`. Provider schemas are fetched from the nearest
    /// `.terraform/providers/` if present.
    dir: PathBuf,

    /// Cursor location as `path:line:col` (1-indexed, editor-style).
    /// `path` may be absolute or relative to `dir`.
    cursor: String,

    /// Which handler to invoke.
    #[arg(value_enum)]
    command: Command,

    /// Skip provider-schema fetch (faster, but resource-attribute
    /// hovers will be empty).
    #[arg(long)]
    no_schemas: bool,

    /// Increase logging verbosity.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    tfls_lsp::configure_rayon_pool();

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

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let dir = cli.dir.canonicalize()?;
    eprintln!("# workspace: {}", dir.display());

    // Parse cursor first so argument errors surface before the slow
    // indexing step.
    let (cursor_path, line, col) = parse_cursor(&cli.cursor, &dir)?;
    let cursor_uri = Url::from_file_path(&cursor_path)
        .map_err(|()| format!("cannot form file URI from {}", cursor_path.display()))?;
    let position = Position {
        line: line.saturating_sub(1),
        character: col.saturating_sub(1),
    };
    eprintln!(
        "# cursor: {}:{}:{}",
        cursor_path.display(),
        line,
        col
    );

    // Build Backend with a test client (same harness the integration
    // tests use). The client is never connected to a real socket —
    // only used as the handler-parameter channel.
    let (service, _socket) = LspService::new(Backend::new);
    let inner = service.inner();
    let backend = Backend::with_shared_state(
        inner.client.clone(),
        inner.state.clone(),
        inner.jobs.clone(),
    );

    // Discover + parse every .tf in the workspace, then walk module
    // sources transitively so child-module variables + outputs are
    // in the store before we call the handlers.
    let files = discover_terraform_files(&dir)?;
    eprintln!("# discovered {} workspace .tf / .tf.json files", files.len());
    parse_and_upsert(&backend.state, &files);
    index_child_modules_transitively(&backend.state, &dir);
    eprintln!("# total indexed documents: {}", backend.state.documents.len());

    // Functions are cheap + useful for hover_function (which dispatches
    // before symbol hover). Schemas drive attribute hover and
    // deprecation diagnostics — optional.
    let functions = tfls_schema::functions_cache::bundled()?;
    backend.state.install_functions(functions);

    if !cli.no_schemas {
        if let Some(init_root) = find_terraform_init_root(&dir) {
            eprintln!("# fetching schemas from {}", init_root.display());
            let tf_dir = init_root.join(".terraform");
            match tfls_provider_protocol::fetch_schemas_from_plugins(&tf_dir, None).await {
                Ok(schemas) => {
                    let n = schemas.provider_schemas.len();
                    backend.state.install_schemas(schemas);
                    eprintln!("# installed {n} provider schemas");
                }
                Err(e) => eprintln!("# WARNING: schema fetch failed: {e}"),
            }
        } else {
            eprintln!("# no .terraform/providers — skipping schema fetch");
        }
    }

    // Sanity: the cursor file must be in the store, otherwise the
    // handler returns None and the user is left guessing whether
    // that's a real "no target" or a harness miss.
    if !backend.state.documents.contains_key(&cursor_uri) {
        return Err(format!(
            "cursor file {} is not in the workspace index — was it discovered?",
            cursor_path.display()
        )
        .into());
    }
    print_cursor_context(&backend.state, &cursor_uri, position);

    match cli.command {
        Command::GotoDef => run_goto_def(&backend, cursor_uri, position).await?,
        Command::Hover => run_hover(&backend, cursor_uri, position).await?,
    }
    Ok(())
}

async fn run_goto_def(
    backend: &Backend,
    uri: Url,
    position: Position,
) -> Result<(), Box<dyn std::error::Error>> {
    let params = GotoDefinitionParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
        partial_result_params: Default::default(),
    };
    match navigation::goto_definition(backend, params).await {
        Ok(None) => println!("goto-def: <no target>"),
        Ok(Some(GotoDefinitionResponse::Scalar(loc))) => print_location("goto-def", &loc),
        Ok(Some(GotoDefinitionResponse::Array(locs))) => {
            println!("goto-def: {} targets", locs.len());
            for loc in locs {
                print_location("  ", &loc);
            }
        }
        Ok(Some(GotoDefinitionResponse::Link(links))) => {
            println!("goto-def: {} link targets", links.len());
            for link in links {
                println!(
                    "  {} @ {}:{} → {}:{}",
                    link.target_uri,
                    link.target_range.start.line + 1,
                    link.target_range.start.character + 1,
                    link.target_range.end.line + 1,
                    link.target_range.end.character + 1,
                );
            }
        }
        Err(e) => return Err(format!("goto-def failed: {e}").into()),
    }
    Ok(())
}

async fn run_hover(
    backend: &Backend,
    uri: Url,
    position: Position,
) -> Result<(), Box<dyn std::error::Error>> {
    let params = HoverParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: WorkDoneProgressParams::default(),
    };
    match navigation::hover(backend, params).await {
        Ok(None) => println!("hover: <no content>"),
        Ok(Some(h)) => match h.contents {
            HoverContents::Scalar(s) => println!("hover:\n{}", marked_to_string(&s)),
            HoverContents::Array(entries) => {
                println!("hover: {} entries", entries.len());
                for entry in entries {
                    println!("---\n{}", marked_to_string(&entry));
                }
            }
            HoverContents::Markup(m) => println!("hover:\n{}", m.value),
        },
        Err(e) => return Err(format!("hover failed: {e}").into()),
    }
    Ok(())
}

fn marked_to_string(m: &lsp_types::MarkedString) -> String {
    match m {
        lsp_types::MarkedString::String(s) => s.clone(),
        lsp_types::MarkedString::LanguageString(ls) => {
            format!("```{}\n{}\n```", ls.language, ls.value)
        }
    }
}

fn print_location(prefix: &str, loc: &lsp_types::Location) {
    let path = loc
        .uri
        .to_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|()| loc.uri.to_string());
    println!(
        "{prefix}: {}:{}:{} — {}:{}",
        path,
        loc.range.start.line + 1,
        loc.range.start.character + 1,
        loc.range.end.line + 1,
        loc.range.end.character + 1,
    );
}

fn print_cursor_context(state: &StateStore, uri: &Url, pos: Position) {
    let Some(doc) = state.documents.get(uri) else {
        return;
    };
    let line = doc.rope.get_line(pos.line as usize);
    if let Some(line) = line {
        let text = line.to_string();
        let trimmed = text.trim_end_matches('\n');
        eprintln!("# line {}: {trimmed}", pos.line + 1);
        let caret: String = " ".repeat(pos.character as usize) + "^";
        eprintln!("#         {caret}");
    }
}

fn parse_cursor(s: &str, dir: &Path) -> Result<(PathBuf, u32, u32), String> {
    let parts: Vec<&str> = s.rsplitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(format!(
            "bad cursor spec {s:?} — expected path:line:col"
        ));
    }
    let col: u32 = parts[0]
        .parse()
        .map_err(|e| format!("bad column in {s:?}: {e}"))?;
    let line: u32 = parts[1]
        .parse()
        .map_err(|e| format!("bad line in {s:?}: {e}"))?;
    let raw_path = PathBuf::from(parts[2]);
    let path = if raw_path.is_absolute() {
        raw_path
    } else {
        dir.join(raw_path)
    };
    let path = path
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize {}: {e}", path.display()))?;
    Ok((path, line, col))
}

fn parse_and_upsert(state: &StateStore, files: &[PathBuf]) {
    for path in files {
        let Ok(url) = Url::from_file_path(path) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        state.upsert_document(DocumentState::new(url, &text, 0));
    }
}

/// Walk every indexed document's `module_sources` map, resolve each
/// to a child directory, and index the child with
/// `index_module_dir_sync`. Repeat until no new directories appear —
/// child modules can reference their own sub-modules, so one pass
/// isn't enough.
fn index_child_modules_transitively(state: &StateStore, workspace_root: &Path) {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    loop {
        let mut newly_discovered: Vec<PathBuf> = Vec::new();
        for entry in state.documents.iter() {
            let Ok(doc_path) = entry.key().to_file_path() else {
                continue;
            };
            let Some(parent) = doc_path.parent() else {
                continue;
            };
            for (label, source) in &entry.value().symbols.module_sources {
                let Some(child) = util::resolve_module_source(parent, label, source) else {
                    continue;
                };
                // Canonicalize so two different spellings of the same
                // dir don't get indexed twice.
                let child = child.canonicalize().unwrap_or(child);
                if seen.insert(child.clone()) {
                    newly_discovered.push(child);
                }
            }
        }
        if newly_discovered.is_empty() {
            break;
        }
        for child in newly_discovered {
            // Skip scanning directories that already live under
            // the workspace root — those are covered by the
            // initial `discover_terraform_files` pass.
            if child.starts_with(workspace_root) {
                continue;
            }
            indexer::index_module_dir_sync(state, &child);
        }
    }
}

fn find_terraform_init_root(start: &Path) -> Option<PathBuf> {
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        if dir.join(".terraform").join("providers").is_dir() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::{EnvFilter, fmt};
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
