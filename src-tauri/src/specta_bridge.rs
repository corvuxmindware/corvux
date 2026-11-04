//! tauri-specta scaffolding for Rust ↔ TypeScript type generation.
//!
//! The builder constructed here drives two things:
//!   1. The TypeScript bindings exported to `src/lib/bindings.ts` on
//!      every dev build (gated by `cfg(debug_assertions)`).
//!   2. The Tauri IPC invoke handler — once C11 lands and the legacy
//!      `tauri::generate_handler![...]` block is retired, this becomes
//!      the single source of truth for command registration.
//!
//! Add commands to `collect_commands![...]` as each slice gets migrated
//! (`#[specta::specta]` + `#[derive(specta::Type)]` annotations). Same
//! for events under `collect_events![...]` (requires the
//! `tauri_specta::Event` derive).

use tauri_specta::{collect_commands, collect_events, Builder};

/// Construct the specta Builder with the current command + event surface.
/// Kept as a free function so tests can exercise `.export()` against a
/// tempfile without booting the Tauri runtime.
pub fn build() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new()
        .commands(collect_commands![
            // ── roost.rs ──────────────────────────────────────────
            crate::commands::roost::open_roost,
            crate::commands::roost::close_roost,
            crate::commands::roost::init_default_roost,
            crate::commands::roost::create_named_roost,
            crate::commands::roost::write_roost_manifest,
            crate::commands::roost::read_roost_manifest,
            crate::commands::roost::delete_roost_index,
            // ── files.rs ──────────────────────────────────────────
            crate::commands::files::read_file,
            crate::commands::files::write_file,
            crate::commands::files::create_file,
            crate::commands::files::delete_file,
            crate::commands::files::rename_file,
            crate::commands::files::create_folder,
            crate::commands::files::delete_folder,
            crate::commands::files::move_entry,
            // ── graph.rs ──────────────────────────────────────────
            crate::commands::graph::init_graph,
            crate::commands::graph::rebuild_database,
            crate::commands::graph::index_note,
            crate::commands::graph::purge_note_from_graph,
            crate::commands::graph::get_backlinks,
            crate::commands::graph::get_note_links,
            crate::commands::graph::get_note_tags,
            crate::commands::graph::get_graph_stats,
            crate::commands::graph::get_graph_window,
            crate::commands::graph::get_full_graph,
            crate::commands::graph::graph_page_rank,
            crate::commands::graph::graph_communities,
            crate::commands::graph::graph_bridges,
            crate::commands::graph::compute_bridges_now,
            crate::commands::graph::graph_shortest_path,
            crate::commands::graph::graph_shortest_path_between,
            // ── search.rs ─────────────────────────────────────────
            crate::commands::search::init_search,
            crate::commands::search::index_search,
            crate::commands::search::remove_from_search,
            crate::commands::search::search_roost,
            // ── parser/*.rs ───────────────────────────────────────
            crate::parser::checkboxes::parse_tasks,
            crate::parser::links::parse_links,
            crate::parser::tags::parse_tags,
            // ── tasks.rs + task_notify.rs ─────────────────────────
            crate::commands::tasks::list_tasks,
            crate::commands::tasks::complete_task,
            crate::commands::tasks::add_task_line,
            crate::commands::tasks::reschedule_task,
            crate::commands::tasks::debug_dump_tasks,
            crate::commands::tasks::get_excluded_task_folders,
            crate::commands::tasks::set_excluded_task_folders,
            crate::commands::task_notify::send_test_notification,
            // ── daily_note + attachments + edit_history + save ───
            crate::commands::daily_note::create_daily_note,
            crate::commands::attachments::save_attachment,
            crate::commands::attachments::read_attachment,
            crate::commands::attachments::find_orphan_attachments,
            crate::commands::edit_history::list_edit_events,
            crate::commands::edit_history::revert_edit,
            crate::commands::edit_history::record_external_edit,
            crate::commands::edit_history::record_external_delete,
            crate::commands::edit_history::purge_note_edit_history,
            crate::commands::edit_history::purge_roost_edit_history,
            crate::commands::edit_history::export_edit_history,
            crate::commands::save::save_roost_note,
            // ── diagnostics.rs + perf.rs ──────────────────────────
            crate::diagnostics::get_diagnostic_report,
            crate::diagnostics::get_recent_logs,
            crate::diagnostics::clear_log_buffer,
            crate::diagnostics::check_previous_crash,
            crate::diagnostics::mark_update_relaunch,
            crate::diagnostics::is_store_packaged,
            crate::perf::get_perf_snapshot,
            // ── entitlement.rs (Pro tier gate) ────────────────────
            crate::entitlement::get_entitlement,
            // ── ai/llm.rs ─────────────────────────────────────────
            crate::commands::ai::llm::get_llm_model_status,
            crate::commands::ai::llm::download_llm_model,
            crate::commands::ai::llm::load_llm_model,
            crate::commands::ai::llm::unload_llm_model,
            crate::commands::ai::llm::delete_llm_model,
            // ── ai/ollama_models.rs ───────────────────────────────
            crate::commands::ai::ollama_models::list_ollama_models,
            // ── ai/connection_test.rs ─────────────────────────────
            crate::commands::ai::connection_test::test_gemini_connection,
            crate::commands::ai::connection_test::test_openrouter_connection,
            crate::commands::ai::connection_test::test_ollama_connection,
            crate::commands::ai::connection_test::test_openai_connection,
            crate::commands::ai::connection_test::test_anthropic_connection,
            // ── ai/subagents.rs + ai/skills.rs ────────────────────
            crate::commands::ai::subagents::subagent_list,
            crate::commands::ai::subagents::subagent_refresh,
            crate::commands::ai::skills::skill_list,
            crate::commands::ai::skills::skill_refresh,
            crate::commands::ai::skills::skill_create,
            // ── ai/settings.rs ────────────────────────────────────
            crate::commands::ai::settings::get_settings,
            crate::commands::ai::settings::save_settings,
            crate::commands::ai::settings::list_agent_profiles,
            crate::commands::ai::settings::get_device_profile,
            crate::commands::ai::settings::decide_tool,
            crate::commands::ai::settings::forget_pending_approval,
            crate::commands::ai::settings::list_approval_log,
            crate::commands::ai::settings::tool_reliability_stats,
            crate::commands::ai::settings::get_ai_learning_summary,
            crate::commands::ai::settings::reset_ai_learning,
            crate::commands::ai::settings::agents_md_status,
            crate::commands::ai::settings::create_agents_md,
            // ── ai/chat.rs ────────────────────────────────────────
            crate::commands::ai::chat::chat_send,
            crate::commands::ai::chat::chat_stop,
            // ── sync.rs ───────────────────────────────────────────
            crate::commands::sync::sync_init,
            crate::commands::sync::sync_try_autostart,
            crate::commands::sync::sync_start_listening,
            crate::commands::sync::sync_listen_iroh,
            crate::commands::sync::sync_connect,
            crate::commands::sync::sync_connect_iroh,
            crate::commands::sync::sync_status,
            crate::commands::sync::sync_unpair,
            crate::commands::sync::sync_rescan_roost,
            crate::commands::sync::sync_reset_docstore,
            crate::commands::sync::sync_reconnect_all,
            crate::commands::sync::sync_stop,
            crate::commands::sync::sync_get_queue_settings,
            crate::commands::sync::sync_set_queue_endpoint,
            crate::commands::sync::sync_pin_custom_queue_host,
            crate::commands::sync::sync_set_strict_p2p_only,
            crate::commands::sync::sync_pull_queue_now,
            crate::commands::sync::sync_outbox_status,
            crate::commands::sync::sync_drain_outbox_now,
            // ── mcp.rs (Connect to Claude Desktop) ────────────────
            crate::commands::mcp::claude_desktop_mcp,
            crate::commands::mcp::mcp_config_json,
        ])
        .events(collect_events![
            // ── AI streaming ──────────────────────────────────────────
            crate::events::AiChunk,
            crate::events::AiDone,
            crate::events::AiError,
            // ── AI tools / agent loop ─────────────────────────────────
            crate::events::AiToolPending,
            crate::events::AiToolResult,
            crate::events::AiToolDecided,
            crate::events::AiSecretWarning,
            crate::events::AiSubagentStart,
            crate::events::AiSubagentDone,
            // ── AI model downloads (local Gemma LLM) ──────────────────
            crate::events::AiModelDownloadStart,
            crate::events::AiModelDownloadDone,
            crate::events::AiModelDownloadProgress,
            // ── graph / diagnostics / menu ────────────────────────────
            crate::events::GraphBridgesProgress,
            crate::events::GraphChanged,
            crate::events::LogEvent,
            crate::events::MenuEvent,
            // ── sync ──────────────────────────────────────────────────
            crate::events::SyncStreamEvent,
            crate::events::SyncDiscoveredEvent,
            // ── roost ─────────────────────────────────────────────────
            crate::events::RoostTreeChanged,
        ])
}

