pub mod anthropic;
pub mod capability;
pub mod gemini;
pub mod models;
pub mod ollama;
pub mod openai;
pub mod openrouter;
pub mod provider;
pub mod skills;
// AUDIT-057: shared streaming + tool-translator helpers extracted from the
// per-provider files so a fourth direct provider (Anthropic Claude, OpenAI
// direct) doesn't replay the same ~350 LoC of copy-paste.
pub mod roost_instructions;
pub mod streaming;
pub mod subagents;
pub mod tool_protocol;
pub mod tool_translator;
pub mod tools;

// Native-dep modules are swapped out on mobile. The real llamacpp.rs pulls in
// the llama-cpp-2 crate (bundled C++ llama.cpp), which doesn't cross-compile
// cleanly through the Android NDK for this first mobile cut. The mobile stub
// exposes the same public surface so the rest of the tree compiles unchanged;
// methods return "not supported on mobile" errors at runtime. Cloud providers
// (Gemini) still work on mobile.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod llamacpp;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[path = "llamacpp_mobile.rs"]
pub mod llamacpp;

pub use provider::{ChatMessage, ChatRole, LLMProvider};
#[allow(unused_imports)]
pub use tool_protocol::{StopReason, StreamEvent, ToolCall as ProtoToolCall, ToolDef};
