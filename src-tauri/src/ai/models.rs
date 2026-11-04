//! Model storage paths + HuggingFace download for local ML artifacts.
//!
//! The only on-device model now is the local Gemma LLM (GGUF), stored
//! under the platform data dir (e.g. `~/.local/share/corvux/` on Linux,
//! `%APPDATA%\corvux\` on Windows, `~/Library/Application Support/corvux/`
//! on macOS) at `models/gemma-4-e2b-it/`.
//!
//! The download command streams the file and emits
//! `ai:model-download-progress` events so the UI can render a progress
//! bar. A partial download is written to `.part` and atomically renamed
//! on success — so an interrupted download won't leave a corrupt model
//! behind.

use std::path::PathBuf;

// AUDIT-043: model storage moved off `directories::ProjectDirs` into
// the canonical `app_data_dir`. See `crate::paths::ai_models_dir` —
// every consumer here now goes through that helper so the OS-managed
// cleanup on uninstall reaches the model files.
use futures_util::StreamExt;
use serde::Serialize;
use tauri::AppHandle;
use tauri_specta::Event;
use tokio::io::AsyncWriteExt;

use crate::events;

// ── Gemma 4 E2B (local LLM) ────────────────────────────────────

/// Model directory name for the local LLM.
pub const GEMMA4_E2B_MODEL_NAME: &str = "gemma-4-e2b-it";

/// GGUF file name inside the model directory.
const GEMMA4_E2B_GGUF_FILE: &str = "gemma-4-E2B-it-Q4_K_M.gguf";

/// HuggingFace download URL for the Q4_K_M quantization (3.46 GB).
const GEMMA4_E2B_URL: &str =
    "https://huggingface.co/bartowski/google_gemma-4-E2B-it-GGUF/resolve/main/google_gemma-4-E2B-it-Q4_K_M.gguf";

/// Approximate size for progress bar fallback if content-length is missing.
const GEMMA4_E2B_APPROX_BYTES: u64 = 3_460_000_000;

/// Minimum file size to consider the GGUF download valid (~1 GB).
/// Anything smaller is likely corrupt or truncated.
const GEMMA4_E2B_MIN_BYTES: u64 = 1_000_000_000;

/// AUDIT-078: SHA-256 of the upstream GGUF (lower-hex). Verified against
/// HuggingFace LFS oid for `google_gemma-4-E2B-it-Q4_K_M.gguf` on
/// 2026-05-23. HF allows model authors to force-push to `main`; a swapped
/// GGUF would reach llama.cpp's tensor mmap as native-code execution.
/// Refuse-on-empty is enforced in [`verify_or_purge`].
const GEMMA4_E2B_SHA256: &str = "b5310340b3a23d31655d7119d100d5df1b2d8ee17b3ca8b0a23ad7e9eb5fa705";

/// Full path to the Gemma 4 E2B GGUF file on disk.
pub fn gemma4_e2b_path() -> Result<std::path::PathBuf, String> {
    Ok(model_dir(GEMMA4_E2B_MODEL_NAME)?.join(GEMMA4_E2B_GGUF_FILE))
}

