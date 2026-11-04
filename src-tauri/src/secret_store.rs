//! Cross-platform secret storage. On desktop this round-trips through the
//! OS keychain (macOS Keychain, Windows Credential Manager, Linux
//! Secret-Service). On mobile (Android/iOS) we encrypt values with
//! AES-256-GCM under a key derived via HKDF-SHA-256 from a 32-byte random
//! seed stored at `.corvux/.enc-seed` in app-private storage. Encrypted
//! values live in `.corvux/encrypted-secrets.json` instead of the
//! plaintext `settings.json`.
//!
//! ### Threat model — be honest about what this buys
//!
//! **Defends against:**
//! - Casual filesystem dumps (`adb backup`, cloud-backup extraction,
//!   accidental sync of the data dir to an external drive).
//! - Other apps on the device. Android/iOS app sandboxing already isolates
//!   per-app storage, but the encryption ensures keys aren't trivially
//!   readable if that boundary is bypassed in a future OS bug.
//! - Logs / crash dumps that incidentally capture the data dir.
//!
//! **Does NOT defend against:**
//! - Root / jailbreak. The seed is plaintext in app storage; root reads
//!   the seed, runs the same HKDF, decrypts the roost. True root-resistant
//!   storage requires a Kotlin/Swift plugin into the platform Keystore /
//!   Secure Enclave, which we've deferred.
//! - Malicious code running inside the app process — same key material is
//!   already in memory.
//!
//! ### Implementation note
//!
//! Pure-Rust intentionally so we don't add a Kotlin JNI plugin per
//! platform target. When/if hardware-backed keystores become a hard
//! requirement we'll revisit.
//!
//! Errors are best-effort: we never let a keychain failure block the app
//! from launching. Callers fall back to in-memory state when this returns
//! `Err`.

