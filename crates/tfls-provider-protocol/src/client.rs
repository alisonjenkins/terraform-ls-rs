//! Connect to a spawned provider plugin over its advertised socket and
//! call `GetProviderSchema` / `GetFunctions`.
//!
//! Providers speak gRPC over an mTLS-protected transport. The channel
//! uses rustls' explicit config (see [`crate::tls`]) and a custom
//! connector that dials the unix socket or TCP address from the
//! handshake.

use std::sync::Arc;

use tonic::Request;
use tonic::transport::{Channel, Endpoint, Uri};

use crate::discovery::ProviderBinary;
use crate::handshake::{Network, PluginInstance, spawn_and_handshake};
use crate::proto::provider_client::ProviderClient as ProviderClientV6;
use crate::proto::{StringKind, get_provider_schema};
use crate::proto_v5::provider_client::ProviderClient as ProviderClientV5;
use crate::tls::{ClientIdentity, build_client_config};
use crate::{ProtocolError, proto, proto_v5, translate, translate_v5};

/// One-shot: launch the binary in `bin`, do the handshake, call the
/// schema RPC (v5 `GetSchema` or v6 `GetProviderSchema` depending on
/// which version the provider negotiated), translate, shut down.
pub async fn fetch_provider_schema(
    bin: &ProviderBinary,
) -> Result<tfls_schema::ProviderSchema, ProtocolError> {
    let identity = ClientIdentity::generate()?;
    let instance = spawn_and_handshake(&bin.binary, Some(&identity.cert_pem)).await?;
    let channel = connect_channel(&instance, &identity).await?;

    let result = match instance.info.app_protocol_version {
        6 => fetch_schema_v6(bin, channel).await,
        5 => fetch_schema_v5(bin, channel).await,
        v => Err(ProtocolError::UnsupportedProtocol { version: v }),
    };
    drop(instance);
    result
}

async fn fetch_schema_v6(
    bin: &ProviderBinary,
    channel: Channel,
) -> Result<tfls_schema::ProviderSchema, ProtocolError> {
    let mut client = ProviderClientV6::new(channel);
    let resp = client
        .get_provider_schema(Request::new(get_provider_schema::Request::default()))
        .await
        .map_err(|status| ProtocolError::Rpc {
            path: bin.binary.display().to_string(),
            status,
        })?
        .into_inner();

    let provider = match resp.provider {
        Some(s) => translate::schema_from_proto(&s)?,
        None => tfls_schema::Schema {
            version: 0,
            block: Default::default(),
        },
    };
    let mut resource_schemas = std::collections::HashMap::new();
    for (name, sch) in resp.resource_schemas {
        resource_schemas.insert(name, translate::schema_from_proto(&sch)?);
    }
    let mut data_source_schemas = std::collections::HashMap::new();
    for (name, sch) in resp.data_source_schemas {
        data_source_schemas.insert(name, translate::schema_from_proto(&sch)?);
    }

    Ok(tfls_schema::ProviderSchema {
        provider,
        resource_schemas,
        data_source_schemas,
    })
}

async fn fetch_schema_v5(
    bin: &ProviderBinary,
    channel: Channel,
) -> Result<tfls_schema::ProviderSchema, ProtocolError> {
    let mut client = ProviderClientV5::new(channel);
    let resp = client
        .get_schema(Request::new(
            proto_v5::get_provider_schema::Request::default(),
        ))
        .await
        .map_err(|status| ProtocolError::Rpc {
            path: bin.binary.display().to_string(),
            status,
        })?
        .into_inner();

    let provider = match resp.provider {
        Some(s) => translate_v5::schema_from_proto(&s)?,
        None => tfls_schema::Schema {
            version: 0,
            block: Default::default(),
        },
    };
    let mut resource_schemas = std::collections::HashMap::new();
    for (name, sch) in resp.resource_schemas {
        resource_schemas.insert(name, translate_v5::schema_from_proto(&sch)?);
    }
    let mut data_source_schemas = std::collections::HashMap::new();
    for (name, sch) in resp.data_source_schemas {
        data_source_schemas.insert(name, translate_v5::schema_from_proto(&sch)?);
    }

    Ok(tfls_schema::ProviderSchema {
        provider,
        resource_schemas,
        data_source_schemas,
    })
}

