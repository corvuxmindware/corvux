// bind_listener for future listener test harness
#![allow(dead_code)]

//! TLS+TCP transport with SPKI fingerprint pinning (TOFU).
//!
//! # Design
//!
//! - **Server side** uses a self-signed cert generated once per install by
//!   `rcgen` and persisted alongside the device identity. Cert material is
//!   stable across restarts so the SPKI fingerprint users verify during
//!   pairing doesn't change.
//! - **Client side** uses a custom `ServerCertVerifier` that accepts *any*
//!   valid cert chain but cross-checks the SHA-256 of the leaf cert DER
//!   against a caller-supplied `pinned` fingerprint.
//! - TLS 1.3 is preferred; TLS 1.2 is the floor (required by `rustls` 0.23
//!   default features, and lets us speak to older Android stacks if the
//!   system rustls is out of date).
//!
//! # Cert pinning vs TOFU
//!
//! On first pairing, the client has **no** pinned fingerprint. The pairing
//! flow:
//!   1. Client connects; our verifier accepts any cert and records its
//!      fingerprint as `seen`.
//!   2. The pairing handshake runs over this session.
//!   3. Post-pairing, the engine writes the seen fingerprint into the
//!      `peer_store` entry for this device.
//!   4. Subsequent connections use the stored fingerprint as `pinned`.
//!
//! The trust decision therefore rests on two things: the user verifying the
//! fingerprint *word form* in the UI on both devices (see
//! `crypto::fingerprint::Fingerprint::as_words`) and the HMAC PIN proof that
//! follows inside the TLS tunnel. A MITM who intercepts pairing cannot forge
//! the HMAC without the PIN; a MITM who steals the PIN can't race the
//! legitimate client because the latter has already TOFU-pinned the real
//! cert.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::sync::crypto::fingerprint::Fingerprint;
use crate::sync::error::TransportError;
use crate::sync::protocol::{self, Message};

use super::frame::{read_frame, write_frame, PROTOCOL_VERSION};
use super::Transport;

/// Persistent TLS material: one self-signed cert + its private key, plus
/// the derived SPKI fingerprint. Serialized alongside the device identity.
pub struct TlsIdentity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
    pub fingerprint: Fingerprint,
}

impl TlsIdentity {
    /// Generate a stable self-signed cert. Subject CN is `corvux-sync`; the
    /// cert is valid for 20 years (effectively forever — the user manages
    /// trust via fingerprint pinning, not cert validity periods).
    pub fn generate() -> Result<Self, TransportError> {
        let mut params = rcgen::CertificateParams::new(vec!["corvux-sync".to_string()])
            .map_err(|e| TransportError::TlsHandshake(e.to_string()))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "corvux-sync");
        // 20-year validity window. TOFU pinning makes this irrelevant for
        // trust; the only reason not to use 100 years is that some older
        // TLS stacks bail on unreasonably-distant expirations.
        let now = std::time::SystemTime::now();
        params.not_before = now.into();
        params.not_after = (now + Duration::from_secs(20 * 365 * 24 * 3600)).into();
        let key_pair =
            rcgen::KeyPair::generate().map_err(|e| TransportError::TlsHandshake(e.to_string()))?;
        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| TransportError::TlsHandshake(e.to_string()))?;
        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialize_der();
        let fingerprint = Fingerprint::of_cert_der(&cert_der);
        Ok(Self {
            cert_der,
            key_der,
            fingerprint,
        })
    }
}

/// A live TLS+TCP message channel. Split into read/write halves so send
/// and recv can run on separate tasks if the engine chooses.
pub struct TcpTlsTransport {
    reader: ReadHalf<tokio_rustls::TlsStream<TcpStream>>,
    writer: WriteHalf<tokio_rustls::TlsStream<TcpStream>>,
    /// The peer's cert fingerprint observed during TLS handshake. Needed
    /// after TOFU pairing so the engine can persist the fingerprint.
    peer_fingerprint: Fingerprint,
}

impl TcpTlsTransport {
    pub fn peer_fingerprint(&self) -> Fingerprint {
        self.peer_fingerprint
    }

