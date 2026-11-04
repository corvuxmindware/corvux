// Pin::generate exposed for future re-pairing UX
#![allow(dead_code)]

//! PIN-based pairing: challenge-response → session keys.
//!
//! # Threat model
//!
//! PIN space is small (10^6). A passive eavesdropper on the TLS connection
//! sees nothing (cert pinning + TLS 1.3), but an *active* attacker who can
//! MITM the very first pairing can brute-force the PIN offline if we leak
//! any PIN-derived value. So:
//!
//! - The PIN is **never sent over the wire**, in any form.
//! - Only an HMAC-SHA256 *proof* is sent — client proves knowledge of the
//!   PIN by signing a server-supplied 256-bit nonce with it.
//! - Session keys are derived from the PIN+nonce via HKDF-SHA256, so the
//!   PIN is not used as a long-term key (and is zeroized immediately after
//!   the derivation).
//! - Rate limiting: after N failed verifications, the server imposes
//!   exponential backoff. This turns a 10^6 brute force from "seconds" into
//!   "years" even with a compromised transport.
//!
//! # Protocol (happy path)
//!
//! ```text
//!     Client                          Server
//!       │                                │
//!       │    1. OPEN (TLS + cert pin)    │
//!       │──────────────────────────────▶ │
//!       │                                │ generate nonce N (32 B, OsRng)
//!       │    2. PairingChallenge{N}      │
//!       │ ◀──────────────────────────────│
//!       │ proof = HMAC(PIN, N)           │
//!       │ sk    = HKDF(PIN, N, "session")│
//!       │ zeroize(PIN)                   │
//!       │    3. PairingProof{proof}      │
//!       │──────────────────────────────▶ │ verify proof == HMAC(PIN, N)
//!       │                                │ derive same sk
//!       │                                │ zeroize(PIN)
//!       │    4. PairingOk{}              │
//!       │ ◀──────────────────────────────│
//! ```
//!
//! # PIN format
//!
//! Server generates a 6-digit PIN (`100000..=999999`) via `OsRng` and
//! displays it in the Settings UI. The human types the same value on the
//! client. ASCII-digit encoding (`"123456"`) is what we HMAC over, so both
//! sides must canonicalize the same way. We strip whitespace and validate
//! `^[0-9]{6}$` before use — any other input is a programming bug.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

use crate::sync::error::{CryptoError, SyncResult};

type HmacSha256 = Hmac<Sha256>;

/// 6-digit pairing PIN. The underlying `String` is zeroized on drop.
#[derive(Clone)]
pub struct Pin(Zeroizing<String>);

impl Pin {
    /// Server-side: generate a random 6-digit PIN via OsRng.
    pub fn generate() -> Self {
        let mut rng = OsRng;
        let n = (rng.next_u32() % 900_000) + 100_000;
        Self(Zeroizing::new(format!("{n}")))
    }

    /// Client-side: accept user input. Strips whitespace, rejects anything
    /// but 6 decimal digits.
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed: String = input.chars().filter(|c| !c.is_whitespace()).collect();
        if trimmed.len() == 6 && trimmed.chars().all(|c| c.is_ascii_digit()) {
            Some(Self(Zeroizing::new(trimmed)))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the PIN, even in debug logs.
        f.write_str("Pin(******)")
    }
}

/// A 32-byte high-entropy one-time pairing secret (SRX layer 3).
///
/// It rides *inside the QR pairing ticket* — carried out of band (like the PIN),
/// NEVER on the wire. When present it is folded into the first-pair proof and
/// session key via [`first_pair_key`], so QR/ticket pairing can't be
/// brute-forced: an attacker who never obtained the ticket lacks the 128+ bits
/// of secret and can't derive the proof key regardless of the 6-digit PIN. The
/// PIN remains as the separate out-of-band human confirmation. Bytes are
/// zeroized on drop.
#[derive(Clone)]
pub struct PairingSecret(Zeroizing<[u8; 32]>);

