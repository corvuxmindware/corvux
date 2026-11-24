//! Ratchet guard for AI-area invariant 11 (App Store guideline 5.1.2(i)):
//! no user content reaches an AI provider without the user's per-provider
//! consent.
//!
//! The consent check lives in `commands::ai::consent::ConsentGated`, and
//! `chat::build_chat_provider` is the only production code allowed to
//! construct a provider, which it returns only through `consent::gate(..)`.
//! A provider built anywhere else would skip consent, so this test counts
//! every `XxxChat::new(` site per file against a frozen baseline. It fails on
//! any NEW site and on an unratcheted removal (update the baseline when you
//! remove one). Test-only constructions in the provider files are in the
//! baseline.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const PROVIDERS: [&str; 5] = [
    "GeminiChat::new(",
    "OpenRouterChat::new(",
    "OllamaChat::new(",
    "OpenAiChat::new(",
    "AnthropicChat::new(",
];

/// file (relative to src/) → number of provider constructions allowed.
const BASELINE: [(&str, usize); 3] = [
    // The five arms of build_chat_provider (the choke point).
    ("commands/ai/chat.rs", 5),
    // #[cfg(test)] unit tests inside the provider modules.
    ("ai/ollama.rs", 2),
    ("ai/anthropic.rs", 1),
];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn providers_are_only_constructed_behind_the_consent_gate() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);

    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("read rs file");
        let n: usize = PROVIDERS.iter().map(|p| text.matches(p).count()).sum();
        if n > 0 {
            let rel = file
                .strip_prefix(&src)
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(rel, n);
        }
    }
    let expected: BTreeMap<String, usize> =
        BASELINE.iter().map(|(f, n)| (f.to_string(), *n)).collect();
    assert_eq!(
        found, expected,
        "AI provider construction sites changed. A provider built outside \
         chat::build_chat_provider skips the user's consent (invariant 11) — \
         route it through build_chat_provider / consent::gate, or update the \
         baseline if you removed a site."
    );
}

/// Every type that can talk to a model, frozen. A NEW provider type (one the
/// constructor list above doesn't know about) or a renamed/aliased one shows
/// up here first; decide whether it sends anything off-device and, if so, make
/// sure it is only reachable through `build_chat_provider` + `consent::gate`.
#[test]
fn the_set_of_llm_provider_impls_is_frozen() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    let mut found = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("read rs file");
        let rel = file
            .strip_prefix(&src)
            .expect("under src")
            .to_string_lossy()
            .replace('\\', "/");
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with("impl") && l.contains("LLMProvider") && l.contains(" for ") {
                let ty = l
                    .rsplit(" for ")
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('{')
                    .trim();
                found.push(format!("{rel}: {ty}"));
            }
            for p in PROVIDERS {
                let name = p.trim_end_matches("::new(");
                assert!(
                    !l.contains(&format!("{name} as ")),
                    "{rel}: aliasing {name} hides it from the consent ratchet"
                );
            }
        }
    }
    found.sort();
    let expected = [
        "ai/anthropic.rs: AnthropicChat",
        "ai/gemini.rs: GeminiChat",
        "ai/llamacpp.rs: LlamaCppChat", // local model, never wired into chat (pack invariant 1)
        "ai/llamacpp_mobile.rs: LlamaCppChat", // mobile stub, errors only
        "ai/ollama.rs: OllamaChat",
        "ai/openai.rs: OpenAiChat",
        "ai/openrouter.rs: OpenRouterChat",
        "ai/tools/trace.rs: FailedProvider", // wraps the gated provider; no I/O
        "ai/tools/trace.rs: RecordingProvider<R>", // wraps the gated provider; no I/O
        "ai/tools/trace.rs: ReplayProvider", // replays a recorded trace; no I/O
        "commands/ai/consent.rs: ConsentGated", // the gate itself
        "commands/ai/consent.rs: Never",     // #[cfg(test)] fake
    ];
    assert_eq!(
        found, expected,
        "the set of LLMProvider impls changed — see the doc comment on this test"
    );
}

/// The core guarantee: each request method of `ConsentGated` checks consent
/// BEFORE delegating. Unit tests can't drive `stream_chat` (it needs a Wry
/// `AppHandle`; the tauri `test` feature isn't enabled), so freeze the shape:
/// deleting a `self.check()?` — or delegating before it — fails here.
#[test]
fn consent_gated_checks_before_every_request() {
    let src = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/commands/ai/consent.rs"),
    )
    .expect("consent.rs");
    let start = src
        .find("impl LLMProvider for ConsentGated {")
        .expect("ConsentGated implements LLMProvider");
    let block = &src[start..];
    let block = &block[..block.find("\n}\n").expect("end of impl block")];

    // Every method that can send must be listed here; any other async fn
    // in the impl is a new egress path that needs the same check.
    let sending = ["async fn stream_chat(", "async fn stream_chat_with_tools("];
    assert_eq!(
        block.matches("async fn ").count(),
        sending.len(),
        "ConsentGated gained/lost a request method — gate it and update this test"
    );
    for sig in sending {
        let at = block.find(sig).unwrap_or_else(|| panic!("missing {sig}"));
        let body = &block[at..];
        let body = &body[..body[sig.len()..]
            .find("async fn ")
            .map(|i| i + sig.len())
            .unwrap_or(body.len())];
        let check = body
            .find("self.check()?")
            .unwrap_or_else(|| panic!("{sig} must call self.check()? before sending"));
        let delegate = body
            .find("self.inner")
            .unwrap_or_else(|| panic!("{sig} must delegate to self.inner"));
        assert!(
            check < delegate,
            "{sig} must check consent before delegating"
        );
    }
}

#[test]
fn build_chat_provider_returns_only_through_the_gate() {
    let chat = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/commands/ai/chat.rs"),
    )
    .expect("chat.rs");
    let start = chat
        .find("fn build_chat_provider(")
        .expect("build_chat_provider exists");
    let gate = chat[start..]
        .find("super::consent::gate(")
        .map(|i| start + i)
        .expect("build_chat_provider must return via super::consent::gate(..)");
    let body = &chat[start..gate];
    let sites: usize = PROVIDERS.iter().map(|p| body.matches(p).count()).sum();
    assert_eq!(
        sites, 5,
        "all provider constructions in chat.rs must sit inside build_chat_provider, before the gate"
    );
    assert!(
        !body.contains("return Ok("),
        "build_chat_provider must not return a provider before the consent gate"
    );
}