    /// Server-side accept: returns a new transport bound to the accepted
    /// socket. Blocking-free; the caller typically spawns a task per accept.
    pub async fn accept(
        acceptor: &TlsAcceptor,
        tcp: TcpStream,
        our_fp: Fingerprint,
    ) -> Result<(Self, SocketAddr), TransportError> {
        let peer_addr = tcp.peer_addr()?;
        let tls = acceptor
            .accept(tcp)
            .await
            .map_err(|e| TransportError::TlsHandshake(e.to_string()))?;
        let tls_stream = tokio_rustls::TlsStream::Server(tls);
        let (reader, writer) = tokio::io::split(tls_stream);
        // Server-side we don't have the client's cert in mTLS-less TLS, so
        // the "peer fingerprint" from the server's perspective is our own
        // advertised cert — what the client pinned. Record that so
        // engine-level logging is consistent.
        Ok((
            TcpTlsTransport {
                reader,
                writer,
                peer_fingerprint: our_fp,
            },
            peer_addr,
        ))
    }

    /// Client-side dial. The caller MUST supply a pinned fingerprint —
    /// AUDIT-003b made TOFU/no-pin paths provably unreachable at the
    /// type level. First-pair flows go through iroh (`sync_listen_iroh`
    /// and `sync_connect_iroh`) where the NodeId IS the identity; the
    /// tcp_tls path is reconnect-only and the pin always comes from
    /// `peer_store` via `commands::sync::sync_connect`.
    ///
    /// A mismatched peer cert causes `FingerprintMismatch` at the
    /// rustls layer (surfaces as `TlsHandshake` here).
    pub async fn connect(addr: SocketAddr, pinned: Fingerprint) -> Result<Self, TransportError> {
        let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
            .await
            .map_err(|_| TransportError::ConnectTimeout)?
            .map_err(TransportError::Io)?;

        let verifier = Arc::new(PinnedVerifier::new(pinned));
        let verifier_for_read = verifier.clone();

        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();

        let connector = TlsConnector::from(Arc::new(config));
        // Server name must be syntactically valid; we use the IP literal.
        let server_name =
            ServerName::try_from("corvux-sync").expect("static server name is always valid");
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| TransportError::TlsHandshake(e.to_string()))?;
        let tls_stream = tokio_rustls::TlsStream::Client(tls);
        let (reader, writer) = tokio::io::split(tls_stream);

        let peer_fingerprint = verifier_for_read
            .observed()
            .ok_or_else(|| TransportError::TlsHandshake("no cert observed".into()))?;

        Ok(TcpTlsTransport {
            reader,
            writer,
            peer_fingerprint,
        })
    }
}

#[async_trait]
impl Transport for TcpTlsTransport {
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        let bytes =
            protocol::encode(msg).map_err(|e| TransportError::TlsHandshake(e.to_string()))?;
        write_frame(&mut self.writer, PROTOCOL_VERSION, &bytes).await
    }

    async fn recv(&mut self) -> Result<Message, TransportError> {
        let (ver, bytes) = read_frame(&mut self.reader).await?;
        if ver != PROTOCOL_VERSION {
            return Err(TransportError::UnsupportedProtocolVersion {
                expected: PROTOCOL_VERSION,
                found: ver,
            });
        }
        protocol::decode(&bytes).map_err(|e| TransportError::TlsHandshake(e.to_string()))
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        use tokio::io::AsyncWriteExt;
        self.writer.shutdown().await.ok();
        Ok(())
    }
}

// ── Server listener ─────────────────────────────────────────────────

pub fn build_server_config(id: &TlsIdentity) -> Result<Arc<ServerConfig>, TransportError> {
    let cert_chain = vec![CertificateDer::from(id.cert_der.clone())];
    let key = PrivateKeyDer::try_from(id.key_der.clone())
        .map_err(|e| TransportError::TlsHandshake(format!("key parse: {e}")))?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| TransportError::TlsHandshake(e.to_string()))?;
    Ok(Arc::new(config))
}

pub async fn bind_listener(addr: SocketAddr) -> Result<TcpListener, TransportError> {
    let listener = TcpListener::bind(addr).await?;
    Ok(listener)
}

// ── Pinned verifier ─────────────────────────────────────────────────

