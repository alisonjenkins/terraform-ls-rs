//! Format-then-diagnostics probe.
//!
//! Reproduces the user-reported bug where diagnostics rendered on
//! the wrong line after running an opinionated reformat that
//! reorders blocks. Drives a fresh `tfls` session (direct or via
//! `lspmux`) through:
//!
//!  1. Open a synthetic file containing several diagnosable issues
//!     (unused variables / locals, an unknown attribute on a known
//!     resource), with blocks deliberately ordered so the
//!     opinionated formatter rearranges them.
//!  2. Drain initial `textDocument/publishDiagnostics` and assert
//!     every diagnostic's range points at a line whose content
//!     actually corresponds to the diagnostic's message.
//!  3. Set `formatStyle = opinionated` via
//!     `workspace/didChangeConfiguration`.
//!  4. Send `textDocument/formatting`. Apply returned `TextEdit`s
//!     locally (mirrors what an LSP client does).
//!  5. Send `textDocument/didChange` with the new whole-document
//!     text.
//!  6. Drain post-format `textDocument/publishDiagnostics` and
//!     assert each diagnostic's range still points at the right
//!     line in the NEW (reformatted) buffer.
//!
//! The unit test
//! `tfls-lsp/tests/phase4.rs::opinionated_format_then_diagnostics_align_to_new_buffer`
//! covers the in-process flow. This probe exercises the same
//! sequence through the real LSP transport (and optionally
//! lspmux's fanout) so transport-layer routing bugs surface here
//! instead of at users.
//!
//! Usage:
//!
//!   # Direct (no lspmux):
//!   cargo run --bin tfls-mux-format-probe -- \
//!     --tfls-path target/debug/tfls --direct
//!
//!   # Via lspmux:
//!   cargo run --bin tfls-mux-format-probe -- \
//!     --tfls-path target/debug/tfls --lspmux-path "$(which lspmux)"

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

type SharedStdin = Arc<Mutex<tokio::process::ChildStdin>>;

#[derive(Debug, Parser)]
#[command(
    name = "tfls-mux-format-probe",
    about = "Drive a tfls session through opinionated-format → didChange → diagnostics"
)]
struct Cli {
    #[arg(long, default_value = "tfls")]
    tfls_path: PathBuf,
    #[arg(long, default_value = "lspmux")]
    lspmux_path: PathBuf,
    /// Per-stage drain window in ms.
    #[arg(long, default_value_t = 2500)]
    drain_ms: u64,
    /// `RUST_LOG`-style filter passed to the spawned tfls.
    #[arg(long, default_value = "tfls_lsp=info")]
    rust_log: String,
    /// Skip lspmux entirely; spawn `tfls` directly.
    #[arg(long)]
    direct: bool,
    /// Path for tfls's captured log when `--direct` is set.
    /// Defaults to `<workspace>/tfls.log`.
    #[arg(long)]
    tfls_log: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => {
            eprintln!("\nprobe: PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nprobe: FAIL — {e}");
            ExitCode::FAILURE
        }
    }
}

// Mirrors the rough shape of the user-reported repro: many top-level
// blocks intentionally out of opinionated order so the formatter
// reshuffles them, plus a multi-line ternary that historically
// confused hcl-edit's span tracking. The probe only asserts
// schema-independent diagnostics (unused declarations); the larger
// shape is here purely so the formatter has plenty of reordering to
// do, which is the trigger for the original bug report.
const FILE_CONTENTS: &str = r#"
variable "unused_z" {
  type = string
}

variable "unused_a" {
  type = string
}

output "out_z" {
  value = "z"
}

output "out_a" {
  value = "a"
}

module "vm" {
  source = "./modules/vm"

  count = var.asr_target && local.create_vm_infrastructure ? 1 : 0

  customer         = var.customer
  enable_public_ip = var.sql_server_public_ip
  environment      = var.environment
  region           = var.region
  resource_group   = module.azure[0].main_resource_group
  subnet_id        = module.azure[0].main_subnet
  tags             = local.tags
}

