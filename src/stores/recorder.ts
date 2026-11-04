/**
 * Voice-memo recorder open/close state. Hoisted from MobileBottomBar so
 * desktop entry points (TopBar mic icon, command palette, Cmd+Shift+R)
 * and the mobile bottom bar can all trigger the same recorder sheet
 * without prop-drilling a setter through five layers of layout.
 *
 * The sheet itself is mounted once at App.tsx; this store just toggles
 * its visibility. Same pattern as `promptStore` and `toastStore`.
 */
import { createSignal } from "solid-js";

const [isOpen, setIsOpen] = createSignal(false);

export const recorderStore = {
  isOpen,
  open: () => setIsOpen(true),
  close: () => setIsOpen(false),
};
