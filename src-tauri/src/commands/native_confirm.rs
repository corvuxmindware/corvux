//! Out-of-band confirmation through a native OS dialog.
//!
//! Every registered Tauri command is callable by whatever script runs in the
//! webview (XSS through a rendered note, a poisoned dependency, devtools). A
//! "confirmation" assembled from values the webview itself supplies — typing a
//! host twice, a `confirmed: true` flag, a modal rendered in the page — can be
//! satisfied by that same script in the same IPC message. The only answer a
//! compromised renderer cannot forge is one given in a window it does not
//! control: the OS dialog.
//!
//! Use [`NativeConfirm`] for commands whose effect is either
//!
//! * a **privilege escalation** the user did not ask for implicitly
//!   (auto-approve agent writes, un-block a path the agent must not touch,
//!   point chat traffic at a new internet host, trust a new sync-queue host), or
//! * **irreversible loss of app state** that the renderer alone authorizes
//!   (dropping a roost from the registry, which later deletes its index and
//!   sync pairing).
//!
//! The decision logic stays in pure functions (what needs confirming); this
//! module is only the thin, fail-closed prompt. Tests use [`test_support`].
//!
//! Sharp-edges audit 2026-11-11, findings F1 / F2 / F3 / F10 — one mechanism,
//! one choke point. Differential review 2026-11-12 AK-2/RF-2 + AK-3/RF-5: the
//! real dialog is modal to the main window, shown one at a time, and an
//! "Allow" faster than a human could read it counts as No (see
//! [`DialogGate`] and [`settle`]).

use async_trait::async_trait;

/// Ask the person at the keyboard, outside the webview.
#[async_trait]
pub(crate) trait NativeConfirm: Send + Sync {
    /// `true` only when the user explicitly pressed `allow_label`. Closing the
    /// dialog, pressing Cancel, or any failure to show it is `false`
    /// (fail closed).
    async fn confirm(&self, title: &str, body: &str, allow_label: &str) -> bool;
}

/// The real thing: a warning-style OS message dialog with `allow_label` and
/// Cancel buttons, parented (modal) to the main window on desktop.
/// Non-blocking (callback + oneshot), so it is safe to await from an async
/// command without parking a runtime thread on the dialog.
///
/// Every dialog goes through the process-wide [`DialogGate`] (one dialog at a
/// time, cool-down after each answer, back-off after repeated declines) and the
/// answer through [`settle`] (an "Allow" faster than [`MIN_DWELL_MS`] is a No).
///
/// Generic over the runtime so the agent loop (generic over `R`) can use it too;
/// everywhere else `R` is inferred as the real `Wry`.
pub(crate) struct OsDialog<R: tauri::Runtime = tauri::Wry>(pub tauri::AppHandle<R>);

// ── Dialog hardening (review 2026-11-12 AK-2/RF-2, AK-3/RF-5) ──────────────
//
// The dialog's FIRST button ("Allow") is the OS default: vendored rfd 0.17.2
// builds the Windows TaskDialog with `nDefaultButton: 0` and macOS makes the
// first button the key equivalent. A hijacked renderer can pop a dialog while
// the user is typing, so the next Enter/Space lands on Allow. We do NOT swap
// the buttons and invert the result: window-close, Esc and dialog errors all
// come back as `false`, and inverting would turn every one of them into Allow
// (fail-open). Instead the mapping stays "true only for the Allow button" and
// three fail-closed rules are layered on top. Each rule can only turn an
// answer into No; none can ever produce a Yes.

/// An "Allow" that arrives sooner than this after the dialog was REQUESTED is
/// treated as No: nobody reads a security warning that fast, so it is almost
/// certainly a keystroke meant for the app (Enter/Space hitting the default
/// button). Measured from the request, so it includes the time the OS takes to
/// put the dialog on screen. Residual: a stray Enter/Space typed LATER than
/// this still lands on Allow — the dwell narrows the race, it cannot close it.
const MIN_DWELL_MS: u64 = 1_000;

