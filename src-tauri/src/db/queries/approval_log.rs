//! Approval-log queries (agent-hardening § 3.4.4).
//!
//! One row per decision the approval gate (`ai/tools/approval.rs`)
//! produces. Four decisions are recorded: `approve`, `always_approve`,
//! `reject`, `forgotten` (panel-unmount / cancel-mid-await).
//!
//! Consumers:
//! - Settings → AI → Approval History UI (read path).
//! - § 4.4 tool reliability scorecard — counts per (tool, decision)
//!   bucket over a sliding window.
//!
//! The args themselves are NOT stored — only `blake3(args_json)`
//! truncated to 16 hex chars plus a human one-liner summary. PII stays
//! out of the log; the hash is enough to group identical calls. If a
//! later feature ever needs raw-args replay, a content-addressed
//! sidecar table is the natural follow-up.

use std::collections::BTreeMap;

use cozo::{DataValue, DbInstance, Num, ScriptMutability};

/// One row of `:approval_log`. Mirrors the schema exactly.
#[derive(Debug, Clone)]
pub struct ApprovalLogEntry {
    /// UUID minted by `approval::request()` and emitted on the
    /// `ai:tool-pending` event. Used as the primary key so a row is
    /// guaranteed unique per request and so cross-referencing with
    /// frontend logs / replay traces is trivial.
    pub request_id: String,
    /// Unix epoch seconds (Float to match the rest of the schema).
    pub timestamp: f64,
    /// Tool name as registered in the `Tool::name()` trait method.
    pub tool: String,
    /// `read` | `write` | `destructive`. Stored as String to avoid
    /// migration cost on tier-enum changes.
    pub tier: String,
    /// `approve` | `always_approve` | `reject` | `forgotten`.
    pub decision: String,
    /// First 16 hex chars of blake3(args_json). Long enough to keep
    /// collision probability sub-cosmic at the row volumes we expect
    /// (millions of decisions per user over the app's lifetime).
    pub args_hash: String,
    /// Human one-liner — the same string the tool card title used.
    /// Truncated to 200 chars by the caller.
    pub args_summary: String,
}

/// Decision variants the log records. Mirrors `ToolDecision` plus the
/// `forgotten` sentinel for the receiver-dropped path (panel unmount,
/// agent task cancel). Kept as a separate type so the approval module
/// doesn't have to know about CozoDB and the queries module doesn't
/// have to import the gate types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    Approve,
    AlwaysApprove,
    Reject,
    Forgotten,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionKind::Approve => "approve",
            DecisionKind::AlwaysApprove => "always_approve",
            DecisionKind::Reject => "reject",
            DecisionKind::Forgotten => "forgotten",
        }
    }
}

const SUMMARY_MAX: usize = 200;

/// Compute the truncated hex hash the audit log stores. Public so the
/// approval gate can compute it once and pass the same value the
/// frontend / replay layer would compute against the same args JSON.
pub fn hash_args(args_json: &str) -> String {
    let h = blake3::hash(args_json.as_bytes());
    let hex = h.to_hex();
    // 16 hex chars = 64 bits of entropy. Truncation matters: full hex
    // (64 chars) bloats the row by ~6x with no practical benefit at
    // this collision profile.
    hex.as_str().chars().take(16).collect()
}