impl PairingSecret {
    /// Mint a fresh secret from the OS CSPRNG. Called once per "Show code".
    pub fn generate() -> Self {
        let mut b = [0u8; 32];
        OsRng.fill_bytes(&mut b);
        Self(Zeroizing::new(b))
    }

    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(Zeroizing::new(b))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// URL-safe, unpadded base64 — the form embedded in the ticket JSON.
    pub fn to_b64(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.0.as_ref())
    }

    /// Parse the ticket-embedded base64 form. `None` on any malformed input or
    /// wrong length (a ticket that didn't carry a valid secret → treated as
    /// "no secret", i.e. the PIN-only path).
    pub fn from_b64(s: &str) -> Option<Self> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.as_bytes())
            .ok()?;
        let arr: [u8; 32] = bytes.try_into().ok()?;
        Some(Self(Zeroizing::new(arr)))
    }
}

impl std::fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret, even in debug logs.
        f.write_str("PairingSecret(****)")
    }
}

/// 32-byte HMAC-SHA256 pairing proof. Attacker-observable; safe to log
/// *once*, but don't store it — it's single-use by construction (bound to a
/// fresh nonce per pairing attempt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proof(pub [u8; 32]);

/// 32-byte pairing nonce from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nonce(pub [u8; 32]);

impl Nonce {
    pub fn generate() -> Self {
        let mut n = [0u8; 32];
        OsRng.fill_bytes(&mut n);
        Self(n)
    }
}

/// 32-byte derived session secret. Zeroized on drop.
///
/// This is a shared secret between the two peers after successful pairing.
/// All subsequent message authentication (for the sync protocol itself)
/// derives from this via HKDF with distinct `info` strings per key.
#[derive(Zeroize)]
#[zeroize(drop)]
pub struct SessionSecret(pub [u8; 32]);

impl std::fmt::Debug for SessionSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionSecret(****)")
    }
}

/// Domain-separation constant for HKDF. Bump if the derivation changes so
/// old and new clients never produce compatible-looking keys by accident.
/// v2 folds the ephemeral X25519 ECDH secret into the IKM (H-2/H-3 forward
/// secrecy), so it is deliberately incompatible with v1's PIN/token-only
/// derivation.
const HKDF_INFO: &[u8] = b"corvux-sync-pairing-v2";

/// Generate a one-shot ephemeral X25519 keypair for a single handshake
/// (H-2/H-3). Returns the secret — held until the peer's public key arrives —
/// and our public-key bytes to put on the wire. The secret is consumed by
/// [`ecdh_shared`], so it can never be reused across handshakes: that is what
/// gives per-handshake forward secrecy.
pub fn generate_ephemeral() -> (x25519_dalek::EphemeralSecret, [u8; 32]) {
    let secret = x25519_dalek::EphemeralSecret::random_from_rng(OsRng);
    let public = x25519_dalek::PublicKey::from(&secret);
    (secret, *public.as_bytes())
}

/// Complete the X25519 exchange: our ephemeral secret + the peer's public
/// bytes → the 32-byte shared secret to mix into the session key. Consumes the
/// secret (one-shot). Both peers arrive at the same value.
///
/// Rejects a **non-contributory** result: the small set of low-order Curve25519
/// points (including all-zero and the twist points) drive the DH output to a
/// fixed value regardless of our secret, so a peer that sends one is trying to
/// force a known/attacker-controlled shared secret rather than contribute real
/// entropy. `was_contributory()` is false exactly then; we refuse instead of
/// mixing a predictable value into the session key.
pub fn ecdh_shared(
    secret: x25519_dalek::EphemeralSecret,
    peer_public: &[u8; 32],
) -> SyncResult<[u8; 32]> {
    let peer = x25519_dalek::PublicKey::from(*peer_public);
    let shared = secret.diffie_hellman(&peer);
    if !shared.was_contributory() {
        return Err(CryptoError::NonContributoryEcdh.into());
    }
    Ok(*shared.as_bytes())
}

/// HKDF label for the per-peer resumption token (SEC-2). Both sides derive
/// `HKDF(first-pair SessionSecret, this-label)` and persist it; thereafter
/// reconnects authenticate with the token instead of re-proving the human PIN,
/// so a PIN that leaks AFTER pairing is no longer a permanent roost key.
pub const RESUMPTION_TOKEN_LABEL: &[u8] = b"corvux-resumption-token-v1";

