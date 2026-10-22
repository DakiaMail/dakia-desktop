import assert from "node:assert/strict";
import test from "node:test";
import {
  assertCompleteMeasurements,
  commandFor,
  compareBaseline,
  mailboxScales,
  parseMeasurement,
  parseResourceSoak,
  resourceSoakTest,
  runPerformance,
} from "./run-mail-resilience-performance.mjs";

const source = { algorithm: "fixture", digest: "stable", fileCount: 1 };
const fixtureBuild = {
  rustProfile: "cargo-test-unoptimized-debuginfo",
  rustc: "rustc fixture",
  cargo: "cargo fixture",
};
const outputFor = (messageCount, elapsedMilliseconds) => {
  const pageCount = Math.ceil(messageCount / 500);
  return `DAKIA_MAILBOX_PERF {"messageCount":${messageCount},"pageSize":500,"pageCount":${pageCount},"elapsedMilliseconds":${elapsedMilliseconds}}\n`;
};
const soakOutput =
  'DAKIA_RESOURCE_SOAK {"reconnectCycles":200,"storeOpenCycles":100,"elapsedMilliseconds":50,"fileDescriptorsBefore":10,"fileDescriptorsAfter":10,"residentBytesBefore":100,"residentBytesAfter":110,"residentGrowthBytes":10,"residentGrowthLimitBytes":67108864}\n';

test("mailbox performance plan stays in ignored serial tests", () => {
  assert.deepEqual(
    mailboxScales.map(([count]) => count),
    [2_000, 30_000, 100_000, 200_000],
  );
  const [command, args] = commandFor(mailboxScales[0][1]);
  assert.equal(command, "cargo");
  assert.ok(args.includes("--ignored"));
  assert.ok(args.includes("--exact"));
  assert.ok(args.includes("--test-threads=1"));
  assert.match(
    commandFor(resourceSoakTest)[1].join(" "),
    /reconnect_and_store_open_close_soak/,
  );
});

test("measurement parsing rejects absent and malformed Rust evidence", () => {
  assert.equal(
    parseMeasurement(outputFor(2_000, 125)).elapsedMilliseconds,
    125,
  );
  assert.throws(() => parseMeasurement("test passed"), /did not emit/);
  assert.throws(
    () =>
      parseMeasurement(
        'DAKIA_MAILBOX_PERF {"messageCount":-1,"pageSize":500,"pageCount":4,"elapsedMilliseconds":10}',
      ),
    /Invalid messageCount/,
  );
  assert.equal(parseResourceSoak(soakOutput).reconnectCycles, 200);
  assert.throws(() => parseResourceSoak("test passed"), /did not emit/);
});

test("repeat report keeps every scale and fails closed on source drift", async () => {
  let calls = 0;
  const report = await runPerformance({
    repeat: 2,
    run: async (_command, args) => {
      if (args.some((value) => value.endsWith(resourceSoakTest))) {
        calls += 1;
        return {
          status: "passed",
          exitCode: 0,
          signal: null,
          output: soakOutput,
        };
      }
      const [messageCount] = mailboxScales[calls % mailboxScales.length];
      calls += 1;
      return {
        status: "passed",
        exitCode: 0,
        signal: null,
        output: outputFor(messageCount, calls),
      };
    },
    snapshot: () => source,
    hardware: () => ({ fixture: true }),
    build: () => fixtureBuild,
  });
  assert.equal(report.status, "passed");
  assert.equal(report.measurements.length, 8);
  assert.equal(calls, 9);
  assert.equal(report.resourceSoak.storeOpenCycles, 100);

  let snapshots = 0;
  const drifted = await runPerformance({
    repeat: 1,
    run: async (_command, args) => {
      const name = args.find((value) => value.includes("manual_"));
      if (name.endsWith(resourceSoakTest)) {
        return {
          status: "passed",
          exitCode: 0,
          signal: null,
          output: soakOutput,
        };
      }
      const [messageCount] = mailboxScales.find(([, testName]) =>
        name.endsWith(testName),
      );
      return {
        status: "passed",
        exitCode: 0,
        signal: null,
        output: outputFor(messageCount, 1),
      };
    },
    snapshot: () => ({
      ...source,
      digest: snapshots++ === 0 ? "before" : "after",
    }),
    hardware: () => ({ fixture: true }),
    build: () => fixtureBuild,
  });
  assert.equal(drifted.status, "failed");
  assert.match(drifted.limitations.at(-1), /Source changed/);
});

