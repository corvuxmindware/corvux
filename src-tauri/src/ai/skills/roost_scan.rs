//! Hardened discovery of roost-local skill and subagent definitions.
//!
//! Review AT-5 (2026-11-12): the registries and watchers used raw `std::fs`
//! (which follows symlinks) on `<roost>/.corvux/skills|agents`. A roost that
//! arrives from somewhere else (a shared folder, a cloned repo) can carry
//! `.corvux/skills/x -> /somewhere/private`; `skill_load` then handed the
//! target's body to the cloud model. This is the same hole TOCTOU-004 closed for
//! `@mention` reads, one directory over.
//!
//! The fix is one choke point. Every roost-side read goes through the roost
//! filesystem (`RoostFs`), whose native backend resolves symlinks at EVERY
//! ancestor and refuses anything that lands outside the roost
//! (`roostfs::native::resolve_within`). Skills and subagents share this module
//! because their layouts are the same: `<roost>/.corvux/<kind>/<name>/<FILE>.md`.
//!
//! * The async [`scan_definitions`] takes any `RoostFs`, so a caller that already
//!   holds the installed backend can use it directly.
//! * [`scan_native`] is the synchronous entry point for the hot-reload watchers
//!   (a plain `notify` thread, no tokio context) and for `refresh`. It builds an
//!   on-demand, UNLOCKED `NativeFs` view of the roost root — a secondary view,
//!   which the roost-boundary guard allows because the open roost's lock is held
//!   by `RoostFsState` for as long as the roost is open.
//!
//! Not covered on purpose: `~/.corvux/skills|agents`. That folder belongs to the
//! user, not to any roost, and has no boundary to stay inside; people keep it
//! under a dotfile manager, so symlinks there keep working.

use std::future::Future;
use std::path::{Path, PathBuf};

use crate::roostfs::native::NativeFs;
use crate::roostfs::{RoostError, RoostFs};

/// What one scan found. `failed` is for the log: a malformed or refused entry
/// never stops the others from loading.
#[derive(Debug, Default)]
pub(crate) struct ScanOutcome {
    /// `(roost-relative manifest path, file text)`, for example
    /// `(".corvux/skills/fit/SKILL.md", "---\nname: fit\n...")`.
    pub manifests: Vec<(String, String)>,
    /// Entries that were skipped, with a reason that names no content.
    pub failed: Vec<(PathBuf, String)>,
}

/// Why a definitions folder or manifest was refused. One wording for every
/// "this leaves the roost" case so the log line is greppable.
const LEAVES_ROOST: &str =
    "refused: the path resolves outside the roost (a link or junction pointing elsewhere)";

/// Read every `<dir_rel>/<name>/<manifest_name>` that `fs` can reach.
///
/// `dir_rel` is roost-relative (`.corvux/skills`), or `""` for the roost root.
/// A missing folder is not an error (the user simply has no definitions yet).
pub(crate) async fn scan_definitions(
    fs: &dyn RoostFs,
    dir_rel: &str,
    manifest_name: &str,
) -> ScanOutcome {
    let mut out = ScanOutcome::default();

    let entries = match fs.list(dir_rel).await {
        Ok(entries) => entries,
        // Absent folder (the backend reports a missing directory as NotDir).
        Err(RoostError::NotFound(_) | RoostError::NotDir(_)) => return out,
        Err(RoostError::OutsideRoost(_)) => {
            out.failed
                .push((PathBuf::from(dir_rel), LEAVES_ROOST.into()));
            return out;
        }
        Err(e) => {
            out.failed.push((
                PathBuf::from(dir_rel),
                format!("could not list folder: {e}"),
            ));
            return out;
        }
    };

    for entry in entries {
        let folder_rel = entry.rel_path;

        if !entry.is_dir {
            // The native backend does not follow links when it classifies an
            // entry, so a link to a folder looks like a file here. Ask the
            // backend to list it: a link to a folder INSIDE the roost is a
            // legitimate definition folder, one that leaves the roost is refused
            // and named, and a plain file (or anything unreadable) is not a
            // definition folder at all.
            match fs.list(&folder_rel).await {
                Ok(_) => {}
                Err(RoostError::OutsideRoost(_)) => {
                    out.failed
                        .push((PathBuf::from(&folder_rel), LEAVES_ROOST.into()));
                    continue;
                }
                Err(_) => continue,
            }
        }

        let manifest_rel = format!("{folder_rel}/{manifest_name}");
        match fs.read(&manifest_rel).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => out.manifests.push((manifest_rel, text)),
                Err(_) => out
                    .failed
                    .push((PathBuf::from(&manifest_rel), "not valid UTF-8".into())),
            },
            // A folder without a manifest is normal (scratch folder, assets).
            Err(RoostError::NotFound(_) | RoostError::NotDir(_)) => {}
            Err(RoostError::OutsideRoost(_)) => out
                .failed
                .push((PathBuf::from(&manifest_rel), LEAVES_ROOST.into())),
            Err(e) => out
                .failed
                .push((PathBuf::from(&manifest_rel), format!("could not read: {e}"))),
        }
    }
    out
}

