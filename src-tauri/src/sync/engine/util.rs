//! Stateless helpers extracted from `engine.rs` to keep the orchestrator
//! file focused on lifecycle. Nothing here touches `SyncEngine` state.

use super::peer::PeerHandle;

pub(super) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Extract the roost's folder name (last path segment) for handshake use.
///
/// This has to agree across platforms. Desktop passes real filesystem
/// paths like `C:\Users\raj\Brain-Dump` → `Brain-Dump`. Android passes
/// SAF tree URIs like
/// `content://com.android.externalstorage.documents/tree/primary%3ADocuments%2FBrain-Dump`
/// where `%3A` is `:` and `%2F` is `/`. Naive `Path::file_name()` on the
/// URI form returns `primary%3ADocuments%2FBrain-Dump`, which would never
/// match the desktop's `Brain-Dump`.
pub(super) fn roost_folder_name(roost: &std::path::Path) -> String {
    let raw = roost.to_string_lossy();
    if let Some(stripped) = raw.strip_prefix("content://") {
        let decoded = percent_decode(stripped);
        let leaf = decoded.rsplit('/').next().unwrap_or(&decoded);
        return leaf.to_string();
    }
    roost
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| raw.into_owned())
}

/// Minimal URL-decoder for `%XX` escapes. We don't pull in the `urlencoding`
/// crate for a single call — SAF tree URIs only contain ASCII after
/// encoding, so a byte-level decode is safe and has no transitive dep.
fn percent_decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Utility: `PeerHandle` is intentionally not `Clone` (the `Sender` is, but
/// duplicating the semantic handle could confuse shutdown semantics). The
/// watcher fan-out needs a short-lived copy per iteration, so we reach
/// under the hood here and clone just the channel half.
pub(super) fn clone_handle(handle: &PeerHandle) -> PeerHandle {
    PeerHandle {
        device_id: handle.device_id,
        display_name: handle.display_name.clone(),
        queue_pair_key: handle.queue_pair_key,
        peer_vault_id: handle.peer_vault_id,
        control: handle.control_clone(),
        generation: handle.generation,
        peer_reconnect: handle.peer_reconnect.clone(),
        resumption_token: handle.resumption_token,
    }
}