resource "aws_instance" "web" {
  ami           = "ami-0"
  instance_type = "t3.micro"
  bogus_attr    = "x"
}

locals {
  _dr_region_short_map = {
    "UK South" = "uks"
    "UK West"  = "ukw"
  }
  _dr_target_region_short = var.dr_failover_config != null ? local._dr_region_short_map[var.dr_failover_config.target_region] : ""
  unused_local_z          = 1
  unused_local_a          = 2
}

terraform {
  required_version = ">= 1.4.0"
}
"#;

/// Pairs of `(message-substring, line-content-substring)` we expect
/// to see across every diagnostic publish — both pre and post-format.
/// Names are unique enough that the line-content check is robust to
/// reordering. We deliberately stick to schema-independent
/// diagnostics (unused declarations) so the assertion shape is
/// identical between `--direct` and lspmux modes; schema-driven
/// diagnostics depend on the registry-docs cache under `$HOME`
/// which the lspmux mode rebuilds in a clean tempdir.
const EXPECTED: &[(&str, &str)] = &[
    ("`unused_z`", "unused_z"),
    ("`unused_a`", "unused_a"),
    ("`unused_local_z`", "unused_local_z"),
    ("`unused_local_a`", "unused_local_a"),
];

async fn run(cli: Cli) -> Result<(), String> {
    let tfls = cli
        .tfls_path
        .canonicalize()
        .map_err(|e| format!("canonicalize tfls path {:?}: {e}", cli.tfls_path))?;
    eprintln!("tfls:   {tfls:?}");
    let lspmux = if cli.direct {
        PathBuf::new()
    } else {
        let p = cli
            .lspmux_path
            .canonicalize()
            .map_err(|e| format!("canonicalize lspmux path {:?}: {e}", cli.lspmux_path))?;
        eprintln!("lspmux: {p:?}");
        p
    };
    if cli.direct {
        eprintln!("mode:   --direct (no lspmux)");
    }

    let workspace =
        tempfile_dir("tfls-mux-format-probe-ws").map_err(|e| format!("tempdir: {e}"))?;
    let main_tf = workspace.join("main.tf");
    std::fs::write(&main_tf, FILE_CONTENTS).map_err(|e| format!("write main.tf: {e}"))?;
    let workspace = workspace
        .canonicalize()
        .map_err(|e| format!("canonicalize: {e}"))?;
    eprintln!("workspace: {workspace:?}");

    let (mut child, daemon_handle, log_path) = if cli.direct {
        let log_path = cli
            .tfls_log
            .clone()
            .unwrap_or_else(|| workspace.join("tfls.log"));
        eprintln!("tfls log:   {log_path:?}");
        let mut c = Command::new(&tfls)
            .env("RUST_LOG", &cli.rust_log)
            .env("TFLS_LOG_FILE", &log_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn tfls: {e}"))?;
        pipe_stderr(&mut c, &log_path.with_extension("crash.log"));
        (c, None, log_path)
    } else {
        let home =
            tempfile_dir("tfls-mux-format-probe-home").map_err(|e| format!("tempdir: {e}"))?;
        let port = pick_free_port().ok_or("no free port")?;
        write_lspmux_config(&home, port).map_err(|e| format!("write config: {e}"))?;
        let mut daemon = spawn_daemon(&lspmux, &home).map_err(|e| format!("spawn daemon: {e}"))?;
        let daemon_log = home.join("lspmux.stderr.log");
        pipe_stderr(&mut daemon, &daemon_log);
        eprintln!("daemon log: {daemon_log:?}");
        wait_for_port(port).await?;
        eprintln!("daemon up on 127.0.0.1:{port}");
        let c = Command::new(&lspmux)
            .arg("client")
            .arg("--server-path")
            .arg(&tfls)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("RUST_LOG", &cli.rust_log)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn client: {e}"))?;
        (c, Some(daemon), daemon_log)
    };

    let stdin = child.stdin.take().ok_or("client stdin")?;
    let stdin: SharedStdin = Arc::new(Mutex::new(stdin));
    let stdout = child.stdout.take().ok_or("client stdout")?;
    let mut reader = BufReader::new(stdout);

    // ---- initialize / initialized / didOpen --------------------------
    let workspace_uri = format!("file://{}", workspace.to_str().ok_or("ws not utf-8")?);
    let main_uri = format!("file://{}", main_tf.to_str().ok_or("path not utf-8")?);
    send(
        &mut *stdin.lock().await,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": workspace_uri,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": true },
                        "formatting": {},
                        "synchronization": { "didSave": true }
                    },
                    "workspace": {
                        "configuration": true,
                        "didChangeConfiguration": { "dynamicRegistration": false },
                        "inlayHint": { "refreshSupport": true },
                        "diagnostics": { "refreshSupport": true }
                    }
                },
                "initializationOptions": { "formatStyle": "opinionated" }
            }
        }),
    )
    .await?;
    recv_response(&stdin, &mut reader, 1).await?;
    send(
        &mut *stdin.lock().await,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await?;
    send(
        &mut *stdin.lock().await,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": main_uri,
                    "languageId": "terraform",
                    "version": 1,
                    "text": FILE_CONTENTS,
                }
            }
        }),
    )
    .await?;

    eprintln!("\n--- stage 1: drain pre-format diagnostics ---");
    let pre = drain_diagnostics(&stdin, &mut reader, &main_uri, cli.drain_ms).await?;
    let pre_text = FILE_CONTENTS.to_string();
    let pre_failures = check_alignment("pre-format", &pre_text, &pre, EXPECTED);
    print_diags("pre-format", &pre_text, &pre);

    // ---- format request ----------------------------------------------
    eprintln!("\n--- stage 2: textDocument/formatting (opinionated) ---");
    send(
        &mut *stdin.lock().await,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/formatting",
            "params": {
                "textDocument": { "uri": main_uri },
                "options": { "tabSize": 2, "insertSpaces": true }
            }
        }),
    )
    .await?;
    let resp = recv_response(&stdin, &mut reader, 2).await?;
    let edits = parse_text_edits(&resp)?;
    if edits.is_empty() {
        return Err("formatter returned no edits — opinionated style not active?".into());
    }
    eprintln!("formatter returned {} edit(s)", edits.len());

    // Apply edits client-side.
    let post_text = apply_text_edits(&pre_text, &edits)?;
    if post_text == pre_text {
        return Err("post-format text identical to pre-format — formatter no-op".into());
    }

    // ---- didChange + drain post-format diagnostics --------------------
    send(
        &mut *stdin.lock().await,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": main_uri, "version": 2 },
                "contentChanges": [{ "text": post_text }]
            }
        }),
    )
    .await?;

    eprintln!("\n--- stage 3: drain post-format diagnostics ---");
    let post = drain_diagnostics(&stdin, &mut reader, &main_uri, cli.drain_ms).await?;
    let post_failures = check_alignment("post-format", &post_text, &post, EXPECTED);
    print_diags("post-format", &post_text, &post);

    // ---- shutdown / exit ---------------------------------------------
    let _ = send(
        &mut *stdin.lock().await,
        &json!({"jsonrpc":"2.0","id":99,"method":"shutdown","params":null}),
    )
    .await;
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        recv_response(&stdin, &mut reader, 99),
    )
    .await;
    let _ = send(
        &mut *stdin.lock().await,
        &json!({"jsonrpc":"2.0","method":"exit","params":null}),
    )
    .await;
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    if let Some(mut d) = daemon_handle {
        let _ = d.kill().await;
    }

    eprintln!("\nlog tail:");
    let _ = Command::new("tail")
        .args(["-n", "60"])
        .arg(&log_path)
        .status()
        .await;

    let mut all_failures = pre_failures;
    all_failures.extend(post_failures);
    if !all_failures.is_empty() {
        let mut msg = String::from("alignment failures:\n");
        for f in &all_failures {
            msg.push_str("  - ");
            msg.push_str(f);
            msg.push('\n');
        }
        return Err(msg);
    }
    Ok(())
}

