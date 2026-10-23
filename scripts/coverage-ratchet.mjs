#!/usr/bin/env node

import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";

function fail(message) {
  throw new Error(message);
}

function metric(found = 0, hit = 0) {
  return { found, hit };
}

function addMetric(target, source) {
  target.found += source.found;
  target.hit += source.hit;
}

/**
 * Parse the portable subset of LCOV produced by cargo-llvm-cov and Vitest.
 * We deliberately count distinct lines/branches/functions per source file so
 * repeated tracefile records cannot inflate a candidate's result.
 */
export function summarizeLcov(source) {
  const files = new Map();
  let current;

  function file() {
    if (!current) fail("LCOV record is missing an SF source-file entry");
    if (!files.has(current)) {
      files.set(current, {
        lines: new Map(),
        branches: new Map(),
        functions: new Map(),
      });
    }
    return files.get(current);
  }

  for (const line of source.split(/\r?\n/)) {
    if (line.startsWith("SF:")) {
      current = line.slice(3);
      continue;
    }
    if (line === "end_of_record") {
      current = undefined;
      continue;
    }
    if (line.startsWith("DA:")) {
      const [lineNumber, hits] = line.slice(3).split(",", 2);
      const key = lineNumber;
      const count = Number(hits);
      if (!Number.isInteger(Number(lineNumber)) || !Number.isFinite(count)) {
        fail(`Invalid DA record: ${line}`);
      }
      const existing = file().lines.get(key) ?? 0;
      file().lines.set(key, Math.max(existing, count));
      continue;
    }
    if (line.startsWith("BRDA:")) {
      const [lineNumber, block, branch, hits] = line.slice(5).split(",", 4);
      const key = `${lineNumber}:${block}:${branch}`;
      const count = hits === "-" ? 0 : Number(hits);
      if (!Number.isFinite(count)) fail(`Invalid BRDA record: ${line}`);
      const existing = file().branches.get(key) ?? 0;
      file().branches.set(key, Math.max(existing, count));
      continue;
    }
    if (line.startsWith("FNDA:")) {
      const separator = line.indexOf(",", 5);
      if (separator === -1) fail(`Invalid FNDA record: ${line}`);
      const count = Number(line.slice(5, separator));
      const name = line.slice(separator + 1);
      if (!name || !Number.isFinite(count))
        fail(`Invalid FNDA record: ${line}`);
      const existing = file().functions.get(name) ?? 0;
      file().functions.set(name, Math.max(existing, count));
    }
  }

  const summary = {
    lines: metric(),
    branches: metric(),
    functions: metric(),
  };
  for (const record of files.values()) {
    for (const hits of record.lines.values()) {
      addMetric(summary.lines, metric(1, hits > 0 ? 1 : 0));
    }
    for (const hits of record.branches.values()) {
      addMetric(summary.branches, metric(1, hits > 0 ? 1 : 0));
    }
    for (const hits of record.functions.values()) {
      addMetric(summary.functions, metric(1, hits > 0 ? 1 : 0));
    }
  }
  return summary;
}

function assertSummary(summary, label) {
  for (const kind of ["lines", "branches", "functions"]) {
    const value = summary?.[kind];
    if (
      !value ||
      !Number.isInteger(value.found) ||
      !Number.isInteger(value.hit) ||
      value.found < 0 ||
      value.hit < 0 ||
      value.hit > value.found
    ) {
      fail(`${label} has an invalid ${kind} metric`);
    }
  }
}

/**
 * LCOV producers do not all emit every metric.  In particular, the current
 * Rust report can contain executable lines while containing no BRDA records.
 * A zero denominator is missing observation, not 100% coverage.
 */
export function coverageAvailability(summary) {
  assertSummary(summary, "coverage summary");
  return Object.fromEntries(
    ["lines", "branches", "functions"].map((kind) => [
      kind,
      summary[kind].found === 0
        ? {
            status: "unavailable",
            reason: "LCOV reported no observable items for this metric",
          }
        : { status: "observed" },
    ]),
  );
}

export function compareCoverage(candidate, baseline) {
  return compareCoverageWithAvailability(candidate, baseline).failures;
}

function metricKind(summary, component, kind) {
  return summary.metricKinds?.[component]?.[kind] ?? `lcov-${kind}`;
}

