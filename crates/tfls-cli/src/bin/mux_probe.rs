//! Multi-session lspmux + tfls probe. Spawns an isolated lspmux
//! daemon on a tmp port and connects N sequential `lspmux client`
//! subprocesses to it, simulating sequential nvim invocations
//! against the same project. For each session, sends `initialize`
//! → `initialized` → `textDocument/didOpen` → drains
//! `publishDiagnostics`, prints a per-session summary.
//!
//! Use to reproduce / verify the bug where the SECOND nvim
//! attaching to a long-lived lspmux+tfls instance receives no
//! diagnostics for files the first nvim already had open.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

#[derive(Debug, Parser)]
#[command(
    name = "tfls-mux-probe",
    about = "Drive N sequential lspmux clients against a single tfls instance"
)]
struct Cli {
    /// Path to the `tfls` binary that lspmux should spawn.
    /// Defaults to "tfls" on PATH; the nix dev shell wraps this
    /// to the workspace's pinned build.
    #[arg(long, default_value = "tfls")]
    tfls_path: PathBuf,

    /// Path to the `lspmux` binary. Defaults to whichever is on
    /// PATH; in the nix dev shell that's the workspace's pinned
    /// build.
    #[arg(long, default_value = "lspmux")]
    lspmux_path: PathBuf,

    /// Workspace root the probe simulates each nvim session
    /// opening.
    #[arg(long)]
    workspace: PathBuf,

    /// Specific file in `workspace` to open (relative path). When
    /// omitted, picks the first `.tf` found.
    #[arg(long)]
    file: Option<PathBuf>,

    /// Number of sequential client sessions.
    #[arg(long, default_value_t = 2)]
    sessions: u32,

    /// Per-session drain window for publishDiagnostics, in ms.
    #[arg(long, default_value_t = 2500)]
    drain_ms: u64,

    /// When set, sessions 2..N do NOT send didOpen — they just
    /// initialize, drain, and exit. Tests whether
    /// workspace-wide diagnostics from session 1's bulk scan are
    /// replayed to subsequent attaching clients.
    #[arg(long)]
    no_open_after_first: bool,

    /// Capture publishes for ANY URI, not just `--file`.
    #[arg(long)]
    any_uri: bool,

