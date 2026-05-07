//! Lock-file-change-through-lspmux probe.
//!
//! Boots an isolated `lspmux` daemon on a temp port + spawns a
//! single `lspmux client` subprocess to a fresh `tfls`. Drives the
//! initialize / didOpen handshake, then mutates
//! `<workspace>/.terraform.lock.hcl` several times mid-session
//! and reports every `textDocument/publishDiagnostics`
//! notification that actually arrives at the client.
//!
//! Used to pin where the lock-file → diagnostic refresh chain
//! breaks: tfls's in-process flow vs the routing through lspmux's
//! fanout. If the in-process probe (`tfls-lock-probe`) shows
//! correct state but THIS probe never sees a publishDiagnostics
//! after a lock mutation, lspmux is dropping / mis-routing the
//! notification.
//!
//! Usage:
//!
//!   cargo run --bin tfls-mux-lock-probe -- \
//!     --tfls-path target/debug/tfls \
//!     --lspmux-path "$(which lspmux)"

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stdout)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

type SharedStdin = Arc<Mutex<tokio::process::ChildStdin>>;

#[derive(Debug, Parser)]
#[command(
    name = "tfls-mux-lock-probe",
    about = "Drive a single lspmux+tfls session, mutate the lock file mid-session"
)]
struct Cli {
    #[arg(long, default_value = "tfls")]
    tfls_path: PathBuf,
    #[arg(long, default_value = "lspmux")]
    lspmux_path: PathBuf,
    /// Per-mutation drain window in ms. Larger values catch slow
    /// watchers / publish chains.
    #[arg(long, default_value_t = 1500)]
    drain_ms: u64,
    /// `RUST_LOG`-style filter passed to the spawned tfls (and
    /// captured in the daemon's stderr log) so you can see what
    /// the server saw.
    #[arg(long, default_value = "tfls_lsp=info")]
    rust_log: String,
    /// Skip lspmux entirely; spawn `tfls` directly and drive
    /// JSON-RPC over its stdio. Use this to isolate whether
    /// publishDiagnostics drops are happening inside lspmux's
    /// fanout or inside tfls itself.
    #[arg(long)]
    direct: bool,
    /// Spawn the `notify-smoke` binary instead of `tfls`. Used
    /// to confirm whether notify works inside a probe-spawned
    /// subprocess. Skips JSON-RPC + initialise; just lets the
    /// smoke binary run to completion and reports its exit.
    #[arg(long)]
    smoke: bool,
    /// Path for tfls's captured stderr when `--direct` is set.
    /// Defaults to `<workspace>/tfls.stderr.log`.
    #[arg(long)]
    tfls_stderr_log: Option<PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("probe error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    // Canonicalise binary paths up front: lspmux spawns the
    // language server from the WORKSPACE cwd, so a relative
    // tfls path (`./target/debug/tfls`) breaks. Same for lspmux
    // itself — relative path would resolve against whichever
    // dir was passed in `Command::current_dir`.
    let tfls = cli.tfls_path.canonicalize().map_err(|e| {
        format!("canonicalize tfls path {:?}: {e}", cli.tfls_path)
    })?;
    eprintln!("tfls:   {tfls:?}");
    let lspmux = if cli.direct {
        PathBuf::new()
    } else {
        let p = cli.lspmux_path.canonicalize().map_err(|e| {
            format!("canonicalize lspmux path {:?}: {e}", cli.lspmux_path)
        })?;
        eprintln!("lspmux: {p:?}");
        p
    };
    if cli.direct {
        eprintln!("mode:   --direct (no lspmux)");
    }

    // Workspace setup: main.tf + initial .terraform.lock.hcl.
    let workspace = tempfile_dir("tfls-mux-lock-probe-ws")
        .map_err(|e| format!("tempdir: {e}"))?;
    let main_tf = workspace.join("main.tf");
    std::fs::write(
        &main_tf,
        r#"terraform {
  required_providers {
    azurerm = {
      source  = "hashicorp/azurerm"
      version = "~> 4.71.0"
    }
  }
}
"#,
    )
    .map_err(|e| format!("write main.tf: {e}"))?;
    write_lock(&workspace, "4.71.0").map_err(|e| format!("write lock: {e}"))?;

    let workspace = workspace
        .canonicalize()
        .map_err(|e| format!("canonicalize: {e}"))?;
    eprintln!("workspace: {workspace:?}");

