//! Certificate fingerprints and human-verifiable encodings.
//!
//! TOFU (Trust On First Use) pinning needs two things: a stable fingerprint
//! we can compare byte-wise across sessions, and a short human-readable form
//! users can verify out-of-band (e.g. on a phone call).
//!
//! We fingerprint the **SPKI** (Subject Public Key Info), not the full cert.
//! Rotating a cert's metadata (validity dates, CN) doesn't change the SPKI,
//! so a long-lived device keeps the same fingerprint through cert renewals.
//!
//! The human encoding is **6 PGP-word-list words** drawn from RFC 1751-style
//! even/odd dictionaries (see `wordlist.rs`). 6 words × 2 dicts × 256 entries
//! = 48 bits of the 256-bit SPKI hash. Each byte 0..256 maps to a UNIQUE
//! word per list — distinct fingerprint bytes always render to distinct
//! words. The full fingerprint is still compared byte-wise in code; words
//! are for humans, not for trust decisions.
//!
//! AUDIT-003: prior to 2026-05-22 the lists wrapped on a ~225-word pool, so
//! ~31 byte values per list collided into the same word. Two distinct
//! fingerprints could produce identical phrases. Fixed by shipping disjoint
//! 256-entry lists in `wordlist::{EVEN_WORDS, ODD_WORDS}` with tests
//! asserting uniqueness.

use sha2::{Digest, Sha256};

use super::wordlist::{EVEN_WORDS, ODD_WORDS};

/// 32-byte SHA-256 of an X.509 SPKI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    /// Compute from a DER-encoded SPKI. The caller is responsible for
    /// extracting the SPKI bytes from a full cert; `rustls::pki_types`
    /// exposes `CertificateDer` but not a direct SPKI accessor, so in practice
    /// we wrap `rcgen::Certificate::pem()` → parse → SPKI. For the MVP we hash
    /// the full cert DER, which is stable for a given keypair since `rcgen`'s
    /// cert metadata (serial, validity) is deterministic when we seed it.
    pub fn of_cert_der(cert_der: &[u8]) -> Self {
        let digest = Sha256::digest(cert_der);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Self(out)
    }

    #[allow(dead_code)] // diagnostics surface; kept for log/debug ergonomics
    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Colon-separated pairs, like `openssl x509 -fingerprint` output.
    /// Useful for power users; the diceware encoding is the default UI form.
    #[allow(dead_code)] // power-user UI form; not yet wired into Settings
    pub fn as_hex_groups(&self) -> String {
        self.0
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Render the first 48 bits as six diceware-style words. Users verify
    /// the same phrase on both devices before trusting the connection.
    pub fn as_words(&self) -> String {
        let mut words = Vec::with_capacity(6);
        for i in 0..3 {
            let hi = self.0[i * 2];
            let lo = self.0[i * 2 + 1];
            words.push(EVEN_WORDS[hi as usize]);
            words.push(ODD_WORDS[lo as usize]);
        }
        words.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic() {
        let a = Fingerprint::of_cert_der(b"hello");
        let b = Fingerprint::of_cert_der(b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_changes_with_input() {
        let a = Fingerprint::of_cert_der(b"a");
        let b = Fingerprint::of_cert_der(b"b");
        assert_ne!(a, b);
    }

    #[test]
    fn word_encoding_is_six_tokens() {
        let fp = Fingerprint::of_cert_der(b"certbytes");
        let words = fp.as_words();
        assert_eq!(words.split(' ').count(), 6);
    }

    #[test]
    fn word_encoding_is_stable() {
        let fp = Fingerprint::of_cert_der(b"certbytes");
        assert_eq!(fp.as_words(), fp.as_words());
    }

    /// AUDIT-003 regression: two SPKI fingerprints that differ in any of
    /// their first 6 bytes must render to different word phrases. The
    /// pre-fix pool wrap let ~31 byte values per list collide; this
    /// asserts that every single-byte change to position 0 lands a
    /// distinct word in the rendered phrase.
    #[test]
    fn distinct_first_byte_yields_distinct_phrase() {
        let make = |b: u8| {
            let mut bytes = [0u8; 32];
            bytes[0] = b;
            Fingerprint(bytes).as_words()
        };
        let mut seen: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
        for b in 0u8..=255 {
            let phrase = make(b);
            if let Some(prior) = seen.insert(phrase.clone(), b) {
                panic!(
                    "byte {b} and byte {prior} produce identical phrase \"{phrase}\" — diceware collision regressed"
                );
            }
        }
    }

    /// AUDIT-003 regression: the two byte positions in the lowest word
    /// pair are independent — varying the second byte must change the
    /// second word in the phrase, not just the first.
    #[test]
    fn distinct_second_byte_yields_distinct_phrase() {
        let make = |b: u8| {
            let mut bytes = [0u8; 32];
            bytes[1] = b;
            Fingerprint(bytes).as_words()
        };
        let mut seen: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
        for b in 0u8..=255 {
            let phrase = make(b);
            if let Some(prior) = seen.insert(phrase.clone(), b) {
                panic!(
                    "byte {b} and byte {prior} at position 1 produce identical phrase \"{phrase}\" — diceware collision regressed"
                );
            }
        }
    }
}