/// HMAC-SHA256 proof keyed by arbitrary key material (PIN bytes for first pair,
/// or a 32-byte resumption token for reconnect), over `nonce ‖ transcript`.
/// `transcript` is the ephemeral-DH transcript (`client_eph_pub ‖
/// server_eph_pub`); binding it here is channel binding (H-1/H-2): an active
/// MITM that substitutes its own ephemeral keys makes the two sides see
/// different transcripts, so the proof it relays no longer verifies — the MITM
/// would need the PIN/token to forge a valid proof. Both sides compute it; the
/// server re-computes to verify.
pub fn proof_from_key(key: &[u8], nonce: &Nonce, transcript: &[u8]) -> SyncResult<Proof> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| CryptoError::HmacLength)?;
    mac.update(&nonce.0);
    mac.update(transcript);
    let digest = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(Proof(out))
}

/// Derive a 32-byte session secret from arbitrary key material + nonce + the
/// ephemeral ECDH shared secret. `key` (PIN or resumption token) is the HKDF
/// salt; the IKM is `nonce ‖ ecdh` so the ephemeral DH output contributes
/// entropy AND forward secrecy — a compromise of `key` alone can't reconstruct
/// past/future session keys without the (discarded) ephemeral secrets.
pub fn derive_session_from_key(
    key: &[u8],
    nonce: &Nonce,
    ecdh: &[u8; 32],
) -> SyncResult<SessionSecret> {
    let mut ikm = Vec::with_capacity(64);
    ikm.extend_from_slice(&nonce.0);
    ikm.extend_from_slice(ecdh);
    let hk = Hkdf::<Sha256>::new(Some(key), &ikm);
    let mut okm = [0u8; 32];
    let result = hk.expand(HKDF_INFO, &mut okm).map_err(|_| CryptoError::Kdf);
    ikm.zeroize();
    result?;
    Ok(SessionSecret(okm))
}

/// Verify a proof against arbitrary key material. Constant-time compare.
pub fn verify_proof_with_key(
    key: &[u8],
    nonce: &Nonce,
    transcript: &[u8],
    claimed: &Proof,
) -> SyncResult<()> {
    let expected = proof_from_key(key, nonce, transcript)?;
    if expected.0.ct_eq(&claimed.0).into() {
        Ok(())
    } else {
        Err(CryptoError::WrongPin.into())
    }
}

/// Compute `HMAC-SHA256(PIN, nonce)`. The return is the client's proof sent
/// to the server. Called identically on both sides; the server re-computes
/// to verify.
pub fn proof_from_pin(pin: &Pin, nonce: &Nonce, transcript: &[u8]) -> SyncResult<Proof> {
    proof_from_key(pin.as_str().as_bytes(), nonce, transcript)
}

/// SRX layer 3: derive the FIRST-PAIR key material from the human PIN and an
/// optional high-entropy [`PairingSecret`] carried in the QR ticket.
///
/// - **With a secret** (QR/ticket pairing): `HMAC-SHA256(key = secret, msg = PIN)`
///   — a 32-byte key that depends on BOTH inputs. The secret dominates the
///   entropy, so the resulting proof/session key is unguessable to anyone who
///   didn't obtain the ticket, *regardless of the 6-digit PIN*. This is what
///   makes ticket pairing non-brute-forceable. Enforcement is emergent, not a
///   wire flag: the host folds its minted secret in here, so a caller that
///   didn't present the ticket derives a different key and its proof simply
///   fails to verify.
/// - **Without a secret** (legacy / no-ticket fallback): the PIN bytes alone,
///   identical to the pre-layer-3 behavior — still gated by the enrollment
///   window + global cap (layers 1/2).
///
/// The key is domain-separated from the raw PIN so a PIN-only proof can never be
/// mistaken for a secret-bound one. Returned zeroizing so the combined material
/// is wiped after the proof/session derivation consumes it.
pub fn first_pair_key(pin: &Pin, secret: Option<&PairingSecret>) -> Zeroizing<Vec<u8>> {
    match secret {
        None => Zeroizing::new(pin.as_str().as_bytes().to_vec()),
        Some(sec) => {
            // HMAC(secret, PIN): keyed by the high-entropy secret, so the output
            // inherits its unguessability; binding the PIN means both are needed.
            let mut mac = <HmacSha256 as Mac>::new_from_slice(sec.as_bytes())
                .expect("HMAC accepts a 32-byte key");
            mac.update(pin.as_str().as_bytes());
            Zeroizing::new(mac.finalize().into_bytes().to_vec())
        }
    }
}

