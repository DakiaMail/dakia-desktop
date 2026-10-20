import assert from "node:assert/strict";
import test from "node:test";

import {
  coreExamplePath,
  parseArgs,
  parseMetrics,
  validateCoreMetrics,
} from "./progressive-sync-live-acceptance.mjs";

test("live runner resolves one prebuilt executable for every scenario", () => {
  assert.match(
    coreExamplePath({ CARGO_TARGET_DIR: "/tmp/dakia-acceptance-target" }),
    /\/tmp\/dakia-acceptance-target\/debug\/examples\/progressive_sync_acceptance$/,
  );
});

test("live acceptance sizes and controlled history delay are explicit and bounded", () => {
  assert.deepEqual(
    parseArgs([
      "--output",
      "/tmp/report.json",
      "--sizes",
      "2000,30000,100000",
      "--history-delay-ms",
      "5000",
    ]),
    {
      output: "/tmp/report.json",
      sizes: [2_000, 30_000, 100_000],
      historyDelayMs: 5_000,
    },
  );
  assert.throws(() => parseArgs([]), /--output/);
  assert.throws(
    () => parseArgs(["--output", "/tmp/x", "--sizes", "100001"]),
    /1 to 100000/,
  );
  assert.throws(
    () => parseArgs(["--output", "/tmp/x", "--history-delay-ms", "499"]),
    /500 to 30000/,
  );
});

test("live runner accepts only explicitly prefixed JSON metrics", () => {
  assert.deepEqual(
    parseMetrics(
      'noise\nDAKIA_LIVE_METRIC {"firstCommitMs":12.5}\nother noise\n',
    ),
    [{ firstCommitMs: 12.5 }],
  );
  assert.deepEqual(
    parseMetrics('DAKIA_METRIC {"uidCount":2000}\n', "DAKIA_METRIC "),
    [{ uidCount: 2_000 }],
  );
});

test("live runner requires measured production transaction and queue timings", () => {
  const timing = (count) => ({
    count,
    totalMs: count ? 12 : 0,
    meanMs: count ? 12 / count : null,
    maxMs: count ? 7 : null,
  });
  const fresh = [
    {
      mailMetrics: {
        publicationTransactions: timing(2),
        submissionQueue: timing(0),
      },
    },
  ];
  assert.deepEqual(validateCoreMetrics("fresh-window", fresh), []);
  assert.match(
    validateCoreMetrics("fresh-window", [
      {
        mailMetrics: {
          publicationTransactions: timing(0),
          submissionQueue: timing(0),
        },
      },
    ]).join("\n"),
    /did not measure a publication transaction/,
  );

  const smtp = [
    {
      mailMetrics: {
        publicationTransactions: timing(1),
        submissionQueue: timing(1),
      },
    },
  ];
  assert.deepEqual(validateCoreMetrics("smtp-while-history-stalled", smtp), []);
  assert.match(
    validateCoreMetrics("smtp-while-history-stalled", fresh).join("\n"),
    /exactly one first-attempt queue wait/,
  );
});