/// Minimum quiet time between one dialog being ANSWERED (or closed, timed out,
/// torn down) and the next one being shown. Stamped on close, not on show, so
/// a dialog the user takes a minute to read does not make the next legitimate
/// one start inside an already-expired window. Legitimate flows ask at most
/// once per user action; nothing chains two gated commands without a click.
const COOL_DOWN_MS: u64 = 1_000;

/// This many declines inside [`DECLINE_WINDOW_MS`] start a back-off during
/// which every confirm answers No WITHOUT showing a dialog (a renderer looping
/// gated commands cannot keep a dialog on screen).
const DECLINES_BEFORE_BACKOFF: usize = 3;
const DECLINE_WINDOW_MS: u64 = 60_000;
/// First back-off; each further back-off doubles it, capped at [`BACKOFF_MAX_MS`].
const BACKOFF_BASE_MS: u64 = 30_000;
const BACKOFF_MAX_MS: u64 = 600_000;
/// The doubling restarts from [`BACKOFF_BASE_MS`] when the previous back-off
/// began at least this long ago, or as soon as the user allows something.
const BACKOFF_RESET_MS: u64 = 600_000;

/// How long we wait for an answer. Past this the request answers No and the
/// gate is released. rfd cannot close a dialog it has shown, so the stale OS
/// dialog may stay on screen; its late answer is ignored (the receiver is gone
/// and its gate ticket is stale).
const ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Label of the window the dialog is made modal to (Tauri's default label;
/// `tauri.conf.json` does not rename it). Mobile has no parent-window API.
#[cfg(desktop)]
const MAIN_WINDOW_LABEL: &str = "main";

/// Label of the second button. The plugin reports `true` when the pressed
/// button's text equals the allow label, so an allow label equal to this
/// would make Cancel (and Esc) read as Allow.
const CANCEL_LABEL: &str = "Cancel";

/// How a dialog request ended, before the dwell rule is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogOutcome {
    /// The user pressed a button: `true` = the Allow button, `false` = Cancel,
    /// Esc, the close box, or an rfd error (all reported as not-Allow).
    Answered(bool),
    /// The callback was dropped without being called: `run_on_main_thread`
    /// failed, the plugin could not show the dialog, or the app is closing.
    NoAnswer,
    /// No answer within [`ANSWER_TIMEOUT`].
    TimedOut,
}

/// The final answer. ONLY a real Allow press that came after the minimum dwell
/// is `true`; every other outcome, at any elapsed time, is `false`.
fn settle(outcome: DialogOutcome, elapsed_since_request_ms: u64) -> bool {
    match outcome {
        DialogOutcome::Answered(true) => elapsed_since_request_ms >= MIN_DWELL_MS,
        DialogOutcome::Answered(false) | DialogOutcome::NoAnswer | DialogOutcome::TimedOut => false,
    }
}

/// `false` when the allow label is blank or reads as the Cancel label — then
/// pressing Cancel would be indistinguishable from Allow, so we refuse to ask.
fn allow_label_is_distinct(allow_label: &str) -> bool {
    let label = allow_label.trim();
    !label.is_empty() && !label.eq_ignore_ascii_case(CANCEL_LABEL)
}

/// Back-off length for the `level`-th consecutive back-off (1-based).
fn backoff_ms(level: u32) -> u64 {
    let doublings = level.saturating_sub(1).min(16);
    BACKOFF_BASE_MS
        .saturating_mul(1u64 << doublings)
        .min(BACKOFF_MAX_MS)
}

/// Why a confirm was answered No without showing a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// Another dialog is on screen right now.
    InFlight,
    /// The previous dialog closed less than [`COOL_DOWN_MS`] ago.
    CoolDown { remaining_ms: u64 },
    /// Too many recent declines.
    BackOff { remaining_ms: u64 },
}

/// What closing a dialog did to the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseEffect {
    Closed,
    /// This decline started a back-off of `wait_ms`.
    BackOffStarted {
        wait_ms: u64,
    },
    /// The ticket was not the dialog in flight (already closed, e.g. a late
    /// release after a timeout) — nothing changed.
    Stale,
}

