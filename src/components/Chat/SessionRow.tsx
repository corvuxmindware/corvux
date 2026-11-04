import { Show, createSignal, type JSX } from "solid-js";
import type { ChatSession } from "../../stores/chat";
import { isMobile as isMobilePlatform } from "../../lib/platform";

/** A single row in the sessions list — title, preview, timestamp, hover
 *  actions (pin / rename / save-as-note / delete) on desktop, plus a
 *  swipe-to-action gesture on mobile (iOS / Android):
 *    • swipe LEFT (content slides left)  → reveals Delete on the right
 *    • swipe RIGHT (content slides right) → reveals Pin/Unpin on the left
 *  Tapping a revealed action button fires it and snaps the row closed.
 *  Tapping anywhere else on a half-open row also snaps it closed without
 *  triggering the row's onSwitch — protects against accidental dismissal.
 *
 *  All state is local; parent owns the session data + callbacks. */
export default function SessionRow(props: {
  session: ChatSession;
  isActive: boolean;
  onSwitch: () => void;
  onRename: (title: string) => void;
  onDelete: () => void;
  onTogglePin: () => void;
  onSaveAsNote: () => void;
}) {
  const [isEditing, setIsEditing] = createSignal(false);
  const [editValue, setEditValue] = createSignal(props.session.title);
  const [confirmDelete, setConfirmDelete] = createSignal(false);

  function commitRename(e?: Event) {
    e?.preventDefault();
    const v = editValue().trim();
    if (v && v !== props.session.title) {
      props.onRename(v);
    }
    setIsEditing(false);
  }

  /** Friendly relative time so the user can scan the list quickly. */
  function fmtTime(ts: number): string {
    if (!ts) return "";
    const d = new Date(ts);
    const now = new Date();
    const sameDay = d.toDateString() === now.toDateString();
    if (sameDay) return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
    const yesterday = new Date(now); yesterday.setDate(now.getDate() - 1);
    if (d.toDateString() === yesterday.toDateString()) return "yesterday";
    const diffDays = Math.floor((now.getTime() - d.getTime()) / 86_400_000);
    if (diffDays < 7) return `${diffDays}d ago`;
    return d.toLocaleDateString([], { month: "short", day: "numeric" });
  }

  // ─── Swipe gesture state ──────────────────────────────────────────
  //
  // Pointer-event based (works identically on iOS WKWebView, Android
  // Chrome WebView, and desktop mouse — we only enable the listeners
  // on mobile so desktop hover actions stay unaffected).
  //
  // `swipeOffset` is the current translateX of the content layer in
  // pixels. Positive = content slid right (Pin action revealed on the
  // left). Negative = content slid left (Delete action revealed on
  // the right). `0` means closed.
  //
  // The action layer sits BEHIND the content layer and is full-row
  // height; the visible reveal width matches the absolute value of
  // swipeOffset. Once dragged past `COMMIT_THRESHOLD`, releasing snaps
  // open to ACTION_WIDTH; below the threshold, snaps back to 0.
  const ACTION_WIDTH = 72;
  const COMMIT_THRESHOLD = 36;
  const HORIZONTAL_LOCK_THRESHOLD = 8; // px before we commit to swiping
  const isMobile = () => isMobilePlatform();
  const [swipeOffset, setSwipeOffset] = createSignal(0);
  const [isDragging, setIsDragging] = createSignal(false);

  // ─── Long-press menu state ────────────────────────────────────────
  //
  // Mobile pattern (matches iOS Mail / Apple Notes): swipe is the
  // primary action shortcut, long-press surfaces secondary actions
  // that don't fit on the row. Here Rename + Save-as-note (Pin and
  // Delete already have swipe gestures, but we include them in the
  // menu too so power users have one consistent gesture for everything).
  //
  // `LONG_PRESS_MS` is the standard iOS feel; users with motor issues
  // can still tap normally (movement > 5 px or release before 500 ms
  // cancels the timer cleanly).
  const LONG_PRESS_MS = 500;
  const [longPressMenuOpen, setLongPressMenuOpen] = createSignal(false);
  let longPressTimer: number | null = null;

  let pointerId: number | null = null;
  let startX = 0;
  let startY = 0;
  let axisLocked: "x" | "y" | null = null;
  let openAtStart = 0;
  // Used by the row-level onClick handler to swallow taps that ended a
  // swipe gesture, that should snap an open row closed, OR that fired
  // a long-press menu — without this, releasing any of those would
  // also call `onSwitch` and jump to that session.
  let suppressNextClick = false;

  const cancelLongPress = () => {
    if (longPressTimer != null) {
      window.clearTimeout(longPressTimer);
      longPressTimer = null;
    }
  };

  const onPointerDown: JSX.EventHandler<HTMLDivElement, PointerEvent> = (e) => {
    if (!isMobile()) return;
    if (isEditing() || confirmDelete()) return;
    // Ignore secondary buttons + non-touch pointers when on a touch
    // device — keeps stylus / Apple Pencil scrolling working normally.
    if (e.pointerType !== "touch") return;
    pointerId = e.pointerId;
    startX = e.clientX;
    startY = e.clientY;
    axisLocked = null;
    openAtStart = swipeOffset();
    // Arm the long-press timer. Cancelled below the moment we detect
    // either (a) movement past the swipe lock threshold, (b) release
    // before LONG_PRESS_MS, or (c) any pointer-cancel.
    cancelLongPress();
    longPressTimer = window.setTimeout(() => {
      longPressTimer = null;
      // Only fire if we haven't committed to a swipe and the row is
      // still in its normal state.
      if (axisLocked === "x") return;
      if (swipeOffset() !== 0) return;
      setLongPressMenuOpen(true);
      // Eat the next click so the press-release doesn't navigate
      // away from the row underneath the menu.
      suppressNextClick = true;
    }, LONG_PRESS_MS);
  };

  const onPointerMove: JSX.EventHandler<HTMLDivElement, PointerEvent> = (e) => {
    if (pointerId == null || e.pointerId !== pointerId) return;
    const dx = e.clientX - startX;
    const dy = e.clientY - startY;
    if (axisLocked == null) {
      // Decide on the dominant axis once we've moved enough. Locking
      // to "y" cancels the swipe so the parent list keeps its vertical
      // scroll feel.
      if (Math.abs(dx) < HORIZONTAL_LOCK_THRESHOLD && Math.abs(dy) < HORIZONTAL_LOCK_THRESHOLD) {
        return;
      }
      axisLocked = Math.abs(dx) > Math.abs(dy) ? "x" : "y";
      // Any committed motion abandons the long-press intent.
      cancelLongPress();
      if (axisLocked === "x") {
        setIsDragging(true);
        // Stop the row's onClick from firing when the user releases
        // — the gesture has now committed to a swipe.
        suppressNextClick = true;
      }
    }
    if (axisLocked !== "x") return;
    // Clamp to ±ACTION_WIDTH * 1.2 so a long drag past the action width
    // still snaps cleanly when released (rubber-band would be nicer
    // but not worth the complexity for v1).
    const next = openAtStart + dx;
    const clamped = Math.max(-ACTION_WIDTH * 1.2, Math.min(ACTION_WIDTH * 1.2, next));
    setSwipeOffset(clamped);
  };

  const releasePointer = (target: HTMLDivElement | null) => {
    if (target && pointerId != null) {
      try { target.releasePointerCapture(pointerId); } catch { /* noop */ }
    }
    pointerId = null;
    setIsDragging(false);
  };

  const onPointerUp: JSX.EventHandler<HTMLDivElement, PointerEvent> = (e) => {
    if (pointerId == null || e.pointerId !== pointerId) return;
    cancelLongPress();
    const final = swipeOffset();
    // Snap decision: keep the same sign the user dragged in, just
    // round to open / closed based on the threshold.
    if (final > COMMIT_THRESHOLD) {
      setSwipeOffset(ACTION_WIDTH);
    } else if (final < -COMMIT_THRESHOLD) {
      setSwipeOffset(-ACTION_WIDTH);
    } else {
      setSwipeOffset(0);
    }
    releasePointer(e.currentTarget);
  };

  const onPointerCancel: JSX.EventHandler<HTMLDivElement, PointerEvent> = (e) => {
    if (pointerId == null || e.pointerId !== pointerId) return;
    cancelLongPress();
    setSwipeOffset(0);
    releasePointer(e.currentTarget);
  };

  const handleRowClick = () => {
    // A click that closed the swipe (suppressNextClick) or that was
    // raised on an open row should NOT navigate — first snap the row
    // closed instead.
    if (suppressNextClick) {
      suppressNextClick = false;
      return;
    }
    if (longPressMenuOpen()) {
      // Tap on the row while menu is open — close the menu, don't
      // navigate. The menu has its own buttons + backdrop.
      setLongPressMenuOpen(false);
      return;
    }
    if (swipeOffset() !== 0) {
      setSwipeOffset(0);
      return;
    }
    if (!isEditing() && !confirmDelete()) props.onSwitch();
  };

  const closeSwipe = () => setSwipeOffset(0);
  const closeLongPressMenu = () => setLongPressMenuOpen(false);

  return (
    <div
      class={`group relative overflow-hidden select-none ${
        props.isActive ? "border-l-2 border-purple" : "border-l-2 border-transparent"
      }`}
    >
      {/* ── Action layer (under the content) ──────────────────────────
          Two fixed-width buttons hugging the row edges. Visibility is
          controlled by the content layer translating over them — when
          content is at offset 0, the layer is completely hidden behind
          it. When content slides, the matching edge becomes visible. */}
      <div class="absolute inset-0 flex pointer-events-none">
        {/* Pin (revealed on swipe-right) */}
        <button
          class="bg-purple/80 text-white flex items-center justify-center gap-1 transition-opacity"
          style={{
            width: `${ACTION_WIDTH}px`,
            opacity: swipeOffset() > 0 ? 1 : 0,
            "pointer-events": swipeOffset() >= ACTION_WIDTH ? "auto" : "none",
          }}
          onClick={(e) => {
            e.stopPropagation();
            props.onTogglePin();
            closeSwipe();
          }}
          aria-label={props.session.pinned ? "Unpin chat" : "Pin chat"}
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
            <path d="M12 2L9.5 8.5L3 9L8 13.5L6.5 20L12 16.5L17.5 20L16 13.5L21 9L14.5 8.5z" />
          </svg>
          <span class="text-[10px] font-medium">{props.session.pinned ? "Unpin" : "Pin"}</span>
        </button>
        <div class="flex-1" />
        {/* Delete (revealed on swipe-left) */}
        <button
          class="bg-red text-white flex items-center justify-center gap-1 transition-opacity"
          style={{
            width: `${ACTION_WIDTH}px`,
            opacity: swipeOffset() < 0 ? 1 : 0,
            "pointer-events": swipeOffset() <= -ACTION_WIDTH ? "auto" : "none",
          }}
          onClick={(e) => {
            e.stopPropagation();
            props.onDelete();
            closeSwipe();
          }}
          aria-label="Delete chat"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M3 6h18M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2" />
          </svg>
          <span class="text-[10px] font-medium">Delete</span>
        </button>
      </div>

      {/* ── Content layer (translates on swipe) ──────────────────────
          Captures the pointer when on mobile + touch. Has the original
          row body and hover-actions for desktop. */}
      <div
        class={`relative flex items-stretch gap-1 px-2 py-2 cursor-pointer transition-colors ${
          props.isActive ? "bg-purple/10" : "hover:bg-bg-hover"
        } bg-bg-secondary`}
        style={{
          transform: `translateX(${swipeOffset()}px)`,
          transition: isDragging() ? "none" : "transform 180ms cubic-bezier(0.22, 0.61, 0.36, 1)",
          "touch-action": "pan-y",
        }}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerCancel={onPointerCancel}
        onClick={handleRowClick}
      >
        <div class="flex-1 min-w-0 flex flex-col justify-center">
          <Show
            when={!isEditing()}
            fallback={
              <input
                type="text"
                value={editValue()}
                onInput={(e) => setEditValue(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") commitRename(e);
                  if (e.key === "Escape") {
                    setIsEditing(false);
                    setEditValue(props.session.title);
                  }
                }}
                onBlur={() => commitRename()}
                onClick={(e) => e.stopPropagation()}
                class="bg-bg-primary border border-border rounded px-1.5 py-0.5 text-xs text-text-primary focus:outline-none focus:border-purple"
                ref={(el) => setTimeout(() => el.focus(), 0)}
              />
            }
          >
            <div class="flex items-center gap-1.5 min-w-0">
              <Show when={props.session.pinned}>
                <svg width="10" height="10" viewBox="0 0 24 24" fill="currentColor" class="text-purple shrink-0">
                  <path d="M12 2L9.5 8.5L3 9L8 13.5L6.5 20L12 16.5L17.5 20L16 13.5L21 9L14.5 8.5z" />
                </svg>
              </Show>
              <span class="text-xs text-text-primary truncate font-medium">{props.session.title}</span>
            </div>
            <Show when={props.session.preview}>
              <div class="text-[10px] text-text-muted truncate mt-0.5 leading-tight">
                {props.session.preview}
              </div>
            </Show>
            <div class="flex items-center gap-1.5 mt-0.5 text-[10px] text-text-muted">
              <span>{props.session.message_count} msg{props.session.message_count === 1 ? "" : "s"}</span>
              <span>·</span>
              <span>{fmtTime(props.session.last_active_at)}</span>
              <Show when={props.session.context_note_path}>
                <span>·</span>
                <span class="truncate text-accent/80" title={`Grounded on ${props.session.context_note_path}`}>
                  ⚓ {props.session.context_note_path?.split("/").pop()?.replace(/\.md$/, "")}
                </span>
              </Show>
            </div>
          </Show>
        </div>

        {/* Hover actions — stacked vertically to the right. Desktop only;
            mobile users use the swipe gestures instead and the hover
            row is functionally invisible there (no hover state on touch). */}
        <Show when={!isEditing() && !confirmDelete()}>
          <div class="flex flex-col items-center justify-center gap-0.5 opacity-0 group-hover:opacity-100 transition-opacity shrink-0">
            <button
              class={`w-6 h-6 flex items-center justify-center rounded hover:bg-bg-tertiary transition-colors ${
                props.session.pinned ? "text-purple" : "text-text-muted hover:text-purple"
              }`}
              onClick={(e) => {
                e.stopPropagation();
                props.onTogglePin();
              }}
              title={props.session.pinned ? "Unpin" : "Pin"}
            >
              <svg width="11" height="11" viewBox="0 0 24 24" fill={props.session.pinned ? "currentColor" : "none"} stroke="currentColor" stroke-width="2">
                <path d="M12 2L9.5 8.5L3 9L8 13.5L6.5 20L12 16.5L17.5 20L16 13.5L21 9L14.5 8.5z" />
              </svg>
            </button>
            <button
              class="w-6 h-6 flex items-center justify-center rounded hover:bg-bg-tertiary text-text-muted hover:text-text-secondary"
              onClick={(e) => {
                e.stopPropagation();
                setEditValue(props.session.title);
                setIsEditing(true);
              }}
              title="Rename"
            >
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" />
                <path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z" />
              </svg>
            </button>
          </div>
          <div class="flex flex-col items-center justify-center gap-0.5 opacity-0 group-hover:opacity-100 transition-opacity shrink-0">
            <button
              class="w-6 h-6 flex items-center justify-center rounded hover:bg-bg-tertiary text-text-muted hover:text-accent"
              onClick={(e) => {
                e.stopPropagation();
                props.onSaveAsNote();
              }}
              title="Save as note in Roost"
            >
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z" />
                <polyline points="17 21 17 13 7 13 7 21" />
                <polyline points="7 3 7 8 15 8" />
              </svg>
            </button>
            <button
              class="w-6 h-6 flex items-center justify-center rounded hover:bg-red/20 text-text-muted hover:text-red"
              onClick={(e) => {
                e.stopPropagation();
                setConfirmDelete(true);
              }}
              title="Delete"
            >
              <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M3 6h18M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2" />
              </svg>
            </button>
          </div>
        </Show>

        {/* Delete confirmation — inline yes/no, no modal. Reachable from
            the desktop trash icon AND from any explicit confirm flow
            the parent might invoke; the mobile swipe-Delete commits
            immediately without this confirm (the swipe itself is the
            confirmation, matching iOS Mail). */}
        <Show when={confirmDelete()}>
          <div class="flex flex-col gap-1 self-center shrink-0">
            <button
              class="text-[10px] px-1.5 py-0.5 rounded bg-red/20 text-red hover:bg-red/30"
              onClick={(e) => {
                e.stopPropagation();
                props.onDelete();
                setConfirmDelete(false);
              }}
            >
              Delete
            </button>
            <button
              class="text-[10px] px-1.5 py-0.5 rounded bg-bg-tertiary text-text-muted hover:bg-bg-hover"
              onClick={(e) => {
                e.stopPropagation();
                setConfirmDelete(false);
              }}
            >
              Cancel
            </button>
          </div>
        </Show>
      </div>

      {/* ── Long-press action menu (mobile) ──────────────────────────
          Overlays the row when the user holds for LONG_PRESS_MS. The
          backdrop dims the row + captures outside-taps to close the
          menu without firing the row's onSwitch. Only Rename + Save
          live here because Pin and Delete are already on swipe; this
          keeps the menu short + scannable. */}
      <Show when={longPressMenuOpen()}>
        <div
          class="absolute inset-0 z-20 flex items-center justify-center bg-black/40 backdrop-blur-[2px]"
          onClick={(e) => {
            e.stopPropagation();
            closeLongPressMenu();
          }}
        >
          <div
            class="flex items-stretch gap-1 bg-bg-secondary border border-border rounded-lg shadow-lg p-1"
            onClick={(e) => e.stopPropagation()}
          >
            <button
              class="flex flex-col items-center justify-center gap-0.5 px-3 py-1.5 rounded text-text-secondary hover:bg-bg-hover hover:text-text-primary"
              onClick={() => {
                setEditValue(props.session.title);
                setIsEditing(true);
                closeLongPressMenu();
              }}
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" />
                <path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z" />
              </svg>
              <span class="text-[10px] font-medium">Rename</span>
            </button>
            <button
              class="flex flex-col items-center justify-center gap-0.5 px-3 py-1.5 rounded text-text-secondary hover:bg-bg-hover hover:text-accent"
              onClick={() => {
                props.onSaveAsNote();
                closeLongPressMenu();
              }}
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z" />
                <polyline points="17 21 17 13 7 13 7 21" />
                <polyline points="7 3 7 8 15 8" />
              </svg>
              <span class="text-[10px] font-medium">Save</span>
            </button>
            <button
              class="flex flex-col items-center justify-center gap-0.5 px-3 py-1.5 rounded text-text-muted hover:bg-bg-hover"
              onClick={closeLongPressMenu}
            >
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
                <path d="M18 6L6 18M6 6l12 12" />
              </svg>
              <span class="text-[10px] font-medium">Cancel</span>
            </button>
          </div>
        </div>
      </Show>
    </div>
  );
}
