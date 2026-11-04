/**
 * CodeMirror 6 autocomplete for [[wiki-links]].
 *
 * Triggers when user types `[[` and shows a dropdown of all notes in the roost.
 * Fuzzy-filters as user types. Completing inserts `[[Note Name]]`.
 */

import {
  autocompletion,
  CompletionContext,
  CompletionResult,
  Completion,
} from "@codemirror/autocomplete";
import { EditorView } from "@codemirror/view";
import { roostStore } from "../../../stores/roost";

export interface NoteInfo {
  name: string;
  path: string;
}

/// Roost-relative, extension-stripped form of an absolute note path — the
/// disambiguating text the backend resolver matches on directly.
function relPath(absPath: string): string {
  const root = (roostStore.roostPath() ?? "").replace(/\\/g, "/").replace(/\/+$/, "");
  const norm = absPath.replace(/\\/g, "/").replace(/\.md$/i, "");
  return root && norm.startsWith(root + "/") ? norm.slice(root.length + 1) : norm;
}

/// LR-2: what to insert for a picked note. If its filename stem is unique in
/// the roost, insert the clean `[[Stem]]`. If several notes share the stem,
/// insert the folder-qualified `[[folder/Stem]]` so the link resolves to the
/// note the user actually picked — otherwise the backend resolver has to
/// guess by folder proximity and the autocomplete choice is silently thrown
/// away.
function insertTextFor(note: NoteInfo, stem: string, duplicateStems: Set<string>): string {
  return duplicateStems.has(stem.toLowerCase()) ? relPath(note.path) : stem;
}

/// Stems (lowercased, extension-stripped filenames) that more than one note
/// in the roost shares — the set that needs folder-qualified links.
function collidingStems(notes: NoteInfo[]): Set<string> {
  const seen = new Set<string>();
  const dup = new Set<string>();
  for (const n of notes) {
    const stem = n.name.replace(/\.md$/i, "").toLowerCase();
    if (seen.has(stem)) dup.add(stem);
    else seen.add(stem);
  }
  return dup;
}

/**
 * Simple fuzzy match scoring (same algorithm as QuickSwitcher).
 */
function fuzzyScore(query: string, target: string): number {
  const q = query.toLowerCase();
  const t = target.toLowerCase();

  if (t === q) return 200;
  if (t.startsWith(q)) return 100;
  if (t.includes(q)) return 50;

  // Subsequence match with consecutive bonus
  let qi = 0;
  let score = 0;
  let consecutive = 0;
  for (let ti = 0; ti < t.length && qi < q.length; ti++) {
    if (t[ti] === q[qi]) {
      qi++;
      consecutive++;
      score += consecutive * 2;
    } else {
      consecutive = 0;
    }
  }
  return qi === q.length ? score : -1;
}

/**
 * Create the wiki-link autocomplete source.
 * `getNotes` returns the current list of all notes in the roost.
 */
function wikilinkCompletions(getNotes: () => NoteInfo[]) {
  return (context: CompletionContext): CompletionResult | null => {
    // Look for [[ before cursor
    const line = context.state.doc.lineAt(context.pos);
    const textBefore = line.text.slice(0, context.pos - line.from);

    // Find the last [[ that hasn't been closed
    const lastOpen = textBefore.lastIndexOf("[[");
    if (lastOpen === -1) return null;

    // Check there's no ]] between [[ and cursor
    const afterBracket = textBefore.slice(lastOpen + 2);
    if (afterBracket.includes("]]")) return null;

    // The query is everything after [[
    const query = afterBracket.replace(/\|.*$/, ""); // ignore pipe display text
    const from = line.from + lastOpen + 2;

    const notes = getNotes();
    const duplicateStems = collidingStems(notes);
    let options: Completion[];

    if (query.length === 0) {
      // Show all notes when [[ is just typed
      options = notes.slice(0, 30).map((note) => {
        const stem = note.name.replace(/\.md$/i, "");
        const insertText = insertTextFor(note, stem, duplicateStems);
        return {
          label: stem,
          detail: note.path,
          type: "text",
          apply: (view: EditorView, _c: Completion, from: number, to: number) => {
            view.dispatch({ changes: { from, to, insert: insertText + "]]" } });
          },
        };
      });
    } else {
      // Fuzzy filter
      const scored = notes
        .map((note) => {
          const name = note.name.replace(/\.md$/i, "");
          const score = fuzzyScore(query, name);
          return { note, name, score };
        })
        .filter((s) => s.score > 0)
        .sort((a, b) => b.score - a.score)
        .slice(0, 20);

      options = scored.map(({ note, name }) => {
        const insertText = insertTextFor(note, name, duplicateStems);
        return {
          label: name,
          detail: note.path,
          type: "text",
          apply: (view: EditorView, _c: Completion, from: number, to: number) => {
            view.dispatch({ changes: { from, to, insert: insertText + "]]" } });
          },
        };
      });
    }

    if (options.length === 0) return null;

    return {
      from,
      options,
      filter: false, // We handle filtering ourselves
    };
  };
}

// Theme for the autocomplete dropdown — uses CSS variables for theme switching
const autocompleteTheme = EditorView.baseTheme({
  ".cm-tooltip-autocomplete": {
    backgroundColor: "var(--color-bg-primary) !important",
    border: "1px solid var(--color-border) !important",
    borderRadius: "6px",
    boxShadow: "0 4px 16px rgba(0, 0, 0, 0.3)",
  },
  ".cm-tooltip-autocomplete ul li": {
    padding: "4px 8px !important",
    color: "var(--color-text-primary)",
  },
  ".cm-tooltip-autocomplete ul li[aria-selected]": {
    backgroundColor: "var(--color-bg-hover) !important",
    color: "var(--color-accent)",
  },
  ".cm-completionLabel": {
    color: "var(--color-text-primary)",
  },
  ".cm-completionDetail": {
    color: "var(--color-text-muted)",
    fontStyle: "normal !important",
    fontSize: "0.85em",
    marginLeft: "8px",
  },
});

/**
 * Full wiki-link autocomplete extension.
 * `getNotes` should return the current roost file list.
 */
export function linkAutocomplete(getNotes: () => NoteInfo[]) {
  return [
    autocompletion({
      override: [wikilinkCompletions(getNotes)],
      activateOnTyping: true,
      maxRenderedOptions: 20,
    }),
    autocompleteTheme,
  ];
}
