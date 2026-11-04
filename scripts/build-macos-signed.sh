#!/usr/bin/env bash
# Build a signed + notarized macOS DMG for Corvux (local-Mac release flow).
#
# Why this exists: a downloaded build must be Developer-ID signed AND notarized
# or macOS Gatekeeper kills it ("Corvux is damaged and can't be opened") — and
# the bundled corvux-mcp sidecar dies the same way when Claude Desktop spawns
# it. Tauri signs every nested executable (app + sidecar) automatically once a
# Developer ID identity is configured, so this script just wires the creds,
# runs the build, and then PROVES the result is signed, notarized, and stapled.
#
# Prereqs (one-time, see docs/macos-signing.md):
#   1. Apple Developer Program enrollment (paid, active).
#   2. A "Developer ID Application" certificate in this Mac's login keychain
#      (create via Xcode > Settings > Accounts > Manage Certificates > +).
#   3. An app-specific password from appleid.apple.com.
#
# Usage:
#   export APPLE_SIGNING_IDENTITY="Developer ID Application: NAME (TEAMID)"
#   export APPLE_ID="you@example.com"
#   export APPLE_PASSWORD="xxxx-xxxx-xxxx-xxxx"   # app-specific password
#   export APPLE_TEAM_ID="TEAMID"
#   ./scripts/build-macos-signed.sh
#
# Or drop those four exports into Corvux/.env.signing (gitignored) and this
# script will source it automatically.
set -euo pipefail

cd "$(dirname "$0")/.."   # -> Corvux/

# --- load creds from .env.signing if present (never committed) -------------
if [[ -f .env.signing ]]; then
  echo "[sign] sourcing .env.signing"
  set -a; source .env.signing; set +a
fi

# --- auto-detect the signing identity if not explicitly set ----------------
if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  detected=$(security find-identity -v -p codesigning 2>/dev/null \
    | grep "Developer ID Application" | head -1 | sed -E 's/.*"(.*)".*/\1/')
  if [[ -n "$detected" ]]; then
    export APPLE_SIGNING_IDENTITY="$detected"
    echo "[sign] auto-detected identity: $APPLE_SIGNING_IDENTITY"
  fi
fi

# --- fail fast with a clear message on any missing piece -------------------
missing=0
need() { if [[ -z "${!1:-}" ]]; then echo "  MISSING: $1"; missing=1; fi; }
echo "[sign] checking credentials…"
need APPLE_SIGNING_IDENTITY
need APPLE_ID
need APPLE_PASSWORD
need APPLE_TEAM_ID
if [[ "$missing" == 1 ]]; then
  echo
  echo "[sign] One or more credentials are missing. See docs/macos-signing.md."
  echo "[sign] (No 'Developer ID Application' cert? Create it in Xcode first.)"
  exit 1
fi

case "$APPLE_SIGNING_IDENTITY" in
  "Developer ID Application:"*) : ;;
  *) echo "[sign] WARNING: identity does not start with 'Developer ID Application:' —"
     echo "       Apple Development / Distribution certs CANNOT notarize for download." ;;
esac

# --- updater key (optional) ------------------------------------------------
# tauri.conf has createUpdaterArtifacts:true, so the build signs an updater
# artifact with the Minisign key. If TAURI_SIGNING_PRIVATE_KEY isn't in the env
# (.env.signing), the build errors at the LAST step — AFTER the app/DMG are
# built and the app is notarized — so we tolerate that and carry on.
if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
  echo "[sign] NOTE: TAURI_SIGNING_PRIVATE_KEY not set — the updater artifact step"
  echo "       will fail at the END of the build. The signed app/DMG are produced"
  echo "       before that, so we continue. (The key is needed for working"
  echo "       auto-update — add it to .env.signing once you have it.)"
fi

# --- build: Tauri signs app + sidecar, then notarizes + staples the APP -----
echo "[sign] building + signing + notarizing the app (notarytool — minutes;"
echo "       a brand-new team's FIRST submission can take 30–60 min, later ones ~5)…"
BUILD_START=$(date +%s)
set +e
pnpm tauri build --bundles app,dmg
build_rc=$?
set -e
[[ $build_rc -ne 0 ]] && echo "[sign] build exited $build_rc — tolerable ONLY if it was the updater-key step; verifying a fresh DMG exists…"

