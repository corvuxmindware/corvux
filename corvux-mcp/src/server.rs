//! Minimal MCP server: JSON-RPC 2.0 dispatch + the Corvux roost tools.
//!
//! We implement just enough of the Model Context Protocol to be a useful
//! local tools server: `initialize`, `tools/list`, `tools/call`, `ping`.
//! Transport is newline-delimited JSON-RPC on stdio (see `main.rs`).
//!
//! The `initialize` instructions below are written to fully replace the
//! standalone SKILL.md / AGENTS.md / cursor-rule docs: a client that
//! connects learns what Corvux is and how to act purely from this server.

use std::cell::{Cell, Ref, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde_json::{json, Value};

use crate::atomic_write::WriteError;
use crate::conventions::{
    append_to_end, append_under_heading, append_under_tasks, complete_task_in, format_task_line,
    is_hhmm, is_iso_date, pick_daily_template, reschedule_task_in, today_local,
};
use crate::frontmatter;
use crate::index::RoostIndex;
use crate::roost::Roost;

/// DN-1: roost-relative path of the user's optional daily-note template,
/// matching the app's `daily_note.rs::DAILY_TEMPLATE_REL`.
const DAILY_TEMPLATE_REL: &str = ".corvux/templates/daily.md";

/// Prefix on the error `checked_read_modify_write`/`write_note` return when
/// a write is refused because the destination changed since it was last
/// read (or, for a create-only write, sprang into existence). Mirrors the
/// main app's own `SAVE_CONFLICT_PREFIX` convention (files.rs) so the shape
/// of "someone else wrote first" is recognizable across both binaries.
const MCP_CONFLICT_PREFIX: &str = "__mcp_conflict__";

const SERVER_NAME: &str = "corvux";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// MCP protocol revisions this server speaks, newest first. Used for proper
/// version negotiation: echo the client's version if we support it, else
/// answer with our latest (`SUPPORTED_PROTOCOLS[0]`) per the spec.
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Cap on how much note text a single read returns, to keep responses sane.
const READ_CAP: usize = 20_000;
/// Captures larger than this get their own note instead of being folded
/// inline into the daily note (which they'd otherwise bloat).
const CAPTURE_INLINE_MAX: usize = 4_000;
/// Cap on graph-expanded notes a `recall` returns. A hub seed can have
/// hundreds of neighbors; snippets for ten well-ranked ones inform the LLM,
/// a hundred bury it (same bounded-output stance as LINK_LIST_DEFAULT_LIMIT).
const RECALL_RELATED_MAX: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Full,
    ReadOnly,
    CaptureOnly,
}

impl AccessMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "full" => Ok(Self::Full),
            "read-only" | "readonly" => Ok(Self::ReadOnly),
            "capture-only" | "capture" => Ok(Self::CaptureOnly),
            _ => Err(format!(
                "Unknown MCP access mode {value:?}; expected full, read-only, or capture-only"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ReadOnly => "read-only",
            Self::CaptureOnly => "capture-only",
        }
    }

    fn allows(self, tool: &str) -> bool {
        const READ_TOOLS: &[&str] = &[
            "recall",
            "search_roost",
            "find_notes",
            "find_by_property",
            "read_note",
            "note_context",
            "list_open_tasks",
            "list_recent_notes",
            "roost_stats",
            "list_tags",
            "get_backlinks",
            "get_forward_links",
            "get_neighbors",
            "find_path",
            "top_hubs",
            "find_broken_links",
            "suggest_links",
        ];
        match self {
            Self::Full => true,
            Self::ReadOnly => READ_TOOLS.contains(&tool),
            Self::CaptureOnly => {
                matches!(tool, "capture_note" | "add_task" | "add_reminder")
            }
        }
    }
}

/// The briefing an MCP client receives on connect. This is the whole "what
/// is Corvux / what should I do" contract — it replaces the website skill
/// files. Keep it dense but complete.
const INSTRUCTIONS: &str = "\
Corvux is a local-first 'second brain'. The user's notes are plain Markdown \
files in a roost folder on THIS machine. This server edits that roost \
directly; the Corvux app watches the files and re-indexes them within \
seconds, so anything you write shows up in the app automatically.

ALWAYS prefer these tools over editing roost files by hand — they write the \
exact formats Corvux's indexer understands.

Actions: capture_note (save a chat/snippet/idea into memory), add_task (a \
to-do), add_reminder (a to-do that fires an OS notification at a date+time), \
reschedule_task (change a task's date/time), search_roost / find_notes / \
find_by_property / read_note (find and read), list_recent_notes ('what did I \
work on lately'), roost_stats (counts), append_to_note / write_note (add to \
or create notes), set_property / add_tags (edit a note's properties), \
list_open_tasks / complete_task (manage to-dos), list_tags (the existing tag \
vocabulary).

Memory: recall is the FIRST tool for 'what do we already know about X' — \
one call keyword-searches the roost AND walks the wiki-link graph one hop \
out, so it also surfaces linked notes that are worded differently and plain \
search would miss. Optionally scope it with `project` (matches a project: \
property, a tag, or a folder name). Follow up with read_note / note_context \
on the notes that matter.

Graph: the roost is a [[wiki-link]] graph, and you can traverse it — \
get_backlinks ('what links to X'), get_forward_links, get_neighbors (N hops \
out from a note), find_path ('how is X connected to Y'), top_hubs (the most \
central notes by PageRank), find_broken_links (dangling / ambiguous links to \
clean up), suggest_links (related-but-UNLINKED note pairs — propose the \
missing [[link]] to the user; every accepted link makes recall smarter). \
Reach for these when the user asks about how their notes relate, not just \
their text. After a writing session, a quick suggest_links pass on the notes \
you touched keeps the graph connected.

Properties matter — they're how Corvux organizes notes. A note's properties \
are its YAML frontmatter (date, type, tags, status, and any custom fields). \
read_note surfaces them as a 'Properties' block. To find notes, prefer \
find_by_property when the user names an attribute ('my draft notes' → \
status=draft; 'meeting notes' → type=meeting; 'tagged project' → tag=project) \
and fall back to search_roost for free text. Use set_property to change a \
field (e.g. status=done) and add_tags to tag a note; both keep the body \
intact. When capturing something with a clear category, pass tags/properties \
so it's filed with real metadata, not a bare note.

