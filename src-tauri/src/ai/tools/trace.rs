// Replay scaffolding for the agent harness. The Record half writes
// today; the Replay half (ReplayProvider, ReplayState, ReplayError,
// from_events/from_file/...) is the consumer for the offline-debug
// agent loop slated for Phase 3 testing infra. Keep the contract
// alive so we don't end up rebuilding the file when that lands.
#![allow(dead_code)]

//! Record / replay scaffolding for the agent loop (agent-hardening § 3.6).
//!
//! Two halves:
//!
//! - [`RecordingProvider`] wraps a live `LLMProvider` at runtime,
//!   appending one JSONL event per `stream_chat` request/response to
//!   `{roost}/.corvux/traces/<trace_id>.jsonl`. Opt-in via env var
//!   `CORVUX_TRACE_AGENT=1`; never enabled in production unless the
//!   user explicitly asks for it.
//!
//! - [`ReplayState`] reads a captured trace file and hands back the
//!   recorded responses in iteration order. [`ReplayProvider`] is the
//!   thin `LLMProvider`-impl shim — most logic lives on `ReplayState`
//!   so unit tests can exercise the state machine without needing a
//!   `tauri::AppHandle`.
//!
//! # Scope of this scaffolding
//!
//! v1 records the full final response per iteration. It does NOT
//! record chunk-by-chunk streaming; replay returns the final text as
//! a single emit, NOT as a sequence of `ai:chunk` events. This is
//! enough to assert tool-call sequencing and final-message regex
//! matches in regression tests, which is what the doc identifies as
//! the load-bearing use case. Byte-exact streaming replay is queued
//! as a v2 enhancement.
//!
//! # Seed corpus
//!
//! This module ships the SCAFFOLDING. Recording an actual corpus
//! against your usage requires you to flip the env var, drive the app
//! through the scenarios in § 3.6 (Approve happy path, Reject + recover,
//! AlwaysApprove, disambiguation, etc.), and check the resulting JSONL
//! files into `src-tauri/traces/regressions/`. CI tests
//! that replay those traces are a follow-up slice.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::ai::provider::{ChatMessage, LLMProvider};

const TRACE_DIR_NAME: &str = "traces";
const ROOST_META_DIR: &str = ".corvux";
const ENV_VAR: &str = "CORVUX_TRACE_AGENT";

/// One line in a trace JSONL. Tagged union — `serde(tag = "kind")`
/// keeps the wire format self-describing so a future tool (CLI,
/// scorecard query, etc.) can read these without a separate schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    /// First event in every trace. Identifies the run.
    SessionStart {
        trace_id: String,
        /// Unix epoch seconds.
        started_at: f64,
        /// Agent profile in use (`Default`, `Research`, etc.).
        profile: String,
        /// Provider's `model_name()` at record time.
        model: String,
    },
    /// One call to `stream_chat`. Captures the full inputs.
    Request {
        iteration: u32,
        messages: Vec<ChatMessage>,
        system_context: Option<String>,
    },
    /// Matching response. `text` is the full accumulated final reply
    /// (same value the LLMProvider returns from `stream_chat`).
    Response {
        iteration: u32,
        text: String,
        elapsed_ms: u64,
    },
    /// Final event. `total_elapsed_ms` covers from `SessionStart` to
    /// the last `Response`.
    SessionEnd { total_elapsed_ms: u64 },
}

// ─── Recording ─────────────────────────────────────────────────────

/// Append-only JSONL writer. Wrapped in `Mutex` because the trait
/// surface is async + multi-iteration; writes are short and not in
/// the hot path so contention is a non-issue.
pub struct TraceWriter {
    path: PathBuf,
    handle: std::fs::File,
}