export function compareCoverageWithAvailability(candidate, baseline) {
  const failures = [];
  const unavailable = [];
  for (const component of Object.keys(baseline.components)) {
    const next = candidate.components[component];
    if (!next) {
      failures.push(`${component} is absent from the candidate`);
      continue;
    }
    for (const kind of ["lines", "branches", "functions"]) {
      const oldMetric = baseline.components[component][kind];
      const nextMetric = next[kind];
      const baselineKind = metricKind(baseline, component, kind);
      const candidateKind = metricKind(candidate, component, kind);
      if (baselineKind !== candidateKind) {
        unavailable.push({
          component,
          metric: kind,
          baselineKind,
          candidateKind,
          reason: "metric kinds differ and cannot be ratio-compared",
        });
        continue;
      }
      if (oldMetric.found === 0) {
        // There is no baseline ratio to preserve.  This remains explicitly
        // unavailable in the candidate report and must never print as 100%.
        unavailable.push({
          component,
          metric: kind,
          baselineKind,
          candidateKind,
          reason: "baseline reported no observable items for this metric",
        });
        continue;
      }
      if (nextMetric.found === 0) {
        failures.push(
          `${component} ${kind} is unavailable in the candidate; baseline observed ${oldMetric.hit}/${oldMetric.found}`,
        );
        continue;
      }
      // Cross multiplication avoids rounding a tiny regression into a pass.
      if (nextMetric.hit * oldMetric.found < oldMetric.hit * nextMetric.found) {
        failures.push(
          `${component} ${kind} regressed: ${nextMetric.hit}/${nextMetric.found} < ${oldMetric.hit}/${oldMetric.found}`,
        );
      }
    }
  }
  return { failures, unavailable };
}

function usage() {
  return [
    "Usage: node scripts/coverage-ratchet.mjs --rust-lcov PATH [--rust-normalization-summary PATH] --frontend-lcov PATH --output PATH [--baseline PATH]",
    "The script never writes a baseline. A missing --baseline produces a candidate JSON for review.",
  ].join("\n");
}

export function parseArgs(args) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--help") return { help: true };
    if (
      ![
        "--rust-lcov",
        "--rust-normalization-summary",
        "--frontend-lcov",
        "--output",
        "--baseline",
      ].includes(argument)
    ) {
      fail(`Unknown option: ${argument}\n${usage()}`);
    }
    const value = args[index + 1];
    if (!value || value.startsWith("--"))
      fail(`Missing value for ${argument}\n${usage()}`);
    options[argument.slice(2).replaceAll("-", "_")] = value;
    index += 1;
  }
  for (const option of ["rust_lcov", "frontend_lcov", "output"]) {
    if (!options[option])
      fail(`Missing --${option.replace("_", "-")}\n${usage()}`);
  }
  return options;
}