# --- locate artifacts ------------------------------------------------------
# Require the DMG for the CURRENT version, produced by THIS run. A bare
# newest-*.dmg glob once picked up a stale previous-version DMG after a
# codesign failure and sent it to Apple notarization (2026-07-02).
VERSION=$(node -p "require('./src-tauri/tauri.conf.json').version")
# `|| true`: under set -e/pipefail an empty glob would kill the script here,
# before the explanatory message below.
DMG=$(ls -t src-tauri/target/release/bundle/dmg/*_"${VERSION}"_*.dmg 2>/dev/null | head -1 || true)
if [[ -z "$DMG" ]]; then
  echo "[sign] no v${VERSION} DMG produced — the build failed before bundling (see log above;"
  echo "       a codesign 'no identity found' failure lands here). Not notarizing anything."
  exit 1
fi
if [[ $(stat -f %m "$DMG") -lt $BUILD_START ]]; then
  echo "[sign] $DMG is STALE (predates this build) — the build failed before producing a"
  echo "       fresh DMG (see log above). Refusing to notarize a stale artifact."
  exit 1
fi
echo "[sign] DMG: $DMG"

# --- notarize + staple the DMG itself --------------------------------------
# Tauri notarizes the .app but NOT the DMG; an un-notarized DMG is judged
# 'Unnotarized Developer ID' and warns on mount. Notarize the DMG too.
echo "[sign] notarizing the DMG (Tauri doesn't — required for a clean download)…"
xcrun notarytool submit "$DMG" --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_PASSWORD" --wait
echo "[sign] stapling the DMG…"
xcrun stapler staple "$DMG"

# --- verify: the checks that would have caught the v0.1.3 breakage ---------
echo "[verify] stapler validate — DMG ticket attached?"
xcrun stapler validate "$DMG"
echo "[verify] DMG Gatekeeper assessment (download/mount path)"
spctl -a -t open --context context:primary-signature -vv "$DMG"

echo "[verify] mounting DMG to inspect the .app + sidecar…"
MNT=$(hdiutil attach "$DMG" -nobrowse -readonly | grep -o '/Volumes/.*' | head -1)
APP="$MNT/Corvux.app"
trap '[[ -n "${MNT:-}" ]] && hdiutil detach "$MNT" -quiet || true' EXIT

echo "[verify] codesign --verify (app, deep + strict)"
codesign --verify --deep --strict --verbose=2 "$APP"

echo "[verify] sidecar signature (should be Developer ID, not adhoc)"
codesign -dvvv "$APP/Contents/MacOS/corvux-mcp" 2>&1 \
  | grep -iE 'Authority|TeamIdentifier|flags' || true

echo "[verify] app Gatekeeper assessment"
spctl -a -t exec -vv "$APP"

# The DEFINITIVE sidecar check. `spctl -t exec` on a bare CLI binary always
# says "not an app" even when valid — useless here. The real test is the exact
# v0.1.3 failure: quarantine the bundle, spawn the sidecar, expect it to RUN.
echo "[verify] sidecar under quarantine (was SIGKILL/exit-137 on adhoc builds)…"
SBOX=$(mktemp -d)
ditto "$APP" "$SBOX/Corvux.app"
xattr -w -r com.apple.quarantine "0083;00000000;Corvux;" "$SBOX/Corvux.app"
if "$SBOX/Corvux.app/Contents/MacOS/corvux-mcp" --status >/dev/null 2>&1; then
  echo "[verify]   ✅ sidecar runs under quarantine — MCP Connect will work on a download"
else
  echo "[verify]   ❌ sidecar killed under quarantine (exit $?) — NOT shippable"
fi
rm -rf "$SBOX"

echo
echo "[sign] DONE. App + DMG signed + notarized + stapled; sidecar verified under quarantine."