fn parse_text_edits(response: &str) -> Result<Vec<lsp_types::TextEdit>, String> {
    let v: Value = serde_json::from_str(response).map_err(|e| format!("parse resp: {e}"))?;
    let result = v.get("result").ok_or("no result")?;
    if result.is_null() {
        return Ok(Vec::new());
    }
    let arr = result.as_array().ok_or("result not array")?;
    let mut out = Vec::new();
    for item in arr {
        let edit: lsp_types::TextEdit =
            serde_json::from_value(item.clone()).map_err(|e| format!("parse edit: {e}"))?;
        out.push(edit);
    }
    Ok(out)
}

fn apply_text_edits(src: &str, edits: &[lsp_types::TextEdit]) -> Result<String, String> {
    use ropey::Rope;
    let mut rope = Rope::from_str(src);
    let mut sorted: Vec<&lsp_types::TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse((e.range.start.line, e.range.start.character)));
    for edit in sorted {
        let start_line = edit.range.start.line as usize;
        let end_line = edit.range.end.line as usize;
        if start_line >= rope.len_lines() || end_line >= rope.len_lines() {
            return Err(format!("edit range out of bounds: {:?}", edit.range));
        }
        let start_byte = rope.line_to_byte(start_line) + edit.range.start.character as usize;
        let end_byte = rope.line_to_byte(end_line) + edit.range.end.character as usize;
        let start_char = rope.byte_to_char(start_byte);
        let end_char = rope.byte_to_char(end_byte);
        rope.remove(start_char..end_char);
        rope.insert(start_char, &edit.new_text);
    }
    Ok(rope.to_string())
}