/// Process-wide dialog state. Pure: every method takes the current time
/// (monotonic milliseconds) so the policy is testable with a fake clock.
#[derive(Debug)]
struct DialogGate {
    /// Ticket of the dialog on screen, if any.
    in_flight: Option<u64>,
    next_ticket: u64,
    /// When the last dialog was answered / closed.
    last_closed_ms: Option<u64>,
    /// Decline times inside [`DECLINE_WINDOW_MS`]; never longer than
    /// [`DECLINES_BEFORE_BACKOFF`] (cleared when a back-off starts).
    recent_declines: Vec<u64>,
    backoff_until_ms: u64,
    backoff_level: u32,
    last_backoff_start_ms: Option<u64>,
}

impl DialogGate {
    const fn new() -> Self {
        Self {
            in_flight: None,
            next_ticket: 0,
            last_closed_ms: None,
            recent_declines: Vec::new(),
            backoff_until_ms: 0,
            backoff_level: 0,
            last_backoff_start_ms: None,
        }
    }

    /// Claim the single dialog slot, or say why not. On success the caller
    /// owns ticket `t` and must hand it back to [`DialogGate::close`].
    fn try_begin(&mut self, now_ms: u64) -> Result<u64, Refusal> {
        if self.in_flight.is_some() {
            return Err(Refusal::InFlight);
        }
        if now_ms < self.backoff_until_ms {
            return Err(Refusal::BackOff {
                remaining_ms: self.backoff_until_ms - now_ms,
            });
        }
        if let Some(closed) = self.last_closed_ms {
            let since = now_ms.saturating_sub(closed);
            if since < COOL_DOWN_MS {
                return Err(Refusal::CoolDown {
                    remaining_ms: COOL_DOWN_MS - since,
                });
            }
        }
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.in_flight = Some(self.next_ticket);
        Ok(self.next_ticket)
    }

    /// Release the slot held by `ticket`, recording the final answer.
    fn close(&mut self, ticket: u64, now_ms: u64, allowed: bool) -> CloseEffect {
        if self.in_flight != Some(ticket) {
            return CloseEffect::Stale;
        }
        self.in_flight = None;
        self.last_closed_ms = Some(now_ms);
        if allowed {
            // A human accepted: whatever pattern of declines came before was
            // not a renderer loop worth throttling.
            self.recent_declines.clear();
            self.backoff_level = 0;
            return CloseEffect::Closed;
        }
        self.recent_declines
            .retain(|&t| now_ms.saturating_sub(t) < DECLINE_WINDOW_MS);
        self.recent_declines.push(now_ms);
        if self.recent_declines.len() < DECLINES_BEFORE_BACKOFF {
            return CloseEffect::Closed;
        }
        self.recent_declines.clear();
        if self
            .last_backoff_start_ms
            .is_some_and(|t| now_ms.saturating_sub(t) >= BACKOFF_RESET_MS)
        {
            self.backoff_level = 0;
        }
        self.backoff_level = self.backoff_level.saturating_add(1);
        self.last_backoff_start_ms = Some(now_ms);
        let wait_ms = backoff_ms(self.backoff_level);
        self.backoff_until_ms = now_ms.saturating_add(wait_ms);
        CloseEffect::BackOffStarted { wait_ms }
    }
}

/// The one gate every real dialog passes through.
static GATE: std::sync::Mutex<DialogGate> = std::sync::Mutex::new(DialogGate::new());

/// Monotonic milliseconds since the first call (wall-clock jumps can neither
/// extend nor skip the cool-down or the back-off).
fn mono_ms() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let base = *BASE.get_or_init(std::time::Instant::now);
    u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// A claimed dialog slot. Releasing it is tied to `Drop`, so EVERY way out of
/// a confirm — answer, timeout, dropped sender, the awaiting future being
/// cancelled, a panic — frees the slot. A slot dropped without
/// [`Slot::finish`] is recorded as a decline.
struct Slot<'a> {
    gate: &'a std::sync::Mutex<DialogGate>,
    clock: fn() -> u64,
    ticket: u64,
    finished: bool,
}

