// Shared platform detection.
//
// iPad caveat: since iPadOS 13, WKWebView defaults its user-agent string
// to a "Macintosh" form (the "Request Desktop Website" behaviour is the
// system default, not just a Safari toggle). So a substring check for
// "iPad" alone returns false on iPad — the iPad gets misclassified as
// macOS and falls into desktop-only code paths (e.g. the dialog-plugin
// folder picker that doesn't exist on iOS). The cross-browser signal
// that actually distinguishes iPad from Mac is `navigator.maxTouchPoints`
// (Macs = 0, iPads = 5). We combine the UA check with the touch-points
// fallback to catch both classic-UA iOS and modern iPad.

const _ua = typeof navigator !== "undefined" ? navigator.userAgent : "";
const _maxTouchPoints =
  typeof navigator !== "undefined" ? (navigator.maxTouchPoints ?? 0) : 0;

const _isIPadMasqueradingAsMac =
  /Macintosh/.test(_ua) && _maxTouchPoints > 1;

const _isIOS =
  /iPhone|iPad|iPod/i.test(_ua) || _isIPadMasqueradingAsMac;

const _isAndroid = /Android/i.test(_ua);

// Mobile = Android or iOS WebView. Desktop keeps the multi-panel layout;
// mobile collapses chrome to a single hamburger + overflow menu
// (Obsidian-style).
const _isMobile = _isIOS || _isAndroid;

// macOS desktop only — excludes iPad-masquerading-as-Mac. Used to hide
// the custom in-app min/max/close trio when the native macOS traffic
// lights are doing that job (see lib.rs setup — decorations are
// re-enabled at runtime on macOS).
const _isMacOS = /Mac/i.test(_ua) && !_isIOS;

export function isMobile(): boolean {
  return _isMobile;
}

export function isIOS(): boolean {
  return _isIOS;
}

export function isAndroid(): boolean {
  return _isAndroid;
}

export function isMacOS(): boolean {
  return _isMacOS;
}
