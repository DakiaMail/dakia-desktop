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

dakia_load_signing_environment() {
  local user_home
  user_home="$(dscl . -read "/Users/$(id -un)" NFSHomeDirectory | awk '{print $2}')"

  if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    APPLE_SIGNING_IDENTITY="$(
      /usr/bin/security find-identity -v -p codesigning 2>/dev/null |
        sed -n 's/.*"\(Developer ID Application:.*\)"/\1/p' |
        head -1
    )"
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
  if [[ -z "$APPLE_SIGNING_IDENTITY" ]]; then
    echo "No Developer ID Application identity is available in Keychain." >&2
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