test("baseline comparison uses medians and marks changes over twenty percent", () => {
  const compatibility = {
    lane: "manual-mailbox-performance",
    status: "passed",
    repeat: 3,
    hardware: {
      platform: "fixture",
      architecture: "fixture",
      osRelease: "fixture",
      cpuModel: "fixture",
      logicalCpuCount: 1,
      totalMemoryBytes: 1,
    },
    build: fixtureBuild,
  };
  const baseline = { ...compatibility, measurements: [] };
  const current = { ...compatibility, status: "running", measurements: [] };
  for (const [messageCount] of mailboxScales) {
    baseline.measurements.push(
      { iteration: 1, messageCount, elapsedMilliseconds: 100 },
      { iteration: 2, messageCount, elapsedMilliseconds: 105 },
      { iteration: 3, messageCount, elapsedMilliseconds: 110 },
    );
    current.measurements.push(
      {
        iteration: 1,
        messageCount,
        elapsedMilliseconds: messageCount === 200_000 ? 130 : 110,
      },
      {
        iteration: 2,
        messageCount,
        elapsedMilliseconds: messageCount === 200_000 ? 132 : 112,
      },
      {
        iteration: 3,
        messageCount,
        elapsedMilliseconds: messageCount === 200_000 ? 134 : 114,
      },
    );
  }
  const comparisons = compareBaseline(current, baseline);
  assert.deepEqual(
    comparisons.map((entry) => entry.status),
    ["passed", "passed", "passed", "regressed"],
  );
});

test("baseline comparison rejects failed, short, hardware, and toolchain mismatches", () => {
  const report = {
    lane: "manual-mailbox-performance",
    status: "running",
    repeat: 3,
    hardware: {
      platform: "darwin",
      architecture: "arm64",
      osRelease: "fixture",
      cpuModel: "fixture",
      logicalCpuCount: 10,
      totalMemoryBytes: 64,
    },
    build: fixtureBuild,
    measurements: [],
  };
  const baseline = { ...structuredClone(report), status: "passed" };
  assert.throws(
    () => compareBaseline(report, { ...baseline, status: "failed" }),
    /status must be passed/,
  );
  assert.throws(
    () => compareBaseline(report, { ...baseline, repeat: 2 }),
    /at least three repeats/,
  );
  assert.throws(
    () =>
      compareBaseline(report, {
        ...baseline,
        hardware: { ...baseline.hardware, cpuModel: "different" },
      }),
    /cpuModel does not match/,
  );
  assert.throws(
    () =>
      compareBaseline(report, {
        ...baseline,
        build: { ...baseline.build, rustc: "different" },
      }),
    /toolchain does not match/,
  );
});

test("baseline comparison rejects missing and duplicate repeat samples", () => {
  const report = {
    repeat: 3,
    measurements: mailboxScales.flatMap(([messageCount]) => [
      { iteration: 1, messageCount, elapsedMilliseconds: 1 },
      { iteration: 2, messageCount, elapsedMilliseconds: 1 },
      { iteration: 3, messageCount, elapsedMilliseconds: 1 },
    ]),
  };
  assert.doesNotThrow(() => assertCompleteMeasurements(report));
  assert.throws(
    () =>
      assertCompleteMeasurements({
        ...report,
        measurements: report.measurements.slice(1),
      }),
    /exactly 3 samples/,
  );
  assert.throws(
    () =>
      assertCompleteMeasurements({
        ...report,
        measurements: report.measurements.map((entry, index) =>
          index === 0 ? { ...entry, iteration: 2 } : entry,
        ),
      }),
    /invalid repeat evidence/,
  );
});

test("runner rejects an incompatible baseline before launching expensive tests", async () => {
  let calls = 0;
  const hardware = {
    platform: "darwin",
    architecture: "arm64",
    osRelease: "fixture",
    cpuModel: "fixture",
    logicalCpuCount: 10,
    totalMemoryBytes: 64,
  };
  const baseline = {
    lane: "manual-mailbox-performance",
    status: "passed",
    repeat: 3,
    hardware: { ...hardware, cpuModel: "different" },
    build: fixtureBuild,
    measurements: [],
  };
  const report = await runPerformance({
    repeat: 3,
    baseline,
    run: async () => {
      calls += 1;
      throw new Error("must not run");
    },
    snapshot: () => source,
    hardware: () => hardware,
    build: () => fixtureBuild,
  });
  assert.equal(report.status, "failed");
  assert.equal(report.failure.kind, "incompatible-baseline");
  assert.match(report.failure.message, /cpuModel does not match/);
  assert.equal(calls, 0);
});

test("failed test cannot produce a passing performance report", async () => {
  const report = await runPerformance({
    repeat: 3,
    run: async () => ({
      status: "failed",
      exitCode: 1,
      signal: null,
      output: "",
    }),
    snapshot: () => source,
    hardware: () => ({ fixture: true }),
    build: () => fixtureBuild,
  });
  assert.equal(report.status, "failed");
  assert.equal(report.failure.expectedCount, 2_000);
  assert.equal(report.measurements.length, 0);
});

test("malformed successful output produces a machine-readable failed report", async () => {
  const report = await runPerformance({
    repeat: 1,
    run: async () => ({
      status: "passed",
      exitCode: 0,
      signal: null,
      output: "test exited zero without a measurement",
    }),
    snapshot: () => source,
    hardware: () => ({ fixture: true }),
    build: () => fixtureBuild,
  });
  assert.equal(report.status, "failed");
  assert.equal(report.failure.kind, "invalid-mailbox-measurement");
  assert.deepEqual(report.sourceTreeAfter, source);
});