/// Insert one decision row. `:put` semantics: a duplicate `request_id`
/// (same approval re-resolved somehow) overwrites; the audit log
/// reflects the LAST decision for a given request, which is the
/// behaviour the UI and scorecard want.
///
/// Best-effort: callers should treat failures as non-fatal — losing one
/// audit row should never break the agent loop. The function returns a
/// String error so the caller can `tracing::warn!` and move on.
pub fn record_decision(db: &DbInstance, entry: &ApprovalLogEntry) -> Result<(), String> {
    let mut summary = entry.args_summary.clone();
    if summary.len() > SUMMARY_MAX {
        // UTF-8 safe truncate — `char_indices` finds a valid boundary.
        let cut = summary
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|i| *i <= SUMMARY_MAX)
            .last()
            .unwrap_or(0);
        summary.truncate(cut);
        summary.push('…');
    }

    let mut params = BTreeMap::new();
    params.insert(
        "request_id".to_string(),
        DataValue::Str(entry.request_id.clone().into()),
    );
    params.insert(
        "timestamp".to_string(),
        DataValue::Num(Num::Float(entry.timestamp)),
    );
    params.insert(
        "tool".to_string(),
        DataValue::Str(entry.tool.clone().into()),
    );
    params.insert(
        "tier".to_string(),
        DataValue::Str(entry.tier.clone().into()),
    );
    params.insert(
        "decision".to_string(),
        DataValue::Str(entry.decision.clone().into()),
    );
    params.insert(
        "args_hash".to_string(),
        DataValue::Str(entry.args_hash.clone().into()),
    );
    params.insert("args_summary".to_string(), DataValue::Str(summary.into()));

    db.run_script(
        r#"
        ?[request_id, timestamp, tool, tier, decision, args_hash, args_summary] <-
            [[$request_id, $timestamp, $tool, $tier, $decision, $args_hash, $args_summary]]
        :put approval_log {
            request_id =>
            timestamp, tool, tier, decision, args_hash, args_summary
        }
        "#,
        params,
        ScriptMutability::Mutable,
    )
    .map_err(|e| format!("approval_log insert failed: {e}"))?;
    Ok(())
}

/// Return the most recent `limit` rows, newest first. Used by the
/// Settings → AI → Approval History UI. Reads via Cozo's `:order`
/// against `timestamp desc`; not paginated yet — the UI can cap `limit`
/// at a sane value (e.g. 200) until volume warrants real paging.
pub fn list_recent(db: &DbInstance, limit: usize) -> Result<Vec<ApprovalLogEntry>, String> {
    let mut params = BTreeMap::new();
    params.insert("limit".to_string(), DataValue::from(limit as i64));
    let result = db
        .run_script(
            r#"
            ?[request_id, timestamp, tool, tier, decision, args_hash, args_summary] :=
                *approval_log{ request_id, timestamp, tool, tier, decision, args_hash, args_summary }
            :order -timestamp
            :limit $limit
            "#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("approval_log list failed: {e}"))?;
    let mut out = Vec::with_capacity(result.rows.len());
    for row in result.rows.iter() {
        let request_id = row
            .first()
            .and_then(|v| v.get_str())
            .unwrap_or("")
            .to_string();
        let timestamp = row
            .get(1)
            .and_then(|v| match v {
                DataValue::Num(Num::Float(f)) => Some(*f),
                _ => None,
            })
            .unwrap_or(0.0);
        let tool = row
            .get(2)
            .and_then(|v| v.get_str())
            .unwrap_or("")
            .to_string();
        let tier = row
            .get(3)
            .and_then(|v| v.get_str())
            .unwrap_or("")
            .to_string();
        let decision = row
            .get(4)
            .and_then(|v| v.get_str())
            .unwrap_or("")
            .to_string();
        let args_hash = row
            .get(5)
            .and_then(|v| v.get_str())
            .unwrap_or("")
            .to_string();
        let args_summary = row
            .get(6)
            .and_then(|v| v.get_str())
            .unwrap_or("")
            .to_string();
        out.push(ApprovalLogEntry {
            request_id,
            timestamp,
            tool,
            tier,
            decision,
            args_hash,
            args_summary,
        });
    }
    Ok(out)
}

/// Per-tool rollup row used by the §4.4 reliability scorecard. Mirrors
/// the shape the Settings → AI → Tool Stats UI renders.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolStat {
    pub tool: String,
    /// Total decisions recorded for this tool (within `window_secs` of
    /// now, when called with a window).
    pub total: i64,
    pub approvals: i64,
    pub always_approvals: i64,
    pub rejections: i64,
    /// `forgotten` doesn't carry signal about user trust the way the
    /// other three do (it's panel-unmount / agent-cancel), so it's
    /// exposed separately rather than rolled into either "approved" or
    /// "rejected." Lets the UI dim these as procedural rather than
    /// failed.
    pub forgotten: i64,
    /// `rejections / max(total - forgotten, 1)` — the trust-relevant
    /// rate, with forgotten rows excluded from the denominator so
    /// closing a panel doesn't artificially deflate the rate.
    pub rejection_rate: f64,
}

