//! Agent write gate — the ONE choke point every agent write passes
//! (review findings AT-1 and AT-4, 2026-11-12).
//!
//! Before this module, `agent_blocked_paths` was checked once, in the agent
//! loop, on the RAW string the model sent (`path` / `file_path` /
//! `note_path`), before the tool worked out what it would really write. Every
//! difference between "what the model typed" and "what the tool writes"
//! was a bypass: a decoy `path` next to the real `note_path`, `roost_write`'s
//! `folder` + `title` (no path key at all), `"daily"` / an empty `note_path`
//! that resolves to the daily-note folder, and the resolver's own spelling
//! fixes (missing `.md`, `[[wiki]]` markers, bare titles). Separately, the
//! reserved-path guard (`.corvux/`, dotfiles) only existed in `roost_write`
//! and `edit_note`, so the six task writers could append a line to a stored
//! `SKILL.md` / `AGENT.md` or corrupt `.corvux/*.json`.
//!
//! Now every agent write goes through [`create_note`], [`mutate_note`] or
//! [`mutate_internal`]. They check the FINAL, cleaned, roost-relative path
//! (the same `roostfs::validate_rel` form the backend writes) against:
//! * the reserved-path rule (notes only; the two app-owned agent files in
//!   [`AGENT_INTERNAL_FILES`] are the only `.corvux/` targets, and only via
//!   [`mutate_internal`]), and
//! * the user's `agent_blocked_paths` (every target, Write and Destructive
//!   alike — only Write/Destructive tools can reach these functions).
//!
//! The blocklist reaches this layer through `ToolContext::write_gate`, which
//! only the agent loop's `dispatch_one` arms (from the loop's autonomy
//! snapshot, taken when the chat turn started). An unarmed context FAILS CLOSED, so a write tool executed by any
//! other path cannot skip the check. The early raw-string check in
//! `dispatch_one` stays, but only as a friendly fast path that refuses before
//! an approval card is shown; this module is the authoritative one.
//!
//! Guards: `write_gate::tests` (one test per known bypass + a registry-level
//! test over every Write/Destructive tool) and
//! `no_raw_fs_writes_test::agent_writes_only_through_write_gate` in
//! `ai/tools/mod.rs` (no file in `ai/tools/` other than this one may call the
//! `commands::files` write helpers or a `RoostFs` mutator).

use std::sync::Arc;

use serde_json::Value;
use tauri::Runtime;

use super::registry::{ToolContext, ToolError};

/// App-owned files inside `.corvux/` that agent tools may write — and only
/// through [`mutate_internal`]. `remember` / `forget` keep long-term memory
/// in `memory.md`; `scratchpad_write` keeps working state in
/// `scratchpad.md`. Everything else under `.corvux/` (settings, sync
/// identity, skills, subagents) stays off-limits to the agent.
pub(crate) const AGENT_INTERNAL_FILES: &[&str] = &[".corvux/memory.md", ".corvux/scratchpad.md"];

/// Argument names that name a write target. A Write/Destructive call may
/// carry at most ONE of them, so the approval card, the early check and the
/// tool can never disagree about which one is the real target.
pub(crate) const PATH_LIKE_KEYS: &[&str] = &[
    "path",
    "file_path",
    "note_path",
    "folder",
    "paths",
    "target",
    "target_path",
    "dest",
    "destination",
];

/// The blocklist snapshot an agent write is checked against. Built by the
/// agent loop for every tool call from the loop's `AgentAutonomy`.
#[derive(Clone, Debug, Default)]
pub struct WriteGate {
    blocked_paths: Arc<Vec<String>>,
}

impl WriteGate {
    pub fn new(blocked_paths: &[String]) -> Self {
        Self {
            blocked_paths: Arc::new(blocked_paths.to_vec()),
        }
    }

    pub fn blocked_paths(&self) -> &[String] {
        &self.blocked_paths
    }
}

/// True if a roost-relative path targets one of Corvux's reserved hidden
/// areas (`.corvux/`, `.attachments/`, `.corvux-trash/`) or any dotfile /
/// dot-directory. Agent note writes must not mutate Corvux's own internal
/// state (sync identity, settings, skills, trash) — this mirrors the MCP
/// server's refusal (`corvux-mcp/src/roost.rs`). Reads are intentionally
/// unaffected. `.`/`..` are rejected by `roostfs::validate_rel`; excluded
/// here so the predicate reads as "a dot-segment that isn't a traversal
/// token". Backslashes count as separators (Windows spelling).
pub(crate) fn is_reserved_path(rel: &str) -> bool {
    rel.replace('\\', "/")
        .split('/')
        .any(|seg| seg.starts_with('.') && seg != "." && seg != "..")
}