/// Split a definitions directory into `(containment root, roost-relative dir)`.
///
/// `<root>/.corvux/<kind>` yields `(<root>, ".corvux/<kind>")`, so a `.corvux`
/// or `.corvux/<kind>` that is itself a link out of the roost is caught. Any
/// other shape (a bare directory with no roost around it) is its own root:
/// still contained, just not against anything wider.
pub(crate) fn split_definitions_dir(dir: &Path, kind: &str) -> (PathBuf, String) {
    let is_named = |p: &Path, name: &str| p.file_name().is_some_and(|n| n == name);
    if is_named(dir, kind) {
        if let Some(dot_corvux) = dir.parent().filter(|p| is_named(p, ".corvux")) {
            let root = match dot_corvux.parent() {
                Some(r) if !r.as_os_str().is_empty() => r.to_path_buf(),
                _ => PathBuf::from("."),
            };
            return (root, format!(".corvux/{kind}"));
        }
    }
    (dir.to_path_buf(), String::new())
}

/// The one place in `ai/skills` + `ai/subagents` that opens a roost directly: an
/// on-demand secondary view (see the module docs).
fn native_view(root: &Path) -> NativeFs {
    NativeFs::new(root)
}

/// Run `fut` to completion from ANY thread, including a tokio worker.
///
/// The watcher callbacks run on a plain `notify` thread, `open_roost` calls the
/// initial scan from inside a tokio task, and unit tests have no runtime at
/// all. A helper thread with its own current-thread runtime works in all three
/// and cannot deadlock against the app's runtimes. The scan is a handful of
/// small reads, so the cost of a short-lived thread is irrelevant.
fn run_blocking<T, F>(fut: F) -> Result<T, String>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    std::thread::Builder::new()
        .name("corvux-definition-scan".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| rt.block_on(fut))
                .map_err(|e| format!("scan runtime failed: {e}"))
        })
        .map_err(|e| format!("could not start the scan thread: {e}"))?
        .join()
        .map_err(|_| "the scan thread panicked".to_string())?
}

/// Synchronous scan of the definitions directory `dir` (`<roost>/.corvux/<kind>`),
/// through a hardened native view of the roost. See the module docs.
pub(crate) fn scan_native(dir: &Path, kind: &str, manifest_name: &str) -> ScanOutcome {
    let (root, dir_rel) = split_definitions_dir(dir, kind);
    // A roost folder that is gone is "nothing to load", not a refusal: the
    // native view fails closed on a root it cannot resolve.
    if !root.is_dir() {
        return ScanOutcome::default();
    }
    let fs = native_view(&root);
    let manifest_name = manifest_name.to_string();
    let rel_for_error = dir_rel.clone();
    run_blocking(async move { scan_definitions(&fs, &dir_rel, &manifest_name).await })
        .unwrap_or_else(|e| ScanOutcome {
            manifests: Vec::new(),
            failed: vec![(PathBuf::from(rel_for_error), e)],
        })
}

/// `true` when `dir` exists AND stays inside its roost. A `.corvux` link that
/// leaves the roost reports `false`, so the watcher does not attach to it.
pub(crate) fn definitions_dir_exists(dir: &Path, kind: &str) -> bool {
    let (root, dir_rel) = split_definitions_dir(dir, kind);
    root.is_dir()
        && native_view(&root)
            .exists_blocking(&dir_rel)
            .unwrap_or(false)
}