impl<'a> Slot<'a> {
    /// `None` = do not show a dialog (answer No): the slot is taken, a
    /// cool-down or back-off is running, or the gate lock is poisoned.
    fn acquire(gate: &'a std::sync::Mutex<DialogGate>, clock: fn() -> u64) -> Option<Self> {
        let Ok(mut state) = gate.lock() else {
            tracing::error!("native confirm: dialog gate lock poisoned — answering No");
            return None;
        };
        match state.try_begin(clock()) {
            Ok(ticket) => Some(Self {
                gate,
                clock,
                ticket,
                finished: false,
            }),
            Err(Refusal::InFlight) => {
                tracing::warn!(
                    "native confirm suppressed: another confirmation dialog is open — answering No"
                );
                None
            }
            Err(Refusal::CoolDown { remaining_ms }) => {
                tracing::warn!(
                    remaining_ms,
                    "native confirm suppressed (cool-down after the previous dialog) — answering No"
                );
                None
            }
            Err(Refusal::BackOff { remaining_ms }) => {
                tracing::warn!(
                    remaining_ms,
                    "native confirm suppressed (back-off after repeated declines) — answering No"
                );
                None
            }
        }
    }

    fn finish(mut self, allowed: bool) {
        self.finished = true;
        release(self.gate, self.ticket, (self.clock)(), allowed);
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        if !self.finished {
            release(self.gate, self.ticket, (self.clock)(), false);
        }
    }
}

fn release(gate: &std::sync::Mutex<DialogGate>, ticket: u64, now_ms: u64, allowed: bool) {
    match gate.lock() {
        Ok(mut state) => match state.close(ticket, now_ms, allowed) {
            CloseEffect::BackOffStarted { wait_ms } => tracing::warn!(
                wait_ms,
                "native confirm: {DECLINES_BEFORE_BACKOFF} declines within {DECLINE_WINDOW_MS} ms — \
                 further confirmations answer No without a dialog for a while"
            ),
            CloseEffect::Closed | CloseEffect::Stale => {}
        },
        // A poisoned gate stays poisoned: every later `acquire` answers No.
        Err(_) => tracing::error!(
            "native confirm: dialog gate lock poisoned — no further dialogs this session"
        ),
    }
}

/// Wait for the dialog callback, bounded by `limit`.
async fn await_answer(
    rx: tokio::sync::oneshot::Receiver<bool>,
    limit: std::time::Duration,
) -> DialogOutcome {
    match tokio::time::timeout(limit, rx).await {
        Ok(Ok(allowed)) => DialogOutcome::Answered(allowed),
        Ok(Err(_)) => DialogOutcome::NoAnswer,
        Err(_) => DialogOutcome::TimedOut,
    }
}

/// The whole policy around one dialog, with the gate, clock and the act of
/// showing the dialog injected. `show` is never called when the request is
/// refused up front.
async fn gated_confirm<F, Fut>(
    gate: &std::sync::Mutex<DialogGate>,
    clock: fn() -> u64,
    allow_label: &str,
    show: F,
) -> bool
where
    F: FnOnce() -> Fut + Send,
    Fut: std::future::Future<Output = DialogOutcome> + Send,
{
    if !allow_label_is_distinct(allow_label) {
        tracing::error!(
            "native confirm refused: the allow label is blank or equals the Cancel label — answering No"
        );
        return false;
    }
    let Some(slot) = Slot::acquire(gate, clock) else {
        return false;
    };
    let requested_ms = clock();
    let outcome = show().await;
    let elapsed_ms = clock().saturating_sub(requested_ms);
    let allowed = settle(outcome, elapsed_ms);
    match outcome {
        DialogOutcome::Answered(true) if !allowed => tracing::warn!(
            elapsed_ms,
            "native confirm: Allow pressed sooner than {MIN_DWELL_MS} ms after the dialog was \
             requested (likely a keystroke meant for the app) — treated as No"
        ),
        DialogOutcome::NoAnswer => {
            tracing::warn!("native confirm: dialog closed without an answer — answering No")
        }
        DialogOutcome::TimedOut => tracing::warn!(
            "native confirm: no answer within {} s — answering No",
            ANSWER_TIMEOUT.as_secs()
        ),
        DialogOutcome::Answered(_) => {}
    }
    slot.finish(allowed);
    allowed
}

