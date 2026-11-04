import { render } from "solid-js/web";
import { ErrorBoundary } from "solid-js";
import App from "./App";
import { installNativeContextMenuGuard } from "./lib/native-context-menu";
import { installReloadShortcutGuard } from "./lib/native-shortcuts";
import { nudgeFirstPaint } from "./lib/force-first-paint";
import "./styles/global.css";

// Block the WebView's default right-click menu (Edge's "Save as / Print /
// More tools → Share") everywhere except editable text. On in dev too, so
// the native-app feel is verifiable while developing. See module docs.
installNativeContextMenuGuard();

// Block browser reload (Ctrl/Cmd+R, F5) in the SHIPPED build only — a
// reload wipes all in-memory app state. In dev, reload stays available as
// a fallback when HMR misses a change.
if (import.meta.env.PROD) {
  installReloadShortcutGuard();
}

function ErrorFallback(props: { error: Error; reset: () => void }) {
  return (
    <div class="h-screen w-screen flex items-center justify-center bg-[#1e1e2e] text-[#cdd6f4]">
      <div class="max-w-md text-center space-y-4 p-8">
        <div class="text-4xl">:(</div>
        <h1 class="text-xl font-bold">Something went wrong</h1>
        <p class="text-sm text-[#a6adc8]">
          Corvux ran into an unexpected error. Your files are safe on disk.
        </p>
        <pre class="text-xs text-left bg-[#313244] rounded p-3 overflow-auto max-h-40 text-[#f38ba8]">
          {props.error?.message || "Unknown error"}
        </pre>
        <div class="flex gap-3 justify-center">
          <button
            onClick={props.reset}
            class="px-4 py-2 bg-[#cba6f7] text-[#1e1e2e] rounded font-medium hover:opacity-90 transition-opacity"
          >
            Try Again
          </button>
          <button
            onClick={() => window.location.reload()}
            class="px-4 py-2 bg-[#313244] text-[#cdd6f4] rounded font-medium hover:bg-[#45475a] transition-colors"
          >
            Reload App
          </button>
        </div>
      </div>
    </div>
  );
}

render(
  () => (
    <ErrorBoundary fallback={(err, reset) => <ErrorFallback error={err} reset={reset} />}>
      <App />
    </ErrorBoundary>
  ),
  document.getElementById("root")!
);

// iOS WKWebView sometimes doesn't composite the first frame until the user
// touches the screen (cold-launch "frozen until tapped"). Force that initial
// repaint ourselves. No-op off iOS. See force-first-paint.ts.
nudgeFirstPaint();
