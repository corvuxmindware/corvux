/**
 * DOM/pointer wiring for in-place mermaid pinch-zoom. The geometry lives in
 * mermaid-zoom.ts (pure + tested); this file is the imperative glue that
 * binds it to a viewport element and an inner pan element.
 *
 * Gesture contract (the bit that matters for not trapping note-scroll):
 *  - At fit (scale === 1): viewport touch-action = "pan-y". One finger
 *    scrolls the note as usual; only a TWO-finger pinch zooms. Nothing is
 *    captured, so the page scrolls right through the diagram.
 *  - When zoomed (scale > 1): touch-action = "none". One finger pans the
 *    diagram, two fingers pinch. A floating "fit" button snaps back to scale
 *    1, which re-enables scroll-through.
 *  - Desktop: ⌘/Ctrl + wheel zooms toward the cursor; plain wheel scrolls.
 *    Dragging with the mouse pans while zoomed.
 *
 * The transform is visual-only (CSS), so CodeMirror's height map is never
 * disturbed — clicks below the diagram stay aligned regardless of zoom.
 */

import {
  clampPan,
  computePinch,
  distance,
  IDENTITY,
  isZoomed,
  zoomAt,
  type PinchStart,
  type Transform,
} from "./mermaid-zoom";

interface PointerPos {
  x: number;
  y: number;
}

