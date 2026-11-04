//! Roost scan + recovery operations on `SyncEngine`.
//!
//! `scan_roost` seeds the Automerge DocStore from disk so pre-existing
//! markdown bootstraps into sync. `recover_missing_files` runs the
//! inverse: any note the DocStore knows about that's missing from the
//! roost gets rendered back to disk. Split off `engine.rs` to keep the
//! orchestrator focused on lifecycle and direct peer wiring.

use tracing::{debug, info, warn};

use crate::sync::error::{SyncError, SyncResult};
use crate::sync::protocol::NotePath;

use super::engine::SyncEngine;
use super::peer::PeerHandle;
use super::state::SyncEvent;
use super::util::clone_handle;

impl SyncEngine {
    /// Walk every markdown file in the roost and seed the Automerge
    /// DocStore with its current content. Needed to bootstrap
    /// **pre-existing** notes into sync — the file watcher only sees
    /// changes that happen *after* it starts.
    ///
    /// After the walk, any currently-connected peer is asked to
    /// re-announce its `NoteList`, so the peer can pull whatever we just
    /// ingested. Safe to call repeatedly; re-applying the same markdown
    /// is a DocStore no-op because `md_hash` dedupes unchanged content.
    ///
    /// Emits `SyncEvent::ScanProgress` every 10 files + a final
    /// `SyncEvent::ScanComplete` so the UI can render a spinner / count.
    pub async fn scan_roost(&self) -> SyncResult<usize> {
        let roost = self.inner.roost.clone();

        // On SAF (content://) roosts the engine must NOT populate its
        // DocStore from disk on its own — doing so creates Automerge
        // docs with a *different* initial text object than the peer's
        // docs for the same notes. When the two later sync, Automerge
        // ends up with two text objects inside one doc; actor-id
        // tiebreak picks a winner and the loser's text becomes invisible
        // to `render()`. The user then sees their file get wiped on
        // the losing side. Instead we leave Android's DocStore empty
        // at startup: when it pairs to a peer that *does* have the docs,
        // the NoteList / FullDoc exchange adopts the peer's copies
        // wholesale and both sides end up with genuinely shared history.
        //
        // Android's own write_file IPC still feeds edits in via
        // notify_local_write, so post-pair changes propagate normally.
        let roost_str = roost.to_string_lossy();
        if roost_str.starts_with("content://") {
            info!(roost = %roost.display(), "scan_roost: skipping — SAF roost, will hydrate via peer FullDoc instead");
            let _ = self.inner.events.send(SyncEvent::ScanComplete { count: 0 });
            return Ok(0);
        }

        // One guarded walk (resolve_fs → backend or on-demand NativeFs). The
        // RoostFs walk (`collect_markdown` → `list_recursive` → `list` →
        // safe_resolve) stays within the roost even across symlinked dirs; the
        // raw std::fs tree walk that could pull external files into the CRDT is
        // gone.
        let files: Vec<(NotePath, String)> = {
            use crate::roostfs::walk::collect_markdown;
            let fsh = super::peer::resolve_fs(&roost, self.inner.roost_fs.as_deref());
            let fs = fsh.as_fs();
            match collect_markdown(fs).await {
                Ok(pairs) => pairs
                    .into_iter()
                    .filter_map(|(full, content)| {
                        let display_root = fs.display_root();
                        let trimmed = full
                            .trim_start_matches(display_root)
                            .trim_start_matches(['/', '\\'])
                            .replace('\\', "/");
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some((NotePath(trimmed), content))
                        }
                    })
                    .collect(),
                Err(e) => {
                    warn!(error = %e, "scan_roost: RoostFs walk failed");
                    Vec::new()
                }
            }
        };

        let total = files.len();
        info!(total, roost = %roost.display(), "scan_roost: starting");
        let mut processed = 0usize;