Be smart about WHERE things go, and ALWAYS tell the user where you put them \
and offer to move it if that's not what they wanted:
- Captures go into today's daily note by default. But if the note clearly \
belongs with a specific topic or project, first use search_roost / \
find_notes to locate that note, then append_to_note there instead.
- A task that names a project or topic should be filed in that note's task \
list — find the note and pass it as `note`. Otherwise it goes to today's \
daily note (a task with no date defaults to today — in Corvux a dateless \
checkbox isn't a task at all).
- Don't silently dump into a random note when a better home is obvious — \
look first.

Conventions (the tools apply these for you — don't hand-format):
- A task is `- [ ] text`, with a due date `📅 YYYY-MM-DD` and optional time \
`⏰ HH:MM`. A checkbox becomes a task only when it has an effective date \
(an inline `📅`, the note's own date, or — for a lone `⏰` — today).
- A 'reminder' in Corvux is simply a task that has BOTH a date and a time — \
that is what triggers the notification. There is NO separate reminder \
syntax. For 'remind me at <time>' use add_reminder.
- Keep inline #tags and [[wikilinks]] in text — Corvux turns them into \
graph links.
- Before tagging, call list_tags and REUSE an existing tag — inventing a \
near-duplicate (#ml vs #machine-learning, #todo vs #to-do) fragments the \
graph. Only coin a new tag when nothing existing fits. The tools snap \
case-variants (#ML→#ml) for you, but they can't guess synonyms — that's why \
you check first.
- [[wikilinks]] only connect when the target note EXISTS. Before linking, \
use search_roost / find_notes to confirm the exact title; a link to a \
non-existent note silently creates no graph edge.
- Resolve relative dates yourself ('tomorrow', 'Friday', 'next week') to an \
absolute YYYY-MM-DD using the user's local date before calling. Times are \
24-hour. If a date or time is ambiguous, ask first.

Don't: invent tasks or notes from the user's prose unless they ask you to \
capture them; don't reschedule or complete existing tasks unless asked; \
don't write into Corvux's own folders (.corvux, .attachments, trash) — the \
tools refuse anyway.";

/// One tool's reply: human-readable `text` (the `content` block) plus an
/// optional `structured` object surfaced in `structuredContent` so MCP
/// clients/LLMs get machine-parseable results instead of re-parsing prose.
struct ToolReply {
    text: String,
    structured: Option<Value>,
}

impl ToolReply {
    fn text(t: impl Into<String>) -> Self {
        Self {
            text: t.into(),
            structured: None,
        }
    }
    fn with(t: impl Into<String>, structured: Value) -> Self {
        Self {
            text: t.into(),
            structured: Some(structured),
        }
    }
}

/// Negotiate the protocol version: echo the client's if we support it,
/// otherwise answer with our latest. A missing/unparseable version also
/// gets our latest.
fn negotiate_protocol(requested: Option<&str>) -> &'static str {
    for &p in SUPPORTED_PROTOCOLS {
        if Some(p) == requested {
            return p;
        }
    }
    SUPPORTED_PROTOCOLS[0]
}

pub struct Server {
    roost: Roost,
    /// Cross-call content + wiki-link cache. `RefCell` because the stdio
    /// loop is single-threaded and handlers take `&self`.
    index: RefCell<RoostIndex>,
    /// Set once `initialize` has been handled. `tools/list`/`tools/call`
    /// before that point are rejected (MCP lifecycle — see `dispatch`).
    initialized: Cell<bool>,
    access_mode: AccessMode,
}

impl Server {
    #[cfg(test)]
    pub fn new(roost: Roost) -> Self {
        Self::with_access_mode(roost, AccessMode::Full)
    }

    pub fn with_access_mode(roost: Roost, access_mode: AccessMode) -> Self {
        Self {
            roost,
            index: RefCell::new(RoostIndex::new()),
            initialized: Cell::new(false),
            access_mode,
        }
    }

    /// Sync the cache to disk, then hand back a shared borrow for querying.
    /// The `borrow_mut` for `refresh` is a statement-scoped temporary, so the
    /// shared `borrow` that follows can't conflict.
    fn index_refreshed(&self) -> Ref<'_, RoostIndex> {
        self.index.borrow_mut().refresh(&self.roost);
        self.index.borrow()
    }

    /// Snap caller-supplied tags onto the roost's existing vocabulary: a tag
    /// that matches an existing one case-insensitively is rewritten to the
    /// existing spelling (so `#ML` doesn't fork from an established `#ml`).
    /// Unknown tags pass through cleaned (`#` and quotes stripped) but
    /// otherwise unchanged — we never silently drop a genuinely new tag, we
    /// only prevent casing/duplication drift. Order and duplicates are left
    /// for the downstream writer to dedup.
    fn canonicalize_tags(&self, tags: &[String]) -> Vec<String> {
        if tags.is_empty() {
            return Vec::new();
        }
        // lowercase -> canonical spelling; tag_counts is count-desc, so the
        // most-used spelling of a case-variant wins.
        let canon: HashMap<String, String> = {
            let idx = self.index_refreshed();
            let mut m = HashMap::new();
            for (t, _) in idx.tag_counts() {
                m.entry(t.to_lowercase()).or_insert(t);
            }
            m
        };
        tags.iter()
            .map(|t| {
                let clean = t
                    .trim()
                    .trim_matches(['"', '\''])
                    .trim_start_matches('#')
                    .trim();
                canon
                    .get(&clean.to_lowercase())
                    .cloned()
                    .unwrap_or_else(|| clean.to_string())
            })
            .filter(|t| !t.is_empty())
            .collect()
    }

    /// A trailing one-line warning if `content`'s `[[links]]` include dangling
    /// or ambiguous targets, else "". `source_dir` is the writing note's folder
    /// so folder-proximity resolution applies (a link to a same-named sibling
    /// in the same folder isn't flagged). Appended to write-tool replies so the
    /// caller learns about broken links at the moment it creates them, rather
    /// than them silently producing no edge / the wrong edge. Non-blocking —
    /// forward-references to not-yet-created notes are legitimate.
    fn link_warning_suffix(&self, content: &str, source_dir: &str) -> String {
        let (dangling, ambiguous) = self
            .index_refreshed()
            .check_content_links(content, source_dir);
        if dangling.is_empty() && ambiguous.is_empty() {
            return String::new();
        }
        let mut parts = Vec::new();
        if !dangling.is_empty() {
            parts.push(format!(
                "{} link(s) to non-existent notes: {}",
                dangling.len(),
                dangling
                    .iter()
                    .map(|t| format!("[[{t}]]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !ambiguous.is_empty() {
            let listed: Vec<String> = ambiguous
                .iter()
                .map(|(t, c)| format!("[[{t}]] (could be {})", c.join(" / ")))
                .collect();
            parts.push(format!(
                "{} ambiguous link(s): {}",
                ambiguous.len(),
                listed.join("; ")
            ));
        }
        format!(
            "\n⚠ Link check: {}. Fix a typo, create the target note, or use a full path [[Folder/Note]] to disambiguate — a dangling link makes no graph connection until its target exists.",
            parts.join("; ")
        )
    }

    /// Handle one parsed JSON-RPC message. Returns `Some(response)` for
    /// requests (those with an `id`) and `None` for notifications.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let is_request = id.is_some();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let outcome = self.dispatch(msg, method, &params);

        if !is_request {
            return None; // notifications never get a reply (JSON-RPC: they
                         // never get one either way, valid or rejected)
        }
        // Per JSON-RPC 2.0 §5: when the id itself couldn't be reliably used
        // (missing, or a shape other than string/number/null), the reply's
        // id MUST be null — echoing a malformed id back is itself invalid.
        let id = match id {
            Some(v @ (Value::String(_) | Value::Number(_) | Value::Null)) => v,
            _ => Value::Null,
        };
        Some(match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
        })
    }

    /// Validate the envelope, enforce the `initialize`-first lifecycle, then
    /// route to the method handler. Split out of `handle` so the validation
    /// steps (each an independent, testable gate) read top-to-bottom.
    fn dispatch(&self, msg: &Value, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match msg.get("jsonrpc") {
            Some(Value::String(v)) if v == "2.0" => {}
            _ => {
                return Err((
                    -32600,
                    "Invalid Request: jsonrpc must be \"2.0\"".to_string(),
                ))
            }
        }
        if let Some(id) = msg.get("id") {
            if !matches!(id, Value::String(_) | Value::Number(_) | Value::Null) {
                return Err((
                    -32600,
                    "Invalid Request: id must be a string, number, or null".to_string(),
                ));
            }
        }
        if method != "initialize" && !self.initialized.get() {
            return Err((
                -32002,
                format!("Server not initialized — call `initialize` first (got `{method}`)"),
            ));
        }

        match method {
            "initialize" => {
                self.initialized.set(true);
                Ok(self.initialize(params))
            }
            "tools/list" => Ok(self.tools_list()),
            "tools/call" => self.tools_call(params),
            "ping" => Ok(json!({})),
            _ => Err((-32601, format!("Method not found: {method}"))),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let protocol = negotiate_protocol(params.get("protocolVersion").and_then(Value::as_str));
        json!({
            "protocolVersion": protocol,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "title": "Corvux", "version": SERVER_VERSION },
            "instructions": INSTRUCTIONS
        })
    }

    fn tools_list(&self) -> Value {
        let tools = tool_specs()
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|tool| {
                tool.get("name")
                    .and_then(Value::as_str)
                    .map(|name| self.access_mode.allows(name))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        json!({ "tools": tools })
    }

    fn tools_call(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call missing `name`".to_string()))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !self.access_mode.allows(name) {
            return Ok(json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Error: tool `{name}` is disabled by the {} MCP access mode",
                        self.access_mode.as_str()
                    )
                }],
                "isError": true
            }));
        }

        // Text-only tools return `Result<String, _>` and get wrapped with
        // `ToolReply::text`; structured tools return `Result<ToolReply, _>`
        // directly so they can attach `structuredContent`.
        let result: Result<ToolReply, String> = match name {
            "capture_note" => self.capture_note(&args).map(ToolReply::text),
            "write_note" => self.write_note(&args).map(ToolReply::text),
            "append_to_note" => self.append_to_note(&args).map(ToolReply::text),
            "add_task" => self.add_task(&args).map(ToolReply::text),
            "add_reminder" => self.add_reminder(&args).map(ToolReply::text),
            "reschedule_task" => self.reschedule_task(&args).map(ToolReply::text),
            "recall" => self.recall(&args),
            "search_roost" => self.search_roost(&args),
            "find_notes" => self.find_notes(&args),
            "find_by_property" => self.find_by_property(&args),
            "read_note" => self.read_note(&args).map(ToolReply::text),
            "note_context" => self.note_context(&args),
            "list_open_tasks" => self.list_open_tasks(&args),
            "list_recent_notes" => self.list_recent_notes(&args),
            "roost_stats" => self.roost_stats(&args),
            "complete_task" => self.complete_task(&args).map(ToolReply::text),
            "set_property" => self.set_property(&args).map(ToolReply::text),
            "add_tags" => self.add_tags(&args).map(ToolReply::text),
            "list_tags" => self.list_tags(&args),
            "get_backlinks" => self.get_backlinks(&args),
            "get_forward_links" => self.get_forward_links(&args),
            "get_neighbors" => self.get_neighbors(&args),
            "find_path" => self.find_path(&args),
            "top_hubs" => self.top_hubs(&args),
            "find_broken_links" => self.find_broken_links(&args),
            "suggest_links" => self.suggest_links(&args),
            other => Err(format!("Unknown tool: {other}")),
        };

        // MCP convention: tool-level failures are a successful JSON-RPC
        // result with `isError: true`, not a JSON-RPC error.
        Ok(match result {
            Ok(reply) => {
                let mut out = json!({
                    "content": [{ "type": "text", "text": reply.text }],
                    "isError": false,
                });
                if let Some(structured) = reply.structured {
                    out["structuredContent"] = structured;
                }
                out
            }
            Err(err) => {
                json!({ "content": [{ "type": "text", "text": format!("Error: {err}") }], "isError": true })
            }
        })
    }

    // ── Shared helpers ──────────────────────────────────────────────────

    fn ensure_roost(&self) -> Result<(), String> {
        if self.roost.is_corvux_roost() {
            Ok(())
        } else {
            Err(format!(
                "{} is not a Corvux roost (no .corvux/ folder). Open it once in the Corvux app, or check the path.",
                self.roost.root.display()
            ))
        }
    }

    /// Read `rel`, apply `f` to get the new content plus an arbitrary
    /// "extra" value the caller wants back (e.g. the exact task line just
    /// touched), then write the new content back IFF the destination hasn't
    /// changed since the read. On conflict, returns a distinguishable
    /// `MCP_CONFLICT_PREFIX`-prefixed error rather than silently retrying —
    /// retrying is unsafe here for `complete_task`/`reschedule_task`, which
    /// match their target by fuzzy substring against whatever's currently on
    /// disk: a concurrent edit could shift which line matches between
    /// attempts (an ABA hazard), so this server never auto-retries a write —
    /// single-shot, clear conflict, let the caller re-fetch and retry.
    fn checked_read_modify_write<R>(
        &self,
        rel: &str,
        f: impl FnOnce(&str) -> Result<(String, R), String>,
    ) -> Result<R, String> {
        let before = self.roost.read(rel)?;
        let (updated, extra) = f(&before)?;
        self.roost
            .write_atomic_checked(rel, Some(&before), &updated)
            .map_err(|e| match e {
                WriteError::Conflict => format!(
                    "{MCP_CONFLICT_PREFIX}: {rel} changed on disk since it was read — retry the operation."
                ),
                WriteError::Io(msg) => msg,
            })?;
        Ok(extra)
    }

    /// Today's daily-note body: the user's `.corvux/templates/daily.md` when
    /// present and non-blank, else the built-in default (DN-1, mirrors the
    /// app's `daily_note.rs`). A read failure (missing file, non-UTF8) falls
    /// back to the default rather than erroring — a bootstrap write must
    /// never fail because of a template problem.
    fn daily_note_body(&self, date_str: &str, weekday: &str) -> String {
        let user_template = self.roost.read_internal(DAILY_TEMPLATE_REL).ok();
        pick_daily_template(user_template.as_deref(), date_str, weekday)
    }

    /// Create an idempotent bootstrap file without ever replacing a file that
    /// appeared after the caller's existence check. A concurrent creator's
    /// conflict is success: its bytes are now the base for the checked append.
    fn ensure_bootstrap(&self, rel: &str, content: &str) -> Result<(), String> {
        match self.roost.write_atomic_checked(rel, None, content) {
            Ok(()) | Err(WriteError::Conflict) => Ok(()),
            Err(WriteError::Io(message)) => Err(message),
        }
    }

    /// Resolve a note reference that may be a roost-relative path
    /// (`Projects/Atlas.md` or `Projects/Atlas`) or a bare title
    /// (`Atlas`). Returns the existing note's relative path or an error.
    fn resolve_note_ref(&self, reference: &str) -> Result<String, String> {
        let lower = reference.to_ascii_lowercase();
        let looks_like_path = reference.contains('/')
            || reference.contains('\\')
            || lower.ends_with(".md")
            || lower.ends_with(".markdown");
        if looks_like_path {
            let rel = if lower.ends_with(".md") || lower.ends_with(".markdown") {
                reference.to_string()
            } else {
                format!("{reference}.md")
            };
            return if self.roost.exists(&rel) {
                Ok(rel)
            } else {
                Err(format!("Note not found: {reference}"))
            };
        }
        let want = reference.to_lowercase();
        for (rel, _) in self.roost.scan_md() {
            let stem = Path::new(&rel)
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if stem == want {
                return Ok(rel);
            }
        }
        Err(format!("No note titled \"{reference}\" found."))
    }

    /// Append a `- [ ] …` task line to a target, creating today's daily
    /// note or the inbox as needed. `note` overrides routing to a specific
    /// existing note's `## Tasks` section.
    fn write_task_line(
        &self,
        line: &str,
        dated: bool,
        note: Option<&str>,
    ) -> Result<String, String> {
        let rel = if let Some(reference) = note {
            self.resolve_note_ref(reference)?
        } else if dated {
            let (today, weekday) = today_local();
            let folder = self
                .roost
                .settings
                .daily_note_folder
                .trim_matches(['/', '\\']);
            let rel = format!("{folder}/{today}.md");
            // Bootstrap-if-missing: idempotent (same template bytes every
            // time for a given date), so a plain unchecked write is fine
            // even if two callers race to create it — no conflict check
            // needed for a create that two writers would produce identically.
            if !self.roost.exists(&rel) {
                self.ensure_bootstrap(&rel, &self.daily_note_body(&today, weekday))?;
            }
            rel
        } else {
            let rel = self.roost.settings.inbox_path.clone();
            if !self.roost.exists(&rel) {
                self.ensure_bootstrap(&rel, "# Inbox\n\n## Tasks\n\n")?;
            }
            rel
        };
        self.checked_read_modify_write(&rel, |content| {
            Ok((append_under_tasks(content, line), ()))
        })?;
        Ok(rel)
    }

    // ── Write tools ───────────────────────────────────────────────────

    fn capture_note(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let content = arg_str(args, "content")?;
        let title = arg_opt_str(args, "title");
        if let Some(t) = &title {
            // A title is spliced into a Markdown heading (`### {t}` / `# {t}`)
            // — an embedded newline would let it fabricate a fake section
            // (e.g. a bogus `## Tasks` with a real dated checkbox) rather
            // than just being a label. Same injection class as the YAML
            // guard below, same fix: reject rather than silently mangle.
            frontmatter::reject_structural_chars(t, "title", false)?;
        }
        let folder = arg_opt_str(args, "folder");
        // Snap tags onto the existing vocabulary before deciding metadata, so
        // a case-variant of an existing tag reuses it rather than forking.
        let tags = self.canonicalize_tags(&arg_str_list(args, "tags")?);
        let properties = args.get("properties").and_then(Value::as_object);
        let (today, weekday) = today_local();

        // Default: fold the capture into today's daily note (under ## Notes).
        // Force a standalone note when the user names a folder, sets
        // properties/tags (which need real frontmatter), or when the content
        // is too big to sit comfortably inline in the daily note.
        let has_metadata = !tags.is_empty() || properties.map(|m| !m.is_empty()).unwrap_or(false);
        let standalone = folder.is_some() || has_metadata || content.len() > CAPTURE_INLINE_MAX;

        if !standalone {
            let folder = self
                .roost
                .settings
                .daily_note_folder
                .trim_matches(['/', '\\']);
            let rel = format!("{folder}/{today}.md");
            // Idempotent bootstrap (see write_task_line) — plain write is fine.
            if !self.roost.exists(&rel) {
                self.ensure_bootstrap(&rel, &self.daily_note_body(&today, weekday))?;
            }
            let block = match title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(t) => format!("### {t}\n\n{}", content.trim_end()),
                None => content.trim_end().to_string(),
            };
            self.checked_read_modify_write(&rel, |daily| {
                Ok((append_under_heading(daily, "Notes", &block), ()))
            })?;
            let warn = self.link_warning_suffix(&content, &note_dir(&rel));
            return Ok(format!("Captured into {rel} (under ## Notes){warn}"));
        }

        // Standalone note.
        let folder = folder.unwrap_or_else(|| "Captured".to_string());
        let folder = folder.trim_matches(['/', '\\']).to_string();
        let base_title = title
            .as_deref()
            .map(sanitize_filename)
            .unwrap_or_else(|| format!("Capture {today}"));
        let heading = title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&base_title);

        // Build frontmatter: date + type (caller may override either via
        // `properties`; otherwise today's date and type "capture"), merged
        // tags (always includes "capture"), then any other properties. Every
        // caller-supplied scalar is validated (no embedded newline; values
        // may contain a colon, keys/tags may not) and YAML-escaped so it
        // can't fabricate frontmatter structure or get reinterpreted as a
        // non-string type on re-parse.
        let type_val = properties
            .and_then(|m| m.get("type"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("capture");
        let date_val = properties
            .and_then(|m| m.get("date"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(today.as_str());
        frontmatter::reject_structural_chars(type_val, "type", false)?;
        frontmatter::reject_structural_chars(date_val, "date", false)?;
        let mut fm = format!(
            "date: {}\ntype: {}\n",
            frontmatter::yaml_scalar(date_val),
            frontmatter::yaml_scalar(type_val)
        );
        let mut all_tags = vec!["capture".to_string()];
        for t in &tags {
            let t = t.trim_start_matches('#');
            frontmatter::reject_structural_chars(t, "tag", true)?;
            if !all_tags.iter().any(|e| e.eq_ignore_ascii_case(t)) {
                all_tags.push(t.to_string());
            }
        }
        fm.push_str("tags:\n");
        for t in &all_tags {
            fm.push_str(&format!("  - {}\n", frontmatter::yaml_scalar(t)));
        }
        if let Some(map) = properties {
            for (k, v) in map {
                // date/type/tags are owned above — don't let callers clobber them.
                if ["date", "type", "tags"].contains(&k.to_lowercase().as_str()) {
                    continue;
                }
                frontmatter::reject_structural_chars(k, "property key", true)?;
                match v {
                    Value::String(s) => {
                        frontmatter::reject_structural_chars(s, "property value", false)?;
                        fm.push_str(&format!("{k}: {}\n", frontmatter::yaml_scalar(s)));
                    }
                    Value::Number(n) => fm.push_str(&format!("{k}: {n}\n")),
                    Value::Bool(b) => fm.push_str(&format!("{k}: {b}\n")),
                    Value::Array(arr) => {
                        let items: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                        if !items.is_empty() {
                            fm.push_str(&format!("{k}:\n"));
                            for it in items {
                                frontmatter::reject_structural_chars(it, "property value", false)?;
                                fm.push_str(&format!("  - {}\n", frontmatter::yaml_scalar(it)));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let note = format!("---\n{fm}---\n\n# {heading}\n\n{}\n", content.trim_end());

        // Filename-collision loop, TOCTOU-safe: each candidate is a
        // create-only checked write, so a concurrent capture that wins the
        // same candidate name is caught as a Conflict and this just advances
        // to the next candidate rather than clobbering it.
        let mut rel = format!("{folder}/{base_title}.md");
        let mut n = 2;
        loop {
            match self.roost.write_atomic_checked(&rel, None, &note) {
                Ok(()) => break,
                Err(WriteError::Conflict) => {
                    rel = format!("{folder}/{base_title} ({n}).md");
                    n += 1;
                }
                Err(WriteError::Io(e)) => return Err(e),
            }
        }
        let warn = self.link_warning_suffix(&content, &note_dir(&rel));
        Ok(format!("Captured to {rel}{warn}"))
    }

    fn write_note(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let path = arg_str(args, "path")?;
        let content = arg_str(args, "content")?;
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let lower = path.to_ascii_lowercase();
        let rel = if lower.ends_with(".md") || lower.ends_with(".markdown") {
            path
        } else {
            format!("{path}.md")
        };
        let body = if content.ends_with('\n') {
            content
        } else {
            format!("{content}\n")
        };
        if overwrite {
            // Blind overwrite is the explicit, intentional contract here —
            // no conflict check.
            self.roost.write_atomic(&rel, &body)?;
        } else {
            // Create-only, TOCTOU-safe: the checked write re-verifies
            // absence immediately before committing, instead of a separate
            // `exists()` check racing against the write.
            self.roost
                .write_atomic_checked(&rel, None, &body)
                .map_err(|e| match e {
                    WriteError::Conflict => format!(
                        "{rel} already exists. Pass overwrite:true to replace it, or use append_to_note to add to it."
                    ),
                    WriteError::Io(msg) => msg,
                })?;
        }
        let warn = self.link_warning_suffix(&body, &note_dir(&rel));
        Ok(format!("Wrote {rel}{warn}"))
    }

    fn append_to_note(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let target = arg_str(args, "note")?;
        let text = arg_str(args, "text")?;
        let heading = arg_opt_str(args, "heading");
        if let Some(h) = &heading {
            // Same injection class as capture_note's `title` — an embedded
            // newline would let `heading` fabricate structure it isn't
            // meant to.
            frontmatter::reject_structural_chars(h, "heading", false)?;
        }
        let rel = self.resolve_note_ref(&target)?;
        self.checked_read_modify_write(&rel, |content| {
            let updated = match &heading {
                Some(h) => append_under_heading(content, h, &text),
                None => append_to_end(content, &text),
            };
            Ok((updated, ()))
        })?;
        let where_ = heading
            .map(|h| format!("{rel} (under ## {h})"))
            .unwrap_or_else(|| rel.clone());
        let warn = self.link_warning_suffix(&text, &note_dir(&rel));
        Ok(format!("Appended to {where_}{warn}"))
    }

    fn add_task(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let text = arg_str(args, "text")?;
        let provided_date = arg_opt_str(args, "due_date");
        let due_time = arg_opt_str(args, "due_time");
        let note = arg_opt_str(args, "note");

        if let Some(d) = &provided_date {
            if !is_iso_date(d) {
                return Err(format!("due_date must be YYYY-MM-DD, got \"{d}\""));
            }
        }
        if let Some(t) = &due_time {
            if !is_hhmm(t) {
                return Err(format!("due_time must be 24-hour HH:MM, got \"{t}\""));
            }
            if provided_date.is_none() {
                return Err(
                    "due_time requires due_date (use add_reminder, or pass due_date)".to_string(),
                );
            }
        }

        // Convention 0.3: a dateless checkbox isn't a task. Default to today
        // so a plain "add a todo" still produces a real task (mirrors the
        // app's create_task), routed to today's daily note unless `note`
        // overrides. The agent can pass due_date for any other day.
        let due_date = provided_date.or_else(|| Some(today_local().0));
        let line = format_task_line(&text, due_date.as_deref(), due_time.as_deref());
        let rel = self.write_task_line(&line, true, note.as_deref())?;
        Ok(format!("Added to {rel}:\n{line}"))
    }

    fn add_reminder(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let text = arg_str(args, "text")?;
        let date = arg_str(args, "date")?;
        let time = arg_str(args, "time")?;
        if !is_iso_date(&date) {
            return Err(format!("date must be YYYY-MM-DD, got \"{date}\""));
        }
        if !is_hhmm(&time) {
            return Err(format!("time must be 24-hour HH:MM, got \"{time}\""));
        }
        let note = arg_opt_str(args, "note");
        // A reminder IS a date+time task — that's what fires the Corvux
        // notification. There is no separate reminder syntax in the app.
        let line = format_task_line(&text, Some(&date), Some(&time));
        let rel = self.write_task_line(&line, true, note.as_deref())?;
        Ok(format!(
            "Reminder set in {rel} (fires {date} {time}):\n{line}"
        ))
    }

    fn complete_task(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let note = arg_str(args, "note")?;
        let text = arg_str(args, "text")?;
        let rel = self.resolve_note_ref(&note)?;
        let (today, _) = today_local();
        // Single-shot checked write, deliberately no retry: this matches by
        // fuzzy substring against whatever's on disk, so re-reading and
        // re-matching after a conflict could silently complete a DIFFERENT
        // task that happens to match post-edit (see checked_read_modify_write's
        // doc). A conflict here is surfaced to the caller instead.
        let line = self
            .checked_read_modify_write(&rel, |content| complete_task_in(content, &text, &today))?;
        Ok(format!("Completed in {rel}:\n{line}"))
    }

    // ── Read tools ──────────────────────────────────────────────────────

    fn search_roost(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let query = arg_str(args, "query")?;
        let limit = arg_usize(args, "limit").unwrap_or(10).clamp(1, 50);
        // Multi-term AND: every whitespace-separated term must appear.
        let terms: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
        if terms.is_empty() {
            return Err("query is empty".to_string());
        }
        let folder = arg_opt_str(args, "folder");
        let tag = arg_opt_str(args, "tag");

        let idx = self.index_refreshed();
        let hits = idx.search(&terms, folder.as_deref(), tag.as_deref(), None, limit);

        if hits.is_empty() {
            return Ok(ToolReply::text(format!("No notes matched \"{query}\".")));
        }
        let mut out = format!("{} match(es) for \"{query}\":\n", hits.len());
        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                out.push_str(&format!("- {}\n    {}\n", h.rel, h.snippet));
                json!({ "path": h.rel, "score": h.score, "snippet": h.snippet })
            })
            .collect();
        Ok(ToolReply::with(out, json!({ "results": results })))
    }

    fn find_notes(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let query = arg_str(args, "query")?;
        let limit = arg_usize(args, "limit").unwrap_or(20).clamp(1, 100);

        let idx = self.index_refreshed();
        let matches = idx.find_paths(&query, limit);

        if matches.is_empty() {
            return Ok(ToolReply::text(format!(
                "No notes whose path matches \"{query}\"."
            )));
        }
        let mut out = format!("{} note(s) matching \"{query}\":\n", matches.len());
        for rel in &matches {
            out.push_str(&format!("- {rel}\n"));
        }
        Ok(ToolReply::with(out, json!({ "paths": matches })))
    }

    fn read_note(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let rel = if let Some(path) = arg_opt_str(args, "path") {
            path
        } else if let Some(title) = arg_opt_str(args, "title") {
            self.resolve_note_ref(&title)?
        } else {
            return Err("read_note needs either `path` or `title`".to_string());
        };
        let content = self.roost.read(&rel)?;

        // Surface the note's properties (frontmatter) up front as structured
        // fields, then the full note text.
        let mut out = format!("[{rel}]\n\n");
        if let (Some(fm_inner), _) = frontmatter::split(&content) {
            let props = frontmatter::parse(&fm_inner);
            if !props.is_empty() {
                out.push_str("Properties:\n");
                out.push_str(&frontmatter::render(&props));
                out.push('\n');
            }
        }
        out.push_str(&cap(&content));
        Ok(out)
    }

    /// Composite "everything about this note" tool: properties, a body snippet,
    /// tags, backlinks and forward links in a single call — so an agent doesn't
    /// have to chain read_note + get_backlinks + get_forward_links to understand
    /// a note and where it sits in the graph.
    fn note_context(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let note = arg_str(args, "note")?;
        let limit = arg_usize(args, "limit").unwrap_or(20).clamp(1, 100);
        let idx = self.index_refreshed();
        let rel = idx
            .resolve_ref(&note)
            .ok_or_else(|| format!("Note not found: {note}"))?;
        let content = self.roost.read(&rel)?;

        let (fm, body) = frontmatter::split(&content);
        let props = fm.as_deref().map(frontmatter::parse).unwrap_or_default();
        let tags = frontmatter::all_tags(&content);
        let snippet = body_snippet(&body, 280);
        let backlinks = idx.backlinks(&rel);
        let forward = idx.forward_links(&rel);

        let mut out = format!("[{rel}]\n");
        if !props.is_empty() {
            out.push_str("\nProperties:\n");
            out.push_str(&frontmatter::render(&props));
        }
        if !tags.is_empty() {
            out.push_str(&format!("\nTags: {}\n", tags.join(", ")));
        }
        if !snippet.is_empty() {
            out.push_str(&format!("\nSnippet: {snippet}\n"));
        }
        out.push_str(&format!("\nBacklinks — {} total:\n", backlinks.len()));
        push_link_lines(&mut out, &backlinks, limit, "←");
        out.push_str(&format!("Forward links — {} total:\n", forward.len()));
        push_link_lines(&mut out, &forward, limit, "→");

        let structured = json!({
            "note": rel,
            "properties": props_to_json(&props),
            "tags": tags,
            "snippet": snippet,
            "backlinks": {
                "total": backlinks.len(),
                "shown": backlinks.iter().take(limit).collect::<Vec<_>>(),
            },
            "forward_links": {
                "total": forward.len(),
                "shown": forward.iter().take(limit).collect::<Vec<_>>(),
            },
        });
        Ok(ToolReply::with(out, structured))
    }

    /// One-call agent memory retrieval: keyword-search the roost for `topic`
    /// (optionally scoped to a project), then walk the wiki-link graph one
    /// hop out from the top hits. Linked notes that never mention the topic's
    /// words — the ones plain search misses — come back in the same bundle,
    /// so a caller doesn't have to chain search_roost + N × note_context.
    /// Snippets only, never full bodies: the caller follows up with
    /// read_note / note_context on the notes that matter.
    fn recall(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let topic = arg_str(args, "topic")?;
        let terms: Vec<String> = topic.split_whitespace().map(|t| t.to_lowercase()).collect();
        if terms.is_empty() {
            return Err("topic is empty".to_string());
        }
        let project = arg_opt_str(args, "project");
        let limit = arg_usize(args, "limit").unwrap_or(5).clamp(1, 15);

        let idx = self.index_refreshed();
        let seeds = idx.search(&terms, None, None, project.as_deref(), limit);
        if seeds.is_empty() {
            let scope = project
                .map(|p| format!(" in project \"{p}\""))
                .unwrap_or_default();
            return Ok(ToolReply::text(format!(
                "Nothing recalled for \"{topic}\"{scope}. Try different keywords, drop the project scope, or use search_roost / find_notes for a broader look."
            )));
        }

        // One hop out: every note linked to/from a seed that isn't itself a
        // seed, ranked by how many seeds it connects to. BTreeMap keeps ties
        // deterministic.
        let seed_set: HashSet<&str> = seeds.iter().map(|h| h.rel.as_str()).collect();
        let mut related: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for hit in &seeds {
            // HashSet dedup — a hub seed can carry tens of thousands of
            // links; a Vec::contains merge would be O(degree²) (audit
            // finding, 2026-07-18).
            let mut seen: HashSet<String> = HashSet::new();
            let connected = idx
                .backlinks(&hit.rel)
                .into_iter()
                .chain(idx.forward_links(&hit.rel));
            for rel in connected {
                if !seen.insert(rel.clone()) || seed_set.contains(rel.as_str()) {
                    continue;
                }
                let via = related.entry(rel).or_default();
                if !via.contains(&hit.rel) {
                    via.push(hit.rel.clone());
                }
            }
        }
        let mut related: Vec<(String, Vec<String>)> = related.into_iter().collect();
        related.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        related.truncate(RECALL_RELATED_MAX);

        let scope = project
            .as_deref()
            .map(|p| format!(", project \"{p}\""))
            .unwrap_or_default();
        let mut out = format!(
            "Recall for \"{topic}\"{scope} — {} direct match(es), {} related via links:\n\nDirect matches:\n",
            seeds.len(),
            related.len()
        );
        let seeds_json: Vec<Value> = seeds
            .iter()
            .map(|h| {
                out.push_str(&format!("- {}\n    {}\n", h.rel, h.snippet));
                let content = idx.content(&h.rel).unwrap_or_default();
                let (fm, _) = frontmatter::split(content);
                let props = fm.as_deref().map(frontmatter::parse).unwrap_or_default();
                json!({
                    "path": h.rel,
                    "score": h.score,
                    "snippet": h.snippet,
                    "properties": props_to_json(&props),
                    "tags": frontmatter::all_tags(content),
                })
            })
            .collect();

        let related_json: Vec<Value> = if related.is_empty() {
            out.push_str("\nRelated: (none — the matched notes have no wiki-links yet)\n");
            Vec::new()
        } else {
            out.push_str("\nRelated (linked to the matches; may not mention the topic):\n");
            related
                .iter()
                .map(|(rel, via)| {
                    let content = idx.content(rel).unwrap_or_default();
                    let (_, body) = frontmatter::split(content);
                    let snippet = body_snippet(&body, 200);
                    out.push_str(&format!(
                        "- {rel} (via {})\n    {snippet}\n",
                        via.join(", ")
                    ));
                    json!({ "path": rel, "via": via, "snippet": snippet })
                })
                .collect()
        };
        out.push_str("\nUse read_note or note_context on any of these for the full content.");

        let structured = json!({
            "topic": topic,
            "project": project,
            "seeds": seeds_json,
            "related": related_json,
        });
        Ok(ToolReply::with(out, structured))
    }

    fn find_by_property(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let key = arg_str(args, "key")?;
        let value = arg_str(args, "value")?;
        let limit = arg_usize(args, "limit").unwrap_or(50).clamp(1, 200);

        let idx = self.index_refreshed();
        let hits = idx.find_by_property(&key, &value, limit);
        if hits.is_empty() {
            return Ok(ToolReply::text(format!("No notes where {key} = {value}.")));
        }
        let mut out = format!("{} note(s) where {key} = {value}:\n", hits.len());
        for rel in &hits {
            out.push_str(&format!("- {rel}\n"));
        }
        Ok(ToolReply::with(out, json!({ "paths": hits })))
    }

    fn set_property(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let note = arg_str(args, "note")?;
        let key = arg_str(args, "key")?;
        let value = arg_str(args, "value")?;
        if key.eq_ignore_ascii_case("tags") {
            return Err("Use add_tags to manage tags (they're a list).".to_string());
        }
        let rel = self.resolve_note_ref(&note)?;
        self.checked_read_modify_write(&rel, |content| {
            Ok((frontmatter::set_property(content, &key, &value)?, ()))
        })?;
        Ok(format!("Set {key}: {value} in {rel}"))
    }

    fn add_tags(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let note = arg_str(args, "note")?;
        let tags = arg_str_list(args, "tags")?;
        if tags.is_empty() {
            return Err("Provide at least one tag.".to_string());
        }
        // Reuse existing tag spellings where they exist (kills #ml/#ML forks).
        let tags = self.canonicalize_tags(&tags);
        let rel = self.resolve_note_ref(&note)?;
        self.checked_read_modify_write(&rel, |content| {
            Ok((frontmatter::add_tags(content, &tags)?, ()))
        })?;
        Ok(format!("Tagged {rel} with: {}", tags.join(", ")))
    }

    /// The roost's existing tag vocabulary, most-used first. Call this BEFORE
    /// tagging so you reuse an established tag instead of inventing a variant
    /// (`#ml` vs `#machine-learning`) that fragments the graph.
    fn list_tags(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let limit = arg_usize(args, "limit").unwrap_or(100).clamp(1, 500);
        let idx = self.index_refreshed();
        let counts = idx.tag_counts();
        let total = counts.len();
        if total == 0 {
            return Ok(ToolReply::text(
                "No tags in the roost yet — the first tags you add define the vocabulary.",
            ));
        }
        let shown: Vec<(String, usize)> = counts.into_iter().take(limit).collect();
        let lines: Vec<String> = shown.iter().map(|(t, c)| format!("#{t} ({c})")).collect();
        let header = if total > shown.len() {
            format!(
                "{total} tag(s), showing the top {}. Reuse these when tagging — don't invent near-duplicates:",
                shown.len()
            )
        } else {
            format!("{total} tag(s). Reuse these when tagging — don't invent near-duplicates:")
        };
        let structured = json!({
            "total": total,
            "tags": shown
                .iter()
                .map(|(t, c)| json!({ "tag": t, "count": c }))
                .collect::<Vec<_>>(),
        });
        Ok(ToolReply::with(
            format!("{header}\n{}", lines.join("  ")),
            structured,
        ))
    }

    /// Audit every `[[wiki-link]]` in the roost and report the broken ones:
    /// dangling (no such note) or ambiguous (the stem matches several notes,
    /// so the graph guesses one and may connect the wrong pair). Use this to
    /// clean up existing links; the write tools warn about new ones inline.
    fn find_broken_links(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let limit = arg_usize(args, "limit").unwrap_or(100).clamp(1, 500);
        let idx = self.index_refreshed();
        let broken = idx.broken_links();
        if broken.is_empty() {
            return Ok(ToolReply::text(
                "No broken links — every [[wiki-link]] resolves to exactly one note.",
            ));
        }
        let total = broken.len();
        let shown: Vec<_> = broken.into_iter().take(limit).collect();
        let lines: Vec<String> = shown
            .iter()
            .map(|b| {
                if b.candidates.is_empty() {
                    format!("{}: [[{}]] → no such note", b.source, b.target)
                } else {
                    format!(
                        "{}: [[{}]] → ambiguous ({})",
                        b.source,
                        b.target,
                        b.candidates.join(" / ")
                    )
                }
            })
            .collect();
        let header = if total > shown.len() {
            format!("{total} broken link(s), showing {}:", shown.len())
        } else {
            format!("{total} broken link(s):")
        };
        let structured = json!({
            "total": total,
            "broken": shown
                .iter()
                .map(|b| json!({
                    "source": b.source,
                    "target": b.target,
                    "kind": if b.candidates.is_empty() { "dangling" } else { "ambiguous" },
                    "candidates": b.candidates,
                }))
                .collect::<Vec<_>>(),
        });
        Ok(ToolReply::with(
            format!("{header}\n{}", lines.join("\n")),
            structured,
        ))
    }

    /// Propose MISSING `[[links]]` — the twin of `find_broken_links` (that
    /// audits bad edges; this finds absent ones). Two mechanical signals:
    /// a note mentioning another's title as plain text without linking it,
    /// and unlinked pairs sharing a rare tag. Read-only — acting on a
    /// suggestion (after confirming with the user) is the caller's job, via
    /// append_to_note. Every accepted link densifies the graph `recall`
    /// walks, so this is the memory layer's gardener.
    fn suggest_links(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let limit = arg_usize(args, "limit").unwrap_or(20).clamp(1, 100);
        let idx = self.index_refreshed();
        let scope = match arg_opt_str(args, "note") {
            Some(n) => Some(
                idx.resolve_ref(&n)
                    .ok_or_else(|| format!("Note not found: {n}"))?,
            ),
            None => None,
        };
        let suggestions = idx.suggest_links(scope.as_deref(), limit);

        if suggestions.is_empty() {
            let where_ = scope.map(|s| format!(" for {s}")).unwrap_or_default();
            return Ok(ToolReply::text(format!(
                "No missing links suggested{where_} — nothing unlinked looks related (or the related notes are already linked)."
            )));
        }

        let mut out = format!(
            "{} suggested missing link(s). To act: confirm with the user, then add a `Related: [[Target]]` line via append_to_note.\n",
            suggestions.len()
        );
        let rows: Vec<Value> = suggestions
            .iter()
            .map(|s| {
                let mut reasons: Vec<String> = Vec::new();
                if s.mentioned {
                    reasons.push("mentions the title".to_string());
                }
                if !s.shared_tags.is_empty() {
                    let tags: Vec<String> = s
                        .shared_tags
                        .iter()
                        .map(|(t, c)| format!("#{t} ({c} notes)"))
                        .collect();
                    reasons.push(format!("shared rare tags: {}", tags.join(", ")));
                }
                out.push_str(&format!(
                    "- {} → {} — {}\n",
                    s.source,
                    s.target,
                    reasons.join("; ")
                ));
                if !s.evidence.is_empty() {
                    out.push_str(&format!("    \"{}\"\n", s.evidence));
                }
                json!({
                    "source": s.source,
                    "target": s.target,
                    "mentioned": s.mentioned,
                    "shared_tags": s.shared_tags
                        .iter()
                        .map(|(t, c)| json!({ "tag": t, "notes": c }))
                        .collect::<Vec<_>>(),
                    "evidence": s.evidence,
                    "score": s.score,
                })
            })
            .collect();

        Ok(ToolReply::with(
            out,
            json!({ "total": rows.len(), "suggestions": rows }),
        ))
    }

    fn list_open_tasks(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let date_filter =
            arg_opt_str(args, "date").map(|d| if d == "today" { today_local().0 } else { d });
        if let Some(d) = &date_filter {
            if !is_iso_date(d) {
                return Err(format!("date must be \"today\" or YYYY-MM-DD, got \"{d}\""));
            }
        }
        let limit = arg_usize(args, "limit").unwrap_or(50).clamp(1, 200);
        let (today, _) = today_local();

        // Bucket each open task by its due date relative to today so the most
        // urgent work surfaces first. ISO dates compare lexicographically, so
        // a plain string `<`/`==` is a correct date comparison.
        const LABELS: [&str; 4] = ["Overdue", "Today", "Upcoming", "No date"];
        let idx = self.index_refreshed();
        // (bucket, due, rel, line, text)
        let mut rows: Vec<(u8, Option<String>, String, usize, String)> = Vec::new();
        for (rel, task) in idx.all_open_tasks() {
            if let Some(df) = &date_filter {
                if task.due_date.as_deref() != Some(df.as_str()) {
                    continue;
                }
            }
            let bucket = match task.due_date.as_deref() {
                Some(d) if d < today.as_str() => 0,
                Some(d) if d == today.as_str() => 1,
                Some(_) => 2,
                None => 3,
            };
            rows.push((bucket, task.due_date, rel, task.line_no, task.text));
        }

        let scope = date_filter
            .as_ref()
            .map(|d| format!(" due {d}"))
            .unwrap_or_default();
        if rows.is_empty() {
            return Ok(ToolReply::text(format!("No open tasks{scope}.")));
        }

        // Sort by bucket, then due date asc, then path + line for stability.
        rows.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.cmp(&b.3))
        });

        let total = rows.len();
        let mut counts = [0usize; 4];
        for r in &rows {
            counts[r.0 as usize] += 1;
        }
        rows.truncate(limit);

        let shown = if total > rows.len() {
            format!(" — showing first {}", rows.len())
        } else {
            String::new()
        };
        let mut out = format!(
            "{total} open task(s){scope} (overdue {}, today {}, upcoming {}, no-date {}){shown}:\n",
            counts[0], counts[1], counts[2], counts[3]
        );
        let mut cur: Option<u8> = None;
        let mut tasks_json: Vec<Value> = Vec::with_capacity(rows.len());
        for (bucket, due, rel, line_no, text) in &rows {
            if cur != Some(*bucket) {
                out.push_str(&format!("\n{}:\n", LABELS[*bucket as usize]));
                cur = Some(*bucket);
            }
            // Frontmatter tasks have no body line (line_no 0) — omit `:line`.
            if *line_no == 0 {
                out.push_str(&format!("- [ ] {text}   ({rel})\n"));
            } else {
                out.push_str(&format!("- [ ] {text}   ({rel}:{line_no})\n"));
            }
            tasks_json.push(json!({
                "text": text,
                "due": due,
                "path": rel,
                "line": line_no,
                "bucket": LABELS[*bucket as usize].to_lowercase(),
            }));
        }
        Ok(ToolReply::with(
            out,
            json!({
                "tasks": tasks_json,
                "counts": {
                    "overdue": counts[0],
                    "today": counts[1],
                    "upcoming": counts[2],
                    "no_date": counts[3],
                    "total": total,
                },
            }),
        ))
    }

    // ── New read tools (P1.3) ───────────────────────────────────────────

    fn list_recent_notes(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let days = arg_usize(args, "days").unwrap_or(7).clamp(1, 3650) as u64;
        let limit = arg_usize(args, "limit").unwrap_or(20).clamp(1, 100);
        let idx = self.index_refreshed();
        let rows = idx.recent(days, limit, std::time::SystemTime::now());
        if rows.is_empty() {
            return Ok(ToolReply::text(format!(
                "No notes modified in the last {days} day(s)."
            )));
        }
        let mut out = format!(
            "{} note(s) modified in the last {days} day(s):\n",
            rows.len()
        );
        let results: Vec<Value> = rows
            .iter()
            .map(|r| {
                out.push_str(&format!("- {}  ({})\n", r.rel, r.modified));
                json!({ "path": r.rel, "modified": r.modified })
            })
            .collect();
        Ok(ToolReply::with(out, json!({ "results": results })))
    }

    fn roost_stats(&self, _args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let idx = self.index_refreshed();
        let s = idx.stats();
        let text = format!(
            "Roost: {} notes, {} open tasks, {} folders, {} wiki-links.",
            s.notes, s.open_tasks, s.folders, s.links
        );
        Ok(ToolReply::with(
            text,
            json!({
                "notes": s.notes,
                "open_tasks": s.open_tasks,
                "folders": s.folders,
                "links": s.links,
            }),
        ))
    }

    // ── Graph tools (P3) — in-memory wiki-link graph, no CozoDB ──────────

    fn get_backlinks(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let note = arg_str(args, "note")?;
        let idx = self.index_refreshed();
        let rel = idx
            .resolve_ref(&note)
            .ok_or_else(|| format!("Note not found: {note}"))?;
        let links = idx.backlinks(&rel);
        let limit = arg_usize(args, "limit").unwrap_or(LINK_LIST_DEFAULT_LIMIT);
        Ok(render_link_list(
            &format!("{} note(s) link to {rel}", links.len()),
            "No backlinks.",
            &rel,
            links,
            limit,
        ))
    }

    fn get_forward_links(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let note = arg_str(args, "note")?;
        let idx = self.index_refreshed();
        let rel = idx
            .resolve_ref(&note)
            .ok_or_else(|| format!("Note not found: {note}"))?;
        let links = idx.forward_links(&rel);
        let limit = arg_usize(args, "limit").unwrap_or(LINK_LIST_DEFAULT_LIMIT);
        Ok(render_link_list(
            &format!("{rel} links to {} note(s)", links.len()),
            "No outgoing links.",
            &rel,
            links,
            limit,
        ))
    }

    fn get_neighbors(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let note = arg_str(args, "note")?;
        let depth = arg_usize(args, "depth").unwrap_or(1).clamp(1, 4);
        let idx = self.index_refreshed();
        let rel = idx
            .resolve_ref(&note)
            .ok_or_else(|| format!("Note not found: {note}"))?;
        let (nb, truncated) = idx.neighbors(&rel, depth);
        if nb.is_empty() {
            return Ok(ToolReply::text(format!("{rel} has no linked neighbors.")));
        }
        // The BFS itself stops early once it hits index::NEIGHBORS_MAX_RESULTS
        // (a dense graph at shallow depth could otherwise touch thousands of
        // notes) — a result count at the cap is the signal that happened.
        let mut out = format!("{} note(s) within {depth} hop(s) of {rel}:\n", nb.len());
        if truncated {
            out.push_str("(capped — the graph has more reachable notes than shown)\n");
        }
        let results: Vec<Value> = nb
            .iter()
            .map(|(r, hop)| {
                out.push_str(&format!("- {r}  ({hop} hop)\n"));
                json!({ "path": r, "hops": hop })
            })
            .collect();
        Ok(ToolReply::with(
            out,
            json!({ "focus": rel, "neighbors": results, "truncated": truncated }),
        ))
    }

    fn find_path(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let from = arg_str(args, "from")?;
        let to = arg_str(args, "to")?;
        let idx = self.index_refreshed();
        let from_rel = idx
            .resolve_ref(&from)
            .ok_or_else(|| format!("Note not found: {from}"))?;
        let to_rel = idx
            .resolve_ref(&to)
            .ok_or_else(|| format!("Note not found: {to}"))?;
        match idx.find_path(&from_rel, &to_rel) {
            Some(path) => {
                let hops = path.len().saturating_sub(1);
                let text = format!("Path ({hops} hop(s)): {}", path.join(" → "));
                Ok(ToolReply::with(
                    text,
                    json!({ "from": from_rel, "to": to_rel, "hops": hops, "path": path }),
                ))
            }
            None => Ok(ToolReply::with(
                format!("No link path between {from_rel} and {to_rel}."),
                json!({ "from": from_rel, "to": to_rel, "path": Value::Null }),
            )),
        }
    }

    fn top_hubs(&self, args: &Value) -> Result<ToolReply, String> {
        self.ensure_roost()?;
        let limit = arg_usize(args, "limit").unwrap_or(10).clamp(1, 100);
        let idx = self.index_refreshed();
        let hubs = idx.top_hubs(limit);
        if hubs.is_empty() {
            return Ok(ToolReply::text("No notes to rank.".to_string()));
        }
        let mut out = format!("Top {} hub note(s) by PageRank:\n", hubs.len());
        let results: Vec<Value> = hubs
            .iter()
            .map(|(rel, rank)| {
                out.push_str(&format!("- {rel}  ({rank:.4})\n"));
                json!({ "path": rel, "rank": rank })
            })
            .collect();
        Ok(ToolReply::with(out, json!({ "results": results })))
    }

    fn reschedule_task(&self, args: &Value) -> Result<String, String> {
        self.ensure_roost()?;
        let note = arg_str(args, "note")?;
        let text = arg_str(args, "text")?;
        let new_date = arg_opt_str(args, "date").unwrap_or_default();
        let new_time = arg_opt_str(args, "time").unwrap_or_default();
        if new_date.is_empty() && new_time.is_empty() {
            return Err("pass at least one of `date` or `time` (or \"clear\")".to_string());
        }
        if !new_date.is_empty() && new_date != "clear" && !is_iso_date(&new_date) {
            return Err(format!(
                "date must be YYYY-MM-DD or \"clear\", got \"{new_date}\""
            ));
        }
        if !new_time.is_empty() && new_time != "clear" && !is_hhmm(&new_time) {
            return Err(format!(
                "time must be 24-hour HH:MM or \"clear\", got \"{new_time}\""
            ));
        }
        let rel = self.resolve_note_ref(&note)?;
        // Single-shot, no retry — same fuzzy-match ABA hazard as complete_task.
        let line = self.checked_read_modify_write(&rel, |content| {
            reschedule_task_in(content, &text, &new_date, &new_time)
        })?;
        Ok(format!("Rescheduled in {rel}:\n{line}"))
    }
}

/// One-line body snippet: whitespace-flattened, capped to `max` chars.
fn body_snippet(body: &str, max: usize) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let s: String = flat.chars().take(max).collect();
        format!("{}…", s.trim_end())
    } else {
        flat
    }
}

/// Frontmatter props → a JSON object (scalars as strings, lists as arrays).
fn props_to_json(props: &[(String, frontmatter::PropValue)]) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in props {
        let jv = match v {
            frontmatter::PropValue::Scalar(s) => json!(s),
            frontmatter::PropValue::List(items) => json!(items),
        };
        map.insert(k.clone(), jv);
    }
    Value::Object(map)
}

