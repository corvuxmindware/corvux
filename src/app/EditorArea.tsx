// Main editor area: TabBar (titlebar) + TopBar (mobile breadcrumb row)
// + UpdateBanner + the scroll container with either GraphView or the
// note view (heading + Properties + Editor) + StatusBar. Split off
// App.tsx to shrink the shell — everything inside is content for the
// active tab.

import { Show, createSignal, type Setter } from "solid-js";
import type { EditorView } from "@codemirror/view";
import Editor from "../components/Editor/Editor";
import GraphView from "../components/Graph/GraphView";
import Properties from "../components/Properties/Properties";
import StatusBar from "../components/StatusBar/StatusBar";
import TabBar from "../components/Tabs/TabBar";
import TopBar from "../components/TopBar/TopBar";
import UpdateBanner from "../components/Updater/UpdateBanner";
import Welcome from "../components/Welcome/Welcome";
import MobileFormatBar from "../components/MobileFormatBar/MobileFormatBar";
import type { NoteInfo } from "../components/Editor/extensions/link-autocomplete";
import type { Frontmatter } from "../lib/frontmatter";
import { bookmarksStore } from "../stores/bookmarks";
import { GRAPH_TAB_PATH, tabStore } from "../stores/tabs";
import type { createRightSidebar } from "./right-sidebar";

interface Breadcrumb {
  roost: string;
  segments: string[];
  path: string;
}

interface EditorAreaProps {
  // Layout/state getters
  isMobile: () => boolean;
  isMacDesktop: () => boolean;
  sidebarVisible: () => boolean;
  isGraphActive: () => boolean;
  showChat: () => boolean;
  breadcrumb: () => Breadcrumb | null;
  parsed: () => { frontmatter?: Frontmatter | null };
  stats: () => {
    words: number;
    chars: number;
    lines: number;
    properties: number;
  };
  scrollContainerRef: (el: HTMLDivElement) => void;
  /** Fired on every scroll of the note content container — drives the
   *  mobile bottom bar's auto-hide-while-scrolling behaviour. */
  onContentScroll?: () => void;
  /** True while the user is scrolling — forwarded to MobileFormatBar so
   *  it slides away with MobileBottomBar instead of hovering alone. */
  chromeHidden?: () => boolean;
  desktopTrafficLightInset: number;

  // Editor helpers (closures from App)
  allNotes: () => NoteInfo[];
  goToLine: () => number;
  setGoToLine: Setter<number>;
  handleWikiNavigate: (target: string) => Promise<void>;
  handleImagePaste: (file: File) => Promise<string | null>;
  handleLoadImage: (relPath: string) => Promise<string | null>;
  handleLoadAudio: (relPath: string) => Promise<import("../lib/attachments").AudioResource | null>;
  handleDeleteAudio: (relPath: string) => Promise<boolean>;
  /** Last file the user was on before any non-file tab (e.g. graph) took
   *  focus. The graph view uses this to seed its local-mode focus when
   *  the user opens the graph from a file tab. */
  lastFilePath: () => string | null;

  // Chrome callbacks
  rightSidebar: ReturnType<typeof createRightSidebar>;
  onNewNote: () => void | Promise<void>;
  onOpenSettings: () => void;
  onOpenQuickSwitcher: () => void;
  onOpenCommandPalette: () => void;
  onInsertImage: () => void;
  onRecordVoice: () => void;
  onToggleMobileSidebar: () => void;
  onOpenMobileMenu: () => void;
}

