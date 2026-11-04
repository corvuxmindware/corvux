//! `roost_search` — FTS5 keyword search with stop-word stripping and
//! shorter-query auto-retry. The first place the agent looks for "where
//! did I write about X?". The LLM drives recall by re-querying with
//! different keywords (the Claude-Code-style search loop), so there is
//! no embedding/HNSW lane here — keyword search is sub-50ms locally and
//! works identically on every device.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::Manager;

use crate::commands::search::SearchState;

use super::registry::{SafetyTier, Tool, ToolContext, ToolError};

pub struct RoostSearchTool;

#[async_trait]
impl Tool for RoostSearchTool {
    fn name(&self) -> &'static str {
        "roost_search"
    }

    fn description(&self) -> &'static str {
        "Search the roost using full-text (BM25 keyword) search. Returns \
         ranked notes with snippets. Use this when the user asks \
         'where did I write about X', 'find notes about Y', or any time you \
         need to discover notes by content rather than by name.\n\
         IMPORTANT: pass content KEYWORDS, not the user's full sentence. \
         E.g. for 'what are the topics in the brainstorm for corvux', search \
         for 'brainstorm corvux' (or just 'brainstorm'). The tool drops \
         common stop words automatically and auto-retries with shorter \
         queries when the first attempt returns zero hits — but the more \
         focused your input, the better the ranking. \
         If a keyword search misses what the user means (synonyms, \
         paraphrases), DON'T give up — re-call this tool with different \
         keywords (a synonym, a related term, the other language they \
         might have used). Iterating beats one perfect query. \
         For 'topics / sections / outline of [[X]]' style questions, prefer \
         calling note_outline AFTER you find the note."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query — keywords work best; the tool strips stop words and auto-retries with shorter queries" },
                "limit": { "type": "integer", "default": 10, "maximum": 50 }
            },
            "required": ["query"]
        })
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::Read
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> Result<Value, ToolError> {
        let original_query = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::retryable("missing or empty 'query'"))?
            .to_string();

        let limit = args
            .get("limit")
            .and_then(|v| v.as_i64())
            .unwrap_or(10)
            .clamp(1, 50) as usize;

        let _ = ctx.require_roost(self.name())?;

        // Strip stop-words. SQLite FTS5 ANDs every quoted token, so a 7-word
        // natural-language question like "what are the topics in brainstorm
        // for corvux" requires every one of those tokens to appear in one
        // note — almost never matches. Cleaning up front is the difference
        // between "0 results" and "found it on the first try".
        let keywords = extract_keywords(&original_query);
        let cleaned_query = keywords.join(" ");

        // FTS5 lookup with auto-retry. First try the cleaned query; if it
        // returns 0, retry with progressively shorter queries (longest
        // single keyword, then next-longest). The LLM gets one structured
        // response with a `tried` field so it can see what was attempted.
        let (fts_results, used_query, tried) =
            run_fts_with_retry(&ctx.app, &cleaned_query, &keywords, &original_query)?;

        let results: Vec<Value> = fts_results
            .into_iter()
            .take(limit)
            .map(|r| {
                json!({
                    "title": r.title,
                    "path": r.path,
                    "snippet": clip(&r.snippet, 500),
                })
            })
            .collect();

        let summary = if results.is_empty() {
            format!(
                "0 results. Tried: {}. Try a different keyword or call roost_read on a likely note title directly.",
                tried.join(" → ")
            )
        } else {
            format!("{} result(s) for \"{}\"", results.len(), used_query)
        };

        Ok(json!({
            "summary": summary,
            "original_query": original_query,
            "used_query": used_query,
            "tried": tried,
            "results": results,
        }))
    }
}

/// Common English stop-words plus a handful of Tenglish/abbreviation
/// noise the user occasionally emits ("imp" for "important"). Kept short
/// — over-aggressive filtering kills legitimately rare-but-content-bearing
/// terms. The list is deliberately not 200+ — we want to drop scaffolding,
/// not vocabulary.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "should", "can", "could", "may", "might", "must", "of",
    "to", "in", "on", "at", "by", "for", "with", "about", "from", "up", "down", "into", "out",
    "what", "which", "who", "whom", "this", "that", "these", "those", "and", "but", "or", "so",
    "if", "as", "than", "then", "i", "me", "my", "we", "our", "you", "your", "he", "him", "his",
    "she", "her", "it", "its", "they", "them", "their",
    // Tenglish / texting noise the user actually types
    "imp", "pls", "plz",
];

fn extract_keywords(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 2)
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect()
}

