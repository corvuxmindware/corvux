//! AUDIT-057: provider-specific translators between Corvux's internal
//! `ToolCall` / `ToolDef` types and each vendor's wire JSON shape.
//!
//! The internal types live in `ai/tool_protocol.rs` and are vendor-neutral.
//! Each submodule here knows exactly one wire shape:
//!
//! - `openai_compat` — `tools: [{ type: "function", function: { ... } }]` +
//!   stringified `arguments` deltas. Used by OpenRouter, Ollama, OpenAI
//!   direct, and (with the same shape) Anthropic's OpenAI-compat endpoint.
//! - `gemini` — `tools: [{ functionDeclarations: [...] }]` with object-shape
//!   `args` and the thinking-model `thoughtSignature` round-trip blob.
//!
//! Providers stay responsible for HTTP, auth, and the SSE consumption loop.
//! This module is only the JSON shape translation.

pub mod anthropic;
pub mod gemini;
pub mod openai_compat;

// AI-8: end-to-end privacy-boundary assertions on the assembled outbound
// cloud request payloads (both provider shapes), not just the path
// sanitizers in isolation.
#[cfg(test)]
mod privacy_boundary_tests;