/// Derive a 32-byte session secret. Both sides run this independently and
/// must arrive at the same bytes — the PIN is `salt`, the nonce is the IKM.
///
/// Why salt=PIN: HKDF's salt is optional and used to extract entropy from
/// low-entropy IKM. The PIN is the low-entropy end here, so we keep the
/// convention that our long-lived secret is in the salt position.
pub fn derive_session_secret(
    pin: &Pin,
    nonce: &Nonce,
    ecdh: &[u8; 32],
) -> SyncResult<SessionSecret> {
    derive_session_from_key(pin.as_str().as_bytes(), nonce, ecdh)
}

/// Verify a client-supplied proof against the server's nonce and expected
/// PIN. Constant-time compare — never early-return.
pub fn verify_proof(
    pin: &Pin,
    nonce: &Nonce,
    transcript: &[u8],
    claimed: &Proof,
) -> SyncResult<()> {
    verify_proof_with_key(pin.as_str().as_bytes(), nonce, transcript, claimed)
}

/// 32-byte symmetric key with **enforced HKDF domain separation**.
///
/// AUDIT-048: the only way to construct one is via [`derive_subkey`],
/// which takes a [`SessionSecret`] and a label. This makes it
/// impossible at the type level to feed the raw `SessionSecret` bytes
/// directly into a downstream cipher (which would key two different
/// purposes off the same material — a classic crypto foot-gun).
///
/// The inner field is private; reach the bytes via [`DomainSeparatedKey::expose`].
/// The key is zeroized when this value drops.
pub struct DomainSeparatedKey([u8; 32]);

impl DomainSeparatedKey {
    /// Borrow the raw key bytes. Intended only for plumbing into a
    /// concrete cipher constructor (e.g. `XChaCha20Poly1305::new`) —
    /// not for storage, comparison, or further derivation. Use a fresh
    /// `derive_subkey` call with a different label for any second purpose.
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    /// AUDIT-048 escape hatch — re-wrap bytes that were ORIGINALLY
    /// produced by [`derive_subkey`] and have since been persisted to
    /// the peer store. Call sites must verify the byte source: it MUST
    /// be a previously-derived subkey, never the raw `SessionSecret`
    /// or any other 32-byte material. The `pub(crate)` visibility
    /// confines uses to the sync module, where the persistence layer
    /// can audit them in one place.
    ///
    /// If you find yourself reaching for this from outside the
    /// persistence boundary, you almost certainly want a fresh
    /// `derive_subkey(session, label)` call instead.
    pub(crate) fn from_derived_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl Drop for DomainSeparatedKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for DomainSeparatedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DomainSeparatedKey(****)")
    }
}

/// Derive a domain-separated 32-byte subkey from the session secret.
/// Callers pick labels like `"frame-mac"` or `"notify-channel"` to get
/// distinct keys without needing a second pairing.
///
/// AUDIT-048: returns [`DomainSeparatedKey`] (newtype) instead of a
/// raw `[u8; 32]` so the type system tracks that the bytes ARE the
/// output of a labelled HKDF call — downstream consumers can demand
/// the wrapper instead of bare bytes and the compiler enforces the
/// rule.
pub fn derive_subkey(session: &SessionSecret, label: &[u8]) -> SyncResult<DomainSeparatedKey> {
    let hk = Hkdf::<Sha256>::new(None, &session.0);
    let mut okm = [0u8; 32];
    hk.expand(label, &mut okm).map_err(|_| CryptoError::Kdf)?;
    Ok(DomainSeparatedKey(okm))
}

// ── Rate limiter for failed pairings ──────────────────────────────

/// Simple exponential-backoff rate limiter for failed pairing attempts.
/// Tracked per-IP on the server side by the engine; this struct just
/// computes the delay and decides whether to accept.
///
/// Formula: base delay 1s, doubles each failure, capped at 10 min.
/// 10 consecutive failures → ~10 min lockout → brute-forcing 10^6 PINs takes
/// ~19 years of sustained attempts.
#[derive(Debug, Default, Clone)]
pub struct PairingRateLimiter {
    failures: u32,
    last_attempt: Option<std::time::Instant>,
}

