//! go-plugin handshake: spawn the provider binary with the right env,
//! read its announcement line from stdout, and parse it into a
//! [`HandshakeInfo`].
//!
//! A typical announcement looks like:
//!
//! ```text
//! 1|6|unix|/tmp/plugin4215896.sock|grpc|<base64 server cert>
//! ```
//!
//! Fields in order:
//!   1. core protocol version (always 1 currently)
//!   2. app protocol version (5 or 6 for terraform)
//!   3. network type (`unix` or `tcp`)
//!   4. address (socket path for unix, `host:port` for tcp)
//!   5. protocol (`grpc`; `netrpc` is legacy and unsupported)
//!   6. optional base64-encoded server public cert (present when
//!      AutoMTLS is enabled)

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

use crate::ProtocolError;

/// Terraform's hashicorp/go-plugin magic cookie. Required in env for the
/// provider to consider us a legitimate host. The value is not the
/// generic go-plugin UUID — Terraform uses a specific 64-char hex cookie
/// compiled into the plugin SDK (see
/// <https://github.com/hashicorp/terraform-plugin-go/blob/main/tfprotov6/tf6server/server.go>).
pub const MAGIC_COOKIE_KEY: &str = "TF_PLUGIN_MAGIC_COOKIE";
pub const MAGIC_COOKIE_VALUE: &str =
    "d602bf8f470bc67ca7faa0386276bbdd4330efaf76d1a219cb4d6991ca9872b2";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Parsed fields from the handshake line.
#[derive(Debug, Clone)]
pub struct HandshakeInfo {
    pub core_protocol_version: u32,
    pub app_protocol_version: u32,
    pub network: Network,
    pub address: String,
    pub protocol: Protocol,
    /// Base64-encoded server cert, if AutoMTLS is enabled. Callers are
    /// expected to decode + trust exactly this certificate.
    pub server_cert_b64: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Unix,
    Tcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Grpc,
}

/// A running provider plugin process with its handshake info. Dropping
/// the instance sends SIGTERM so the child exits promptly.
pub struct PluginInstance {
    pub info: HandshakeInfo,
    pub path: PathBuf,
    child: Option<Child>,
}

impl PluginInstance {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Best-effort: try to kill the child. `tokio::process::Child`
            // handles `start_kill` synchronously; wait is async but we
            // can't await in Drop, so just fire the signal and let the
            // OS reap when we return.
            let _ = child.start_kill();
        }
    }
}

/// Spawn `binary` with the appropriate env, wait for its handshake
/// announcement line, parse it, and return the running process + info.
///
/// `client_cert_pem` is this tfls process's own cert — if provided, it's
/// forwarded to the provider via the `PLUGIN_CLIENT_CERT` env var so the
/// provider will accept an mTLS connection presenting that cert.
pub async fn spawn_and_handshake(
    binary: &Path,
    client_cert_pem: Option<&str>,
) -> Result<PluginInstance, ProtocolError> {
    let mut cmd = Command::new(binary);
    cmd.env(MAGIC_COOKIE_KEY, MAGIC_COOKIE_VALUE);
    cmd.env("TF_PLUGIN_MAGIC_COOKIE_KEY", MAGIC_COOKIE_KEY);
    cmd.env("TF_PLUGIN_MAGIC_COOKIE_VALUE", MAGIC_COOKIE_VALUE);
    // Tell the provider which tfplugin protocol versions we accept.
    // Providers that support both v5 and v6 will pick v6; older providers
    // (and many in-the-wild, including AWS <= 5.x) only speak v5.
    cmd.env("PLUGIN_PROTOCOL_VERSIONS", "5,6");
    if let Some(pem) = client_cert_pem {
        cmd.env("PLUGIN_CLIENT_CERT", pem);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|source| ProtocolError::Spawn {
        path: binary.display().to_string(),
        source,
    })?;

    // Forward stderr to our log so provider errors surface when TLS or
    // RPC calls fail.
    if let Some(stderr) = child.stderr.take() {
        let path_for_log = binary.display().to_string();
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => tracing::debug!(
                        provider = %path_for_log,
                        "{}",
                        line.trim_end_matches(['\r', '\n']),
                    ),
                }
            }
        });
    }

    let stdout = child.stdout.take().ok_or_else(|| ProtocolError::Io {
        path: binary.display().to_string(),
        source: std::io::Error::other("child stdout not captured"),
    })?;

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    let read_result = timeout(HANDSHAKE_TIMEOUT, reader.read_line(&mut line)).await;

    let bytes = match read_result {
        Ok(Ok(0)) => {
            return Err(ProtocolError::BadHandshake {
                path: binary.display().to_string(),
                reason: "provider exited before emitting a handshake line".into(),
            });
        }
        Ok(Ok(n)) => n,
        Ok(Err(source)) => {
            return Err(ProtocolError::Io {
                path: binary.display().to_string(),
                source,
            });
        }
        Err(_) => {
            return Err(ProtocolError::HandshakeTimeout {
                path: binary.display().to_string(),
            });
        }
    };

    let trimmed = line[..bytes].trim_end_matches(['\r', '\n']);
    let info = parse_handshake_line(trimmed).map_err(|reason| ProtocolError::BadHandshake {
        path: binary.display().to_string(),
        reason,
    })?;

    if !matches!(info.app_protocol_version, 5 | 6) {
        return Err(ProtocolError::UnsupportedProtocol {
            version: info.app_protocol_version,
        });
    }

    // Put stdout back into the child so stderr/stdout don't accumulate.
    // (Actually we can't put it back; just let the reader own it and
    // keep pulling to avoid SIGPIPE when the child writes logs.)
    tokio::spawn(async move {
        let mut remaining = reader;
        let mut sink = String::new();
        while let Ok(n) = remaining.read_line(&mut sink).await {
            if n == 0 {
                break;
            }
            sink.clear();
        }
    });

    Ok(PluginInstance {
        info,
        path: binary.to_path_buf(),
        child: Some(child),
    })
}

