#!/usr/bin/env bash

# Source this file from local release scripts. It reads secrets into the
# current process only; values are never printed or written into the repo.

dakia_keychain_read() {
  local service="$1"
  local account="${2:-}"
  if [[ -n "$account" ]]; then
    /usr/bin/security find-generic-password \
      -s "$service" -a "$account" -w 2>/dev/null
  else
    /usr/bin/security find-generic-password -s "$service" -w 2>/dev/null
  fi
}

dakia_require_expected_release_origin() {
  local root_dir="$1" origin_url
  origin_url="$(git -C "$root_dir" remote get-url origin 2>/dev/null || true)"
  case "$origin_url" in
    git@github.com:DakiaMail/dakia-desktop.git|https://github.com/DakiaMail/dakia-desktop.git|https://github.com/DakiaMail/dakia-desktop)
      ;;
    *)
      echo "origin does not identify the expected repository: DakiaMail/dakia-desktop" >&2
      return 1
      ;;
  esac
}

# A cached refs/remotes/origin/main can be stale even after a successful
# earlier fetch. Every release mutation entrypoint must prove the checkout is
# still at the branch tip currently advertised by origin.
dakia_require_live_main_provenance() {
  local root_dir="$1" head cached_main live_refs live_main

  head="$(git -C "$root_dir" rev-parse --verify HEAD 2>/dev/null)" || {
    echo "Could not resolve the release checkout HEAD." >&2
    return 1
  }
  cached_main="$(git -C "$root_dir" rev-parse --verify refs/remotes/origin/main 2>/dev/null)" || {
    echo "Cached origin/main is unavailable; fetch origin before releasing." >&2
    return 1
  }
  if ! live_refs="$(git -C "$root_dir" ls-remote --exit-code origin refs/heads/main)"; then
    echo "Could not verify live origin/main with git ls-remote; refusing to rely on cached refs." >&2
    return 1
  fi
  live_main="$(awk '$2 == "refs/heads/main" { print $1 }' <<<"$live_refs")"
  if [[ "$live_main" != "$cached_main" || "$live_main" != "$head" ]]; then
    echo "HEAD, cached origin/main, and live origin/main must all match before releasing; fetch and fast-forward main." >&2
    return 1
  fi
}

dakia_require_release_mutation_provenance() {
  local root_dir="$1"
  dakia_require_expected_release_origin "$root_dir" && \
    dakia_require_live_main_provenance "$root_dir"
}

dakia_load_signing_environment() {
  local user_home
  user_home="$(dscl . -read "/Users/$(id -un)" NFSHomeDirectory | awk '{print $2}')"

  DAKIA_EXPECTED_APPLE_SIGNING_IDENTITY="Developer ID Application: Mashal Tech OU (34T9L3FGZC)"
  DAKIA_EXPECTED_APPLE_TEAM_ID="34T9L3FGZC"
  export DAKIA_EXPECTED_APPLE_SIGNING_IDENTITY DAKIA_EXPECTED_APPLE_TEAM_ID

  if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    APPLE_SIGNING_IDENTITY="$DAKIA_EXPECTED_APPLE_SIGNING_IDENTITY"
    export APPLE_SIGNING_IDENTITY
  fi
  local updater_key_source
  updater_key_source="${TAURI_SIGNING_PRIVATE_KEY:-$user_home/.tauri/dakia-updater.key}"
  if [[ -f "$updater_key_source" ]]; then
    DAKIA_UPDATER_PRIVATE_KEY_PATH="$updater_key_source"
    TAURI_SIGNING_PRIVATE_KEY="$(<"$updater_key_source")"
    export DAKIA_UPDATER_PRIVATE_KEY_PATH TAURI_SIGNING_PRIVATE_KEY
  fi
  if [[ -z "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD+x}" ]]; then
    TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$(
      dakia_keychain_read dev.dakia.mail.updater-signing || true
    )"
    export TAURI_SIGNING_PRIVATE_KEY_PASSWORD
  fi
  APPLE_NOTARY_PROFILE="${APPLE_NOTARY_PROFILE:-dakia-notary}"
  export APPLE_NOTARY_PROFILE
}

dakia_load_r2_environment() {
  if [[ -z "${R2_ACCESS_KEY_ID:-}" ]]; then
    R2_ACCESS_KEY_ID="$(
      dakia_keychain_read dev.dakia.mail.r2 access-key-id || true
    )"
    export R2_ACCESS_KEY_ID
  fi
  if [[ -z "${R2_SECRET_ACCESS_KEY:-}" ]]; then
    R2_SECRET_ACCESS_KEY="$(
      dakia_keychain_read dev.dakia.mail.r2 secret-access-key || true
    )"
    export R2_SECRET_ACCESS_KEY
  fi
  CLOUDFLARE_ACCOUNT_ID="${CLOUDFLARE_ACCOUNT_ID:-b225fd2027198472b627795dd126aa15}"
  export CLOUDFLARE_ACCOUNT_ID
}

dakia_require_signing_environment() {
  dakia_load_signing_environment
  if [[ "$APPLE_SIGNING_IDENTITY" != "$DAKIA_EXPECTED_APPLE_SIGNING_IDENTITY" ]]; then
    echo "Release signing identity must be exactly '$DAKIA_EXPECTED_APPLE_SIGNING_IDENTITY'." >&2
    return 1
  fi
  if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
    echo "Missing updater private key content." >&2
    return 1
  fi
  if [[ -z "$TAURI_SIGNING_PRIVATE_KEY_PASSWORD" ]]; then
    echo "Missing updater key password in Keychain service dev.dakia.mail.updater-signing." >&2
    return 1
  fi
  /usr/bin/security find-identity -v -p codesigning |
    grep -Fq "\"$APPLE_SIGNING_IDENTITY\""
  xcrun notarytool history --keychain-profile "$APPLE_NOTARY_PROFILE" >/dev/null
}

dakia_require_r2_environment() {
  dakia_load_r2_environment
  if [[ -z "$R2_ACCESS_KEY_ID" || -z "$R2_SECRET_ACCESS_KEY" ]]; then
    echo "Missing bucket-scoped R2 credentials in Keychain service dev.dakia.mail.r2." >&2
    return 1
  fi
}
