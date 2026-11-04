//! TextRank-based keyphrase extraction.
//!
//! Pure Rust, no model download. Builds a word co-occurrence graph
//! within a sliding window, runs PageRank on the word graph, and
//! merges adjacent top-ranked words back into phrases. The classic
//! TextRank pipeline from Mihalcea & Tarau (2004).
//!
//! Why not TF-IDF: TextRank works on a single document — no roost-
//! corpus IDF model to maintain, keep stale, or recompute. For a
//! note-taking app where each save needs a quick "what's this note
//! about" pass, this is the right shape.
//!
//! Output is intentionally a small list (5–10 phrases). Phase 2.3
//! turns these strings into `concepts` rows with embeddings + dedup.

use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct Keyphrase {
    pub text: String,
    pub score: f64,
}

/// Common English stopwords. Kept short on purpose — over-aggressive
/// filtering loses signal in technical notes (e.g. "for" can be a real
/// preposition pattern in code-flavored writing). Tunable later.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "of", "at", "by", "for", "with", "about",
    "against", "between", "into", "through", "during", "to", "from", "up", "down", "in", "out",
    "on", "off", "over", "under", "is", "are", "was", "were", "be", "been", "being", "am", "have",
    "has", "had", "having", "do", "does", "did", "doing", "will", "would", "should", "could",
    "can", "may", "might", "must", "shall", "ought", "i", "me", "my", "we", "our", "us", "you",
    "your", "they", "them", "their", "this", "that", "these", "those", "it", "its", "he", "she",
    "him", "her", "his", "hers", "as", "than", "so", "very", "just", "also", "too", "only", "not",
    "no", "nor", "yes", "what", "which", "who", "whom", "whose", "when", "where", "why", "how",
    "any", "all", "some", "each", "every", "both", "few", "more", "most", "other", "another",
    "such", "same", "own", "many", "then", "now", "here", "there", "still", "already", "again",
    "once", "while", "because", "since", "though", "although", "well", "even", "ever", "yet",
    "really", "actually", "let", "lets",
];

fn is_stopword(token: &str) -> bool {
    STOPWORDS.contains(&token)
}

/// Strip markdown noise that pollutes keyphrase ranks: code blocks,
/// inline code, YAML frontmatter, raw URLs, wiki-link target syntax.
/// We keep the *display text* of wiki-links (`[[target|display]]`) and
/// hashtag bodies because those usually carry topical signal.
fn clean_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());

    // Line-based scan — much easier to reason about than streaming
    // chars and dealing with cross-line state (code fences, frontmatter).
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    let mut in_frontmatter = false;
    let mut in_code_fence = false;

    if !lines.is_empty() && lines[0].trim() == "---" {
        in_frontmatter = true;
        i = 1;
        while i < lines.len() {
            if lines[i].trim() == "---" {
                i += 1;
                in_frontmatter = false;
                break;
            }
            i += 1;
        }
    }

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
            i += 1;
            continue;
        }
        if in_code_fence || in_frontmatter {
            i += 1;
            continue;
        }

        let mut cleaned = String::with_capacity(line.len());
        let bytes: Vec<char> = line.chars().collect();
        let mut j = 0;
        while j < bytes.len() {
            let c = bytes[j];
            // Inline code: drop ``...``
            if c == '`' {
                j += 1;
                while j < bytes.len() && bytes[j] != '`' {
                    j += 1;
                }
                if j < bytes.len() {
                    j += 1;
                }
                cleaned.push(' ');
                continue;
            }
            // Wiki-link `[[target|display]]` → keep display, else target.
            if c == '[' && j + 1 < bytes.len() && bytes[j + 1] == '[' {
                j += 2;
                let mut content = String::new();
                while j < bytes.len() {
                    if bytes[j] == ']' && j + 1 < bytes.len() && bytes[j + 1] == ']' {
                        j += 2;
                        break;
                    }
                    content.push(bytes[j]);
                    j += 1;
                }
                let display = content
                    .split_once('|')
                    .map(|(_, d)| d.to_string())
                    .unwrap_or(content);
                cleaned.push(' ');
                cleaned.push_str(&display);
                cleaned.push(' ');
                continue;
            }
            // Markdown link `[text](url)` → keep text, drop url.
            if c == '[' {
                j += 1;
                let mut text = String::new();
                while j < bytes.len() && bytes[j] != ']' {
                    text.push(bytes[j]);
                    j += 1;
                }
                if j < bytes.len() {
                    j += 1; // skip ]
                }
                if j < bytes.len() && bytes[j] == '(' {
                    while j < bytes.len() && bytes[j] != ')' {
                        j += 1;
                    }
                    if j < bytes.len() {
                        j += 1; // skip )
                    }
                }
                cleaned.push(' ');
                cleaned.push_str(&text);
                cleaned.push(' ');
                continue;
            }
            cleaned.push(c);
            j += 1;
        }

        out.push_str(&cleaned);
        out.push('\n');
        i += 1;
    }

    out
}

