import { afterEach, describe, expect, it } from "vitest";
import {
  installNativeContextMenuGuard,
  isEditableTarget,
} from "../../src/lib/native-context-menu";

function el(html: string): Element {
  const host = document.createElement("div");
  host.innerHTML = html;
  return host.firstElementChild!;
}

describe("isEditableTarget", () => {
  it("allows the native menu in text inputs and textareas", () => {
    expect(isEditableTarget(el("<input />"))).toBe(true);
    expect(isEditableTarget(el("<textarea></textarea>"))).toBe(true);
  });

  it("allows it inside a contenteditable region", () => {
    const ce = el('<div contenteditable="true"><span>hi</span></div>');
    expect(isEditableTarget(ce)).toBe(true);
    // a descendant of the editable region also counts
    expect(isEditableTarget(ce.querySelector("span"))).toBe(true);
  });

  it("allows it inside the CodeMirror editor", () => {
    const cm = el('<div class="cm-editor"><div class="cm-content">x</div></div>');
    expect(isEditableTarget(cm.querySelector(".cm-content"))).toBe(true);
  });

  it("blocks it on non-editable elements", () => {
    expect(isEditableTarget(el("<div>read-only note body</div>"))).toBe(false);
    expect(isEditableTarget(el("<button>tab</button>"))).toBe(false);
  });

  it("blocks it for null / non-Element targets", () => {
    expect(isEditableTarget(null)).toBe(false);
    expect(isEditableTarget(document)).toBe(false);
  });
});

describe("installNativeContextMenuGuard", () => {
  let dispose: (() => void) | null = null;
  afterEach(() => {
    dispose?.();
    dispose = null;
  });

  function fireContextMenu(target: Element): MouseEvent {
    const ev = new MouseEvent("contextmenu", { bubbles: true, cancelable: true });
    target.dispatchEvent(ev);
    return ev;
  }

  it("prevents the default menu on non-editable targets", () => {
    dispose = installNativeContextMenuGuard(document);
    const node = el("<div>note body</div>");
    document.body.appendChild(node);
    const ev = fireContextMenu(node);
    expect(ev.defaultPrevented).toBe(true);
    node.remove();
  });

  it("leaves the native menu intact in editable targets", () => {
    dispose = installNativeContextMenuGuard(document);
    const input = el("<input />");
    document.body.appendChild(input);
    const ev = fireContextMenu(input);
    expect(ev.defaultPrevented).toBe(false);
    input.remove();
  });

  it("does not re-handle an event an app menu already prevented", () => {
    dispose = installNativeContextMenuGuard(document);
    const node = el("<div>file tree row</div>");
    document.body.appendChild(node);
    // Simulate an app-owned handler that ran first and already prevented.
    node.addEventListener("contextmenu", (e) => e.preventDefault(), { once: true });
    const ev = fireContextMenu(node);
    expect(ev.defaultPrevented).toBe(true); // still prevented, no double work
    node.remove();
  });

  it("stops preventing after the disposer runs", () => {
    const off = installNativeContextMenuGuard(document);
    off();
    const node = el("<div>note body</div>");
    document.body.appendChild(node);
    const ev = fireContextMenu(node);
    expect(ev.defaultPrevented).toBe(false);
    node.remove();
  });
});
