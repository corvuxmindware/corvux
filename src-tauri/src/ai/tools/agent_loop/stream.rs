//! Stream-event reducer for the V2 agent loop — the first slice of the AUDIT-035
//! split ("Stream-event reducer → `agent_loop/stream.rs`").
//!
//! One function, [`drain_stream`], owns everything the loop does while a reply
//! is arriving: collect the text and tool calls, honour the user's Stop, and —
//! review AT-6 — enforce the limits the loop used to check only BETWEEN
//! iterations:
//!
//! * a time limit ([`StreamTimeLimit`]), checked on every event and on every
//!   quiet tick, so a reply can no longer outlive its limit just because no
//!   iteration boundary comes. Cloud providers get the agent's wall-clock budget;
//!   a local or private model server (Ollama) gets a PROGRESS limit instead,
//!   because a CPU-only model can legitimately stream a long answer slowly;
//! * the size of what is held: total text, total tool-call arguments, number of
//!   tool calls (`ai::streaming` caps).
//!
//! It touches no Tauri type, so it is unit-tested without a `MockRuntime`.
//!
//! The provider's task keeps reading until it sees its own cancel flag, and
//! dropping the receiver does not stop a task that is only seeing keep-alives. So
//! the loop hands the provider a PER-ITERATION `stream_cancel` flag, and this
//! function sets it whenever it stops early (user Stop, time limit, size cap).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc::Receiver;
use tokio::time::{timeout, Instant};

use crate::ai::streaming::{
    push_capped, too_many_tool_calls_error, ByteCounter, MAX_STREAMED_TEXT_BYTES,
    MAX_STREAMED_TOOL_ARG_BYTES, MAX_STREAMED_TOOL_CALLS,
};
use crate::ai::tool_protocol::{StopReason, StreamEvent, ToolCall};

/// How long to wait for the next event before re-checking Stop and the limit.
/// The providers poll their own cancel flag every 100 ms; this matches it.
const QUIET_TICK: Duration = Duration::from_millis(100);

/// How ONE reply is bounded in time.
#[derive(Debug, Clone, Copy)]
pub(super) enum StreamTimeLimit {
    /// Cloud providers: the reply must finish inside the agent loop's
    /// wall-clock budget, counted from when the loop started.
    WallClock {
        loop_started: Instant,
        max_secs: u64,
    },
    /// A local or private model server: a slow-but-steady reply may run as long
    /// as events keep arriving. Only a gap of `idle` with NOTHING arriving stops
    /// it. Everything else that bounds such a reply still applies: the size caps,
    /// the Stop button, and the 30-minute stream backstop in `SseEvents`. (The
    /// between-iterations wall-clock check in the loop is unchanged.)
    Progress { idle: Duration },
}

/// Pick the limit for a provider. `is_local()` is true for Ollama (a model
/// server on the user's machine or network) and false for every cloud provider;
/// the consent and trace wrappers both delegate it.
pub(super) fn limit_for(
    provider_is_local: bool,
    loop_started: std::time::Instant,
    max_wall_clock_secs: u64,
) -> StreamTimeLimit {
    if provider_is_local {
        // The same five minutes the Ollama HTTP client uses as its per-read idle
        // timeout (`USER_URL_READ_TIMEOUT`): a silent stretch that long is a
        // stalled server, and the byte-level timeout alone cannot see a server
        // that keeps the connection warm with bytes that carry no tokens.
        StreamTimeLimit::Progress {
            idle: crate::ai::http::USER_URL_READ_TIMEOUT,
        }
    } else {
        StreamTimeLimit::WallClock {
            loop_started: Instant::from_std(loop_started),
            max_secs: max_wall_clock_secs,
        }
    }
}

/// Everything one iteration's stream delivered.
#[derive(Debug, Default)]
pub(super) struct DrainedStream {
    pub text: String,
    pub calls: Vec<ToolCall>,
    /// `None` when the channel closed without a `Stop` event.
    pub stop_reason: Option<StopReason>,
}

/// User-visible reason a reply was cut off by the wall-clock budget.
pub(super) fn wall_clock_error(max_wall_clock_secs: u64) -> String {
    format!(
        "The AI reply was stopped because the agent's {max_wall_clock_secs}-second time limit \
         ran out while the reply was still arriving."
    )
}

/// User-visible reason a local model's reply was cut off for going quiet.
pub(super) fn stalled_error(idle: Duration) -> String {
    format!(
        "The AI reply was stopped because nothing arrived for {} seconds, so the model \
         server looks stuck.",
        idle.as_secs()
    )
}