/// Aggregate per-tool stats across an optional time window. `None`
/// returns all-time stats; `Some(seconds)` restricts to decisions
/// recorded within the last `seconds` seconds.
///
/// Tools with zero decisions don't appear in the result — caller is
/// expected to join against the full tool registry if it wants empty
/// rows shown.
pub fn tool_stats(db: &DbInstance, window_secs: Option<f64>) -> Result<Vec<ToolStat>, String> {
    let mut params = BTreeMap::new();
    let cutoff = match window_secs {
        Some(w) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            (now - w).max(0.0)
        }
        // Sentinel that any plausible timestamp clears.
        None => 0.0,
    };
    params.insert("cutoff".to_string(), DataValue::Num(Num::Float(cutoff)));

    // One query that emits (tool, decision, n). We pivot to per-tool
    // rows in Rust — cheap, and avoids Cozo's lack of pivot ops.
    let result = db
        .run_script(
            r#"
            ?[tool, decision, count(rid)] :=
                *approval_log{ request_id: rid, timestamp, tool, decision },
                timestamp >= $cutoff
            "#,
            params,
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("approval_log tool_stats failed: {e}"))?;

    use std::collections::HashMap;
    let mut by_tool: HashMap<String, ToolStat> = HashMap::new();
    for row in result.rows.iter() {
        let tool = row
            .first()
            .and_then(|v| v.get_str())
            .unwrap_or("")
            .to_string();
        let decision = row
            .get(1)
            .and_then(|v| v.get_str())
            .unwrap_or("")
            .to_string();
        let n = row
            .get(2)
            .and_then(|v| match v {
                DataValue::Num(Num::Int(i)) => Some(*i),
                _ => None,
            })
            .unwrap_or(0);
        if tool.is_empty() {
            continue;
        }
        let entry = by_tool.entry(tool.clone()).or_insert(ToolStat {
            tool: tool.clone(),
            total: 0,
            approvals: 0,
            always_approvals: 0,
            rejections: 0,
            forgotten: 0,
            rejection_rate: 0.0,
        });
        entry.total += n;
        match decision.as_str() {
            "approve" => entry.approvals += n,
            "always_approve" => entry.always_approvals += n,
            "reject" => entry.rejections += n,
            "forgotten" => entry.forgotten += n,
            _ => {} // Unknown decision string — skip silently
        }
    }
    let mut out: Vec<ToolStat> = by_tool.into_values().collect();
    for s in out.iter_mut() {
        let denom = (s.total - s.forgotten).max(1);
        s.rejection_rate = s.rejections as f64 / denom as f64;
    }
    // Most-rejected first so the UI surfaces problems at the top.
    out.sort_by(|a, b| {
        b.rejection_rate
            .partial_cmp(&a.rejection_rate)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.total.cmp(&a.total))
    });
    Ok(out)
}

/// Return the set of tools whose rejection rate exceeds `threshold`
/// across the window. Read from the agent loop right before assembling
/// the system prompt, then used to soften the matching tool blurbs
/// ("*(low confidence — user often rejects this)*"). Cheap: same query
/// shape as `tool_stats` plus a filter.
///
/// `min_decisions` excludes thinly-sampled tools so one rejection out
/// of one call doesn't suppress the tool from the prompt entirely.
pub fn high_rejection_tools(
    db: &DbInstance,
    window_secs: Option<f64>,
    threshold: f64,
    min_decisions: i64,
) -> Result<Vec<String>, String> {
    let stats = tool_stats(db, window_secs)?;
    Ok(stats
        .into_iter()
        .filter(|s| s.total - s.forgotten >= min_decisions && s.rejection_rate >= threshold)
        .map(|s| s.tool)
        .collect())
}

