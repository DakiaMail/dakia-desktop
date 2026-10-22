#!/usr/bin/env bash
set -euo pipefail

# Run the opt-in SQLite recovery harness against a separately compiled bundled
# SQLite. This lane is manual because SQLITE_TEST exposes process-global fault
# controls that must never enter an ordinary application or release build.

if (( $# != 0 )); then
  echo "Usage: $0" >&2
  exit 64
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_bin="${DAKIA_SQLITE_FAULT_CARGO_BIN:-cargo}"
target_dir="${DAKIA_SQLITE_FAULT_TARGET_DIR:-${TMPDIR:-/tmp}/dakia-sqlite-fault-target}"
ort_lib_location="${ORT_LIB_LOCATION:-$repo_root/apps/desktop/src-tauri/frameworks}"

for required_command in rg uname; do
  if ! command -v "$required_command" >/dev/null 2>&1; then
    echo "Required command is unavailable: $required_command" >&2
    exit 69
  fi
done
if ! command -v "$cargo_bin" >/dev/null 2>&1; then
  echo "Cargo is required for the SQLite fault-injection lane: $cargo_bin" >&2
  exit 69
fi
if [[ "$target_dir" != /* ]]; then
  echo "DAKIA_SQLITE_FAULT_TARGET_DIR must be an absolute path outside the checkout." >&2
  exit 64
fi
case "$target_dir/" in
  "$repo_root/"*)
    echo "DAKIA_SQLITE_FAULT_TARGET_DIR must be outside the checkout: $target_dir" >&2
    exit 64
    ;;
esac

if ! rg -q '^sqlite-fault-injection = \["dep:libsqlite3-sys"\]$' "$repo_root/crates/dakia-core/Cargo.toml"; then
  echo "The dakia-core sqlite-fault-injection feature is unavailable in this source tree." >&2
  exit 69
fi
if ! rg -q '^name = "sqlite_fault_injection"$' "$repo_root/crates/dakia-core/Cargo.toml"; then
  echo "The dedicated sqlite_fault_injection test target is unavailable in this source tree." >&2
  exit 69
fi

# Match the other manual Rust harnesses. On macOS this prepares or verifies the
# pinned ONNX Runtime required by dakia-core's dependency graph. Other systems
# use the normal locked dependency setup.
"$repo_root/scripts/prepare-onnx-runtime.sh"
if [[ "$(uname -s)" == "Darwin" ]] && [[ ! -f "$ort_lib_location/libonnxruntime.dylib" ]]; then
  echo "The pinned macOS ONNX Runtime is unavailable at: $ort_lib_location" >&2
  exit 69
fi

mkdir -p "$target_dir"

echo "Running the isolated SQLite WRITE, FSYNC, and torn-WAL recovery harness"
echo "Cargo target directory: $target_dir"
(
  cd "$repo_root"
  # SQLite's amalgamated FTS3 test-only hooks depend on Tcl and source files
  # that libsqlite3-sys does not bundle. Disable FTS3 only in this dedicated
  # test build; Dakia's schema uses FTS5 and the VFS fault hooks stay enabled.
  CARGO_TARGET_DIR="$target_dir" \
    CARGO_NET_OFFLINE=true \
    LIBSQLITE3_SYS_USE_PKG_CONFIG=0 \
    LIBSQLITE3_FLAGS='SQLITE_TEST -USQLITE_ENABLE_FTS3 -USQLITE_ENABLE_FTS3_PARENTHESIS' \
    ORT_LIB_LOCATION="$ort_lib_location" \
    ORT_PREFER_DYNAMIC_LINK=1 \
    DYLD_FALLBACK_LIBRARY_PATH="$ort_lib_location${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}" \
    "$cargo_bin" test \
      -p dakia-core \
      --locked \
      --offline \
      --features sqlite-fault-injection \
      --test sqlite_fault_injection \
      -- \
      --exact sqlite_write_fsync_and_torn_wal_preserve_committed_state \
      --nocapture
)

echo "SQLite fault-injection recovery harness passed."
