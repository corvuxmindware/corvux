//! CozoDB graph queries for notes, links, and tags.
//!
//! Split across submodules by read/write + domain, with a flat public
//! surface re-exported here so call sites keep using `queries::foo()`.
//!
//! # `note_id` canonicalization (AUDIT-023)
//!
//! **Every `note_id` written to or read from CozoDB is a lowercased,
//! forward-slash, roost-relative path.** The single canonicalization point
//! is [`note_id_from_abs_path`] (and its sibling `roost_relative_id` in
//! `commands::files`). Callers MUST NOT pre-lowercase or store mixed-case
//! ids — doing so silently breaks every read query.
//!
//! Read queries [`debug_assert!`] their inputs to catch a violator at the
//! call site before it returns empty rows. Release builds skip the check
//! and trust the contract; that's the price of one fewer
//! `String::to_lowercase` allocation per query.

pub mod ai_signals;
mod algorithms;
pub mod approval_log;
mod edit_events;
mod graph;
mod notes;
mod path;
mod types;

pub use ai_signals::{
    clear_all_signals, query_hash, record_signal, signal_sums_for_notes, sum_to_weight,
    summarize_signals, SignalSummary,
};

pub use edit_events::{
    compute_diff, content_hash, get_event as get_edit_event, latest_after_hash, list_all_events,
    list_events as list_edit_events, mark_reverted, migrate_events_to_new_id, purge_all_events,
    purge_note_events, purge_note_history, record_event as record_edit_event, Actor, EditEvent,
    Intent,
};