/// Drain one iteration's event stream.
///
/// * `Ok(Some(drained))` — the stream ended (a `Stop` event, or the channel
///   closed).
/// * `Ok(None)` — the user pressed Stop; the caller returns what it has.
/// * `Err(message)` — the reply was stopped for running past its time limit or a
///   size cap. `message` is safe to show the user.
///
/// On every early exit `stream_cancel` is set so the provider task stops
/// reading instead of lingering on a connection nobody is listening to.
pub(super) async fn drain_stream(
    rx: &mut Receiver<StreamEvent>,
    cancel: &AtomicBool,
    stream_cancel: &AtomicBool,
    limit: StreamTimeLimit,
) -> Result<Option<DrainedStream>, String> {
    let mut out = DrainedStream::default();
    let mut arg_bytes = ByteCounter::default();
    let mut last_progress = Instant::now();
    let stop = |flag: &AtomicBool| flag.store(true, Ordering::Relaxed);

    loop {
        if cancel.load(Ordering::Relaxed) {
            stop(stream_cancel);
            return Ok(None);
        }
        match limit {
            StreamTimeLimit::WallClock {
                loop_started,
                max_secs,
            } => {
                if loop_started.elapsed().as_secs() >= max_secs {
                    stop(stream_cancel);
                    return Err(wall_clock_error(max_secs));
                }
            }
            StreamTimeLimit::Progress { idle } => {
                if last_progress.elapsed() >= idle {
                    stop(stream_cancel);
                    return Err(stalled_error(idle));
                }
            }
        }

        let event = match timeout(QUIET_TICK, rx.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,  // channel closed
            Err(_) => continue, // quiet tick: look at Stop and the limit again
        };
        // Anything arriving counts as progress; only silence is stall.
        last_progress = Instant::now();

        match event {
            StreamEvent::TextChunk(chunk) => {
                // The provider already emitted `ai:chunk` for the panel; this is
                // the copy kept for the assistant history entry.
                if let Err(msg) =
                    push_capped(&mut out.text, &chunk, MAX_STREAMED_TEXT_BYTES, "text")
                {
                    stop(stream_cancel);
                    return Err(msg);
                }
            }
            StreamEvent::ToolCall(call) => {
                let over = if out.calls.len() >= MAX_STREAMED_TOOL_CALLS {
                    Some(too_many_tool_calls_error())
                } else {
                    arg_bytes
                        .add(
                            call.args.to_string().len(),
                            MAX_STREAMED_TOOL_ARG_BYTES,
                            "tool-call data",
                        )
                        .err()
                };
                if let Some(msg) = over {
                    stop(stream_cancel);
                    return Err(msg);
                }
                out.calls.push(call);
            }
            StreamEvent::Stop(reason) => {
                out.stop_reason = Some(reason);
                break;
            }
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: format!("id-{name}"),
            name: name.into(),
            args,
            provider_metadata: None,
        }
    }

    fn flags() -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// A cloud-style limit starting now.
    fn wall(max_secs: u64) -> StreamTimeLimit {
        StreamTimeLimit::WallClock {
            loop_started: Instant::now(),
            max_secs,
        }
    }

    fn progress(idle_ms: u64) -> StreamTimeLimit {
        StreamTimeLimit::Progress {
            idle: Duration::from_millis(idle_ms),
        }
    }

    #[tokio::test]
    async fn collects_text_and_calls_until_stop() {
        let (tx, mut rx) = mpsc::channel(16);
        tx.send(StreamEvent::TextChunk("Hel".into())).await.unwrap();
        tx.send(StreamEvent::TextChunk("lo".into())).await.unwrap();
        tx.send(StreamEvent::ToolCall(call("a", json!({"k": 1}))))
            .await
            .unwrap();
        tx.send(StreamEvent::Stop(StopReason::ToolUse))
            .await
            .unwrap();
        let (cancel, stream_cancel) = flags();
        let drained = drain_stream(&mut rx, &cancel, &stream_cancel, wall(300))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(drained.text, "Hello");
        assert_eq!(drained.calls.len(), 1);
        assert!(matches!(drained.stop_reason, Some(StopReason::ToolUse)));
        assert!(
            !stream_cancel.load(Ordering::Relaxed),
            "a normal end must not cancel"
        );
    }

    #[tokio::test]
    async fn a_closed_channel_without_stop_is_a_normal_end() {
        for limit in [wall(300), progress(60_000)] {
            let (tx, mut rx) = mpsc::channel(4);
            tx.send(StreamEvent::TextChunk("partial".into()))
                .await
                .unwrap();
            drop(tx);
            let (cancel, stream_cancel) = flags();
            let drained = drain_stream(&mut rx, &cancel, &stream_cancel, limit)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(drained.text, "partial");
            assert!(drained.stop_reason.is_none());
        }
    }

    /// AT-6: the loop's own copy of the text is bounded, and the provider is told
    /// to stop. The size caps apply to a local model too.
    #[tokio::test]
    async fn text_past_the_cap_ends_the_reply_with_a_clear_error() {
        for limit in [wall(300), progress(60_000)] {
            let (tx, mut rx) = mpsc::channel(64);
            let mib = "x".repeat(1024 * 1024);
            let sender = tokio::spawn(async move {
                for _ in 0..(MAX_STREAMED_TEXT_BYTES / mib.len() + 1) {
                    if tx.send(StreamEvent::TextChunk(mib.clone())).await.is_err() {
                        return;
                    }
                }
            });
            let (cancel, stream_cancel) = flags();
            let err = drain_stream(&mut rx, &cancel, &stream_cancel, limit)
                .await
                .unwrap_err();
            assert!(err.contains("text") && err.contains("stopped"), "{err}");
            assert!(
                stream_cancel.load(Ordering::Relaxed),
                "the provider must be told to stop"
            );
            drop(rx);
            let _ = sender.await;
        }
    }

    #[tokio::test]
    async fn too_many_tool_calls_end_the_reply() {
        let (tx, mut rx) = mpsc::channel(MAX_STREAMED_TOOL_CALLS + 2);
        for i in 0..=MAX_STREAMED_TOOL_CALLS {
            tx.send(StreamEvent::ToolCall(call(&format!("t{i}"), json!({}))))
                .await
                .unwrap();
        }
        let (cancel, stream_cancel) = flags();
        let err = drain_stream(&mut rx, &cancel, &stream_cancel, wall(300))
            .await
            .unwrap_err();
        assert!(err.contains("tool calls"), "{err}");
        assert!(stream_cancel.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn oversized_tool_arguments_end_the_reply() {
        let (tx, mut rx) = mpsc::channel(4);
        let big = "x".repeat(MAX_STREAMED_TOOL_ARG_BYTES + 1);
        tx.send(StreamEvent::ToolCall(call("w", json!({ "body": big }))))
            .await
            .unwrap();
        let (cancel, stream_cancel) = flags();
        let err = drain_stream(&mut rx, &cancel, &stream_cancel, wall(300))
            .await
            .unwrap_err();
        assert!(err.contains("tool-call data"), "{err}");
        assert!(stream_cancel.load(Ordering::Relaxed));
    }

    /// AT-6: the budget is enforced INSIDE a cloud stream. Here the budget ran
    /// out before the first event even arrives.
    #[tokio::test]
    async fn an_exhausted_budget_stops_a_cloud_stream_immediately() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(StreamEvent::TextChunk("late".into()))
            .await
            .unwrap();
        let limit = StreamTimeLimit::WallClock {
            loop_started: Instant::now() - Duration::from_secs(10),
            max_secs: 5,
        };
        let (cancel, stream_cancel) = flags();
        let err = drain_stream(&mut rx, &cancel, &stream_cancel, limit)
            .await
            .unwrap_err();
        assert_eq!(err, wall_clock_error(5));
        assert!(err.contains("5-second"), "{err}");
        assert!(stream_cancel.load(Ordering::Relaxed));
    }

    /// Virtual time (`start_paused`): the sleeps below cost no real time and
    /// cannot be disturbed by a busy machine.
    ///
    /// A cloud-style stream that is STEADY but runs past the budget IS cut: ten
    /// tokens, one every 200 ms (2 s in all), against a 1 s budget.
    #[tokio::test(start_paused = true)]
    async fn a_steady_cloud_stream_past_the_budget_is_cut() {
        let (tx, mut rx) = mpsc::channel(4);
        let started = Instant::now();
        let sender = tokio::spawn(async move {
            for i in 0..10 {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if tx
                    .send(StreamEvent::TextChunk(format!("t{i} ")))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = tx.send(StreamEvent::Stop(StopReason::EndTurn)).await;
        });
        let limit = StreamTimeLimit::WallClock {
            loop_started: started,
            max_secs: 1,
        };
        let (cancel, stream_cancel) = flags();
        let err = drain_stream(&mut rx, &cancel, &stream_cancel, limit)
            .await
            .unwrap_err();
        assert_eq!(err, wall_clock_error(1));
        assert!(started.elapsed() < Duration::from_secs(2), "cut mid-reply");
        assert!(stream_cancel.load(Ordering::Relaxed));
        drop(rx);
        let _ = sender.await;
    }

    /// The same stream from a local model (Ollama on a CPU) is NOT cut: tokens
    /// keep arriving, each one resets the idle timer, so a slow-but-steady reply
    /// runs to its natural end however long it takes.
    #[tokio::test(start_paused = true)]
    async fn a_slow_but_steady_local_stream_past_the_budget_is_not_cut() {
        let (tx, mut rx) = mpsc::channel(4);
        let started = Instant::now();
        tokio::spawn(async move {
            for i in 0..10 {
                // 200 ms between tokens against a 300 ms idle limit.
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = tx.send(StreamEvent::TextChunk(format!("t{i} "))).await;
            }
            let _ = tx.send(StreamEvent::Stop(StopReason::EndTurn)).await;
        });
        let (cancel, stream_cancel) = flags();
        let drained = drain_stream(&mut rx, &cancel, &stream_cancel, progress(300))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(drained.text, "t0 t1 t2 t3 t4 t5 t6 t7 t8 t9 ");
        assert!(matches!(drained.stop_reason, Some(StopReason::EndTurn)));
        assert!(
            started.elapsed() >= Duration::from_secs(2),
            "the reply really did run past a 1 s budget"
        );
        assert!(!stream_cancel.load(Ordering::Relaxed));
    }

    /// A local stream that goes silent IS stopped, by the idle limit.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_local_stream_is_cut_by_the_idle_limit() {
        let (tx, mut rx) = mpsc::channel(4);
        let started = Instant::now();
        // Two tokens, then silence while the connection stays open.
        tx.send(StreamEvent::TextChunk("a".into())).await.unwrap();
        tx.send(StreamEvent::TextChunk("b".into())).await.unwrap();
        let (cancel, stream_cancel) = flags();
        let err = drain_stream(&mut rx, &cancel, &stream_cancel, progress(300))
            .await
            .unwrap_err();
        assert_eq!(err, stalled_error(Duration::from_millis(300)));
        assert!(err.contains("nothing arrived"), "{err}");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(300) && waited < Duration::from_secs(1),
            "{waited:?}"
        );
        assert!(stream_cancel.load(Ordering::Relaxed));
        drop(tx); // sender was alive the whole time
    }

    /// The case the old between-iterations check could never catch: a cloud
    /// server that keeps the connection open and says nothing (or only
    /// keep-alives, which the provider swallows). No event ever arrives, yet the
    /// budget still ends it.
    #[tokio::test(start_paused = true)]
    async fn a_silent_cloud_stream_is_ended_by_the_budget() {
        let (_tx, mut rx) = mpsc::channel::<StreamEvent>(4); // sender stays alive
        let (cancel, stream_cancel) = flags();
        let started = Instant::now();
        let limit = StreamTimeLimit::WallClock {
            loop_started: started,
            max_secs: 1,
        };
        let err = drain_stream(&mut rx, &cancel, &stream_cancel, limit)
            .await
            .unwrap_err();
        assert!(err.contains("1-second"));
        assert!(stream_cancel.load(Ordering::Relaxed));
        assert!(started.elapsed() >= Duration::from_secs(1));
    }

    /// Stop works the same under both limits and is passed on to the provider.
    #[tokio::test(start_paused = true)]
    async fn user_stop_is_honoured_and_propagated_to_the_provider() {
        for limit in [wall(300), progress(60_000)] {
            let (_tx, mut rx) = mpsc::channel::<StreamEvent>(4);
            let (cancel, stream_cancel) = flags();
            let setter = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                setter.store(true, Ordering::Relaxed);
            });
            let result = drain_stream(&mut rx, &cancel, &stream_cancel, limit).await;
            assert!(matches!(result, Ok(None)));
            assert!(stream_cancel.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn a_local_provider_gets_the_progress_limit_and_a_cloud_one_the_budget() {
        let started = std::time::Instant::now();
        match limit_for(true, started, 300) {
            StreamTimeLimit::Progress { idle } => {
                assert_eq!(idle, crate::ai::http::USER_URL_READ_TIMEOUT)
            }
            other => panic!("a local provider must get the progress limit, got {other:?}"),
        }
        match limit_for(false, started, 300) {
            StreamTimeLimit::WallClock { max_secs, .. } => assert_eq!(max_secs, 300),
            other => panic!("a cloud provider must get the wall-clock budget, got {other:?}"),
        }
    }

    /// The choice is made from the PROVIDER, in `run_agent_loop_v2`. A source
    /// check, because the loop itself cannot run without a `MockRuntime`: if the
    /// call goes back to a hard-coded budget, a local model is cut at 300 s again.
    #[test]
    fn the_loop_picks_the_limit_from_the_provider() {
        // Whitespace is squashed so rustfmt's line wrapping cannot matter.
        let squashed: String = include_str!("mod.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            squashed.contains("stream::limit_for(chat_provider.is_local(),"),
            "run_agent_loop_v2 must call stream::limit_for(chat_provider.is_local(), ..)"
        );
    }
}
