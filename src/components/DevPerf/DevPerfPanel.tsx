import { Show } from "solid-js";
import { devPerfStore } from "../../stores/devPerf";

/**
 * Hidden developer perf panel. Renders only when `devPerfStore.open()`
 * is true (toggled via Ctrl+Alt+P — see lib/keyboard-shortcuts.ts).
 *
 * Floating bottom-right card so it doesn't compete with the editor or
 * sidebars. Updates every 2s while visible. No interactivity beyond
 * dismiss + manual refresh — this is a read-out, not a control surface.
 */
export default function DevPerfPanel() {
  return (
    <Show when={devPerfStore.open()}>
      <div
        class="fixed bottom-4 right-4 z-[120] w-[320px] max-h-[80vh] overflow-y-auto rounded-lg border border-accent/40 bg-bg-primary/95 shadow-2xl text-[11px] font-mono"
        style={{ "backdrop-filter": "blur(8px)" }}
        role="status"
        aria-label="Developer performance panel"
      >
        <header class="flex items-center justify-between px-3 py-2 border-b border-border bg-accent/10">
          <span class="text-accent font-semibold tracking-wide uppercase text-[10px]">
            Dev perf · Ctrl+Alt+P
          </span>
          <button
            onClick={() => devPerfStore.hide()}
            class="text-text-muted hover:text-text-primary p-0.5"
            aria-label="Close perf panel"
          >
            <svg
              width="12"
              height="12"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              stroke-width="2"
            >
              <path d="M18 6L6 18M6 6l12 12" />
            </svg>
          </button>
        </header>

        <div class="p-3 space-y-3">
          <Show when={devPerfStore.error()}>
            {(err) => (
              <div class="text-red text-[11px]">Snapshot error: {err()}</div>
            )}
          </Show>

          <Show when={devPerfStore.snapshot()} fallback={<div class="text-text-muted">Loading…</div>}>
            {(snap) => (
              <>
                <Section title="Process">
                  <Row label="RSS">
                    <Show when={snap().rss_mb !== null} fallback={<span>—</span>}>
                      {snap().rss_mb!.toFixed(1)} MB
                    </Show>
                  </Row>
                  <Row label="Free RAM">{snap().free_ram_mb.toLocaleString()} MB</Row>
                  <Row label="As of">{snap().at.slice(11, 19)}</Row>
                </Section>

                <Section title="Counters (this session)">
                  <Row label="chat_send">{snap().counters.chat_send_total}</Row>
                  <Row label="roost_open">{snap().counters.roost_open_total}</Row>
                </Section>

                <Show when={snap().counters.last_chat_send}>
                  {(b) => (
                    <Section title={`Last chat (${b().profile})`}>
                      <Row label="Total setup">{b().setup_ms} ms</Row>
                      <Row label="· prompt build">{b().prompt_ms} ms</Row>
                      <Row label="prompt chars">
                        {b().system_prompt_chars.toLocaleString()}
                      </Row>
                      <Row label="At">{b().at.slice(11, 19)}</Row>
                    </Section>
                  )}
                </Show>
              </>
            )}
          </Show>

          <button
            onClick={() => void devPerfStore.refresh()}
            class="w-full text-[10px] px-2 py-1 rounded border border-border text-text-muted hover:text-text-primary hover:bg-bg-hover transition-colors"
          >
            Refresh now
          </button>
        </div>
      </div>
    </Show>
  );
}

function Section(props: { title: string; children: unknown }) {
  return (
    <section>
      <h3 class="text-text-muted text-[10px] uppercase tracking-wider mb-1">
        {props.title}
      </h3>
      <div class="rounded border border-border/60 bg-bg-secondary/40 divide-y divide-border/40">
        {props.children as any}
      </div>
    </section>
  );
}

function Row(props: { label: string; children: unknown }) {
  return (
    <div class="flex items-center justify-between px-2 py-1">
      <span class="text-text-muted">{props.label}</span>
      <span class="text-text-primary tabular-nums">{props.children as any}</span>
    </div>
  );
}