        for (note, _snapshot) in files {
            {
                // Serialize this note's ingest against a concurrent remote adopt
                // (a peer can connect and adopt mid-scan). See `note_locks`.
                let _note_guard = self.inner.note_locks.lock(&note).await;
                // B4 (SYN-I3): re-read the content UNDER the lock. The
                // `collect_markdown` snapshot was taken before any lock, so a
                // peer that adopted this note between the walk and here could
                // have committed+rendered newer content; applying the pre-lock
                // snapshot would clobber it and fan the stale text back out.
                // Read-after-lock mirrors the `apply_local_change` Modified arm.
                // (Cost: one extra read per note vs. reusing the walk snapshot —
                // acceptable for a one-time bootstrap; a paths-only walk is a
                // possible follow-up.) A file that vanished since the walk, or a
                // rejected escape, reads Err → skip.
                let text = match super::peer::resolve_fs(
                    &self.inner.roost,
                    self.inner.roost_fs.as_deref(),
                )
                .as_fs()
                .read(&note.0)
                .await
                {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(_) => continue,
                };
                let store = self.inner.doc_store.lock().await;
                // First-time ingest vs. rescan:
                //
                // - EXISTING doc → `load_or_create` (under our device actor). No
                //   data loss on rescan; the `apply_markdown` below no-ops when
                //   the content hasn't changed.
                // - FIRST-TIME ingest of an on-disk note → seed with a
                //   DETERMINISTIC, path-derived actor (`seed_from_markdown`)
                //   instead of the device actor. Two desktops that independently
                //   scan the SAME path with IDENTICAL content then produce
                //   byte-identical Automerge histories that merge to ONE text
                //   object — sidestepping the dual-text-object divergence a
                //   per-device actor causes (two independent `text` objects,
                //   actor-id tiebreak silently hiding one side's content). This
                //   is the same convergence fix the SAF bootstrap path already
                //   uses (`peer::seed_from_markdown`); desktop scan just wasn't
                //   using it. Genuinely different content under the same
                //   path-actor forks the seq and surfaces as a detectable
                //   conflict (handled by the recovery/conflict-artifact path)
                //   rather than a silent partial loss. The DocStore reloads the
                //   persisted seed under our device actor on the next
                //   `load_or_create`, so every subsequent local edit is authored
                //   normally.
                let mut doc = if store.exists(&note) {
                    match store.load_or_create(&note) {
                        Ok(d) => d,
                        Err(e) => {
                            warn!(error = %e, note = %note, "scan_roost: load_or_create failed");
                            continue;
                        }
                    }
                } else {
                    match crate::sync::crdt::NoteDoc::seed_from_markdown(&note.0, &text) {
                        Ok(d) => d,
                        Err(e) => {
                            warn!(error = %e, note = %note, "scan_roost: seed_from_markdown failed");
                            continue;
                        }
                    }
                };
                // A tombstoned doc whose file is back on disk (external
                // recreate, restore-from-backup, etc.) must NOT be silently
                // un-deleted by a bulk scan — that would resurrect a note the
                // user deleted on purpose. Un-delete is an explicit Restore
                // action only. Skip applying the on-disk text here.
                match doc.is_deleted() {
                    Ok(true) => {
                        debug!(note = %note, "scan_roost: skipping tombstoned note present on disk");
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(error = %e, note = %note, "scan_roost: is_deleted check failed");
                        continue;
                    }
                }
                if let Err(e) = doc.apply_markdown(&text) {
                    warn!(error = %e, note = %note, "scan_roost: apply_markdown failed");
                    continue;
                }
                if let Err(e) = store.save(&note, &mut doc) {
                    warn!(error = %e, note = %note, "scan_roost: save failed");
                    continue;
                }
            }
            processed += 1;
            if processed.is_multiple_of(10) || processed == total {
                let _ = self
                    .inner
                    .events
                    .send(SyncEvent::ScanProgress { processed, total });
            }
        }

        info!(processed, "scan_roost: complete");
        let _ = self
            .inner
            .events
            .send(SyncEvent::ScanComplete { count: processed });

