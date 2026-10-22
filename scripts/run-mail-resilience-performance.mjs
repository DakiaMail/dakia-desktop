import { spawn } from "node:child_process";
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import os from "node:os";
import { pathToFileURL } from "node:url";
import { sourceSnapshot } from "./run-mail-safety.mjs";

export const mailboxScales = [
  [2_000, "manual_two_thousand_message_snapshot_baseline"],
  [30_000, "manual_thirty_thousand_message_snapshot_baseline"],
  [100_000, "manual_one_hundred_thousand_message_snapshot_baseline"],
  [200_000, "manual_two_hundred_thousand_message_snapshot_baseline"],
];
export const resourceSoakTest =
  "manual_repeated_reconnect_and_store_open_close_soak";

export function commandFor(testName) {
  return [
    "cargo",
    [
      "test",
      "-p",
      "dakia-core",
      "--lib",
      "--locked",
      `mail::tests::resilience_performance_tests::${testName}`,
      "--",
      "--ignored",
      "--exact",
      "--nocapture",
      "--test-threads=1",
    ],
  ];
}

export function parseMeasurement(output) {
  const marker = output.match(/DAKIA_MAILBOX_PERF (\{[^\r\n]+\})/);
  if (!marker)
    throw new Error(
      "Rust performance test did not emit a DAKIA_MAILBOX_PERF measurement",
    );
  const measurement = JSON.parse(marker[1]);
  for (const field of [
    "messageCount",
    "pageSize",
    "pageCount",
    "elapsedMilliseconds",
  ]) {
    if (!Number.isInteger(measurement[field]) || measurement[field] < 0) {
      throw new Error(`Invalid ${field} in Rust performance measurement`);
    }
  }
  return measurement;
}

export function parseResourceSoak(output) {
  const marker = output.match(/DAKIA_RESOURCE_SOAK (\{[^\r\n]+\})/);
  if (!marker) {
    throw new Error(
      "Rust resource test did not emit a DAKIA_RESOURCE_SOAK measurement",
    );
  }
  const measurement = JSON.parse(marker[1]);
  for (const field of [
    "reconnectCycles",
    "storeOpenCycles",
    "elapsedMilliseconds",
    "fileDescriptorsBefore",
    "fileDescriptorsAfter",
    "residentBytesBefore",
    "residentBytesAfter",
    "residentGrowthBytes",
    "residentGrowthLimitBytes",
  ]) {
    if (!Number.isInteger(measurement[field]) || measurement[field] < 0) {
      throw new Error(`Invalid ${field} in Rust resource measurement`);
    }
  }
  if (measurement.reconnectCycles === 0 || measurement.storeOpenCycles === 0) {
    throw new Error("Resource soak cycle counts must be positive");
  }
  return measurement;
}

export function median(values) {
  const ordered = [...values].sort((left, right) => left - right);
  const middle = Math.floor(ordered.length / 2);
  return ordered.length % 2
    ? ordered[middle]
    : (ordered[middle - 1] + ordered[middle]) / 2;
}

export function assertCompatibleBaseline(current, baseline) {
  if (baseline.status !== "passed") {
    throw new Error("Baseline report status must be passed");
  }
  if (current.repeat < 3 || baseline.repeat < 3) {
    throw new Error(
      "Baseline comparison requires at least three repeats in both reports",
    );
  }
  if (
    current.lane !== baseline.lane ||
    current.build?.rustProfile !== baseline.build?.rustProfile ||
    current.build?.rustc !== baseline.build?.rustc ||
    current.build?.cargo !== baseline.build?.cargo
  ) {
    throw new Error(
      "Baseline build profile or Rust toolchain does not match this run",
    );
  }
  for (const field of [
    "platform",
    "architecture",
    "osRelease",
    "cpuModel",
    "logicalCpuCount",
    "totalMemoryBytes",
  ]) {
    if (current.hardware?.[field] !== baseline.hardware?.[field]) {
      throw new Error(
        `Baseline hardware field ${field} does not match this run`,
      );
    }
  }
  assertCompleteMeasurements(baseline, "Baseline");
}

