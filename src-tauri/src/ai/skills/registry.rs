//! In-memory `SkillRegistry`. Scans `<roost>/.corvux/skills/<name>/SKILL.md`
//! and the global `~/.corvux/skills/<name>/SKILL.md` directory, parses
//! each manifest, and exposes lookup for the `skill_list` / `skill_load`
//! tools. Roost-local skills win on name collision so a per-project
//! customization can shadow the global.
//!
//! The watcher is built separately in `super::watcher` so tests can
//! construct a registry without spawning a notify thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use super::manifest::{parse_skill_md, read_and_parse, SkillManifest, SkillParseError};

/// Source bucket for a registered skill — used for precedence (roost
/// beats global) and for diagnostic output (`skill_list` could surface
/// the source in a follow-up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// Lives under `<roost>/.corvux/skills/`.
    Roost,
    /// Lives under `~/.corvux/skills/`. Lower precedence.
    Global,
}

/// One registered skill, ready to be listed or loaded.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub manifest: SkillManifest,
    pub source: SkillSource,
}

/// Lock-protected hash map of `name -> SkillEntry`. Reads are O(1);
/// `refresh()` rebuilds the map from disk. Cheap to call on every
/// debounced fs event.
#[derive(Default)]
pub struct SkillRegistry {
    inner: RwLock<HashMap<String, SkillEntry>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan both roost and global skill directories and replace the
    /// in-memory map with the result. Parsing failures for individual
    /// skills are logged but never bubble up — a malformed SKILL.md
    /// must not take down the registry.
    pub fn refresh(
        &self,
        roost_skills_dir: Option<&Path>,
        global_skills_dir: Option<&Path>,
    ) -> RefreshOutcome {
        let mut outcome = RefreshOutcome::default();
        let mut next: HashMap<String, SkillEntry> = HashMap::new();

        if let Some(dir) = global_skills_dir {
            collect_dir(dir, SkillSource::Global, &mut next, &mut outcome);
        }
        if let Some(dir) = roost_skills_dir {
            collect_dir(dir, SkillSource::Roost, &mut next, &mut outcome);
        }

        if let Ok(mut guard) = self.inner.write() {
            *guard = next;
        }
        outcome
    }

    /// Refresh with roost manifests already read through `RoostFs`. Global
    /// skills remain native/app-global, while roost-local definitions can now
    /// come from Android SAF where `std::fs::read_dir` cannot operate.
    pub fn refresh_with_roost_contents(
        &self,
        roost_manifests: Vec<(String, String)>,
        global_skills_dir: Option<&Path>,
    ) -> RefreshOutcome {
        let mut outcome = RefreshOutcome::default();
        let mut next = HashMap::new();
        if let Some(dir) = global_skills_dir {
            collect_dir(dir, SkillSource::Global, &mut next, &mut outcome);
        }
        for (rel, content) in roost_manifests {
            let path = PathBuf::from(&rel);
            let Some(folder) = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
            else {
                outcome
                    .failed
                    .push((path, "could not derive folder name".into()));
                continue;
            };
            match parse_skill_md(&content, folder, path.clone()) {
                Ok(manifest) => {
                    insert_entry(&mut next, manifest, SkillSource::Roost);
                    outcome.loaded += 1;
                }
                Err(e) => outcome.failed.push((path, e.to_string())),
            }
        }
        if let Ok(mut guard) = self.inner.write() {
            *guard = next;
        }
        outcome
    }

