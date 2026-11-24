//! The path contract, enforced ONCE, in front of every backend.
//!
//! [`RoostFs`] documents that "`..` segments and absolute paths are rejected at
//! the impl boundary". Until now that was true only because each impl chose to
//! call [`validate_rel`]: `NativeFs` always did (via `resolve_within`), but
//! `SafFs` did it in `trash_inner` alone — `read`, `write_atomic`, `create`,
//! `rename`, `mkdir`, `exists`, `list` forwarded the caller's string to the
//! Kotlin plugin untouched. Whether `read("../../other-tree/x.md")` escaped on
//! Android depended on `DocumentFile` resolution that no Rust test can see
//! (sharp-edges F5, 2026-11-11).
//!
//! [`ValidatedFs`] closes that structurally: [`RoostFsState::install`] — the
//! only way a backend becomes THE roost backend — wraps whatever it is given,
//! so the lexical gate is single-sourced and identical for native, SAF and any
//! future backend. A hostile path never reaches the inner impl; the
//! per-impl checks (symlink-safe `resolve_within`, SAF's `trash_inner`) stay as
//! defence in depth.
//!
//! The wrapper VALIDATES but forwards the caller's ORIGINAL string, so a path
//! that was legal yesterday behaves byte-for-byte the same today (e.g.
//! `NativeFs::list` echoes `rel_dir` into `Entry::rel_path`). Everything the
//! wrapper accepts has no `..`/`.` segment in either slash convention, no drive
//! or device prefix and no NUL.

use std::sync::Arc;

use async_trait::async_trait;

use super::{validate_rel, Entry, RoostError, RoostFs};

/// A backend that cannot be handed a path outside the roost.
pub struct ValidatedFs {
    inner: Arc<dyn RoostFs>,
}

impl ValidatedFs {
    pub fn new(inner: Arc<dyn RoostFs>) -> Self {
        Self { inner }
    }
}

/// Gate one roost-relative path. The cleaned form is discarded on purpose —
/// see the module docs.
fn gate(rel: &str) -> Result<(), RoostError> {
    validate_rel(rel).map(|_| ())
}

#[async_trait]
impl RoostFs for ValidatedFs {
    async fn list(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError> {
        gate(rel_dir)?;
        self.inner.list(rel_dir).await
    }

    // Delegate (don't inherit the default): SAF overrides this with a single
    // bulk walk, and losing that override would turn one JNI call into one per
    // directory.
    async fn list_recursive(&self, rel_dir: &str) -> Result<Vec<Entry>, RoostError> {
        gate(rel_dir)?;
        self.inner.list_recursive(rel_dir).await
    }

    async fn read(&self, rel: &str) -> Result<Vec<u8>, RoostError> {
        gate(rel)?;
        self.inner.read(rel).await
    }

    async fn write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), RoostError> {
        gate(rel)?;
        self.inner.write_atomic(rel, bytes).await
    }

    async fn create(&self, rel: &str, initial: &[u8]) -> Result<(), RoostError> {
        gate(rel)?;
        self.inner.create(rel, initial).await
    }

    async fn trash(&self, rel: &str) -> Result<(), RoostError> {
        gate(rel)?;
        self.inner.trash(rel).await
    }

    // Delegate: SAF overrides this to cross-check the caller's enumeration.
    async fn trash_expecting(&self, rel: &str, expected: &[String]) -> Result<(), RoostError> {
        gate(rel)?;
        for e in expected {
            gate(e)?;
        }
        self.inner.trash_expecting(rel, expected).await
    }

    async fn rename(&self, from_rel: &str, to_rel: &str) -> Result<(), RoostError> {
        gate(from_rel)?;
        gate(to_rel)?;
        self.inner.rename(from_rel, to_rel).await
    }

    async fn mkdir(&self, rel_dir: &str) -> Result<(), RoostError> {
        gate(rel_dir)?;
        self.inner.mkdir(rel_dir).await
    }

    async fn rmdir(&self, rel_dir: &str) -> Result<(), RoostError> {
        gate(rel_dir)?;
        self.inner.rmdir(rel_dir).await
    }

