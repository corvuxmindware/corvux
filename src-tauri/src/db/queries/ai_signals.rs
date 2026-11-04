//! Read/write helpers for the `ai_signals` relation — the agentic-loop
//! feedback log. Schema lives in `db/schema.rs`.
//!
//! Phase 2.4 wires the consumer: approval-gate decisions on path-aware
//! write tools land here as `+1` (Approve / AlwaysApprove) or `-1`
//! (Reject), keyed by `context_note_id = roost-relative path`. The RAG
//! retrieval reads `signal_weights_for_notes` to adjust top-K results
//! — notes the user has consistently approved float up, ones they've
//! rejected sink. See `rag.rs::build_rag_context` for the integration.

use cozo::{DataValue, DbInstance, Num, ScriptMutability};
use std::collections::{BTreeMap, HashMap};

/// Stable query hash. blake3 16-char prefix matches the convention used
/// by `db/concepts.rs::concept_id_for_label` so we don't have two
/// hashing patterns in the same DB.
pub fn query_hash(query: &str) -> String {
    let normalized = query.trim().to_lowercase();
    let h = blake3::hash(normalized.as_bytes());
    h.to_hex().as_str()[..16].to_string()
}

/// Append a single signal row. Best-effort — DB unavailable returns Ok(())
/// silently because losing a feedback row should never break the chat
/// stream. Errors get logged for telemetry.
#[allow(clippy::too_many_arguments)]
pub fn record_signal(
    db: &DbInstance,
    query_hash: &str,
    tool_name: &str,
    suggestion_kind: &str,
    suggestion_id: &str,
    signal: i64,
    context_note_id: &str,
    tool_iteration: i64,
    result_rank: i64,
    roost_path: &str,
) -> Result<(), String> {
    let signal_id = uuid_like();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let mut params = BTreeMap::new();
    params.insert("signal_id".to_string(), DataValue::Str(signal_id.into()));
    params.insert(
        "query_hash".to_string(),
        DataValue::Str(query_hash.to_string().into()),
    );
    params.insert(
        "tool_name".to_string(),
        DataValue::Str(tool_name.to_string().into()),
    );
    params.insert(
        "suggestion_kind".to_string(),
        DataValue::Str(suggestion_kind.to_string().into()),
    );
    params.insert(
        "suggestion_id".to_string(),
        DataValue::Str(suggestion_id.to_string().into()),
    );
    params.insert("signal".to_string(), DataValue::Num(Num::Int(signal)));
    params.insert("timestamp".to_string(), DataValue::from(now));
    params.insert(
        "context_note_id".to_string(),
        DataValue::Str(context_note_id.to_string().into()),
    );
    params.insert(
        "tool_iteration".to_string(),
        DataValue::Num(Num::Int(tool_iteration)),
    );
    params.insert(
        "result_rank".to_string(),
        DataValue::Num(Num::Int(result_rank)),
    );
    params.insert(
        "roost_path".to_string(),
        DataValue::Str(roost_path.to_string().into()),
    );

    db.run_script(
        r#"?[signal_id, query_hash, tool_name, suggestion_kind, suggestion_id, signal, timestamp, context_note_id, tool_iteration, result_rank, roost_path]
            <- [[$signal_id, $query_hash, $tool_name, $suggestion_kind, $suggestion_id, $signal, $timestamp, $context_note_id, $tool_iteration, $result_rank, $roost_path]]
           :put ai_signals { signal_id => query_hash, tool_name, suggestion_kind, suggestion_id, signal, timestamp, context_note_id, tool_iteration, result_rank, roost_path }"#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("ai_signals write failed: {e}"))?;
    Ok(())
}

/// Cheap monotonic-ish id. Avoids pulling in the `uuid` crate for one
/// caller — `timestamp_ns + counter` is unique within a process.
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    format!("sig-{n:x}-{c:x}")
}

