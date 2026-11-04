import { Show } from "solid-js";

export interface ScanActionsProps {
  scan: { processed: number; total: number } | null;
  rescanning: boolean;
  onRescan: () => void;
  onResetDocstore: () => void;
}

/// Manual rescan + sync-history reset, with a progress bar that renders
/// only while a scan is in flight. The reset button is the last-resort
/// recovery for the "edits on one side wipe the other" failure mode and
/// is gated behind a `window.confirm` in the parent before reaching here.
export default function ScanActions(props: ScanActionsProps) {
  return (
    <div class="space-y-1.5 pt-2 border-t border-border">
      <Show when={props.scan}>
        {(s) => (
          <div class="bg-bg-secondary rounded px-3 py-2 space-y-1">
            <div class="flex items-center justify-between text-xs">
              <span class="text-text-secondary">Scanning Roost...</span>
              <span class="text-text-muted font-mono">
                {s().processed} / {s().total || "?"}
              </span>
            </div>
            <div class="h-1 bg-bg-tertiary rounded overflow-hidden">
              <div
                class="h-full bg-blue-400 transition-all"
                style={{
                  width:
                    s().total > 0
                      ? `${Math.min(100, (s().processed / s().total) * 100)}%`
                      : "100%",
                }}
              />
            </div>
          </div>
        )}
      </Show>
      <button
        class="w-full bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-3 py-1.5 text-sm disabled:opacity-50"
        onClick={props.onRescan}
        disabled={props.rescanning}
        title="Re-read every markdown file from disk and sync to paired peers. Safe to run anytime."
      >
        {props.rescanning ? "Scanning..." : "Rescan Roost & sync now"}
      </button>
      <div class="text-text-muted text-[10px]">
        Use this the first time you pair, or whenever files feel out of sync.
      </div>
      <button
        class="w-full bg-bg-secondary hover:bg-bg-tertiary border border-border rounded px-3 py-1.5 text-sm mt-2"
        onClick={props.onResetDocstore}
        title="Full sync reset: wipes local sync history, forgets all paired devices, clears auto-reconnect cache. Roost files are left intact. Re-pair each device after."
      >
        Forget sync &amp; start over
      </button>
      <div class="text-text-muted text-[10px]">
        Recovery action: forgets every paired device + wipes sync history.
        Your roost files are not touched. You'll need to re-pair afterward.
      </div>
    </div>
  );
}