    async fn exists(&self, rel: &str) -> Result<bool, RoostError> {
        // Same answer `NativeFs::exists` gives for an escaping path: "no", not
        // an error — an error here would turn the probe into an oracle.
        match gate(rel) {
            Ok(()) => self.inner.exists(rel).await,
            Err(e) if e.is_path_refusal() => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn display_root(&self) -> &str {
        self.inner.display_root()
    }

    fn root_id(&self) -> String {
        self.inner.root_id()
    }
}

#[cfg(test)]
mod tests {
    //! The shared hostile-path corpus, run through the PUBLIC trait against a
    //! backend that records every call it receives. "Blocked" means the inner
    //! backend never saw the path — which is the property SAF lacked.
    use std::sync::Mutex;

    use super::*;

    /// Records every call; answers `Ok` to everything.
    #[derive(Default)]
    struct Probe {
        calls: Mutex<Vec<String>>,
    }

    impl Probe {
        fn note(&self, s: String) {
            self.calls.lock().unwrap().push(s);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl RoostFs for Probe {
        async fn list(&self, d: &str) -> Result<Vec<Entry>, RoostError> {
            self.note(format!("list:{d}"));
            Ok(vec![])
        }
        async fn list_recursive(&self, d: &str) -> Result<Vec<Entry>, RoostError> {
            self.note(format!("list_recursive:{d}"));
            Ok(vec![])
        }
        async fn read(&self, r: &str) -> Result<Vec<u8>, RoostError> {
            self.note(format!("read:{r}"));
            Ok(vec![])
        }
        async fn write_atomic(&self, r: &str, _b: &[u8]) -> Result<(), RoostError> {
            self.note(format!("write_atomic:{r}"));
            Ok(())
        }
        async fn create(&self, r: &str, _b: &[u8]) -> Result<(), RoostError> {
            self.note(format!("create:{r}"));
            Ok(())
        }
        async fn trash(&self, r: &str) -> Result<(), RoostError> {
            self.note(format!("trash:{r}"));
            Ok(())
        }
        async fn trash_expecting(&self, r: &str, e: &[String]) -> Result<(), RoostError> {
            self.note(format!("trash_expecting:{r}:{}", e.join("|")));
            Ok(())
        }
        async fn rename(&self, f: &str, t: &str) -> Result<(), RoostError> {
            self.note(format!("rename:{f}->{t}"));
            Ok(())
        }
        async fn mkdir(&self, d: &str) -> Result<(), RoostError> {
            self.note(format!("mkdir:{d}"));
            Ok(())
        }
        async fn rmdir(&self, d: &str) -> Result<(), RoostError> {
            self.note(format!("rmdir:{d}"));
            Ok(())
        }
        async fn exists(&self, r: &str) -> Result<bool, RoostError> {
            self.note(format!("exists:{r}"));
            Ok(true)
        }
        fn display_root(&self) -> &str {
            "probe"
        }
        fn root_id(&self) -> String {
            "probe-id".into()
        }
    }

    fn wrapped() -> (Arc<Probe>, ValidatedFs) {
        let probe = Arc::new(Probe::default());
        let fs = ValidatedFs::new(probe.clone());
        (probe, fs)
    }

    /// Everything `validate_rel` must refuse, in both slash conventions.
    const HOSTILE: &[&str] = &[
        "..",
        "../x.md",
        "a/../b.md",
        "a/..",
        "./x.md",
        "a/./b.md",
        r"..\x.md",
        r"a\..\b.md",
        "C:/Windows/win.ini",
        r"c:\x",
        "subdir/C:/win.ini",
        r"\\?\C:\x",
        r"\\.\COM1",
        "a\0b.md",
        "a/b\0",
    ];

    const BENIGN: &[&str] = &[
        "",
        "a.md",
        "Folder/Note.md",
        "Folder/",
        "资料/笔记.md",
        "🦀.md",
        ".hidden.md",
        ".corvux/skills/x.md",
        "/leading-slash.md",
        "notes.v2/a.md",
    ];

    fn blocked<T>(r: Result<T, RoostError>, what: &str, bad: &str) {
        assert!(
            matches!(r, Err(RoostError::OutsideRoost(_))),
            "{what}({bad:?}) should be OutsideRoost"
        );
    }

    #[tokio::test]
    async fn every_method_blocks_every_hostile_path_before_the_backend_sees_it() {
        for &bad in HOSTILE {
            let (probe, fs) = wrapped();
            blocked(fs.list(bad).await, "list", bad);
            blocked(fs.list_recursive(bad).await, "list_recursive", bad);
            blocked(fs.read(bad).await, "read", bad);
            blocked(fs.write_atomic(bad, b"x").await, "write_atomic", bad);
            blocked(fs.create(bad, b"x").await, "create", bad);
            blocked(fs.trash(bad).await, "trash", bad);
            blocked(fs.trash_expecting(bad, &[]).await, "trash_expecting", bad);
            blocked(fs.rename(bad, "ok.md").await, "rename(from)", bad);
            blocked(fs.rename("ok.md", bad).await, "rename(to)", bad);
            blocked(fs.mkdir(bad).await, "mkdir", bad);
            blocked(fs.rmdir(bad).await, "rmdir", bad);
            assert!(
                probe.calls().is_empty(),
                "{bad:?} reached the backend: {:?}",
                probe.calls()
            );
        }
    }

    #[tokio::test]
    async fn exists_answers_no_for_hostile_paths_and_never_asks_the_backend() {
        for &bad in HOSTILE {
            let (probe, fs) = wrapped();
            assert!(
                !fs.exists(bad).await.unwrap(),
                "{bad:?}: an escaping path must read as absent, not as an error"
            );
            assert!(probe.calls().is_empty(), "{bad:?} reached the backend");
        }
    }

    #[tokio::test]
    async fn trash_expecting_vets_the_expected_list_too() {
        let (probe, fs) = wrapped();
        let err = fs
            .trash_expecting("Work", &["Work/a.md".into(), "../evil.md".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, RoostError::OutsideRoost(_)));
        assert!(probe.calls().is_empty());
    }

    #[tokio::test]
    async fn benign_paths_reach_the_backend_byte_for_byte() {
        for &ok in BENIGN {
            let (probe, fs) = wrapped();
            fs.read(ok).await.unwrap();
            fs.write_atomic(ok, b"x").await.unwrap();
            fs.create(ok, b"x").await.unwrap();
            fs.list(ok).await.unwrap();
            assert!(fs.exists(ok).await.unwrap());
            assert_eq!(
                probe.calls(),
                vec![
                    format!("read:{ok}"),
                    format!("write_atomic:{ok}"),
                    format!("create:{ok}"),
                    format!("list:{ok}"),
                    format!("exists:{ok}"),
                ],
                "the original string must be forwarded unchanged"
            );
        }
    }

    #[tokio::test]
    async fn overrides_are_delegated_not_reimplemented() {
        let (probe, fs) = wrapped();
        fs.list_recursive("Notes").await.unwrap();
        fs.trash_expecting("Notes", &["Notes/a.md".into(), "Notes/b.md".into()])
            .await
            .unwrap();
        assert_eq!(
            probe.calls(),
            vec![
                "list_recursive:Notes".to_string(),
                "trash_expecting:Notes:Notes/a.md|Notes/b.md".to_string(),
            ],
            "SAF's bulk walk and expected-list cross-check live behind these"
        );
    }

    #[test]
    fn identity_passes_through() {
        let (_p, fs) = wrapped();
        assert_eq!(fs.display_root(), "probe");
        assert_eq!(fs.root_id(), "probe-id");
    }

    /// The property that matters in production: installing ANY backend makes
    /// it the wrapped one. A backend that is installed but not wrapped is the
    /// regression this guards (SAF reaches the app only through `install`).
    #[tokio::test]
    async fn installing_a_backend_wraps_it() {
        let probe = Arc::new(Probe::default());
        let state = crate::commands::files::RoostFsState::default();
        state.install(probe.clone()).await;

        let current = state.current().await.expect("installed");
        assert!(matches!(
            current.read("../../outside.md").await,
            Err(RoostError::OutsideRoost(_))
        ));
        assert!(probe.calls().is_empty(), "hostile path reached the backend");
        current.read("inside.md").await.unwrap();
        assert_eq!(probe.calls(), vec!["read:inside.md".to_string()]);
        // Identity still resolves through the wrapper (open_roost compares it).
        assert_eq!(current.root_id(), "probe-id");

        let (with_gen, _) = state.current_with_generation().await.expect("installed");
        assert!(with_gen.read("..").await.is_err());
    }

    /// Same corpus through a REAL backend, so the wrapper and `NativeFs`'s own
    /// symlink-safe resolve agree (defence in depth, not either/or).
    #[tokio::test]
    async fn real_native_backend_behind_the_wrapper_refuses_the_corpus() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "hi").unwrap();
        let fs = ValidatedFs::new(Arc::new(super::super::native::NativeFs::new(dir.path())));
        for &bad in HOSTILE {
            assert!(fs.read(bad).await.is_err(), "read({bad:?})");
            assert!(fs.write_atomic(bad, b"x").await.is_err(), "write({bad:?})");
        }
        assert_eq!(fs.read("a.md").await.unwrap(), b"hi");
        fs.write_atomic("sub/b.md", b"x").await.unwrap();
        assert_eq!(fs.read("sub/b.md").await.unwrap(), b"x");
    }
}
