#!/usr/bin/env bash
set -euo pipefail

name="${1:-}"
case "$name" in
  updater-password)
    service="dev.dakia.mail.updater-signing"
    account="password"
    ;;
  r2-access-key-id)
    service="dev.dakia.mail.r2"
    account="access-key-id"
    ;;
  r2-secret-access-key)
    service="dev.dakia.mail.r2"
    account="secret-access-key"
    ;;
  google-oauth-client-secret)
    service="dev.dakia.mail.google-oauth"
    account="client-secret"
    ;;
  *)
    echo "Usage: $0 <updater-password|r2-access-key-id|r2-secret-access-key|google-oauth-client-secret>" >&2
    exit 2
    ;;
esac

printf 'Enter %s: ' "$name" >&2
IFS= read -r -s value
printf '\n' >&2
if [[ -z "$value" ]]; then
  echo "Refusing to store an empty value." >&2
  exit 1
fi
/usr/bin/security add-generic-password \
  -U -s "$service" -a "$account" -w "$value"
unset value
echo "Stored $name in the login Keychain."