/// The message the agent (and the user's tool card) sees when a target is on
/// the blocklist. Shared with the early check in the agent loop so both
/// refusals read the same.
pub(crate) fn blocked_message(rel: &str) -> String {
    format!(
        "Path `{rel}` is on your blocked list — the agent can't write here. \
         Adjust the request to use a different path, or remove the pattern \
         in Settings → AI → Agent Autonomy if you intended to allow this."
    )
}

/// Clean `rel` exactly the way the roost backend will before it writes, so
/// the check sees the same path the write lands on.
fn clean_target(rel: &str) -> Result<String, ToolError> {
    let cleaned = crate::roostfs::validate_rel(rel)
        .map_err(|e| ToolError::fatal(format!("refusing to write: {e}")))?;
    if cleaned.is_empty() {
        return Err(ToolError::fatal(
            "refusing to write: the target resolves to the roost root",
        ));
    }
    Ok(cleaned)
}

fn check_blocklist(cleaned: &str, blocked: &[String]) -> Result<(), ToolError> {
    if super::glob::path_is_blocked(cleaned, blocked) {
        return Err(ToolError::retryable(blocked_message(cleaned)));
    }
    Ok(())
}

/// Pure core of the note-write check. Returns the cleaned roost-relative
/// path the caller must write to.
pub(crate) fn check_note_target(rel: &str, blocked: &[String]) -> Result<String, ToolError> {
    let cleaned = clean_target(rel)?;
    if is_reserved_path(&cleaned) {
        return Err(ToolError::fatal(format!(
            "refusing to write to a reserved path '{cleaned}'. Corvux's own folders (.corvux, .attachments) and dotfiles are off-limits to the agent."
        )));
    }
    check_blocklist(&cleaned, blocked)?;
    Ok(cleaned)
}

/// Pure core of the app-owned-file check: only the exact files in
/// [`AGENT_INTERNAL_FILES`] pass, and the user's blocklist still applies.
pub(crate) fn check_internal_target(rel: &str, blocked: &[String]) -> Result<String, ToolError> {
    let cleaned = clean_target(rel)?;
    if !AGENT_INTERNAL_FILES.contains(&cleaned.as_str()) {
        return Err(ToolError::fatal(format!(
            "refusing to write '{cleaned}': it is not an agent-owned file"
        )));
    }
    check_blocklist(&cleaned, blocked)?;
    Ok(cleaned)
}

