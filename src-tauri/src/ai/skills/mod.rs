//! Skills — user-authored markdown instruction files the agent loads
//! on demand via `skill_list` / `skill_load`. See `manifest.rs` for
//! the SKILL.md format spec.
//!
//! ## Layout
//! - `<roost>/.corvux/skills/<name>/SKILL.md` (preferred, per-roost)
//! - `~/.corvux/skills/<name>/SKILL.md` (global, shared across roosts)
//!
//! Roost-local skills shadow global on name collision. Loading is
//! lazy: `skill_list` returns just `name + description`, the agent
//! decides whether to call `skill_load` for the full body.

mod manifest;
mod registry;
mod watcher;

#[allow(unused_imports)] // first consumer is the skill_list / skill_load tools
pub use manifest::{is_valid_skill_name, SkillManifest, SkillParseError};
#[allow(unused_imports)]
pub use registry::{
    global_skills_dir, roost_skills_dir, RefreshOutcome, SkillEntry, SkillRegistry, SkillSource,
};
#[allow(unused_imports)]
pub use watcher::{spawn_default_watcher, spawn_watcher, SkillWatcher};
