import { For, Show, createSignal, createMemo } from "solid-js";
import { lsGet, lsSet } from "../../lib/storage";
import type { Frontmatter } from "../../lib/frontmatter";
import {
  getPropertyType,
  formatDate,
  setProperty,
  removeProperty,
  removeTagFromProperty,
  addTagToProperty,
} from "../../lib/frontmatter";
import { tabStore } from "../../stores/tabs";

interface PropertiesProps {
  frontmatter: Frontmatter;
}

/**
 * Tag hues cycle through --color-tag-1..7, defined per-theme in global.css.
 * Colour is picked by hashing the tag name so the same tag always gets the
 * same colour across notes (and adjacent tags look visibly different —
 * index-based cycling made `project/*` and `type/*` collide into one hue).
 */
const TAG_COLOR_COUNT = 7;
function getTagColorVar(tag: string): string {
  let hash = 0;
  for (let i = 0; i < tag.length; i++) {
    hash = (hash * 31 + tag.charCodeAt(i)) | 0;
  }
  return `var(--color-tag-${(Math.abs(hash) % TAG_COLOR_COUNT) + 1})`;
}

const PROPERTY_OPTIONS = [
  { key: "aliases", icon: "M13.828 10.172a4 4 0 00-5.656 0l-4 4a4 4 0 105.656 5.656l1.102-1.101", label: "aliases", defaultValue: [] as string[] },
  { key: "tags", icon: "M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A2 2 0 013 12V7a4 4 0 014-4z", label: "tags", defaultValue: [] as string[] },
  { key: "date", icon: "M8 7V3m8 4V3m-9 8h10M5 21h14a2 2 0 002-2V7a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z", label: "date", defaultValue: new Date().toISOString().split("T")[0] },
  { key: "status", icon: "M9 12l2 2 4-4m6 2a9 9 0 11-18 0 9 9 0 0118 0z", label: "status", defaultValue: "draft" },
  { key: "priority", icon: "M3 21l1.65-3.8a9 9 0 1112.712-12.712l.088.088A9 9 0 013 21z", label: "priority", defaultValue: "medium" },
  { key: "category", icon: "M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A2 2 0 013 12V7a4 4 0 014-4z", label: "category", defaultValue: "" },
  { key: "project", icon: "M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z", label: "project", defaultValue: "" },
  { key: "author", icon: "M16 7a4 4 0 11-8 0 4 4 0 018 0zM12 14a7 7 0 00-7 7h14a7 7 0 00-7-7z", label: "author", defaultValue: "" },
  { key: "source", icon: "M10 6H6a2 2 0 00-2 2v10a2 2 0 002 2h10a2 2 0 002-2v-4M14 4h6m0 0v6m0-6L10 14", label: "source", defaultValue: "" },
  { key: "rating", icon: "M11.049 2.927c.3-.921 1.603-.921 1.902 0l1.519 4.674a1 1 0 00.95.69h4.915c.969 0 1.371 1.24.588 1.81l-3.976 2.888a1 1 0 00-.363 1.118l1.518 4.674c.3.922-.755 1.688-1.538 1.118l-3.976-2.888a1 1 0 00-1.176 0l-3.976 2.888c-.783.57-1.838-.197-1.538-1.118l1.518-4.674a1 1 0 00-.363-1.118l-3.976-2.888c-.784-.57-.38-1.81.588-1.81h4.914a1 1 0 00.951-.69l1.519-4.674z", label: "rating", defaultValue: 0 },
  { key: "cssclasses", icon: "M4 6h16M4 12h16M4 18h7", label: "cssclasses", defaultValue: [] as string[] },
];

const ICONS: Record<string, string> = {
  date: "M8 7V3m8 4V3m-9 8h10M5 21h14a2 2 0 002-2V7a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z",
  type: "M4 6h16M4 12h16M4 18h7",
  tags: "M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A2 2 0 013 12V7a4 4 0 014-4z",
  updated: "M8 7V3m8 4V3m-9 8h10M5 21h14a2 2 0 002-2V7a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z",
  created: "M8 7V3m8 4V3m-9 8h10M5 21h14a2 2 0 002-2V7a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z",
  status: "M9 12l2 2 4-4m6 2a9 9 0 11-18 0 9 9 0 0118 0z",
  priority: "M3 21l1.65-3.8a9 9 0 1112.712-12.712l.088.088A9 9 0 013 21z",
  aliases: "M13.828 10.172a4 4 0 00-5.656 0l-4 4a4 4 0 105.656 5.656l1.102-1.101",
  category: "M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A2 2 0 013 12V7a4 4 0 014-4z",
  project: "M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z",
  author: "M16 7a4 4 0 11-8 0 4 4 0 018 0zM12 14a7 7 0 00-7 7h14a7 7 0 00-7-7z",
  default: "M4 6h16M4 12h16M4 18h7",
};