/// Parse a handshake announcement string (without the trailing newline)
/// into structured fields. Returns the error message for logging if the
/// line is malformed.
pub fn parse_handshake_line(line: &str) -> Result<HandshakeInfo, String> {
    let parts: Vec<&str> = line.split('|').collect();
    if !(5..=6).contains(&parts.len()) {
        return Err(format!("expected 5 or 6 '|'-separated fields, got {}", parts.len()));
    }

    let core_protocol_version = parts[0]
        .parse::<u32>()
        .map_err(|e| format!("bad core protocol version {:?}: {e}", parts[0]))?;
    let app_protocol_version = parts[1]
        .parse::<u32>()
        .map_err(|e| format!("bad app protocol version {:?}: {e}", parts[1]))?;
    let network = match parts[2] {
        "unix" => Network::Unix,
        "tcp" => Network::Tcp,
        other => return Err(format!("unknown network type {other:?}")),
    };
    let address = parts[3].to_string();
    let protocol = match parts[4] {
        "grpc" => Protocol::Grpc,
        other => return Err(format!("unsupported wire protocol {other:?} (only `grpc`)")),
    };
    let server_cert_b64 = parts.get(5).map(|s| s.to_string()).filter(|s| !s.is_empty());

    Ok(HandshakeInfo {
        core_protocol_version,
        app_protocol_version,
        network,
        address,
        protocol,
        server_cert_b64,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_handshake() {
        let info =
            parse_handshake_line("1|6|unix|/tmp/plugin.sock|grpc").expect("parse");
        assert_eq!(info.core_protocol_version, 1);
        assert_eq!(info.app_protocol_version, 6);
        assert_eq!(info.network, Network::Unix);
        assert_eq!(info.address, "/tmp/plugin.sock");
        assert_eq!(info.protocol, Protocol::Grpc);
        assert!(info.server_cert_b64.is_none());
    }

    #[test]
    fn parses_handshake_with_server_cert() {
        let info = parse_handshake_line(
            "1|6|unix|/tmp/plugin.sock|grpc|MIIC123...",
        )
        .expect("parse");
        assert_eq!(info.server_cert_b64.as_deref(), Some("MIIC123..."));
    }

    #[test]
    fn parses_tcp_endpoint() {
        let info = parse_handshake_line("1|5|tcp|127.0.0.1:54321|grpc").expect("parse");
        assert_eq!(info.app_protocol_version, 5);
        assert_eq!(info.network, Network::Tcp);
        assert_eq!(info.address, "127.0.0.1:54321");
    }

    #[test]
    fn rejects_netrpc() {
        assert!(parse_handshake_line("1|6|unix|/tmp/s|netrpc").is_err());
    }

    #[test]
    fn rejects_short_line() {
        assert!(parse_handshake_line("1|6|unix|/tmp/s").is_err());
    }

    #[test]
    fn rejects_non_numeric_versions() {
        assert!(parse_handshake_line("a|6|unix|/tmp/s|grpc").is_err());
        assert!(parse_handshake_line("1|x|unix|/tmp/s|grpc").is_err());
    }
}