struct PinnedVerifier {
    /// AUDIT-003b: the pinned fingerprint is **mandatory**. The earlier
    /// `Option<Fingerprint>` shape let `None` mean "accept any cert",
    /// which made the TOFU layer load-bearing on the inner PIN
    /// handshake alone — an attacker on the same LAN with the PIN
    /// could MITM every reconnect. Requiring a pin at the type level
    /// makes that vulnerability provably unreachable: the caller can't
    /// construct a `PinnedVerifier` without supplying a fingerprint,
    /// and `TcpTlsTransport::connect` can't be called without one. The
    /// resolution of "which fingerprint" happens at the IPC layer
    /// (`commands::sync::sync_connect` looks it up from `peer_store`
    /// by address) — see Option B in the audit-053b decision block.
    pinned: Fingerprint,
    observed: std::sync::Mutex<Option<Fingerprint>>,
    /// SRX TLS-signature fix: the crypto provider's webpki signature-verification
    /// algorithms, used to actually validate the handshake signature against the
    /// pinned leaf cert's public key. Previously the two signature callbacks
    /// rubber-stamped every signature (`assertion()`), so a party that had only
    /// *observed* the victim's public cert could present it — pinning proved
    /// *which* cert, but nothing proved the peer held the matching private key.
    /// Delegating to the provider's verifiers closes that.
    verify_algs: WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for PinnedVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Manual impl: `WebPkiSupportedAlgorithms` isn't relevant to debug
        // output, and this keeps the required `ServerCertVerifier: Debug`
        // supertrait satisfied without depending on that field's Debug impl.
        f.debug_struct("PinnedVerifier")
            .field("pinned", &self.pinned)
            .finish_non_exhaustive()
    }
}

impl PinnedVerifier {
    fn new(pinned: Fingerprint) -> Self {
        // Pull the signature-verification algorithms from the ring provider —
        // the same provider installed as default at engine startup. The field
        // holds only `&'static` slices, so this is cheap to capture per dial.
        let verify_algs =
            rustls::crypto::ring::default_provider().signature_verification_algorithms;
        Self {
            pinned,
            observed: std::sync::Mutex::new(None),
            verify_algs,
        }
    }

    fn observed(&self) -> Option<Fingerprint> {
        // Recover from a poisoned mutex: rustls cert verification panicking
        // mid-write is exceedingly unlikely, but if it ever does the next
        // sync attempt should still observe the most recent fingerprint
        // rather than killing the whole transport. The data behind this
        // lock is a single `Option<Fingerprint>` (32-byte SHA-256, plain
        // value type) — there is no torn-state risk on poisoning.
        *self.observed.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = Fingerprint::of_cert_der(end_entity.as_ref());
        // Same poison-recovery rationale as `observed()` above.
        *self.observed.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
        // AUDIT-003b: no more `None → accept any cert` branch. The
        // verifier accepts iff the observed cert matches the pin, full
        // stop. Mismatch is a hard reject — caller surfaces this as
        // `FingerprintMismatch` to the user.
        if self.pinned == fp {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // SRX TLS-signature fix: ACTUALLY verify the TLS 1.2 handshake signature
        // against the pinned leaf cert's public key using the provider's webpki
        // verifiers. Cert pinning (above) proves *which* cert; this proves the
        // peer holds the matching private key. `cert` was already accepted by
        // `verify_server_cert`, so this is the second, previously-missing half.
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.verify_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // SRX TLS-signature fix: same as TLS 1.2, with TLS 1.3's tighter ECDSA
        // curve semantics handled by the provider's verifier.
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.verify_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        // Delegate to the provider so the schemes we advertise exactly match the
        // ones our verifiers can actually validate — no hand-maintained list to
        // drift out of sync with the crypto backend.
        self.verify_algs.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::protocol::Message;

    #[tokio::test]
    async fn loopback_roundtrip_pin_accepts_matching_fingerprint() {
        let id = TlsIdentity::generate().unwrap();
        let fp = id.fingerprint;

        // Install default ring-based crypto provider (required by rustls 0.23).
        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_cfg = build_server_config(&id).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let our_fp = id.fingerprint;

        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let acceptor = TlsAcceptor::from(server_cfg);
            let (mut t, _) = TcpTlsTransport::accept(&acceptor, tcp, our_fp)
                .await
                .unwrap();
            let msg = t.recv().await.unwrap();
            assert!(matches!(msg, Message::Ping));
            t.send(&Message::Pong).await.unwrap();
        });

        let mut client = match TcpTlsTransport::connect(addr, fp).await {
            Ok(c) => c,
            Err(e) => panic!("client connect failed: {e:?}"),
        };
        client.send(&Message::Ping).await.unwrap();
        let reply = client.recv().await.unwrap();
        assert!(matches!(reply, Message::Pong));
        assert_eq!(client.peer_fingerprint(), fp);
    }

