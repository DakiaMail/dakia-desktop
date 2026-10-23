# Mail resilience and performance lane

This lane uses fictional loopback IMAP data and Dakia's production protocol,
snapshot, and SQLite paths. It measures mailbox inventories at 2,000, 30,000,
100,000, and 200,000 messages. Every IMAP inventory request remains limited to
500 messages. After the timing matrix it also performs 200 TCP reconnect and
logout cycles plus 100 SQLite profile open and close cycles. The soak fails if
file descriptors accumulate or resident memory grows by more than 64 MiB.

Run three serial measurements and save the machine-readable report:

```sh
npm run test:mail-safety:performance -- \
  --repeat 3 \
  --output docs/mail-safety-results/mailbox-performance-local.json
```

Compare a later run with an accepted report from the same machine:

```sh
npm run test:mail-safety:performance -- \
  --repeat 3 \
  --baseline docs/mail-safety-results/mailbox-performance-local.json \
  --output docs/mail-safety-results/mailbox-performance-comparison.json
```

The comparison fails when a median duration increases by more than 20 percent.
It also fails closed when the baseline did not pass, either report has fewer
than three repeats, or the platform, architecture, OS release, CPU, logical CPU
count, memory size, Cargo version, Rust version, or build profile differs.
Source changes during a run invalidate the report.

The ordinary source suite keeps deterministic bounded-page assertions. The
timed duplicates and the 200,000-message case are ignored unless this manual
runner selects them, so repeat measurements do not consume the PR budget.
It also cancels a 2,000-message inventory after one complete page and verifies
that the transport closes while the previously committed mailbox remains
intact.

The elapsed duration covers fixture creation, production IMAP exchange,
snapshot staging and finalization, and local verification. It does not measure
a real provider, energy use, native WebKit response, or foreground interaction
while synchronization runs. The resource soak is bounded and catches retained
sockets, database handles, and large short-run memory growth; it is not a
multi-hour leak campaign. Native responsiveness and real sleep and wake remain
separate acceptance work.

Use an idle machine with stable power and thermal conditions. The current Mac
can establish a local development baseline without credentials. Choosing the
machine and configuration that acts as the release baseline requires an owner
decision because results cannot be compared honestly across hardware or OS
versions.

## Native sleep and wake acceptance

Sleep and wake requires a real macOS power transition, so it runs only after
the fictional provider-to-Reader fixture is open and stable. The acceptance
operator starts the observer with the PID, ports, and app log printed by
`scripts/native-provider-reader-acceptance.sh`:

```sh
node scripts/observe-native-sleep-wake.mjs \
  --metadata "$RUN_ROOT/sleep-wake-fixture.json" \
  --output "$RUN_ROOT/sleep-wake.json"
```

Wait for `READY_FOR_SLEEP`. The user then puts the Mac to sleep for at least 15
seconds and wakes it. The observer requires matching macOS `pmset` sleep and
wake records, confirms that the exact app process stays alive, then submits one
new fictional message after the local provider is reachable. It records the
production watcher's reconnect count, IMAP socket recovery, new-arrival log
marker, and the exact new RFC Message-ID in the fixture SQLite profile. More
than ten reconnects before recovery fails the run.
The operator must also see `Fictional mail received after wake` in the Reader.

Do not run this while another test suite is active. macOS sleep pauses all local
servers and build jobs, so it would invalidate their timing and failure results.