    let (mut client, daemon_handle, daemon_log_path) = if cli.direct {
        let stderr_log = cli
            .tfls_stderr_log
            .clone()
            .unwrap_or_else(|| workspace.join("tfls.stderr.log"));
        eprintln!("tfls stderr log: {stderr_log:?}");
        // tfls's `init_tracing` defaults to `$XDG_RUNTIME_DIR/tfls.log`
        // or `/tmp/tfls.log` — a shared global file across every
        // instance. Pin it to a probe-owned path so we can `tail`
        // the log for THIS run without race / interleaving.
        // Don't set current_dir(&workspace): tfls receives the
        // workspace path via `initialize.rootUri`. Setting cwd on
        // the spawned process is unnecessary and may interact
        // badly with macOS FSEvents which sometimes refuses to
        // register watches on the cwd of the calling process.
        let mut child = Command::new(&tfls)
            .env("RUST_LOG", &cli.rust_log)
            .env("TFLS_LOG_FILE", &stderr_log)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn tfls: {e}"))?;
        // tfls prefers its own file sink when writable, so child's
        // stderr usually stays empty. Capture anyway for crashes.
        pipe_stderr(&mut child, &stderr_log.with_extension("crash.log"));
        (child, None, stderr_log)
    } else {
        // Isolated lspmux daemon on a free port + temp HOME.
        let home = tempfile_dir("tfls-mux-lock-probe-home")
            .map_err(|e| format!("tempdir: {e}"))?;
        std::fs::create_dir_all(home.join(".config/lspmux"))
            .map_err(|e| format!("config dir: {e}"))?;
        let port = pick_free_port().ok_or("no free port")?;
        write_lspmux_config(&home, port).map_err(|e| format!("write config: {e}"))?;

        let mut daemon =
            spawn_daemon(&lspmux, &home).map_err(|e| format!("spawn daemon: {e}"))?;
        let daemon_log = home.join("lspmux.stderr.log");
        pipe_stderr(&mut daemon, &daemon_log);
        eprintln!("daemon log: {daemon_log:?}");
        wait_for_port(port).await?;
        eprintln!("daemon up on 127.0.0.1:{port}");

        // Spawn lspmux client → tfls. Pass RUST_LOG so we can grep
        // the daemon log for `watcher: LockFileChanged` after mutation.
        let child = Command::new(&lspmux)
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
        (child, Some(daemon), daemon_log)
    };

    let stdin = client.stdin.take().ok_or("client stdin")?;
    let stdin: SharedStdin = Arc::new(Mutex::new(stdin));
    let stdout = client.stdout.take().ok_or("client stdout")?;
    let mut reader = BufReader::new(stdout);

    // 1. initialize — declare client capability for inlay-hint refresh
    // so tfls's `LockFileChanged` arm doesn't surprise us with an
    // unsolicited `workspace/inlayHint/refresh` request that the
    // probe needs to answer (which it does in `drain` below).
    let workspace_uri = format!("file://{}", workspace.to_str().ok_or("ws not utf-8")?);
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
                        "inlayHint": { "dynamicRegistration": true }
                    },
                    "workspace": {
                        "inlayHint": { "refreshSupport": true },
                        "diagnostics": { "refreshSupport": true }
                    }
                }
            }
        }),
    )
    .await?;
    let _ = recv_response(&stdin, &mut reader, 1).await?;
    send(
        &mut *stdin.lock().await,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await?;

    // 2. didOpen
    let main_uri = format!("file://{}", main_tf.to_str().ok_or("path not utf-8")?);
    let main_text = std::fs::read_to_string(&main_tf).map_err(|e| format!("read: {e}"))?;
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
                    "text": main_text,
                }
            }
        }),
    )
    .await?;

    eprintln!("\n--- step 0: initial state, lock 4.71.0 ---");
    drain(&stdin, &mut reader, &main_uri, cli.drain_ms).await?;

    let steps = [
        ("step 1: lock → 2.71.0 (drift expected)", "2.71.0"),
        ("step 2: lock → 4.71.0 (clear expected)", "4.71.0"),
        ("step 3: lock → 1.0.0 (drift again)", "1.0.0"),
        ("step 4: lock → 4.71.0 (clear)", "4.71.0"),
        ("step 5: lock → 3.0.0 (drift)", "3.0.0"),
    ];
    for (label, version) in steps {
        eprintln!("\n--- {label} ---");
        write_lock(&workspace, version).map_err(|e| format!("rewrite lock: {e}"))?;
        drain(&stdin, &mut reader, &main_uri, cli.drain_ms).await?;
    }

    // 3. shutdown / exit
    let _ = send(
        &mut *stdin.lock().await,
        &json!({"jsonrpc":"2.0","id":99,"method":"shutdown","params":null}),
    )
    .await;
    let _ = recv_response(&stdin, &mut reader, 99).await;
    let _ = send(
        &mut *stdin.lock().await,
        &json!({"jsonrpc":"2.0","method":"exit","params":null}),
    )
    .await;
    let _ = client.wait().await;
    if let Some(mut d) = daemon_handle {
        let _ = d.kill().await;
    }

    let label = if cli.direct {
        "tfls stderr tail (grep for LockFileChanged / publish):"
    } else {
        "Daemon log tail (grep for LockFileChanged / publish):"
    };
    eprintln!("\n{label}");
    let _ = Command::new("tail")
        .args(["-n", "120"])
        .arg(&daemon_log_path)
        .status()
        .await;

    Ok(())
}

