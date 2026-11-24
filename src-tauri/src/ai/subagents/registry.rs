//! In-memory `SubagentRegistry`. Loads:
//!  - compiled-in built-ins (always available — guarantees `general` works)
//!  - global `~/.corvux/agents/<name>/AGENT.md`
//!  - roost-local `<roost>/.corvux/agents/<name>/AGENT.md`
//!
//! Roost > Global > Builtin precedence on name collision.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use super::manifest::{parse_agent_md, read_and_parse, SubagentDef, SubagentParseError};
use crate::ai::skills::roost_scan::{scan_native, ScanOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentSource {
    Roost,
    Global,
    Builtin,
}

#[derive(Debug, Clone)]
pub struct SubagentEntry {
    pub def: SubagentDef,
    pub source: SubagentSource,
}

#[derive(Default)]
pub struct SubagentRegistry {
    inner: RwLock<HashMap<String, SubagentEntry>>,
}

#[derive(Debug, Default)]
pub struct RefreshOutcome {
    pub loaded: usize,
    pub failed: Vec<(PathBuf, String)>,
}

impl SubagentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build (or rebuild) the in-memory map. Loads built-ins first,
    /// then global, then roost — later sources OVERRIDE earlier on name
    /// collision, which gives us roost > global > builtin.
    ///
    /// AT-5: the roost directory is read through the hardened roost filesystem
    /// (`skills::roost_scan`), never with raw `std::fs`, so a link or junction
    /// inside `<roost>/.corvux/agents` that points outside the roost is refused
    /// instead of followed. Pass the directory in its roost shape
    /// (`<roost>/.corvux/agents`, as `roost_agents_dir` builds it) so a link at
    /// `.corvux` or `agents` itself is caught too.
    pub fn refresh(&self, roost_dir: Option<&Path>, global_dir: Option<&Path>) -> RefreshOutcome {
        let scan = roost_dir
            .map(|dir| scan_native(dir, "agents", "AGENT.md"))
            .unwrap_or_default();
        self.rebuild(scan, global_dir)
    }

    pub fn refresh_with_roost_contents(
        &self,
        roost_manifests: Vec<(String, String)>,
        global_dir: Option<&Path>,
    ) -> RefreshOutcome {
        self.rebuild(
            ScanOutcome {
                manifests: roost_manifests,
                failed: Vec::new(),
            },
            global_dir,
        )
    }

    /// The one place the map is rebuilt: built-ins, then the global folder (raw
    /// read, see `collect_dir`), then the roost manifests (highest precedence).
    fn rebuild(&self, roost: ScanOutcome, global_dir: Option<&Path>) -> RefreshOutcome {
        let mut outcome = RefreshOutcome::default();
        let mut next = HashMap::new();
        for def in super::builtin::builtins() {
            outcome.loaded += 1;
            next.insert(
                def.name.clone(),
                SubagentEntry {
                    def,
                    source: SubagentSource::Builtin,
                },
            );
        }
        if let Some(dir) = global_dir {
            collect_dir(dir, SubagentSource::Global, &mut next, &mut outcome);
        }
        outcome.failed.extend(roost.failed);
        for (rel, content) in roost.manifests {
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
            match parse_agent_md(&content, folder, path.clone()) {
                Ok(def) => {
                    outcome.loaded += 1;
                    next.insert(
                        def.name.clone(),
                        SubagentEntry {
                            def,
                            source: SubagentSource::Roost,
                        },
                    );
                }
                Err(e) => outcome.failed.push((path, e.to_string())),
            }
        }
        if let Ok(mut guard) = self.inner.write() {
            *guard = next;
        }
        outcome
    }

    /// All subagent defs, sorted by name. Used by the `task` tool's
    /// dynamic description and by the future settings UI.
    pub fn list(&self) -> Vec<SubagentDef> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        let mut out: Vec<_> = guard.values().map(|e| e.def.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn get(&self, name: &str) -> Option<SubagentEntry> {
        self.inner.read().ok()?.get(name).cloned()
    }

    /// `<name, source>` snapshot for tests + diagnostics.
    pub fn entries(&self) -> Vec<(String, SubagentSource)> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        let mut out: Vec<_> = guard.iter().map(|(k, v)| (k.clone(), v.source)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Names sorted alphabetically — handy for the `task` tool's
    /// `subagent_type` enum schema.
    pub fn names(&self) -> Vec<String> {
        let Ok(guard) = self.inner.read() else {
            return Vec::new();
        };
        let mut out: Vec<_> = guard.keys().cloned().collect();
        out.sort();
        out
    }
}

/// Scan the user's GLOBAL agents folder (`~/.corvux/agents`) with plain
/// `std::fs`. Deliberately not the hardened path: the folder belongs to the user,
/// not to a roost, so there is no boundary to stay inside, and people keep it
/// under a dotfile manager (symlinks must keep working). Never call this for a
/// roost directory — those go through `skills::roost_scan` (AT-5).
fn collect_dir(
    dir: &Path,
    source: SubagentSource,
    out: &mut HashMap<String, SubagentEntry>,
    outcome: &mut RefreshOutcome,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let agent_md = path.join("AGENT.md");
        if !agent_md.is_file() {
            continue;
        }
        match read_and_parse(&agent_md) {
            Ok(def) => {
                let name = def.name.clone();
                out.insert(name, SubagentEntry { def, source });
                outcome.loaded += 1;
            }
            Err(SubagentParseError::Io(msg)) => outcome.failed.push((agent_md, msg)),
            Err(e) => outcome.failed.push((agent_md, e.to_string())),
        }
    }
}

pub fn roost_agents_dir(roost_root: &Path) -> PathBuf {
    roost_root.join(".corvux").join("agents")
}

pub fn global_agents_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().join(".corvux").join("agents"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_agent(root: &Path, name: &str, description: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("AGENT.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody\n"),
        )
        .unwrap();
    }

    #[test]
    fn refresh_loads_builtin_general_by_default() {
        let reg = SubagentRegistry::new();
        let outcome = reg.refresh(None, None);
        assert!(outcome.loaded >= 1);
        let general = reg.get("general").unwrap();
        assert_eq!(general.source, SubagentSource::Builtin);
    }

    #[test]
    fn refresh_loads_backend_supplied_roost_subagents() {
        let reg = SubagentRegistry::new();
        let outcome = reg.refresh_with_roost_contents(
            vec![(
                ".corvux/agents/mobile/AGENT.md".into(),
                "---\nname: mobile\ndescription: SAF agent\n---\nbody\n".into(),
            )],
            None,
        );
        assert_eq!(outcome.loaded, 2); // builtin general + roost mobile
        assert_eq!(reg.get("mobile").unwrap().source, SubagentSource::Roost);
    }

    #[test]
    fn roost_overrides_global_overrides_builtin() {
        let roost = TempDir::new().unwrap();
        let global = TempDir::new().unwrap();
        write_agent(global.path(), "general", "GLOBAL general");
        write_agent(roost.path(), "general", "ROOST general");

        let reg = SubagentRegistry::new();
        reg.refresh(Some(roost.path()), Some(global.path()));

        let entry = reg.get("general").unwrap();
        assert_eq!(entry.source, SubagentSource::Roost);
        assert_eq!(entry.def.description, "ROOST general");
    }

    /// AT-5 twin of the skills test: an agent folder that is a link out of the
    /// roost is refused (its body would otherwise become a subagent system
    /// prompt). Runs on Windows (junction) as well as Unix (symlink).
    #[test]
    fn an_agent_folder_linked_out_of_the_roost_is_not_loaded() {
        use crate::ai::skills::roost_scan::test_support::make_dir_link;
        let roost = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let agents = roost_agents_dir(roost.path());
        write_agent(&agents, "good", "an honest agent");
        write_agent(outside.path(), "evil", "PRIVATE outside the roost");
        make_dir_link(&agents.join("evil"), &outside.path().join("evil"));

        let reg = SubagentRegistry::new();
        let outcome = reg.refresh(Some(&agents), None);
        assert!(reg.get("good").is_some());
        assert!(
            reg.get("evil").is_none(),
            "a link out of the roost was followed"
        );
        assert_eq!(outcome.failed.len(), 1, "{:?}", outcome.failed);
        assert!(outcome.failed[0].0.ends_with("evil"));
        // Built-ins are unaffected by a hostile roost.
        assert!(reg.get("general").is_some());
    }

    #[test]
    fn a_dot_corvux_link_out_of_the_roost_loads_no_agents() {
        use crate::ai::skills::roost_scan::test_support::make_dir_link;
        let roost = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        write_agent(&outside.path().join("agents"), "evil", "PRIVATE");
        make_dir_link(&roost.path().join(".corvux"), outside.path());

        let reg = SubagentRegistry::new();
        reg.refresh(Some(&roost_agents_dir(roost.path())), None);
        assert!(reg.get("evil").is_none());
        assert!(reg.get("general").is_some());
    }

    #[test]
    fn global_overrides_builtin_when_no_roost_entry() {
        let global = TempDir::new().unwrap();
        write_agent(global.path(), "general", "GLOBAL general");
        let reg = SubagentRegistry::new();
        reg.refresh(None, Some(global.path()));

        let entry = reg.get("general").unwrap();
        assert_eq!(entry.source, SubagentSource::Global);
        assert_eq!(entry.def.description, "GLOBAL general");
    }

    #[test]
    fn refresh_skips_malformed_without_failing_the_world() {
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "good", "ok");
        let bad = dir.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("AGENT.md"), "---\nname: bad\n---\nbody\n").unwrap();

        let reg = SubagentRegistry::new();
        let outcome = reg.refresh(Some(dir.path()), None);
        // 1 builtin + 1 valid user agent
        assert_eq!(outcome.loaded, 2);
        assert_eq!(outcome.failed.len(), 1);
        assert!(reg.get("good").is_some());
        assert!(reg.get("bad").is_none());
        assert!(reg.get("general").is_some());
    }

    #[test]
    fn list_is_sorted() {
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "zeta", "z");
        write_agent(dir.path(), "alpha", "a");
        let reg = SubagentRegistry::new();
        reg.refresh(Some(dir.path()), None);
        let names: Vec<_> = reg.list().into_iter().map(|d| d.name).collect();
        // alpha, general, zeta
        assert_eq!(names, vec!["alpha", "general", "zeta"]);
    }

    #[test]
    fn refresh_clears_removed_user_agents_but_keeps_builtin() {
        let dir = TempDir::new().unwrap();
        write_agent(dir.path(), "x", "x");
        let reg = SubagentRegistry::new();
        reg.refresh(Some(dir.path()), None);
        assert!(reg.get("x").is_some());

        std::fs::remove_dir_all(dir.path().join("x")).unwrap();
        reg.refresh(Some(dir.path()), None);
        assert!(reg.get("x").is_none());
        // builtin survives
        assert!(reg.get("general").is_some());
    }
}
