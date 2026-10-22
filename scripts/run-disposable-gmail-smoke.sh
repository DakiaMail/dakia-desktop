#!/usr/bin/env bash
set -euo pipefail

# This is intentionally a local, supervised provider probe. It never accepts
# an app password through an argument, environment variable, file, or chat.
# The compiled binary reads it directly from the controlling terminal without
# echoing it. By default it performs the read-neutral IMAP and SMTP AUTH/QUIT
# probe. Passing the explicit `--roundtrip` argument sends exactly one
# text-only message to the same disposable account and verifies it in INBOX.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
umask 077

roundtrip=false
recover_roundtrip=""
case "${1:-}" in
  "") ;;
  --roundtrip) roundtrip=true ;;
  --recover|--recover-roundtrip)
    recover_roundtrip="${2:-}"
    if [[ ! "$recover_roundtrip" =~ ^[A-Fa-f0-9]{32}$ ]]; then
      echo "Usage: $0 --recover-roundtrip ROUNDTRIP_UUID" >&2
      exit 64
    fi
    ;;
  *)
    echo "Usage: $0 [--roundtrip | --recover-roundtrip ROUNDTRIP_UUID]" >&2
    exit 64
    ;;
esac

read -r -p "Disposable Gmail address: " account_email
if [[ ! "$account_email" =~ ^[A-Za-z0-9._+-]+@gmail\.com$ ]]; then
  echo "A single conventional disposable Gmail address is required." >&2
  exit 64
fi

config_dir="$(mktemp -d "${TMPDIR:-/private/tmp}/dakia-gmail-smoke-config.XXXXXX")"
evidence_dir="$(mktemp -d "${TMPDIR:-/private/tmp}/dakia-gmail-smoke-evidence.XXXXXX")"
config_path="$config_dir/config.json"
evidence_path="$evidence_dir/provider-smoke-evidence.json"
cleanup() {
  rm -rf "$config_dir"
}
trap cleanup EXIT HUP INT TERM

cd "$repo_root"
cargo build --locked -p dakia-cli --bin dakia-provider-smoke
# The shared target directory can be rebuilt by another test lane after this
# build returns. Run only this private, mode-0700 copy so the executable whose
# hash the provider harness records is the executable that received the prompt.
provider_smoke_binary="$config_dir/dakia-provider-smoke"
cp target/debug/dakia-provider-smoke "$provider_smoke_binary"
chmod 700 "$provider_smoke_binary"
"$provider_smoke_binary" \
  --write-gmail-template "$config_path" \
  --template-account "$account_email"

probe_args=(
  --config-file "$config_path" \
  --allow-account "$account_email" \
  --evidence-report "$evidence_path"
)
if [[ "$roundtrip" == true ]]; then
  probe_args+=(
    --roundtrip
    --allow-recipient "$account_email"
    --confirm-disposable-roundtrip
  )
elif [[ -n "$recover_roundtrip" ]]; then
  probe_args+=(--recover-roundtrip "$recover_roundtrip")
fi
source_revision="$(git rev-parse HEAD)"
set +e
DAKIA_PROVIDER_SMOKE_SOURCE_REVISION="$source_revision" "$provider_smoke_binary" "${probe_args[@]}"
probe_status=$?
set -e

echo "Gmail provider evidence: $evidence_path"
exit "$probe_status"