impl<R: tauri::Runtime> OsDialog<R> {
    async fn show_and_wait(&self, title: &str, body: &str, allow_label: &str) -> DialogOutcome {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let builder = self
            .0
            .dialog()
            .message(body)
            .title(title)
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                allow_label.to_string(),
                CANCEL_LABEL.to_string(),
            ));
        // Modal to the main window, so the dialog sits over the app instead of
        // floating free. If the window is gone (or its handle is unavailable —
        // the plugin then ignores the parent) the dialog is shown unparented;
        // the answer mapping is unchanged either way.
        #[cfg(desktop)]
        let builder = {
            use tauri::Manager;
            match self.0.get_webview_window(MAIN_WINDOW_LABEL) {
                Some(window) => builder.parent(&window),
                None => {
                    tracing::warn!(
                        "native confirm: main window not found — showing the dialog unparented"
                    );
                    builder
                }
            }
        };
        // If the plugin cannot reach the main thread it drops this callback
        // without calling it; the sender drops with it -> `NoAnswer`.
        builder.show(move |allowed| {
            let _ = tx.send(allowed);
        });
        await_answer(rx, ANSWER_TIMEOUT).await
    }
}

#[async_trait]
impl<R: tauri::Runtime> NativeConfirm for OsDialog<R> {
    async fn confirm(&self, title: &str, body: &str, allow_label: &str) -> bool {
        gated_confirm(&GATE, mono_ms, allow_label, || {
            self.show_and_wait(title, body, allow_label)
        })
        .await
    }
}