    /// Lightweight list view used by `skill_list`. Sorted by name for
    /// stable output.
    pub fn list(&self) -> Vec<SkillManifest> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        let mut out: Vec<_> = guard.values().map(|e| e.manifest.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Lookup by exact name. Returns `None` if the skill was removed
    /// since the last refresh.
    pub fn get(&self, name: &str) -> Option<SkillEntry> {
        self.inner.read().ok()?.get(name).cloned()
    }

    /// Snapshot of `<name, source>` tuples — useful for tests and for
    /// the future settings UI.
    pub fn entries(&self) -> Vec<(String, SkillSource)> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        let mut out: Vec<_> = guard.iter().map(|(k, v)| (k.clone(), v.source)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// Summary of a `refresh()` call. Helps tests assert what was loaded
/// and skipped; production code logs the failures.
#[derive(Debug, Default)]
pub struct RefreshOutcome {
    pub loaded: usize,
    pub failed: Vec<(PathBuf, String)>,
}

fn collect_dir(
    dir: &Path,
    source: SkillSource,
    out: &mut HashMap<String, SkillEntry>,
    outcome: &mut RefreshOutcome,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return, // missing dir is fine — user just hasn't added skills yet
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        match read_and_parse(&skill_md) {
            Ok(manifest) => {
                insert_entry(out, manifest, source);
                outcome.loaded += 1;
            }
            Err(SkillParseError::Io(msg)) => {
                outcome.failed.push((skill_md, msg));
            }
            Err(e) => {
                outcome.failed.push((skill_md, e.to_string()));
            }
        }
    }
}

fn insert_entry(
    out: &mut HashMap<String, SkillEntry>,
    manifest: SkillManifest,
    source: SkillSource,
) {
    let name = manifest.name.clone();
    let should_insert = match out.get(&name) {
        Some(existing) => match (existing.source, source) {
            (SkillSource::Global, SkillSource::Roost) => true,
            (SkillSource::Roost, SkillSource::Global) => false,
            _ => true,
        },
        None => true,
    };
    if should_insert {
        out.insert(name, SkillEntry { manifest, source });
    }
}

/// Resolve the roost-local skills dir from a roost root. Centralized
/// here so the watcher + ToolContext + registry share one definition.
pub fn roost_skills_dir(roost_root: &Path) -> PathBuf {
    roost_root.join(".corvux").join("skills")
}

/// Resolve the global skills dir from the user's home. Returns `None`
/// when home dir resolution fails (tests, restricted environments).
pub fn global_skills_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().join(".corvux").join("skills"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(root: &Path, name: &str, description: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let md =
            format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nbody\n");
        std::fs::write(dir.join("SKILL.md"), md).unwrap();
    }

    #[test]
    fn refresh_loads_roost_skills() {
        let roost = TempDir::new().unwrap();
        let skills_dir = roost.path().join(".corvux").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        write_skill(&skills_dir, "fit", "fitness summary");
        write_skill(&skills_dir, "tasks", "task extraction");

        let reg = SkillRegistry::new();
        let outcome = reg.refresh(Some(&skills_dir), None);
        assert_eq!(outcome.loaded, 2);
        assert!(outcome.failed.is_empty());

        let names: Vec<_> = reg.list().into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["fit".to_string(), "tasks".to_string()]);
    }

    #[test]
    fn refresh_loads_backend_supplied_roost_skills() {
        let reg = SkillRegistry::new();
        let outcome = reg.refresh_with_roost_contents(
            vec![(
                ".corvux/skills/mobile/SKILL.md".into(),
                "---\nname: mobile\ndescription: SAF skill\n---\nbody\n".into(),
            )],
            None,
        );
        assert_eq!(outcome.loaded, 1);
        assert_eq!(reg.get("mobile").unwrap().source, SkillSource::Roost);
    }

    #[test]
    fn refresh_skips_invalid_skills_without_failing() {
        let dir = TempDir::new().unwrap();
        // valid
        write_skill(dir.path(), "good", "ok");
        // invalid — missing description
        let bad = dir.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "---\nname: bad\n---\nbody\n").unwrap();
        // not a skill dir at all (plain file)
        std::fs::write(dir.path().join("loose.md"), "ignored").unwrap();

        let reg = SkillRegistry::new();
        let outcome = reg.refresh(Some(dir.path()), None);
        assert_eq!(outcome.loaded, 1);
        assert_eq!(outcome.failed.len(), 1);
        assert!(reg.get("good").is_some());
        assert!(reg.get("bad").is_none());
    }

    #[test]
    fn roost_skills_shadow_global_on_collision() {
        let roost = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        write_skill(global.path(), "fit", "GLOBAL description");
        write_skill(roost.path(), "fit", "ROOST description");

        let reg = SkillRegistry::new();
        reg.refresh(Some(roost.path()), Some(global.path()));

        let entry = reg.get("fit").unwrap();
        assert_eq!(entry.source, SkillSource::Roost);
        assert_eq!(entry.manifest.description, "ROOST description");
    }

    #[test]
    fn global_only_when_roost_absent() {
        let global = TempDir::new().unwrap();
        write_skill(global.path(), "fit", "global only");

        let reg = SkillRegistry::new();
        reg.refresh(None, Some(global.path()));

        let entry = reg.get("fit").unwrap();
        assert_eq!(entry.source, SkillSource::Global);
    }

    #[test]
    fn refresh_is_idempotent_and_reflects_deletions() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "fit", "ok");
        let reg = SkillRegistry::new();
        reg.refresh(Some(dir.path()), None);
        assert!(reg.get("fit").is_some());

        // delete + re-refresh
        std::fs::remove_dir_all(dir.path().join("fit")).unwrap();
        reg.refresh(Some(dir.path()), None);
        assert!(reg.get("fit").is_none());
        assert!(reg.list().is_empty());
    }

    #[test]
    fn missing_directories_return_empty_silently() {
        let reg = SkillRegistry::new();
        let outcome = reg.refresh(
            Some(Path::new("/nonexistent/roost/.corvux/skills")),
            Some(Path::new("/nonexistent/home/.corvux/skills")),
        );
        assert_eq!(outcome.loaded, 0);
        assert!(outcome.failed.is_empty());
        assert!(reg.list().is_empty());
    }

    #[test]
    fn list_is_sorted_by_name() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "zeta", "z");
        write_skill(dir.path(), "alpha", "a");
        write_skill(dir.path(), "mid", "m");
        let reg = SkillRegistry::new();
        reg.refresh(Some(dir.path()), None);
        let names: Vec<_> = reg.list().into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn rejects_skill_with_mismatched_folder_name() {
        let dir = TempDir::new().unwrap();
        let folder = dir.path().join("folder-a");
        std::fs::create_dir_all(&folder).unwrap();
        // frontmatter says name: folder-b → mismatch
        std::fs::write(
            folder.join("SKILL.md"),
            "---\nname: folder-b\ndescription: x\n---\nbody\n",
        )
        .unwrap();

        let reg = SkillRegistry::new();
        let outcome = reg.refresh(Some(dir.path()), None);
        assert_eq!(outcome.loaded, 0);
        assert_eq!(outcome.failed.len(), 1);
    }
}
