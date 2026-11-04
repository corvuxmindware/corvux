import { createResource, createSignal, For, Show } from "solid-js";
import { commands, isTauri, unwrap } from "../../lib/invoke";
import type { SkillListView as SkillView } from "../../lib/bindings";
import { log } from "../../lib/log";
import { roostStore } from "../../stores/roost";
import { tabStore } from "../../stores/tabs";
import { promptStore } from "../Modals/PromptModal";

/** Settings → AI → Skills.
 *
 *  Pure read-only view today: lists every skill registered in
 *  `<roost>/.corvux/skills/` and `~/.corvux/skills/`, plus a Refresh
 *  button that triggers a one-shot rescan (the hot-reload watcher
 *  already covers fs-event-driven updates; this is for "I just dropped
 *  a folder, show me now" without waiting). Adding/editing skills is
 *  filesystem-only — users author SKILL.md directly. */
export default function SkillsPanel() {
  const [skills, { refetch }] = createResource<SkillView[]>(async () => {
    if (!isTauri) return [];
    try {
      return await commands.skillList();
    } catch (e) {
      log.error("skills", "skill_list failed", e);
      return [];
    }
  });

  const [creating, setCreating] = createSignal(false);
  const [createError, setCreateError] = createSignal<string | null>(null);

  const handleRefresh = async () => {
    if (!isTauri) return;
    try {
      // `skill_refresh` takes an optional roost path; pass the active
      // one so the rescan picks up local skills under
      // `<roost>/.corvux/skills/`. Backend falls back to global-only
      // when roostPath is undefined.
      await unwrap(commands.skillRefresh(roostStore.roostPath() ?? null));
      await refetch();
    } catch (e) {
      log.error("skills", "skill_refresh failed", e);
    }
  };

  const handleCreate = async () => {
    if (!isTauri) return;
    setCreateError(null);
    const roost = roostStore.roostPath();
    if (!roost) {
      setCreateError("Open a roost first.");
      return;
    }
    // In-app modal — the native `window.prompt` renders as a
    // localhost:1420 popup in WebView2 (ugly + breaks the dark theme).
    // Validation runs live so the user sees the rule before submit;
    // the backend re-validates and returns its own error string on
    // anything that slips through.
    const raw = await promptStore.ask({
      title: "New skill",
      description:
        "Creates `<roost>/.corvux/skills/<name>/SKILL.md`. Pick a kebab-case name.",
      placeholder: "e.g. weekly-fitness-summary",
      confirmLabel: "Create",
      validate: (v) => {
        const s = v.trim();
        if (!s) return "Name is required.";
        if (s.length > 64) return "Max 64 characters.";
        if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(s)) {
          return "Lowercase a-z, 0-9, hyphens. No spaces or capitals.";
        }
        return null;
      },
    });
    if (raw === null) return; // user cancelled
    const name = raw.trim();
    if (!name) return;
    setCreating(true);
    try {
      const skillMdPath = await unwrap(commands.skillCreate(roost, name));
      // Open the new SKILL.md in a tab so the user can fill it in
      // immediately. The watcher will register the skill after first
      // save; the resource refetch catches up after that.
      await tabStore.openTab(skillMdPath);
      // Soft refresh — gives the user the new entry the moment they
      // come back to Settings.
      await refetch();
    } catch (e) {
      const msg = typeof e === "string" ? e : (e as Error)?.message ?? "Failed to create skill";
      setCreateError(msg);
    } finally {
      setCreating(false);
    }
  };

  return (
    <div class="space-y-3 border-t border-border pt-4">
      <div class="flex items-start justify-between gap-2">
        <div>
          <h4 class="text-text-secondary text-xs font-semibold uppercase tracking-wider">
            Skills
          </h4>
          <p class="text-text-muted text-[11px] mt-1">
            Markdown instruction packs the agent can load on demand via{" "}
            <code class="text-accent">skill_list</code> + {" "}
            <code class="text-accent">skill_load</code>. Drop a folder under{" "}
            <code class="text-accent">.corvux/skills/&lt;name&gt;/SKILL.md</code>{" "}
            inside your roost (or <code class="text-accent">~/.corvux/skills/</code>{" "}
            for global). Hot-reload watches both paths.
          </p>
        </div>
        <div class="flex items-center gap-3 shrink-0 mt-0.5">
          <button
            class="text-[11px] text-accent hover:underline disabled:opacity-50 disabled:cursor-not-allowed"
            onClick={handleCreate}
            disabled={creating() || !roostStore.roostPath()}
            title={
              roostStore.roostPath()
                ? "Scaffold a new SKILL.md and open it in a tab"
                : "Open a roost first to create skills"
            }
          >
            {creating() ? "Creating…" : "New skill…"}
          </button>
          <button
            class="text-[11px] text-accent hover:underline"
            onClick={handleRefresh}
            disabled={skills.loading}
          >
            Refresh
          </button>
        </div>
      </div>

      <Show when={createError()}>
        <div class="text-[11px] text-red px-1">{createError()}</div>
      </Show>

      <Show
        when={!skills.loading && (skills() ?? []).length > 0}
        fallback={
          <Show
            when={skills.loading}
            fallback={
              <div class="text-text-muted text-[11px] italic px-1">
                No skills installed. Drop a{" "}
                <code class="text-accent">SKILL.md</code> under{" "}
                <code class="text-accent">.corvux/skills/&lt;name&gt;/</code>{" "}
                in your roost to add one.
              </div>
            }
          >
            <div class="text-text-muted text-[11px] italic px-1">Loading…</div>
          </Show>
        }
      >
        <div class="space-y-1.5">
          <For each={skills() ?? []}>
            {(s) => (
              <div class="border border-border rounded px-2.5 py-1.5 bg-bg-secondary/40">
                <div class="flex items-baseline gap-2 mb-0.5">
                  <code class="text-xs font-mono text-text-primary">{s.name}</code>
                  <span
                    class={`text-[10px] px-1.5 py-0 rounded uppercase tracking-wide ${
                      s.source === "roost"
                        ? "bg-purple/15 text-purple"
                        : "bg-bg-hover text-text-muted"
                    }`}
                    title={
                      s.source === "roost"
                        ? "Loaded from <roost>/.corvux/skills/"
                        : "Loaded from ~/.corvux/skills/"
                    }
                  >
                    {s.source}
                  </span>
                </div>
                <div class="text-[11px] text-text-muted whitespace-pre-wrap leading-snug">
                  {s.description}
                </div>
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