    #[tokio::test]
    async fn wrong_pin_rejects_connection() {
        let id = TlsIdentity::generate().unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();

        let server_cfg = build_server_config(&id).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let our_fp = id.fingerprint;

        tokio::spawn(async move {
            if let Ok((tcp, _)) = listener.accept().await {
                let acceptor = TlsAcceptor::from(server_cfg);
                let _ = TcpTlsTransport::accept(&acceptor, tcp, our_fp).await;
            }
        });

        // Wrong fingerprint — all zeros. AUDIT-003b: the dial requires
        // a pinned Fingerprint at the type level, so the previous
        // `Some(bad)` form is now just `bad`. The mismatch rejection
        // semantics are unchanged — what changed is that the
        // "no pin at all → accept anything" branch is gone.
        let bad = Fingerprint([0u8; 32]);
        match TcpTlsTransport::connect(addr, bad).await {
            Err(TransportError::TlsHandshake(_)) => {}
            Err(other) => panic!("expected TlsHandshake, got {other:?}"),
            Ok(_) => panic!("expected TLS failure, but connection succeeded"),
        }
    }

    /// SRX TLS-signature regression: an attacker who has only OBSERVED the
    /// victim's (public) cert presents it verbatim — so fingerprint pinning
    /// passes — but signs the handshake with its OWN private key because it
    /// doesn't hold the victim's. Before the fix the signature callbacks
    /// rubber-stamped every signature, so this MITM succeeded. Now the handshake
    /// signature is verified against the pinned cert's public key, so the
    /// connection MUST be refused.
    ///
    /// Robust to where rustls draws the line: `with_single_cert` MAY reject a
    /// key that doesn't match the leaf cert at config-build time — that also
    /// blocks the attack (just earlier), so the test treats a build-time
    /// rejection as a pass. The security property is "a stolen public cert
    /// without its private key cannot establish a pinned connection".
    #[tokio::test]
    async fn stolen_cert_without_matching_key_is_rejected() {
        let victim = TlsIdentity::generate().unwrap();
        let attacker = TlsIdentity::generate().unwrap(); // a DIFFERENT keypair
        assert_ne!(
            victim.key_der, attacker.key_der,
            "test setup: attacker must hold a different private key"
        );
        let _ = rustls::crypto::ring::default_provider().install_default();
        let pinned = victim.fingerprint;

        // Attacker presents the victim's cert but its own key.
        let cert_chain = vec![CertificateDer::from(victim.cert_der.clone())];
        let key = PrivateKeyDer::try_from(attacker.key_der.clone()).unwrap();
        let server_cfg = match ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
        {
            Ok(cfg) => Arc::new(cfg),
            // rustls refused the cert/key mismatch at build time — the attack is
            // blocked here instead. Property still holds.
            Err(_) => return,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let victim_fp = victim.fingerprint;
        tokio::spawn(async move {
            if let Ok((tcp, _)) = listener.accept().await {
                let acceptor = TlsAcceptor::from(server_cfg);
                // The server-side handshake will fail once the client rejects
                // the signature; swallow the result.
                let _ = TcpTlsTransport::accept(&acceptor, tcp, victim_fp).await;
            }
        });

        // The client pins the victim's fingerprint. `verify_server_cert` passes
        // (the presented cert IS the victim's), but signature verification
        // against the victim's public key now fails because the handshake was
        // signed with the attacker's key.
        match TcpTlsTransport::connect(addr, pinned).await {
            Err(_) => {} // rejected — correct
            Ok(_) => {
                panic!(
                    "a stolen public cert without its private key must NOT establish a connection"
                )
            }
        }
    }

    /// SRX: the advertised verify-schemes come from the crypto provider (and are
    /// non-empty), proving we delegate real verification rather than returning a
    /// hand-maintained list that could drift from what the verifiers accept.
    #[test]
    fn supported_verify_schemes_delegate_to_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let verifier = PinnedVerifier::new(Fingerprint([1u8; 32]));
        let schemes = verifier.supported_verify_schemes();
        assert!(
            !schemes.is_empty(),
            "verifier must advertise the provider's real signature schemes"
        );
        assert_eq!(
            schemes,
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes(),
            "advertised schemes must be exactly the provider's"
        );
    }
}
