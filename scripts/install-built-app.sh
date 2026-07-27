#!/bin/sh
#
# Build Dakia and install the .app into /Applications, replacing any
# previous version.  If the app is currently running it is killed first
# so the replacement does not fail.

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

# ---------------------------------------------------------------------------
# 1.  Build the release app bundle. Local installation does not need a DMG or
# updater archive, so this intentionally uses the narrow install bundle.
# ---------------------------------------------------------------------------
echo "==> Building Dakia (release) …"
npm run build:install:bundle

# ---------------------------------------------------------------------------
# 2.  Locate the freshly built .app
# ---------------------------------------------------------------------------
app_bundle="$repo_root/target/release/bundle/macos/Dakia.app"
if [ ! -d "$app_bundle" ]; then
  echo "ERROR: expected bundle at $app_bundle but it does not exist." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# 3.  Kill a running Dakia instance (so the copy below does not fail)
# ---------------------------------------------------------------------------
if pgrep -x "Dakia" >/dev/null 2>&1; then
  echo "==> Dakia is running — quitting …"
  # Ask nicely first (macOS / Tauri).  pkill -x matches the exact process
  # name, which is what Tauri apps use on macOS.
  pkill -x "Dakia" 2>/dev/null || true
  sleep 1

  # If it is still alive, force-kill.
  if pgrep -x "Dakia" >/dev/null 2>&1; then
    echo "==> Force-quitting Dakia …"
    pkill -9 -x "Dakia" 2>/dev/null || true
    sleep 1
  fi
fi

# ---------------------------------------------------------------------------
# 4.  Replace the installed app
# ---------------------------------------------------------------------------
echo "==> Installing Dakia.app into /Applications …"
rm -rf /Applications/Dakia.app
cp -Rp "$app_bundle" /Applications/Dakia.app

echo "==> Done.  Dakia.app is installed at /Applications/Dakia.app"
