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

dakia_load_google_oauth_environment() {
  if [[ -z "${DAKIA_GOOGLE_CLIENT_ID:-}" || \
        -z "${DAKIA_GOOGLE_CLIENT_ID//[[:space:]]/}" ]]; then
    DAKIA_GOOGLE_CLIENT_ID="77400090557-np3jvrl1d13oec7i9evs0i9c89u7q3hg.apps.googleusercontent.com"
  fi
  if [[ -z "${DAKIA_GOOGLE_CLIENT_SECRET:-}" ]]; then
    DAKIA_GOOGLE_CLIENT_SECRET="$(
      dakia_keychain_read dev.dakia.mail.google-oauth client-secret || true
    )"
  fi
  export -n DAKIA_GOOGLE_CLIENT_ID DAKIA_GOOGLE_CLIENT_SECRET
}

dakia_google_oauth_probe() {
  local client_id="$1"
  local client_secret="$2"
  local secret_file
  local response

  # Keep the secret out of the process list. curl accepts an @file value for
  # --data-urlencode, while the file itself is owner-readable only and removed
  # before this function returns.
  umask 077
  secret_file="$(mktemp "${TMPDIR:-/tmp}/dakia-google-oauth-secret.XXXXXX")"
  if ! printf '%s' "$client_secret" >"$secret_file"; then
    rm -f "$secret_file"
    return 1
  fi
  response="$(
    curl -sS --connect-timeout 10 --max-time 30 \
      -X POST https://oauth2.googleapis.com/token \
      --data-urlencode "client_id=$client_id" \
      --data-urlencode "client_secret@$secret_file" \
      --data-urlencode grant_type=authorization_code \
      --data-urlencode code=dakia-intentional-release-preflight \
      --data-urlencode redirect_uri=http://127.0.0.1:49152 \
      --data-urlencode code_verifier=01234567890123456789012345678901234567890123456789
  )"
  local curl_status=$?
  rm -f "$secret_file"
  [[ "$curl_status" -eq 0 ]] || return 1
  [[ "$(jq -r '.error // empty' <<<"$response")" == "invalid_grant" ]]
}

dakia_require_google_oauth_environment() {
  dakia_load_google_oauth_environment
  if [[ -z "${DAKIA_GOOGLE_CLIENT_SECRET//[[:space:]]/}" ]]; then
    echo "Missing Google OAuth client secret in environment or Keychain service dev.dakia.mail.google-oauth." >&2
    return 1
  fi
  if ! dakia_google_oauth_probe "$DAKIA_GOOGLE_CLIENT_ID" "$DAKIA_GOOGLE_CLIENT_SECRET"; then
    echo "Google rejected the configured OAuth client ID and secret pairing." >&2
    return 1
  fi
}

dakia_prepare_google_oauth_compiler_environment() {
  local compiler_dir
  local existing_rustc_wrapper="${RUSTC_WRAPPER:-}"

  if [[ -n "${DAKIA_RELEASE_GOOGLE_COMPILER_DIR:-}" ]]; then
    echo "Google OAuth compiler environment is already active." >&2
    return 1
  fi
  umask 077
  compiler_dir="$(mktemp -d "${TMPDIR:-/tmp}/dakia-google-oauth-compiler.XXXXXX")"
  if ! printf '%s' "$DAKIA_GOOGLE_CLIENT_ID" >"$compiler_dir/client-id" ||
     ! printf '%s' "$DAKIA_GOOGLE_CLIENT_SECRET" >"$compiler_dir/client-secret"; then
    rm -rf "$compiler_dir"
    return 1
  fi
  printf '%s\n' \
    '#!/usr/bin/env bash' \
    'set -euo pipefail' \
    'rustc="$1"' \
    'shift' \
    'args=("$@")' \
    'crate_name=' \
    'for ((index = 0; index < ${#args[@]}; index += 1)); do' \
    '  if [[ "${args[$index]}" == "--crate-name" && $((index + 1)) -lt ${#args[@]} ]]; then' \
    '    crate_name="${args[$((index + 1))]}"' \
    '    break' \
    '  fi' \
    'done' \
    'if [[ "$crate_name" == "dakia_desktop" ]]; then' \
    '  client_id="$(<"$DAKIA_RELEASE_GOOGLE_CLIENT_ID_FILE")"' \
    '  client_secret="$(<"$DAKIA_RELEASE_GOOGLE_CLIENT_SECRET_FILE")"' \
    '  if [[ -n "${DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER:-}" ]]; then' \
    '    exec env DAKIA_GOOGLE_CLIENT_ID="$client_id" DAKIA_GOOGLE_CLIENT_SECRET="$client_secret" "$DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER" "$rustc" "${args[@]}"' \
    '  fi' \
    '  exec env DAKIA_GOOGLE_CLIENT_ID="$client_id" DAKIA_GOOGLE_CLIENT_SECRET="$client_secret" "$rustc" "${args[@]}"' \
    'fi' \
    'if [[ -n "${DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER:-}" ]]; then' \
    '  exec "$DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER" "$rustc" "${args[@]}"' \
    'fi' \
    'exec "$rustc" "${args[@]}"' >"$compiler_dir/rustc-wrapper"
  chmod 700 "$compiler_dir/rustc-wrapper"

  DAKIA_RELEASE_GOOGLE_COMPILER_DIR="$compiler_dir"
  DAKIA_RELEASE_GOOGLE_CLIENT_ID_FILE="$compiler_dir/client-id"
  DAKIA_RELEASE_GOOGLE_CLIENT_SECRET_FILE="$compiler_dir/client-secret"
  DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER="$existing_rustc_wrapper"
  RUSTC_WRAPPER="$compiler_dir/rustc-wrapper"
  export DAKIA_RELEASE_GOOGLE_COMPILER_DIR \
    DAKIA_RELEASE_GOOGLE_CLIENT_ID_FILE \
    DAKIA_RELEASE_GOOGLE_CLIENT_SECRET_FILE \
    DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER \
    RUSTC_WRAPPER
}

dakia_clear_google_oauth_compiler_environment() {
  if [[ -n "${DAKIA_RELEASE_GOOGLE_COMPILER_DIR:-}" ]]; then
    rm -rf "$DAKIA_RELEASE_GOOGLE_COMPILER_DIR"
  fi
  if [[ -n "${DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER:-}" ]]; then
    RUSTC_WRAPPER="$DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER"
    export RUSTC_WRAPPER
  else
    unset RUSTC_WRAPPER
  fi
  unset DAKIA_RELEASE_GOOGLE_COMPILER_DIR \
    DAKIA_RELEASE_GOOGLE_CLIENT_ID_FILE \
    DAKIA_RELEASE_GOOGLE_CLIENT_SECRET_FILE \
    DAKIA_RELEASE_GOOGLE_ORIGINAL_RUSTC_WRAPPER \
    RUSTC_WRAPPER
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