        // Nudge every connected peer to re-announce so they can pull any
        // new paths we just ingested. Non-fatal if a peer's channel is
        // already closed — the peer will sync fresh on next reconnect.
        let peers: Vec<PeerHandle> = self
            .inner
            .peers
            .read()
            .await
            .values()
            .map(clone_handle)
            .collect();
        for peer in peers {
            if let Err(e) = peer.reannounce_notes().await {
                debug!(peer = %peer.display_name, error = %e, "scan_roost: reannounce failed");
            }
        }

        Ok(processed)
    }

    /// Render to disk every note the DocStore knows about that's missing
    /// from the roost.
    ///
    /// Closes the asymmetry that the strategy doc flagged: when the user
    /// deletes roost files (or files are lost out from under the app —
    /// SAF tree URI yanked, sync to a fresh device that already paired
    /// with a populated peer, etc.), the DocStore retains the CRDT
    /// history but `announce_notes` and the peer's matching `NoteList`
    /// reply both think both sides "have" each note, so neither side
    /// triggers a `RequestFullDoc` bootstrap. Without this method, the
    /// notes stay invisible until the user manually re-opens them.
    ///
    /// Skips notes whose rendered text is empty — the natural state of a
    /// freshly-created CRDT before any markdown has been applied. We
    /// don't want to scatter zero-byte files all over the roost every
    /// time someone touches the DocStore.
    ///
    /// Returns the number of files actually written. Per-note failures
    /// are logged and skipped so a single bad path can't bring down
    /// roost-wide recovery.
    pub async fn recover_missing_files(&self) -> SyncResult<usize> {
        let store_arc = self.inner.doc_store.clone();
        let paths = {
            let store = store_arc.lock().await;
            store.list()?
        };

        let mut restored = 0usize;
        for note in paths {
            // Serialize the missing-check → doc read → disk write against a
            // concurrent adopt/local-save for this note, so we never render a
            // stale doc over a file another path is actively writing. See
            // `note_locks`.
            let _note_guard = self.inner.note_locks.lock(&note).await;
            let exists = match self.note_exists_on_disk(&note).await {
                Ok(b) => b,
                Err(e) => {
                    warn!(note = %note, error = %e, "recover_missing_files: existence check failed — skipping");
                    continue;
                }
            };
            if exists {
                continue;
            }

            let (text, md_hash) = {
                let store = store_arc.lock().await;
                let doc = match store.load_or_create(&note) {
                    Ok(d) => d,
                    Err(e) => {
                        warn!(note = %note, error = %e, "recover_missing_files: load_or_create failed — skipping");
                        continue;
                    }
                };
                // CRITICAL: a tombstone is "missing from disk" by definition
                // (the file was trashed on delete). Recovering it would
                // resurrect every deleted note on the next pass. Skip
                // deleted docs — the file is supposed to be gone.
                match doc.is_deleted() {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        warn!(note = %note, error = %e, "recover_missing_files: is_deleted check failed — skipping");
                        continue;
                    }
                }
                let text = match doc.render() {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(note = %note, error = %e, "recover_missing_files: render failed — skipping");
                        continue;
                    }
                };
                let md_hash = match doc.md_hash() {
                    Ok(h) => h,
                    Err(e) => {
                        warn!(note = %note, error = %e, "recover_missing_files: md_hash failed — skipping");
                        continue;
                    }
                };
                (text, md_hash)
            };
            if text.is_empty() {
                // A doc that renders empty is EITHER a legitimately empty note
                // (the user cleared it) OR a divergent/corrupt CRDT object
                // whose real text is trapped in a *losing* Automerge text
                // object and is invisible to `render()` — the dual-text-object
                // artifact that also makes such notes uneditable. Tell them
                // apart by md_hash: an intentionally-empty note hashes the
                // empty string.
                //
                // - Legit empty  → recover it (restoring the file is correct).
                // - Divergent    → writing an empty file would cement the data
                //   loss, so refuse AND surface it loudly instead of the
                //   previous silent `continue` that dropped notes without a
                //   trace.
                if md_hash != crate::sync::crdt::sha256_text("") {
                    warn!(
                        note = %note,
                        "recover_missing_files: doc renders EMPTY but md_hash says it holds content — likely a divergent CRDT object (dual text object). NOT writing an empty file over it; note left in the docstore for inspection/repair."
                    );
                    continue;
                }
                info!(note = %note, "recover_missing_files: restoring legitimately-empty note to disk");
            }

            if let Err(e) = self.write_recovered_note(&note, &text).await {
                warn!(note = %note, error = %e, "recover_missing_files: write failed — skipping");
                continue;
            }
            restored += 1;
            let _ = self.inner.events.send(SyncEvent::NoteWritten {
                path: note.0.clone(),
            });
        }
        Ok(restored)
    }

    /// Existence probe used by `recover_missing_files` and the watcher's
    /// removal-verification guard. Routes through `RoostFs::exists` when a
    /// backend is installed (the only path that works for SAF `content://`
    /// roots) and falls back to `std::fs` so desktop still works if no backend
    /// is mounted.
    pub(super) async fn note_exists_on_disk(&self, note: &NotePath) -> SyncResult<bool> {
        super::peer::resolve_fs(&self.inner.roost, self.inner.roost_fs.as_deref())
            .as_fs()
            .exists(&note.0)
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))
    }

    /// Write `text` to the roost at the relative path the note represents.
    /// Mirrors what `peer::render_to_roost` does on the receive path —
    /// `mkdir` parents, then `write_atomic` (or temp+rename on
    /// `std::fs`). Kept narrow on purpose: this path doesn't carry the
    /// `md_hash` divergence-guard from the sync receive path because
    /// recovery only fires when the file *doesn't exist*, so there's no
    /// existing-content invariant to protect.
    pub(super) async fn write_recovered_note(&self, note: &NotePath, text: &str) -> SyncResult<()> {
        // SYN-B1 (twin of render_to_roost): recover_missing_files (auto-fired at
        // startup) and restore_note materialize DocStore paths that were NOT
        // vetted at adopt time on a migration-poisoned store — a pre-fix
        // malicious peer could have committed `.git/config` / `.corvux/*` /
        // non-markdown. Gate at the sink so recover + restore + queue-adopt are
        // all covered by ONE choke; skip (don't error) so one poisoned entry
        // can't abort recovery of the rest.
        if super::policy::vet_remote_rel(note).is_err() {
            warn!(path = %note, "write_recovered_note: path rejected by write policy — skipping (not materialized)");
            return Ok(());
        }
        let rel = &note.0;
        // One guarded path (resolve_fs → backend or on-demand NativeFs); the
        // raw std::fs fallback that could follow a symlink out of the roost is
        // gone — safe_resolve handles every component.
        let fsh = super::peer::resolve_fs(&self.inner.roost, self.inner.roost_fs.as_deref());
        let fs = fsh.as_fs();
        if let Some(slash) = rel.rfind('/') {
            let parent_rel = &rel[..slash];
            if !parent_rel.is_empty() {
                if let Err(e) = fs.mkdir(parent_rel).await {
                    use crate::roostfs::RoostError;
                    if !matches!(e, RoostError::AlreadyExists(_)) {
                        warn!(path = parent_rel, error = %e, "recover_missing_files: mkdir failed");
                    }
                }
            }
        }
        fs.write_atomic(rel, text.as_bytes())
            .await
            .map_err(|e| SyncError::Io(std::io::Error::other(e.to_string())))
    }

    /// Trash a note's markdown (recoverable) when a tombstone arrives via a
    /// path that doesn't go through `peer::reconcile_doc_to_roost` (the queue
    /// receive path).
    ///
    /// Three outcomes, deliberately distinguished so the queue path can honor
    /// ACK-after-disk (H-6) for deletions:
    /// - `Ok(true)`  — a file was actually present and removed. Caller fires
    ///   `NoteDeleted` (a real disk transition) and ACKs the message.
    /// - `Ok(false)` — nothing was on disk (`NotFound`). Disk already agrees
    ///   with the tombstone, so it's safe to ACK; no event (nothing changed).
    /// - `Err(e)`    — the delete genuinely failed (permissions, I/O). Caller
    ///   must NOT ACK: the DocStore is tombstoned but the file survives, so a
    ///   later desktop scan would resurrect it. Keep the message for retry.
    ///
    /// Previously this collapsed `NotFound` and a real failure into a single
    /// `false`, so the queue path ACKed on genuine disk failures — the gap this
    /// return type closes.
    pub(super) async fn trash_note(&self, note: &NotePath) -> SyncResult<bool> {
        // Delegate to the single shared trash impl (SYN-B8): identical 3-state
        // contract + symlink-safe `resolve_fs` routing for the queue and live
        // receive paths, so they can't drift apart.
        super::peer::trash_in_roost(&self.inner.roost, self.inner.roost_fs.as_deref(), note).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::identity::Identity;
    use crate::sync::transport::tcp_tls::TlsIdentity;
    use std::sync::Arc;
    use tempfile::tempdir;

    /// Mirrors `queue_ops::tests::setup_engine` / `engine::tests::setup_engine`
    /// (both private to their own modules) — a single, unpaired `SyncEngine`
    /// with a real roost + app-data temp dir and no `RoostFs` backend, so
    /// `trash_note` exercises the desktop `std::fs` fallback branch.
    fn setup_engine() -> (SyncEngine, tempfile::TempDir, tempfile::TempDir) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let roost = tempdir().unwrap();
        let app_data = tempdir().unwrap();
        let identity = Arc::new(Identity::load_or_create(app_data.path()).unwrap());
        let tls = Arc::new(TlsIdentity::generate().unwrap());
        let state_dir = app_data.path().join("sync-state");
        let engine = SyncEngine::new(
            roost.path().to_path_buf(),
            state_dir,
            None, // no RoostFs backend — tests use the std::fs fallback
            identity,
            tls,
            "test-device".into(),
        )
        .unwrap();
        (engine, roost, app_data)
    }

    /// Ok(true): a file genuinely present on disk gets removed, and the
    /// caller (`apply_queue_full_doc`) treats that as a real disk transition
    /// worth a `NoteDeleted` event + ACK.
    #[tokio::test(flavor = "multi_thread")]
    async fn trash_note_returns_ok_true_and_removes_file_when_present() {
        let (engine, roost, _app) = setup_engine();
        let note = NotePath("present.md".into());
        let path = roost.path().join("present.md");
        std::fs::write(&path, b"hello").unwrap();

        let removed = engine.trash_note(&note).await.unwrap();
        assert!(
            removed,
            "trash_note must return Ok(true) when a file was present and removed"
        );
        assert!(!path.exists(), "the file must actually be gone from disk");
    }

    /// Ok(false): nothing on disk means disk already agrees with the
    /// tombstone — safe to ACK, but no `NoteDeleted` event since nothing
    /// changed. This is the "NotFound" branch of the three-state contract.
    #[tokio::test(flavor = "multi_thread")]
    async fn trash_note_returns_ok_false_when_nothing_on_disk() {
        let (engine, _roost, _app) = setup_engine();
        let note = NotePath("never-existed.md".into());

        let removed = engine.trash_note(&note).await.unwrap();
        assert!(
            !removed,
            "trash_note must return Ok(false), not Err, when there is nothing to delete"
        );
    }

    /// SYN-B1 (twin of `render_to_roost`): the recovery sink must NOT
    /// materialize a policy-rejected DocStore path — e.g. a `.git/config` a
    /// pre-fix malicious peer committed before the adopt-time gate existed, which
    /// `recover_missing_files` would otherwise write to disk automatically at
    /// startup. Skip (Ok, not Err) so one poisoned entry can't abort recovery of
    /// the rest; a legitimate note through the same sink still writes.
    #[tokio::test(flavor = "multi_thread")]
    async fn write_recovered_note_refuses_policy_rejected_path() {
        let (engine, roost, _app) = setup_engine();

        let poisoned = NotePath(".git/config".into());
        let r = engine
            .write_recovered_note(&poisoned, "[core]\n\thooksPath = /tmp/evil\n")
            .await;
        assert!(
            r.is_ok(),
            "policy reject must skip (Ok), not Err — one bad entry can't abort recovery"
        );
        assert!(
            !roost.path().join(".git/config").exists(),
            "a policy-rejected path must NOT be materialized to the roost"
        );
        // A non-markdown poisoned path is likewise refused.
        let sh = NotePath("evil.sh".into());
        engine
            .write_recovered_note(&sh, "#!/bin/sh\n")
            .await
            .unwrap();
        assert!(!roost.path().join("evil.sh").exists());

        // A legitimate note through the same sink still recovers.
        let ok = NotePath("legit.md".into());
        engine.write_recovered_note(&ok, "# hi").await.unwrap();
        assert!(
            roost.path().join("legit.md").exists(),
            "a legit note must still be recovered to disk"
        );
    }

    /// Fix #3 headline: two DIFFERENT desktop devices that scan the SAME note
    /// path with IDENTICAL content must produce byte-identical Automerge
    /// histories — so when they later sync they converge to ONE text object
    /// instead of forking two (the dual-text-object silent-loss bug). This holds
    /// because first-time scan ingest now seeds with a deterministic path-derived
    /// actor, not the per-device actor.
    #[tokio::test(flavor = "multi_thread")]
    async fn desktop_scan_seeds_path_deterministically_across_devices() {
        let (engine_a, roost_a, app_a) = setup_engine();
        let (engine_b, roost_b, app_b) = setup_engine();

        let rel = "Notes/shared.md";
        let body = "# Shared\n\nWritten independently on two devices before pairing.\n";
        std::fs::create_dir_all(roost_a.path().join("Notes")).unwrap();
        std::fs::create_dir_all(roost_b.path().join("Notes")).unwrap();
        std::fs::write(roost_a.path().join(rel), body).unwrap();
        std::fs::write(roost_b.path().join(rel), body).unwrap();

        engine_a.scan_roost().await.unwrap();
        engine_b.scan_roost().await.unwrap();

        let doc_a = std::fs::read(
            app_a
                .path()
                .join("sync-state/docs/Notes/shared.md.automerge"),
        )
        .unwrap();
        let doc_b = std::fs::read(
            app_b
                .path()
                .join("sync-state/docs/Notes/shared.md.automerge"),
        )
        .unwrap();
        assert_eq!(
            doc_a, doc_b,
            "identical path+content on two devices must seed byte-identical CRDT histories"
        );

        // And the persisted bytes match a direct deterministic seed — proving
        // the scan used `seed_from_markdown`, not a per-device actor.
        let reference = crate::sync::crdt::NoteDoc::seed_from_markdown(rel, body)
            .unwrap()
            .save();
        assert_eq!(
            doc_a, reference,
            "scan ingest must use the deterministic path-derived seed"
        );
    }

    /// Err(e): a genuine disk-delete failure (not "file already gone") must
    /// surface as an error so the caller does NOT ACK — the regression this
    /// return type exists to prevent. The note file exists (so it's not
    /// `NotFound`), but `.corvux-trash` is occupied by a FILE, so the backend's
    /// `create_dir_all(".corvux-trash")` fails and `trash` returns a genuine
    /// I/O error — distinct from the `NotFound` "already gone" case, which the
    /// old boolean-collapsing code wrongly reported as `false`/success.
    #[tokio::test(flavor = "multi_thread")]
    async fn trash_note_returns_err_when_delete_genuinely_fails() {
        let (engine, roost, _app) = setup_engine();
        let note = NotePath("occupied.md".into());
        // The note really exists on disk (rules out NotFound)...
        std::fs::write(roost.path().join("occupied.md"), b"content").unwrap();
        // ...but the trash directory can't be created because a file sits where
        // it needs to go — a genuine, non-NotFound delete failure.
        std::fs::write(roost.path().join(".corvux-trash"), b"not a dir").unwrap();

        let result = engine.trash_note(&note).await;
        assert!(
            result.is_err(),
            "a genuine delete failure (trash dir un-creatable) must be Err, not \
             silently collapsed into Ok(false) like NotFound is"
        );
    }
}