impl TraceWriter {
    pub fn create(dir: &Path, trace_id: &str) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("trace: create_dir_all {} failed: {e}", dir.display()))?;
        let path = dir.join(format!("{trace_id}.jsonl"));
        let handle = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("trace: open {} failed: {e}", path.display()))?;
        Ok(Self { path, handle })
    }

    pub fn write(&mut self, event: &TraceEvent) -> Result<(), String> {
        use std::io::Write;
        let line =
            serde_json::to_string(event).map_err(|e| format!("trace: serialize failed: {e}"))?;
        writeln!(self.handle, "{line}").map_err(|e| format!("trace: write failed: {e}"))?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Provider wrapper that records every `stream_chat` round-trip to
/// disk. Pass-through on `model_name` / `is_local` — the recorder
/// must be transparent to the rest of the agent.
///
/// Generic over runtime `R` to match the underlying `LLMProvider` it
/// wraps. Production hits `R = Wry`; the type stays generic so a
/// `MockRuntime` provider can be recorded too if a test needs to.
pub struct RecordingProvider<R: tauri::Runtime + 'static = tauri::Wry> {
    inner: Box<dyn LLMProvider<R>>,
    writer: Arc<Mutex<TraceWriter>>,
    iteration: AtomicU32,
    started: Instant,
    trace_id: String,
}

impl<R: tauri::Runtime + 'static> RecordingProvider<R> {
    pub fn wrap(
        inner: Box<dyn LLMProvider<R>>,
        traces_dir: &Path,
        profile: &str,
    ) -> Result<Self, String> {
        let trace_id = format!(
            "{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            uuid::Uuid::new_v4().simple()
        );
        let mut writer = TraceWriter::create(traces_dir, &trace_id)?;
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        writer.write(&TraceEvent::SessionStart {
            trace_id: trace_id.clone(),
            started_at,
            profile: profile.to_string(),
            model: inner.model_name().to_string(),
        })?;
        tracing::info!(
            trace_id = %trace_id,
            path = %writer.path().display(),
            "trace: recording session"
        );
        Ok(Self {
            inner,
            writer: Arc::new(Mutex::new(writer)),
            iteration: AtomicU32::new(0),
            started: Instant::now(),
            trace_id,
        })
    }

    /// Resolve the traces dir under the user's roost. `None` when no
    /// roost is open (recording requires one).
    pub fn traces_dir_for_roost(roost: &Path) -> PathBuf {
        roost.join(ROOST_META_DIR).join(TRACE_DIR_NAME)
    }

    /// True when the recorder is enabled via env var. Cheap — single
    /// env lookup. Callers should use [`try_wrap_from_env`] which
    /// also handles the roost-open precondition.
    pub fn enabled_via_env() -> bool {
        matches!(
            std::env::var(ENV_VAR).ok().as_deref(),
            Some("1") | Some("true")
        )
    }

    /// Convenience: wrap `inner` IF the env var is set AND a roost
    /// path is available. Returns `inner` unchanged otherwise.
    /// Errors from `wrap` are logged and swallowed — a broken
    /// recording surface must never break the agent.
    pub fn try_wrap_from_env(
        inner: Box<dyn LLMProvider<R>>,
        roost: Option<&Path>,
        profile: &str,
    ) -> Box<dyn LLMProvider<R>> {
        if !Self::enabled_via_env() {
            return inner;
        }
        let Some(roost) = roost else {
            tracing::warn!("trace: {ENV_VAR}=1 but no roost is open; agent recording skipped");
            return inner;
        };
        let dir = Self::traces_dir_for_roost(roost);
        match Self::wrap(inner, &dir, profile) {
            Ok(r) => Box::new(r),
            Err(e) => {
                tracing::warn!(error = %e, "trace: wrap failed; agent recording disabled");
                // We swallowed inner in the failed wrap — the caller
                // no longer has a provider. We have to surface this
                // somehow. Since we can't return the original inner
                // (it was moved), this path produces a stub error
                // provider. In practice `wrap` only fails on FS
                // problems (write-protected roost), which are rare.
                Box::new(FailedProvider {
                    reason: format!("agent recording wrap failed: {e}"),
                })
            }
        }
    }
}

