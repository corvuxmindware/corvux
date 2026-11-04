//! In-memory roost index: an mtime-keyed content cache plus a resolved
//! wiki-link graph, both rebuilt incrementally.
//!
//! Why this exists: every read tool used to re-walk the roost AND re-read
//! every file on each call (`O(roost)` per call — fine at 200 notes, but
//! minutes at the 50K-note desktop target). [`RoostIndex::refresh`] re-reads
//! only the files whose mtime changed (so warm calls do `O(changed)` work),
//! and fans the stat+read sweep across all CPU cores for large roosts, since
//! that sweep is I/O-bound and otherwise dominates every call. The cache is
//! also the natural home for the wiki-link graph that powers the
//! `get_backlinks` / `neighbors` / `find_path` / `top_hubs` tools — no
//! CozoDB, just the markdown.
//!
//! The stdio server processes one request at a time (see `main.rs`) and owns
//! this behind a `RefCell` rather than a lock; `refresh` only borrows the
//! note cache immutably while its worker threads read from disk, then merges
//! the results single-threaded.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::SystemTime;

use crate::conventions::parse_open_tasks;
use crate::roost::{strip_markdown_suffix, Roost, MAX_NOTE_BYTES};

/// One cached note: its content plus the cheap-to-derive bits we reuse
/// across calls. Re-read only when the file's mtime changes.
struct CachedNote {
    mtime: SystemTime,
    content: String,
    /// Lowercased filename stem (no extension) — wiki-link target matching.
    stem: String,
    /// Raw `[[target]]` strings pulled from the body, pre-resolution.
    link_targets: Vec<String>,
}

/// A search result, ranked. `rel` is the roost-relative path.
pub struct SearchHit {
    pub rel: String,
    pub score: u32,
    pub snippet: String,
}

/// A recently-modified note.
pub struct RecentHit {
    pub rel: String,
    /// `YYYY-MM-DD HH:MM` in local time.
    pub modified: String,
}

/// Roost-wide counts for `roost_stats`.
pub struct RoostStats {
    pub notes: usize,
    pub open_tasks: usize,
    pub folders: usize,
    pub links: usize,
}

/// How a `[[wiki-link]]` target resolves against the roost. Matches the app's
/// CozoDB resolver: a shared stem is only `Ambiguous` when folder proximity
/// gives no clear winner — a `[[note]]` link from the right folder resolves
/// confidently and is not flagged.
pub enum LinkClass {
    /// Lands on exactly one note, or a confident proximity winner. Holds the
    /// resolved rel (the graph edge target).
    Resolved(String),
    /// Stem shared by several notes with no proximity winner — a genuine
    /// guess. `best` is the edge the graph picks; `candidates` are all options.
    Ambiguous {
        best: String,
        candidates: Vec<String>,
    },
    /// No note matches — dangling (no graph edge).
    Dangling,
}

/// A broken link found by the audit. `candidates` empty = dangling (no such
/// note); non-empty = ambiguous (the notes the target could mean).
pub struct BrokenLink {
    pub source: String,
    pub target: String,
    pub candidates: Vec<String>,
}

/// A proposed MISSING link (the twin of [`BrokenLink`]): two notes look
/// related — `source` mentions `target`'s title as plain text, and/or they
/// share rare tags — but no wiki-link edge exists between them in either
/// direction. Read-only evidence for the `suggest_links` tool; acting on it
/// (adding the `[[link]]`) is the caller's job.
pub struct LinkSuggestion {
    /// The note to edit. For a mention this is the note whose body contains
    /// the target's title; for a tag-only pair, the alphabetically first.
    pub source: String,
    pub target: String,
    /// True when `source`'s body mentions `target`'s title as plain text.
    pub mentioned: bool,
    /// Rare tags both notes carry, as `(tag, roost-wide note count)`.
    pub shared_tags: Vec<(String, usize)>,
    /// Capped context around the mention (empty for tag-only pairs).
    pub evidence: String,
    /// Mention-weighted relatedness; higher = stronger suggestion.
    pub score: u32,
}

pub struct RoostIndex {
    notes: BTreeMap<String, CachedNote>,
    /// rel -> resolved out-link rels (deduped, no self-loops). Rebuilt when
    /// the note set or any note's links change.
    out_links: HashMap<String, Vec<String>>,
    /// rel -> rels that link TO it (the reverse of `out_links`).
    in_links: HashMap<String, Vec<String>>,
    /// Lowercased filename stem -> ALL rels sharing it. `resolve` picks one
    /// by proximity; this keeps every candidate for ambiguity detection.
    stems: HashMap<String, Vec<String>>,
    /// Normalized rel (no `.md`, lowercased) -> rel. For exact-path links.
    paths: HashMap<String, String>,
}

/// Roosts at or above this many notes have their refresh sweep parallelized;
/// smaller ones run inline (thread-spawn overhead isn't worth it).
const PARALLEL_SCAN_THRESHOLD: usize = 512;

/// Hard cap on total nodes `neighbors` will visit/return, independent of the
/// depth clamp the caller already applies — a dense graph can still touch
/// thousands of notes at shallow depth. Mirrors the truncation shape already
/// used by `get_backlinks`/`get_forward_links` (`LINK_LIST_DEFAULT_LIMIT` /
/// `render_link_list` in server.rs).
pub const NEIGHBORS_MAX_RESULTS: usize = 2000;
const MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;

/// Outcome of statting one scanned file during [`RoostIndex::refresh`],
/// computed in the parallel phase and applied in the single-threaded merge.
enum Resolved {
    /// mtime unchanged — move the existing cache entry across unchanged.
    Reuse(String),
    /// New or modified — freshly read and parsed.
    Fresh(String, CachedNote),
    /// Stat or read failed — drop it (and mark the index changed).
    Unreadable,
}

/// Stat one scanned file and decide whether its cache entry can be reused or
/// must be re-read. Pure (only reads `notes` + the filesystem), so it runs on
/// the refresh worker threads. `rel` is the roost-relative path; `path` its
/// absolute location.
fn resolve_entry(
    notes: &BTreeMap<String, CachedNote>,
    rel: &str,
    path: &std::path::Path,
) -> Resolved {
    let Ok(metadata) = std::fs::metadata(path) else {
        return Resolved::Unreadable;
    };
    if metadata.len() > MAX_NOTE_BYTES as u64 {
        return Resolved::Unreadable;
    }
    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    if let Some(existing) = notes.get(rel) {
        if existing.mtime == mtime {
            return Resolved::Reuse(rel.to_string());
        }
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        return Resolved::Unreadable;
    };
    let stem = std::path::Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let link_targets = extract_link_targets(&content);
    Resolved::Fresh(
        rel.to_string(),
        CachedNote {
            mtime,
            content,
            stem,
            link_targets,
        },
    )
}

impl RoostIndex {
    pub fn new() -> Self {
        Self {
            notes: BTreeMap::new(),
            out_links: HashMap::new(),
            in_links: HashMap::new(),
            stems: HashMap::new(),
            paths: HashMap::new(),
        }
    }