/// Check if the Gemma 4 E2B model is downloaded and cached.
pub fn gemma4_e2b_status() -> ModelStatus {
    let path = match gemma4_e2b_path() {
        Ok(p) => p,
        Err(_) => {
            return ModelStatus {
                name: GEMMA4_E2B_MODEL_NAME.to_string(),
                installed: false,
                size_bytes: 0,
                path: String::new(),
            };
        }
    };

    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let installed = size > GEMMA4_E2B_MIN_BYTES;

    ModelStatus {
        name: GEMMA4_E2B_MODEL_NAME.to_string(),
        installed,
        size_bytes: size,
        path: path
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}

/// Download the Gemma 4 E2B Q4_K_M GGUF model from HuggingFace.
/// Skips if already downloaded and valid. Uses atomic .part + rename.
pub async fn download_gemma4_e2b(client: &reqwest::Client, app: &AppHandle) -> Result<(), String> {
    let gguf_path = gemma4_e2b_path()?;

    events::AiModelDownloadStart(GEMMA4_E2B_MODEL_NAME.to_string())
        .emit(app)
        .ok();

    if !is_valid_install(&gguf_path, GEMMA4_E2B_MIN_BYTES, GEMMA4_E2B_SHA256) {
        // Existing file (if any) failed the SHA pin — delete before
        // re-downloading so we don't leave a poisoned blob next to a fresh .part.
        if gguf_path.exists() {
            let _ = std::fs::remove_file(&gguf_path);
        }
        download_file(
            client,
            app,
            GEMMA4_E2B_URL,
            &gguf_path,
            GEMMA4_E2B_GGUF_FILE,
            GEMMA4_E2B_APPROX_BYTES,
            GEMMA4_E2B_SHA256,
        )
        .await?;
    }

    events::AiModelDownloadDone(GEMMA4_E2B_MODEL_NAME.to_string())
        .emit(app)
        .ok();
    Ok(())
}

/// Delete the cached Gemma 4 E2B model to free disk space.
pub fn delete_gemma4_e2b() -> Result<(), String> {
    let path = gemma4_e2b_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| format!("Failed to delete {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Resolve the directory where we persist on-device models. Creates
/// parents on demand. Delegates to [`crate::paths::ai_models_dir`] so
/// the path lives under the OS-managed app data dir (cleaned on
/// uninstall). The previous `ProjectDirs::from("com","Corvux","Corvux")`
/// path leaked ~137 MB on every uninstall — see AUDIT-043 and the
/// `paths::run_legacy_migrations` first-boot mover.
pub fn models_root() -> Result<PathBuf, String> {
    crate::paths::ai_models_dir()
}

/// Directory for a specific model (created on demand).
pub fn model_dir(model_name: &str) -> Result<PathBuf, String> {
    let dir = models_root()?.join(model_name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Create model dir ({}): {e}", dir.display()))?;
    Ok(dir)
}

/// Snapshot of what's installed on disk. Returned by `get_model_status`.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ModelStatus {
    pub name: String,
    pub installed: bool,
    /// Total on-disk size of the model artifacts, bytes.
    #[specta(type = u32)]
    pub size_bytes: u64,
    /// Human-friendly install location (for the settings UI).
    pub path: String,
}

/// Per-chunk progress event payload. `file` is the artifact name so the UI
/// can show "Downloading model.onnx... 42%".
#[derive(Debug, Clone, Serialize, serde::Deserialize, specta::Type)]
pub struct ProgressPayload {
    pub file: String,
    /// Bytes downloaded so far. `u32` on the TS side — model files cap
    /// out in the low GB range; >4 GB would already overflow our retry
    /// math. Cast occurs in this struct, not at the emit site.
    #[specta(type = u32)]
    pub bytes_downloaded: u64,
    #[specta(type = u32)]
    pub total_bytes: u64,
    pub percent: f32,
}

/// AUDIT-002: stream the file at `path` through SHA-256 and return the
/// hex digest. 64 KiB read buffer so the 3.46 GB GGUF doesn't end up
/// resident in RAM during verification. Errors surface as Strings so
/// callers can treat them as a download failure.
fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("Open {} for hashing: {e}", path.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("Read {} for hashing: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// AUDIT-078: enforce a SHA-256 pin on a downloaded file before it's
/// promoted from `.part` to its final name. Empty `expected` is a
/// release blocker — refuse to install rather than fail-open.
fn verify_or_purge(tmp: &std::path::Path, expected: &str, file_label: &str) -> Result<(), String> {
    if expected.is_empty() {
        let _ = std::fs::remove_file(tmp);
        return Err(format!(
            "{file_label} integrity pin missing — refusing to install an unverified model. \
             Populate the *_SHA256 const in ai/models.rs and rebuild."
        ));
    }

    let actual = sha256_file(tmp)?;
    if !subtle_eq(actual.as_bytes(), expected.as_bytes()) {
        // Failure here is a security signal, not a corrupted file —
        // delete the .part so the next download attempt starts clean
        // and surface a clear error to the user.
        let _ = std::fs::remove_file(tmp);
        return Err(format!(
            "{file_label} integrity check failed (expected {expected}, got {actual}). \
             Refusing to use this download — possible compromised CDN. .part file removed; \
             retry the download or check the upstream model repo for tampering."
        ));
    }
    Ok(())
}

/// Constant-time string compare for the hex digest. Overkill against an
/// attacker who controls the bytes anyway, but cheap discipline that
/// also stops the optimizer from short-circuiting on first mismatch
/// (helpful when reading logs in pathological cases).
fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

/// Stream a single file from `url` → `dest`, emitting progress events via
/// `ai:model-download-progress` (throttled to avoid flooding the IPC). Writes
/// to `dest.part` first, verifies SHA-256 if `expected_sha256` is non-empty,
/// then renames on success so an interrupted or tampered run leaves no
/// usable artifact.
async fn download_file(
    client: &reqwest::Client,
    app: &AppHandle,
    url: &str,
    dest: &std::path::Path,
    file_label: &str,
    fallback_total: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    let tmp = dest.with_extension("part");

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Download HTTP error ({file_label}): {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Download failed for {file_label}: HTTP {}",
            response.status()
        ));
    }

    // `content-length` is missing when HF uses chunked transfer; fall back
    // to the constant so the UI can still render a progress estimate.
    let total_bytes = response.content_length().unwrap_or(fallback_total).max(1);

    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("Create {} failed: {e}", tmp.display()))?;

    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_emit_pct: i32 = -1;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Stream error ({file_label}): {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("Write error ({file_label}): {e}"))?;

        downloaded += chunk.len() as u64;

        // Throttle to ~1% changes — keeps the frontend smooth without flooding.
        let pct = ((downloaded as f64 / total_bytes as f64) * 100.0).clamp(0.0, 100.0);
        let pct_bucket = pct.floor() as i32;
        if pct_bucket != last_emit_pct {
            last_emit_pct = pct_bucket;
            events::AiModelDownloadProgress(ProgressPayload {
                file: file_label.to_string(),
                bytes_downloaded: downloaded,
                total_bytes,
                percent: pct as f32,
            })
            .emit(app)
            .ok();
        }
    }

    file.flush()
        .await
        .map_err(|e| format!("Flush error ({file_label}): {e}"))?;
    drop(file);

    // AUDIT-002: integrity check BEFORE the atomic rename. If the
    // upstream byte-for-byte matches the pinned hash, promote .part →
    // dest. Otherwise verify_or_purge has already deleted the .part
    // and returned an Err.
    verify_or_purge(&tmp, expected_sha256, file_label)?;

    // Atomic rename: either the full file exists or the .part does.
    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(|e| format!("Rename {} → {}: {e}", tmp.display(), dest.display()))?;

    Ok(())
}

fn is_valid_file(path: &std::path::Path, min_bytes: u64) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() >= min_bytes)
        .unwrap_or(false)
}

