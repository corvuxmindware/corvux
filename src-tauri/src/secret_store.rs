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

    pub fn get(key: &str) -> Result<Option<crate::sensitive::SecretString>, String> {
        match keyring::Entry::new(SERVICE, key)
            .map_err(|e| format!("keyring open: {e}"))?
            .get_password()
        {
            Ok(s) => Ok(Some(s.into())),
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
use mobile as imp;

#[cfg(any(target_os = "android", target_os = "ios", test))]
#[cfg_attr(test, allow(dead_code))]
mod mobile {
    use aes_gcm::aead::{Aead, KeyInit, OsRng};
    use aes_gcm::{AeadCore, Aes256Gcm, Nonce};
    use hkdf::Hkdf;
    use sha2::Sha256;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use zeroize::Zeroizing;

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

    fn get_or_create_seed() -> Result<Zeroizing<Vec<u8>>, String> {
        get_or_create_seed_at(&seed_path()?, &secrets_path()?)
    }

    fn get_or_create_seed_at(
        path: &std::path::Path,
        secrets: &std::path::Path,
    ) -> Result<Zeroizing<Vec<u8>>, String> {
        if path.exists() {
            let seed = Zeroizing::new(fs::read(path).map_err(|e| format!("read seed: {e}"))?);
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
            match fs::remove_file(secrets) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("remove unrecoverable secrets: {e}")),
            }
        }
        let seed = Zeroizing::new(rand::random::<[u8; 32]>().to_vec());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create seed dir: {e}"))?;
        }
        write_atomic(path, &seed)?;
        Ok(seed)
    }

    /// Write `data` to `path` atomically: write to `path.tmp` first, then
    /// rename. Power-cut mid-write leaves the destination intact (either
    /// the old contents or nothing — never a truncated half).
    fn write_atomic(path: &std::path::Path, data: &[u8]) -> Result<(), String> {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, data).map_err(|e| format!("write tmp {}: {e}", tmp.display()))?;
        fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
        Ok(())
    }

    fn derive_key(seed: &[u8]) -> Result<Zeroizing<[u8; 32]>, String> {
        let hkdf = Hkdf::<Sha256>::new(None, seed);
        let mut key = Zeroizing::new([0u8; 32]);
        hkdf.expand(HKDF_INFO, &mut *key)
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

    fn decrypt(encoded: &str, key: &[u8; 32]) -> Result<crate::sensitive::SecretString, String> {
        let combined = base64_decode(encoded)?;
        if combined.len() < 12 {
            return Err("ciphertext too short".into());
        }
        let (nonce_bytes, ciphertext) = combined.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("cipher init: {e}"))?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(nonce, ciphertext)
                .map_err(|e| format!("decrypt: {e}"))?,
        );
        let text =
            std::str::from_utf8(&plaintext).map_err(|_| "secret is not UTF-8".to_string())?;
        Ok(text.into())
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

    pub fn get(key: &str) -> Result<Option<crate::sensitive::SecretString>, String> {
        let seed = get_or_create_seed()?;
        let sf = load_secrets()?;
        let encrypted = match sf.entries.get(key) {
            Some(v) => v,
            None => return Ok(None),
        };
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
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn failed_recovery_cleanup_preserves_old_seed() {
            let dir = tempfile::tempdir().unwrap();
            let seed = dir.path().join("seed");
            let secrets = dir.path().join("secrets");
            fs::write(&seed, b"corrupt").unwrap();
            fs::create_dir(&secrets).unwrap();
            assert!(get_or_create_seed_at(&seed, &secrets).is_err());
            assert_eq!(fs::read(&seed).unwrap(), b"corrupt");
        }
        #[test]
        fn corrupt_seed_recovery_removes_unrecoverable_secrets() {
            let dir = tempfile::tempdir().unwrap();
            let seed = dir.path().join("seed");
            let secrets = dir.path().join("secrets");
            fs::write(&seed, b"corrupt").unwrap();
            fs::write(&secrets, b"unrecoverable ciphertext").unwrap();
            let fresh = get_or_create_seed_at(&seed, &secrets).unwrap();
            assert_eq!(fresh.len(), 32);
            assert!(!secrets.exists());
            assert_eq!(&*get_or_create_seed_at(&seed, &secrets).unwrap(), &*fresh);
        }
        #[test]
        fn mobile_secret_round_trip_and_authentication_failure() {
            let seed = Zeroizing::new([17u8; 32]);
            let key = derive_key(&*seed).unwrap();
            let encoded = encrypt("credential-sentinel", &key).unwrap();
            assert_eq!(&*decrypt(&encoded, &key).unwrap(), "credential-sentinel");
            let other = derive_key(&[18; 32]).unwrap();
            assert!(decrypt(&encoded, &other).is_err());
        }
    }
}

pub fn get(key: &str) -> Result<Option<crate::sensitive::SecretString>, String> {
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

    // Per-paired-peer sync secrets (reconnect PIN, queue pair key, SEC-2
    // resumption token, AUDIT-015 pinned peer vault id) are deliberately NOT
    // built here. Their keys are built only by `PeerSecret` in
    // `sync/store/peer_store.rs`, scoped to one roost's peer registry
    // (`<kind>:<scope>:<device>`, SYN-I11). The old device-id-only builders
    // that lived here let the same device paired in two roosts overwrite
    // (and on unpair, delete) the other roost's credentials.
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    #[test]
    fn encrypt_decrypt_roundtrip() {
        let seed = Zeroizing::new(rand::random::<[u8; 32]>().to_vec());
        let key = super::imp::derive_key(&seed).unwrap();
        let plaintext = "AIza-super-secret-key-12345";
        let encrypted = super::imp::encrypt(plaintext, &key).unwrap();
        assert_ne!(encrypted, plaintext);
        let decrypted = super::imp::decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }
}