/// Returns provider-defined functions. The caller is responsible for
/// namespacing them before merging into the global `FunctionsSchema`
/// (e.g. `provider::<ns>::<name>::<fn>`). Only available over tfplugin6;
/// v5 providers don't export functions and return an empty result.
pub async fn fetch_provider_functions(
    bin: &ProviderBinary,
) -> Result<Vec<(String, tfls_schema::FunctionSignature)>, ProtocolError> {
    let identity = ClientIdentity::generate()?;
    let instance = spawn_and_handshake(&bin.binary, Some(&identity.cert_pem)).await?;
    if instance.info.app_protocol_version != 6 {
        return Ok(Vec::new());
    }
    let channel = connect_channel(&instance, &identity).await?;
    let mut client = ProviderClientV6::new(channel);

    let resp = client
        .get_provider_schema(Request::new(get_provider_schema::Request::default()))
        .await
        .map_err(|status| ProtocolError::Rpc {
            path: bin.binary.display().to_string(),
            status,
        })?
        .into_inner();

    let mut out = Vec::new();
    for (fn_name, func) in resp.functions {
        let sig = translate::function_from_proto(&func)?;
        let qualified = format!(
            "provider::{ns}::{name}::{fn_name}",
            ns = bin.namespace,
            name = bin.name
        );
        out.push((qualified, sig));
    }
    drop(instance);
    Ok(out)
}

async fn connect_channel(
    instance: &PluginInstance,
    identity: &ClientIdentity,
) -> Result<Channel, ProtocolError> {
    let server_cert_b64 = instance
        .info
        .server_cert_b64
        .as_deref()
        .ok_or_else(|| ProtocolError::BadHandshake {
            path: instance.path().display().to_string(),
            reason: "AutoMTLS required but no server cert in handshake".into(),
        })?;
    let tls_config = build_client_config(identity, server_cert_b64)?;

    let channel = match instance.info.network {
        Network::Unix => connect_unix(&instance.info.address, tls_config, instance).await?,
        Network::Tcp => connect_tcp(&instance.info.address, tls_config, instance).await?,
    };
    Ok(channel)
}

async fn connect_tcp(
    addr: &str,
    tls_config: Arc<rustls::ClientConfig>,
    instance: &PluginInstance,
) -> Result<Channel, ProtocolError> {
    use tokio::net::TcpStream;
    use tower::service_fn;

    let addr = addr.to_string();
    let endpoint = Endpoint::try_from("http://[::]").map_err(|source| ProtocolError::Transport {
        path: instance.path().display().to_string(),
        source,
    })?;

    let channel = endpoint
        .connect_with_connector(service_fn(move |_: Uri| {
            let addr = addr.clone();
            let tls = tls_config.clone();
            async move {
                let sock = TcpStream::connect(&addr).await?;
                let connector = tokio_rustls::TlsConnector::from(tls);
                let server_name = rustls::pki_types::ServerName::try_from("localhost")
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let tls_stream = connector
                    .connect(server_name, sock)
                    .await
                    .map_err(std::io::Error::other)?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(tls_stream))
            }
        }))
        .await
        .map_err(|source| ProtocolError::Transport {
            path: instance.path().display().to_string(),
            source,
        })?;
    Ok(channel)
}

async fn connect_unix(
    socket_path: &str,
    tls_config: Arc<rustls::ClientConfig>,
    instance: &PluginInstance,
) -> Result<Channel, ProtocolError> {
    use tokio::net::UnixStream;
    use tonic::transport::Endpoint;
    use tower::service_fn;

    let socket_path = socket_path.to_string();
    let socket_path_for_service = socket_path.clone();

    // Tonic needs an Endpoint even though we're not using its own
    // connector. The URI is only used to thread through the gRPC router.
    let endpoint = Endpoint::try_from("http://[::]")
        .map_err(|source| ProtocolError::Transport {
            path: instance.path().display().to_string(),
            source,
        })?;

    // Custom connector that dials the unix socket and wraps in TLS.
    let channel = endpoint
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = socket_path_for_service.clone();
            let tls = tls_config.clone();
            async move {
                let sock = UnixStream::connect(&path).await?;
                let connector = tokio_rustls::TlsConnector::from(tls);
                let server_name = rustls::pki_types::ServerName::try_from("localhost")
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let tls_stream = connector
                    .connect(server_name, sock)
                    .await
                    .map_err(std::io::Error::other)?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(tls_stream))
            }
        }))
        .await
        .map_err(|source| ProtocolError::Transport {
            path: instance.path().display().to_string(),
            source,
        })?;

    Ok(channel)
}

/// Silence dead-code warnings until these are wired into richer hover
/// output.
#[allow(dead_code)]
fn _kind_marker(k: StringKind) -> StringKind {
    k
}

#[allow(dead_code)]
fn _proto_marker(p: &proto::Schema) -> &proto::Schema {
    p
}
