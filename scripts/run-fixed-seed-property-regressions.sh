#!/usr/bin/env bash
set -euo pipefail

iterations="${1:-3}"
if ! [[ "$iterations" =~ ^[1-9][0-9]*$ ]] || (( iterations > 20 )); then
  echo "Usage: $0 [iterations 1..20]" >&2
  exit 2
fi

# This is intentionally a bounded deterministic corpus, not an unbounded
# generative fuzzer. Any future fuzzer must add its target, seed corpus, and
# resource budget before this lane can claim broader fuzz coverage.
for ((iteration = 1; iteration <= iterations; iteration += 1)); do
  echo "Fixed-seed property corpus iteration $iteration/$iterations"
  cargo test -p dakia-core \
    storage::tests::fixed_seed_thread_graphs_are_invariant_to_insert_and_chunk_order_across_accounts \
    -- --exact --test-threads=1
  npm run test -- apps/desktop/src/threadsProperties.test.ts --maxWorkers=1
  cargo test -p dakia-core --test mail_boundary_properties -- --test-threads=1
  cargo test -p dakia-core \
    mail::tests::parses_the_checked_in_mime_regression_corpus \
    -- --exact --test-threads=1
done