function getIcon(key: string): string {
  return ICONS[key] || ICONS.default;
}

/** Keys that are always treated as long-form text (textarea edit). */
const LONG_TEXT_KEYS = new Set(["description", "summary", "notes", "abstract", "content"]);

function isLongText(key: string, value: unknown): boolean {
  if (LONG_TEXT_KEYS.has(key)) return true;
  const s = String(value ?? "");
  return s.length > 80 || s.includes("\n");
}

/** Apply a frontmatter edit to the active tab's content */
function applyEdit(editFn: (content: string) => string) {
  const content = tabStore.activeContent();
  const updated = editFn(content);
  if (updated !== content) {
    tabStore.updateContent(updated);
  }
}

export default function Properties(props: PropertiesProps) {
  // Collapsed-by-default. The user is here to read the note, not edit
  // metadata — gives the title + body the top of the page. We persist
  // the *user's* preference (open/closed) in localStorage so power
  // users who do want to see properties at a glance get them
  // remembered. Per-note state would be over-engineering for this pass.
  const initialCollapsed = (() => {
    if (typeof localStorage === "undefined") return true;
    try {
      const v = lsGet("corvux:propertiesCollapsed");
      // null (never set) → collapsed; "0" → open; "1" → collapsed.
      return v === null ? true : v === "1";
    } catch {
      return true;
    }
  })();
  const [collapsed, setCollapsedSignal] = createSignal(initialCollapsed);
  function setCollapsed(next: boolean) {
    setCollapsedSignal(next);
    if (typeof localStorage !== "undefined") {
      try {
        lsSet("corvux:propertiesCollapsed", next ? "1" : "0");
      } catch {
        // Quota / private mode — non-fatal, runtime signal still updates.
      }
    }
  }

  const [showAddMenu, setShowAddMenu] = createSignal(false);
  const [filterText, setFilterText] = createSignal("");
  const entries = createMemo(() => Object.entries(props.frontmatter));

  const existingKeys = createMemo(() => new Set(Object.keys(props.frontmatter)));

  const filteredOptions = createMemo(() => {
    const filter = filterText().toLowerCase();
    return PROPERTY_OPTIONS.filter(
      (opt) => !existingKeys().has(opt.key) && opt.label.includes(filter)
    );
  });

  function handleAddProperty(opt: (typeof PROPERTY_OPTIONS)[number]) {
    applyEdit((content) => setProperty(content, opt.key, opt.defaultValue));
    setShowAddMenu(false);
    setFilterText("");
  }

  function handleRemoveProperty(key: string) {
    applyEdit((content) => removeProperty(content, key));
  }

  return (
    <div class="content-container pt-4 sm:pt-6 pb-2">
      {/* Header acts as a disclosure toggle. Collapsed-by-default so the
          note title + body land at the top of the page instead of being
          pushed down by a metadata form. The summary line gives a glance
          at what's hidden (count + first 2 tags) so collapsing doesn't
          hide information the user needs to see at a glance. */}
      <button
        type="button"
        onClick={() => setCollapsed(!collapsed())}
        class="group flex items-center gap-2 mb-2 sm:mb-3 px-1 w-full text-left"
        aria-expanded={!collapsed()}
      >
        <svg
          width="12"
          height="12"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2.5"
          stroke-linecap="round"
          stroke-linejoin="round"
          class={`text-text-muted shrink-0 transition-transform ${
            collapsed() ? "" : "rotate-90"
          }`}
          aria-hidden="true"
        >
          <path d="M9 18l6-6-6-6" />
        </svg>
        <h3 class="text-text-primary font-semibold text-[14px] sm:text-[15px]">
          Properties
        </h3>
        <Show when={collapsed() && entries().length > 0}>
          <span class="text-text-muted text-xs font-normal truncate">
            · {entries().length} field{entries().length === 1 ? "" : "s"}
          </span>
        </Show>
      </button>

      <Show when={!collapsed()}>
        {/* Density audit 2026-07-02: a 9-property note kept open filled a
            full first screen (~43px/row incl. spacing). space-y-3 → 1.5 and
            slimmer container padding buys ~25% height back without touching
            the 32px row min-height (touch targets unchanged). */}
        <div class="rounded-xl bg-bg-secondary/50 border border-border/50 px-3 sm:px-4 py-2 sm:py-3 shadow-sm">

          <div class="space-y-1.5">
            <For each={entries()}>
              {([key, value]) => (
                <PropertyRow
                  key={key}
                  value={value}
                  onRemove={() => handleRemoveProperty(key)}
                />
              )}
            </For>
          </div>

          {/* Add property */}
          <div class="relative mt-2 pt-2 border-t border-border/30">
            <button
              class="flex items-center gap-2 text-text-muted hover:text-text-secondary text-sm transition-colors"
              onClick={() => setShowAddMenu(!showAddMenu())}
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M12 5v14M5 12h14" />
              </svg>
              Add property
            </button>

            <Show when={showAddMenu()}>
              <div class="absolute left-0 top-full mt-1 w-56 bg-bg-secondary border border-border rounded-lg shadow-xl z-50 py-1 max-h-64 overflow-y-auto">
                <div class="px-2 pb-1 pt-1">
                  <input
                    type="text"
                    placeholder="Filter..."
                    value={filterText()}
                    onInput={(e) => setFilterText(e.currentTarget.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Escape") {
                        setShowAddMenu(false);
                        setFilterText("");
                      }
                    }}
                    class="w-full px-2 py-1 rounded bg-bg-tertiary border border-border/50 text-text-primary text-sm focus:outline-none focus:border-accent"
                    autofocus
                  />
                </div>
                <For each={filteredOptions()}>
                  {(opt) => (
                    <button
                      class="w-full flex items-center gap-2.5 px-3 py-1.5 text-sm text-text-secondary hover:bg-bg-hover hover:text-text-primary transition-colors"
                      onClick={() => handleAddProperty(opt)}
                    >
                      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" class="text-text-muted shrink-0">
                        <path d={opt.icon} />
                      </svg>
                      {opt.label}
                    </button>
                  )}
                </For>
                <Show when={filteredOptions().length === 0}>
                  <div class="px-3 py-2 text-text-muted text-sm">No matching properties</div>
                </Show>
              </div>
            </Show>
          </div>
        </div>
      </Show>
    </div>
  );
}

