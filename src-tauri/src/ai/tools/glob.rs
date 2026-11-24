//! Lightweight gitignore-style glob matcher for path-level guardrails.
//!
//! Patterns supported (a small subset of gitignore — enough for the
//! `agent_blocked_paths` use case without pulling a regex / globset
//! dependency):
//!
//! | Pattern             | Matches                                  |
//! |---------------------|------------------------------------------|
//! | `Journal`           | `Journal` and everything under `Journal/`|
//! | `Journal/`          | Same — explicit folder form              |
//! | `Journal/**`        | Same — explicit recursive form           |
//! | `Finance/*.md`      | `.md` files directly inside `Finance/`   |
//! | `**/secret.md`      | Any `secret.md` at any depth             |
//! | `.private/**`       | Everything under any `.private/` folder  |
//!
//! Wildcards: `*` matches any chars except `/`, `**` matches any chars
//! including `/`, `?` matches a single non-`/` char. Patterns are
//! matched against roost-relative POSIX paths (forward slashes,
//! leading slashes stripped).

/// True iff `path` matches any of `patterns`. Empty / whitespace-only
/// patterns are skipped. Paths and patterns are normalized to forward
/// slashes and stripped of a leading `/` before matching.
pub fn path_is_blocked(path: &str, patterns: &[String]) -> bool {
    let normalized = normalize(path);
    patterns.iter().any(|p| {
        let p = p.trim();
        !p.is_empty() && matches(&normalize(p), &normalized)
    })
}

/// Canonical form for comparison, applied to BOTH the pattern and the path.
///
/// `agent_blocked_paths` is the one agent control the webview cannot talk its
/// way past (the loop hard-rejects before any approval card exists), so the
/// matcher has to see every spelling the file system treats as the same path
/// (reviewer finding #10):
/// * case-insensitive — `journal/x.md` IS `Journal/x.md` on Windows and macOS,
///   and over-blocking on a case-sensitive file system only errs safe;
/// * Windows ignores trailing dots and spaces in a name (`Journal./x.md`,
///   `Journal /x.md` resolve to `Journal\x.md`), so strip them per segment;
/// * empty segments (`Journal//x.md`) collapse.
fn normalize(s: &str) -> String {
    s.replace('\\', "/")
        .split('/')
        .map(|seg| seg.trim_end_matches(['.', ' ']))
        .filter(|seg| !seg.is_empty())
        .collect::<Vec<_>>()
        .join("/")
        .to_lowercase()
}

/// Match `path` against `pattern`. Trailing `/` on the pattern is
/// already stripped by `normalize`, so a bare pattern like `Journal`
/// matches both the file `Journal` AND any path starting with
/// `Journal/`. Patterns containing wildcards are matched literally —
/// no implicit "folder prefix" rule, because the user explicitly used
/// `*` / `**` to control the shape.
fn matches(pattern: &str, path: &str) -> bool {
    // Wildcardless patterns: exact match OR folder prefix.
    if !pattern.contains('*') && !pattern.contains('?') {
        return path == pattern || path.starts_with(&format!("{pattern}/"));
    }
    let pat: Vec<char> = pattern.chars().collect();
    let pth: Vec<char> = path.chars().collect();
    matches_chars(&pat, &pth)
}