pub use algorithms::{
    cached_betweenness_centrality, cached_community_detection, cached_page_rank,
    community_detection, invalidate_algo_cache, page_rank, recompute_betweenness_centrality,
    recompute_community_detection, recompute_page_rank, shortest_path, CentralityEntry,
    CommunityEntry, PageRankEntry, ShortestPathResult,
};
pub use graph::{
    get_backlinks, get_full_graph, get_graph_stats, get_graph_window, get_note_links, get_note_tags,
};
pub use notes::{
    all_tag_names, bulk_index_pass1, bulk_index_pass2, check_links, dangling_count,
    file_mtime_secs, index_note_full, index_note_full_with_resolver, index_note_pass1,
    index_note_pass2, index_note_tasks, list_all_notes, lookup_note_id_by_path,
    lookup_notes_by_ids, note_ids_set, note_paths_under_dir, note_updated_ats,
    promote_dangling_links, purge_note, reconcile_deleted_notes, top_tags, LinkReport,
    LinkResolution, LinkResolver,
};
#[allow(unused_imports)] // tests use these; retain for public-API stability.
pub use path::{note_id_from_abs_path, note_rel_dir};
pub use types::NoteIndexEntry;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_normalizes_separators_and_case() {
        let id = note_id_from_abs_path(
            r"C:\Users\me\Roost\Fit-ME\2026-W10\puli-fitness.md",
            r"C:\Users\me\Roost",
        );
        assert_eq!(id, "fit-me/2026-w10/puli-fitness");
    }

    #[test]
    fn id_handles_root_level_notes() {
        let id = note_id_from_abs_path("/roost/Ideas.md", "/roost");
        assert_eq!(id, "ideas");
    }

    #[test]
    fn id_falls_back_when_not_under_root() {
        // If abs isn't under root, fall back to the full path stem so we
        // still produce a usable id rather than panicking.
        let id = note_id_from_abs_path("/elsewhere/Note.md", "/roost");
        assert!(!id.is_empty());
    }

    #[test]
    fn id_is_form_tolerant_abs_vs_relative() {
        // The `purge_note_from_graph` command (called from the frontend's
        // `note_deleted` sync handler) passes the roost-RELATIVE path the sync
        // engine emits. It must resolve to the SAME note_id the indexer
        // produced from the ABSOLUTE path — otherwise a synced delete would
        // purge nothing. `strip_prefix` fails for the relative form and falls
        // back to the whole string, which (ext dropped, lowercased) matches.
        let from_abs = note_id_from_abs_path("/roost/Corvux/Foo.md", "/roost");
        let from_rel = note_id_from_abs_path("Corvux/Foo.md", "/roost");
        assert_eq!(from_abs, "corvux/foo");
        assert_eq!(
            from_rel, from_abs,
            "relative path must resolve to the same id"
        );
    }

    #[test]
    fn rel_dir_strips_basename_and_extension() {
        let d = note_rel_dir("/roost/Fit-ME/2026-W10/note.md", "/roost");
        assert_eq!(d, "fit-me/2026-w10");
    }

    #[test]
    fn rel_dir_empty_for_root_notes() {
        assert_eq!(note_rel_dir("/roost/Ideas.md", "/roost"), "");
    }

    #[test]
    fn resolver_unique_basename_matches_directly() {
        let r = LinkResolver::from_ids(&["ideas", "fit-me/2026-w10/puli-fitness"]);
        assert_eq!(r.resolve("Ideas", ""), Some("ideas".to_string()));
    }

    #[test]
    fn resolver_exact_path_match_wins() {
        let r = LinkResolver::from_ids(&[
            "fit-me/2026-w10/5th-mar-26/puli-fitness",
            "fit-me/2026-w10/6th-mar-26/puli-fitness",
        ]);
        assert_eq!(
            r.resolve(
                "fit-me/2026-w10/5th-mar-26/puli-fitness",
                "fit-me/2026-w10/6th-mar-26"
            ),
            Some("fit-me/2026-w10/5th-mar-26/puli-fitness".to_string())
        );
    }

    #[test]
    fn resolver_picks_nearest_folder_on_basename_collision() {
        // User's roost has ~20 puli-fitness.md files under daily folders.
        // A link from the 5th-mar-26 folder should resolve to the 5th-mar-26 file.
        let r = LinkResolver::from_ids(&[
            "fit-me/2026-w10/5th-mar-26/puli-fitness",
            "fit-me/2026-w10/6th-mar-26/puli-fitness",
            "fit-me/2026-w11/12th-mar-26/puli-fitness",
        ]);
        assert_eq!(
            r.resolve("puli-fitness", "fit-me/2026-w10/5th-mar-26"),
            Some("fit-me/2026-w10/5th-mar-26/puli-fitness".to_string())
        );
    }

    #[test]
    fn resolver_falls_back_to_sibling_when_no_exact_folder() {
        // Link from 5th-mar-26 but only 6th-mar-26 and 12th-mar-26 exist.
        // Should prefer 6th-mar-26 (same week) over 12th-mar-26 (different week).
        let r = LinkResolver::from_ids(&[
            "fit-me/2026-w10/6th-mar-26/puli-fitness",
            "fit-me/2026-w11/12th-mar-26/puli-fitness",
        ]);
        assert_eq!(
            r.resolve("puli-fitness", "fit-me/2026-w10/5th-mar-26"),
            Some("fit-me/2026-w10/6th-mar-26/puli-fitness".to_string())
        );
    }

    #[test]
    fn resolver_strips_md_extension_in_target() {
        let r = LinkResolver::from_ids(&["ideas"]);
        assert_eq!(r.resolve("Ideas.md", ""), Some("ideas".to_string()));
    }

    #[test]
    fn resolver_returns_none_for_unknown_target() {
        let r = LinkResolver::from_ids(&["ideas"]);
        assert_eq!(r.resolve("nonexistent", ""), None);
    }

    #[test]
    fn resolver_add_id_makes_target_resolvable() {
        let mut r = LinkResolver::from_ids(&["existing"]);
        assert_eq!(r.resolve("just-added", ""), None);
        r.add_id("just-added");
        assert_eq!(r.resolve("just-added", ""), Some("just-added".to_string()));
    }

    #[test]
    fn resolver_add_id_is_idempotent() {
        let mut r = LinkResolver::from_ids(&["foo/bar"]);
        r.add_id("foo/bar");
        r.add_id("foo/bar");
        // Resolver should not have duplicated the id in by_basename;
        // resolution still picks the single canonical id.
        assert_eq!(r.resolve("bar", "foo"), Some("foo/bar".to_string()));
    }

    #[test]
    fn resolver_remove_id_makes_target_unresolvable() {
        let mut r = LinkResolver::from_ids(&["a/note", "b/note"]);
        assert_eq!(r.resolve("note", "a"), Some("a/note".to_string()));
        r.remove_id("a/note");
        // Only b/note remains for basename "note".
        assert_eq!(r.resolve("note", ""), Some("b/note".to_string()));
        r.remove_id("b/note");
        assert_eq!(r.resolve("note", ""), None);
    }

    #[test]
    fn resolver_remove_id_unknown_is_noop() {
        let mut r = LinkResolver::from_ids(&["foo"]);
        r.remove_id("bar"); // not present
        assert_eq!(r.resolve("foo", ""), Some("foo".to_string()));
    }

    #[test]
    fn resolver_handles_folder_prefixed_target() {
        // `[[Fit-ME/puli-fitness]]` — explicit folder hint in the link.
        let r = LinkResolver::from_ids(&[
            "fit-me/2026-w10/5th-mar-26/puli-fitness",
            "other/puli-fitness",
        ]);
        // Basename is "puli-fitness" so both match; nearest-folder ranking
        // should pick whichever shares a prefix with the source dir.
        let resolved = r.resolve("Fit-ME/puli-fitness", "fit-me/2026-w10/5th-mar-26");
        assert_eq!(
            resolved,
            Some("fit-me/2026-w10/5th-mar-26/puli-fitness".to_string())
        );
    }

    #[test]
    fn classify_distinguishes_resolved_ambiguous_dangling() {
        let r = LinkResolver::from_ids(&["ideas", "work/index", "personal/index"]);
        // Unique basename → resolved.
        assert_eq!(
            r.classify("Ideas", ""),
            LinkResolution::Resolved("ideas".to_string())
        );
        // No such note → dangling.
        assert_eq!(r.classify("nope", ""), LinkResolution::Dangling);
        // Shared basename → ambiguous, candidates sorted.
        assert_eq!(
            r.classify("index", ""),
            LinkResolution::Ambiguous(vec!["personal/index".to_string(), "work/index".to_string()])
        );
        // Explicit full path beats the ambiguity.
        assert_eq!(
            r.classify("work/index", ""),
            LinkResolution::Resolved("work/index".to_string())
        );
    }

    #[test]
    fn classify_confident_folder_match_is_not_flagged() {
        let r = LinkResolver::from_ids(&[
            "fit-me/2026-w10/5th-mar-26/puli-fitness",
            "fit-me/2026-w10/6th-mar-26/puli-fitness",
        ]);
        // From inside the 5th-mar-26 folder the pick is confident → resolved,
        // NOT flagged (avoids noise on roosts with many same-named notes).
        assert_eq!(
            r.classify("puli-fitness", "fit-me/2026-w10/5th-mar-26"),
            LinkResolution::Resolved("fit-me/2026-w10/5th-mar-26/puli-fitness".to_string())
        );
        // From an unrelated folder there's no proximity winner → genuine
        // guess → ambiguous.
        assert!(matches!(
            r.classify("puli-fitness", "other/place"),
            LinkResolution::Ambiguous(_)
        ));
    }

    #[test]
    fn check_links_collects_dangling_and_ambiguous_deduped() {
        let r = LinkResolver::from_ids(&["ideas", "work/index", "personal/index"]);
        let content = "See [[Ideas]], [[Index]] and [[Ghost]]. Again [[ideas]] and [[Ghost]].";
        let report = check_links(content, &r, "");
        assert_eq!(report.dangling, vec!["Ghost".to_string()]); // deduped
        assert_eq!(report.ambiguous.len(), 1);
        assert_eq!(report.ambiguous[0].0, "Index");
        assert!(!report.is_empty());
        // A note whose links all resolve is clean.
        assert!(check_links("just [[Ideas]] here", &r, "").is_empty());
    }
}