/// Dialog text is built from renderer-influenced strings (host names, path
/// globs, tool names). Keep it short and free of control characters so a
/// hostile value cannot stretch the dialog, forge extra lines, or hide the real
/// request behind its own text.
pub(crate) fn dialog_safe(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() <= max_chars {
        cleaned.to_string()
    } else {
        let cut: String = cleaned.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers every prompt the same way and counts how often it was asked.
    pub(crate) struct Scripted {
        answer: bool,
        asked: AtomicUsize,
    }

    impl Scripted {
        pub(crate) fn allow() -> Self {
            Self {
                answer: true,
                asked: AtomicUsize::new(0),
            }
        }
        pub(crate) fn deny() -> Self {
            Self {
                answer: false,
                asked: AtomicUsize::new(0),
            }
        }
        pub(crate) fn times_asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl NativeConfirm for Scripted {
        async fn confirm(&self, _title: &str, _body: &str, _allow_label: &str) -> bool {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_safe_strips_control_characters() {
        assert_eq!(dialog_safe("a\nb\r\tc", 50), "a b  c");
    }

    #[test]
    fn dialog_safe_truncates_on_char_boundaries() {
        let s = "\u{754c}".repeat(100);
        let out = dialog_safe(&s, 10);
        assert_eq!(out.chars().count(), 11, "10 chars + ellipsis");
        assert!(out.ends_with('…'));
        assert_eq!(dialog_safe("short", 10), "short");
    }
}

/// Review 2026-11-12 AK-2/RF-2 + AK-3/RF-5: the dwell rule, the one-at-a-time
/// gate, cool-down-from-answer, decline back-off, timeout and every error path.
/// Fake clock only (thread-local, so parallel tests cannot disturb it; tokio
/// tests use the current-thread runtime). No real sleeps.
#[cfg(test)]
mod hardening_tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    thread_local! {
        static NOW: Cell<u64> = const { Cell::new(0) };
    }
    fn fake_clock() -> u64 {
        NOW.with(|n| n.get())
    }
    fn set_now(ms: u64) {
        NOW.with(|n| n.set(ms));
    }

    /// Show one dialog at `shown`, close it at `closed` with `allowed`.
    fn round(gate: &mut DialogGate, shown: u64, closed: u64, allowed: bool) -> CloseEffect {
        let t = gate.try_begin(shown).expect("gate should be free");
        gate.close(t, closed, allowed)
    }

    // ── settle: the dwell rule and the error paths ──────────────────────

    #[test]
    fn settle_allows_only_a_real_allow_after_the_dwell() {
        assert!(settle(DialogOutcome::Answered(true), MIN_DWELL_MS));
        assert!(settle(DialogOutcome::Answered(true), 5_000));
        assert!(!settle(DialogOutcome::Answered(true), MIN_DWELL_MS - 1));
        assert!(!settle(DialogOutcome::Answered(true), 0));
        assert!(
            !settle(DialogOutcome::Answered(true), 150),
            "keystroke-speed allow"
        );
    }

    #[test]
    fn settle_never_turns_a_no_or_an_error_into_a_yes() {
        let elapsed = [0, 1, MIN_DWELL_MS - 1, MIN_DWELL_MS, 60_000, u64::MAX];
        for outcome in [
            DialogOutcome::Answered(false),
            DialogOutcome::NoAnswer,
            DialogOutcome::TimedOut,
        ] {
            for e in elapsed {
                assert!(!settle(outcome, e), "{outcome:?} at {e} ms must be No");
            }
        }
        // And an Allow is only ever kept, never created.
        for e in elapsed {
            assert_eq!(settle(DialogOutcome::Answered(true), e), e >= MIN_DWELL_MS);
        }
    }

    #[test]
    fn allow_label_must_differ_from_the_cancel_label() {
        assert!(allow_label_is_distinct("Allow change"));
        assert!(allow_label_is_distinct("Pair"));
        assert!(!allow_label_is_distinct(""));
        assert!(!allow_label_is_distinct("   "));
        assert!(!allow_label_is_distinct("Cancel"));
        assert!(!allow_label_is_distinct(" cancel "));
        assert!(!allow_label_is_distinct("CANCEL"));
    }

    // ── DialogGate: one at a time, cool-down, back-off ───────────────────

    #[test]
    fn a_second_dialog_while_one_is_open_is_refused() {
        let mut g = DialogGate::new();
        let first = g.try_begin(10_000).expect("first dialog shows");
        // Even minutes later, while the first is still open.
        assert_eq!(g.try_begin(10_001), Err(Refusal::InFlight));
        assert_eq!(g.try_begin(200_000), Err(Refusal::InFlight));
        assert_eq!(g.close(first, 200_500, true), CloseEffect::Closed);
        assert_eq!(g.in_flight, None);
    }

    #[test]
    fn cool_down_runs_from_the_answer_not_from_the_show() {
        let mut g = DialogGate::new();
        // Shown at 0, the user reads it for a minute, answers at 60 s.
        round(&mut g, 0, 60_000, true);
        assert_eq!(
            g.try_begin(60_400),
            Err(Refusal::CoolDown { remaining_ms: 600 }),
            "stamped on the answer, so a dialog shown long ago does not open the window early"
        );
        assert!(g.try_begin(60_000 + COOL_DOWN_MS).is_ok());
    }

    #[test]
    fn three_declines_in_a_minute_start_a_back_off_that_doubles() {
        let mut g = DialogGate::new();
        assert_eq!(round(&mut g, 0, 1_000, false), CloseEffect::Closed);
        assert_eq!(round(&mut g, 3_000, 4_000, false), CloseEffect::Closed);
        assert_eq!(
            round(&mut g, 6_000, 7_000, false),
            CloseEffect::BackOffStarted { wait_ms: 30_000 }
        );
        assert_eq!(
            g.try_begin(10_000),
            Err(Refusal::BackOff {
                remaining_ms: 27_000
            })
        );
        assert!(g.try_begin(37_000).is_ok(), "back-off over");
        let t = g.in_flight.expect("in flight");
        g.close(t, 38_000, false);
        round(&mut g, 40_000, 41_000, false);
        assert_eq!(
            round(&mut g, 43_000, 44_000, false),
            CloseEffect::BackOffStarted { wait_ms: 60_000 },
            "second back-off doubles"
        );
        assert!(matches!(g.try_begin(100_000), Err(Refusal::BackOff { .. })));
        assert!(g.try_begin(104_000).is_ok());
    }

    #[test]
    fn back_off_schedule_doubles_and_is_capped() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(3), 120_000);
        assert_eq!(backoff_ms(5), 480_000);
        assert_eq!(backoff_ms(6), BACKOFF_MAX_MS);
        assert_eq!(backoff_ms(u32::MAX), BACKOFF_MAX_MS);
    }

    #[test]
    fn an_allow_or_a_long_quiet_spell_resets_the_back_off_level() {
        let mut g = DialogGate::new();
        for base in [0, 40_000] {
            round(&mut g, base, base + 100, false);
            round(&mut g, base + 2_000, base + 2_100, false);
            round(&mut g, base + 4_000, base + 4_100, false);
        }
        assert_eq!(g.backoff_level, 2);
        // A human allows something -> level back to 0.
        round(&mut g, 200_000, 201_000, true);
        assert_eq!(g.backoff_level, 0);
        for base in [300_000, 340_000] {
            round(&mut g, base, base + 100, false);
            round(&mut g, base + 2_000, base + 2_100, false);
            round(&mut g, base + 4_000, base + 4_100, false);
        }
        assert_eq!(g.backoff_level, 2);
        // Ten quiet minutes after the last back-off began -> starts at 30 s again.
        let later = 344_100 + BACKOFF_RESET_MS;
        round(&mut g, later, later + 100, false);
        round(&mut g, later + 2_000, later + 2_100, false);
        assert_eq!(
            round(&mut g, later + 4_000, later + 4_100, false),
            CloseEffect::BackOffStarted { wait_ms: 30_000 }
        );
    }

    #[test]
    fn declines_spread_over_more_than_a_minute_never_back_off() {
        let mut g = DialogGate::new();
        for i in 0..50u64 {
            let t = i * 30_001;
            assert_eq!(
                round(&mut g, t, t + 1, false),
                CloseEffect::Closed,
                "decline #{i}"
            );
        }
        assert!(g.recent_declines.len() <= DECLINES_BEFORE_BACKOFF);
    }

    #[test]
    fn one_dialog_per_user_action_is_never_refused() {
        let mut g = DialogGate::new();
        let mut t = 0;
        for i in 0..200u64 {
            // Two declines in a row now and then, then an allow.
            let allowed = i % 3 == 2;
            assert!(g.try_begin(t).is_ok(), "action #{i} refused");
            let ticket = g.in_flight.expect("in flight");
            g.close(ticket, t + 2_000, allowed);
            t += 2_000 + COOL_DOWN_MS + 500;
        }
    }

    #[test]
    fn a_late_close_with_a_stale_ticket_changes_nothing() {
        let mut g = DialogGate::new();
        let old = g.try_begin(0).expect("free");
        assert_eq!(g.close(old, 500, false), CloseEffect::Closed);
        let current = g.try_begin(5_000).expect("free");
        assert_eq!(g.close(old, 5_100, true), CloseEffect::Stale);
        assert_eq!(g.in_flight, Some(current));
        assert_eq!(g.last_closed_ms, Some(500));
    }

    // ── Slot: release on every exit path ─────────────────────────────────

    #[test]
    fn dropping_a_slot_releases_the_gate_and_counts_as_a_decline() {
        set_now(50_000);
        let gate = Mutex::new(DialogGate::new());
        let slot = Slot::acquire(&gate, fake_clock).expect("free");
        assert!(Slot::acquire(&gate, fake_clock).is_none(), "one at a time");
        set_now(52_000);
        drop(slot);
        let g = gate.lock().expect("lock");
        assert_eq!(g.in_flight, None);
        assert_eq!(g.last_closed_ms, Some(52_000));
        assert_eq!(g.recent_declines, vec![52_000]);
    }

    #[test]
    fn a_poisoned_gate_answers_no_without_a_dialog() {
        let gate = Arc::new(Mutex::new(DialogGate::new()));
        let g2 = Arc::clone(&gate);
        let _ = std::thread::spawn(move || {
            let _held = g2.lock().expect("lock");
            panic!("poison the gate on purpose");
        })
        .join();
        assert!(gate.is_poisoned());
        assert!(Slot::acquire(&gate, fake_clock).is_none());
    }

    // ── gated_confirm: the composed policy ───────────────────────────────

    #[tokio::test]
    async fn an_allow_after_reading_is_yes_and_frees_the_gate() {
        set_now(100_000);
        let gate = Mutex::new(DialogGate::new());
        let ok = gated_confirm(&gate, fake_clock, "Allow change", || async {
            set_now(101_500);
            DialogOutcome::Answered(true)
        })
        .await;
        assert!(ok);
        let g = gate.lock().expect("lock");
        assert_eq!(g.in_flight, None);
        assert_eq!(g.last_closed_ms, Some(101_500));
        assert!(g.recent_declines.is_empty());
    }

    #[tokio::test]
    async fn a_keystroke_speed_allow_is_no_and_counts_as_a_decline() {
        set_now(100_000);
        let gate = Mutex::new(DialogGate::new());
        let ok = gated_confirm(&gate, fake_clock, "Allow change", || async {
            set_now(100_120);
            DialogOutcome::Answered(true)
        })
        .await;
        assert!(!ok);
        let g = gate.lock().expect("lock");
        assert_eq!(g.in_flight, None);
        assert_eq!(g.recent_declines, vec![100_120]);
    }

    #[tokio::test]
    async fn every_error_outcome_answers_no() {
        for outcome in [DialogOutcome::NoAnswer, DialogOutcome::TimedOut] {
            set_now(0);
            let gate = Mutex::new(DialogGate::new());
            let ok = gated_confirm(&gate, fake_clock, "Allow", || async move {
                set_now(5_000);
                outcome
            })
            .await;
            assert!(!ok, "{outcome:?}");
            assert_eq!(gate.lock().expect("lock").in_flight, None);
        }
    }

    #[tokio::test]
    async fn refused_requests_never_show_a_dialog() {
        set_now(10_000);
        // In flight.
        let busy = Mutex::new(DialogGate::new());
        let _held = busy.lock().expect("lock").try_begin(10_000);
        // Cool-down.
        let cooling = Mutex::new(DialogGate::new());
        round(&mut cooling.lock().expect("lock"), 9_000, 9_500, true);
        // Poisoned.
        let poisoned = Arc::new(Mutex::new(DialogGate::new()));
        let p2 = Arc::clone(&poisoned);
        let _ = std::thread::spawn(move || {
            let _g = p2.lock().expect("lock");
            panic!("poison the gate on purpose");
        })
        .join();

        for (name, gate, label) in [
            ("in flight", &busy, "Allow"),
            ("cool-down", &cooling, "Allow"),
            ("poisoned", &*poisoned, "Allow"),
            ("cancel label", &Mutex::new(DialogGate::new()), "Cancel"),
        ] {
            let shown = AtomicBool::new(false);
            let ok = gated_confirm(gate, fake_clock, label, || {
                shown.store(true, Ordering::SeqCst);
                async { DialogOutcome::Answered(true) }
            })
            .await;
            assert!(!ok, "{name}");
            assert!(
                !shown.load(Ordering::SeqCst),
                "{name}: no dialog may be shown"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_confirm_releases_the_gate() {
        set_now(10_000);
        let gate = Mutex::new(DialogGate::new());
        let fut = gated_confirm(&gate, fake_clock, "Allow", || {
            std::future::pending::<DialogOutcome>()
        });
        assert!(tokio::time::timeout(Duration::from_secs(1), fut)
            .await
            .is_err());
        let g = gate.lock().expect("lock");
        assert_eq!(g.in_flight, None, "the awaiting future was dropped");
        assert_eq!(g.recent_declines.len(), 1);
    }

    // ── await_answer: dropped sender and timeout ─────────────────────────

    #[tokio::test]
    async fn await_answer_reports_the_button_and_a_dropped_sender() {
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        assert!(tx.send(true).is_ok());
        assert_eq!(
            await_answer(rx, ANSWER_TIMEOUT).await,
            DialogOutcome::Answered(true)
        );
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        drop(tx);
        assert_eq!(
            await_answer(rx, ANSWER_TIMEOUT).await,
            DialogOutcome::NoAnswer
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_answer_times_out() {
        let (_tx, rx) = tokio::sync::oneshot::channel::<bool>();
        assert_eq!(
            await_answer(rx, ANSWER_TIMEOUT).await,
            DialogOutcome::TimedOut
        );
    }
}
