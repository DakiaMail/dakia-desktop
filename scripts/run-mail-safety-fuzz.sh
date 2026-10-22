#!/usr/bin/env bash
set -euo pipefail

# Run bounded, coverage-guided fuzzing against Dakia's real complete,
# headers-only, and catalogue MIME paths plus attachment metadata. This is
# intentionally manual: ordinary PR checks use deterministic fixtures.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
duration_seconds="${1:-30}"
cargo_fuzz_bin="${DAKIA_CARGO_FUZZ_BIN:-cargo-fuzz}"
# The original 512 MiB-bound campaign retained 263 MiB in ASan quarantine
# against 44 MiB of live allocations, then stopped without a reproducing input.
# Keep temporal use-after-free checks on, but bound quarantine by default so the
# governed 30-second run is repeatable. A caller's nonempty ASAN_OPTIONS stays
# verbatim; lowering this quarantine reduces temporal UAF detection sensitivity.
asan_options="${ASAN_OPTIONS:-quarantine_size_mb=64:thread_local_quarantine_size_kb=64}"

if ! [[ "$duration_seconds" =~ ^[1-9][0-9]*$ ]] || (( duration_seconds > 300 )); then
  echo "Usage: $0 [duration-seconds: 1..300]" >&2
  exit 64
fi

if ! command -v "$cargo_fuzz_bin" >/dev/null 2>&1; then
  echo "cargo-fuzz is required. Install it with: cargo +nightly install cargo-fuzz --version 0.13.2 --locked" >&2
  exit 69
fi
if ! rustup run nightly rustc --version >/dev/null 2>&1; then
  echo "The nightly Rust toolchain is required: rustup toolchain install nightly --profile minimal" >&2
  exit 69
fi

fuzz_root="$(mktemp -d "${TMPDIR:-/tmp}/dakia-mail-safety-fuzz.XXXXXX")"
artifact_root="$fuzz_root/artifacts"
fuzz_target_dir="${DAKIA_FUZZ_TARGET_DIR:-${TMPDIR:-/tmp}/dakia-mail-safety-fuzz-target}"
ort_lib_location="${ORT_LIB_LOCATION:-$repo_root/apps/desktop/src-tauri/frameworks}"
mkdir -p "$fuzz_target_dir"
completed=false
cleanup() {
  if [[ "$completed" == true ]]; then
    rm -rf "$fuzz_root"
  else
    echo "Fuzzing did not complete. Preserved crash artifacts and corpus at: $fuzz_root" >&2
  fi
}
trap cleanup EXIT

# Cargo-fuzz stores its corpus under the fuzz directory. Copy the small target
# definition to a disposable directory so a crash cannot alter a user-created
# corpus or leave artifacts in the checkout. The target keeps an absolute path
# back to the checked-out public Dakia crate.
cp "$repo_root/fuzz/Cargo.toml" "$repo_root/fuzz/Cargo.lock" "$fuzz_root/"
cp -R "$repo_root/fuzz/fuzz_targets" "$fuzz_root/fuzz_targets"
perl -0pi -e "s#path = \"\.\./crates/dakia-core\"#path = \"$repo_root/crates/dakia-core\"#" "$fuzz_root/Cargo.toml"
perl -0pi -e "s#path = \"\.\./vendor/mail-parser\"#path = \"$repo_root/vendor/mail-parser\"#" "$fuzz_root/Cargo.toml"
perl -0pi -e "s#path = \"\.\./vendor/lettre\"#path = \"$repo_root/vendor/lettre\"#" "$fuzz_root/Cargo.toml"
lock_before="$fuzz_root/Cargo.lock.before"
cp "$fuzz_root/Cargo.lock" "$lock_before"
# The checked-in standalone lock, not the parent workspace lock, binds this
# run to its reviewed dependency graph. Check it before invoking cargo-fuzz,
# whose CLI does not expose Cargo's `--locked` switch.
CARGO_NET_OFFLINE=true cargo metadata --locked --offline --manifest-path "$fuzz_root/Cargo.toml" --format-version 1 >/dev/null
if ! cmp -s "$lock_before" "$fuzz_root/Cargo.lock"; then
  echo "The checked-in fuzz dependency lock changed during metadata preflight." >&2
  exit 1
fi
seed_root="$fuzz_root/corpus"
mkdir -p "$seed_root/mime_budget" "$seed_root/attachment_metadata"
mkdir -p "$artifact_root/mime_budget" "$artifact_root/attachment_metadata"
node "$repo_root/scripts/seed-mail-safety-fuzz.mjs" \
  "$repo_root/crates/dakia-core/testdata/mime" "$seed_root"

for target in mime_budget attachment_metadata; do
  echo "Running $target for ${duration_seconds}s with checked-in MIME seeds"
  target_log="$fuzz_root/$target.log"
  (
    cd "$repo_root"
    # cargo-fuzz 0.13.2 has no `--locked` option. The checked-in fuzz lock is
    # copied above and offline mode makes dependency drift fail rather than
    # resolving newer registry versions.
    ASAN_OPTIONS="$asan_options" CARGO_NET_OFFLINE=true CARGO_TARGET_DIR="$fuzz_target_dir" ORT_LIB_LOCATION="$ort_lib_location" ORT_PREFER_DYNAMIC_LINK=1 DYLD_FALLBACK_LIBRARY_PATH="$ort_lib_location${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}" RUSTUP_TOOLCHAIN=nightly "$cargo_fuzz_bin" run --fuzz-dir "$fuzz_root" "$target" -- \
      -max_total_time="$duration_seconds" \
      -timeout=10 \
      -rss_limit_mb=512 \
      -max_len=1048576 \
      -artifact_prefix="$artifact_root/$target/"
  ) >"$target_log" 2>&1
  tail -n 20 "$target_log"
done

if ! cmp -s "$lock_before" "$fuzz_root/Cargo.lock"; then
  echo "The checked-in fuzz dependency lock changed during the campaign." >&2
  exit 1
fi

completed=true
echo "Bounded mail-safety fuzzing completed without a crash artifact."