#[cfg_attr(any(target_os = "android", target_os = "ios"), allow(dead_code))]
const SERVICE: &str = "com.corvuxmindware.corvux";

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod imp {
    use super::SERVICE;

    pub fn get(key: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(SERVICE, key)
            .map_err(|e| format!("keyring open: {e}"))?
            .get_password()
        {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("keyring get: {e}")),
        }
    }

    pub fn set(key: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, key)
            .map_err(|e| format!("keyring open: {e}"))?
            .set_password(value)
            .map_err(|e| format!("keyring set: {e}"))
    }

    pub fn delete(key: &str) -> Result<(), String> {
        match keyring::Entry::new(SERVICE, key)
            .map_err(|e| format!("keyring open: {e}"))?
            .delete_credential()
        {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keyring delete: {e}")),
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
mod imp {
    use aes_gcm::aead::{Aead, KeyInit, OsRng};
    use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
    use hkdf::Hkdf;
    use sha2::Sha256;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    const SEED_FILE: &str = ".corvux/.enc-seed";
    const SECRETS_FILE: &str = ".corvux/encrypted-secrets.json";
    const HKDF_INFO: &[u8] = b"corvux-mobile-secret-store-v1";

    #[derive(serde::Serialize, serde::Deserialize, Default)]
    struct SecretsFile {
        entries: HashMap<String, String>,
    }

    fn roost_data_dir() -> Result<PathBuf, String> {
        // AUDIT-043c: route through the canonical Tauri-managed
        // `app_data_dir` so the OS cleans up encrypted secrets on
        // uninstall. The previous `directories::ProjectDirs(...)`
        // fallback isn't officially supported on iOS/Android by the
        // `directories` crate; the new path is the platform-correct
        // app-private sandbox both OSes manage. Migration of legacy
        // files (`.corvux/.enc-seed`, `.corvux/encrypted-secrets.json`)
        // runs eagerly at boot from `crate::paths::run_legacy_migrations`.
        crate::paths::mobile_secret_store_dir()
    }

    fn seed_path() -> Result<PathBuf, String> {
        Ok(roost_data_dir()?.join(SEED_FILE))
    }

    fn secrets_path() -> Result<PathBuf, String> {
        Ok(roost_data_dir()?.join(SECRETS_FILE))
    }

    fn get_or_create_seed() -> Result<Vec<u8>, String> {
        let path = seed_path()?;
        if path.exists() {
            let seed = fs::read(&path).map_err(|e| format!("read seed: {e}"))?;
            if seed.len() == 32 {
                return Ok(seed);
            }
            tracing::warn!(
                "seed file wrong size ({} bytes), regenerating; existing encrypted secrets are unrecoverable and will be wiped",
                seed.len()
            );
            // Without the matching seed every entry in the secrets file
            // is a permanent decrypt failure. Wipe it so the next read
            // returns "no entry" instead of a confusing crypto error,
            // and downstream callers prompt the user to re-enter keys.
            if let Ok(secrets) = secrets_path() {
                let _ = fs::remove_file(&secrets);
            }
        }
        let seed: [u8; 32] = rand::random();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create seed dir: {e}"))?;
        }
        write_atomic(&path, &seed)?;
        Ok(seed.to_vec())
    }

    /// Write `data` to `path` atomically: write to `path.tmp` first, then
    /// rename. Power-cut mid-write leaves the destination intact (either
    /// the old contents or nothing — never a truncated half).
    fn write_atomic(path: &PathBuf, data: &[u8]) -> Result<(), String> {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, data).map_err(|e| format!("write tmp {}: {e}", tmp.display()))?;
        fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
        Ok(())
    }

    fn derive_key(seed: &[u8]) -> Result<[u8; 32], String> {
        let hkdf = Hkdf::<Sha256>::new(None, seed);
        let mut key = [0u8; 32];
        hkdf.expand(HKDF_INFO, &mut key)
            .map_err(|e| format!("HKDF expand: {e}"))?;
        Ok(key)
    }

    fn encrypt(plaintext: &str, key: &[u8; 32]) -> Result<String, String> {
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("cipher init: {e}"))?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| format!("encrypt: {e}"))?;
        let mut combined = nonce.to_vec();
        combined.extend_from_slice(&ciphertext);
        Ok(base64_encode(&combined))
    }

    fn decrypt(encoded: &str, key: &[u8; 32]) -> Result<String, String> {
        let combined = base64_decode(encoded)?;
        if combined.len() < 12 {
            return Err("ciphertext too short".into());
        }
        let (nonce_bytes, ciphertext) = combined.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("cipher init: {e}"))?;
        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| format!("decrypt: {e}"))?;
        String::from_utf8(plaintext).map_err(|e| format!("utf8: {e}"))
    }

    fn load_secrets() -> Result<SecretsFile, String> {
        let path = secrets_path()?;
        if !path.exists() {
            return Ok(SecretsFile::default());
        }
        let text = fs::read_to_string(&path).map_err(|e| format!("read secrets: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("parse secrets: {e}"))
    }

    fn save_secrets(sf: &SecretsFile) -> Result<(), String> {
        let path = secrets_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create secrets dir: {e}"))?;
        }
        let text = serde_json::to_string(sf).map_err(|e| format!("serialize secrets: {e}"))?;
        write_atomic(&path, text.as_bytes())?;
        Ok(())
    }

    pub fn get(key: &str) -> Result<Option<String>, String> {
        let sf = load_secrets()?;
        let encrypted = match sf.entries.get(key) {
            Some(v) => v,
            None => return Ok(None),
        };
        let seed = get_or_create_seed()?;
        let derived_key = derive_key(&seed)?;
        let plaintext = decrypt(encrypted, &derived_key)?;
        Ok(Some(plaintext))
    }

    pub fn set(key: &str, value: &str) -> Result<(), String> {
        let seed = get_or_create_seed()?;
        let derived_key = derive_key(&seed)?;
        let encrypted = encrypt(value, &derived_key)?;
        let mut sf = load_secrets()?;
        sf.entries.insert(key.to_string(), encrypted);
        save_secrets(&sf)
    }

    pub fn delete(key: &str) -> Result<(), String> {
        let mut sf = load_secrets()?;
        sf.entries.remove(key);
        save_secrets(&sf)
    }

    fn base64_encode(data: &[u8]) -> String {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        STANDARD.encode(data)
    }

    fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        STANDARD
            .decode(s)
            .map_err(|e| format!("base64 decode: {e}"))
    }
}