async fn drain_diagnostics<R: AsyncReadExt + Unpin>(
    stdin: &SharedStdin,
    reader: &mut R,
    target_uri: &str,
    drain_ms: u64,
) -> Result<Vec<lsp_types::Diagnostic>, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(drain_ms);
    // Final-state semantics: every publish is a *replacement* for
    // the URI's diagnostics, so keep only the last one we see.
    let mut latest: Vec<lsp_types::Diagnostic> = Vec::new();
    let mut saw_any = false;
    loop {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            break;
        }
        let msg = match tokio::time::timeout(timeout, recv(reader)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(format!("recv: {e}")),
            Err(_) => break,
        };
        let Ok(value): Result<Value, _> = serde_json::from_str(&msg) else {
            continue;
        };
        // Auto-reply to server→client requests so tfls's
        // workspace/inlayHint/refresh and similar don't deadlock.
        let id = value.get("id").cloned();
        let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if let (Some(id), true) = (id.clone(), !method.is_empty()) {
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": null});
            send(&mut *stdin.lock().await, &reply).await?;
            continue;
        }
        if method != "textDocument/publishDiagnostics" {
            continue;
        }
        let params = value.get("params").ok_or("publish missing params")?;
        let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
        if uri != target_uri {
            continue;
        }
        let diags_v = params.get("diagnostics").cloned().unwrap_or(Value::Null);
        let diags: Vec<lsp_types::Diagnostic> = serde_json::from_value(diags_v).unwrap_or_default();
        latest = diags;
        saw_any = true;
    }
    if !saw_any {
        return Err("no publishDiagnostics arrived during drain".into());
    }
    Ok(latest)
}

fn check_alignment(
    label: &str,
    src: &str,
    diags: &[lsp_types::Diagnostic],
    expected: &[(&str, &str)],
) -> Vec<String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut failures = Vec::new();
    for (msg_needle, content_needle) in expected {
        let matching: Vec<_> = diags
            .iter()
            .filter(|d| d.message.contains(msg_needle))
            .collect();
        if matching.is_empty() {
            failures.push(format!(
                "[{label}] expected diagnostic matching {msg_needle:?}, none found"
            ));
            continue;
        }
        for d in matching {
            let line_idx = d.range.start.line as usize;
            let line = lines.get(line_idx).copied().unwrap_or("");
            if !line.contains(content_needle) {
                failures.push(format!(
                    "[{label}] {msg_needle} points at line {line_idx} = {line:?}, \
                     expected line content to contain {content_needle:?}"
                ));
            }
        }
    }
    failures
}

