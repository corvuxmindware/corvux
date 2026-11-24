//! Stateless helpers extracted from `engine.rs` to keep the orchestrator
//! file focused on lifecycle. Nothing here touches `SyncEngine` state.

use super::peer::PeerHandle;

/// Run a serialized DocStore transaction on the blocking pool. Keep a share
/// of the note/reset guard inside the job: cancelling its async waiter must
/// not release the mutation barrier while the filesystem commit is unfinished.
pub(super) async fn store_task<T, F>(
    store: std::sync::Arc<tokio::sync::Mutex<crate::sync::store::doc_store::DocStore>>,
    note_guard: std::sync::Arc<super::note_locks::NoteGuard>,
    operation: F,
) -> crate::sync::error::SyncResult<T>
where
    T: Send + 'static,
    F: FnOnce(&crate::sync::store::doc_store::DocStore) -> crate::sync::error::SyncResult<T>
        + Send
        + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _note_guard = note_guard;
        let store = store.blocking_lock();
        operation(&store)
    })
    .await
    .map_err(|error| std::io::Error::other(format!("sync storage task failed: {error}")))?
}

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
        queue_pair_key: handle.queue_pair_key.clone(),
        peer_vault_id: handle.peer_vault_id,
        control: handle.control_clone(),
        generation: handle.generation,
        dialed_by_us: handle.dialed_by_us,
        established_at: handle.established_at,
        peer_reconnect: handle.peer_reconnect.clone(),
        resumption_token: handle.resumption_token.clone(),
    }
}

#[cfg(test)]
mod storage_task_tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_waiter_keeps_reset_barrier_until_storage_job_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(tokio::sync::Mutex::new(
            crate::sync::store::doc_store::DocStore::new(
                dir.path().to_path_buf(),
                crate::sync::identity::DeviceId([1; 16]),
            ),
        ));
        let locks = Arc::new(super::super::note_locks::NoteLocks::new());
        let guard = Arc::new(
            locks
                .lock(&crate::sync::protocol::NotePath("note.md".into()))
                .await,
        );
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(store_task(store, guard, move |_| {
            let _ = started.send(());
            wait.recv_timeout(Duration::from_secs(2))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(())
        }));
        ready.await.unwrap();
        task.abort();
        let _ = task.await;
        let mut reset = tokio::spawn(async move { locks.block_all().await });
        assert!(tokio::time::timeout(Duration::from_millis(25), &mut reset)
            .await
            .is_err());
        release.send(()).unwrap();
        let _guard = tokio::time::timeout(Duration::from_secs(1), reset)
            .await
            .unwrap()
            .unwrap();
    }
}