/// Net signal sum per note. `+1` per Approve / AlwaysApprove,
/// `-1` per Reject, clamped to `[-5, +5]` so a single hot streak can't
/// dominate retrieval forever. Notes with no rows are absent from the
/// returned map; callers should treat absence as "weight = 1.0".
///
/// Only counts signals from path-aware tool approvals (the producer
/// in `approval::request`). The dimension `result_rank` and the older
/// `query_hash` field are unused here — they're reserved for the
/// query→suggestion-card UX that hasn't shipped yet.
pub fn signal_sums_for_notes(
    db: &DbInstance,
    note_ids: &[String],
) -> Result<HashMap<String, i64>, String> {
    if note_ids.is_empty() {
        return Ok(HashMap::new());
    }
    // Cozo doesn't have an `IN` operator across query params, so we
    // fetch every signal once and filter in Rust. At realistic row
    // counts (~thousands of approval decisions per user over months)
    // this is well under a millisecond; the alternative is N queries
    // which is worse for the lock-time profile.
    //
    // `signal_id` is included in the projection because Cozo's Datalog
    // returns SET semantics — without the primary key, ten +1 rows
    // for the same note collapse to one tuple in the result.
    let rows = db
        .run_script(
            r#"
            ?[signal_id, context_note_id, signal] :=
                *ai_signals{ signal_id, context_note_id, signal }
            "#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("ai_signals read failed: {e}"))?;

    let wanted: std::collections::HashSet<&String> = note_ids.iter().collect();
    let mut sums: HashMap<String, i64> = HashMap::new();
    for row in &rows.rows {
        let Some(DataValue::Str(note_id)) = row.get(1) else {
            continue;
        };
        let Some(DataValue::Num(Num::Int(signal))) = row.get(2) else {
            continue;
        };
        let key = note_id.to_string();
        if wanted.contains(&key) {
            *sums.entry(key).or_insert(0) += signal;
        }
    }
    // Clamp so one note can't pin retrieval to itself forever.
    for v in sums.values_mut() {
        *v = (*v).clamp(-5, 5);
    }
    Ok(sums)
}

/// Convert a clamped signal sum to a multiplicative weight in
/// `[0.5, 1.5]`. Linear: 0 → 1.0, +5 → 1.5, -5 → 0.5. The 50%
/// envelope was picked so the user's learned signal can re-order
/// near-tied retrievals but never override a strongly relevant note
/// (a 95%-similarity result still beats a 60%-similarity boosted one).
pub fn sum_to_weight(sum: i64) -> f64 {
    1.0 + 0.1 * (sum.clamp(-5, 5) as f64)
}

/// Aggregate counts for the Settings → AI Learning panel. Three
/// numbers + the top-N most-boosted and top-N most-penalized note ids.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct SignalSummary {
    #[specta(type = i32)]
    pub total_signals: i64,
    #[specta(type = i32)]
    pub approve_signals: i64,
    #[specta(type = i32)]
    pub reject_signals: i64,
    #[specta(type = Vec<(String, i32)>)]
    pub top_boosted: Vec<(String, i64)>,
    #[specta(type = Vec<(String, i32)>)]
    pub top_penalized: Vec<(String, i64)>,
}