function PropertyRow(props: { key: string; value: unknown; onRemove: () => void }) {
  const [editing, setEditing] = createSignal(false);
  const [editValue, setEditValue] = createSignal("");
  const type = createMemo(() => getPropertyType(props.key, props.value));

  function startEditing() {
    setEditValue(String(props.value ?? ""));
    setEditing(true);
  }

  function commitEdit() {
    const val = editValue().trim();
    if (val !== String(props.value ?? "")) {
      let typedVal: string | number | boolean = val;
      if (val === "true") typedVal = true;
      else if (val === "false") typedVal = false;
      else if (/^\d+$/.test(val)) typedVal = parseInt(val, 10);
      else if (/^\d+\.\d+$/.test(val)) typedVal = parseFloat(val);
      applyEdit((content) => setProperty(content, props.key, typedVal));
    }
    setEditing(false);
  }

  return (
    <div class="group flex items-start gap-2 sm:gap-4 min-h-[32px]">
      {/* Icon + Key — narrower label column on mobile so values/tags get more room */}
      <div class="flex items-center gap-2 sm:gap-2.5 w-[96px] sm:w-[130px] shrink-0 pt-0.5">
        <svg
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="1.5"
          class="text-text-muted shrink-0"
        >
          <path d={getIcon(props.key)} />
        </svg>
        <span class="text-text-secondary text-sm truncate">{props.key}</span>
      </div>

      {/* Value */}
      <div class="flex-1 flex items-center flex-wrap gap-1.5 min-h-[28px]">
        <Show when={type() === "tags" || type() === "list"}>
          <TagList
            propKey={props.key}
            values={Array.isArray(props.value) ? (props.value as string[]) : []}
          />
        </Show>
        <Show when={type() === "date"}>
          <Show
            when={!editing()}
            fallback={
              <input
                type="date"
                value={editValue()}
                onInput={(e) => setEditValue(e.currentTarget.value)}
                onBlur={commitEdit}
                onKeyDown={(e) => {
                  if (e.key === "Enter") commitEdit();
                  if (e.key === "Escape") setEditing(false);
                }}
                class="bg-bg-tertiary border border-border rounded px-2 py-0.5 text-text-primary text-sm focus:outline-none focus:border-accent"
                autofocus
              />
            }
          >
            <div
              class="inline-flex items-center gap-1.5 cursor-pointer hover:bg-bg-hover rounded px-1 -mx-1 group/date"
              onClick={() => {
                setEditValue(String(props.value ?? ""));
                setEditing(true);
              }}
              title="Edit date"
            >
              <span class="text-text-primary text-sm">{formatDate(String(props.value))}</span>
              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" class="text-text-muted opacity-60 group-hover/date:opacity-100 transition-opacity">
                <path d="M6 9l6 6 6-6" />
              </svg>
              <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" class="text-text-muted opacity-60 group-hover/date:opacity-100 transition-opacity">
                <path d="M10 13a5 5 0 007.54.54l3-3a5 5 0 00-7.07-7.07l-1.72 1.71" />
                <path d="M14 11a5 5 0 00-7.54-.54l-3 3a5 5 0 007.07 7.07l1.71-1.71" />
              </svg>
            </div>
          </Show>
        </Show>
        <Show when={type() === "text" || type() === "number"}>
          <Show
            when={!editing()}
            fallback={
              <Show
                when={isLongText(props.key, props.value)}
                fallback={
                  <input
                    type="text"
                    value={editValue()}
                    onInput={(e) => setEditValue(e.currentTarget.value)}
                    onBlur={commitEdit}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") commitEdit();
                      if (e.key === "Escape") setEditing(false);
                    }}
                    class="flex-1 bg-bg-tertiary border border-border rounded px-2 py-0.5 text-text-primary text-sm focus:outline-none focus:border-accent"
                    autofocus
                  />
                }
              >
                <textarea
                  rows={5}
                  value={editValue()}
                  onInput={(e) => setEditValue(e.currentTarget.value)}
                  onBlur={commitEdit}
                  onKeyDown={(e) => {
                    // Shift+Enter → newline, plain Enter → commit (mirrors most editors)
                    if (e.key === "Enter" && !e.shiftKey) {
                      e.preventDefault();
                      commitEdit();
                    }
                    if (e.key === "Escape") setEditing(false);
                  }}
                  class="flex-1 w-full bg-bg-tertiary border border-border rounded px-2 py-1.5 text-text-primary text-sm focus:outline-none focus:border-accent resize-y leading-relaxed"
                  autofocus
                />
              </Show>
            }
          >
            <span
              class="text-text-primary text-sm font-medium cursor-pointer hover:bg-bg-hover rounded px-1 -mx-1 min-w-0 prop-value-clamp"
              onClick={startEditing}
              title={String(props.value)}
            >
              {String(props.value) || <span class="text-text-muted italic">empty</span>}
            </span>
          </Show>
        </Show>
        <Show when={type() === "boolean"}>
          <button
            class={`text-sm px-2 py-0.5 rounded ${
              props.value ? "text-green bg-green/10" : "text-red bg-red/10"
            } hover:opacity-80 transition-opacity`}
            onClick={() =>
              applyEdit((content) => setProperty(content, props.key, !props.value))
            }
          >
            {String(props.value)}
          </button>
        </Show>
      </div>

      {/* Delete property button */}
      <button
        class="opacity-0 group-hover:opacity-60 hover:!opacity-100 text-text-muted hover:text-red p-0.5 rounded transition-all shrink-0"
        onClick={props.onRemove}
        title={`Remove ${props.key}`}
      >
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <path d="M18 6L6 18M6 6l12 12" />
        </svg>
      </button>
    </div>
  );
}