/// Stub provider returned when recorder setup fails — surfaces the
/// underlying FS error on the next chat send instead of crashing or
/// silently dropping traces.
struct FailedProvider {
    reason: String,
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for FailedProvider {
    async fn stream_chat(
        &self,
        _app: &tauri::AppHandle<R>,
        _messages: Vec<ChatMessage>,
        _system_context: Option<String>,
        _cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<String, String> {
        Err(self.reason.clone())
    }
    fn model_name(&self) -> &str {
        "failed-trace-wrap"
    }
    fn is_local(&self) -> bool {
        false
    }
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for RecordingProvider<R> {
    async fn stream_chat(
        &self,
        app: &tauri::AppHandle<R>,
        messages: Vec<ChatMessage>,
        system_context: Option<String>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<String, String> {
        let iteration = self.iteration.fetch_add(1, Ordering::Relaxed);
        let request = TraceEvent::Request {
            iteration,
            messages: messages.clone(),
            system_context: system_context.clone(),
        };
        if let Err(e) = self.writer.lock().await.write(&request) {
            tracing::warn!(error = %e, "trace: write Request failed");
        }
        let started = Instant::now();
        let result = self
            .inner
            .stream_chat(app, messages, system_context, cancel)
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if let Ok(text) = &result {
            let response = TraceEvent::Response {
                iteration,
                text: text.clone(),
                elapsed_ms,
            };
            if let Err(e) = self.writer.lock().await.write(&response) {
                tracing::warn!(error = %e, "trace: write Response failed");
            }
        }
        // SessionEnd is written when the recorder is dropped (see
        // Drop impl below). Doing it on first error would mis-attribute
        // partial sessions; we'd rather have a partially-written trace
        // file than no final event.
        result
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn is_local(&self) -> bool {
        self.inner.is_local()
    }
}

impl<R: tauri::Runtime + 'static> Drop for RecordingProvider<R> {
    fn drop(&mut self) {
        let total = self.started.elapsed().as_millis() as u64;
        // Drop runs sync; tokio Mutex can't be locked from sync code
        // without bridging. Use try_lock and accept the rare miss —
        // SessionEnd is a nice-to-have for the replay test runner,
        // not a correctness invariant.
        if let Ok(mut w) = self.writer.try_lock() {
            let _ = w.write(&TraceEvent::SessionEnd {
                total_elapsed_ms: total,
            });
        }
    }
}

// ─── Replay ────────────────────────────────────────────────────────

/// Pure state-machine half of the replay surface. Holds the recorded
/// events plus an iteration cursor. The async trait wrapper
/// [`ReplayProvider`] is a thin shim that pipes `stream_chat` calls
/// through here. Unit tests target this directly so they don't need
/// a tauri `AppHandle`.
pub struct ReplayState {
    events: Vec<TraceEvent>,
    cursor: AtomicUsize,
    model: String,
    strict_messages: bool,
}

/// Result of a replay step. Errors are returned as `Err` so the test
/// harness can fail loudly on a drift.
#[derive(Debug)]
pub enum ReplayError {
    /// No more responses in the trace; the agent loop iterated more
    /// times than the recording captured. Almost always a sign that
    /// loop logic changed between record and replay.
    OutOfEvents,
    /// Strict-message mode caught a divergence between the recorded
    /// messages and what the replay caller passed in.
    MessagesDiverged {
        iteration: u32,
        recorded_len: usize,
        provided_len: usize,
    },
    /// Trace file ill-formed (missing SessionStart, bad ordering, etc.).
    BadTrace(String),
}

impl ReplayState {
    pub fn from_events(events: Vec<TraceEvent>, strict_messages: bool) -> Self {
        let model = events
            .iter()
            .find_map(|e| match e {
                TraceEvent::SessionStart { model, .. } => Some(model.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "replay".to_string());
        Self {
            events,
            cursor: AtomicUsize::new(0),
            model,
            strict_messages,
        }
    }

    pub fn from_file(path: &Path, strict_messages: bool) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("trace replay: read {} failed: {e}", path.display()))?;
        let mut events = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event: TraceEvent = serde_json::from_str(line)
                .map_err(|e| format!("trace replay: line {}: parse failed: {e}", i + 1))?;
            events.push(event);
        }
        Ok(Self::from_events(events, strict_messages))
    }

    pub fn model_name(&self) -> &str {
        &self.model
    }

    /// Consume the next recorded request/response pair. Returns the
    /// recorded response text and elapsed_ms — the caller can assert
    /// on either. `provided_messages` is matched against the recorded
    /// `Request.messages` when `strict_messages` is on.
    pub fn next_response(
        &self,
        provided_messages: &[ChatMessage],
    ) -> Result<(String, u64), ReplayError> {
        loop {
            let i = self.cursor.fetch_add(1, Ordering::Relaxed);
            if i >= self.events.len() {
                return Err(ReplayError::OutOfEvents);
            }
            match &self.events[i] {
                TraceEvent::Request {
                    iteration,
                    messages,
                    ..
                } => {
                    if self.strict_messages && messages.len() != provided_messages.len() {
                        return Err(ReplayError::MessagesDiverged {
                            iteration: *iteration,
                            recorded_len: messages.len(),
                            provided_len: provided_messages.len(),
                        });
                    }
                    // The next event must be the matching Response.
                    let resp_i = self.cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(resp) = self.events.get(resp_i) else {
                        return Err(ReplayError::BadTrace(format!(
                            "Request at {i} has no following Response"
                        )));
                    };
                    return match resp {
                        TraceEvent::Response {
                            text, elapsed_ms, ..
                        } => Ok((text.clone(), *elapsed_ms)),
                        other => Err(ReplayError::BadTrace(format!(
                            "expected Response after Request at {i}, got {other:?}"
                        ))),
                    };
                }
                TraceEvent::SessionStart { .. } | TraceEvent::SessionEnd { .. } => {
                    // Skip — frame events, not iterations.
                    continue;
                }
                TraceEvent::Response { .. } => {
                    return Err(ReplayError::BadTrace(format!(
                        "unexpected Response at index {i} without matching Request"
                    )));
                }
            }
        }
    }

    /// Number of Request/Response pairs in the trace.
    pub fn iteration_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, TraceEvent::Request { .. }))
            .count()
    }
}