/// Append up to `limit` link lines (each prefixed with `arrow`) to `out`, with
/// a "… and N more" tail when the list is longer than the cap.
fn push_link_lines(out: &mut String, links: &[String], limit: usize, arrow: &str) {
    if links.is_empty() {
        out.push_str("  (none)\n");
        return;
    }
    for r in links.iter().take(limit) {
        out.push_str(&format!("  {arrow} {r}\n"));
    }
    if links.len() > limit {
        out.push_str(&format!("  … and {} more\n", links.len() - limit));
    }
}

/// Default cap on how many links `get_backlinks` / `get_forward_links` return
/// to the LLM. A hub note can have thousands of backlinks; dumping them all
/// blows the model's context and buries the signal. Matches the bounded shape
/// of `top_hubs` / `find_broken_links`.
const LINK_LIST_DEFAULT_LIMIT: usize = 100;

/// Render a simple `(summary, [paths])` link-list reply with structured
/// `{ note, links: [...] }`. Shared by `get_backlinks` / `get_forward_links`.
///
/// CO-1: caps the list at `limit` and appends a "showing N of M" note when
/// truncated, so a highly-connected note doesn't flood the LLM. The structured
/// payload is capped to the same slice, and carries `total` + `truncated` so a
/// client can tell there's more.
fn render_link_list(
    summary: &str,
    empty: &str,
    note: &str,
    links: Vec<String>,
    limit: usize,
) -> ToolReply {
    if links.is_empty() {
        return ToolReply::text(empty.to_string());
    }
    let total = links.len();
    let limit = limit.clamp(1, 1000);
    let truncated = total > limit;
    let shown: Vec<String> = links.into_iter().take(limit).collect();

    let mut out = if truncated {
        format!("{summary} — showing first {} of {total}:\n", shown.len())
    } else {
        format!("{summary}:\n")
    };
    for l in &shown {
        out.push_str(&format!("- {l}\n"));
    }
    ToolReply::with(
        out,
        json!({ "note": note, "links": shown, "total": total, "truncated": truncated }),
    )
}