// Obsidian tag rule (mirrors the Rust parser in `parser/tags.rs::push_tag`):
// only letters, digits, `-`, `_`, `/`. Dots, spaces, ampersands etc. silently
// drop the tag from the graph indexer — so we reject them at input time
// here so users never end up with a chip that looks saved but isn't connected.
// Also strips a leading `#` if the user types one out of muscle memory.
function sanitizeTagInput(raw: string): string {
  let s = raw.replace(/^#+/, "");
  s = s.replace(/[^a-zA-Z0-9\-_/]/g, "");
  return s;
}

// Purely-numeric tags (#1, #2024) match list markers / issue refs in Obsidian
// and are deliberately not treated as tags. Reject here so the chip doesn't
// look saved while silently never appearing in the graph.
function isValidTag(tag: string): boolean {
  if (tag.length === 0) return false;
  return /[^0-9]/.test(tag);
}

function TagList(props: { propKey: string; values: string[] }) {
  const [addingTag, setAddingTag] = createSignal(false);
  const [newTag, setNewTag] = createSignal("");

  function handleAddTag() {
    const tag = newTag().trim();
    if (tag && isValidTag(tag)) {
      applyEdit((content) => addTagToProperty(content, props.propKey, tag));
      setNewTag("");
      setAddingTag(false);
    }
  }

  function handleRemoveTag(tag: string) {
    applyEdit((content) => removeTagFromProperty(content, props.propKey, tag));
  }

  return (
    <>
      <For each={props.values}>
        {(tag) => {
          const hue = getTagColorVar(tag);
          // `group/tag` scopes the hover so each chip controls its
          // own × visibility — independently from any ancestor
          // group:hover relationships (PropertyRow uses `group`).
          return (
            <span
              class="group/tag inline-flex items-center gap-1.5 px-3 py-1 rounded-full text-[13px] font-medium"
              style={{
                "background-color": `color-mix(in srgb, ${hue} 15%, transparent)`,
                color: hue,
                border: `1px solid color-mix(in srgb, ${hue} 30%, transparent)`,
              }}
            >
              {tag}
              <svg
                width="13"
                height="13"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="2.5"
                class="cursor-pointer opacity-0 group-hover/tag:opacity-70 hover:!opacity-100 transition-opacity"
                onClick={() => handleRemoveTag(tag)}
                aria-label={`Remove tag ${tag}`}
              >
                <path d="M18 6L6 18M6 6l12 12" />
              </svg>
            </span>
          );
        }}
      </For>

      <Show
        when={addingTag()}
        fallback={
          <button
            class="inline-flex items-center gap-1 px-2 py-1 rounded-full text-[12px] text-text-muted hover:text-text-secondary border border-border/50 hover:border-border transition-colors"
            onClick={() => setAddingTag(true)}
          >
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
              <path d="M12 5v14M5 12h14" />
            </svg>
            add
          </button>
        }
      >
        <input
          type="text"
          placeholder="tag name..."
          value={newTag()}
          onInput={(e) => {
            // Strip disallowed chars as the user types — keeps the chip
            // value in lockstep with what the graph indexer will accept,
            // so the user can never produce a "looks saved but invisible
            // in the graph" tag like `phase-2.1`.
            const sanitized = sanitizeTagInput(e.currentTarget.value);
            if (sanitized !== e.currentTarget.value) {
              e.currentTarget.value = sanitized;
            }
            setNewTag(sanitized);
          }}
          onPaste={(e) => {
            // Same sanitiser applies to paste so users can't bypass the
            // keystroke filter by pasting `phase-2.1`. We preventDefault
            // and insert the cleaned text manually.
            e.preventDefault();
            const text = e.clipboardData?.getData("text") ?? "";
            const sanitized = sanitizeTagInput(text);
            const input = e.currentTarget;
            const start = input.selectionStart ?? input.value.length;
            const end = input.selectionEnd ?? input.value.length;
            const next = input.value.slice(0, start) + sanitized + input.value.slice(end);
            input.value = next;
            setNewTag(next);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") handleAddTag();
            if (e.key === "Escape") {
              setAddingTag(false);
              setNewTag("");
            }
          }}
          onBlur={() => {
            if (newTag().trim()) handleAddTag();
            else {
              setAddingTag(false);
              setNewTag("");
            }
          }}
          class="px-2 py-0.5 rounded bg-bg-tertiary border border-border text-text-primary text-xs focus:outline-none focus:border-accent w-24"
          autofocus
        />
      </Show>
    </>
  );
}