/// Recursive wildcard matcher. `*` doesn't cross `/`; `**` does.
fn matches_chars(pattern: &[char], path: &[char]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(&'*'), _) => {
            // Detect `**` — match zero or more of anything including `/`.
            if pattern.get(1) == Some(&'*') {
                let mut rest = &pattern[2..];
                // Conventionally `**/foo` should match `foo` too — strip
                // the directly-following slash so `**/foo` ≡ `foo` at
                // the path root.
                if rest.first() == Some(&'/') {
                    rest = &rest[1..];
                }
                for i in 0..=path.len() {
                    if matches_chars(rest, &path[i..]) {
                        return true;
                    }
                }
                false
            } else {
                // Single `*` — match zero or more non-`/` chars.
                let rest = &pattern[1..];
                for i in 0..=path.len() {
                    if matches_chars(rest, &path[i..]) {
                        return true;
                    }
                    if path.get(i) == Some(&'/') {
                        break;
                    }
                }
                false
            }
        }
        (Some(&'?'), Some(&c)) if c != '/' => matches_chars(&pattern[1..], &path[1..]),
        (Some(&p), Some(&c)) if p == c => matches_chars(&pattern[1..], &path[1..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn empty_patterns_block_nothing() {
        assert!(!path_is_blocked("Journal/2026-05-20.md", &pats(&[])));
        assert!(!path_is_blocked(
            "Journal/2026-05-20.md",
            &pats(&["", "   "])
        ));
    }

    #[test]
    fn bare_folder_blocks_self_and_children() {
        let p = pats(&["Journal"]);
        assert!(path_is_blocked("Journal", &p));
        assert!(path_is_blocked("Journal/2026-05-20.md", &p));
        assert!(path_is_blocked("Journal/sub/nested.md", &p));
        assert!(!path_is_blocked("JournalArchive/x.md", &p));
        assert!(!path_is_blocked("Other/Journal.md", &p));
    }

    #[test]
    fn trailing_slash_is_equivalent_to_bare() {
        let p = pats(&["Finance/"]);
        assert!(path_is_blocked("Finance/q1.md", &p));
        assert!(path_is_blocked("Finance", &p));
    }

    #[test]
    fn double_star_matches_anywhere() {
        let p = pats(&["**/secret.md"]);
        assert!(path_is_blocked("secret.md", &p));
        assert!(path_is_blocked("foo/secret.md", &p));
        assert!(path_is_blocked("a/b/c/secret.md", &p));
        assert!(!path_is_blocked("secret.txt", &p));
        assert!(!path_is_blocked("foo/secret/x.md", &p));
    }

    #[test]
    fn single_star_stays_in_segment() {
        let p = pats(&["Finance/*.md"]);
        assert!(path_is_blocked("Finance/q1.md", &p));
        assert!(!path_is_blocked("Finance/sub/q1.md", &p)); // * doesn't cross /
        assert!(!path_is_blocked("Other/q1.md", &p));
    }

    #[test]
    fn double_star_in_middle_crosses_segments() {
        let p = pats(&[".private/**"]);
        assert!(path_is_blocked(".private/x.md", &p));
        assert!(path_is_blocked(".private/nested/deeper/y.md", &p));
        assert!(!path_is_blocked("public.private/x.md", &p));
    }

    #[test]
    fn leading_slash_and_backslashes_normalized() {
        let p = pats(&["/Journal"]);
        assert!(path_is_blocked("Journal/a.md", &p));
        // Windows-style separator should be tolerated.
        assert!(path_is_blocked("Journal\\a.md", &p));
    }

    #[test]
    fn question_mark_matches_single_non_slash() {
        let p = pats(&["q?.md"]);
        assert!(path_is_blocked("q1.md", &p));
        assert!(path_is_blocked("qa.md", &p));
        assert!(!path_is_blocked("q.md", &p)); // ? requires exactly one
        assert!(!path_is_blocked("q12.md", &p));
    }

    /// Reviewer #10: the one hard agent control must see every spelling the
    /// file system treats as the same path.
    #[test]
    fn blocking_is_case_insensitive() {
        let p = pats(&["Journal/"]);
        assert!(path_is_blocked("journal/x.md", &p));
        assert!(path_is_blocked("JOURNAL/x.md", &p));
        assert!(path_is_blocked("Journal/X.MD", &p));
        let p = pats(&["Finance/*.md"]);
        assert!(path_is_blocked("finance/Q1.md", &p));
        let p = pats(&["Notes/Secret.md"]);
        assert!(path_is_blocked("notes/secret.md", &p));
    }

    #[test]
    fn windows_trailing_dots_and_spaces_do_not_dodge_a_block() {
        let p = pats(&["Journal/"]);
        assert!(path_is_blocked("Journal./x.md", &p));
        assert!(path_is_blocked("Journal../x.md", &p));
        assert!(path_is_blocked("Journal /x.md", &p));
        assert!(path_is_blocked("Journal. ./x.md", &p));
        // ...and in the PATTERN, so a pattern typed with a stray dot still works.
        let p = pats(&["Journal./"]);
        assert!(path_is_blocked("Journal/x.md", &p));
    }

    #[test]
    fn repeated_slashes_collapse() {
        let p = pats(&["Journal/"]);
        assert!(path_is_blocked("Journal//x.md", &p));
        assert!(path_is_blocked("//Journal///x.md", &p));
    }

    #[test]
    fn the_new_folding_does_not_over_block_siblings() {
        let p = pats(&["Journal"]);
        assert!(!path_is_blocked("JournalArchive/x.md", &p));
        assert!(!path_is_blocked("Journals/x.md", &p));
        assert!(!path_is_blocked("Other/Journal.md", &p));
    }

    #[test]
    fn multiple_patterns_any_match_blocks() {
        let p = pats(&["Finance/", "**/secret.md"]);
        assert!(path_is_blocked("Finance/q1.md", &p));
        assert!(path_is_blocked("Notes/secret.md", &p));
        assert!(!path_is_blocked("Notes/public.md", &p));
    }
}