export function attachPinchZoom(
  viewport: HTMLElement,
  pan: HTMLElement,
): () => void {
  const ac = new AbortController();
  const { signal } = ac;

  let t: Transform = { ...IDENTITY };
  const pointers = new Map<number, PointerPos>();
  let pinch: PinchStart | null = null;
  let panLast: PointerPos | null = null; // single-pointer pan baseline

  // will-change toggling. A *static* will-change:transform (it used to live in
  // the CSS theme) pins the compositor's raster scale at 1×, so zooming in
  // up-scales a 1× bitmap and the diagram blurs. Instead we flip it ON while a
  // gesture is live (keeps the pinch composited + smooth) and drop it shortly
  // after the transform settles, so Chromium re-rasterizes the SVG vectors at
  // the final scale → crisp. See the .cm-mermaid-pan comment in
  // mermaid-render.ts.
  let settleTimer: ReturnType<typeof setTimeout> | undefined;
  function markActive() {
    pan.style.willChange = "transform";
    if (settleTimer) clearTimeout(settleTimer);
    settleTimer = setTimeout(() => {
      pan.style.willChange = "auto";
    }, 200);
  }

  // Floating "reset to fit" affordance — only visible while zoomed. Tapping
  // it is the escape hatch back to scroll-through. stopPropagation keeps the
  // tap from bubbling into CodeMirror's double-click-to-edit.
  const fit = document.createElement("button");
  fit.className = "cm-mermaid-fit";
  fit.type = "button";
  fit.setAttribute("aria-label", "Reset zoom");
  fit.title = "Reset zoom";
  fit.textContent = "Fit";
  fit.style.display = "none";
  fit.addEventListener(
    "click",
    (e) => {
      e.stopPropagation();
      e.preventDefault();
      reset();
    },
    { signal },
  );
  viewport.appendChild(fit);

  function boxCoords(clientX: number, clientY: number): PointerPos {
    const r = viewport.getBoundingClientRect();
    return { x: clientX - r.left, y: clientY - r.top };
  }

  function apply() {
    t = clampPan(t, viewport.clientWidth, viewport.clientHeight);
    pan.style.transform = `translate(${t.tx}px, ${t.ty}px) scale(${t.scale})`;
    markActive();
    const zoomed = isZoomed(t);
    viewport.style.touchAction = zoomed ? "none" : "pan-y";
    viewport.style.cursor = zoomed ? "grab" : "";
    fit.style.display = zoomed ? "" : "none";
  }

  function reset() {
    t = { ...IDENTITY };
    pinch = null;
    panLast = null;
    apply();
  }

  function twoPointerState(): { dist: number; cx: number; cy: number } | null {
    if (pointers.size < 2) return null;
    const [a, b] = [...pointers.values()];
    return {
      dist: distance(a.x, a.y, b.x, b.y),
      cx: (a.x + b.x) / 2,
      cy: (a.y + b.y) / 2,
    };
  }

  function beginPinch() {
    const tp = twoPointerState();
    if (!tp) return;
    pinch = {
      scale0: t.scale,
      tx0: t.tx,
      ty0: t.ty,
      dist0: tp.dist || 1,
      fx0: tp.cx,
      fy0: tp.cy,
    };
    panLast = null;
  }

  function onPointerDown(e: PointerEvent) {
    const isTouch = e.pointerType === "touch" || e.pointerType === "pen";
    const p = boxCoords(e.clientX, e.clientY);

    if (isTouch) {
      pointers.set(e.pointerId, p);
      if (pointers.size === 2) {
        beginPinch();
        viewport.setPointerCapture(e.pointerId);
        e.preventDefault();
      } else if (pointers.size === 1 && isZoomed(t)) {
        // Single-finger pan only makes sense once zoomed; at fit we leave the
        // touch alone so the note scrolls.
        panLast = p;
        viewport.setPointerCapture(e.pointerId);
        e.preventDefault();
      }
      return;
    }

    // Mouse: drag-to-pan only while zoomed (left button).
    if (e.button === 0 && isZoomed(t)) {
      panLast = p;
      viewport.setPointerCapture(e.pointerId);
      viewport.style.cursor = "grabbing";
      e.preventDefault();
    }
  }

  function onPointerMove(e: PointerEvent) {
    const isTouch = e.pointerType === "touch" || e.pointerType === "pen";
    const p = boxCoords(e.clientX, e.clientY);
    if (isTouch && pointers.has(e.pointerId)) pointers.set(e.pointerId, p);

    if (pinch && pointers.size >= 2) {
      const tp = twoPointerState();
      if (tp) {
        t = computePinch(pinch, tp.dist, tp.cx, tp.cy);
        apply();
        e.preventDefault();
      }
      return;
    }

    if (panLast) {
      t = { scale: t.scale, tx: t.tx + (p.x - panLast.x), ty: t.ty + (p.y - panLast.y) };
      panLast = p;
      apply();
      e.preventDefault();
    }
  }

  function endPointer(e: PointerEvent) {
    pointers.delete(e.pointerId);
    if (viewport.hasPointerCapture?.(e.pointerId)) {
      viewport.releasePointerCapture(e.pointerId);
    }
    if (pinch && pointers.size < 2) {
      pinch = null;
      // A finger lifted mid-pinch — re-anchor the survivor for pan so it
      // doesn't jump.
      const [survivor] = pointers.size === 1 ? [...pointers.values()] : [null];
      panLast = survivor && isZoomed(t) ? survivor : null;
    }
    if (pointers.size === 0) {
      panLast = null;
      if (viewport.style.cursor === "grabbing") {
        viewport.style.cursor = isZoomed(t) ? "grab" : "";
      }
    }
  }

  function onWheel(e: WheelEvent) {
    // Plain wheel scrolls the note; only ⌘/Ctrl + wheel zooms (standard
    // zoom-modifier convention, avoids trapping page scroll on hover).
    if (!e.ctrlKey && !e.metaKey) return;
    e.preventDefault();
    const p = boxCoords(e.clientX, e.clientY);
    const factor = e.deltaY < 0 ? 1.1 : 1 / 1.1;
    t = zoomAt(t, factor, p.x, p.y);
    apply();
  }

  viewport.addEventListener("pointerdown", onPointerDown, { signal });
  viewport.addEventListener("pointermove", onPointerMove, { signal });
  viewport.addEventListener("pointerup", endPointer, { signal });
  viewport.addEventListener("pointercancel", endPointer, { signal });
  viewport.addEventListener("wheel", onWheel, { passive: false, signal });

  return () => {
    ac.abort();
    if (settleTimer) clearTimeout(settleTimer);
    fit.remove();
  };
}