// ── Tool schemas (advertised by tools/list) ────────────────────────────

fn tool_specs() -> Value {
    let mut tools = json!([
        {
            "name": "capture_note",
            "description": "Save a chat answer, snippet, or thought into the user's Corvux memory. By default it's folded into today's daily note under ## Notes. Pass `folder`, `tags`, or `properties` to save it as its own note (with frontmatter) instead; very large captures also get their own note. Use this for 'remember this' / 'save this'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The note body (markdown allowed)." },
                    "title": { "type": "string", "description": "Optional title; becomes a ### heading in the daily note, or the filename + H1 of a standalone note." },
                    "folder": { "type": "string", "description": "Optional: save as a standalone note in this folder instead of the daily note." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags (frontmatter). Saving with tags makes it a standalone note." },
                    "properties": { "type": "object", "description": "Optional extra frontmatter properties (e.g. {\"status\":\"draft\",\"project\":\"Atlas\"}). Values may be strings, numbers, booleans, or string arrays." }
                },
                "required": ["content"]
            },
            "annotations": { "destructiveHint": false, "idempotentHint": false }
        },
        {
            "name": "write_note",
            "description": "Create a note at a specific roost path. Prefer this over a raw file Write — it writes correct frontmatter and won't write outside the roost. Refuses to overwrite an existing note unless overwrite:true. Use append_to_note to add to an existing note instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Roost-relative path, e.g. 'Projects/Atlas.md'. '.md' is added if missing." },
                    "content": { "type": "string", "description": "Full note content." },
                    "overwrite": { "type": "boolean", "description": "Replace the note if it already exists (default false)." }
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "append_to_note",
            "description": "Append text to an existing note, optionally under a specific '## heading' (the heading is created if missing). Prefer this over a raw Edit/Write — it inserts under the right heading and preserves the note's frontmatter and task syntax. Use for adding to a project log, journal, or notes section.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Target note: a roost path or a title." },
                    "text": { "type": "string", "description": "Text to append (markdown allowed)." },
                    "heading": { "type": "string", "description": "Optional H2 section to append under, e.g. 'Notes'. Without it, text is appended to the end of the note." }
                },
                "required": ["note", "text"]
            },
            "annotations": { "destructiveHint": false, "idempotentHint": false }
        },
        {
            "name": "add_task",
            "description": "Add a to-do. Defaults to TODAY and goes to today's daily note (in Corvux a checkbox with no date isn't a task, so undated to-dos are stamped with today's date); pass `due_date` for another day, or `note` to file under a specific note's task list. Resolve relative dates (\"tomorrow\", \"Friday\") to YYYY-MM-DD yourself first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Task text. May include #tags and [[wikilinks]]." },
                    "due_date": { "type": "string", "description": "Optional due date as YYYY-MM-DD." },
                    "due_time": { "type": "string", "description": "Optional 24-hour time HH:MM. Only valid with due_date. Adding a time makes it notify (see add_reminder)." },
                    "note": { "type": "string", "description": "Optional existing note (path or title) to add the task under, e.g. a project note." }
                },
                "required": ["text"]
            },
            "annotations": { "destructiveHint": false, "idempotentHint": false }
        },
        {
            "name": "add_reminder",
            "description": "Set a reminder that fires an OS notification at a date and time. In Corvux a reminder is a task with both a date and a time — this tool writes exactly that. Use for 'remind me at ...'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "What to be reminded about." },
                    "date": { "type": "string", "description": "Date as YYYY-MM-DD." },
                    "time": { "type": "string", "description": "24-hour time HH:MM." },
                    "note": { "type": "string", "description": "Optional existing note (path or title) to file under. Defaults to today's daily note." }
                },
                "required": ["text", "date", "time"]
            },
            "annotations": { "destructiveHint": false, "idempotentHint": false }
        },
        {
            "name": "recall",
            "description": "One-call memory retrieval — use this FIRST when you need what's already known about a topic ('what do we know about X', starting a work session, before re-deciding something). It keyword-searches the roost, then walks the wiki-link graph one hop out from the top matches, so it also returns linked notes that are related but worded differently (which search_roost alone would miss). Returns direct matches + related notes with snippets; follow up with read_note / note_context on the ones that matter.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic": { "type": "string", "description": "Keywords to recall (multi-word = AND, every word must appear in a match)." },
                    "project": { "type": "string", "description": "Optional project scope. A note is in scope when it has a matching `project:` property, carries the project as a tag, or the name appears in its path (case-insensitive)." },
                    "limit": { "type": "integer", "description": "Max direct matches to expand from (default 5, max 15)." }
                },
                "required": ["topic"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "search_roost",
            "description": "Full-text search across note bodies + titles. Multi-term AND — every whitespace-separated word must appear. Ranked by title hits then term frequency; each result carries a short snippet. Optional `folder` and `tag` filters narrow the set first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "One or more words; ALL must appear (AND)." },
                    "limit": { "type": "integer", "description": "Max results (default 10, max 50)." },
                    "folder": { "type": "string", "description": "Optional: only notes whose roost path starts with this folder." },
                    "tag": { "type": "string", "description": "Optional: only notes carrying this tag (frontmatter or inline #tag)." }
                },
                "required": ["query"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "find_notes",
            "description": "Find notes by title or path (not body). Use to locate a note before reading or appending. Case-insensitive substring match on the path.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Part of a note's title or path." },
                    "limit": { "type": "integer", "description": "Max results (default 20, max 100)." }
                },
                "required": ["query"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "find_by_property",
            "description": "Find notes by a property (frontmatter field) — e.g. status=draft, type=meeting, or a tag. Matches scalar values and list membership; for key 'tag'/'tags' it also matches inline #tags in the body. Case-insensitive.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Property name, e.g. 'status', 'type', or 'tag'." },
                    "value": { "type": "string", "description": "Value to match, e.g. 'draft', 'meeting', 'project'." },
                    "limit": { "type": "integer", "description": "Max results (default 50, max 200)." }
                },
                "required": ["key", "value"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "read_note",
            "description": "Read a single note by roost-relative path or by title (filename without extension). Prefer this over a raw file Read — it also surfaces the note's Properties (YAML frontmatter) block and resolves the note through the roost's path/title lookup.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Roost-relative path, e.g. 'Daily Notes/2026-06-01.md'." },
                    "title": { "type": "string", "description": "Note title (filename without .md). Used if path is omitted." }
                }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "note_context",
            "description": "Everything about one note in a single call: its properties (frontmatter), a short body snippet, its tags, its backlinks (notes that link TO it) and its forward links (notes it links to). Prefer this over chaining read_note + get_backlinks + get_forward_links when you want to understand a note and where it sits in the graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Target note: a roost path or a title." },
                    "limit": { "type": "integer", "description": "Max backlinks and forward links to list each (default 20, max 100)." }
                },
                "required": ["note"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "list_open_tasks",
            "description": "List unchecked tasks across the roost, grouped Overdue / Today / Upcoming / No date (most urgent first). Optionally filter to a due date ('today' or YYYY-MM-DD). Notes with `tasks: off` in their frontmatter are skipped.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "date": { "type": "string", "description": "Optional: 'today' or YYYY-MM-DD to show only tasks due that day." },
                    "limit": { "type": "integer", "description": "Max tasks (default 50, max 200)." }
                }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "complete_task",
            "description": "Mark an open task complete (flips [ ] to [x] and stamps the completion date). Identifies the task by a unique text fragment within a given note; errors if zero or multiple tasks match. Only use when the user asks to complete something.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "The note (path or title) containing the task." },
                    "text": { "type": "string", "description": "A distinctive fragment of the task's text to match." }
                },
                "required": ["note", "text"]
            },
            "annotations": { "idempotentHint": false }
        },
        {
            "name": "set_property",
            "description": "Set or update a single frontmatter property on a note (e.g. status=done, project=Atlas). Edits only the frontmatter; the note body is preserved. Creates frontmatter if missing. For tags, use add_tags instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "The note (path or title)." },
                    "key": { "type": "string", "description": "Property name, e.g. 'status'." },
                    "value": { "type": "string", "description": "Property value, e.g. 'done'." }
                },
                "required": ["note", "key", "value"]
            },
            "annotations": { "destructiveHint": true, "idempotentHint": true }
        },
        {
            "name": "add_tags",
            "description": "Add one or more tags to a note's frontmatter (deduplicated). Creates frontmatter / a tags list if missing. Preserves the note body.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "The note (path or title)." },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags to add (a leading # is stripped)." }
                },
                "required": ["note", "tags"]
            },
            "annotations": { "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "reschedule_task",
            "description": "Change an open task's due date and/or time. Identifies the task by a unique text fragment within a note. Pass a YYYY-MM-DD `date` and/or HH:MM `time` to set them, or the literal \"clear\" to remove a token. Errors if zero or multiple tasks match.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "The note (path or title) containing the task." },
                    "text": { "type": "string", "description": "A distinctive fragment of the task's text." },
                    "date": { "type": "string", "description": "New due date YYYY-MM-DD, or \"clear\" to remove. Omit to leave unchanged." },
                    "time": { "type": "string", "description": "New due time HH:MM (24h), or \"clear\". Omit to leave unchanged." }
                },
                "required": ["note", "text"]
            },
            "annotations": { "idempotentHint": false }
        },
        {
            "name": "list_recent_notes",
            "description": "List notes modified in the last N days, newest first. Use for 'what did I work on this week' / 'recent notes'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "days": { "type": "integer", "description": "Look-back window in days (default 7)." },
                    "limit": { "type": "integer", "description": "Max results (default 20, max 100)." }
                }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "roost_stats",
            "description": "Summary counts for the roost: notes, open tasks, folders, and resolved wiki-links.",
            "inputSchema": { "type": "object", "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "get_backlinks",
            "description": "List notes that link TO a given note via [[wiki-links]] (incoming links). Answers 'what references X'. Capped at `limit` (default 100) for highly-referenced notes; the reply says how many were omitted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "The note (path or title)." },
                    "limit": { "type": "integer", "description": "Max links to return (default 100, max 1000)." }
                },
                "required": ["note"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "get_forward_links",
            "description": "List notes a given note links TO via [[wiki-links]] (outgoing links). Capped at `limit` (default 100); the reply says how many were omitted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "The note (path or title)." },
                    "limit": { "type": "integer", "description": "Max links to return (default 100, max 1000)." }
                },
                "required": ["note"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "get_neighbors",
            "description": "BFS the wiki-link graph out from a note to `depth` hops (links followed in either direction). Returns each reachable note with its hop distance. Use to explore a note's neighbourhood.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "The focus note (path or title)." },
                    "depth": { "type": "integer", "description": "Hops to expand (default 1, max 4)." }
                },
                "required": ["note"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "find_path",
            "description": "Find the shortest [[wiki-link]] path between two notes (links treated as undirected). Answers 'how is X connected to Y'. Returns the chain of notes, or reports no connection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "Start note (path or title)." },
                    "to": { "type": "string", "description": "End note (path or title)." }
                },
                "required": ["from", "to"]
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "top_hubs",
            "description": "Rank the roost's hub notes by PageRank over the wiki-link graph — the notes everything connects through. Use for 'what are my most important / central notes'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "How many hubs to return (default 10, max 100)." }
                }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "list_tags",
            "description": "List the roost's existing tags with use counts, most-used first. CALL THIS BEFORE tagging a note (via capture_note / add_tags) so you reuse an established tag instead of inventing a near-duplicate (#ml vs #machine-learning) that fragments the knowledge graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "How many tags to return (default 100, max 500)." }
                }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "find_broken_links",
            "description": "Audit every [[wiki-link]] in the roost and list the broken ones: dangling (the target note doesn't exist) or ambiguous (the name matches several notes, so the graph guesses and may link the wrong pair). Use to clean up existing links — 'find my broken links', 'are any links broken'. Fix dangling links by creating the note or correcting the name; fix ambiguous ones by using a full path [[Folder/Note]].",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "How many broken links to return (default 100, max 500)." }
                }
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "suggest_links",
            "description": "Propose MISSING [[wiki-links]]: finds pairs of notes that look related — one mentions the other's title as plain text, or both carry the same rare tag — but have NO link between them. The twin of find_broken_links (that audits bad links; this finds absent ones). More links directly improve recall, which walks the link graph. Read-only: to act on a suggestion, confirm with the user, then add a `Related: [[Target]]` line via append_to_note. Pass `note` to garden one specific note (e.g. right after writing it); omit it for a roost-wide pass.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "note": { "type": "string", "description": "Optional: only suggest links involving this note (path or title)." },
                    "limit": { "type": "integer", "description": "Max suggestions (default 20, max 100)." }
                }
            },
            "annotations": { "readOnlyHint": true }
        }
    ]);
    const STRUCTURED_TOOLS: &[&str] = &[
        "recall",
        "search_roost",
        "find_notes",
        "find_by_property",
        "note_context",
        "list_open_tasks",
        "list_recent_notes",
        "roost_stats",
        "list_tags",
        "get_backlinks",
        "get_forward_links",
        "get_neighbors",
        "find_path",
        "top_hubs",
        "find_broken_links",
        "suggest_links",
    ];
    if let Some(items) = tools.as_array_mut() {
        for tool in items {
            if let Some(schema) = tool.get_mut("inputSchema").and_then(Value::as_object_mut) {
                schema.insert("additionalProperties".to_string(), Value::Bool(false));
            }
            let annotations = tool
                .as_object_mut()
                .expect("tool spec is an object")
                .entry("annotations")
                .or_insert_with(|| json!({}));
            annotations["openWorldHint"] = Value::Bool(false);
            if tool
                .get("name")
                .and_then(Value::as_str)
                .map(|name| STRUCTURED_TOOLS.contains(&name))
                .unwrap_or(false)
            {
                tool["outputSchema"] = json!({ "type": "object" });
            }
        }
    }
    tools
}