/// Create `dir` (and parents) the way the roost backend would: refused if any
/// ancestor is a link that leaves the roost. Replaces a raw `create_dir_all`
/// that followed such a link and made a folder outside the roost.
pub(crate) fn ensure_definitions_dir(dir: &Path, kind: &str) -> Result<(), String> {
    let (root, dir_rel) = split_definitions_dir(dir, kind);
    if !root.is_dir() {
        return Err("the roost folder does not exist".into());
    }
    native_view(&root)
        .mkdir_all_blocking(&dir_rel)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    /// Make `link` a directory link to `target`. A Unix symlink, or a Windows
    /// junction: `mklink /J` needs no administrator rights or developer mode,
    /// so these tests run on a plain Windows host (the `#[cfg(unix)]`-only
    /// symlink tests of TOCTOU-004 never did). Panics, loudly, if it cannot.
    pub(crate) fn make_dir_link(link: &Path, target: &Path) {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).expect("create directory symlink");
        }
        #[cfg(windows)]
        {
            // `mklink` reads a `/` in a path as the start of a switch, and a
            // path built with `join(".corvux/skills")` has one.
            let backslashed = |p: &Path| p.to_string_lossy().replace('/', "\\");
            let out = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(backslashed(link))
                .arg(backslashed(target))
                .output()
                .expect("run mklink");
            assert!(
                out.status.success(),
                "mklink /J failed: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::make_dir_link;
    use super::*;
    use tempfile::TempDir;

    const SKILL: &str = "---\nname: x\ndescription: d\n---\nbody\n";

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A roost with one honest skill (`good`) and an unrelated outside folder
    /// holding a skill-shaped file that must never be read through the roost.
    fn roost_and_outside() -> (TempDir, TempDir, PathBuf) {
        let roost = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write(
            &roost.path().join(".corvux/skills/good/SKILL.md"),
            "---\nname: good\ndescription: d\n---\nbody\n",
        );
        write(&outside.path().join("stolen/SKILL.md"), SKILL);
        let skills = roost.path().join(".corvux/skills");
        (roost, outside, skills)
    }

    fn names(out: &ScanOutcome) -> Vec<&str> {
        out.manifests.iter().map(|(p, _)| p.as_str()).collect()
    }

    #[test]
    fn splits_a_roost_shaped_directory_and_leaves_other_shapes_alone() {
        let (root, rel) = split_definitions_dir(Path::new("/r/.corvux/skills"), "skills");
        assert_eq!(root, PathBuf::from("/r"));
        assert_eq!(rel, ".corvux/skills");
        // Wrong kind, or no `.corvux` parent: the directory is its own root.
        for dir in ["/r/.corvux/agents", "/r/skills", "/r/other/skills"] {
            let (root, rel) = split_definitions_dir(Path::new(dir), "skills");
            assert_eq!(root, PathBuf::from(dir), "{dir}");
            assert_eq!(rel, "");
        }
    }

    #[test]
    fn finds_folders_that_hold_a_manifest_and_ignores_the_rest() {
        let (_roost, _outside, skills) = roost_and_outside();
        write(&skills.join("no-manifest/notes.txt"), "x");
        write(&skills.join("loose.md"), "a file, not a folder");
        let out = scan_native(&skills, "skills", "SKILL.md");
        assert_eq!(names(&out), vec![".corvux/skills/good/SKILL.md"]);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
    }

    #[test]
    fn a_missing_definitions_folder_or_roost_is_empty_not_a_failure() {
        let roost = TempDir::new().unwrap();
        let out = scan_native(&roost.path().join(".corvux/skills"), "skills", "SKILL.md");
        assert!(out.manifests.is_empty() && out.failed.is_empty());
        let out = scan_native(
            Path::new("/no/such/roost/.corvux/skills"),
            "skills",
            "SKILL.md",
        );
        assert!(out.manifests.is_empty() && out.failed.is_empty());
    }

    /// AT-5 core: a skill folder that is a link out of the roost is refused and
    /// its target is never read. Runs on Windows (junction) and Unix (symlink).
    #[test]
    fn a_skill_folder_linked_out_of_the_roost_is_refused() {
        let (_roost, outside, skills) = roost_and_outside();
        make_dir_link(&skills.join("evil"), &outside.path().join("stolen"));
        let out = scan_native(&skills, "skills", "SKILL.md");
        assert_eq!(names(&out), vec![".corvux/skills/good/SKILL.md"], "{out:?}");
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert!(out.failed[0].0.ends_with("evil"));
        assert!(out.failed[0].1.contains("outside the roost"));
    }

    #[test]
    fn a_skills_folder_that_is_itself_a_link_out_is_refused() {
        let roost = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write(&outside.path().join("stolen/SKILL.md"), SKILL);
        std::fs::create_dir_all(roost.path().join(".corvux")).unwrap();
        let skills = roost.path().join(".corvux/skills");
        make_dir_link(&skills, outside.path());
        let out = scan_native(&skills, "skills", "SKILL.md");
        assert!(out.manifests.is_empty(), "{out:?}");
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
    }

    #[test]
    fn a_dot_corvux_folder_that_is_a_link_out_is_refused() {
        let roost = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write(&outside.path().join("skills/stolen/SKILL.md"), SKILL);
        make_dir_link(&roost.path().join(".corvux"), outside.path());
        let skills = roost.path().join(".corvux/skills");
        let out = scan_native(&skills, "skills", "SKILL.md");
        assert!(out.manifests.is_empty(), "{out:?}");
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        // The watcher must not attach to it, and must not create folders there.
        assert!(!definitions_dir_exists(&skills, "skills"));
        assert!(ensure_definitions_dir(&skills, "skills").is_err());
    }

    #[test]
    fn a_folder_link_that_stays_inside_the_roost_still_loads() {
        let (roost, _outside, skills) = roost_and_outside();
        make_dir_link(
            &skills.join("alias"),
            &roost.path().join(".corvux/skills/good"),
        );
        let out = scan_native(&skills, "skills", "SKILL.md");
        let mut found = names(&out);
        found.sort_unstable();
        assert_eq!(
            found,
            vec![
                ".corvux/skills/alias/SKILL.md",
                ".corvux/skills/good/SKILL.md"
            ]
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
    }

    /// The same-shape check for the other consumer: the manifest file name and
    /// the kind folder are parameters, so agents are covered by the same code.
    #[test]
    fn agents_use_the_same_hardened_scan() {
        let roost = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write(&outside.path().join("stolen/AGENT.md"), SKILL);
        write(&roost.path().join(".corvux/agents/ok/AGENT.md"), SKILL);
        let agents = roost.path().join(".corvux/agents");
        make_dir_link(&agents.join("evil"), &outside.path().join("stolen"));
        let out = scan_native(&agents, "agents", "AGENT.md");
        assert_eq!(names(&out), vec![".corvux/agents/ok/AGENT.md"]);
        assert_eq!(out.failed.len(), 1);
    }

    /// A SKILL.md that is itself a link to a file outside the roost. Creating a
    /// FILE symlink on Windows needs developer mode, so this half is Unix-only;
    /// the directory-link tests above cover Windows.
    #[cfg(unix)]
    #[test]
    fn a_manifest_that_is_a_file_symlink_out_of_the_roost_is_refused() {
        let (_roost, outside, skills) = roost_and_outside();
        std::fs::create_dir_all(skills.join("sneaky")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("stolen/SKILL.md"),
            skills.join("sneaky/SKILL.md"),
        )
        .unwrap();
        let out = scan_native(&skills, "skills", "SKILL.md");
        assert_eq!(names(&out), vec![".corvux/skills/good/SKILL.md"]);
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert!(out.failed[0].0.ends_with("sneaky/SKILL.md"));
    }

    #[test]
    fn the_scan_works_from_inside_a_running_tokio_task() {
        // `open_roost` runs the first scan from an async fn on a tokio worker.
        let (_roost, _outside, skills) = roost_and_outside();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        // Spawned, so it runs ON the runtime's only worker thread — exactly the
        // `open_roost` situation. A scan that tried to start or block on a
        // runtime from there would panic or deadlock.
        let out = rt.block_on(async {
            let skills = skills.clone();
            tokio::spawn(async move { scan_native(&skills, "skills", "SKILL.md") })
                .await
                .unwrap()
        });
        assert_eq!(names(&out), vec![".corvux/skills/good/SKILL.md"]);
    }

    #[test]
    fn a_folder_without_a_manifest_does_not_hide_its_neighbours() {
        let (_roost, _outside, skills) = roost_and_outside();
        write(&skills.join("aaa-empty/readme.txt"), "x");
        write(
            &skills.join("zzz/SKILL.md"),
            "---\nname: zzz\ndescription: d\n---\nb\n",
        );
        let out = scan_native(&skills, "skills", "SKILL.md");
        assert_eq!(out.manifests.len(), 2, "{out:?}");
    }
}
