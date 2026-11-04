import { createResource, createSignal, onMount, Show } from "solid-js";
import { commands, unwrap, isTauri } from "../../lib/invoke";
import { toastStore } from "../Toast/Toast";
import { roostStore } from "../../stores/roost";
import { isMobile } from "../../lib/platform";

/**
 * "Connect AI tools" settings section.
 *
 * One-click wiring of the bundled `corvux-mcp` helper into Claude Desktop's
 * config (via the `claude_desktop_mcp` IPC command, which spawns the helper
 * with `--connect <roost>` / `--disconnect` / `--status`). Desktop only —
 * the websites can't reach a local server; this is for the Claude Desktop
 * app on the same machine.
 */
export default function McpToolsSection() {
  type McpMode = "read-only" | "capture-only" | "full";
  const [status, setStatus] = createSignal<string>("");
  const [busy, setBusy] = createSignal(false);
  const [copied, setCopied] = createSignal(false);
  const [mode, setMode] = createSignal<McpMode>("read-only");

  const connected = () => status().startsWith("Connected");

  // The copy-paste MCP config for any other client (Cursor, Claude Code,
  // Windsurf, Cline…). Re-fetched whenever the active roost changes; only
  // runs in Tauri with a roost open (the fetcher is skipped otherwise).
  const [config] = createResource(
    () => {
      const roost = isTauri ? roostStore.roostPath() : undefined;
      return roost ? JSON.stringify([roost, mode()]) : undefined;
    },
    async (source) => {
      const [roost, selectedMode] = JSON.parse(source) as [string, McpMode];
      try {
        return await unwrap(commands.mcpConfigJson(roost, selectedMode));
      } catch (e) {
        return `// Couldn't generate config: ${e}`;
      }
    },
  );

  async function copyConfig() {
    const text = config();
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toastStore.show("Couldn't copy — select the text and copy it manually.", "error");
    }
  }

  async function refreshStatus() {
    if (!isTauri) {
      setStatus("Desktop only.");
      return;
    }
    try {
      setStatus(await unwrap(commands.claudeDesktopMcp("status", null, null)));
    } catch (e) {
      setStatus(`Couldn't check: ${e}`);
    }
  }

  onMount(refreshStatus);

  async function connect() {
    const roost = roostStore.roostPath();
    if (!roost) {
      toastStore.show("Open a roost first.", "error");
      return;
    }
    const exactConfig = config();
    const disclosure =
      mode() === "full"
        ? "FULL access lets the AI client read and modify every non-hidden Markdown note in this roost. Tool results may be sent to that client's cloud model provider."
        : mode() === "capture-only"
          ? "CAPTURE-ONLY access lets the AI client create captures, tasks, and reminders, but not read existing notes. Tool inputs may be sent through that client's cloud model provider."
          : "READ-ONLY access lets the AI client read every non-hidden Markdown note in this roost. Note content returned by tools may be sent to that client's cloud model provider.";
    if (
      !window.confirm(
        `${disclosure}\n\nCorvux will add this exact local command to Claude Desktop:\n\n${exactConfig ?? "Configuration is still loading."}\n\nContinue?`,
      )
    ) {
      return;
    }
    setBusy(true);
    try {
      const msg = await unwrap(commands.claudeDesktopMcp("connect", roost, mode()));
      toastStore.show(msg, "success", 6000);
      await refreshStatus();
    } catch (e) {
      toastStore.show(`Connect failed: ${e}`, "error");
    } finally {
      setBusy(false);
    }
  }

  async function disconnect() {
    setBusy(true);
    try {
      const msg = await unwrap(commands.claudeDesktopMcp("disconnect", null, null));
      toastStore.show(msg, "success", 5000);
      await refreshStatus();
    } catch (e) {
      toastStore.show(`Disconnect failed: ${e}`, "error");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section>
      <h3 class="text-text-secondary text-xs font-semibold uppercase tracking-wider mb-3">
        Connect AI tools
      </h3>

      <Show
        when={!isMobile()}
        fallback={
          <p class="text-text-muted text-xs leading-relaxed">
            Connecting to Claude Desktop is a desktop-only feature — the
            helper runs on your computer, where your roost lives.
          </p>
        }
      >
        <div class="space-y-3">
          <p class="text-text-muted text-xs leading-relaxed">
            Let the Claude Desktop app work with this roost — capture chats,
            add tasks and reminders, search your notes. One click wires up the
            Corvux helper; restart Claude Desktop afterwards to finish. The
            helper stays local, but content returned to an AI client may be
            sent to that client's cloud model provider.
          </p>

          <label class="block space-y-1">
            <span class="text-text-primary text-sm">Access level</span>
            <select
              value={mode()}
              onChange={(event) => setMode(event.currentTarget.value as McpMode)}
              class="w-full rounded-md bg-bg-secondary border border-border px-3 py-2 text-text-primary text-sm"
            >
              <option value="read-only">Read only (recommended)</option>
              <option value="capture-only">Capture, tasks and reminders only</option>
              <option value="full">Full read and write access</option>
            </select>
            <span class="block text-text-muted text-[11px]">
              Read only is the safest default. Full access includes overwrite,
              append, properties, tags, and task mutation tools.
            </span>
          </label>

          <Show when={roostStore.roostPath()}>
            <div class="flex items-center justify-between gap-3">
              <div class="text-text-primary text-sm shrink-0">Current Roost</div>
              <div class="text-text-secondary text-xs bg-bg-secondary px-3 py-1 rounded-lg border border-border truncate">
                {roostStore.roostPath()}
              </div>
            </div>
          </Show>

          <div class="flex items-center justify-between gap-3">
            <div class="min-w-0">
              <div class="text-text-primary text-sm">Claude Desktop</div>
              <div class="text-text-muted text-xs truncate" aria-live="polite">
                {status() || "Checking…"}
              </div>
            </div>
            <div class="flex gap-2 shrink-0">
              <button
                onClick={connect}
                disabled={busy() || !roostStore.roostPath()}
                class="px-3 py-1.5 rounded-md bg-accent text-white text-xs font-medium hover:bg-accent-hover disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
              >
                {connected() ? "Reconnect" : "Connect"}
              </button>
              <Show when={connected()}>
                <button
                  onClick={disconnect}
                  disabled={busy()}
                  class="px-3 py-1.5 rounded-md bg-bg-secondary border border-border text-text-primary text-xs font-medium hover:bg-bg-hover disabled:opacity-50 transition-colors"
                >
                  Disconnect
                </button>
              </Show>
            </div>
          </div>

          <p class="text-text-muted text-[11px] leading-relaxed">
            After connecting (or switching roosts), fully quit and reopen
            Claude Desktop so it reloads the connection.
          </p>

          <Show when={roostStore.roostPath()}>
            <div class="border-t border-border pt-3 space-y-2">
              <div class="flex items-center justify-between gap-3">
                <div class="min-w-0">
                  <div class="text-text-primary text-sm">Other AI tools</div>
                  <div class="text-text-muted text-xs">
                    Cursor, Claude Code, Windsurf, Cline — paste this into the
                    tool's MCP config.
                  </div>
                </div>
                <button
                  onClick={copyConfig}
                  disabled={!config() || config()!.startsWith("//")}
                  class="px-3 py-1.5 rounded-md bg-bg-secondary border border-border text-text-primary text-xs font-medium hover:bg-bg-hover disabled:opacity-50 disabled:cursor-not-allowed transition-colors shrink-0"
                >
                  {copied() ? "Copied!" : "Copy"}
                </button>
              </div>

              <pre class="text-text-secondary text-[11px] leading-relaxed bg-bg-secondary border border-border rounded-lg p-3 overflow-x-auto whitespace-pre">
                {config() ?? "Generating…"}
              </pre>

              <p class="text-text-muted text-[11px] leading-relaxed">
                Where it goes: Cursor → <code>~/.cursor/mcp.json</code>; Claude
                Code → <code>.mcp.json</code> (or <code>claude mcp add</code>).
                If you move or reinstall Corvux, copy this again — the path
                changes.
              </p>
            </div>
          </Show>
        </div>
      </Show>
    </section>
  );
}