/// Search FTS5 with the cleaned query first. If 0 hits and we have
/// multiple keywords, retry with the longest single keyword, then the
/// next-longest. Returns (matches, query_actually_used, list_of_attempts)
/// so the agent can see the trail.
fn run_fts_with_retry(
    app: &tauri::AppHandle,
    cleaned: &str,
    keywords: &[String],
    original: &str,
) -> Result<(Vec<FtsHit>, String, Vec<String>), ToolError> {
    let mut tried: Vec<String> = Vec::new();

    // Attempt 1: cleaned full keyword set (or original if cleaning emptied it).
    let first_query = if cleaned.is_empty() {
        original
    } else {
        cleaned
    };
    tried.push(first_query.to_string());
    let hits = run_fts(app, first_query)?;
    if !hits.is_empty() {
        return Ok((hits, first_query.to_string(), tried));
    }

    // Attempt 2+: longest distinct keywords, one at a time. Sorting by
    // length descending means the most-specific token gets tried first
    // ('brainstorm' before 'corvux' before 'note').
    let mut by_len: Vec<&String> = keywords.iter().collect();
    by_len.sort_by_key(|k| std::cmp::Reverse(k.len()));
    for kw in by_len.into_iter().take(2) {
        if tried.iter().any(|t| t == kw) {
            continue;
        }
        tried.push(kw.clone());
        let hits = run_fts(app, kw)?;
        if !hits.is_empty() {
            return Ok((hits, kw.clone(), tried));
        }
    }

    Ok((Vec::new(), first_query.to_string(), tried))
}

/// One row from FTS5.
struct FtsHit {
    path: String,
    title: String,
    snippet: String,
}

fn run_fts(app: &tauri::AppHandle, query: &str) -> Result<Vec<FtsHit>, ToolError> {
    let search_state = app
        .try_state::<SearchState>()
        .ok_or_else(|| ToolError::fatal("search index not initialized — open a roost first"))?;
    let guard = search_state
        .0
        .lock()
        .map_err(|e| ToolError::retryable(format!("search lock: {e}")))?;
    let Some(bound) = guard.as_ref() else {
        // Don't lie with an empty Vec. Tell the LLM the index isn't
        // populated so it falls back to the roost index in its system
        // prompt + roost_read on a likely title.
        return Err(ToolError::retryable(
            "FTS5 search index is not populated yet. Use the Roost index from your system prompt to find the note title and call roost_read or note_outline directly. Full-text search will become available after the user opens the Search panel once."
        ));
    };
    let matches = bound
        .db
        .search(query)
        .map_err(|e| ToolError::retryable(format!("FTS5 search: {e}")))?;
    Ok(matches
        .into_iter()
        .map(|m| {
            let title = Path::new(&m.path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            FtsHit {
                path: m.path,
                title,
                snippet: m.snippet,
            }
        })
        .collect())
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_content_keywords_from_natural_question() {
        let q = "what are the imp topics in the brainstorm for corvux";
        let kw = extract_keywords(q);
        // Should keep 'topics', 'brainstorm', 'corvux'. Drop scaffolding +
        // 'imp' (Tenglish for 'important').
        assert!(kw.contains(&"topics".to_string()));
        assert!(kw.contains(&"brainstorm".to_string()));
        assert!(kw.contains(&"corvux".to_string()));
        assert!(!kw.contains(&"the".to_string()));
        assert!(!kw.contains(&"imp".to_string()));
        assert!(!kw.contains(&"for".to_string()));
        assert!(!kw.contains(&"in".to_string()));
    }

    #[test]
    fn keeps_short_meaningful_tokens() {
        // Two-letter content words like 'ai' or 'ml' should survive even
        // though they're at the min-length boundary.
        let kw = extract_keywords("ai and ml notes");
        assert!(kw.contains(&"ai".to_string()));
        assert!(kw.contains(&"ml".to_string()));
        assert!(kw.contains(&"notes".to_string()));
        assert!(!kw.contains(&"and".to_string()));
    }

    #[test]
    fn handles_punctuation_in_query() {
        let kw = extract_keywords("what's in [[brainstorm]]?");
        assert!(kw.contains(&"brainstorm".to_string()));
        // 'what' (from "what's") is a stop word.
        assert!(!kw.contains(&"what".to_string()));
    }

    #[test]
    fn keeps_underscores_and_hyphens() {
        let kw = extract_keywords("the _brainstorm doc and the api-spec");
        assert!(kw.contains(&"_brainstorm".to_string()));
        assert!(kw.contains(&"api-spec".to_string()));
    }
}