fn print_diags(label: &str, src: &str, diags: &[lsp_types::Diagnostic]) {
    let lines: Vec<&str> = src.lines().collect();
    eprintln!("[{label}] {} diagnostics:", diags.len());
    for d in diags {
        let line_idx = d.range.start.line as usize;
        let line = lines.get(line_idx).copied().unwrap_or("<oob>");
        let first_msg_line = d.message.lines().next().unwrap_or("");
        eprintln!(
            "  L{line_idx:>3} col {col:>3}: {msg}\n         line content: {line:?}",
            col = d.range.start.character,
            msg = first_msg_line,
        );
    }
}

// --- shared scaffolding (mirrors mux_lock_probe) -------------------

async fn send<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &Value) -> Result<(), String> {
    let body = msg.to_string();
    let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    w.write_all(frame.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    w.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

async fn recv<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<String, String> {
    let mut header = Vec::new();
    loop {
        let mut b = [0u8; 1];
        r.read_exact(&mut b)
            .await
            .map_err(|e| format!("read header: {e}"))?;
        header.push(b[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_str = std::str::from_utf8(&header).map_err(|e| format!("header utf-8: {e}"))?;
    let len: usize = header_str
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .ok_or("missing Content-Length")?
        .trim()
        .parse()
        .map_err(|e| format!("parse Content-Length: {e}"))?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .map_err(|e| format!("read body: {e}"))?;
    String::from_utf8(body).map_err(|e| format!("body utf-8: {e}"))
}

async fn recv_response<R: AsyncReadExt + Unpin>(
    stdin: &SharedStdin,
    r: &mut R,
    want_id: i64,
) -> Result<String, String> {
    let id_marker = format!("\"id\":{want_id}");
    loop {
        let body = recv(r).await?;
        if body.contains(&id_marker) {
            return Ok(body);
        }
        if let Ok(value) = serde_json::from_str::<Value>(&body) {
            let id = value.get("id").cloned();
            let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if let (Some(id), true) = (id, !method.is_empty()) {
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": null});
                send(&mut *stdin.lock().await, &reply).await?;
            }
        }
    }
}

fn tempfile_dir(prefix: &str) -> std::io::Result<PathBuf> {
    let p = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

fn write_lspmux_config(home: &Path, port: u16) -> std::io::Result<()> {
    let cfg = format!(
        "instance_timeout = 300\n\
         gc_interval = 10\n\
         listen = [\"127.0.0.1\", {port}]\n\
         connect = [\"127.0.0.1\", {port}]\n\
         log_filters = \"debug\"\n\
         pass_environment = [\"RUST_LOG\", \"TFLS_LOG_FILE\"]\n",
    );
    let xdg = home.join(".config/lspmux");
    std::fs::create_dir_all(&xdg)?;
    let mut f = std::fs::File::create(xdg.join("config.toml"))?;
    f.write_all(cfg.as_bytes())?;
    let macos = home.join("Library/Application Support/lspmux");
    std::fs::create_dir_all(&macos)?;
    let mut f = std::fs::File::create(macos.join("config.toml"))?;
    f.write_all(cfg.as_bytes())?;
    Ok(())
}

fn pick_free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

fn spawn_daemon(lspmux: &Path, home: &Path) -> std::io::Result<Child> {
    Command::new(lspmux)
        .arg("server")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

async fn wait_for_port(port: u16) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("daemon never bound".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn pipe_stderr(child: &mut Child, log: &Path) {
    let log = log.to_path_buf();
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut f = match tokio::fs::File::create(&log).await {
                Ok(f) => f,
                Err(_) => return,
            };
            let mut reader = stderr;
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let _ = tokio::io::AsyncWriteExt::write_all(&mut f, &buf[..n]).await;
            }
        });
    }
}