/// Shape rules for a Write/Destructive call, checked by the agent loop after
/// schema validation and BEFORE the early blocklist check and the approval
/// card:
/// * no argument the tool's schema does not declare (the effect of JSON
///   Schema `additionalProperties: false`, enforced here instead of on the
///   wire because Gemini's `parameters` schema subset does not accept that
///   keyword), so a decoy key can't sit next to the real target;
/// * at most one path-like argument ([`PATH_LIKE_KEYS`]).
pub(crate) fn check_write_call_shape(args: &Value, schema: &Value) -> Result<(), String> {
    let Some(obj) = args.as_object() else {
        // `validate_args` already refused a non-object.
        return Ok(());
    };
    let declared: Vec<&str> = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|p| p.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let mut unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !declared.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        let mut allowed = declared.clone();
        allowed.sort_unstable();
        return Err(format!(
            "unexpected field(s) {} — this tool accepts only: {}. Extra fields are refused on write tools so the note you name is the note that gets written.",
            unknown
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", "),
            allowed
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut path_keys: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| PATH_LIKE_KEYS.contains(k))
        .collect();
    if path_keys.len() > 1 {
        path_keys.sort_unstable();
        return Err(format!(
            "this call names more than one target ({}) — pass exactly one.",
            path_keys
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

fn armed<R: Runtime + 'static>(ctx: &ToolContext<R>) -> Result<&WriteGate, ToolError> {
    ctx.write_gate.as_ref().ok_or_else(|| {
        ToolError::fatal(
            "agent write refused: the write gate is not armed (agent writes must be dispatched by the agent loop)",
        )
    })
}

/// Create a note iff it is absent. The ONLY create path for agent tools.
pub(crate) async fn create_note<R: Runtime + 'static>(
    ctx: &ToolContext<R>,
    rel: &str,
    content: &str,
) -> Result<bool, ToolError> {
    let cleaned = check_note_target(rel, armed(ctx)?.blocked_paths())?;
    crate::commands::files::create_roost_note_synced(&ctx.app, &cleaned, content)
        .await
        .map_err(ToolError::retryable)
}

/// Read-modify-write an existing note. The ONLY edit path for agent tools.
pub(crate) async fn mutate_note<R, T, F>(
    ctx: &ToolContext<R>,
    rel: &str,
    mutate: F,
) -> Result<(String, String, T), ToolError>
where
    R: Runtime + 'static,
    F: FnOnce(&str) -> Result<(String, T), String>,
{
    let cleaned = check_note_target(rel, armed(ctx)?.blocked_paths())?;
    crate::commands::files::mutate_roost_note_synced(&ctx.app, &cleaned, mutate)
        .await
        .map_err(ToolError::retryable)
}

/// Read-modify-write one of [`AGENT_INTERNAL_FILES`] (created lazily).
/// `map_err` keeps each caller's retry semantics for I/O failures.
pub(crate) async fn mutate_internal<R, T, F>(
    ctx: &ToolContext<R>,
    rel: &str,
    map_err: fn(String) -> ToolError,
    mutate: F,
) -> Result<(String, String, T), ToolError>
where
    R: Runtime + 'static,
    F: FnOnce(&str) -> Result<(String, T), String>,
{
    let cleaned = check_internal_target(rel, armed(ctx)?.blocked_paths())?;
    crate::commands::files::mutate_roost_text_synced(&ctx.app, &cleaned, true, mutate)
        .await
        .map_err(map_err)
}

#[cfg(test)]
mod tests {
    //! One test per AT-1 bypass, AT-4, and a registry-level guard over every
    //! Write/Destructive tool. Tool `execute()` needs a real Tauri app
    //! (`MockRuntime` is not built in this crate), so these drive the same
    //! pure pieces `execute()` calls — the resolver core over a real
    //! `NativeFs` roost, `roost_write::planned_rel`, and the gate checks —
    //! while the source ratchet in `ai/tools/mod.rs` proves no tool can write
    //! without going through this module.
    use super::*;
    use crate::ai::tools::note_ref::resolve_rel_with;
    use crate::ai::tools::{AgentProfile, SafetyTier, Tool};
    use crate::roostfs::native::NativeFs;
    use serde_json::json;

    fn pats(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn no_titles(_: &str) -> Option<String> {
        None
    }

    fn assert_blocked(res: Result<String, ToolError>, what: &str) {
        let err = res.expect_err(what);
        assert!(
            err.message.contains("blocked list"),
            "{what}: expected a blocked-list refusal, got: {}",
            err.message
        );
    }

    /// A tempdir roost with the notes the bypass tests aim at.
    fn roost() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (rel, body) in [
            ("Journal/x.md", "- [ ] secret task 📅 2026-01-01\n"),
            ("Finance/Q1.md", "numbers\n"),
            ("Scratch/ok.md", "ok\n"),
        ] {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, body).expect("write fixture");
        }
        dir
    }

    async fn resolve(
        dir: &tempfile::TempDir,
        input: &str,
        allow_create_daily: bool,
        title_lookup: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    ) -> Result<String, ToolError> {
        let root = dir.path().to_string_lossy().to_string();
        let backend = NativeFs::new(dir.path());
        resolve_rel_with(
            input,
            &root,
            None,
            &backend,
            allow_create_daily,
            false,
            title_lookup,
        )
        .await
    }

    // ── (1) decoy key ──────────────────────────────────────────────────

    #[test]
    fn decoy_path_key_next_to_note_path_is_refused_for_every_note_writer() {
        use crate::ai::tools::roost_edit::RoostEditTool;
        use crate::ai::tools::tasks_crud::{
            ArchiveTaskTool, CompleteTaskTool, CreateTaskTool, DeleteTaskTool, RescheduleTaskTool,
            UpdateTaskTool,
        };
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(RoostEditTool),
            Box::new(CreateTaskTool),
            Box::new(UpdateTaskTool),
            Box::new(CompleteTaskTool),
            Box::new(RescheduleTaskTool),
            Box::new(ArchiveTaskTool),
            Box::new(DeleteTaskTool),
        ];
        for t in tools {
            let args = json!({
                "path": "Scratch/ok.md",
                "note_path": "Journal/x.md",
                "text": "x", "text_anchor": "secret", "new_text": "y", "new_content": "z"
            });
            let err = check_write_call_shape(&args, &t.input_schema())
                .expect_err("decoy key must be refused");
            assert!(err.contains("`path`"), "{}: {err}", t.name());
        }
    }

    #[test]
    fn two_declared_path_keys_are_refused() {
        let schema = json!({"type":"object","properties":{"path":{},"note_path":{}}});
        let err = check_write_call_shape(&json!({"path":"a","note_path":"b"}), &schema)
            .expect_err("two targets");
        assert!(err.contains("more than one target"), "{err}");
        assert!(check_write_call_shape(&json!({"note_path":"b"}), &schema).is_ok());
    }

    // ── (2) roost_write folder + title ─────────────────────────────────

    #[test]
    fn roost_write_folder_plus_title_target_is_checked() {
        let args = json!({"title": "Plan", "body": "b", "folder": "Journal"});
        let rel = crate::ai::tools::roost_write::planned_rel(&args).expect("planned");
        assert_eq!(rel, "Journal/Plan.md");
        assert_blocked(
            check_note_target(&rel, &pats(&["Journal/"])),
            "folder+title",
        );
        // Same through a backslash / leading-slash spelling of the folder.
        let args = json!({"title": "Plan", "body": "b", "folder": "/Journal\\sub"});
        let rel = crate::ai::tools::roost_write::planned_rel(&args).expect("planned");
        assert_blocked(
            check_note_target(&rel, &pats(&["Journal/"])),
            "backslash folder",
        );
    }

    // ── (3) daily default ──────────────────────────────────────────────

    #[tokio::test]
    async fn daily_shorthand_and_empty_note_path_resolve_into_the_blocked_daily_folder() {
        let dir = roost();
        std::fs::create_dir_all(dir.path().join(".corvux")).expect("mkdir");
        std::fs::write(
            dir.path().join(".corvux/roost-settings.json"),
            r#"{"tasks":{"daily_note_folder":"Journal"}}"#,
        )
        .expect("settings");
        // create_task with no note_path → "daily" (tasks_crud's own mapping).
        let input = crate::ai::tools::tasks_crud::effective_note_input("", false);
        assert_eq!(input, "daily");
        for spelling in [input, "daily", "DAILY"] {
            let rel = resolve(&dir, spelling, true, &no_titles)
                .await
                .expect("daily resolves");
            assert!(rel.starts_with("Journal/"), "got {rel}");
            assert_blocked(
                check_note_target(&rel, &pats(&["Journal/"])),
                "daily target",
            );
        }
    }

    // ── (4) spellings fixed up after the old raw check ─────────────────

    #[tokio::test]
    async fn extension_less_name_hits_a_pattern_ending_in_md() {
        let dir = roost();
        let blocked = pats(&["Finance/Q1.md"]);
        // The old raw check saw `Finance/Q1` and missed this pattern.
        assert!(!super::super::glob::path_is_blocked("Finance/Q1", &blocked));
        let rel = resolve(&dir, "Finance/Q1", false, &no_titles)
            .await
            .expect("resolves");
        assert_eq!(rel, "Finance/Q1.md");
        assert_blocked(check_note_target(&rel, &blocked), "extension-less");
    }

    #[tokio::test]
    async fn wiki_link_markers_are_resolved_before_the_check() {
        let dir = roost();
        let rel = resolve(&dir, "[[Finance/Q1]]", false, &no_titles)
            .await
            .expect("resolves");
        assert_blocked(
            check_note_target(&rel, &pats(&["Finance/Q1.md"])),
            "[[wiki]]",
        );
    }

    #[tokio::test]
    async fn bare_title_resolved_by_lookup_is_checked() {
        let dir = roost();
        let lookup = |t: &str| (t == "Q1").then(|| "Finance/Q1.md".to_string());
        let rel = resolve(&dir, "Q1", false, &lookup).await.expect("resolves");
        assert_eq!(rel, "Finance/Q1.md");
        assert_blocked(check_note_target(&rel, &pats(&["Finance/"])), "bare title");
    }

    #[test]
    fn gate_keeps_the_matcher_spelling_protections() {
        let blocked = pats(&["Journal/", "café/**"]);
        for rel in [
            "journal/x.md",
            "JOURNAL/X.MD",
            "/Journal/x.md",
            "Journal\\x.md",
            "Journal//x.md",
            "cafe\u{301}/secret.md",
        ] {
            assert_blocked(check_note_target(rel, &blocked), rel);
        }
        // Windows trailing-dot spelling: refused either as blocked or (on
        // Windows) by validate_rel as a hostile segment — never written.
        assert!(check_note_target("Journal./x.md", &blocked).is_err());
        assert!(check_note_target("Notes/ok.md", &blocked).is_ok());
    }

    // ── AT-4: reserved paths ────────────────────────────────────────────

    #[test]
    fn reserved_paths_are_refused_for_note_writes() {
        for rel in [
            ".corvux/skills/x/SKILL.md",
            ".corvux/agents/y/AGENT.md",
            ".corvux/roost-settings.json",
            ".corvux/memory.md",
            "Notes/.secret.md",
            ".attachments/img.png",
        ] {
            let err = check_note_target(rel, &[]).expect_err(rel);
            assert!(err.fatal && err.message.contains("reserved"), "{rel}");
        }
        assert!(check_note_target("../outside.md", &[]).is_err());
        assert!(check_note_target("", &[]).is_err());
    }

    #[test]
    fn internal_exemption_is_exactly_the_agent_owned_files() {
        for rel in AGENT_INTERNAL_FILES {
            assert!(check_internal_target(rel, &[]).is_ok(), "{rel}");
            // The user's blocklist still wins.
            assert_blocked(check_internal_target(rel, &pats(&[".corvux/"])), rel);
        }
        for rel in [
            ".corvux/skills/x/SKILL.md",
            ".corvux/roost-settings.json",
            "Notes/a.md",
        ] {
            assert!(check_internal_target(rel, &[]).is_err(), "{rel}");
        }
    }

    // ── registry level ──────────────────────────────────────────────────

    /// How each Write/Destructive tool picks its target. A NEW write tool
    /// fails `every_write_tool_target_is_checked_by_the_gate` until it is
    /// listed here — and it can only write through this module (see the
    /// ratchet in `ai/tools/mod.rs`), which checks the resolved target.
    enum Target {
        /// `note_path` → `note_ref::resolve_rel` → `mutate_note`/`create_note`.
        NotePath,
        /// `folder` + `title` → `roost_write::planned_rel` → `create_note`.
        FolderTitle,
        /// A fixed app-owned file → `mutate_internal`.
        Internal(&'static str),
    }

    const WRITE_TOOLS: &[(&str, Target)] = &[
        ("roost_write", Target::FolderTitle),
        ("create_note", Target::FolderTitle),
        ("edit_note", Target::NotePath),
        ("create_task", Target::NotePath),
        ("update_task", Target::NotePath),
        ("complete_task", Target::NotePath),
        ("reschedule_task", Target::NotePath),
        ("archive_task", Target::NotePath),
        ("delete_task", Target::NotePath),
        ("remember", Target::Internal(".corvux/memory.md")),
        ("forget", Target::Internal(".corvux/memory.md")),
        (
            "scratchpad_write",
            Target::Internal(".corvux/scratchpad.md"),
        ),
    ];

    #[tokio::test]
    async fn every_write_tool_target_is_checked_by_the_gate() {
        let dir = roost();
        let blocked = pats(&["Journal/", ".corvux/"]);
        let mut seen = Vec::new();
        for profile in AgentProfile::all() {
            let registry = profile.build().registry;
            for name in registry.names() {
                let Some(tool) = registry.get(name) else {
                    continue;
                };
                if !matches!(
                    tool.safety_tier(),
                    SafetyTier::Write | SafetyTier::Destructive
                ) || seen.contains(&name)
                {
                    continue;
                }
                seen.push(name);
                let Some((_, target)) = WRITE_TOOLS.iter().find(|(n, _)| *n == name) else {
                    panic!(
                        "write tool `{name}` is not in WRITE_TOOLS: route its write through \
                         ai/tools/write_gate.rs and list how it picks its target"
                    );
                };
                let schema = tool.input_schema();
                match target {
                    Target::NotePath => {
                        assert!(
                            schema["properties"].get("note_path").is_some(),
                            "{name}: expected a note_path argument"
                        );
                        let rel = resolve(&dir, "Journal/x", false, &no_titles)
                            .await
                            .expect("resolves");
                        assert_blocked(check_note_target(&rel, &blocked), name);
                        let decoy = json!({"path": "Scratch/ok.md", "note_path": "Journal/x"});
                        assert!(
                            check_write_call_shape(&decoy, &schema).is_err(),
                            "{name}: decoy key accepted"
                        );
                    }
                    Target::FolderTitle => {
                        let args = json!({"title": "t", "body": "b", "folder": "Journal"});
                        let rel =
                            crate::ai::tools::roost_write::planned_rel(&args).expect("planned");
                        assert_blocked(check_note_target(&rel, &blocked), name);
                    }
                    Target::Internal(rel) => {
                        assert_blocked(check_internal_target(rel, &blocked), name);
                    }
                }
            }
        }
        let mut listed: Vec<&str> = WRITE_TOOLS.iter().map(|(n, _)| *n).collect();
        listed.sort_unstable();
        seen.sort_unstable();
        assert_eq!(
            seen, listed,
            "WRITE_TOOLS must match the shipped write tools"
        );
    }
}