export function assertCompleteMeasurements(report, label = "Report") {
  if (!Array.isArray(report.measurements)) {
    throw new Error(`${label} measurements must be an array`);
  }
  for (const [messageCount] of mailboxScales) {
    const samples = report.measurements.filter(
      (entry) => entry.messageCount === messageCount,
    );
    if (samples.length !== report.repeat) {
      throw new Error(
        `${label} must contain exactly ${report.repeat} samples for ${messageCount} messages`,
      );
    }
    const iterations = samples
      .map((entry) => entry.iteration)
      .sort((a, b) => a - b);
    for (let index = 0; index < report.repeat; index += 1) {
      if (
        iterations[index] !== index + 1 ||
        !Number.isInteger(samples[index].elapsedMilliseconds) ||
        samples[index].elapsedMilliseconds < 0
      ) {
        throw new Error(
          `${label} has invalid repeat evidence for ${messageCount} messages`,
        );
      }
    }
  }
}

export function compareBaseline(current, baseline, thresholdPercent = 20) {
  if (!baseline) return [];
  assertCompatibleBaseline(current, baseline);
  assertCompleteMeasurements(current, "Current report");
  const comparisons = [];
  for (const [messageCount] of mailboxScales) {
    const currentValues = current.measurements
      .filter((entry) => entry.messageCount === messageCount)
      .map((entry) => entry.elapsedMilliseconds);
    const baselineValues =
      baseline.measurements
        ?.filter((entry) => entry.messageCount === messageCount)
        .map((entry) => entry.elapsedMilliseconds) ?? [];
    if (!currentValues.length || !baselineValues.length) {
      throw new Error(
        `Baseline is missing repeat measurements for ${messageCount} messages`,
      );
    }
    const currentMedian = median(currentValues);
    const baselineMedian = median(baselineValues);
    const changePercent =
      baselineMedian === 0
        ? currentMedian === 0
          ? 0
          : Number.POSITIVE_INFINITY
        : ((currentMedian - baselineMedian) / baselineMedian) * 100;
    comparisons.push({
      messageCount,
      currentMedianMilliseconds: currentMedian,
      baselineMedianMilliseconds: baselineMedian,
      changePercent,
      status: changePercent > thresholdPercent ? "regressed" : "passed",
    });
  }
  return comparisons;
}

function execute(command, args) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { shell: false });
    let output = "";
    child.stdout.on("data", (chunk) => {
      output += chunk;
      process.stderr.write(chunk);
    });
    child.stderr.on("data", (chunk) => {
      output += chunk;
      process.stderr.write(chunk);
    });
    child.once("error", (error) =>
      resolve({
        status: "unavailable",
        exitCode: null,
        signal: null,
        output,
        error: error.message,
      }),
    );
    child.once("exit", (exitCode, signal) =>
      resolve({
        status: exitCode === 0 && !signal ? "passed" : "failed",
        exitCode,
        signal,
        output,
      }),
    );
  });
}

