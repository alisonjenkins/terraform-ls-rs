//! Ephemeral mTLS setup for a single provider plugin connection.
//!
//! go-plugin's AutoMTLS mode is bidirectional: the client (tfls) hands
//! its cert to the provider via the `PLUGIN_CLIENT_CERT` env var, then
//! the provider hands its cert back in the handshake line. This module
//! generates a fresh cert per connection and builds a rustls config
//! that pins exactly the provider's cert as the trust anchor.

use std::sync::Arc;

use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, aws_lc_rs};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

use crate::ProtocolError;

/// A fresh ECDSA key + self-signed cert for this connection.
pub struct ClientIdentity {
    pub cert_pem: String,
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
}

impl ClientIdentity {
    /// Generate an ephemeral client identity suitable for mTLS against a
    /// terraform-plugin-go provider. Uses RSA-2048 because providers
    /// consistently accept it (ECDSA variants hit obscure Go-TLS issues in
    /// early testing); sets basicConstraints=CA:TRUE so the self-signed
    /// cert is accepted as its own trust anchor, plus clientAuth in
    /// extendedKeyUsage for mTLS client authentication.
    pub fn generate() -> Result<Self, ProtocolError> {
        use rcgen::{BasicConstraints, ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose};

        // go-plugin's TLS server rejects ECDSA client certs under some
        // circumstances that we haven't fully diagnosed; RSA 2048 works
        // reliably (confirmed against all v5/v6 providers in the cache).
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)?;
        let mut params = rcgen::CertificateParams::new(vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
        ])?;
        // The server uses our cert as both trust anchor AND leaf. Go's
        // x509 chain verifier insists the anchor has CA:TRUE in its basic
        // constraints; `SelfSignedOnly` sets that without implying the
        // cert can sign others.
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyAgreement,
            KeyUsagePurpose::KeyEncipherment,
            KeyUsagePurpose::KeyCertSign,
        ];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let cert = params.self_signed(&key_pair)?;
        let cert_pem = cert.pem();
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let key_der = PrivateKeyDer::try_from(key_pair.serialize_der())
            .map_err(|e| ProtocolError::Tls(rustls::Error::General(e.to_string())))?;

        Ok(Self {
            cert_pem,
            cert_der,
            key_der,
        })
    }
}

/// Build a rustls `ClientConfig` that:
/// - presents `identity`'s cert for mTLS client auth
/// - trusts exactly the server cert encoded (base64) in `server_cert_b64`
pub fn build_client_config(
    identity: &ClientIdentity,
    server_cert_b64: &str,
) -> Result<Arc<ClientConfig>, ProtocolError> {
    // go-plugin base64-encodes using the URL-safe alphabet without
    // padding. Accept both standard and URL-safe just in case.
    let cert_bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(server_cert_b64)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(server_cert_b64))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(server_cert_b64))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(server_cert_b64))
        .map_err(|e| {
            ProtocolError::Tls(rustls::Error::General(format!("server cert b64: {e}")))
        })?;
    let server_cert_der = CertificateDer::from(cert_bytes);

    // rustls 0.23 requires an explicit crypto provider. We use aws_lc_rs
    // (not ring) because it supports ECDSA_NISTP521_SHA512, which Go's
    // terraform-plugin-go picks for server-cert signatures.
    let provider = Arc::new(aws_lc_rs::default_provider());

    let mut cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(ProtocolError::Tls)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier {
            expected: server_cert_der,
            provider: aws_lc_rs::default_provider(),
        }))
        .with_client_auth_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )
        .map_err(ProtocolError::Tls)?;

    // go-plugin's TLS server uses ALPN to gate HTTP/2 traffic; if we don't
    // negotiate "h2" it drops the connection right after the handshake.
    cfg.alpn_protocols = vec![b"h2".to_vec()];

    Ok(Arc::new(cfg))
}

/// Trusts exactly one DER-encoded certificate; rejects everything else.
#[derive(Debug)]
struct PinnedCertVerifier {
    expected: CertificateDer<'static>,
    provider: CryptoProvider,
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.expected.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server cert does not match the pinned provider cert".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