impl PairingRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `Ok(())` if an attempt is allowed, `Err(RateLimited)` if not.
    pub fn check(&self) -> SyncResult<()> {
        let Some(last) = self.last_attempt else {
            return Ok(());
        };
        let wait = self.current_wait();
        if last.elapsed() < wait {
            Err(CryptoError::RateLimited.into())
        } else {
            Ok(())
        }
    }

    /// Atomically check the limit AND, if allowed, reserve the slot by
    /// recording a provisional failure. This closes the concurrent-handshake
    /// bypass: `check()` alone lets N connections opened in parallel all pass
    /// before any of them records a failure, so a fresh-NodeId-per-connection
    /// attacker could issue thousands of PIN guesses inside one backoff window.
    /// Reserving at challenge-issue time means the 2nd concurrent attempt
    /// already sees a bumped counter and is throttled.
    ///
    /// A successful proof MUST call [`record_success`] to clear the reservation;
    /// an abandoned or wrong-PIN attempt leaves the reservation as the failure
    /// count, so callers must NOT also call [`record_failure`] on this path.
    pub fn check_and_reserve(&mut self) -> SyncResult<()> {
        self.check()?;
        self.failures = self.failures.saturating_add(1);
        self.last_attempt = Some(std::time::Instant::now());
        Ok(())
    }

    pub fn record_success(&mut self) {
        self.failures = 0;
        self.last_attempt = None;
    }

    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
        self.last_attempt = Some(std::time::Instant::now());
    }

    fn current_wait(&self) -> std::time::Duration {
        // 2^(failures-1) seconds, capped. failures=0 → 0s.
        if self.failures == 0 {
            return std::time::Duration::ZERO;
        }
        let exp = (self.failures - 1).min(10); // cap at 2^10 = 1024s
        std::time::Duration::from_secs(1u64 << exp).min(std::time::Duration::from_secs(600))
    }

    /// True once this limiter has been idle long enough that forgetting it
    /// loses nothing (the source would already be past its max lockout). Used
    /// by [`KeyedPairingRateLimiter`] to bound memory. See M-11.
    fn is_cold(&self) -> bool {
        match self.last_attempt {
            None => true,
            Some(t) => t.elapsed() > std::time::Duration::from_secs(1200),
        }
    }
}

/// Per-source pairing rate limiter (M-11). The engine holds ONE of these,
/// shared across every accepted connection, but backoff is tracked per source
/// key (the remote IP) rather than globally — so one abusive source (or one
/// flappy reconnect loop) can no longer drive a single global counter to the
/// lockout cap and deny pairing to every other peer. Cold entries are GC'd on
/// access so the map can't grow without bound.
#[derive(Debug, Default)]
pub struct KeyedPairingRateLimiter {
    per_key: std::collections::HashMap<String, PairingRateLimiter>,
}

impl KeyedPairingRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check-and-reserve against `key`'s own backoff. See
    /// [`PairingRateLimiter::check_and_reserve`].
    pub fn check_and_reserve(&mut self, key: &str) -> SyncResult<()> {
        self.gc_cold();
        self.per_key
            .entry(key.to_string())
            .or_default()
            .check_and_reserve()
    }

    /// Clear `key`'s reservation on a successful proof and drop the entry.
    pub fn record_success(&mut self, key: &str) {
        self.per_key.remove(key);
    }

    /// Number of tracked source keys (diagnostics / tests).
    pub fn tracked_keys(&self) -> usize {
        self.per_key.len()
    }

    fn gc_cold(&mut self) {
        self.per_key.retain(|_, rl| !rl.is_cold());
    }
}

// ── First-pair enrollment window (brute-force hardening) ──────────

/// How long a first-pair enrollment window stays open after the user takes an
/// explicit pairing action ("Show code" / scan+PIN). Long enough to type a PIN
/// on the other device, short enough that the first-pair attack surface is a
/// few minutes rather than "whenever sync is on".
pub const ENROLLMENT_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

/// Global cap on FIRST-PAIR failures within a single open window, across ALL
/// sources. The per-source [`KeyedPairingRateLimiter`] is defeated by an
/// attacker who mints a fresh iroh NodeId per guess (each gets an always-empty
/// per-key bucket); this counter is keyed on nothing, so fresh-NodeId spraying
/// trips a hard lockout regardless. Scoped to the open window so it can never
/// deny pairing permanently — closing/reopening the window (a fresh "Show
/// code") resets it. 10 is generous for PIN typos from a legitimate device
/// (which hits the gentler per-key exponential backoff first).
pub const GLOBAL_FIRST_PAIR_CAP: u32 = 10;

