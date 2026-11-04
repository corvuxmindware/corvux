import { Show, Suspense, lazy, type JSX } from "solid-js";
import type { PaletteCommand } from "../components/CommandPalette/CommandPalette";
import type { PageId as SettingsPageId } from "../components/Settings/SettingsPanel";
import type { MobileMenuProps } from "../components/TopBar/MobileMenu";
import PromptModal from "../components/Modals/PromptModal";

// These surfaces are only needed after explicit user interaction. Keeping them
// out of the startup chunk reduces launch parsing work without changing their
// lifecycle: each component is still mounted only while its `Show` is true.
const QuickSwitcher = lazy(
  () => import("../components/QuickSwitcher/QuickSwitcher"),
);
const CommandPalette = lazy(
  () => import("../components/CommandPalette/CommandPalette"),
);
const SettingsPanel = lazy(
  () => import("../components/Settings/SettingsPanel"),
);
const MobileMenu = lazy(() => import("../components/TopBar/MobileMenu"));
const Onboarding = lazy(() => import("../components/Onboarding/Onboarding"));
const ShortcutOverlay = lazy(
  () => import("../components/Shortcuts/ShortcutOverlay"),
);
const PathFinderModal = lazy(
  () => import("../components/PathFinder/PathFinderModal"),
);

export interface AppModalsProps {
  showQuickSwitcher: boolean;
  closeQuickSwitcher: () => void;

  showCommandPalette: boolean;
  closeCommandPalette: () => void;
  paletteCommands: PaletteCommand[];

  showSettings: boolean;
  closeSettings: () => void;
  openShortcuts: () => void;
  /** Optional deep-link target — render Settings opened on this page
   *  instead of the index. Used by the chat panel's "Add an API key"
   *  CTA so the user lands directly on AI Models. Re-evaluated each
   *  time `showSettings` flips true (SettingsPanel reads it once at
   *  mount). */
  settingsInitialPage?: SettingsPageId;

  showMobileMenu: boolean;
  closeMobileMenu: () => void;
  mobileMenuHandlers: Omit<MobileMenuProps, "onClose">;

  showOnboarding: boolean;
  /** `openChat` becomes true when the user finishes onboarding via the
   *  "Open chat and try it" path, so App opens the AI panel for them. */
  completeOnboarding: (openChat?: boolean) => void;

  showShortcuts: boolean;
  closeShortcuts: () => void;

  showPathFinder: boolean;
  closePathFinder: () => void;
}

/// Modal stack rendered at the App root. Every modal owns its own visible
/// flag in App; this component just routes them so the JSX in App.tsx isn't
/// a 100-line waterfall of `<Show when>` blocks.
export default function AppModals(props: AppModalsProps): JSX.Element {
  return (
    <>
      <Suspense fallback={null}>
        <Show when={props.showQuickSwitcher}>
          <QuickSwitcher onClose={props.closeQuickSwitcher} />
        </Show>
      </Suspense>

      <Suspense fallback={null}>
        <Show when={props.showCommandPalette}>
          <CommandPalette
            onClose={props.closeCommandPalette}
            commands={props.paletteCommands}
          />
        </Show>
      </Suspense>

      <Suspense fallback={null}>
        <Show when={props.showSettings}>
          <SettingsPanel
            onClose={props.closeSettings}
            initialPage={props.settingsInitialPage}
            onOpenShortcuts={() => {
              // Keep Settings mounted underneath. ShortcutOverlay sits
              // on top (higher z-index) so closing it returns the user
              // to wherever they were in Settings — fixes the earlier
              // "no way back from shortcuts" trap that closed Settings.
              props.openShortcuts();
            }}
          />
        </Show>
      </Suspense>

      <Suspense fallback={null}>
        <Show when={props.showMobileMenu}>
          <MobileMenu
            onClose={props.closeMobileMenu}
            {...props.mobileMenuHandlers}
          />
        </Show>
      </Suspense>

      <Suspense fallback={null}>
        <Show when={props.showOnboarding}>
          <Onboarding onComplete={props.completeOnboarding} />
        </Show>
      </Suspense>

      <Suspense fallback={null}>
        <Show when={props.showShortcuts}>
          <ShortcutOverlay onClose={props.closeShortcuts} />
        </Show>
      </Suspense>

      <Suspense fallback={null}>
        <Show when={props.showPathFinder}>
          <PathFinderModal onClose={props.closePathFinder} />
        </Show>
      </Suspense>

      {/* Self-driving via promptStore signal — keep it outside Suspense so a
          lazy modal load cannot interrupt an in-progress prompt. */}
      <PromptModal />
    </>
  );
}