pub fn get(key: &str) -> Result<Option<String>, String> {
    imp::get(key)
}

pub fn set(key: &str, value: &str) -> Result<(), String> {
    imp::set(key, value)
}

pub fn delete(key: &str) -> Result<(), String> {
    imp::delete(key)
}

/// True if this platform has a backed keychain. Now returns true on all
/// platforms — mobile uses AES-256-GCM with a device-specific key.
pub fn is_supported() -> bool {
    true
}

pub mod keys {
    pub const GEMINI_API_KEY: &str = "gemini_api_key";
    pub const OPENROUTER_API_KEY: &str = "openrouter_api_key";
    /// S-03: Ollama bearer token (only used when the user fronts their
    /// Ollama server with an auth proxy). Mirrors the strip-on-save +
    /// hydrate-on-start pattern of the Gemini / OpenRouter keys so
    /// Locked Decision #10 (API keys live in the OS keychain, not in
    /// settings.json) actually covers all three providers.
    pub const OLLAMA_API_KEY: &str = "ollama_api_key";
    /// OpenAI direct-provider key. Same strip-on-save + hydrate-on-start
    /// pattern as the other provider keys (Locked Decision #10).
    pub const OPENAI_API_KEY: &str = "openai_api_key";
    /// Anthropic (Claude) direct-provider key. Same pattern.
    pub const ANTHROPIC_API_KEY: &str = "anthropic_api_key";

    /// Sync reconnect PIN per paired device. The keychain stores the
    /// 6-digit PIN the user typed at pair time so the auto-reconnect
    /// path can re-authenticate without prompting again. Was on disk
    /// in `peers.bin` (peer-store v3); moved to the keychain in v4 so
    /// a backup of the app data dir doesn't expose pairing PINs.
    pub fn sync_pin(device_id_hex: &str) -> String {
        format!("sync_pin:{device_id_hex}")
    }

    /// Sync queue per-pair symmetric key (32 bytes hex-encoded). Used by
    /// the store-and-forward queue to encrypt payloads to a recipient
    /// without an active handshake. Same migration story as the PIN —
    /// previously persisted in `peers.bin`, now keychain-backed.
    pub fn sync_queue_key(device_id_hex: &str) -> String {
        format!("sync_queue_key:{device_id_hex}")
    }

    /// SEC-2 per-peer resumption token (32 bytes hex-encoded). Derived from
    /// the first-pair `SessionSecret` and stored keychain-side; thereafter
    /// reconnects prove this high-entropy token instead of re-proving the
    /// human 6-digit PIN, so a PIN that leaks after pairing is no longer a
    /// permanent credential for joining the roost.
    pub fn sync_resumption_token(device_id_hex: &str) -> String {
        format!("sync_resumption_token:{device_id_hex}")
    }

    /// AUDIT-015: peer's roost id (16 bytes hex-encoded), pinned at first
    /// pair. Stored keychain-side (alongside PIN + queue key) so a backup
    /// of `peers.bin` doesn't carry it, and so a peer-record schema bump
    /// isn't needed. On reconnect, the engine compares the value the peer
    /// advertises in `Hello` against this — mismatch means the same
    /// device is now pointing at a different roost.
    pub fn sync_peer_vault_id(device_id_hex: &str) -> String {
        format!("sync_peer_vault_id:{device_id_hex}")
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    #[test]
    fn encrypt_decrypt_roundtrip() {
        let seed: [u8; 32] = rand::random();
        let key = super::imp::derive_key(&seed).unwrap();
        let plaintext = "AIza-super-secret-key-12345";
        let encrypted = super::imp::encrypt(plaintext, &key).unwrap();
        assert_ne!(encrypted, plaintext);
        let decrypted = super::imp::decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }
}