// ── Small arg helpers ───────────────────────────────────────────────────

/// A note rel's folder portion, lowercased (no filename). `""` for root.
/// Used as the `source_dir` for folder-proximity link resolution.
fn note_dir(rel: &str) -> String {
    rel.rsplit_once('/')
        .map(|(d, _)| d.to_lowercase())
        .unwrap_or_default()
}

fn arg_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required argument `{key}`"))
}

fn arg_opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

/// Read a JSON string array argument (e.g. `tags`), dropping blanks.
fn arg_str_list(args: &Value, key: &str) -> Result<Vec<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(arr)) => Ok(arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()),
        _ => Err(format!("`{key}` must be an array of strings")),
    }
}

fn cap(s: &str) -> String {
    if s.len() <= READ_CAP {
        return s.to_string();
    }
    let mut end = READ_CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[... truncated at {READ_CAP} chars ...]", &s[..end])
}

/// Make a string safe to use as a filename: strip path separators and
/// characters Windows/macOS reject, collapse whitespace.
fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches('.').trim();
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else {
        trimmed.chars().take(120).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn server() -> (TempDir, Server) {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        let v = Roost::open(dir.path().to_path_buf()).unwrap();
        let s = Server::new(v);
        initialize(&s);
        (dir, s)
    }

    /// Send `initialize` so subsequent `tools/call`/`tools/list` pass the
    /// lifecycle gate (Cluster F) — every test server needs this once.
    fn initialize(s: &Server) {
        s.handle(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}))
            .unwrap();
    }

    fn call(s: &Server, name: &str, args: Value) -> Value {
        s.handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":name,"arguments":args}}))
            .unwrap()["result"]
            .clone()
    }
    fn text_of(result: &Value) -> String {
        result["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn initialize_and_list_all_tools() {
        let (_d, s) = server();
        // A supported version is echoed verbatim.
        let init = s
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("second brain"));

        let list = s
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 26);
    }

    #[test]
    fn initialize_negotiates_unsupported_version_to_latest() {
        let (_d, s) = server();
        // An unknown version gets our latest, not the client's.
        let init = s
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1.0.0"}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    }

    #[test]
    fn read_tools_are_annotated_read_only() {
        let (_d, s) = server();
        let list = s
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap().clone();
        let find = |name: &str| {
            tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .clone()
        };
        for name in [
            "recall",
            "search_roost",
            "read_note",
            "get_backlinks",
            "top_hubs",
            "suggest_links",
        ] {
            assert_eq!(
                find(name)["annotations"]["readOnlyHint"],
                true,
                "{name} should be readOnlyHint"
            );
        }
    }

    #[test]
    fn capture_defaults_to_daily_note() {
        let (_d, s) = server();
        let (today, _) = today_local();
        let r = call(
            &s,
            "capture_note",
            json!({"content":"a quick thought","title":"Idea"}),
        );
        assert_eq!(r["isError"], false);
        let daily = s.roost.read(&format!("Daily Notes/{today}.md")).unwrap();
        assert!(daily.contains("## Notes"));
        assert!(daily.contains("### Idea"));
        assert!(daily.contains("a quick thought"));
        assert!(!s.roost.exists("Captured/Idea.md")); // not standalone
    }

    #[test]
    fn capture_then_search_find_read() {
        let (_d, s) = server();
        // Pass a folder to force a standalone note (default goes to the daily note).
        call(
            &s,
            "capture_note",
            json!({"content":"atomic writes matter","title":"MCP chat","folder":"Captured"}),
        );
        assert!(s.roost.exists("Captured/MCP chat.md"));

        let search = call(&s, "search_roost", json!({"query":"atomic"}));
        assert!(text_of(&search).contains("Captured/MCP chat.md"));

        let find = call(&s, "find_notes", json!({"query":"mcp"}));
        assert!(text_of(&find).contains("Captured/MCP chat.md"));

        let read = call(&s, "read_note", json!({"title":"MCP chat"}));
        assert!(text_of(&read).contains("atomic writes matter"));
    }

    #[test]
    fn recall_pulls_in_linked_notes_missing_the_keyword() {
        let (_d, s) = server();
        // "Session bug" never says "login" — only the wiki-link connects it.
        call(
            &s,
            "write_note",
            json!({"path":"Auth flow.md","content":"# Auth flow\n\nThe login design uses tokens. Related: [[Session bug]]\n"}),
        );
        call(
            &s,
            "write_note",
            json!({"path":"Session bug.md","content":"# Session bug\n\nTimeout after refresh; cookies dropped.\n"}),
        );
        call(
            &s,
            "write_note",
            json!({"path":"Unrelated.md","content":"# Unrelated\n\ngardening notes\n"}),
        );

        let r = call(&s, "recall", json!({"topic":"login"}));
        assert_eq!(r["isError"], false);
        let text = text_of(&r);
        assert!(text.contains("Auth flow.md"));
        assert!(text.contains("Session bug.md"), "linked note must surface");
        assert!(!text.contains("Unrelated.md"));

        let sc = &r["structuredContent"];
        assert_eq!(sc["seeds"][0]["path"], "Auth flow.md");
        assert_eq!(sc["related"][0]["path"], "Session bug.md");
        assert_eq!(sc["related"][0]["via"][0], "Auth flow.md");
    }

    #[test]
    fn recall_project_scope_filters_by_path_and_property() {
        let (_d, s) = server();
        call(
            &s,
            "write_note",
            json!({"path":"Corvux/Sync design.md","content":"# Sync design\n\nsync uses iroh\n"}),
        );
        call(
            &s,
            "write_note",
            json!({"path":"Notes/Old sync.md","content":"---\nproject: hrms\n---\n\n# Old sync\n\nsync via cron\n"}),
        );

        let r = call(&s, "recall", json!({"topic":"sync","project":"corvux"}));
        let text = text_of(&r);
        assert!(text.contains("Corvux/Sync design.md"));
        assert!(!text.contains("Old sync.md"));

        let r2 = call(&s, "recall", json!({"topic":"sync","project":"hrms"}));
        let text2 = text_of(&r2);
        assert!(text2.contains("Notes/Old sync.md"));
        assert!(!text2.contains("Sync design.md"));
    }

    #[test]
    fn recall_no_match_is_friendly_not_error() {
        let (_d, s) = server();
        call(
            &s,
            "write_note",
            json!({"path":"A.md","content":"# A\n\nsomething\n"}),
        );
        let r = call(&s, "recall", json!({"topic":"zebrasaurus"}));
        assert_eq!(r["isError"], false);
        assert!(text_of(&r).contains("Nothing recalled"));
    }

    #[test]
    fn recall_rejects_empty_or_missing_topic() {
        let (_d, s) = server();
        let r = call(&s, "recall", json!({"topic":"   "}));
        assert_eq!(r["isError"], true);
        let r2 = call(&s, "recall", json!({}));
        assert_eq!(r2["isError"], true);
    }

    #[test]
    fn recall_seeds_carry_properties_and_tags() {
        let (_d, s) = server();
        call(
            &s,
            "write_note",
            json!({"path":"Decisions/Auth.md","content":"---\ntype: decision\nstatus: final\ntags:\n  - security\n---\n\n# Auth\n\nlogin uses tokens\n"}),
        );
        let r = call(&s, "recall", json!({"topic":"login"}));
        let sc = &r["structuredContent"];
        assert_eq!(sc["seeds"][0]["properties"]["type"], "decision");
        assert_eq!(sc["seeds"][0]["properties"]["status"], "final");
        assert_eq!(sc["seeds"][0]["tags"][0], "security");
        assert!(sc["related"].as_array().unwrap().is_empty());
    }

    #[test]
    fn suggest_links_proposes_mention_and_rare_tag_pairs() {
        let (_d, s) = server();
        call(
            &s,
            "write_note",
            json!({"path":"Auth flow.md","content":"# Auth flow\n\nWe hit the session bug again during login.\n"}),
        );
        call(
            &s,
            "write_note",
            json!({"path":"Session bug.md","content":"# Session bug\n\nTimeout after refresh.\n"}),
        );
        call(
            &s,
            "write_note",
            json!({"path":"P1.md","content":"---\ntags:\n  - quic\n---\n\n# P1\n"}),
        );
        call(
            &s,
            "write_note",
            json!({"path":"P2.md","content":"---\ntags:\n  - quic\n---\n\n# P2\n"}),
        );

        let r = call(&s, "suggest_links", json!({}));
        assert_eq!(r["isError"], false);
        let text = text_of(&r);
        assert!(text.contains("Auth flow.md"));
        assert!(text.contains("Session bug.md"));
        assert!(text.contains("mentions the title"));
        assert!(text.contains("shared rare tags: #quic"));

        let sc = &r["structuredContent"];
        assert_eq!(sc["suggestions"][0]["source"], "Auth flow.md");
        assert_eq!(sc["suggestions"][0]["target"], "Session bug.md");
        assert_eq!(sc["suggestions"][0]["mentioned"], true);
        assert_eq!(sc["suggestions"][1]["shared_tags"][0]["tag"], "quic");

        let scoped = call(&s, "suggest_links", json!({"note":"P1"}));
        let sc2 = &scoped["structuredContent"];
        assert_eq!(sc2["suggestions"].as_array().unwrap().len(), 1);
        assert_eq!(sc2["suggestions"][0]["source"], "P1.md");

        // Acting on the suggestion makes it disappear.
        call(
            &s,
            "append_to_note",
            json!({"note":"Auth flow.md","text":"Related: [[Session bug]]"}),
        );
        let after = call(&s, "suggest_links", json!({}));
        assert!(!text_of(&after).contains("Auth flow.md"));
    }

    #[test]
    fn suggest_links_no_findings_is_friendly_and_unknown_note_errors() {
        let (_d, s) = server();
        call(
            &s,
            "write_note",
            json!({"path":"Solo.md","content":"# Solo\n\nnothing related here\n"}),
        );
        let r = call(&s, "suggest_links", json!({}));
        assert_eq!(r["isError"], false);
        assert!(text_of(&r).contains("No missing links suggested"));

        let bad = call(&s, "suggest_links", json!({"note":"Ghost"}));
        assert_eq!(bad["isError"], true);
    }

    #[test]
    fn note_context_bundles_properties_links_and_snippet() {
        let (_d, s) = server();
        // Hub has properties + tags, is linked-to by A, and links out to Leaf.
        call(
            &s,
            "write_note",
            json!({"path":"Hub.md","content":"---\nstatus: active\ntags:\n  - core\n---\n\n# Hub\n\nThe central idea, which links to [[Leaf]].\n"}),
        );
        call(
            &s,
            "write_note",
            json!({"path":"A.md","content":"see [[Hub]]\n"}),
        );
        call(
            &s,
            "write_note",
            json!({"path":"Leaf.md","content":"leaf note\n"}),
        );

        let r = call(&s, "note_context", json!({"note":"Hub"}));
        assert_eq!(r["isError"], false);
        let txt = text_of(&r);
        assert!(txt.contains("Hub.md"));
        assert!(txt.contains("status: active"), "properties missing:\n{txt}");
        assert!(txt.contains("core"), "tags missing:\n{txt}");
        assert!(txt.contains("A.md"), "backlink missing:\n{txt}");
        assert!(txt.contains("Leaf.md"), "forward link missing:\n{txt}");

        // Structured payload carries the same directional link sets.
        let sc = &r["structuredContent"];
        assert_eq!(sc["backlinks"]["total"], 1);
        assert_eq!(sc["forward_links"]["total"], 1);
    }

    #[test]
    fn tasks_reminders_and_completion() {
        let (_d, s) = server();
        let (today, _) = today_local();

        // Undated add_task defaults to today (0.3: a dateless checkbox isn't a
        // task) → lands in today's daily note, stamped with today's date.
        call(&s, "add_task", json!({"text":"Buy milk"}));
        let daily_path = format!("Daily Notes/{today}.md");
        let daily = s.roost.read(&daily_path).unwrap();
        assert!(daily.contains(&format!("- [ ] Buy milk 📅 {today}")));

        // Reminder -> today's daily note, with date+time.
        let r = call(
            &s,
            "add_reminder",
            json!({"text":"Standup","date":today,"time":"10:00"}),
        );
        assert_eq!(r["isError"], false);
        let daily = s.roost.read(&daily_path).unwrap();
        assert!(daily.contains(&format!("- [ ] Standup 📅 {today} ⏰ 10:00")));

        // Complete the task in the daily note.
        let c = call(
            &s,
            "complete_task",
            json!({"note": daily_path, "text":"Buy milk"}),
        );
        assert_eq!(c["isError"], false);
        let daily = s.roost.read(&daily_path).unwrap();
        assert!(daily.contains(&format!("- [x] Buy milk 📅 {today} ✅ {today}")));
    }

    #[test]
    fn task_routed_to_named_note() {
        let (_d, s) = server();
        s.roost
            .write_atomic("Projects/Atlas.md", "# Atlas\n\n## Tasks\n\n")
            .unwrap();
        let r = call(&s, "add_task", json!({"text":"Review spec","note":"Atlas"}));
        assert_eq!(r["isError"], false);
        assert!(s
            .roost
            .read("Projects/Atlas.md")
            .unwrap()
            .contains("- [ ] Review spec"));
    }

    #[test]
    fn list_open_tasks_orders_dated_first_with_counts() {
        let (_d, s) = server();
        // Distinct, non-overlapping task text so substring finds are exact.
        // Under 0.3 a no-date task can only come from a frontmatter `tasks:`
        // entry (body checkboxes always resolve to a date); b's body task
        // carries an inline 📅.
        s.roost
            .write_atomic("a.md", "---\ntasks:\n  - text: \"alpha no date\"\n---\n")
            .unwrap();
        s.roost
            .write_atomic("b.md", "## Tasks\n\n- [ ] beta has date 📅 2026-06-05\n")
            .unwrap();
        let r = call(&s, "list_open_tasks", json!({}));
        assert_eq!(r["isError"], false);
        let t = text_of(&r);
        // Header carries the grouped total. (Clock-independent assertions
        // only — beta's exact bucket depends on today's date.)
        assert!(t.contains("2 open task(s)"), "header missing total:\n{t}");
        assert!(
            t.contains("no-date 1"),
            "header missing no-date count:\n{t}"
        );
        // A dated task always sorts ahead of an undated one (any dated bucket
        // precedes No date), regardless of the wall clock.
        let beta = t.find("beta has date").unwrap();
        let alpha = t.find("alpha no date").unwrap();
        assert!(beta < alpha, "dated task should be listed first:\n{t}");
        // structuredContent mirrors the text.
        assert_eq!(r["structuredContent"]["counts"]["total"], 2);
    }

    #[test]
    fn graph_tools_backlinks_path_and_hubs() {
        let (_d, s) = server();
        s.roost
            .write_atomic("Atlas.md", "# Atlas\n\nsee [[Beta]]\n")
            .unwrap();
        s.roost
            .write_atomic("Beta.md", "# Beta\n\nsee [[Gamma]]\n")
            .unwrap();
        s.roost.write_atomic("Gamma.md", "# Gamma\n").unwrap();

        // Backlinks: Beta is linked from Atlas.
        let back = call(&s, "get_backlinks", json!({"note":"Beta"}));
        assert_eq!(back["isError"], false);
        assert!(text_of(&back).contains("Atlas.md"));
        assert_eq!(back["structuredContent"]["links"][0], "Atlas.md");

        // Forward links: Atlas -> Beta.
        let fwd = call(&s, "get_forward_links", json!({"note":"Atlas"}));
        assert!(text_of(&fwd).contains("Beta.md"));

        // Path Atlas -> Gamma is 2 hops through Beta.
        let path = call(&s, "find_path", json!({"from":"Atlas","to":"Gamma"}));
        assert_eq!(path["structuredContent"]["hops"], 2);
        assert!(text_of(&path).contains("Atlas.md → Beta.md → Gamma.md"));

        // Hubs: Beta is the most central (linked + linking).
        let hubs = call(&s, "top_hubs", json!({"limit":3}));
        assert_eq!(hubs["isError"], false);
        assert!(
            hubs["structuredContent"]["results"]
                .as_array()
                .unwrap()
                .len()
                <= 3
        );

        // Unknown note errors cleanly.
        let bad = call(&s, "get_backlinks", json!({"note":"Nope"}));
        assert_eq!(bad["isError"], true);
    }

    #[test]
    fn get_backlinks_caps_output_and_reports_omitted() {
        // CO-1: a hub with many backlinks must not flood the LLM. With a small
        // `limit`, the reply is capped and announces how many were omitted.
        let (_d, s) = server();
        s.roost.write_atomic("Hub.md", "# Hub\n").unwrap();
        for i in 0..25 {
            s.roost
                .write_atomic(&format!("ref-{i:02}.md"), "see [[Hub]]\n")
                .unwrap();
        }
        let r = call(&s, "get_backlinks", json!({"note": "Hub", "limit": 5}));
        assert_eq!(r["isError"], false);
        let links = r["structuredContent"]["links"].as_array().unwrap();
        assert_eq!(links.len(), 5, "structured payload capped to the limit");
        assert_eq!(r["structuredContent"]["total"], 25);
        assert_eq!(r["structuredContent"]["truncated"], true);
        assert!(
            text_of(&r).contains("showing first 5 of 25"),
            "reply must announce the omission:\n{}",
            text_of(&r)
        );
    }

    #[test]
    fn recent_and_stats_tools() {
        let (_d, s) = server();
        s.roost
            .write_atomic("A.md", "# A\n\n- [ ] task 📅 2026-06-12\n[[B]]\n")
            .unwrap();
        s.roost.write_atomic("B.md", "# B\n").unwrap();

        let recent = call(&s, "list_recent_notes", json!({"days":30}));
        assert_eq!(recent["isError"], false);
        assert!(
            recent["structuredContent"]["results"]
                .as_array()
                .unwrap()
                .len()
                >= 2
        );

        let stats = call(&s, "roost_stats", json!({}));
        assert_eq!(stats["structuredContent"]["notes"], 2);
        assert_eq!(stats["structuredContent"]["open_tasks"], 1);
        assert_eq!(stats["structuredContent"]["links"], 1);
    }

    #[test]
    fn reschedule_task_updates_tokens() {
        let (_d, s) = server();
        s.roost
            .write_atomic("Projects/P.md", "## Tasks\n\n- [ ] ship it 📅 2026-06-05\n")
            .unwrap();
        let r = call(
            &s,
            "reschedule_task",
            json!({"note":"P","text":"ship it","date":"2026-06-09","time":"09:30"}),
        );
        assert_eq!(r["isError"], false, "{}", text_of(&r));
        let body = s.roost.read("Projects/P.md").unwrap();
        assert!(
            body.contains("- [ ] ship it 📅 2026-06-09 ⏰ 09:30"),
            "{body}"
        );

        // Bad date is rejected.
        let bad = call(
            &s,
            "reschedule_task",
            json!({"note":"P","text":"ship it","date":"06/09/2026"}),
        );
        assert_eq!(bad["isError"], true);
    }

    #[test]
    fn list_open_tasks_honors_tasks_off() {
        let (_d, s) = server();
        s.roost
            .write_atomic(
                "Plan.md",
                "---\ntasks: off\n---\n\n- [ ] doc checkbox 📅 2026-06-12\n",
            )
            .unwrap();
        s.roost
            .write_atomic("Real.md", "## Tasks\n\n- [ ] actual todo 📅 2026-06-12\n")
            .unwrap();
        let r = call(&s, "list_open_tasks", json!({}));
        let t = text_of(&r);
        assert!(t.contains("actual todo"), "{t}");
        assert!(
            !t.contains("doc checkbox"),
            "tasks: off note must be hidden:\n{t}"
        );
    }

    #[test]
    fn write_note_guards_overwrite_and_reserved() {
        let (_d, s) = server();
        call(&s, "write_note", json!({"path":"Note","content":"hi"}));
        assert!(s.roost.exists("Note.md"));
        // Second write without overwrite is rejected.
        let again = call(&s, "write_note", json!({"path":"Note","content":"bye"}));
        assert_eq!(again["isError"], true);
        // Reserved path is refused.
        let reserved = call(
            &s,
            "write_note",
            json!({"path":".corvux/evil.md","content":"x"}),
        );
        assert_eq!(reserved["isError"], true);
    }

    #[test]
    fn append_to_note_under_heading() {
        let (_d, s) = server();
        s.roost
            .write_atomic("Log.md", "# Log\n\n## Notes\n\n## Journal\n")
            .unwrap();
        call(
            &s,
            "append_to_note",
            json!({"note":"Log","text":"entry one","heading":"Journal"}),
        );
        let log = s.roost.read("Log.md").unwrap();
        assert!(log.contains("## Journal\n\nentry one"));
    }

    #[test]
    fn add_task_rejects_lone_time() {
        let (_d, s) = server();
        let r = call(&s, "add_task", json!({"text":"x","due_time":"10:00"}));
        assert_eq!(r["isError"], true);
    }

    #[test]
    fn tools_refuse_non_corvux_folder() {
        let dir = TempDir::new().unwrap(); // no .corvux/
        let s = Server::new(Roost::open(dir.path().to_path_buf()).unwrap());
        initialize(&s);
        let r = call(&s, "add_task", json!({"text":"x"}));
        assert_eq!(r["isError"], true);
        assert!(text_of(&r).contains("not a Corvux roost"));
    }

    #[test]
    fn capture_with_properties_is_standalone_and_findable() {
        let (_d, s) = server();
        // Tags/properties force a standalone note with real frontmatter.
        let r = call(
            &s,
            "capture_note",
            json!({"content":"meeting notes","title":"Sync","tags":["project"],"properties":{"type":"meeting","status":"open"}}),
        );
        assert_eq!(r["isError"], false);
        assert!(s.roost.exists("Captured/Sync.md"));
        let note = s.roost.read("Captured/Sync.md").unwrap();
        assert!(note.contains("type: meeting"));
        assert!(note.contains("status: open"));
        assert!(note.contains("- project"));

        // Findable by property.
        let by_type = call(
            &s,
            "find_by_property",
            json!({"key":"type","value":"meeting"}),
        );
        assert!(text_of(&by_type).contains("Captured/Sync.md"));
        let by_tag = call(
            &s,
            "find_by_property",
            json!({"key":"tag","value":"project"}),
        );
        assert!(text_of(&by_tag).contains("Captured/Sync.md"));

        // read_note surfaces a Properties block.
        let read = call(&s, "read_note", json!({"title":"Sync"}));
        assert!(text_of(&read).contains("Properties:"));
        assert!(text_of(&read).contains("status: open"));
    }

    #[test]
    fn set_property_and_add_tags() {
        let (_d, s) = server();
        s.roost
            .write_atomic("Note.md", "---\ntype: note\n---\n\n# Note\n\nbody\n")
            .unwrap();

        let sp = call(
            &s,
            "set_property",
            json!({"note":"Note","key":"status","value":"done"}),
        );
        assert_eq!(sp["isError"], false);
        assert!(s.roost.read("Note.md").unwrap().contains("status: done"));

        let at = call(
            &s,
            "add_tags",
            json!({"note":"Note","tags":["alpha","beta"]}),
        );
        assert_eq!(at["isError"], false);
        let note = s.roost.read("Note.md").unwrap();
        assert!(note.contains("- alpha"));
        assert!(note.contains("- beta"));
        assert!(note.contains("# Note\n\nbody")); // body intact

        // set_property refuses tags.
        let bad = call(
            &s,
            "set_property",
            json!({"note":"Note","key":"tags","value":"x"}),
        );
        assert_eq!(bad["isError"], true);
    }

    #[test]
    fn list_tags_reports_existing_vocabulary() {
        let (_d, s) = server();
        s.roost
            .write_atomic("A.md", "---\ntags:\n  - sync\n  - project\n---\n\n# A\n")
            .unwrap();
        s.roost
            .write_atomic("B.md", "---\ntags: [sync]\n---\n\n# B\n")
            .unwrap();

        let res = call(&s, "list_tags", json!({}));
        assert_eq!(res["isError"], false);
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("#sync (2)"), "got: {text}");
        assert!(text.contains("#project (1)"), "got: {text}");
        // Structured payload mirrors the text.
        assert_eq!(res["structuredContent"]["total"], 2);
    }

    #[test]
    fn add_tags_snaps_case_variant_to_existing_spelling() {
        let (_d, s) = server();
        // Establish "#ml" as the existing vocabulary.
        s.roost
            .write_atomic("Seed.md", "---\ntags:\n  - ml\n---\n\n# Seed\n")
            .unwrap();
        s.roost.write_atomic("Note.md", "# Note\n\nbody\n").unwrap();

        // Caller passes "#ML" (wrong case) → should reuse "ml", not fork.
        let at = call(&s, "add_tags", json!({"note":"Note","tags":["ML"]}));
        assert_eq!(at["isError"], false);
        let note = s.roost.read("Note.md").unwrap();
        assert!(note.contains("- ml"), "should snap to existing #ml: {note}");
        assert!(
            !note.contains("- ML"),
            "must not fork a #ML variant: {note}"
        );
    }

    #[test]
    fn capture_note_snaps_tag_case_to_existing() {
        let (_d, s) = server();
        s.roost
            .write_atomic("Seed.md", "---\ntags:\n  - project/corvux\n---\n\n# Seed\n")
            .unwrap();

        // Capturing with a case-variant tag reuses the established spelling.
        let cap = call(
            &s,
            "capture_note",
            json!({"content":"a thought","title":"T","tags":["Project/Corvux"]}),
        );
        assert_eq!(cap["isError"], false);
        let rel = cap["content"][0]["text"]
            .as_str()
            .unwrap()
            .trim_start_matches("Captured to ")
            .to_string();
        let note = s.roost.read(&rel).unwrap();
        assert!(note.contains("- project/corvux"), "got: {note}");
        assert!(!note.contains("- Project/Corvux"), "got: {note}");
    }

    #[test]
    fn find_broken_links_audits_and_write_tools_warn() {
        let (_d, s) = server();
        s.roost
            .write_atomic("Hub.md", "# Hub\n\nlink to [[Missing]]\n")
            .unwrap();

        // Audit surfaces the dangling link.
        let res = call(&s, "find_broken_links", json!({}));
        assert_eq!(res["isError"], false);
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("[[Missing]]"), "got: {text}");
        assert_eq!(res["structuredContent"]["broken"][0]["kind"], "dangling");

        // A write tool warns inline about a dangling link in the new content.
        let w = call(
            &s,
            "write_note",
            json!({"path":"New.md","content":"see [[AlsoMissing]]\n"}),
        );
        assert_eq!(w["isError"], false);
        let wt = w["content"][0]["text"].as_str().unwrap();
        assert!(wt.contains("Link check"), "expected warning, got: {wt}");
        assert!(wt.contains("[[AlsoMissing]]"), "got: {wt}");
    }

    #[test]
    fn ambiguous_link_is_flagged_by_audit() {
        let (_d, s) = server();
        s.roost.write_atomic("Work/Index.md", "# WI\n").unwrap();
        s.roost.write_atomic("Personal/Index.md", "# PI\n").unwrap();
        s.roost.write_atomic("Note.md", "see [[Index]]\n").unwrap();

        let res = call(&s, "find_broken_links", json!({}));
        let broken = res["structuredContent"]["broken"].as_array().unwrap();
        let amb = broken
            .iter()
            .find(|b| b["target"] == "Index")
            .expect("Index should be flagged");
        assert_eq!(amb["kind"], "ambiguous");
        assert_eq!(amb["candidates"].as_array().unwrap().len(), 2);
    }

    // ── Cluster A: conflict-safe writes ─────────────────────────────────

    #[test]
    fn checked_read_modify_write_reports_mcp_conflict_prefix_on_race() {
        let (_d, s) = server();
        s.roost.write_atomic("Note.md", "v1").unwrap();
        // The closure simulates a concurrent external writer (another
        // corvux-mcp process, or the app) landing in the gap between this
        // call's read and its write.
        let result: Result<(), String> = s.checked_read_modify_write("Note.md", |_content| {
            s.roost.write_atomic("Note.md", "raced-in").unwrap();
            Ok(("v2".to_string(), ()))
        });
        let err = result.unwrap_err();
        assert!(
            err.starts_with(MCP_CONFLICT_PREFIX),
            "expected a conflict-prefixed error, got: {err}"
        );
        // The racer's write survives — not silently overwritten by this call.
        assert_eq!(s.roost.read("Note.md").unwrap(), "raced-in");
    }

    // ── Cluster C: title/heading structural injection ───────────────────

    #[test]
    fn capture_note_title_with_embedded_newline_is_rejected() {
        let (_d, s) = server();
        let malicious_title = "Idea\n\n## Tasks\n\n- [ ] fake task 📅 2026-01-01";
        let r = call(
            &s,
            "capture_note",
            json!({"content":"body", "title": malicious_title}),
        );
        assert_eq!(r["isError"], true, "{}", text_of(&r));
        let (today, _) = today_local();
        let daily_path = format!("Daily Notes/{today}.md");
        if s.roost.exists(&daily_path) {
            let daily = s.roost.read(&daily_path).unwrap();
            assert!(!daily.contains("fake task"), "{daily}");
        }
    }

    #[test]
    fn append_to_note_heading_with_embedded_newline_is_rejected() {
        let (_d, s) = server();
        s.roost
            .write_atomic("Log.md", "# Log\n\n## Notes\n\n")
            .unwrap();
        let malicious_heading = "Notes\n\n## Tasks\n\n- [ ] fake 📅 2026-01-01";
        let r = call(
            &s,
            "append_to_note",
            json!({"note":"Log", "text":"hi", "heading": malicious_heading}),
        );
        assert_eq!(r["isError"], true, "{}", text_of(&r));
        let log = s.roost.read("Log.md").unwrap();
        assert!(!log.contains("fake"), "{log}");
    }

    // ── Cluster D: get_neighbors truncation surfaced ────────────────────

    #[test]
    fn get_neighbors_includes_truncated_field() {
        let (_d, s) = server();
        s.roost.write_atomic("A.md", "[[B]]\n").unwrap();
        s.roost.write_atomic("B.md", "leaf\n").unwrap();
        let r = call(&s, "get_neighbors", json!({"note":"A"}));
        assert_eq!(r["isError"], false);
        assert_eq!(r["structuredContent"]["truncated"], false);
    }

    // ── Cluster H: daily-template drift ──────────────────────────────────

    #[test]
    fn daily_note_uses_custom_template_when_present() {
        let (d, s) = server();
        std::fs::create_dir_all(d.path().join(".corvux").join("templates")).unwrap();
        std::fs::write(
            d.path().join(".corvux").join("templates").join("daily.md"),
            "# {{title}}\n\nHabit tracker: [ ] water [ ] walk\n",
        )
        .unwrap();

        call(&s, "add_task", json!({"text":"first task of the day"}));

        let (today, weekday) = today_local();
        let daily = s.roost.read(&format!("Daily Notes/{today}.md")).unwrap();
        assert!(daily.contains(&format!("# {weekday}, {today}")), "{daily}");
        assert!(daily.contains("Habit tracker"), "{daily}");
    }

    // ── Cluster F: protocol validation + lifecycle + annotations ────────

    #[test]
    fn handle_rejects_non_2_0_jsonrpc_version() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({"jsonrpc":"1.0","id":1,"method":"ping"}))
            .unwrap();
        assert_eq!(r["error"]["code"], -32600);
    }

    #[test]
    fn handle_rejects_missing_jsonrpc() {
        let (_d, s) = server();
        let r = s.handle(&json!({"id":1,"method":"ping"})).unwrap();
        assert_eq!(r["error"]["code"], -32600);
    }

    #[test]
    fn handle_rejects_malformed_id_shape() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":{"nested":true},"method":"ping"}))
            .unwrap();
        assert_eq!(r["error"]["code"], -32600);
        // JSON-RPC 2.0 §5: a reply to a request whose id couldn't be
        // reliably used must carry a null id, not echo the malformed value.
        assert_eq!(r["id"], Value::Null, "must not echo a malformed id back");
    }

    #[test]
    fn handle_accepts_string_and_null_id() {
        let (_d, s) = server();
        let r1 = s
            .handle(&json!({"jsonrpc":"2.0","id":"abc","method":"ping"}))
            .unwrap();
        assert_eq!(r1["result"], json!({}));
        let r2 = s
            .handle(&json!({"jsonrpc":"2.0","id":null,"method":"ping"}))
            .unwrap();
        assert_eq!(r2["result"], json!({}));
    }

    #[test]
    fn tools_call_before_initialize_is_rejected() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        let v = Roost::open(dir.path().to_path_buf()).unwrap();
        let s = Server::new(v); // deliberately NOT initialized
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"roost_stats","arguments":{}}}))
            .unwrap();
        assert_eq!(r["error"]["code"], -32002);
    }

    #[test]
    fn notification_without_id_never_gets_a_reply_even_when_invalid() {
        let (_d, s) = server();
        // No `id` → notification; JSON-RPC notifications never get a
        // response, valid or rejected.
        let r = s.handle(&json!({"jsonrpc":"1.0","method":"ping"}));
        assert!(r.is_none());
    }

    #[test]
    fn write_tools_are_annotated_with_destructive_and_idempotent_hints() {
        let (_d, s) = server();
        let list = s
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap().clone();
        let find = |name: &str| {
            tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .clone()
        };

        for name in [
            "capture_note",
            "append_to_note",
            "add_task",
            "add_reminder",
            "add_tags",
        ] {
            assert_eq!(
                find(name)["annotations"]["destructiveHint"],
                false,
                "{name} should be destructiveHint:false"
            );
        }
        for name in [
            "capture_note",
            "append_to_note",
            "add_task",
            "add_reminder",
            "complete_task",
            "reschedule_task",
        ] {
            assert_eq!(
                find(name)["annotations"]["idempotentHint"],
                false,
                "{name} should be idempotentHint:false"
            );
        }
        for name in ["add_tags", "set_property"] {
            assert_eq!(
                find(name)["annotations"]["idempotentHint"],
                true,
                "{name} should be idempotentHint:true"
            );
        }
    }

    #[test]
    fn read_only_mode_filters_and_denies_write_tools() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        let roost = Roost::open(dir.path().to_path_buf()).unwrap();
        let server = Server::with_access_mode(roost, AccessMode::ReadOnly);
        initialize(&server);
        let list = server
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .unwrap();
        let names: Vec<_> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(names.contains(&"read_note"));
        assert!(!names.contains(&"write_note"));
        let denied = server
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"write_note","arguments":{"path":"x.md","content":"x"}}}))
            .unwrap();
        assert_eq!(denied["result"]["isError"], true);
    }

    #[test]
    fn tool_specs_close_inputs_and_describe_structured_outputs() {
        let specs = tool_specs();
        let tools = specs.as_array().unwrap();
        assert!(tools
            .iter()
            .all(|tool| tool["inputSchema"]["additionalProperties"] == false));
        let search = tools
            .iter()
            .find(|tool| tool["name"] == "search_roost")
            .unwrap();
        assert_eq!(search["outputSchema"]["type"], "object");
        let set_property = tools
            .iter()
            .find(|tool| tool["name"] == "set_property")
            .unwrap();
        assert_eq!(set_property["annotations"]["destructiveHint"], true);
    }
}
