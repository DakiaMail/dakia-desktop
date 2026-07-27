#!/bin/sh

set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
run_npm=false

usage() {
  echo "Usage: $0 [--npm]" >&2
  echo "  --npm  Also submit both npm lockfiles to the configured registry audit endpoint." >&2
}

if [ "$#" -gt 1 ]; then
  usage
  exit 2
fi

case "${1:-}" in
  "")
    ;;
  --npm)
    run_npm=true
    ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    usage
    exit 2
    ;;
esac

if [ -n "${DAKIA_CARGO_AUDIT_BIN:-}" ]; then
  cargo_audit_bin=$DAKIA_CARGO_AUDIT_BIN
elif command -v cargo-audit >/dev/null 2>&1; then
  cargo_audit_bin=$(command -v cargo-audit)
else
  echo "cargo-audit is required. Install it with 'cargo install cargo-audit --locked'." >&2
  exit 1
fi

if [ ! -x "$cargo_audit_bin" ]; then
  echo "DAKIA_CARGO_AUDIT_BIN is not executable: $cargo_audit_bin" >&2
  exit 1
fi

cd "$repo_root"

supported_targets="aarch64-apple-darwin x86_64-apple-darwin"

assert_unreachable() {
  package=$1
  advisory=$2

  for target in $supported_targets; do
    if ! tree_output=$(cargo tree --workspace --locked --target "$target" -i "$package" 2>&1); then
      echo "$tree_output" >&2
      echo "Could not prove $advisory reachability for $target." >&2
      exit 1
    fi

    package_name=${package%@*}
    package_version=${package#*@}
    if printf '%s\n' "$tree_output" | grep -Fq "$package_name v$package_version"; then
      echo "$tree_output" >&2
      echo "$advisory is reachable through $package on supported target $target." >&2
      echo "Remove its exception from .cargo/audit.toml and remediate the dependency." >&2
      exit 1
    fi
  done
}

# cargo-audit scans every package in Cargo.lock and cannot distinguish these
# inactive/target-only subgraphs. Keep the exceptions fail-closed by checking
# the actual supported target graphs first.
assert_unreachable "rsa@0.9.10" "RUSTSEC-2023-0071"
assert_unreachable "glib@0.18.5" "RUSTSEC-2024-0429"

if [ "${DAKIA_AUDIT_OFFLINE:-false}" = true ]; then
  "$cargo_audit_bin" audit --file Cargo.lock --no-fetch --no-yanked
else
  "$cargo_audit_bin" audit --file Cargo.lock
fi

if [ "$run_npm" = true ]; then
  echo "Auditing the root npm lockfile through the configured registry..."
  npm audit --package-lock-only --ignore-scripts --audit-level=low

  echo "Auditing the public-site npm lockfile through the configured registry..."
  npm --prefix apps/site audit --package-lock-only --ignore-scripts --audit-level=low
else
  echo "Rust dependency audit passed."
  echo "npm audit was not run because it submits dependency metadata to the configured registry."
  echo "Run '$0 --npm' when that network disclosure is authorized."
fi