export async function runPerformance({
  repeat = 3,
  baseline = null,
  run = execute,
  snapshot = sourceSnapshot,
  now = () => new Date().toISOString(),
  hardware = () => ({
    platform: process.platform,
    architecture: process.arch,
    osRelease: os.release(),
    cpuModel: os.cpus()[0]?.model ?? "unavailable",
    logicalCpuCount: os.cpus().length,
    totalMemoryBytes: os.totalmem(),
    node: process.version,
  }),
  build = () => ({
    rustProfile: "cargo-test-unoptimized-debuginfo",
    rustc: execFileSync("rustc", ["-Vv"], { encoding: "utf8" }).trim(),
    cargo: execFileSync("cargo", ["--version"], { encoding: "utf8" }).trim(),
  }),
} = {}) {
  if (!Number.isInteger(repeat) || repeat < 1 || repeat > 5) {
    throw new Error("Repeat must be an integer from 1 to 5");
  }
  const sourceTree = snapshot();
  const report = {
    schemaVersion: 1,
    lane: "manual-mailbox-performance",
    status: "running",
    startedAt: now(),
    finishedAt: null,
    sourceTree,
    sourceTreeAfter: null,
    hardware: hardware(),
    build: build(),
    repeat,
    measurements: [],
    resourceSoak: null,
    comparisons: [],
    limitations: [
      "Loopback scripted IMAP measures Dakia's production protocol and SQLite paths, not provider latency.",
      "Elapsed time is comparable only on the same idle hardware and build profile.",
      "This runner does not establish native WebKit responsiveness or sustained process memory.",
    ],
  };

  const fail = (kind, error, details = {}) => {
    report.status = "failed";
    report.failure = {
      kind,
      message: error instanceof Error ? error.message : String(error),
      ...details,
    };
    report.finishedAt = now();
    report.sourceTreeAfter = snapshot();
    return report;
  };

  if (baseline) {
    try {
      assertCompatibleBaseline(report, baseline);
    } catch (error) {
      report.status = "failed";
      report.failure = {
        kind: "incompatible-baseline",
        message: error.message,
      };
      report.finishedAt = now();
      report.sourceTreeAfter = snapshot();
      return report;
    }
  }

  for (let iteration = 1; iteration <= repeat; iteration += 1) {
    for (const [expectedCount, testName] of mailboxScales) {
      const [command, args] = commandFor(testName);
      const result = await run(command, args);
      if (
        result.status !== "passed" ||
        result.exitCode !== 0 ||
        result.signal
      ) {
        report.status =
          result.status === "unavailable" ? "unavailable" : "failed";
        report.failure = {
          iteration,
          expectedCount,
          testName,
          ...result,
          output: undefined,
        };
        report.finishedAt = now();
        report.sourceTreeAfter = snapshot();
        return report;
      }
      let measurement;
      try {
        measurement = parseMeasurement(result.output);
      } catch (error) {
        return fail("invalid-mailbox-measurement", error, {
          iteration,
          expectedCount,
          testName,
        });
      }
      if (measurement.messageCount !== expectedCount) {
        return fail(
          "invalid-mailbox-measurement",
          new Error(
            `Expected ${expectedCount} messages, got ${measurement.messageCount}`,
          ),
          { iteration, expectedCount, testName },
        );
      }
      report.measurements.push({ iteration, testName, ...measurement });
    }
  }

  {
    const [command, args] = commandFor(resourceSoakTest);
    const result = await run(command, args);
    if (result.status !== "passed" || result.exitCode !== 0 || result.signal) {
      report.status =
        result.status === "unavailable" ? "unavailable" : "failed";
      report.failure = {
        kind: "resource-soak",
        testName: resourceSoakTest,
        ...result,
        output: undefined,
      };
      report.finishedAt = now();
      report.sourceTreeAfter = snapshot();
      return report;
    }
    try {
      report.resourceSoak = parseResourceSoak(result.output);
    } catch (error) {
      return fail("invalid-resource-soak-measurement", error, {
        testName: resourceSoakTest,
      });
    }
  }

  try {
    assertCompleteMeasurements(report);
    report.comparisons = compareBaseline(report, baseline);
  } catch (error) {
    return fail("invalid-comparison-evidence", error);
  }
  report.sourceTreeAfter = snapshot();
  report.finishedAt = now();
  if (
    !sourceTree ||
    !report.sourceTreeAfter ||
    sourceTree.digest !== report.sourceTreeAfter.digest
  ) {
    report.status = "failed";
    report.limitations.push(
      "Source changed during execution; measurements are not an accepted baseline.",
    );
  } else if (report.comparisons.some((entry) => entry.status === "regressed")) {
    report.status = "regressed";
  } else {
    report.status = "passed";
  }
  return report;
}

async function main() {
  const options = { repeat: 3, output: null, baseline: null };
  const args = process.argv.slice(2);
  for (let index = 0; index < args.length; index += 2) {
    const key = args[index];
    const value = args[index + 1];
    if (!value || !["--repeat", "--output", "--baseline"].includes(key)) {
      throw new Error(
        "Use [--repeat 1..5] [--baseline REPORT.json] [--output REPORT.json]",
      );
    }
    options[key.slice(2)] = key === "--repeat" ? Number(value) : value;
  }
  const baseline = options.baseline
    ? JSON.parse(readFileSync(options.baseline, "utf8"))
    : null;
  const report = await runPerformance({ repeat: options.repeat, baseline });
  const json = `${JSON.stringify(report, null, 2)}\n`;
  if (options.output) writeFileSync(options.output, json);
  else process.stdout.write(json);
  if (report.status !== "passed") process.exitCode = 1;
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  main().catch((error) => {
    process.stderr.write(`${error.stack ?? error.message}\n`);
    process.exitCode = 1;
  });
}