export default function EditorArea(props: EditorAreaProps) {
  // Lifted CodeMirror view ref. The mobile format bar dispatches
  // commands through it. Null before/after the editor mounts (graph
  // tab open, no active tab, etc.). Set once per Editor instance via
  // the `onReady` callback; cleared on cleanup so a stale ref can't
  // dispatch into a destroyed view.
  const [editorView, setEditorView] = createSignal<EditorView | null>(null);

  return (
    <div class="flex-1 flex flex-col h-full overflow-hidden min-w-0">
      <TabBar
        leftInset={
          props.isMacDesktop() && !props.sidebarVisible()
            ? props.desktopTrafficLightInset
            : 0
        }
        onToggleRightSidebar={() =>
          props.rightSidebar.setVisible(!props.rightSidebar.visible())
        }
        rightSidebarOpen={props.rightSidebar.visible()}
        chatOpen={props.showChat() && !props.isMobile()}
        onNewNote={props.onNewNote}
        onOpenSettings={props.onOpenSettings}
        onInsertImage={props.onInsertImage}
        onRecordVoice={props.onRecordVoice}
      />
      <Show when={props.isMobile()}>
        <TopBar
          breadcrumb={props.breadcrumb}
          onOpenQuickSwitcher={props.onOpenQuickSwitcher}
          onOpenCommandPalette={props.onOpenCommandPalette}
          onOpenSettings={props.onOpenSettings}
          onToggleMobileSidebar={props.onToggleMobileSidebar}
          onOpenMobileMenu={props.onOpenMobileMenu}
          sidebarOpen={props.sidebarVisible()}
        />
      </Show>
      <UpdateBanner />

      <div
        ref={props.scrollContainerRef}
        onScroll={() => props.onContentScroll?.()}
        class={`flex-1 min-h-0 ${
          props.isGraphActive()
            ? "overflow-hidden"
            : "overflow-y-auto overflow-x-hidden"
        }`}
      >
        <Show when={props.isGraphActive()}>
          <GraphView
            activeNotePath={(() => {
              const path = tabStore.activeTabPath();
              if (path && path !== GRAPH_TAB_PATH) return path;
              // We're sitting on the graph tab. Use the last file the
              // user was actively viewing, not the first file in the
              // tabs() list — the latter is just "whichever tab opened
              // first" and has nothing to do with current intent.
              return (
                props.lastFilePath() ??
                tabStore.tabs().find((t) => t.kind === "file")?.path
              );
            })()}
            onNodeClick={(path) => tabStore.openTab(path)}
            onClose={() => tabStore.closeTab(GRAPH_TAB_PATH)}
          />
        </Show>
        <Show when={!props.isGraphActive()}>
          <Show
            when={tabStore.activeTabPath()}
            fallback={
              <Welcome
                onOpenQuickSwitcher={props.onOpenQuickSwitcher}
                onOpenCommandPalette={props.onOpenCommandPalette}
              />
            }
          >
            <Show when={props.breadcrumb()?.segments?.length}>
              {/* Eyebrow — parent folder path as small uppercase trail, with
                  a brass dot separator. Gives editorial context above the
                  title (cf. the Corvid design handoff) without inventing
                  frontmatter conventions we don't have yet. Only renders
                  when the note is nested inside at least one folder. */}
              <Show when={(props.breadcrumb()!.segments?.length ?? 0) > 1}>
                <div class="content-container pt-5 sm:pt-7 pb-0 text-[10px] uppercase tracking-[0.16em] text-text-muted font-medium flex items-center gap-2 flex-wrap">
                  {props.breadcrumb()!.segments.slice(0, -1).map((seg, i, arr) => (
                    <>
                      <span>{seg}</span>
                      {i < arr.length - 1 && (
                        <span class="inline-block w-1 h-1 rounded-full" style={{ background: "var(--color-accent)" }} />
                      )}
                    </>
                  ))}
                </div>
              </Show>
              <div class={`content-container ${(props.breadcrumb()!.segments?.length ?? 0) > 1 ? "pt-1.5" : "pt-6 sm:pt-8"} pb-1 flex items-start gap-2`}>
                <h1 class="flex-1 min-w-0 text-text-primary font-semibold text-[26px] sm:text-[30px] leading-tight truncate">
                  {props.breadcrumb()!.segments[
                    props.breadcrumb()!.segments.length - 1
                  ]}
                </h1>
                <Show
                  when={
                    tabStore.activeTabPath() &&
                    tabStore.activeTab()?.kind === "file"
                  }
                >
                  {(_) => {
                    const path = () => tabStore.activeTabPath()!;
                    const isBM = () => bookmarksStore.isBookmarked(path());
                    return (
                      <button
                        onClick={() => void bookmarksStore.toggle(path())}
                        class={`shrink-0 w-11 h-11 sm:w-9 sm:h-9 mt-0.5 sm:mt-1 flex items-center justify-center rounded transition-colors ${
                          isBM()
                            ? "text-accent hover:bg-bg-hover"
                            : "text-text-muted hover:text-text-secondary hover:bg-bg-hover"
                        }`}
                        title={isBM() ? "Remove bookmark" : "Bookmark this note"}
                        aria-label={
                          isBM() ? "Remove bookmark" : "Bookmark this note"
                        }
                        aria-pressed={isBM()}
                      >
                        <svg
                          width="18"
                          height="18"
                          viewBox="0 0 24 24"
                          fill={isBM() ? "currentColor" : "none"}
                          stroke="currentColor"
                          stroke-width="1.8"
                          stroke-linecap="round"
                          stroke-linejoin="round"
                        >
                          <path d="M19 21l-7-5-7 5V5a2 2 0 012-2h10a2 2 0 012 2z" />
                        </svg>
                      </button>
                    );
                  }}
                </Show>
              </div>
            </Show>
            <Properties frontmatter={props.parsed().frontmatter ?? {}} />
            <div
              class="content-container flex-1 flex flex-col"
              style={{ "min-height": "calc(100vh - 200px)" }}
            >
              {/* Bottom clearance for the floating bar lives on .cm-content
                  (global.css, safe-area-aware) — that's the actual scroll
                  content, so it works whether CM scrolls internally or the
                  outer container does. */}
              <Editor
                content={tabStore.activeContent()}
                onChange={(val) => tabStore.updateContent(val)}
                onNavigate={props.handleWikiNavigate}
                getNotes={() => props.allNotes()}
                goToLine={props.goToLine}
                onImagePaste={props.handleImagePaste}
                onLoadImage={props.handleLoadImage}
                onLoadAudio={props.handleLoadAudio}
                onDeleteAudio={props.handleDeleteAudio}
                readOnly={tabStore.isReadOnly}
                onReady={(v) => setEditorView(v)}
              />
            </div>
          </Show>
        </Show>
      </div>

      {/* Note status strip — desktop note views only. The Graph view has
          its own footer, and on mobile the bottom belongs solely to the
          floating action bar (the footer competed with it for the thumb
          zone). Mobile surfaces save-state in the top bar instead, and
          word count / backlinks on demand. */}
      <Show when={!props.isGraphActive() && !props.isMobile()}>
        <StatusBar stats={props.stats} />
      </Show>

      {/* Mobile markdown format bar. Renders whenever a file tab is
          active and the editor is unlocked — with the keyboard up it
          docks to the keyboard, with it down it parks above the bottom
          bar so the formatting buttons are discoverable before typing.
          The bar's own visibility gate handles all of that.
          `position: fixed` inside, so DOM placement here is just for
          lifecycle/timing. */}
      <MobileFormatBar
        view={editorView}
        onInsertImage={props.onInsertImage}
        chromeHidden={props.chromeHidden}
      />
    </div>
  );
}