/// AUDIT-078b: integrity-check the Gemma GGUF on disk. Used by the
/// `load_llm_model` IPC right before `LlamaCppModel::load` mmaps the
/// file into native llama.cpp code paths. Returns the verified path on
/// success; an error string the UI can show on failure. Runs SHA-256
/// over the 3.46 GB file — callers should invoke from `spawn_blocking`.
pub fn verify_gemma4_e2b_or_err() -> Result<std::path::PathBuf, String> {
    let path = gemma4_e2b_path()?;
    if !is_valid_install(&path, GEMMA4_E2B_MIN_BYTES, GEMMA4_E2B_SHA256) {
        return Err(format!(
            "Local LLM integrity check failed at {}. The on-disk file does not match the \
             pinned SHA-256 — either the install predates the pin or the bytes have been \
             tampered with. Re-download from Settings → AI.",
            path.display()
        ));
    }
    Ok(path)
}

/// AUDIT-078: same intent as [`is_valid_file`] but also enforces the
/// SHA-256 pin on the file already on disk. Used by the download flow so
/// a poisoned-but-correct-size file (planted via any other write path,
/// e.g. a symlink escape or the user's own tinkering) is rejected before
/// we mmap it into llama.cpp / ONNX Runtime. Empty `expected_sha256` is
/// treated as invalid — same refuse-on-empty semantics as `verify_or_purge`.
fn is_valid_install(path: &std::path::Path, min_bytes: u64, expected_sha256: &str) -> bool {
    if expected_sha256.is_empty() || !is_valid_file(path, min_bytes) {
        return false;
    }
    match sha256_file(path) {
        Ok(actual) => subtle_eq(actual.as_bytes(), expected_sha256.as_bytes()),
        Err(e) => {
            tracing::warn!(
                file = %path.display(),
                error = %e,
                "AUDIT-078: hashing existing model file failed — treating as invalid"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{sha256_file, verify_or_purge};
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Known-answer test: SHA-256 of "hello world".
    /// `printf 'hello world' | shasum -a 256` →
    /// b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
    const HELLO_WORLD_SHA256: &str =
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    fn temp_with(content: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(content).expect("write");
        f.flush().expect("flush");
        f
    }

    #[test]
    fn sha256_file_matches_known_answer() {
        let f = temp_with(b"hello world");
        let got = sha256_file(f.path()).unwrap();
        assert_eq!(got, HELLO_WORLD_SHA256);
    }

    #[test]
    fn verify_or_purge_accepts_matching_hash() {
        let f = temp_with(b"hello world");
        let path = f.path().to_path_buf();
        verify_or_purge(&path, HELLO_WORLD_SHA256, "test.bin").unwrap();
        // File must still exist on success.
        assert!(path.exists());
    }

    #[test]
    fn verify_or_purge_deletes_part_on_mismatch() {
        // Wrong expected hash → delete the .part + error. The test
        // mirrors the production guarantee: a tampered download leaves
        // nothing behind for the retry path to pick up by accident.
        let f = temp_with(b"hello world");
        let path = f.path().to_path_buf();
        let err = verify_or_purge(
            &path,
            "0000000000000000000000000000000000000000000000000000000000000000",
            "test.bin",
        )
        .unwrap_err();
        assert!(err.contains("integrity check failed"), "got: {err}");
        assert!(!path.exists(), ".part must be removed on mismatch");
        // Drop the NamedTempFile guard — it'll silently no-op since the
        // file's already gone.
        let _ = f;
    }

    /// AUDIT-078 (replaces the prior fail-open test): empty `expected`
    /// now means "no pin defined for this artifact" — refuse to install
    /// and delete the `.part` so the next download attempt starts clean.
    /// Verifies the inversion of the pre-2026-05-23 fail-open behavior.
    #[test]
    fn verify_or_purge_refuses_when_pin_empty() {
        let f = temp_with(b"any bytes here");
        let path = f.path().to_path_buf();
        let err = verify_or_purge(&path, "", "test.bin").unwrap_err();
        assert!(
            err.contains("integrity pin missing"),
            "error must explain why the install is refused, got: {err}"
        );
        assert!(
            !path.exists(),
            "AUDIT-078: empty-pin path must delete the .part too — no half-installed bytes left"
        );
        let _ = f;
    }

    /// AUDIT-078: the model-status `is_valid_install` gate must reject a
    /// file whose bytes don't match the pinned SHA, even if size is OK.
    /// Catches "attacker swaps the bytes of an already-installed file" —
    /// the previous size-only check missed this.
    #[test]
    fn is_valid_install_rejects_wrong_sha_at_correct_size() {
        use super::is_valid_install;
        let f = temp_with(b"hello world");
        // Wrong expected hash → reject even though size is plenty for
        // the min_bytes=1 floor.
        assert!(
            !is_valid_install(
                f.path(),
                1,
                "0000000000000000000000000000000000000000000000000000000000000000"
            ),
            "is_valid_install must reject a file whose SHA-256 doesn't match the pin"
        );
    }

    #[test]
    fn is_valid_install_rejects_empty_pin() {
        use super::is_valid_install;
        let f = temp_with(b"hello world");
        assert!(
            !is_valid_install(f.path(), 1, ""),
            "empty pin means we have no anchor — must NOT accept an existing file as valid"
        );
    }

    #[test]
    fn is_valid_install_accepts_matching_sha() {
        use super::is_valid_install;
        let f = temp_with(b"hello world");
        assert!(
            is_valid_install(f.path(), 1, HELLO_WORLD_SHA256),
            "matching SHA on a present file must pass"
        );
    }
}