/// Outcome of gating a first-pair attempt against the enrollment window.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstPairGate {
    /// Proceed — the window is open and under the global cap.
    Allow,
    /// The window is not open (no active pairing). Reject before the PIN check.
    Closed,
    /// The window is open but the global failure cap tripped. Reject.
    Locked,
}

/// First-pair enrollment window + source-independent global failure counter.
/// One per engine, shared across every accepted connection (like
/// [`KeyedPairingRateLimiter`]).
///
/// Rationale (SRX brute-force fix): the per-source rate limiter keys backoff on
/// the caller's iroh NodeId, which an attacker regenerates per guess for an
/// always-empty bucket — so a sticky 6-digit PIN was brute-forceable *whenever
/// sync was on*. Gating first-pair on an explicit, time-boxed window (opened
/// only when the user is actively pairing) plus a source-independent global cap
/// closes that: outside the window first-pair is refused entirely, and inside it
/// fresh-NodeId spraying trips the global lock. Known peers reconnect via
/// resumption token and are NEVER gated by this.
#[derive(Debug, Default)]
pub struct EnrollmentWindow {
    open_until: Option<std::time::Instant>,
    global_failures: u32,
}

impl EnrollmentWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open (or extend) the window for `dur` and clear the global failure
    /// counter. Called when the user takes an explicit pairing action.
    pub fn open(&mut self, dur: std::time::Duration) {
        self.open_until = Some(std::time::Instant::now() + dur);
        self.global_failures = 0;
    }

    /// Close the window and clear the counter. Called on engine stop / when
    /// pairing is finished.
    pub fn close(&mut self) {
        self.open_until = None;
        self.global_failures = 0;
    }

    /// Whether the window is currently open (and not expired).
    pub fn is_open(&self) -> bool {
        match self.open_until {
            Some(deadline) => std::time::Instant::now() < deadline,
            None => false,
        }
    }

    /// Gate a first-pair attempt AND, if allowed, reserve the slot by counting
    /// it — mirroring [`PairingRateLimiter::check_and_reserve`], so
    /// concurrently-opened connections can't all pass a read-only check before
    /// any records a failure. A successful proof MUST call [`Self::record_success`];
    /// an abandoned or wrong-PIN attempt leaves the reservation as the failure
    /// count (so callers must NOT double-count on the failure path).
    pub fn check_and_reserve_first_pair(&mut self) -> FirstPairGate {
        if !self.is_open() {
            return FirstPairGate::Closed;
        }
        if self.global_failures >= GLOBAL_FIRST_PAIR_CAP {
            return FirstPairGate::Locked;
        }
        self.global_failures = self.global_failures.saturating_add(1);
        FirstPairGate::Allow
    }

    /// Clear the reservation on a successful first pair.
    pub fn record_success(&mut self) {
        self.global_failures = 0;
    }

    /// Current global failure count (diagnostics / tests).
    pub fn failure_count(&self) -> u32 {
        self.global_failures
    }
}