/// LLMProvider impl that returns recorded responses from a trace
/// file. Used by replay tests via [`ReplayState::from_file`].
pub struct ReplayProvider {
    state: Arc<ReplayState>,
}

impl ReplayProvider {
    pub fn new(state: ReplayState) -> Self {
        Self {
            state: Arc::new(state),
        }
    }
}

#[async_trait]
impl<R: tauri::Runtime + 'static> LLMProvider<R> for ReplayProvider {
    async fn stream_chat(
        &self,
        _app: &tauri::AppHandle<R>,
        messages: Vec<ChatMessage>,
        _system_context: Option<String>,
        _cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<String, String> {
        match self.state.next_response(&messages) {
            Ok((text, _ms)) => Ok(text),
            Err(ReplayError::OutOfEvents) => Err(
                "trace replay exhausted — agent loop iterated more than the trace recorded"
                    .to_string(),
            ),
            Err(ReplayError::MessagesDiverged {
                iteration,
                recorded_len,
                provided_len,
            }) => Err(format!(
                "trace replay: messages diverged at iteration {iteration} \
                 (recorded {recorded_len}, got {provided_len})"
            )),
            Err(ReplayError::BadTrace(msg)) => Err(format!("trace replay: bad trace: {msg}")),
        }
    }

    fn model_name(&self) -> &str {
        self.state.model_name()
    }

    fn is_local(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::provider::ChatRole;

    fn req(iteration: u32, count: usize) -> TraceEvent {
        TraceEvent::Request {
            iteration,
            messages: vec![
                ChatMessage {
                    role: ChatRole::User,
                    content: "stub".to_string(),
                    ..Default::default()
                };
                count
            ],
            system_context: None,
        }
    }
    fn resp(iteration: u32, text: &str) -> TraceEvent {
        TraceEvent::Response {
            iteration,
            text: text.to_string(),
            elapsed_ms: 100,
        }
    }

    #[test]
    fn replay_state_returns_recorded_responses_in_order() {
        let events = vec![
            TraceEvent::SessionStart {
                trace_id: "t".into(),
                started_at: 0.0,
                profile: "Default".into(),
                model: "mock".into(),
            },
            req(0, 1),
            resp(0, "hello"),
            req(1, 1),
            resp(1, "world"),
            TraceEvent::SessionEnd {
                total_elapsed_ms: 1000,
            },
        ];
        let state = ReplayState::from_events(events, false);
        assert_eq!(state.iteration_count(), 2);

        let dummy_msgs = vec![ChatMessage {
            role: ChatRole::User,
            content: "x".into(),
            ..Default::default()
        }];
        let (a, _) = state.next_response(&dummy_msgs).unwrap();
        let (b, _) = state.next_response(&dummy_msgs).unwrap();
        assert_eq!(a, "hello");
        assert_eq!(b, "world");

        let err = state.next_response(&dummy_msgs).unwrap_err();
        assert!(matches!(err, ReplayError::OutOfEvents));
    }

    #[test]
    fn replay_state_skips_session_frame_events() {
        let events = vec![
            TraceEvent::SessionStart {
                trace_id: "t".into(),
                started_at: 0.0,
                profile: "Default".into(),
                model: "mock".into(),
            },
            req(0, 1),
            resp(0, "ok"),
        ];
        let state = ReplayState::from_events(events, false);
        let dummy_msgs = vec![ChatMessage {
            role: ChatRole::User,
            content: "x".into(),
            ..Default::default()
        }];
        let (text, _) = state.next_response(&dummy_msgs).unwrap();
        assert_eq!(text, "ok");
    }

    #[test]
    fn strict_mode_flags_message_count_drift() {
        let events = vec![req(0, 3), resp(0, "ok")];
        let state = ReplayState::from_events(events, true);
        let only_one = vec![ChatMessage {
            role: ChatRole::User,
            content: "x".into(),
            ..Default::default()
        }];
        let err = state.next_response(&only_one).unwrap_err();
        match err {
            ReplayError::MessagesDiverged {
                iteration,
                recorded_len,
                provided_len,
            } => {
                assert_eq!(iteration, 0);
                assert_eq!(recorded_len, 3);
                assert_eq!(provided_len, 1);
            }
            other => panic!("expected MessagesDiverged, got {other:?}"),
        }
    }

    #[test]
    fn lax_mode_ignores_message_drift() {
        let events = vec![req(0, 3), resp(0, "ok")];
        let state = ReplayState::from_events(events, false);
        let only_one = vec![ChatMessage {
            role: ChatRole::User,
            content: "x".into(),
            ..Default::default()
        }];
        // Lax mode accepts the divergence and returns the recorded text.
        let (text, _) = state.next_response(&only_one).unwrap();
        assert_eq!(text, "ok");
    }

    #[test]
    fn replay_state_detects_bad_trace_missing_response() {
        let events = vec![req(0, 1)]; // Request with no Response
        let state = ReplayState::from_events(events, false);
        let dummy = vec![ChatMessage {
            role: ChatRole::User,
            content: "x".into(),
            ..Default::default()
        }];
        let err = state.next_response(&dummy).unwrap_err();
        assert!(matches!(err, ReplayError::BadTrace(_)));
    }

    #[test]
    fn trace_writer_round_trips_events_to_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = TraceWriter::create(dir.path(), "test-trace").unwrap();
        let events = vec![
            TraceEvent::SessionStart {
                trace_id: "test-trace".into(),
                started_at: 1.5,
                profile: "Default".into(),
                model: "mock".into(),
            },
            req(0, 2),
            resp(0, "first reply"),
            TraceEvent::SessionEnd {
                total_elapsed_ms: 500,
            },
        ];
        for e in &events {
            writer.write(e).unwrap();
        }

        // Now read back via ReplayState::from_file.
        let state = ReplayState::from_file(writer.path(), false).unwrap();
        assert_eq!(state.iteration_count(), 1);
        let dummy = vec![ChatMessage {
            role: ChatRole::User,
            content: "x".into(),
            ..Default::default()
        }];
        let (text, _) = state.next_response(&dummy).unwrap();
        assert_eq!(text, "first reply");
    }

    #[test]
    fn traces_dir_resolves_under_roost_meta_dir() {
        let p = RecordingProvider::<tauri::Wry>::traces_dir_for_roost(Path::new("/roost"));
        let s = p.to_string_lossy();
        assert!(s.contains(".corvux"));
        assert!(s.contains("traces"));
    }

    #[test]
    fn enabled_via_env_reads_var() {
        let key = ENV_VAR;
        let prior = std::env::var(key).ok();
        // SAFETY: tests in Rust 1.95 mark set_var as unsafe because it
        // mutates a process-wide value. We accept it for this single
        // test and restore the prior value after.
        unsafe {
            std::env::set_var(key, "1");
        }
        assert!(RecordingProvider::<tauri::Wry>::enabled_via_env());
        unsafe {
            std::env::set_var(key, "0");
        }
        assert!(!RecordingProvider::<tauri::Wry>::enabled_via_env());
        unsafe {
            std::env::remove_var(key);
        }
        assert!(!RecordingProvider::<tauri::Wry>::enabled_via_env());
        // Restore.
        if let Some(v) = prior {
            unsafe {
                std::env::set_var(key, v);
            }
        }
    }
}
