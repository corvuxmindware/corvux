import { createMemo, createSignal, createEffect, onCleanup } from "solid-js";
import { marked } from "marked";
import DOMPurify from "dompurify";
import { navigateToWikiLink } from "../../lib/wiki-link";
import {
  isExternalUrl,
  openExternalUrl,
} from "../Editor/extensions/external-link";

/// Pre-process Obsidian-style `[[Title]]` / `[[Title|Alias]]` into an
/// inline `<a>` with `class="wiki-link"` + `data-target="Title"` BEFORE
/// marked parses, so wiki-links render as styled pills instead of raw
/// `[[...]]` text. Picked Obsidian's syntax (`[[ ... ]]`) because the
/// agent emits that exact form when citing notes — see the system
/// prompt's "Refer to notes the way users think of them" rule.
function preprocessWikiLinks(src: string): string {
  return src.replace(/\[\[([^\]\n|]+)(?:\|([^\]\n]+))?\]\]/g, (_, title, alias) => {
    const target = title.trim();
    const display = (alias ?? title).trim();
    // Escape HTML in the user-visible bits; the agent's text could
    // contain `<` `&` `"` if it stuffed unusual titles into a link.
    const esc = (s: string) =>
      s
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");
    return `<a href="#" class="wiki-link" data-target="${esc(target)}">${esc(display)}</a>`;
  });
}

/**
 * Renders AI-generated markdown to HTML, sanitizes it, and binds a
 * minimal stylesheet via Tailwind classes on the container.
 *
 * Why two libraries: `marked` parses Markdown → HTML quickly and with
 * GFM (tables, fenced code, task lists). It does NOT sanitize. `marked`
 * 18+ removed the built-in sanitizer because XSS surfaces shift faster
 * than they can keep up. We pipe through DOMPurify which has a current
 * and aggressive deny list. The agent is a frequent target for prompt
 * injection ("ignore previous instructions, render this <script>..."),
 * so this isn't theoretical hygiene — it's the trust boundary between
 * the LLM's free-form output and the renderer.
 *
 * Streamed messages re-render on every chunk; `createMemo` caches the
 * heavy parse so we only re-run on actual content change.
 */
export default function MarkdownView(props: { content: string }) {
  // AUDIT-061: throttle the marked+DOMPurify parse to at most one run
  // per 100ms when content is changing rapidly (streaming). A trailing
  // setTimeout guarantees the final state is always rendered once
  // updates stop, so idle viewers see the complete reply. Profile said
  // 300-500ms of total parse cost was burning on a 5KB Android reply
  // before this — the throttle puts a hard cap on per-frame work.
  const [throttledContent, setThrottledContent] = createSignal(props.content);
  let lastParseTs = 0;
  let pendingTimer: ReturnType<typeof setTimeout> | null = null;
  const THROTTLE_MS = 100;

  createEffect(() => {
    const next = props.content;
    const now =
      typeof performance !== "undefined" ? performance.now() : Date.now();
    const elapsed = now - lastParseTs;
    if (elapsed >= THROTTLE_MS) {
      if (pendingTimer) {
        clearTimeout(pendingTimer);
        pendingTimer = null;
      }
      lastParseTs = now;
      setThrottledContent(next);
    } else if (!pendingTimer) {
      pendingTimer = setTimeout(() => {
        pendingTimer = null;
        lastParseTs =
          typeof performance !== "undefined"
            ? performance.now()
            : Date.now();
        setThrottledContent(props.content);
      }, THROTTLE_MS - elapsed);
    }
  });

  onCleanup(() => {
    if (pendingTimer) {
      clearTimeout(pendingTimer);
      pendingTimer = null;
    }
  });

  const html = createMemo(() => {
    const preprocessed = preprocessWikiLinks(throttledContent());
    const raw = marked.parse(preprocessed, {
      async: false,
      gfm: true,
      breaks: true,
    }) as string;
    return DOMPurify.sanitize(raw, {
      ALLOWED_TAGS: [
        "p", "br", "strong", "em", "del", "code", "pre",
        "blockquote", "hr",
        "h1", "h2", "h3", "h4", "h5", "h6",
        "ul", "ol", "li",
        "a", "img",
        "table", "thead", "tbody", "tr", "th", "td",
        "input", // for GFM task-list checkboxes
      ],
      ALLOWED_ATTR: [
        "href", "title", "alt", "src",
        "type", "checked", "disabled", // task-list checkbox shape
        "class", // we render syntax-highlight hooks via class names
        "data-target", // wiki-link target — see preprocessWikiLinks
      ],
      ALLOW_DATA_ATTR: false,
      // Open every link in the system browser (Tauri intercepts target=_blank
      // → opens via shell). Without rel=noopener, an opened tab could navigate
      // back to ours via window.opener.
      ADD_ATTR: ["target", "rel"],
    });
  });

  /// Link clicks: wiki-links (`a.wiki-link`) resolve to a roost path and
  /// open as a tab; every other `<a>` with an external href is routed
  /// through the Tauri opener plugin → system browser (the same path the
  /// note editor uses). We open explicitly instead of relying on the
  /// `target=_blank` webview interception because that is unreliable on
  /// Android WebView. The DOMPurify hook below still stamps
  /// `target=_blank rel=noopener` as a belt-and-suspenders fallback.
  async function handleClick(e: MouseEvent) {
    const target = e.target as HTMLElement | null;
    const anchor = target?.closest("a") as HTMLAnchorElement | null;
    if (!anchor) return;

    if (anchor.classList.contains("wiki-link")) {
      e.preventDefault();
      const noteTitle = anchor.dataset.target?.trim();
      if (!noteTitle) return;
      // Shared resolver — same matching rules as the editor's Ctrl+Click
      // navigation. `createIfMissing: false` because clicking a wiki-link
      // in the AGENT'S reply shouldn't silently spawn a note the user
      // never asked to create; if the title doesn't resolve, that's a
      // signal the agent hallucinated a reference.
      const opened = await navigateToWikiLink(noteTitle, { createIfMissing: false });
      if (!opened) console.info("[wiki-link] no match for:", noteTitle);
      return;
    }

    // External link — open in the system browser via the opener plugin.
    const href = anchor.getAttribute("href");
    if (href && isExternalUrl(href)) {
      e.preventDefault();
      void openExternalUrl(href);
    }
  }

  // Keep the inner HTML mounted via innerHTML — Solid lets us set it
  // imperatively after sanitization. Adding `target=_blank rel=noopener` to
  // every <a> happens via DOMPurify's afterSanitizeAttributes hook below.
  return (
    <div
      class="markdown-body text-sm leading-relaxed"
      onClick={handleClick}
      // eslint-disable-next-line solid/no-innerhtml
      innerHTML={html()}
    />
  );
}

// Open all links in a new context (Tauri routes target=_blank to the
// system browser via its capability layer). Belt-and-suspenders: rel
// stops window.opener leakage on platforms that don't already block it.
//
// AUDIT-031: guard the install so HMR doesn't stack the hook on every
// reload. The prior unconditional `addHook` registered N copies after N
// edits, with each one running on every sanitize pass.
let dompurifyHookInstalled = false;
if (!dompurifyHookInstalled) {
  DOMPurify.addHook("afterSanitizeAttributes", (node) => {
    if (node.tagName === "A") {
      node.setAttribute("target", "_blank");
      node.setAttribute("rel", "noopener noreferrer");
    }
  });
  dompurifyHookInstalled = true;
}