async fn drain<R: AsyncReadExt + Unpin>(
    stdin: &SharedStdin,
    reader: &mut R,
    target_uri: &str,
    drain_ms: u64,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(drain_ms);
    let mut publishes = 0usize;
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
        // tfls's `LockFileChanged` arm fires `inlay_hint_refresh` and
        // `workspace_diagnostic_refresh` — server→client REQUESTS that
        // expect a response. Ignoring them deadlocks tfls because
        // tower-lsp's `Client::*_refresh` awaits the response. Reply
        // with a generic null result so the server can move on.
        let id = value.get("id").cloned();
        let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if let (Some(id), true) = (id.clone(), !method.is_empty()) {
            // It's a server-to-client request (has both id + method).
            let reply = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": null,
            });
            send(&mut *stdin.lock().await, &reply).await?;
            continue;
        }
        if method != "textDocument/publishDiagnostics" {
            continue;
        }
        let params = match value.get("params") {
            Some(p) => p,
            None => continue,
        };
        let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
        if uri != target_uri {
            continue;
        }
        publishes += 1;
        let diags = params
            .get("diagnostics")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        let drift_msgs: Vec<&str> = diags
            .iter()
            .filter_map(|d| d.get("message").and_then(|m| m.as_str()))
            .filter(|m| m.contains("does not admit"))
            .collect();
        eprintln!(
            "  publish #{publishes}: {} diagnostics, {} drift",
            diags.len(),
            drift_msgs.len()
        );
        for m in drift_msgs {
            eprintln!("    ⚠ {}", m.lines().next().unwrap_or(""));
        }
    }
    if publishes == 0 {
        eprintln!("  (no publishDiagnostics during {drain_ms}ms drain — POTENTIAL BUG)");
    }
    Ok(())
}

fn write_lock(dir: &Path, azurerm_version: &str) -> std::io::Result<()> {
    let body = format!(
        r#"provider "registry.opentofu.org/hashicorp/azurerm" {{
  version     = "{azurerm_version}"
  constraints = "~> 4.71.0"
  hashes      = []
}}
"#
    );
    std::fs::write(dir.join(".terraform.lock.hcl"), body)
}

// --- shared scaffolding (mirrors tfls-mux-probe) -------------------

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
    let header_str =
        std::str::from_utf8(&header).map_err(|e| format!("header utf-8: {e}"))?;
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
        // Auto-reply to server-to-client requests so tfls's
        // `inlay_hint_refresh` / `workspace_diagnostic_refresh`
        // calls don't deadlock waiting for a response.
        if body.contains(&id_marker) {
            // Either a response to our own id OR a server request
            // happening to use the same id (extremely unlikely;
            // tower-lsp uses incrementing ids for its requests
            // separately from client-issued ids). The marker check
            // alone is safe in practice.
            return Ok(body);
        }
        if let Ok(value) = serde_json::from_str::<Value>(&body) {
            let id = value.get("id").cloned();
            let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
            if let (Some(id), true) = (id, !method.is_empty()) {
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null,
                });
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
    // `directories::ProjectDirs` looks at $XDG_CONFIG_HOME first
    // (Linux) and `$HOME/Library/Application Support/lspmux`
    // (macOS). Write to BOTH so the daemon finds the config
    // regardless of which platform's ProjectDirs lookup wins
    // when XDG_CONFIG_HOME is set on macOS.
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
