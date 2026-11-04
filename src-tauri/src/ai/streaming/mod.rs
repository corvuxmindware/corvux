//! AUDIT-057: shared streaming helpers for the cloud-provider SSE paths.
//!
//! Every cloud provider (Gemini, OpenRouter, Ollama) was previously carrying
//! its own copies of the same SSE-line boundary detector, cancel-poll loop,
//! and finish-reason classification. The duplication was small per file but
//! grew with every new provider — by the time Anthropic Claude lands as a
//! direct provider, four copies is one too many.
//!
//! What lives here is **provider-agnostic**: the shape of an SSE stream
//! (`event:` line, `data:` payload, blank-line delimiter) is HTTP/RFC and
//! identical across every vendor that ships streaming chat.
//!
//! What does NOT live here: anything that knows about JSON shapes (delta
//! arrays, tool-call envelopes, finish-reason strings). Those vary per
//! provider and stay in the provider files.

pub mod consumer;
pub mod openai_compat_chat;
pub mod sse;
