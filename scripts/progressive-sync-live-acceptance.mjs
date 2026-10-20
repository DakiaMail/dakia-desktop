#!/usr/bin/env node

import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import {
  fixtureCaDer,
  startFixtureServers,
} from "./initial-sync-fixture-server.mjs";

const root = fileURLToPath(new URL("..", import.meta.url));

export function parseArgs(argv) {
  let output;
  let sizes = [2_000, 30_000, 100_000];
  let historyDelayMs = 5_000;
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--output") output = argv[++index];
    else if (value === "--sizes") sizes = argv[++index]?.split(",").map(Number);
    else if (value === "--history-delay-ms")
      historyDelayMs = Number(argv[++index]);
    else throw new Error(`unknown argument: ${value}`);
  }
  if (!output) throw new Error("--output requires a file path");
  if (
    !sizes?.length ||
    sizes.some((size) => !Number.isInteger(size) || size < 1 || size > 100_000)
  )
    throw new Error("--sizes must contain integers from 1 to 100000");
  if (
    !Number.isInteger(historyDelayMs) ||
    historyDelayMs < 500 ||
    historyDelayMs > 30_000
  )
    throw new Error("--history-delay-ms must be from 500 to 30000");
  return { output, sizes, historyDelayMs };
}

export function parseMetrics(output, prefix = "DAKIA_LIVE_METRIC ") {
  return output
    .split(/\r?\n/)
    .filter((line) => line.startsWith(prefix))
    .map((line) => JSON.parse(line.slice(prefix.length)));
}

function validateTiming(name, timing, errors) {
  if (!timing || !Number.isInteger(timing.count) || timing.count < 0) {
    errors.push(`${name}.count must be a non-negative integer`);
    return;
  }
  if (!Number.isFinite(timing.totalMs) || timing.totalMs < 0)
    errors.push(`${name}.totalMs must be a non-negative number`);
  for (const field of ["meanMs", "maxMs"]) {
    const value = timing[field];
    if (timing.count > 0 && (!Number.isFinite(value) || value < 0))
      errors.push(`${name}.${field} must be measured when count is non-zero`);
    if (timing.count === 0 && value !== null)
      errors.push(`${name}.${field} must be null when count is zero`);
  }
}

export function validateCoreMetrics(mode, metrics) {
  const errors = [];
  if (metrics.length !== 1) {
    errors.push(`expected one live metric, received ${metrics.length}`);
    return errors;
  }
  const snapshot = metrics[0].mailMetrics;
  validateTiming(
    "mailMetrics.publicationTransactions",
    snapshot?.publicationTransactions,
    errors,
  );
  validateTiming(
    "mailMetrics.submissionQueue",
    snapshot?.submissionQueue,
    errors,
  );
  if (mode === "fresh-window" && snapshot?.publicationTransactions?.count < 1)
    errors.push("fresh-window did not measure a publication transaction");
  if (
    mode === "smtp-while-history-stalled" &&
    snapshot?.submissionQueue?.count !== 1
  )
    errors.push(
      "SMTP scenario must measure exactly one first-attempt queue wait",
    );
  return errors;
}

function collectProcess(command, args, options) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, options);
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk) => (stdout += chunk));
    child.stderr.setEncoding("utf8").on("data", (chunk) => (stderr += chunk));
    child.on("error", reject);
    child.on("close", (exitCode) =>
      resolve({
        exitCode,
        stdout,
        stderr,
        metrics: parseMetrics(`${stdout}\n${stderr}`),
        parserMetrics: parseMetrics(`${stdout}\n${stderr}`, "DAKIA_METRIC "),
      }),
    );
  });
}

export function coreExamplePath(environment = process.env) {
  const configured = environment.CARGO_TARGET_DIR;
  const target = configured
    ? isAbsolute(configured)
      ? configured
      : resolve(root, configured)
    : join(root, "target");
  return join(
    target,
    "debug",
    "examples",
    `progressive_sync_acceptance${process.platform === "win32" ? ".exe" : ""}`,
  );
}