/// Header pasted at the top of every generated bindings.ts. Disables
/// tsc strictness and eslint on the file since it's machine-generated.
pub const BINDINGS_HEADER: &str =
    "// @generated by tauri-specta — DO NOT EDIT.\n// Regenerated on every `pnpm tauri dev` boot.\n/* eslint-disable */\n// @ts-nocheck\n";

/// Target path for the generated bindings, relative to `src-tauri/`.
pub const BINDINGS_PATH: &str = "../src/lib/bindings.ts";

/// Configured TypeScript exporter (header + bigint behavior). Not
/// `cfg(debug_assertions)`-gated so the `gen_bindings` example + the
/// CI guard test can call it from either build profile.
///
/// Specta forbids exporting `usize`/`isize`/`i64`/`u64`/`i128`/`u128`
/// by default (precision-loss footgun for apps that may hit 64-bit
/// ranges). Corvux's IPC structs use `usize` for counts and `u64` for
/// elapsed-ms timestamps — all well under 2^53. We narrow these per
/// struct with `#[specta(type = u32 | f64 | ...)]` casts where they
/// cross the IPC boundary, so the default forbid stays on for safety.
pub fn exporter() -> specta_typescript::Typescript {
    specta_typescript::Typescript::default().header(BINDINGS_HEADER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Smoke test: scaffolding compiles, builder constructs, exporter
    /// writes a non-empty file with the expected header. Catches API
    /// drift the moment we bump tauri-specta versions.
    #[test]
    fn export_writes_bindings_file() {
        let dir = tempdir().expect("tempdir");
        let out = dir.path().join("bindings.ts");
        build()
            .export(exporter(), &out)
            .expect("export should succeed with empty command/event lists");

        let contents = fs::read_to_string(&out).expect("read bindings");
        assert!(
            contents.starts_with("// @generated by tauri-specta"),
            "bindings.ts must lead with our @generated header, got:\n{}",
            &contents[..contents.len().min(200)]
        );
    }

    /// CI guard. Regenerates the bindings into a tempfile and asserts
    /// the checked-in `src/lib/bindings.ts` is byte-identical. Runs
    /// unconditionally — `CARGO_MANIFEST_DIR` resolves the committed
    /// file regardless of where `cargo test` was invoked.
    ///
    /// To fix a failure:
    ///   `cd Corvux/src-tauri && cargo run --example gen_bindings`
    /// then commit the resulting bindings.ts.
    #[test]
    fn bindings_are_up_to_date() {
        let dir = tempdir().expect("tempdir");
        let out = dir.path().join("bindings.ts");
        build()
            .export(exporter(), &out)
            .expect("export should succeed");

        let generated = fs::read_to_string(&out).expect("read generated");

        // CARGO_MANIFEST_DIR points at `Corvux/src-tauri/` regardless of
        // where the test was invoked from, so the guard works under
        // `cd src-tauri && cargo test`, `cd Corvux && cargo test
        // --manifest-path src-tauri/Cargo.toml`, and CI runners that
        // pass cwd through unchanged.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let committed_path = std::path::Path::new(manifest_dir)
            .join("..")
            .join("src")
            .join("lib")
            .join("bindings.ts");
        let committed = fs::read_to_string(&committed_path).unwrap_or_else(|e| {
            panic!(
                "couldn't read {} (resolved via CARGO_MANIFEST_DIR={manifest_dir}); error: {e}",
                committed_path.display()
            )
        });

        if generated != committed {
            // Surface a useful diff hint without dumping thousands of
            // lines. First mismatched line is usually enough to
            // identify the missing command or struct.
            let first_diff_line = generated
                .lines()
                .zip(committed.lines())
                .enumerate()
                .find(|(_, (a, b))| a != b)
                .map(|(i, (a, b))| {
                    format!(
                        "\n  line {}:\n    generated: {a}\n    committed: {b}",
                        i + 1
                    )
                })
                .unwrap_or_default();
            panic!(
                "bindings.ts is stale. Run `cargo run --example gen_bindings` from src-tauri/ and commit the result.\n\
                 (generated len={}, committed len={}){first_diff_line}",
                generated.len(),
                committed.len()
            );
        }
    }
}