    /// Sync the cache to disk: stat every markdown file, re-read only the
    /// changed ones, drop the deleted ones. Rebuilds the link graph when
    /// anything changed. Read failures skip the file rather than abort —
    /// a single unreadable note shouldn't blank the whole index.
    ///
    /// The stat+read sweep is I/O-bound and runs on every read tool, so for
    /// large roosts it's fanned out across the available cores; the merge
    /// (and the graph rebuild) stays single-threaded.
    pub fn refresh(&mut self, roost: &Roost) {
        let mut indexed_bytes = 0u64;
        let entries: Vec<_> = roost
            .scan_md()
            .into_iter()
            .filter(|(_, path)| {
                let Ok(len) = std::fs::metadata(path).map(|m| m.len()) else {
                    return false;
                };
                if len > MAX_NOTE_BYTES as u64
                    || indexed_bytes.saturating_add(len) > MAX_INDEX_BYTES
                {
                    return false;
                }
                indexed_bytes += len;
                true
            })
            .collect();

        // Phase 1 — resolve every entry (stat, and re-read iff mtime moved).
        // Pure reads against an immutable borrow of the cache, so it's safe
        // to spread across worker threads. Small roosts run inline to skip
        // the thread-spawn overhead.
        let resolved: Vec<Resolved> = {
            let notes = &self.notes;
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            if cores <= 1 || entries.len() < PARALLEL_SCAN_THRESHOLD {
                entries
                    .iter()
                    .map(|(rel, path)| resolve_entry(notes, rel, path))
                    .collect()
            } else {
                let chunk = entries.len().div_ceil(cores);
                std::thread::scope(|s| {
                    let handles: Vec<_> = entries
                        .chunks(chunk)
                        .map(|slice| {
                            s.spawn(move || {
                                slice
                                    .iter()
                                    .map(|(rel, path)| resolve_entry(notes, rel, path))
                                    .collect::<Vec<_>>()
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .flat_map(|h| h.join().expect("scan worker panicked"))
                        .collect()
                })
            }
        };

        // Phase 2 — merge results single-threaded. Reused notes are moved out
        // of the old cache; anything left in it afterward was deleted on disk.
        let mut next: BTreeMap<String, CachedNote> = BTreeMap::new();
        let mut changed = false;
        for r in resolved {
            match r {
                Resolved::Reuse(rel) => match self.notes.remove(&rel) {
                    Some(note) => {
                        next.insert(rel, note);
                    }
                    // The mtime matched a cache entry that's now gone — only
                    // possible under a data race; re-reading next call heals it.
                    None => changed = true,
                },
                Resolved::Fresh(rel, note) => {
                    changed = true;
                    next.insert(rel, note);
                }
                Resolved::Unreadable => changed = true,
            }
        }
        if !self.notes.is_empty() {
            changed = true;
        }
        self.notes = next;

        if changed {
            self.rebuild_graph();
        }
    }

    /// Recompute `out_links` / `in_links` from the cached notes' raw link
    /// targets, resolving each `[[target]]` to a known note.
    fn rebuild_graph(&mut self) {
        // Resolution tables (kept on self): every rel by normalized path, and
        // EVERY candidate per stem — so collisions can be ranked by proximity
        // and ambiguity reported, not silently first-won.
        let mut stems: HashMap<String, Vec<String>> = HashMap::new();
        let mut paths: HashMap<String, String> = HashMap::new();
        for (rel, note) in &self.notes {
            let key = strip_markdown_suffix(rel).to_lowercase();
            paths.entry(key).or_insert_with(|| rel.clone());
            stems
                .entry(note.stem.clone())
                .or_default()
                .push(rel.clone());
        }
        for v in stems.values_mut() {
            v.sort();
        }
        self.stems = stems;
        self.paths = paths;

        let mut out_links: HashMap<String, Vec<String>> = HashMap::new();
        let mut in_links: HashMap<String, Vec<String>> = HashMap::new();
        for (rel, note) in &self.notes {
            let source_dir = dir_of(rel);
            let mut seen: HashSet<String> = HashSet::new();
            let mut outs: Vec<String> = Vec::new();
            for raw in &note.link_targets {
                // Same resolution path as the diagnostics; an ambiguous link
                // still gets its best-guess edge so the graph stays connected.
                let target = match self.classify_link(raw, &source_dir) {
                    LinkClass::Resolved(t) => t,
                    LinkClass::Ambiguous { best, .. } => best,
                    LinkClass::Dangling => continue,
                };
                if &target == rel || !seen.insert(target.clone()) {
                    continue; // skip self-links and duplicates
                }
                in_links
                    .entry(target.clone())
                    .or_default()
                    .push(rel.clone());
                outs.push(target);
            }
            out_links.insert(rel.clone(), outs);
        }
        self.out_links = out_links;
        self.in_links = in_links;
    }

    // ── Read queries ───────────────────────────────────────────────────

    /// Resolve a note reference (roost path, `path.md`, or bare title) to a
    /// cached rel path. Mirrors the server's `resolve_note_ref` but against
    /// the in-memory set so graph tools don't re-walk the disk.
    pub fn resolve_ref(&self, reference: &str) -> Option<String> {
        let lower = reference.to_ascii_lowercase();
        let looks_like_path = reference.contains('/')
            || reference.contains('\\')
            || lower.ends_with(".md")
            || lower.ends_with(".markdown");
        if looks_like_path {
            let normalized = reference.replace('\\', "/");
            let key = strip_markdown_suffix(&normalized).to_lowercase();
            return self
                .notes
                .keys()
                .find(|rel| strip_markdown_suffix(rel).to_lowercase() == key)
                .cloned();
        }
        let want = reference.to_lowercase();
        self.notes
            .iter()
            .find(|(_, n)| n.stem == want)
            .map(|(rel, _)| rel.clone())
    }

    /// Cached body of a note, if present. Lets callers reuse the refresh
    /// sweep's read instead of going back to disk per note.
    pub fn content(&self, rel: &str) -> Option<&str> {
        self.notes.get(rel).map(|n| n.content.as_str())
    }

    /// Multi-term AND search over titles + bodies. A note matches only when
    /// EVERY term appears (in its path or body). Ranked: title hits weigh
    /// heavily, then body occurrence count. Optional folder-prefix, tag, and
    /// project filters narrow the set first (before ranking/truncation, so
    /// scoping never drops in-scope hits in favor of out-of-scope ones).
    pub fn search(
        &self,
        terms: &[String],
        folder: Option<&str>,
        tag: Option<&str>,
        project: Option<&str>,
        limit: usize,
    ) -> Vec<SearchHit> {
        let folder_norm = folder.map(|f| f.trim_matches(['/', '\\']).to_lowercase());
        let mut hits: Vec<SearchHit> = Vec::new();

        for (rel, note) in &self.notes {
            if let Some(fp) = &folder_norm {
                if !rel.to_lowercase().starts_with(fp.as_str()) {
                    continue;
                }
            }
            if let Some(t) = tag {
                if !crate::frontmatter::matches(&note.content, "tag", t) {
                    continue;
                }
            }
            if let Some(p) = project {
                if !note_in_project(rel, &note.content, p) {
                    continue;
                }
            }

            let rel_l = rel.to_lowercase();
            let body_l = note.content.to_lowercase();

            // AND: every term must land somewhere (path or body).
            if !terms
                .iter()
                .all(|t| rel_l.contains(t.as_str()) || body_l.contains(t.as_str()))
            {
                continue;
            }

            let mut score = 0u32;
            for t in terms {
                if rel_l.contains(t.as_str()) {
                    score += 5;
                }
                score += body_l.matches(t.as_str()).count() as u32;
            }
            hits.push(SearchHit {
                rel: rel.clone(),
                score,
                snippet: snippet_for(&note.content, terms),
            });
        }

        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.rel.cmp(&b.rel)));
        hits.truncate(limit);
        hits
    }

    /// Notes modified within the last `days`, newest first. `now` is passed
    /// in (not read here) so callers control the clock for testing.
    pub fn recent(&self, days: u64, limit: usize, now: SystemTime) -> Vec<RecentHit> {
        let cutoff = now
            .checked_sub(std::time::Duration::from_secs(days.saturating_mul(86_400)))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut rows: Vec<(&String, SystemTime)> = self
            .notes
            .iter()
            .filter(|(_, n)| n.mtime >= cutoff)
            .map(|(rel, n)| (rel, n.mtime))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        rows.truncate(limit);
        rows.into_iter()
            .map(|(rel, mtime)| RecentHit {
                rel: rel.clone(),
                modified: fmt_local(mtime),
            })
            .collect()
    }

    pub fn stats(&self) -> RoostStats {
        let mut open_tasks = 0usize;
        let mut folders: HashSet<&str> = HashSet::new();
        let today = crate::conventions::today_local().0;
        for (rel, note) in &self.notes {
            open_tasks += parse_open_tasks(
                &note.content,
                crate::conventions::note_date_from_path(rel).as_deref(),
                Some(&today),
            )
            .len();
            if let Some(slash) = rel.rfind('/') {
                folders.insert(&rel[..slash]);
            }
        }
        let links = self.out_links.values().map(Vec::len).sum();
        RoostStats {
            notes: self.notes.len(),
            open_tasks,
            folders: folders.len(),
            links,
        }
    }

    /// Notes whose roost-relative path contains `query` (case-insensitive),
    /// sorted, capped at `limit`.
    pub fn find_paths(&self, query: &str, limit: usize) -> Vec<String> {
        let q = query.to_lowercase();
        self.notes
            .keys()
            .filter(|rel| rel.to_lowercase().contains(&q))
            .take(limit)
            .cloned()
            .collect()
    }

    /// Notes matching a frontmatter property `key = value` (tag membership +
    /// inline `#tags` handled by `frontmatter::matches`), capped at `limit`.
    pub fn find_by_property(&self, key: &str, value: &str, limit: usize) -> Vec<String> {
        let mut out = Vec::new();
        for (rel, note) in &self.notes {
            if crate::frontmatter::matches(&note.content, key, value) {
                out.push(rel.clone());
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    /// Every OPEN task across the roost as `(rel, task)`. Honors `tasks: off`
    /// (the parser drops those notes), so this stays in step with the app.
    pub fn all_open_tasks(&self) -> Vec<(String, crate::conventions::OpenTask)> {
        let mut out = Vec::new();
        let today = crate::conventions::today_local().0;
        for (rel, note) in &self.notes {
            for t in parse_open_tasks(
                &note.content,
                crate::conventions::note_date_from_path(rel).as_deref(),
                Some(&today),
            ) {
                out.push((rel.clone(), t));
            }
        }
        out
    }

    /// Every tag across the roost with its use count, most-used first (ties
    /// broken alphabetically). Frontmatter `tags` + inline `#tags`, deduped
    /// per note so one note tagged `#sync` twice counts once. This is the
    /// existing tag vocabulary a caller should reuse instead of inventing
    /// near-duplicate variants.
    pub fn tag_counts(&self) -> Vec<(String, usize)> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for note in self.notes.values() {
            let mut seen: HashSet<String> = HashSet::new();
            for t in crate::frontmatter::all_tags(&note.content) {
                if seen.insert(t.to_lowercase()) {
                    *counts.entry(t).or_default() += 1;
                }
            }
        }
        let mut v: Vec<(String, usize)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// Classify a raw `[[target]]` against the roost, given the linking note's
    /// folder (`source_dir`, lowercased, no filename). Resolved (one note or a
    /// confident proximity winner), ambiguous (stem shared, no winner), or
    /// dangling. This is the single resolution path — `rebuild_graph` uses it
    /// to pick edges too, so the graph and the diagnostics never disagree.
    pub fn classify_link(&self, raw: &str, source_dir: &str) -> LinkClass {
        let head = raw.split(['|', '#', '^']).next().unwrap_or(raw).trim();
        if head.is_empty() {
            return LinkClass::Dangling;
        }
        let norm = head.replace('\\', "/").to_lowercase();
        let norm = norm.strip_suffix(".md").unwrap_or(&norm);
        if norm.contains('/') {
            if let Some(rel) = self.paths.get(norm) {
                return LinkClass::Resolved(rel.clone());
            }
        }
        let stem = norm
            .rsplit_once('/')
            .map(|(_, b)| b.to_string())
            .unwrap_or_else(|| norm.to_string());
        let Some(candidates) = self.stems.get(&stem) else {
            return LinkClass::Dangling;
        };
        if candidates.len() == 1 {
            return LinkClass::Resolved(candidates[0].clone());
        }
        // Rank by folder proximity (shared prefix desc), then shallowest, then
        // lexical — the same bias the app's resolver uses.
        let mut ranked: Vec<&String> = candidates.iter().collect();
        ranked.sort_by(|a, b| {
            shared_dir_prefix(b, source_dir)
                .cmp(&shared_dir_prefix(a, source_dir))
                .then_with(|| a.matches('/').count().cmp(&b.matches('/').count()))
                .then_with(|| a.cmp(b))
        });
        let best = ranked[0].clone();
        let best_prefix = shared_dir_prefix(&best, source_dir);
        let winners = candidates
            .iter()
            .filter(|c| shared_dir_prefix(c, source_dir) == best_prefix)
            .count();
        if best_prefix > 0 && winners == 1 {
            LinkClass::Resolved(best)
        } else {
            let mut sorted = candidates.clone();
            sorted.sort();
            LinkClass::Ambiguous {
                best,
                candidates: sorted,
            }
        }
    }

    /// Dangling + ambiguous `[[links]]` in `content` (deduped by normalized
    /// target), as seen from `source_dir` (the writing note's folder, so
    /// folder-proximity resolution applies). Used at write time so a bad link
    /// surfaces to the caller.
    pub fn check_content_links(
        &self,
        content: &str,
        source_dir: &str,
    ) -> (Vec<String>, Vec<(String, Vec<String>)>) {
        let mut dangling = Vec::new();
        let mut ambiguous = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for raw in extract_link_targets(content) {
            let key = raw
                .split(['|', '#', '^'])
                .next()
                .unwrap_or(&raw)
                .trim()
                .to_lowercase();
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            match self.classify_link(&raw, source_dir) {
                LinkClass::Resolved(_) => {}
                LinkClass::Dangling => dangling.push(raw),
                LinkClass::Ambiguous { candidates, .. } => ambiguous.push((raw, candidates)),
            }
        }
        (dangling, ambiguous)
    }

    /// Every broken (dangling or ambiguous) link across the roost, sorted by
    /// source note then target — the `find_broken_links` audit.
    pub fn broken_links(&self) -> Vec<BrokenLink> {
        let mut out = Vec::new();
        for (rel, note) in &self.notes {
            let source_dir = dir_of(rel);
            let mut seen: HashSet<String> = HashSet::new();
            for raw in &note.link_targets {
                let key = raw
                    .split(['|', '#', '^'])
                    .next()
                    .unwrap_or(raw)
                    .trim()
                    .to_lowercase();
                if key.is_empty() || !seen.insert(key) {
                    continue;
                }
                match self.classify_link(raw, &source_dir) {
                    LinkClass::Resolved(_) => {}
                    LinkClass::Dangling => out.push(BrokenLink {
                        source: rel.clone(),
                        target: raw.clone(),
                        candidates: Vec::new(),
                    }),
                    LinkClass::Ambiguous { candidates, .. } => out.push(BrokenLink {
                        source: rel.clone(),
                        target: raw.clone(),
                        candidates,
                    }),
                }
            }
        }
        out.sort_by(|a, b| {
            a.source
                .cmp(&b.source)
                .then_with(|| a.target.cmp(&b.target))
        });
        out
    }

    /// Notes that link TO `rel` (incoming wiki-links).
    pub fn backlinks(&self, rel: &str) -> Vec<String> {
        let mut v = self.in_links.get(rel).cloned().unwrap_or_default();
        v.sort();
        v
    }

    /// Notes `rel` links TO (outgoing wiki-links, resolved).
    pub fn forward_links(&self, rel: &str) -> Vec<String> {
        let mut v = self.out_links.get(rel).cloned().unwrap_or_default();
        v.sort();
        v
    }

    /// BFS out from `rel` to `depth` hops over the UNDIRECTED link graph.
    /// Returns `(rel, hop)` for every reachable note (excluding the focus),
    /// closest first. Stops early — possibly before `depth` is reached — once
    /// `NEIGHBORS_MAX_RESULTS` notes have been visited, so a dense graph at
    /// shallow depth can't force an unbounded BFS or an unbounded reply.
    pub fn neighbors(&self, rel: &str, depth: usize) -> (Vec<(String, usize)>, bool) {
        let mut visited: HashMap<String, usize> = HashMap::new();
        visited.insert(rel.to_string(), 0);
        let mut frontier: Vec<String> = vec![rel.to_string()];
        let mut truncated = false;
        'hops: for hop in 1..=depth {
            let mut next: Vec<String> = Vec::new();
            for node in &frontier {
                for nb in self.undirected_neighbors(node) {
                    if !visited.contains_key(&nb) {
                        if visited.len().saturating_sub(1) >= NEIGHBORS_MAX_RESULTS {
                            truncated = true;
                            break 'hops;
                        }
                        visited.insert(nb.clone(), hop);
                        next.push(nb);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let mut out: Vec<(String, usize)> = visited.into_iter().filter(|(k, _)| k != rel).collect();
        out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        (out, truncated)
    }

    /// Shortest path between two notes over the UNDIRECTED link graph, as a
    /// list of rels from `a` to `b` inclusive. `None` if disconnected.
    pub fn find_path(&self, a: &str, b: &str) -> Option<Vec<String>> {
        if a == b {
            return Some(vec![a.to_string()]);
        }
        let mut prev: HashMap<String, String> = HashMap::new();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(a.to_string());
        let mut q: VecDeque<String> = VecDeque::new();
        q.push_back(a.to_string());
        while let Some(node) = q.pop_front() {
            for nb in self.undirected_neighbors(&node) {
                if visited.insert(nb.clone()) {
                    prev.insert(nb.clone(), node.clone());
                    if nb == b {
                        // Walk the predecessor chain back from b to a.
                        let mut path = vec![b.to_string()];
                        let mut cur = b.to_string();
                        while let Some(p) = prev.get(&cur).cloned() {
                            path.push(p.clone());
                            if p.as_str() == a {
                                break;
                            }
                            cur = p;
                        }
                        path.reverse();
                        return Some(path);
                    }
                    q.push_back(nb);
                }
            }
        }
        None
    }

    /// PageRank over the directed link graph (damping 0.85, ~40 iterations,
    /// dangling-mass redistributed). Returns `(rel, rank)` for the top
    /// `limit`, highest first — the roost's hub notes. No CozoDB; pure power
    /// iteration over the in-memory edges.
    pub fn top_hubs(&self, limit: usize) -> Vec<(String, f64)> {
        let nodes: Vec<&String> = self.notes.keys().collect();
        let n = nodes.len();
        if n == 0 {
            return Vec::new();
        }
        let idx: HashMap<&str, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, r)| (r.as_str(), i))
            .collect();
        let out: Vec<Vec<usize>> = nodes
            .iter()
            .map(|r| {
                self.out_links
                    .get(r.as_str())
                    .map(|ts| {
                        ts.iter()
                            .filter_map(|t| idx.get(t.as_str()).copied())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();

        let d = 0.85;
        let base = (1.0 - d) / n as f64;
        let mut rank = vec![1.0 / n as f64; n];
        for _ in 0..40 {
            let mut next = vec![base; n];
            let mut dangling = 0.0;
            for i in 0..n {
                if out[i].is_empty() {
                    dangling += rank[i];
                    continue;
                }
                let share = rank[i] / out[i].len() as f64;
                for &j in &out[i] {
                    next[j] += d * share;
                }
            }
            // Spread dangling-node mass evenly so ranks keep summing to ~1.
            let spread = d * dangling / n as f64;
            for slot in next.iter_mut() {
                *slot += spread;
            }
            rank = next;
        }

        let mut scored: Vec<(String, f64)> = nodes
            .iter()
            .enumerate()
            .map(|(i, r)| ((*r).clone(), rank[i]))
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(limit);
        scored
    }

    /// Neighbors in EITHER direction (out-links ∪ back-links).
    fn undirected_neighbors(&self, rel: &str) -> Vec<String> {
        let mut v: Vec<String> = self.out_links.get(rel).cloned().unwrap_or_default();
        if let Some(ins) = self.in_links.get(rel) {
            for r in ins {
                if !v.contains(r) {
                    v.push(r.clone());
                }
            }
        }
        v
    }

    /// Is there a wiki-link edge between `a` and `b` in either direction?
    fn linked(&self, a: &str, b: &str) -> bool {
        self.out_links
            .get(a)
            .is_some_and(|ts| ts.iter().any(|t| t == b))
            || self
                .out_links
                .get(b)
                .is_some_and(|ts| ts.iter().any(|t| t == a))
    }

    /// Propose MISSING links — related note pairs with no edge between them.
    /// Two mechanical signals, no LLM:
    ///
    /// 1. **Unlinked mention** (strong): a note's body contains another
    ///    note's title as plain text (whole-word) but never links it. Noise
    ///    guards: the title must be ≥ 4 chars, contain a letter (drops
    ///    daily-note dates), and be unambiguous (exactly one note owns the
    ///    stem — a shared stem would be a guess).
    /// 2. **Shared rare tag** (weak): both notes carry a tag used by only
    ///    `RARE_TAG_MAX` notes roost-wide. A tag on 50 notes says nothing;
    ///    one on 3 is a real topic cluster.
    ///
    /// `scope` limits suggestions to pairs involving that one note (already
    /// resolved to a rel) — the "garden the note I just wrote" mode. Mentions
    /// outrank tag-only pairs; capped at `limit`.
    pub fn suggest_links(&self, scope: Option<&str>, limit: usize) -> Vec<LinkSuggestion> {
        const RARE_TAG_MAX: usize = 8;

        // Mention candidates: unambiguous, letter-bearing, ≥4-char stems.
        // `first_word` powers the per-note word-set prefilter below.
        struct Candidate<'a> {
            stem: &'a str,
            rel: &'a str,
            first_word: &'a str,
        }
        let candidates: Vec<Candidate> = self
            .stems
            .iter()
            .filter(|(stem, rels)| {
                rels.len() == 1 && stem.len() >= 4 && stem.chars().any(|c| c.is_alphabetic())
            })
            .filter_map(|(stem, rels)| {
                let first_word = stem
                    .split(|c: char| !c.is_alphanumeric())
                    .find(|w| !w.is_empty())?;
                Some(Candidate {
                    stem,
                    rel: rels[0].as_str(),
                    first_word,
                })
            })
            .collect();

        // Keyed by unordered pair so a mention and a shared tag on the same
        // two notes merge into one suggestion. BTreeMap = deterministic.
        let mut pairs: BTreeMap<(String, String), LinkSuggestion> = BTreeMap::new();

        // Signal 1 — unlinked mentions.
        for (rel, note) in &self.notes {
            // Scoped mode: inside the scoped note check every title; in any
            // other note only look for the scoped note's own title.
            let relevant: Vec<&Candidate> = match scope {
                None => candidates.iter().collect(),
                Some(s) if s == rel => candidates.iter().collect(),
                Some(s) => candidates.iter().filter(|c| c.rel == s).collect(),
            };
            if relevant.is_empty() {
                continue;
            }
            let body_l = note.content.to_lowercase();
            let words: HashSet<&str> = body_l
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
                .collect();
            for c in relevant {
                if c.rel == rel || !words.contains(c.first_word) {
                    continue;
                }
                let Some(pos) = find_whole_word(&body_l, c.stem) else {
                    continue;
                };
                if self.linked(rel, c.rel) {
                    continue;
                }
                let key = pair_key(rel, c.rel);
                let entry = pairs.entry(key).or_insert_with(|| LinkSuggestion {
                    source: rel.clone(),
                    target: c.rel.to_string(),
                    mentioned: false,
                    shared_tags: Vec::new(),
                    evidence: String::new(),
                    score: 0,
                });
                // A mention fixes the direction: source = the mentioning note.
                entry.source = rel.clone();
                entry.target = c.rel.to_string();
                entry.mentioned = true;
                entry.evidence = line_around(&note.content, pos);
                entry.score += 10;
            }
        }

        // Signal 2 — shared rare tags. Tag → notes map, deduped per note
        // (same shape as `tag_counts`).
        let mut tag_notes: HashMap<String, Vec<&str>> = HashMap::new();
        for (rel, note) in &self.notes {
            let mut seen: HashSet<String> = HashSet::new();
            for t in crate::frontmatter::all_tags(&note.content) {
                let t = t.to_lowercase();
                if seen.insert(t.clone()) {
                    tag_notes.entry(t).or_default().push(rel.as_str());
                }
            }
        }
        let mut tags: Vec<(&String, &Vec<&str>)> = tag_notes
            .iter()
            .filter(|(_, rels)| (2..=RARE_TAG_MAX).contains(&rels.len()))
            .collect();
        tags.sort_by(|a, b| a.0.cmp(b.0)); // deterministic across HashMap order
        for (tag, rels) in tags {
            let count = rels.len();
            for i in 0..rels.len() {
                for j in i + 1..rels.len() {
                    let (a, b) = (rels[i], rels[j]);
                    if let Some(s) = scope {
                        if s != a && s != b {
                            continue;
                        }
                    }
                    if self.linked(a, b) {
                        continue;
                    }
                    let entry = pairs
                        .entry(pair_key(a, b))
                        .or_insert_with(|| LinkSuggestion {
                            source: a.min(b).to_string(),
                            target: a.max(b).to_string(),
                            mentioned: false,
                            shared_tags: Vec::new(),
                            evidence: String::new(),
                            score: 0,
                        });
                    entry.shared_tags.push((tag.clone(), count));
                    entry.score += (RARE_TAG_MAX + 1 - count) as u32;
                }
            }
        }

        let mut out: Vec<LinkSuggestion> = pairs.into_values().collect();
        out.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.source.cmp(&b.source))
                .then_with(|| a.target.cmp(&b.target))
        });
        out.truncate(limit);
        out
    }
}

// ── free helpers ────────────────────────────────────────────────────────

/// Forgiving project scope (powers `recall`): a note belongs to `project`
/// when its frontmatter says so (`project: X`), it carries the project as a
/// tag (frontmatter or inline), or the project name appears anywhere in its
/// path — roosts organized by folder (`Corvux/...`) have no `project`
/// property, so path matching keeps them scopeable. Case-insensitive.
fn note_in_project(rel: &str, content: &str, project: &str) -> bool {
    let p = project.trim();
    if p.is_empty() {
        return true;
    }
    rel.to_lowercase().contains(&p.to_lowercase())
        || crate::frontmatter::matches(content, "project", p)
        || crate::frontmatter::matches(content, "tag", p)
}

/// Unordered pair key so `(a,b)` and `(b,a)` land on one suggestion entry.
fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

/// Byte offset of the first WHOLE-WORD occurrence of `needle` in `hay`
/// (both already lowercased): the chars just before and after the match
/// must not be alphanumeric, so "sessions" never counts as a mention of
/// "session". Multi-word needles ("session bug") match across their exact
/// spacing only.
fn find_whole_word(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let start = from + i;
        let end = start + needle.len();
        let before_ok = hay[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = hay[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return Some(start);
        }
        // Step past this occurrence (at least one char) and keep looking.
        from = start + hay[start..].chars().next().map_or(1, char::len_utf8);
        if from >= hay.len() {
            break;
        }
    }
    None
}

/// The (trimmed, capped) line of `content` containing byte offset `pos` —
/// mention evidence for a `LinkSuggestion`. `pos` comes from a search over
/// the lowercased content; lowercasing is 1:1 on byte positions for the
/// ASCII common case, and for the rare non-ASCII drift the nearest newline
/// bounds still produce a sane line, so this stays best-effort on purpose.
fn line_around(content: &str, pos: usize) -> String {
    let pos = pos.min(content.len());
    let pos = (0..=pos)
        .rev()
        .find(|&p| content.is_char_boundary(p))
        .unwrap_or(0);
    let start = content[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = content[pos..]
        .find('\n')
        .map(|i| pos + i)
        .unwrap_or(content.len());
    cap_snippet(content[start..end].trim())
}

/// Pull `[[target]]` link targets out of body text. Keeps the raw target
/// (alias/anchor stripping happens at resolution time).
fn extract_link_targets(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(end) = content[i + 2..].find("]]") {
                let inner = &content[i + 2..i + 2 + end];
                let trimmed = inner.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                i = i + 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// A note rel's folder portion, lowercased (no filename). `""` for root.
fn dir_of(rel: &str) -> String {
    rel.rsplit_once('/')
        .map(|(d, _)| d.to_lowercase())
        .unwrap_or_default()
}

/// Count matching leading folder segments between a candidate rel's folder and
/// the linking note's folder (`source_dir`, already lowercased). Higher =
/// closer in the roost tree. Mirrors the app's `shared_dir_prefix`.
fn shared_dir_prefix(candidate_rel: &str, source_dir: &str) -> usize {
    let cand_dir = candidate_rel
        .rsplit_once('/')
        .map(|(d, _)| d.to_lowercase())
        .unwrap_or_default();
    cand_dir
        .split('/')
        .filter(|s| !s.is_empty())
        .zip(source_dir.split('/').filter(|s| !s.is_empty()))
        .take_while(|(x, y)| x == y)
        .count()
}

/// First body line containing any term, plus the next non-empty line for
/// context (2 lines). Trimmed and length-capped so snippets stay scannable.
fn snippet_for(content: &str, terms: &[String]) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let lc: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    let first = lc
        .iter()
        .position(|l| terms.iter().any(|t| l.contains(t.as_str())));
    let Some(i) = first else {
        return lines
            .iter()
            .find(|l| !l.trim().is_empty())
            .map(|l| cap_snippet(l.trim()))
            .unwrap_or_default();
    };
    let mut snip = lines[i].trim().to_string();
    if let Some(nextline) = lines[i + 1..].iter().find(|l| !l.trim().is_empty()) {
        snip.push(' ');
        snip.push_str(nextline.trim());
    }
    cap_snippet(&snip)
}

fn cap_snippet(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// `SystemTime` → `YYYY-MM-DD HH:MM` in the user's local timezone.
fn fmt_local(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = chrono::DateTime::<chrono::Utc>::from(t).into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn roost_with(files: &[(&str, &str)]) -> (TempDir, Roost) {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".corvux")).unwrap();
        let v = Roost::open(dir.path().to_path_buf()).unwrap();
        for (rel, content) in files {
            v.write_atomic(rel, content).unwrap();
        }
        (dir, v)
    }

    fn fresh(files: &[(&str, &str)]) -> (TempDir, Roost, RoostIndex) {
        let (d, v) = roost_with(files);
        let mut idx = RoostIndex::new();
        idx.refresh(&v);
        (d, v, idx)
    }

    #[test]
    fn search_requires_all_terms_and_ranks_title_first() {
        let (_d, _v, idx) = fresh(&[
            ("Atlas Project.md", "# Atlas\n\nrocket science notes\n"),
            ("Notes.md", "# Notes\n\nrocket fuel and atlas mentions\n"),
            ("Other.md", "# Other\n\nunrelated\n"),
        ]);
        // Both terms must appear; the note with the title hit ranks first.
        let hits = idx.search(&["atlas".into(), "rocket".into()], None, None, None, 10);
        let rels: Vec<&str> = hits.iter().map(|h| h.rel.as_str()).collect();
        assert_eq!(rels, vec!["Atlas Project.md", "Notes.md"]);
        // A term that matches nothing yields no hits (AND semantics).
        assert!(idx
            .search(&["atlas".into(), "missing".into()], None, None, None, 10)
            .is_empty());
    }

    #[test]
    fn search_folder_filter() {
        let (_d, _v, idx) = fresh(&[
            ("Projects/A.md", "alpha topic\n"),
            ("Daily/B.md", "alpha topic\n"),
        ]);
        let hits = idx.search(&["alpha".into()], Some("Projects"), None, None, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rel, "Projects/A.md");
    }

    #[test]
    fn search_project_scope_matches_property_tag_and_path() {
        let (_d, _v, idx) = fresh(&[
            ("Corvux/A.md", "alpha topic\n"),
            ("Other/B.md", "---\nproject: corvux\n---\n\nalpha topic\n"),
            ("Other/C.md", "---\ntags:\n  - Corvux\n---\n\nalpha topic\n"),
            ("Other/D.md", "alpha topic\n"),
        ]);
        let hits = idx.search(&["alpha".into()], None, None, Some("corvux"), 10);
        let rels: Vec<&str> = hits.iter().map(|h| h.rel.as_str()).collect();
        assert!(rels.contains(&"Corvux/A.md"), "path segment should match");
        assert!(
            rels.contains(&"Other/B.md"),
            "project property should match"
        );
        assert!(rels.contains(&"Other/C.md"), "tag should match");
        assert!(
            !rels.contains(&"Other/D.md"),
            "unscoped note must be excluded"
        );
    }

    #[test]
    fn search_project_scope_applies_before_truncation() {
        // The out-of-scope note scores far higher; scoping must run before
        // the limit cut or the in-scope hit would be silently dropped.
        let (_d, _v, idx) = fresh(&[
            ("Other/Loud.md", "alpha alpha alpha alpha alpha\n"),
            ("Corvux/Quiet.md", "alpha once\n"),
        ]);
        let hits = idx.search(&["alpha".into()], None, None, Some("corvux"), 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rel, "Corvux/Quiet.md");
    }

    #[test]
    fn suggest_links_finds_unlinked_mention_and_skips_linked_pairs() {
        let (_d, _v, idx) = fresh(&[
            (
                "Auth flow.md",
                "# Auth flow\n\nWe hit the session bug again during login.\n",
            ),
            (
                "Session bug.md",
                "# Session bug\n\nTimeout after refresh.\n",
            ),
            (
                "Linked.md",
                "# Linked\n\nAlso the session bug, but see [[Session bug]].\n",
            ),
        ]);
        let s = idx.suggest_links(None, 20);
        let hit = s
            .iter()
            .find(|x| x.source == "Auth flow.md")
            .expect("mention suggestion");
        assert_eq!(hit.target, "Session bug.md");
        assert!(hit.mentioned);
        assert!(hit.evidence.contains("session bug"));
        // Linked.md already links the pair → never suggested.
        assert!(!s.iter().any(|x| x.source == "Linked.md"));
    }

    #[test]
    fn suggest_links_requires_whole_word_and_letters_in_stem() {
        let (_d, _v, idx) = fresh(&[
            ("Session.md", "# Session\n"),
            ("A.md", "# A\n\nmany sessions happened\n"),
            ("Daily Notes/2026-07-18.md", "# 2026-07-18\n"),
            ("B.md", "# B\n\nshipped on 2026-07-18 evening\n"),
        ]);
        assert!(idx.suggest_links(None, 20).is_empty());
    }

    #[test]
    fn suggest_links_skips_ambiguous_stems() {
        let (_d, _v, idx) = fresh(&[
            ("Work/Index.md", "# WI\n"),
            ("Personal/Index.md", "# PI\n"),
            ("A.md", "# A\n\ncheck the index folder\n"),
        ]);
        assert!(idx.suggest_links(None, 20).is_empty());
    }

    #[test]
    fn suggest_links_pairs_rare_tags_but_not_common_ones() {
        let mut files: Vec<(String, String)> = vec![
            (
                "P1.md".into(),
                "---\ntags:\n  - quic\n---\n\n# P1\ntransport\n".into(),
            ),
            (
                "P2.md".into(),
                "---\ntags:\n  - quic\n---\n\n# P2\nhandshake\n".into(),
            ),
        ];
        for i in 0..9 {
            files.push((
                format!("C{i}.md"),
                "---\ntags:\n  - common\n---\nbody\n".into(),
            ));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, _v, idx) = fresh(&refs);
        let s = idx.suggest_links(None, 50);
        assert_eq!(s.len(), 1, "only the rare-tag pair");
        assert_eq!(s[0].source, "P1.md");
        assert_eq!(s[0].target, "P2.md");
        assert!(!s[0].mentioned);
        assert_eq!(s[0].shared_tags, vec![("quic".to_string(), 2)]);
    }

    #[test]
    fn suggest_links_scope_limits_to_one_note_and_mentions_outrank_tags() {
        let (_d, _v, idx) = fresh(&[
            (
                "Focus.md",
                "---\ntags:\n  - rare\n---\n\n# Focus\n\nsee also deep dive\n",
            ),
            ("Deep dive.md", "# Deep dive\n"),
            ("Other.md", "---\ntags:\n  - rare\n---\n\n# Other\n"),
            ("Elsewhere.md", "---\ntags:\n  - rare\n---\n\n# Elsewhere\n"),
        ]);
        let s = idx.suggest_links(Some("Focus.md"), 20);
        assert!(s
            .iter()
            .all(|x| x.source == "Focus.md" || x.target == "Focus.md"));
        assert!(!s
            .iter()
            .any(|x| x.source == "Elsewhere.md" && x.target == "Other.md"));
        assert!(s[0].mentioned);
        assert_eq!(s[0].target, "Deep dive.md");
    }

    #[test]
    fn backlinks_and_forward_links_resolve_wikilinks() {
        let (_d, _v, idx) = fresh(&[
            (
                "Atlas.md",
                "# Atlas\n\nlinks to [[Beta]] and [[Projects/Gamma]]\n",
            ),
            ("Beta.md", "# Beta\n"),
            ("Projects/Gamma.md", "# Gamma\n\nback to [[Atlas]]\n"),
        ]);
        assert_eq!(
            idx.forward_links("Atlas.md"),
            vec!["Beta.md", "Projects/Gamma.md"]
        );
        assert_eq!(idx.backlinks("Beta.md"), vec!["Atlas.md"]);
        // Gamma → Atlas (by stem) and Atlas → Gamma (by path) both resolved.
        assert_eq!(idx.backlinks("Atlas.md"), vec!["Projects/Gamma.md"]);
    }

    #[test]
    fn find_path_bfs_shortest() {
        let (_d, _v, idx) = fresh(&[
            ("A.md", "[[B]]\n"),
            ("B.md", "[[C]]\n"),
            ("C.md", "[[D]]\n"),
            ("D.md", "leaf\n"),
            ("Z.md", "island\n"),
        ]);
        let path = idx.find_path("A.md", "D.md").unwrap();
        assert_eq!(path, vec!["A.md", "B.md", "C.md", "D.md"]);
        assert!(idx.find_path("A.md", "Z.md").is_none());
    }

    #[test]
    fn neighbors_depth_limited() {
        let (_d, _v, idx) = fresh(&[("A.md", "[[B]]\n"), ("B.md", "[[C]]\n"), ("C.md", "leaf\n")]);
        let (one, one_truncated) = idx.neighbors("A.md", 1);
        assert_eq!(one, vec![("B.md".to_string(), 1)]);
        assert!(!one_truncated);
        let (two, two_truncated) = idx.neighbors("A.md", 2);
        assert_eq!(two, vec![("B.md".to_string(), 1), ("C.md".to_string(), 2)]);
        assert!(!two_truncated);
    }

    #[test]
    fn top_hubs_ranks_the_hub_highest() {
        let (_d, _v, idx) = fresh(&[
            ("Hub.md", "the hub\n"),
            ("A.md", "[[Hub]]\n"),
            ("B.md", "[[Hub]]\n"),
            ("C.md", "[[Hub]]\n"),
        ]);
        let hubs = idx.top_hubs(4);
        assert_eq!(hubs[0].0, "Hub.md", "most-linked note must rank first");
    }

    #[test]
    fn stats_counts_notes_tasks_folders_links() {
        // Convention 0.3: a checkbox is a task only with an effective date,
        // so the open checkboxes carry an inline 📅 to qualify.
        let (_d, _v, idx) = fresh(&[
            (
                "Projects/A.md",
                "# A\n\n- [ ] do thing 📅 2026-06-12\n[[B]]\n",
            ),
            ("B.md", "# B\n\n- [ ] another 📅 2026-06-12\n- [x] done\n"),
        ]);
        let s = idx.stats();
        assert_eq!(s.notes, 2);
        assert_eq!(s.open_tasks, 2); // both open + dated; the [x] is excluded
        assert_eq!(s.folders, 1); // "Projects"
        assert_eq!(s.links, 1); // A -> B
    }

    #[test]
    fn refresh_reuses_unchanged_and_picks_up_new() {
        let (_d, v) = roost_with(&[("A.md", "[[B]]\n"), ("B.md", "b\n")]);
        let mut idx = RoostIndex::new();
        idx.refresh(&v);
        assert_eq!(idx.stats().notes, 2);
        // Add a note and re-refresh — graph updates.
        v.write_atomic("C.md", "[[A]]\n").unwrap();
        idx.refresh(&v);
        assert_eq!(idx.stats().notes, 3);
        assert_eq!(idx.backlinks("A.md"), vec!["C.md"]);
    }

    #[test]
    fn refresh_parallel_path_indexes_add_and_delete() {
        // Above PARALLEL_SCAN_THRESHOLD so the sweep fans out across threads —
        // exercises the parallel resolve + the single-threaded merge of
        // reuse / fresh / delete results.
        let n = PARALLEL_SCAN_THRESHOLD + 100;
        let files: Vec<(String, String)> = (0..n)
            .map(|i| {
                // Everyone links the hub (Note-0000) so the graph is non-trivial.
                (
                    format!("Note-{i:04}.md"),
                    format!("# Note {i}\n\n[[Note-0000]]\n"),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(r, c)| (r.as_str(), c.as_str()))
            .collect();
        let (_d, v) = roost_with(&refs);
        let mut idx = RoostIndex::new();
        idx.refresh(&v);
        assert_eq!(idx.stats().notes, n);
        // The hub is linked by every note except itself.
        assert_eq!(idx.backlinks("Note-0000.md").len(), n - 1);
        assert_eq!(idx.top_hubs(1)[0].0, "Note-0000.md");

        // Delete one note and add another. The parallel sweep must reflect
        // both while reusing the untouched majority; net count is unchanged.
        std::fs::remove_file(v.root.join("Note-0500.md")).unwrap();
        v.write_atomic(&format!("Note-{n:04}.md"), "[[Note-0000]]\n")
            .unwrap();
        idx.refresh(&v);
        assert_eq!(idx.stats().notes, n);
        assert!(idx.resolve_ref("Note-0500").is_none());
        assert_eq!(idx.backlinks("Note-0000.md").len(), n - 1);
    }

    #[test]
    fn tag_counts_aggregates_and_ranks() {
        let (_d, _v, idx) = fresh(&[
            ("A.md", "---\ntags:\n  - sync\n  - project\n---\n\n# A\n"),
            (
                "B.md",
                "---\ntags: [sync]\n---\n\n# B\n\ninline #sync should count once per note\n",
            ),
            ("C.md", "# C\n\nbody #project and #idea\n"),
        ]);
        let counts = idx.tag_counts();
        // sync: A + B (B's frontmatter+inline dedup to 1) = 2; project: A + C = 2; idea: 1.
        let map: std::collections::HashMap<_, _> = counts.iter().cloned().collect();
        assert_eq!(map.get("sync"), Some(&2));
        assert_eq!(map.get("project"), Some(&2));
        assert_eq!(map.get("idea"), Some(&1));
        // Sorted count-desc then alpha: the two 2-count tags lead, idea last.
        assert_eq!(counts.last().unwrap().0, "idea");
    }

    #[test]
    fn classify_link_and_broken_links_audit() {
        let (_d, _v, idx) = fresh(&[
            ("A.md", "links [[Ideas]], [[Index]], [[Ghost]]\n"),
            ("Ideas.md", "# Ideas\n"),
            ("Work/Index.md", "# WI\n"),
            ("Personal/Index.md", "# PI\n"),
        ]);
        assert!(matches!(
            idx.classify_link("Ideas", ""),
            LinkClass::Resolved(_)
        ));
        assert!(matches!(
            idx.classify_link("Ghost", ""),
            LinkClass::Dangling
        ));
        // From root (no shared folder) the two Index notes are a true guess.
        match idx.classify_link("Index", "") {
            LinkClass::Ambiguous { candidates, .. } => assert_eq!(
                candidates,
                vec!["Personal/Index.md".to_string(), "Work/Index.md".to_string()]
            ),
            _ => panic!("expected ambiguous"),
        }
        // Exact path beats ambiguity.
        assert!(matches!(
            idx.classify_link("Work/Index", ""),
            LinkClass::Resolved(_)
        ));

        let broken = idx.broken_links();
        assert_eq!(broken.len(), 2); // A.md (root): Ghost dangling + Index ambiguous
        assert!(broken
            .iter()
            .any(|b| b.target == "Ghost" && b.candidates.is_empty()));
        assert!(broken
            .iter()
            .any(|b| b.target == "Index" && b.candidates.len() == 2));
    }

    #[test]
    fn classify_link_is_confident_when_folder_proximity_decides() {
        let (_d, _v, idx) = fresh(&[
            ("a/2026/note.md", "x\n"),
            ("b/2026/note.md", "x\n"),
            ("a/2026/src.md", "see [[note]]\n"),
        ]);
        // From a/2026 the link resolves confidently (not flagged).
        assert!(matches!(
            idx.classify_link("note", "a/2026"),
            LinkClass::Resolved(_)
        ));
        // From an unrelated folder it's a genuine guess.
        assert!(matches!(
            idx.classify_link("note", "z"),
            LinkClass::Ambiguous { .. }
        ));
        // The audit must NOT flag src.md's link — proximity resolved it.
        let broken = idx.broken_links();
        assert!(
            broken.iter().all(|b| b.target != "note"),
            "confident proximity link should not be flagged: {:?}",
            broken.iter().map(|b| &b.target).collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolve_ref_by_title_and_path() {
        let (_d, _v, idx) = fresh(&[("Projects/Atlas.md", "# Atlas\n")]);
        assert_eq!(
            idx.resolve_ref("Atlas").as_deref(),
            Some("Projects/Atlas.md")
        );
        assert_eq!(
            idx.resolve_ref("Projects/Atlas.md").as_deref(),
            Some("Projects/Atlas.md")
        );
        assert_eq!(idx.resolve_ref("Nope"), None);
    }

    #[cfg(unix)]
    #[test]
    fn refresh_does_not_index_content_through_a_symlink() {
        use std::os::unix::fs::symlink;
        let (d, v) = roost_with(&[("real.md", "kept content\n")]);
        let outside = d.path().parent().unwrap().join("index-symlink-victim.md");
        fs::write(&outside, "secret leak content").unwrap();
        symlink(&outside, d.path().join("linked.md")).unwrap();

        let mut idx = RoostIndex::new();
        idx.refresh(&v);

        assert_eq!(idx.stats().notes, 1);
        assert!(idx
            .search(&["secret".into()], None, None, None, 10)
            .is_empty());
        assert!(idx.resolve_ref("linked").is_none());

        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn neighbors_caps_total_visited_nodes() {
        // Star graph: a hub linking to more notes than the cap allows, all
        // within 1 hop — the BFS must stop early rather than return them all.
        let n = NEIGHBORS_MAX_RESULTS + 50;
        let hub_body: String = (0..n).map(|i| format!("[[Leaf{i:05}]]\n")).collect();
        let mut files: Vec<(String, String)> = vec![("Hub.md".to_string(), hub_body)];
        for i in 0..n {
            files.push((format!("Leaf{i:05}.md"), "x\n".to_string()));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_d, _v, idx) = fresh(&refs);

        let (nb, truncated) = idx.neighbors("Hub.md", 1);
        assert!(
            nb.len() <= NEIGHBORS_MAX_RESULTS,
            "expected at most {NEIGHBORS_MAX_RESULTS}, got {}",
            nb.len()
        );
        assert!(truncated);
    }
}