/// Engine-shared enrollment window handle. See [`EnrollmentWindow`].
pub type SharedEnrollmentWindow = std::sync::Arc<std::sync::Mutex<EnrollmentWindow>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecdh_agrees_on_both_sides() {
        // Two honest ephemeral keypairs must derive the same contributory secret.
        let (a_secret, a_pub) = generate_ephemeral();
        let (b_secret, b_pub) = generate_ephemeral();
        let a = ecdh_shared(a_secret, &b_pub).unwrap();
        let b = ecdh_shared(b_secret, &a_pub).unwrap();
        assert_eq!(a, b, "both sides must reach the same ECDH secret");
        assert_ne!(a, [0u8; 32], "a healthy exchange is never all-zero");
    }

    #[test]
    fn ecdh_rejects_low_order_point() {
        // A peer sending a low-order point forces a non-contributory (all-zero)
        // shared secret regardless of our secret — refuse it instead of mixing a
        // predictable value into the session key. All-zeros is the canonical
        // low-order/identity point on Curve25519.
        let (secret, _pub) = generate_ephemeral();
        let low_order = [0u8; 32];
        let res = ecdh_shared(secret, &low_order);
        assert!(
            matches!(
                res,
                Err(crate::sync::error::SyncError::Crypto(
                    CryptoError::NonContributoryEcdh
                ))
            ),
            "low-order peer key must be rejected, got {res:?}"
        );
    }

    #[test]
    fn pin_parse_accepts_six_digits() {
        assert!(Pin::parse("123456").is_some());
        assert!(Pin::parse(" 123 456 ").is_some());
    }

    #[test]
    fn pin_parse_rejects_non_numeric() {
        assert!(Pin::parse("12345").is_none());
        assert!(Pin::parse("1234567").is_none());
        assert!(Pin::parse("12345a").is_none());
    }

    #[test]
    fn proof_matches_on_both_sides() {
        let pin = Pin::parse("123456").unwrap();
        let nonce = Nonce::generate();
        let p1 = proof_from_pin(&pin, &nonce, b"").unwrap();
        let p2 = proof_from_pin(&pin, &nonce, b"").unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn verify_accepts_correct_proof() {
        let pin = Pin::parse("123456").unwrap();
        let nonce = Nonce::generate();
        let proof = proof_from_pin(&pin, &nonce, b"").unwrap();
        verify_proof(&pin, &nonce, b"", &proof).unwrap();
    }

    #[test]
    fn verify_rejects_wrong_pin() {
        let pin_a = Pin::parse("123456").unwrap();
        let pin_b = Pin::parse("654321").unwrap();
        let nonce = Nonce::generate();
        let proof = proof_from_pin(&pin_a, &nonce, b"").unwrap();
        assert!(verify_proof(&pin_b, &nonce, b"", &proof).is_err());
    }

    #[test]
    fn session_keys_agree_and_differ_from_proof() {
        let pin = Pin::parse("424242").unwrap();
        let nonce = Nonce::generate();
        let sk1 = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        let sk2 = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        let proof = proof_from_pin(&pin, &nonce, b"").unwrap();
        assert_eq!(sk1.0, sk2.0);
        assert_ne!(sk1.0, proof.0); // domain separation via HKDF info
    }

    #[test]
    fn subkeys_differ_by_label() {
        let pin = Pin::parse("000000").unwrap();
        let nonce = Nonce::generate();
        let sk = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        let a = derive_subkey(&sk, b"a").unwrap();
        let b = derive_subkey(&sk, b"b").unwrap();
        assert_ne!(a.expose(), b.expose());
    }

    /// AUDIT-048: `DomainSeparatedKey` is intentionally not `Clone`,
    /// not `Copy`, not `Eq` — so the type can't accidentally fan out
    /// or get compared in non-constant time. This test pins the
    /// "construction is gated through `derive_subkey` + escape hatch"
    /// invariant by exercising the only two constructor paths and
    /// confirming a fresh subkey differs from the master session secret.
    #[test]
    fn domain_separated_key_diverges_from_session_secret() {
        let pin = Pin::parse("424242").unwrap();
        let nonce = Nonce::generate();
        let session = derive_session_secret(&pin, &nonce, &[0u8; 32]).unwrap();
        let sub = derive_subkey(&session, b"corvux-app-envelope-v1").unwrap();
        // If `from_raw_key` had accepted raw session bytes (the pre-fix
        // shape), an attacker who learned the SessionSecret would
        // immediately have the envelope key — same bytes, same MAC.
        // HKDF with a label moves the subkey somewhere else.
        assert_ne!(
            sub.expose().as_slice(),
            session.0.as_slice(),
            "subkey must NOT equal session secret"
        );
    }

    /// `from_derived_bytes` exists only inside the sync crate. Sanity
    /// check that the bytes round-trip exactly — the escape hatch must
    /// not silently transform the key.
    #[test]
    fn from_derived_bytes_round_trips() {
        let bytes = [0x42_u8; 32];
        let key = DomainSeparatedKey::from_derived_bytes(bytes);
        assert_eq!(key.expose(), &bytes);
    }

    #[test]
    fn rate_limiter_blocks_after_failure() {
        let mut rl = PairingRateLimiter::new();
        rl.check().unwrap();
        rl.record_failure();
        assert!(rl.check().is_err());
    }

    #[test]
    fn rate_limiter_reset_on_success() {
        let mut rl = PairingRateLimiter::new();
        rl.record_failure();
        rl.record_failure();
        rl.record_success();
        rl.check().unwrap();
    }

    #[test]
    fn enrollment_window_rejects_first_pair_when_closed() {
        // The headline SRX property: outside an active pairing window, a
        // first-pair attempt is refused BEFORE the PIN is ever checked — so a
        // fresh-NodeId sprayer can't guess "whenever sync is on".
        let mut win = EnrollmentWindow::new();
        assert!(!win.is_open());
        assert_eq!(win.check_and_reserve_first_pair(), FirstPairGate::Closed);
    }

    #[test]
    fn enrollment_window_allows_first_pair_when_open() {
        let mut win = EnrollmentWindow::new();
        win.open(std::time::Duration::from_secs(60));
        assert!(win.is_open());
        assert_eq!(win.check_and_reserve_first_pair(), FirstPairGate::Allow);
    }

    #[test]
    fn enrollment_window_global_cap_trips_regardless_of_source() {
        // Fresh-NodeId spraying defeats the per-KEY limiter (empty bucket per
        // guess). The global counter is keyed on nothing, so GLOBAL_FIRST_PAIR_CAP
        // consecutive first-pair attempts trip a hard lockout inside the window
        // no matter how many distinct NodeIds the attacker rotates through.
        let mut win = EnrollmentWindow::new();
        win.open(std::time::Duration::from_secs(60));
        for _ in 0..GLOBAL_FIRST_PAIR_CAP {
            assert_eq!(win.check_and_reserve_first_pair(), FirstPairGate::Allow);
        }
        assert_eq!(win.check_and_reserve_first_pair(), FirstPairGate::Locked);
    }

    #[test]
    fn enrollment_window_success_clears_counter() {
        let mut win = EnrollmentWindow::new();
        win.open(std::time::Duration::from_secs(60));
        let _ = win.check_and_reserve_first_pair();
        let _ = win.check_and_reserve_first_pair();
        assert_eq!(win.failure_count(), 2);
        win.record_success();
        assert_eq!(win.failure_count(), 0);
    }

    #[test]
    fn pairing_secret_b64_round_trips() {
        let s = PairingSecret::from_bytes([0x2b; 32]);
        let back = PairingSecret::from_b64(&s.to_b64()).unwrap();
        assert_eq!(s.as_bytes(), back.as_bytes());
        // Garbage / wrong length → None (treated as "no secret").
        assert!(PairingSecret::from_b64("not base64!!").is_none());
        assert!(PairingSecret::from_b64("YWJj").is_none()); // decodes to 3 bytes
    }

    #[test]
    fn first_pair_key_without_secret_is_pin_bytes() {
        // Back-compat: the no-secret path must be byte-identical to keying on the
        // raw PIN (so a pre-layer-3 peer still interoperates on the fallback).
        let pin = Pin::parse("424242").unwrap();
        let key = first_pair_key(&pin, None);
        assert_eq!(key.as_slice(), pin.as_str().as_bytes());
    }

    #[test]
    fn first_pair_key_with_secret_differs_and_needs_both() {
        let pin = Pin::parse("424242").unwrap();
        let sec = PairingSecret::from_bytes([9u8; 32]);
        let with = first_pair_key(&pin, Some(&sec));
        // A secret-bound key must NOT equal the PIN-only key (domain separation:
        // a PIN-only proof can't be mistaken for a secret-bound one).
        assert_ne!(with.as_slice(), first_pair_key(&pin, None).as_slice());
        // Changing EITHER input changes the key — both are required.
        let other_pin = first_pair_key(&Pin::parse("000000").unwrap(), Some(&sec));
        let other_sec = first_pair_key(&pin, Some(&PairingSecret::from_bytes([1u8; 32])));
        assert_ne!(with.as_slice(), other_pin.as_slice(), "PIN must matter");
        assert_ne!(with.as_slice(), other_sec.as_slice(), "secret must matter");
        // Deterministic — both sides derive the same key from the same inputs.
        assert_eq!(with.as_slice(), first_pair_key(&pin, Some(&sec)).as_slice());
    }

    #[test]
    fn enrollment_window_reopen_resets_lockout() {
        // Re-opening (a fresh "Show code") must clear a tripped global lock, so
        // the cap can never deny pairing permanently.
        let mut win = EnrollmentWindow::new();
        win.open(std::time::Duration::from_secs(60));
        for _ in 0..GLOBAL_FIRST_PAIR_CAP {
            let _ = win.check_and_reserve_first_pair();
        }
        assert_eq!(win.check_and_reserve_first_pair(), FirstPairGate::Locked);
        win.open(std::time::Duration::from_secs(60));
        assert_eq!(win.check_and_reserve_first_pair(), FirstPairGate::Allow);
    }
}