async function buildCoreExample() {
  const build = await collectProcess(
    "cargo",
    [
      "build",
      "--quiet",
      "--locked",
      "-p",
      "dakia-core",
      "--example",
      "progressive_sync_acceptance",
    ],
    {
      cwd: root,
      env: process.env,
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  if (build.exitCode !== 0)
    throw new Error(`acceptance example did not compile:\n${build.stderr}`);
  const executable = coreExamplePath();
  const executableSha256 = createHash("sha256")
    .update(await readFile(executable))
    .digest("hex");
  return { executable, executableSha256 };
}

function runCoreExample(executable, mode, fixture, caPath, messages) {
  return collectProcess(executable, [mode], {
    cwd: root,
    env: {
      ...process.env,
      DAKIA_ACCEPTANCE_FIXTURE_CA_DER: caPath,
      DAKIA_ACCEPTANCE_METRICS: "1",
      DAKIA_FIXTURE_IMAP_PORT: String(fixture.imapPort),
      DAKIA_FIXTURE_SMTP_PORT: String(fixture.smtpPort),
      DAKIA_FIXTURE_MESSAGES: String(messages),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
}

async function runScenario({
  messages,
  historyDelayMs,
  mode,
  caPath,
  sparse,
  executable,
}) {
  const fixture = await startFixtureServers({
    messages,
    delayMs: 0,
    historyDelayMs,
    sparse,
    writeCa: undefined,
  });
  try {
    const core = await runCoreExample(
      executable,
      mode,
      fixture,
      caPath,
      messages,
    );
    const allFetchCommands = fixture.events.filter(
      ({ protocol, event, line }) =>
        protocol === "imap" &&
        event === "command" &&
        /\s(?:UID )?FETCH\s/i.test(line),
    ).length;
    const headerFetchCommands = fixture.events.filter(
      ({ protocol, event, line }) =>
        protocol === "imap" &&
        event === "command" &&
        /\s(?:UID )?FETCH\s/i.test(line) &&
        /HEADER\.FIELDS/i.test(line),
    ).length;
    const countOnlyHeaderFetchFloor = Math.ceil(messages / 50);
    const metricErrors = validateCoreMetrics(mode, core.metrics);
    return {
      mode,
      messages,
      sparseUids: sparse,
      responseDelayMs: 0,
      historyDelayMs,
      core,
      transport: structuredClone(fixture.metrics),
      acceptedSmtpMessages: fixture.events.filter(
        ({ protocol, event }) => protocol === "smtp" && event === "accepted",
      ).length,
      headerFetchCommands,
      allFetchCommands,
      countOnlyHeaderFetchFloor,
      headerFetchCommandsAboveCountOnlyFloor:
        headerFetchCommands - countOnlyHeaderFetchFloor,
      metricErrors,
      acceptancePassed: core.exitCode === 0 && metricErrors.length === 0,
    };
  } finally {
    await fixture.close();
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const built = await buildCoreExample();
  const directory = await mkdtemp(join(tmpdir(), "dakia-progressive-live-"));
  const caPath = join(directory, "fixture-ca.der");
  await writeFile(caPath, fixtureCaDer);
  try {
    const results = [];
    for (const messages of options.sizes) {
      const result = await runScenario({
        messages,
        historyDelayMs: 0,
        mode: "fresh-window",
        caPath,
        sparse: messages !== 2_000,
        executable: built.executable,
      });
      results.push(result);
      process.stdout.write(
        `fresh-window ${messages}: ${result.acceptancePassed ? "passed" : "failed"}\n`,
      );
      if (!result.acceptancePassed) break;
    }
    if (results.every(({ acceptancePassed }) => acceptancePassed)) {
      const concurrency = await runScenario({
        messages: 2_000,
        historyDelayMs: options.historyDelayMs,
        mode: "smtp-while-history-stalled",
        caPath,
        sparse: true,
        executable: built.executable,
      });
      results.push(concurrency);
      process.stdout.write(
        `smtp-while-history-stalled: ${concurrency.acceptancePassed ? "passed" : "failed"}\n`,
      );
    }
    const report = {
      schemaVersion: 1,
      measuredAt: new Date().toISOString(),
      conditions: {
        host: `${process.platform}-${process.arch}`,
        fixture:
          "implicit TLS loopback, dense 2,000-message and deterministic sparse larger fictional UIDs",
        freshWindowRows: 50,
        historicalBatchRows: 50,
        headerFetchCountComparison: "recorded against ceil(messages / 50)",
        compilationExcludedFromCoreMetrics: true,
        executableSha256: built.executableSha256,
      },
      results,
      allPassed:
        results.length === options.sizes.length + 1 &&
        results.every(({ acceptancePassed }) => acceptancePassed),
    };
    await writeFile(options.output, `${JSON.stringify(report, null, 2)}\n`);
    if (!report.allPassed) process.exitCode = 1;
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

if (process.argv[1] && new URL(import.meta.url).pathname === process.argv[1])
  main().catch((error) => {
    process.stderr.write(`${error.stack ?? error}\n`);
    process.exitCode = 1;
  });
