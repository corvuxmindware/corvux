pub mod ai;
pub mod attachments;
pub mod daily_note;
pub mod edit_history;
pub mod files;
pub mod frontmatter_tasks;
pub mod graph;
pub mod index_watcher;
// "Connect to Claude Desktop" — spawns the bundled corvux-mcp sidecar to
// register itself as an MCP server in Claude Desktop's config.
pub mod mcp;
// AUDIT-053: bundled save IPC. Keeps the existing per-domain commands
// (files / graph / search) intact while offering one round-trip for the
// roost-note save hot path.
pub mod save;
pub mod search;
pub mod sync;
// Q-03: queue endpoint URL validation lives here so the trust boundary
// between caller-supplied URLs and the network layer is in one
// auditable file (S-02 motivated the rewrite).
pub mod roost;
pub mod roost_tree_cache;
mod sync_endpoint;
pub mod task_notify;
pub mod tasks;
