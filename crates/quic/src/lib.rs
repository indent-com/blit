//! Shared QUIC endpoint construction.
//!
//! Every blit QUIC endpoint caps transmits to one datagram per sendmsg,
//! i.e. no UDP GSO batching.  The kernel's software segmentation of a
//! GSO batch emits every segment with a broken UDP checksum when the NIC
//! offers neither scatter-gather nor checksum offload (observed with
//! mt7921e WiFi on Linux 6.18.38), so the network silently drops the
//! whole batch.  The QUIC handshake's single-datagram sends go through,
//! then the first batched data burst — and all of its retransmissions,
//! batched too — vanishes: every session stalls and dies within seconds
//! while small packets still flow.  blit's bandwidths don't need the
//! syscall savings; set BLIT_WT_GSO=1 to restore batching.

use std::sync::Arc;

use web_transport_quinn as wt;

/// Caps quinn to one datagram per sendmsg (no UDP GSO batching).
struct NoGsoSocket(Arc<dyn wt::quinn::AsyncUdpSocket>);

impl std::fmt::Debug for NoGsoSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl wt::quinn::AsyncUdpSocket for NoGsoSocket {
    fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn wt::quinn::UdpPoller>> {
        self.0.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &wt::quinn::udp::Transmit) -> std::io::Result<()> {
        self.0.try_send(transmit)
    }
    fn poll_recv(
        &self,
        cx: &mut std::task::Context,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [wt::quinn::udp::RecvMeta],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.poll_recv(cx, bufs, meta)
    }
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.0.local_addr()
    }
    fn max_transmit_segments(&self) -> usize {
        1
    }
    fn max_receive_segments(&self) -> usize {
        self.0.max_receive_segments()
    }
    fn may_fragment(&self) -> bool {
        self.0.may_fragment()
    }
}

fn endpoint(
    server_config: Option<wt::quinn::ServerConfig>,
    sock: std::net::UdpSocket,
) -> Result<wt::quinn::Endpoint, String> {
    let runtime = wt::quinn::default_runtime().ok_or("no async runtime")?;
    let inner = runtime
        .wrap_udp_socket(sock)
        .map_err(|e| format!("wrap UDP socket: {e}"))?;
    let socket: Arc<dyn wt::quinn::AsyncUdpSocket> =
        if std::env::var("BLIT_WT_GSO").as_deref() == Ok("1") {
            inner
        } else {
            Arc::new(NoGsoSocket(inner))
        };
    wt::quinn::Endpoint::new_with_abstract_socket(
        wt::quinn::EndpointConfig::default(),
        server_config,
        socket,
        runtime,
    )
    .map_err(|e| format!("QUIC endpoint: {e}"))
}

/// Build a server endpoint from an already-bound socket.
pub fn server_endpoint(
    config: wt::quinn::ServerConfig,
    sock: std::net::UdpSocket,
) -> Result<wt::quinn::Endpoint, String> {
    endpoint(Some(config), sock)
}

/// Build a client endpoint on an ephemeral port, preferring dual-stack.
pub fn client_endpoint() -> Result<wt::quinn::Endpoint, String> {
    let sock = std::net::UdpSocket::bind("[::]:0")
        .or_else(|_| std::net::UdpSocket::bind("0.0.0.0:0"))
        .map_err(|e| format!("UDP socket: {e}"))?;
    endpoint(None, sock)
}

/// Build a WebTransport client with either system-root (`cert_hash: None`)
/// or pinned-certificate TLS verification.  `transport` overrides quinn's
/// transport defaults (keepalives etc.).
pub fn client(
    cert_hash: Option<&[u8]>,
    transport: Option<Arc<wt::quinn::TransportConfig>>,
) -> Result<wt::Client, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("TLS config: {e}"))?;

    let mut crypto = match cert_hash {
        Some(hash) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCert {
                hash: hash.to_vec(),
                provider,
            }))
            .with_no_client_auth(),
        None => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    crypto.alpn_protocols = vec![wt::ALPN.as_bytes().to_vec()];

    let quic_crypto = wt::quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|e| format!("QUIC TLS config: {e}"))?;
    let mut config = wt::quinn::ClientConfig::new(Arc::new(quic_crypto));
    if let Some(transport) = transport {
        config.transport_config(transport);
    }
    Ok(wt::Client::new(client_endpoint()?, config))
}

/// Pins the server's end-entity certificate to a SHA-256 hash (the
/// gateway's `serverCertificateHashes` flow, client side).  Chain and
/// expiry are deliberately not checked — the hash is the trust anchor.
#[derive(Debug)]
struct PinnedCert {
    hash: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if digest.as_ref() == self.hash.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