function readLcov(path, label) {
  const summary = summarizeLcov(readFileSync(path, "utf8"));
  if (summary.lines.found === 0)
    fail(`${label} coverage report contains no executable lines`);
  return summary;
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function validMetric(value, label) {
  if (
    !Number.isInteger(value?.found) ||
    !Number.isInteger(value?.hit) ||
    value.found < 0 ||
    value.hit < 0 ||
    value.hit > value.found
  ) {
    fail(`${label} has an invalid coverage metric`);
  }
  return { found: value.found, hit: value.hit };
}

function validUnavailableMappings(value) {
  if (!Array.isArray(value))
    fail(
      "Rust normalization summary has no unavailable function mappings array",
    );
  return value.map((mapping) => {
    if (
      !mapping ||
      typeof mapping.name !== "string" ||
      !mapping.name ||
      typeof mapping.filename !== "string" ||
      !mapping.filename ||
      !Number.isInteger(mapping.invocationCount) ||
      mapping.invocationCount < 0 ||
      !Number.isInteger(mapping.branchRecordCount) ||
      mapping.branchRecordCount < 0
    ) {
      fail(
        "Rust normalization summary has an invalid unavailable function mapping",
      );
    }
    return {
      name: mapping.name,
      filename: mapping.filename,
      invocationCount: mapping.invocationCount,
      branchRecordCount: mapping.branchRecordCount,
    };
  });
}

function readRustNormalization(path, rustLcov) {
  let report;
  try {
    report = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    fail(
      `Rust normalization summary is invalid: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (
    report?.version !== 1 ||
    typeof report.sourceDigest !== "string" ||
    !/^[a-f0-9]{64}$/i.test(report.sourceDigest) ||
    !/^[a-f0-9]{64}$/i.test(report.profileSha256 ?? "") ||
    !/^[a-f0-9]{64}$/i.test(report.input?.objectSetSha256 ?? "") ||
    report.output?.lcovSha256 !== sha256(rustLcov) ||
    report.functions?.metric !== "llvm-instrumentation-instantiations"
  ) {
    fail("Rust normalization summary is not bound to the supplied LCOV cohort");
  }
  const lines = validMetric(
    report.linesBranches?.lines,
    "Rust normalization line",
  );
  const branches = validMetric(
    report.linesBranches?.branches,
    "Rust normalization branch",
  );
  const functions = validMetric(
    report.functions?.instantiations,
    "Rust normalization instrumentation instantiation",
  );
  const lcov = summarizeLcov(readFileSync(rustLcov, "utf8"));
  if (
    lcov.lines.found !== lines.found ||
    lcov.lines.hit !== lines.hit ||
    lcov.branches.found !== branches.found ||
    lcov.branches.hit !== branches.hit ||
    lcov.functions.found !== functions.found ||
    lcov.functions.hit !== functions.hit
  ) {
    fail("Rust normalization summary metrics do not match the supplied LCOV");
  }
  return {
    functions,
    provenance: {
      normalizationSummarySha256: sha256(path),
      sourceDigest: report.sourceDigest,
      profileSha256: report.profileSha256,
      objectSetSha256: report.input.objectSetSha256,
      unavailableFunctionMappings: validUnavailableMappings(
        report.functions.unavailableFunctionMappings,
      ),
    },
  };
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    console.log(usage());
    return;
  }
  const rust = readLcov(options.rust_lcov, "Rust");
  const normalized = options.rust_normalization_summary
    ? readRustNormalization(
        options.rust_normalization_summary,
        options.rust_lcov,
      )
    : undefined;
  const candidate = {
    version: 1,
    components: {
      rust,
      frontend: readLcov(options.frontend_lcov, "Frontend"),
    },
    metricKinds: {
      rust: {
        lines: "lcov-lines",
        branches: "lcov-branches",
        functions: options.rust_normalization_summary
          ? "llvm-instrumentation-instantiations"
          : "lcov-functions",
      },
      frontend: {
        lines: "lcov-lines",
        branches: "lcov-branches",
        functions: "lcov-functions",
      },
    },
  };
  if (normalized) {
    candidate.rustCoverageProvenance = normalized.provenance;
  }
  candidate.observations = Object.fromEntries(
    Object.entries(candidate.components).map(([component, summary]) => [
      component,
      coverageAvailability(summary),
    ]),
  );
  const output = resolve(options.output);
  mkdirSync(dirname(output), { recursive: true });
  writeFileSync(output, `${JSON.stringify(candidate, null, 2)}\n`);

  const unavailable = Object.entries(candidate.observations).flatMap(
    ([component, metrics]) =>
      Object.entries(metrics)
        .filter(([, observation]) => observation.status === "unavailable")
        .map(([kind]) => `${component} ${kind}`),
  );
  if (unavailable.length > 0) {
    console.log(
      `Coverage observations unavailable: ${unavailable.join(", ")}. This is unavailable evidence, not 100% coverage.`,
    );
  }

  if (!options.baseline) {
    console.log(`Wrote coverage candidate without a baseline: ${output}`);
    return;
  }
  const baseline = JSON.parse(readFileSync(options.baseline, "utf8"));
  if (baseline.version !== 1 || !baseline.components)
    fail("Unsupported coverage baseline version");
  for (const [name, summary] of Object.entries(baseline.components)) {
    assertSummary(summary, `baseline ${name}`);
  }
  const comparison = compareCoverageWithAvailability(candidate, baseline);
  candidate.comparisonAvailability = comparison.unavailable;
  writeFileSync(output, `${JSON.stringify(candidate, null, 2)}\n`);
  if (comparison.unavailable.length > 0) {
    console.log(
      `Coverage comparisons unavailable: ${comparison.unavailable.map(({ component, metric }) => `${component} ${metric}`).join(", ")}.`,
    );
  }
  const failures = comparison.failures;
  if (failures.length > 0)
    fail(`Coverage ratchet failed:\n- ${failures.join("\n- ")}`);
  if (comparison.unavailable.length > 0) {
    console.log(
      `Coverage ratchet partially compared available metrics against ${options.baseline}; candidate: ${output}`,
    );
    return;
  }
  console.log(
    `Coverage ratchet passed against ${options.baseline}; candidate: ${output}`,
  );
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    main();
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
