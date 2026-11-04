//! Lightweight perf counters + process-RSS sampler. Backs the hidden
//! dev perf panel (Ctrl+Alt+P) used to triage user reports without
//! asking them to dig through logs.
//!
//! Design notes:
//!
//! - **Process-local, in-memory only.** No disk persistence — counters
//!   reset on relaunch by design. Persisting would require schema +
//!   migration + privacy review for what users can see, none of which
//!   is worth the cost for a triage surface that's only useful while
//!   reproducing an issue.
//! - **Single mutex.** Counter math is microseconds; lock contention
//!   isn't a concern. Splitting into per-counter atomics would buy
//!   nothing and lose the structured-record semantics of
//!   `last_chat_send`.
//! - **Counters are bumped from the existing `tracing::info!` sites.**
//!   We don't replace the logs — they stay so file-based debugging
//!   keeps working. The bumps are additional, not substitutional.
//! - **RSS via `sysinfo`.** Already a dep (used by `ai::capability`).
//!   Sampled lazily inside `get_perf_snapshot` so the counters layer
//!   doesn't pay the syscall cost when no panel is open.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::State;

use crate::diagnostics; // reuse format_now() — keeps timestamps consistent across surfaces

/// Snapshot of one chat turn's pre-LLM setup latency. The agent loop
/// itself logs its own iteration timings via `tracing::info!`; this
/// breakdown captures the steps before the LLM stream starts (which
/// is where the bulk of the 95th-percentile slowness has historically
/// hidden during testing).
#[derive(Debug, Clone, Default, Serialize, specta::Type)]
pub struct ChatSendBreakdown {
    /// RFC3339 UTC timestamp the breakdown was captured.
    pub at: String,
    /// `chat: system prompt built` — prompt assembly + DB locks.
    #[specta(type = u32)]
    pub prompt_ms: u64,
    /// Total wall-clock from `chat_send` entry to handler spawn.
    /// Includes `prompt_ms` and any other setup overhead.
    #[specta(type = u32)]
    pub setup_ms: u64,
    /// Length of the assembled system prompt, in characters. Useful
    /// for spotting "prompt grew unboundedly" regressions.
    #[specta(type = u32)]
    pub system_prompt_chars: usize,
    /// Active agent profile id (`default`, `quicklookup`, etc.).
    /// Different profiles have different bundle counts, so latency
    /// isn't comparable across profiles unless this is captured.
    pub profile: String,
    /// NEW-AI2: prompt (input) tokens the provider reported for this turn,
    /// when available. `None` for local models and until per-provider usage
    /// parsing lands — this is the forward-path plumbing for the AI-5 cloud
    /// cost ledger, so the dimension exists before the ledger is built.
    #[specta(type = Option<u32>)]
    pub prompt_tokens: Option<u64>,
    /// NEW-AI2: completion (output) tokens the provider reported, when
    /// available. `None` until provider usage parsing lands (AI-5).
    #[specta(type = Option<u32>)]
    pub completion_tokens: Option<u64>,
    /// NEW-AI2: estimated USD cost of this turn (tokens × per-model price),
    /// when computable. `None` until the AI-5 pricing table is wired.
    pub est_cost_usd: Option<f64>,
}

/// Counters that accumulate across the lifetime of the process.
/// Resets on app restart — no on-disk persistence by design.
#[derive(Debug, Default, Clone, Serialize, specta::Type)]
pub struct PerfCounters {
    /// Total `chat_send` invocations since launch.
    #[specta(type = u32)]
    pub chat_send_total: u64,
    /// Total `roost_open` invocations since launch — includes warm
    /// re-opens of the same roost.
    #[specta(type = u32)]
    pub roost_open_total: u64,
    /// Most recent chat-send breakdown. None until the first turn
    /// completes setup.
    pub last_chat_send: Option<ChatSendBreakdown>,
}

/// Tauri-managed state. Always cheap to clone (Arc) — every bump
/// site holds a reference for ~microseconds.
#[derive(Default, Clone)]
pub struct PerfCountersState {
    pub inner: Arc<Mutex<PerfCounters>>,
}

impl PerfCountersState {
    pub fn new() -> Self {
        Self::default()
    }

    fn with<F: FnOnce(&mut PerfCounters)>(&self, f: F) {
        // Poison recovery: counters can't be torn — they're scalars
        // and a single struct. Recover and continue rather than
        // letting a panicking thread silence telemetry forever.
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        f(&mut guard);
    }

    pub fn bump_roost_open(&self) {
        self.with(|c| c.roost_open_total = c.roost_open_total.saturating_add(1));
    }

    pub fn record_chat_send(&self, breakdown: ChatSendBreakdown) {
        self.with(|c| {
            c.chat_send_total = c.chat_send_total.saturating_add(1);
            c.last_chat_send = Some(breakdown);
        });
    }

    pub fn snapshot_counters(&self) -> PerfCounters {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.clone()
    }
}

/// Convenience builder: `ChatSendBreakdown::now()` stamps the timestamp
/// at construction so callers don't have to import diagnostics.
impl ChatSendBreakdown {
    pub fn new(profile: &str) -> Self {
        Self {
            at: diagnostics::format_now_perf(),
            profile: profile.to_string(),
            ..Default::default()
        }
    }
}

/// Wire shape returned to the frontend perf panel. Counters + a fresh
/// process-RSS sample. Process RSS is sampled here (not on every bump)
/// because `sysinfo::System::refresh_processes` is a syscall — no need
/// to pay it unless the panel is actually open.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct PerfSnapshot {
    pub counters: PerfCounters,
    /// Resident-set-size of the corvux process in megabytes, or None
    /// when sysinfo couldn't resolve our pid.
    pub rss_mb: Option<f32>,
    /// System-wide free RAM in megabytes — context for whether RSS
    /// numbers are healthy on this machine.
    #[specta(type = u32)]
    pub free_ram_mb: u64,
    /// RFC3339 UTC capture time so the panel can show "as of …".
    pub at: String,
}

#[tauri::command]
#[specta::specta]
pub async fn get_perf_snapshot(
    state: State<'_, PerfCountersState>,
) -> Result<PerfSnapshot, String> {
    let counters = state.snapshot_counters();
    let (rss_mb, free_ram_mb) = current_rss_and_free_ram();

    Ok(PerfSnapshot {
        counters,
        rss_mb,
        free_ram_mb,
        at: diagnostics::format_now_perf(),
    })
}

/// One-shot sysinfo refresh — looks up *this* process by pid and
/// returns its RSS in MB plus system free-RAM in MB. Returns `(None, 0)`
/// if sysinfo couldn't resolve our pid (rare; would happen if the
/// process is being torn down between two refresh calls).
fn current_rss_and_free_ram() -> (Option<f32>, u64) {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    let mut sys = System::new();
    let pid = sysinfo::Pid::from_u32(std::process::id());
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::new().with_memory(),
    );
    sys.refresh_memory();

    let rss_mb = sys
        .process(pid)
        .map(|p| (p.memory() as f64 / 1_048_576.0) as f32);
    let free_ram_mb = sys.available_memory() / 1_048_576;
    (rss_mb, free_ram_mb)
}