/// Total decisions logged. Cheap — single Cozo aggregation. The
/// Approval History UI shows this as a header chip; the scorecard uses
/// it to detect "no data yet" empty-state.
pub fn count(db: &DbInstance) -> Result<i64, String> {
    let result = db
        .run_script(
            r#"?[count(request_id)] := *approval_log{ request_id }"#,
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )
        .map_err(|e| format!("approval_log count failed: {e}"))?;
    Ok(result
        .rows
        .first()
        .and_then(|r| r.first())
        .and_then(|v| match v {
            DataValue::Num(Num::Int(i)) => Some(*i),
            _ => None,
        })
        .unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_db() -> DbInstance {
        let db = DbInstance::new("mem", "", Default::default()).expect("mem db");
        db.run_script(
            r#"
            :create approval_log {
                request_id: String,
                =>
                timestamp: Float,
                tool: String,
                tier: String,
                decision: String,
                args_hash: String,
                args_summary: String
            }
            "#,
            Default::default(),
            ScriptMutability::Mutable,
        )
        .expect("create approval_log");
        db
    }

    fn entry(request_id: &str, ts: f64, tool: &str, decision: DecisionKind) -> ApprovalLogEntry {
        ApprovalLogEntry {
            request_id: request_id.to_string(),
            timestamp: ts,
            tool: tool.to_string(),
            tier: "write".to_string(),
            decision: decision.as_str().to_string(),
            args_hash: hash_args(r#"{"path":"foo.md"}"#),
            args_summary: format!("Edit foo.md (from {tool})"),
        }
    }

    #[test]
    fn record_then_list_returns_row() {
        let db = fresh_db();
        record_decision(
            &db,
            &entry("r1", 100.0, "roost_edit", DecisionKind::Approve),
        )
        .expect("record");
        let rows = list_recent(&db, 10).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "r1");
        assert_eq!(rows[0].decision, "approve");
        assert_eq!(rows[0].tool, "roost_edit");
    }

    #[test]
    fn list_returns_newest_first() {
        let db = fresh_db();
        record_decision(
            &db,
            &entry("old", 100.0, "roost_edit", DecisionKind::Approve),
        )
        .unwrap();
        record_decision(
            &db,
            &entry("newer", 200.0, "roost_edit", DecisionKind::Reject),
        )
        .unwrap();
        record_decision(
            &db,
            &entry("newest", 300.0, "delete_task", DecisionKind::Reject),
        )
        .unwrap();
        let rows = list_recent(&db, 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].request_id, "newest");
        assert_eq!(rows[1].request_id, "newer");
        assert_eq!(rows[2].request_id, "old");
    }

    #[test]
    fn list_respects_limit() {
        let db = fresh_db();
        for i in 0..5 {
            record_decision(
                &db,
                &entry(
                    &format!("r{i}"),
                    100.0 + i as f64,
                    "roost_edit",
                    DecisionKind::Approve,
                ),
            )
            .unwrap();
        }
        let rows = list_recent(&db, 3).unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn duplicate_request_id_overwrites() {
        // `:put` semantics — re-recording the same request_id reflects
        // the latest decision rather than appending a row.
        let db = fresh_db();
        record_decision(
            &db,
            &entry("r1", 100.0, "roost_edit", DecisionKind::Approve),
        )
        .unwrap();
        record_decision(&db, &entry("r1", 200.0, "roost_edit", DecisionKind::Reject)).unwrap();
        let rows = list_recent(&db, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].decision, "reject");
        assert_eq!(rows[0].timestamp, 200.0);
    }

    #[test]
    fn count_matches_row_count() {
        let db = fresh_db();
        assert_eq!(count(&db).unwrap(), 0);
        record_decision(
            &db,
            &entry("r1", 100.0, "roost_edit", DecisionKind::Approve),
        )
        .unwrap();
        record_decision(&db, &entry("r2", 200.0, "roost_edit", DecisionKind::Reject)).unwrap();
        assert_eq!(count(&db).unwrap(), 2);
    }

    #[test]
    fn hash_args_is_16_hex_chars() {
        let h = hash_args(r#"{"path":"foo.md"}"#);
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_args_is_deterministic() {
        let a = hash_args(r#"{"path":"foo.md"}"#);
        let b = hash_args(r#"{"path":"foo.md"}"#);
        assert_eq!(a, b);
    }

    #[test]
    fn hash_args_differs_on_different_input() {
        let a = hash_args(r#"{"path":"foo.md"}"#);
        let b = hash_args(r#"{"path":"bar.md"}"#);
        assert_ne!(a, b);
    }

    #[test]
    fn summary_is_utf8_safe_truncated_when_over_cap() {
        // Build a summary that's well over SUMMARY_MAX and contains
        // multi-byte chars at the cut boundary. We must NOT split a
        // codepoint in half.
        let big = "📝".repeat(100); // 400 bytes (4 bytes per char × 100)
        let mut e = entry("r1", 1.0, "roost_edit", DecisionKind::Approve);
        e.args_summary = big;
        record_decision(&fresh_db(), &e).expect("record with oversize summary");
    }

    #[test]
    fn decision_kind_as_str_matches_schema() {
        assert_eq!(DecisionKind::Approve.as_str(), "approve");
        assert_eq!(DecisionKind::AlwaysApprove.as_str(), "always_approve");
        assert_eq!(DecisionKind::Reject.as_str(), "reject");
        assert_eq!(DecisionKind::Forgotten.as_str(), "forgotten");
    }

    #[test]
    fn tool_stats_rolls_up_per_tool() {
        let db = fresh_db();
        // 3 roost_edit: 2 approve, 1 reject
        record_decision(
            &db,
            &entry("e1", 100.0, "roost_edit", DecisionKind::Approve),
        )
        .unwrap();
        record_decision(
            &db,
            &entry("e2", 101.0, "roost_edit", DecisionKind::Approve),
        )
        .unwrap();
        record_decision(&db, &entry("e3", 102.0, "roost_edit", DecisionKind::Reject)).unwrap();
        // 1 delete_task: rejected
        record_decision(
            &db,
            &entry("d1", 103.0, "delete_task", DecisionKind::Reject),
        )
        .unwrap();
        let stats = tool_stats(&db, None).unwrap();
        assert_eq!(stats.len(), 2);
        // Sorted by rejection_rate desc — delete_task (1.0) before roost_edit (0.33)
        assert_eq!(stats[0].tool, "delete_task");
        assert_eq!(stats[0].rejection_rate, 1.0);
        assert_eq!(stats[1].tool, "roost_edit");
        assert_eq!(stats[1].total, 3);
        assert_eq!(stats[1].approvals, 2);
        assert_eq!(stats[1].rejections, 1);
        assert!((stats[1].rejection_rate - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn tool_stats_excludes_forgotten_from_denominator() {
        let db = fresh_db();
        // 1 approve, 1 reject, 3 forgotten — rate should be 1/2 not 1/5.
        record_decision(&db, &entry("a", 100.0, "roost_edit", DecisionKind::Approve)).unwrap();
        record_decision(&db, &entry("r", 101.0, "roost_edit", DecisionKind::Reject)).unwrap();
        record_decision(
            &db,
            &entry("f1", 102.0, "roost_edit", DecisionKind::Forgotten),
        )
        .unwrap();
        record_decision(
            &db,
            &entry("f2", 103.0, "roost_edit", DecisionKind::Forgotten),
        )
        .unwrap();
        record_decision(
            &db,
            &entry("f3", 104.0, "roost_edit", DecisionKind::Forgotten),
        )
        .unwrap();
        let stats = tool_stats(&db, None).unwrap();
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.total, 5);
        assert_eq!(s.forgotten, 3);
        assert!(
            (s.rejection_rate - 0.5).abs() < 1e-9,
            "expected 0.5 (1 / (5-3)), got {}",
            s.rejection_rate
        );
    }

    #[test]
    fn high_rejection_tools_respects_threshold_and_min_decisions() {
        let db = fresh_db();
        // delete_task: 1 reject (rate 1.0, but only 1 decision → below
        // min_decisions threshold)
        record_decision(
            &db,
            &entry("d1", 100.0, "delete_task", DecisionKind::Reject),
        )
        .unwrap();
        // roost_edit: 8 rejects, 2 approves (rate 0.8, 10 decisions)
        for i in 0..8 {
            record_decision(
                &db,
                &entry(
                    &format!("vr{i}"),
                    100.0 + i as f64,
                    "roost_edit",
                    DecisionKind::Reject,
                ),
            )
            .unwrap();
        }
        for i in 0..2 {
            record_decision(
                &db,
                &entry(
                    &format!("va{i}"),
                    200.0 + i as f64,
                    "roost_edit",
                    DecisionKind::Approve,
                ),
            )
            .unwrap();
        }
        let hot = high_rejection_tools(&db, None, 0.7, 5).unwrap();
        assert_eq!(hot.len(), 1, "min_decisions should exclude delete_task");
        assert_eq!(hot[0], "roost_edit");
    }
}