/// Tokenize cleaned text into lowercase word stems suitable for the
/// co-occurrence graph. Keeps only alphabetic + apostrophe; drops
/// stopwords; drops anything shorter than 3 chars (filters out a/i/it
/// even if they bypass the stopword list, plus reduces graph noise).
///
/// Returns BOTH the token sequence (for window co-occurrence) AND the
/// per-position original-word string (so phrase merging can recover
/// the surface form).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphabetic() || ch == '\'' || ch == '-' {
            current.push(ch.to_ascii_lowercase());
        } else {
            if !current.is_empty() {
                push_if_ok(&current, &mut tokens);
                current.clear();
            }
        }
    }
    if !current.is_empty() {
        push_if_ok(&current, &mut tokens);
    }
    tokens
}

fn push_if_ok(token: &str, out: &mut Vec<String>) {
    let t = token.trim_matches(|c: char| !c.is_alphanumeric());
    if t.len() < 3 {
        return;
    }
    if is_stopword(t) {
        return;
    }
    out.push(t.to_string());
}

/// Build word co-occurrence graph with a sliding window. Edges are
/// undirected and weighted by co-occurrence count.
fn cooccurrence_graph(tokens: &[String], window: usize) -> HashMap<String, HashMap<String, u32>> {
    let mut adj: HashMap<String, HashMap<String, u32>> = HashMap::new();
    for i in 0..tokens.len() {
        let a = &tokens[i];
        let end = (i + window).min(tokens.len());
        for b in tokens.iter().take(end).skip(i + 1) {
            if a == b {
                continue;
            }
            *adj.entry(a.clone())
                .or_default()
                .entry(b.clone())
                .or_insert(0) += 1;
            *adj.entry(b.clone())
                .or_default()
                .entry(a.clone())
                .or_insert(0) += 1;
        }
    }
    adj
}

/// PageRank on the co-occurrence graph. Damping 0.85, fixed iteration
/// count — the graph is small (typically <1K vertices) so we don't
/// bother with epsilon-based convergence.
fn pagerank(
    graph: &HashMap<String, HashMap<String, u32>>,
    iterations: usize,
) -> HashMap<String, f64> {
    let n = graph.len();
    if n == 0 {
        return HashMap::new();
    }
    let damping = 0.85_f64;
    let initial = 1.0 / n as f64;
    let mut score: HashMap<String, f64> = graph.keys().map(|k| (k.clone(), initial)).collect();

    let weighted_outdeg: HashMap<String, f64> = graph
        .iter()
        .map(|(k, neigh)| (k.clone(), neigh.values().map(|w| *w as f64).sum::<f64>()))
        .collect();

    for _ in 0..iterations {
        let mut next: HashMap<String, f64> = graph
            .keys()
            .map(|k| (k.clone(), (1.0 - damping) / n as f64))
            .collect();

        for (node, neighbors) in graph {
            let s = score.get(node).copied().unwrap_or(0.0);
            let outdeg = weighted_outdeg.get(node).copied().unwrap_or(0.0).max(1.0);
            for (other, weight) in neighbors {
                let contribution = damping * s * (*weight as f64) / outdeg;
                if let Some(v) = next.get_mut(other) {
                    *v += contribution;
                }
            }
        }
        score = next;
    }
    score
}