pub fn summarize_signals(db: &DbInstance, top_n: usize) -> Result<SignalSummary, String> {
    let rows = db
        .run_script(
            r#"
            ?[signal_id, context_note_id, signal] :=
                *ai_signals{ signal_id, context_note_id, signal }
            "#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("ai_signals summary failed: {e}"))?;

    let mut total: i64 = 0;
    let mut approves: i64 = 0;
    let mut rejects: i64 = 0;
    let mut per_note: HashMap<String, i64> = HashMap::new();

    for row in &rows.rows {
        let Some(DataValue::Str(note_id)) = row.get(1) else {
            continue;
        };
        let Some(DataValue::Num(Num::Int(signal))) = row.get(2) else {
            continue;
        };
        total += 1;
        if *signal > 0 {
            approves += 1;
        } else if *signal < 0 {
            rejects += 1;
        }
        if !note_id.is_empty() {
            *per_note.entry(note_id.to_string()).or_insert(0) += signal;
        }
    }

    let mut sorted: Vec<(String, i64)> = per_note.into_iter().collect();
    sorted.sort_by_key(|b| std::cmp::Reverse(b.1));
    let top_boosted: Vec<(String, i64)> = sorted
        .iter()
        .filter(|(_, v)| *v > 0)
        .take(top_n)
        .cloned()
        .collect();
    let top_penalized: Vec<(String, i64)> = sorted
        .iter()
        .rev()
        .filter(|(_, v)| *v < 0)
        .take(top_n)
        .cloned()
        .collect();

    Ok(SignalSummary {
        total_signals: total,
        approve_signals: approves,
        reject_signals: rejects,
        top_boosted,
        top_penalized,
    })
}

/// Wipe every row from `:ai_signals`. Exposed for the Settings →
/// AI Learning → "Reset learning" button. Idempotent.
pub fn clear_all_signals(db: &DbInstance) -> Result<(), String> {
    db.run_script(
        r#"
        ?[signal_id] := *ai_signals{ signal_id }
        :rm ai_signals { signal_id }
        "#,
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("ai_signals clear failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_with_schema() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#"
            :create ai_signals {
                signal_id: String
                =>
                query_hash: String,
                tool_name: String,
                suggestion_kind: String,
                suggestion_id: String,
                signal: Int,
                timestamp: Float,
                context_note_id: String,
                tool_iteration: Int,
                result_rank: Int,
                roost_path: String
            }
            "#,
            BTreeMap::new(),
            ScriptMutability::Mutable,
        )
        .expect("create ai_signals");
        db
    }

    fn write(db: &DbInstance, note: &str, signal: i64, tool: &str) -> Result<(), String> {
        record_signal(
            db,
            "", // query_hash unused on the approval path
            tool,
            "tool_path",
            note,
            signal,
            note,
            0,
            0,
            "roost://test",
        )
    }

    #[test]
    fn sum_to_weight_is_linear_in_clamp_range() {
        assert_eq!(sum_to_weight(0), 1.0);
        assert_eq!(sum_to_weight(5), 1.5);
        assert_eq!(sum_to_weight(-5), 0.5);
        // Out-of-range gets clamped, not extrapolated.
        assert_eq!(sum_to_weight(50), 1.5);
        assert_eq!(sum_to_weight(-50), 0.5);
    }

    #[test]
    fn signal_sums_aggregate_per_note() {
        let db = db_with_schema();
        write(&db, "Projects/Foo", 1, "edit_note").unwrap();
        write(&db, "Projects/Foo", 1, "edit_note").unwrap();
        write(&db, "Projects/Foo", -1, "edit_note").unwrap();
        write(&db, "Journal/Bar", -1, "edit_note").unwrap();
        write(&db, "Other", 1, "edit_note").unwrap();

        let sums = signal_sums_for_notes(
            &db,
            &[
                "Projects/Foo".into(),
                "Journal/Bar".into(),
                "Missing".into(),
            ],
        )
        .unwrap();
        assert_eq!(sums.get("Projects/Foo"), Some(&1));
        assert_eq!(sums.get("Journal/Bar"), Some(&-1));
        // Notes not in the wanted list don't appear.
        assert_eq!(sums.get("Other"), None);
        // Missing notes (no rows) don't appear either.
        assert_eq!(sums.get("Missing"), None);
    }

    #[test]
    fn signal_sums_clamp_to_five() {
        let db = db_with_schema();
        for _ in 0..10 {
            write(&db, "Hot", 1, "edit_note").unwrap();
        }
        let sums = signal_sums_for_notes(&db, &["Hot".into()]).unwrap();
        assert_eq!(sums.get("Hot"), Some(&5));
    }

    #[test]
    fn empty_note_list_short_circuits_to_empty_map() {
        let db = db_with_schema();
        write(&db, "Foo", 1, "edit_note").unwrap();
        let sums = signal_sums_for_notes(&db, &[]).unwrap();
        assert!(sums.is_empty());
    }

    #[test]
    fn summarize_returns_counts_and_top_lists() {
        let db = db_with_schema();
        write(&db, "Projects/Star", 1, "edit_note").unwrap();
        write(&db, "Projects/Star", 1, "edit_note").unwrap();
        write(&db, "Projects/Star", 1, "edit_note").unwrap();
        write(&db, "Journal/Bad", -1, "edit_note").unwrap();
        write(&db, "Journal/Bad", -1, "edit_note").unwrap();
        write(&db, "Other", 1, "edit_note").unwrap();

        let s = summarize_signals(&db, 5).unwrap();
        assert_eq!(s.total_signals, 6);
        assert_eq!(s.approve_signals, 4);
        assert_eq!(s.reject_signals, 2);
        // Top boosted: Projects/Star first (+3), Other (+1).
        assert_eq!(s.top_boosted[0].0, "Projects/Star");
        assert_eq!(s.top_boosted[0].1, 3);
        // Top penalized: Journal/Bad (-2).
        assert_eq!(s.top_penalized[0].0, "Journal/Bad");
        assert_eq!(s.top_penalized[0].1, -2);
    }

    #[test]
    fn clear_wipes_every_row() {
        let db = db_with_schema();
        write(&db, "Foo", 1, "edit_note").unwrap();
        write(&db, "Bar", -1, "edit_note").unwrap();
        clear_all_signals(&db).unwrap();
        let s = summarize_signals(&db, 5).unwrap();
        assert_eq!(s.total_signals, 0);
    }
}