    /// Increase verbosity (`-v` = info, `-vv` = debug, `-vvv` = trace).
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
            eprintln!("probe error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let workspace = cli
        .workspace
        .canonicalize()
        .map_err(|e| format!("canonicalize {:?}: {e}", cli.workspace))?;

    let target_file = match &cli.file {
        Some(p) => workspace.join(p),
        None => first_tf_under(&workspace).ok_or("no .tf file under workspace")?,
    };
    let file_uri = format!(
        "file://{}",
        target_file.to_str().ok_or("file path not utf-8")?
    );
    let file_text =
        std::fs::read_to_string(&target_file).map_err(|e| format!("read {target_file:?}: {e}"))?;
    eprintln!("probe target: {target_file:?}");
    eprintln!("workspace:    {workspace:?}");

    // Isolated lspmux daemon: temp HOME, config on a free TCP port.
    let tmp = tempfile_dir().map_err(|e| format!("tempdir: {e}"))?;
    let port = pick_free_port().ok_or("no free port for lspmux")?;
    write_lspmux_config(&tmp, port).map_err(|e| format!("write config: {e}"))?;

    let mut daemon = spawn_daemon(&cli.lspmux_path, &tmp).map_err(|e| format!("spawn daemon: {e}"))?;
    // Pipe daemon stderr to a file for post-mortem inspection.
    let daemon_log = tmp.join("lspmux.stderr.log");
    if let Some(stderr) = daemon.stderr.take() {
        let log = daemon_log.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
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
    eprintln!("daemon log: {daemon_log:?}");

    // Wait for the daemon to bind. Poll the TCP port.
    if let Err(e) = wait_for_port(port).await {
        let _ = daemon.kill().await;
        return Err(format!("daemon never bound port {port}: {e}"));
    }
    eprintln!("lspmux daemon up on 127.0.0.1:{port}, pid={:?}", daemon.id());

    let mut summaries: Vec<SessionSummary> = Vec::new();
    for n in 1..=cli.sessions {
        eprintln!("\n--- session {n} ---");
        let send_open = n == 1 || !cli.no_open_after_first;
        let summary = drive_session(
            &cli.lspmux_path,
            &cli.tfls_path,
            &tmp,
            &workspace,
            &file_uri,
            &file_text,
            cli.drain_ms,
            send_open,
            cli.any_uri,
        )
        .await
        .map_err(|e| format!("session {n}: {e}"))?;
        summaries.push(summary);
    }

    eprintln!("\n=== summary ===");
    for (i, s) in summaries.iter().enumerate() {
        let first = s
            .first_publish_at
            .map(|d| format!("{:>5}ms", d.as_millis()))
            .unwrap_or_else(|| " -    ".to_string());
        eprintln!(
            "session {} : publishes={:>2}  total_diags={:>3}  first_at={}",
            i + 1,
            s.publish_count,
            s.total_diags,
            first,
        );
    }

    let bug = summaries.first().is_some_and(|s| s.publish_count > 0)
        && summaries.iter().skip(1).all(|s| s.publish_count == 0);
    if bug {
        eprintln!(
            "\nBUG REPRODUCED: session 1 received diagnostics, subsequent sessions did not."
        );
    } else if summaries.iter().all(|s| s.publish_count > 0) {
        eprintln!("\nALL SESSIONS RECEIVED DIAGNOSTICS — bug not reproduced (or already fixed).");
    } else {
        eprintln!("\nMIXED OUTCOME — see per-session table.");
    }

    let _ = daemon.kill().await;
    Ok(())
}

struct SessionSummary {
    publish_count: usize,
    total_diags: usize,
    first_publish_at: Option<Duration>,
}

#[allow(clippy::too_many_arguments)]
async fn drive_session(
    lspmux: &Path,
    tfls: &Path,
    home: &Path,
    workspace: &Path,
    file_uri: &str,
    file_text: &str,
    drain_ms: u64,
    send_open: bool,
    any_uri: bool,
) -> Result<SessionSummary, String> {
    let workspace_uri = format!("file://{}", workspace.to_str().ok_or("ws not utf-8")?);

    let mut client = Command::new(lspmux)
        .arg("client")
        .arg("--server-path")
        .arg(tfls)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn client: {e}"))?;

    let mut stdin = client.stdin.take().ok_or("client stdin missing")?;
    let stdout = client.stdout.take().ok_or("client stdout missing")?;
    let mut reader = BufReader::new(stdout);

    // 1. initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": workspace_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": true }
                },
                "workspace": {}
            }
        }
    });
    send(&mut stdin, &init_req).await?;
    let _init_resp = recv_response(&mut reader, 1).await?;

    // 2. initialized
    send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
    )
    .await?;

    // 3. didOpen (skipped when `send_open` is false — used to
    //    test pure attach-time replay of cached diagnostics).
    let session_start = Instant::now();
    if send_open {
        let did_open = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": file_uri,
                    "languageId": "terraform",
                    "version": 1,
                    "text": file_text,
                }
            }
        });
        send(&mut stdin, &did_open).await?;
    } else {
        eprintln!("  (didOpen skipped — pure attach-time replay test)");
    }

    // 4. drain publishes for `drain_ms`.
    let mut publish_count = 0usize;
    let mut total_diags = 0usize;
    let mut first_at: Option<Duration> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(drain_ms);

    loop {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            break;
        }
        let msg = match tokio::time::timeout(timeout, recv(&mut reader)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(format!("recv: {e}")),
            Err(_) => break,
        };
        let Ok(value): Result<Value, _> = serde_json::from_str(&msg) else {
            continue;
        };
        if value.get("method").and_then(|m| m.as_str())
            != Some("textDocument/publishDiagnostics")
        {
            continue;
        }
        let Some(params) = value.get("params") else { continue };
        let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
        if !any_uri && uri != file_uri {
            continue;
        }
        publish_count += 1;
        let n = params
            .get("diagnostics")
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        total_diags += n;
        if first_at.is_none() {
            first_at = Some(session_start.elapsed());
        }
        if any_uri {
            eprintln!("  publish #{publish_count}: {n} diagnostics  uri={uri}");
        } else {
            eprintln!("  publish #{publish_count}: {n} diagnostics");
        }
    }

    // 5. shutdown + exit so the lspmux client closes cleanly. (We
    //    deliberately let the daemon stay alive across sessions.)
    let _ = send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":99,"method":"shutdown","params":null}),
    )
    .await;
    let _ = recv_response(&mut reader, 99).await;
    let _ = send(
        &mut stdin,
        &json!({"jsonrpc":"2.0","method":"exit","params":null}),
    )
    .await;
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(2), client.wait()).await;
    let _ = client.kill().await;

    Ok(SessionSummary {
        publish_count,
        total_diags,
        first_publish_at: first_at,
    })
}

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
            .map_err(|e| format!("read header byte: {e}"))?;
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
    Ok(String::from_utf8(body).map_err(|e| format!("body utf-8: {e}"))?)
}

async fn recv_response<R: AsyncReadExt + Unpin>(
    r: &mut R,
    want_id: i64,
) -> Result<String, String> {
    let id_marker = format!("\"id\":{want_id}");
    loop {
        let body = recv(r).await?;
        if body.contains(&id_marker) {
            return Ok(body);
        }
        // Otherwise: notification or unrelated request — drop.
    }
}

fn first_tf_under(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).ok()?;
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if matches!(name, ".git" | ".terraform" | "node_modules") {
                        continue;
                    }
                }
                stack.push(p);
            } else if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                if name.ends_with(".tf") {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn tempfile_dir() -> std::io::Result<PathBuf> {
    let p = std::env::temp_dir().join(format!(
        "tfls-mux-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(p.join(".config/lspmux"))?;
    Ok(p)
}

fn write_lspmux_config(home: &Path, port: u16) -> std::io::Result<()> {
    // `Address` deserializes as untagged tuple `[ip, port]`.
    let cfg = format!(
        "instance_timeout = 300\n\
         gc_interval = 10\n\
         listen = [\"127.0.0.1\", {port}]\n\
         connect = [\"127.0.0.1\", {port}]\n\
         log_filters = \"debug\"\n\
         pass_environment = []\n",
    );
    let mut f = std::fs::File::create(home.join(".config/lspmux/config.toml"))?;
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
    // `directories::ProjectDirs` looks at `XDG_CONFIG_HOME` first
    // (then `$HOME/.config`), so override BOTH so the isolated
    // daemon picks up our temp-dir config and not the user's
    // running daemon's config.
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
            return Err("timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