/// Merge adjacent top-ranked tokens back into multi-word phrases when
/// they appear contiguously in the source. "machine learning" beats
/// just "machine" + "learning" as separate concepts.
fn merge_phrases(
    tokens: &[String],
    scores: &HashMap<String, f64>,
    top_set: &HashSet<String>,
    max_phrases: usize,
) -> Vec<Keyphrase> {
    // Walk the token sequence; whenever we hit a run of top-ranked
    // tokens, treat that run as a phrase. Same phrase can occur
    // multiple times — accumulate score.
    let mut phrase_scores: BTreeMap<String, f64> = BTreeMap::new();
    let mut i = 0;
    while i < tokens.len() {
        if !top_set.contains(&tokens[i]) {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < tokens.len() && top_set.contains(&tokens[end]) {
            end += 1;
        }
        let phrase = tokens[i..end].join(" ");
        let phrase_score: f64 = tokens[i..end].iter().map(|t| scores[t]).sum();
        let entry = phrase_scores.entry(phrase).or_insert(0.0);
        *entry = entry.max(phrase_score);
        i = end;
    }

    let mut sorted: Vec<(String, f64)> = phrase_scores.into_iter().collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    sorted.truncate(max_phrases);
    sorted
        .into_iter()
        .map(|(text, score)| Keyphrase { text, score })
        .collect()
}

/// Top-level entry point. Returns up to `max_phrases` keyphrases for a
/// single note's markdown content, ordered by TextRank score
/// descending. Empty / pathological notes return an empty Vec rather
/// than erroring — callers (the extractor pipeline) just skip those.
pub fn extract_keyphrases(content: &str, max_phrases: usize) -> Vec<Keyphrase> {
    if content.trim().is_empty() || max_phrases == 0 {
        return Vec::new();
    }
    let cleaned = clean_markdown(content);
    let tokens = tokenize(&cleaned);
    if tokens.is_empty() {
        return Vec::new();
    }

    let graph = cooccurrence_graph(&tokens, 4);
    if graph.is_empty() {
        // Single-occurrence tokens with no co-occurrences. Fall back to
        // a frequency-based shortlist so we still return something
        // useful for very short notes.
        let mut counts: HashMap<&String, usize> = HashMap::new();
        for t in &tokens {
            *counts.entry(t).or_default() += 1;
        }
        let mut sorted: Vec<(&&String, &usize)> = counts.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        return sorted
            .into_iter()
            .take(max_phrases)
            .map(|(t, _)| Keyphrase {
                text: (*t).clone(),
                score: 1.0,
            })
            .collect();
    }

    let scores = pagerank(&graph, 30);

    // Take top ~1/3 of words as the "candidate" pool — Mihalcea's
    // suggested ratio for phrase merging. Floor to at least max_phrases.
    let pool_size = (graph.len() / 3).max(max_phrases);
    let mut sorted: Vec<(String, f64)> = scores.iter().map(|(k, v)| (k.clone(), *v)).collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let top_set: HashSet<String> = sorted
        .iter()
        .take(pool_size)
        .map(|(k, _)| k.clone())
        .collect();

    merge_phrases(&tokens, &scores, &top_set, max_phrases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_content_returns_empty() {
        assert!(extract_keyphrases("", 5).is_empty());
        assert!(extract_keyphrases("   \n  \t  \n", 5).is_empty());
    }

    #[test]
    fn extracts_dominant_phrase_from_repeated_topic() {
        let text = "
            Machine learning is changing every field. We use machine
            learning for classification, regression, and clustering.
            Modern machine learning relies on neural networks. The
            machine learning community moves fast.
        ";
        let phrases = extract_keyphrases(text, 5);
        assert!(!phrases.is_empty());
        assert!(
            phrases
                .iter()
                .any(|p| p.text.contains("machine") && p.text.contains("learning")),
            "expected 'machine learning' to surface, got: {:?}",
            phrases
        );
    }

    #[test]
    fn ignores_code_blocks() {
        // The fenced block contains "elephant" repeatedly. Real prose talks
        // about "tigers" — a TextRank that respects code fences should
        // surface tigers, not elephants.
        let text = "
            Tigers in the wild. Tigers hunt at night. Tigers prefer dense
            cover. Local tiger populations face habitat pressure. Tiger
            conservation programs require funding.

            ```
            for elephant in elephants:
                elephant.feed()
                elephant.elephant_log()
                elephant_total += elephant.size
            ```
        ";
        let phrases = extract_keyphrases(text, 5);
        let any_tiger = phrases.iter().any(|p| p.text.contains("tiger"));
        let top_is_elephant = phrases
            .first()
            .map(|p| p.text.contains("elephant"))
            .unwrap_or(false);
        assert!(any_tiger, "tigers must rank: {:?}", phrases);
        assert!(
            !top_is_elephant,
            "code-block elephants must not dominate: {:?}",
            phrases
        );
    }

    #[test]
    fn strips_yaml_frontmatter() {
        // Frontmatter mentions "secret-tag" 5 times. Body talks about
        // "kubernetes". Frontmatter must be ignored.
        let text = "---
title: Note
tags: [secret-tag, secret-tag, secret-tag, secret-tag, secret-tag]
---

Kubernetes orchestrates containers. Kubernetes uses pods. Kubernetes
scheduling decisions are driven by resource limits.";
        let phrases = extract_keyphrases(text, 5);
        assert!(
            !phrases.iter().any(|p| p.text.contains("secret-tag")),
            "frontmatter tag must not leak into keyphrases"
        );
        assert!(
            phrases.iter().any(|p| p.text.contains("kubernetes")),
            "body content must surface, got: {:?}",
            phrases
        );
    }

    #[test]
    fn keeps_wiki_link_display_text() {
        let text = "
            See [[Project Plan|the plan]] for the schedule. The plan
            covers next quarter's deliverables. Plan execution starts
            Monday. Every plan needs review.
        ";
        let phrases = extract_keyphrases(text, 3);
        assert!(
            phrases.iter().any(|p| p.text.contains("plan")),
            "wiki-link display text must contribute, got: {:?}",
            phrases
        );
    }

    #[test]
    fn drops_stopwords() {
        let text = "the and or but if the and or but if the and or";
        let phrases = extract_keyphrases(text, 5);
        assert!(
            phrases.is_empty()
                || !phrases
                    .iter()
                    .any(|p| { ["the", "and", "or", "but", "if"].contains(&p.text.as_str()) }),
            "stopwords must not appear: {:?}",
            phrases
        );
    }

    #[test]
    fn limits_results_to_max_phrases() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa
                    alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let phrases = extract_keyphrases(text, 3);
        assert!(phrases.len() <= 3);
    }
}
