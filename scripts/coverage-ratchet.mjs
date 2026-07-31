#!/usr/bin/env node

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

export function compareCoverage(candidate, baseline) {
  const failures = [];
  for (const component of Object.keys(baseline.components)) {
    const next = candidate.components[component];
    if (!next) {
      failures.push(`${component} is absent from the candidate`);
      continue;
    }
    for (const kind of ["lines", "branches", "functions"]) {
      const oldMetric = baseline.components[component][kind];
      const nextMetric = next[kind];
      // Cross multiplication avoids rounding a tiny regression into a pass.
      if (nextMetric.hit * oldMetric.found < oldMetric.hit * nextMetric.found) {
        failures.push(
          `${component} ${kind} regressed: ${nextMetric.hit}/${nextMetric.found} < ${oldMetric.hit}/${oldMetric.found}`,
        );
      }
    }
  }
  return failures;
}

function usage() {
  return [
    "Usage: node scripts/coverage-ratchet.mjs --rust-lcov PATH --frontend-lcov PATH --output PATH [--baseline PATH]",
    "The script never writes a baseline. A missing --baseline produces a candidate JSON for review.",
  ].join("\n");
}

export function parseArgs(args) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--help") return { help: true };
    if (
      !["--rust-lcov", "--frontend-lcov", "--output", "--baseline"].includes(
        argument,
      )
    ) {
      fail(`Unknown option: ${argument}\n${usage()}`);
    }
    const value = args[index + 1];
    if (!value || value.startsWith("--"))
      fail(`Missing value for ${argument}\n${usage()}`);
    options[argument.slice(2).replace("-", "_")] = value;
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

function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.help) {
    console.log(usage());
    return;
  }
  const candidate = {
    version: 1,
    components: {
      rust: readLcov(options.rust_lcov, "Rust"),
      frontend: readLcov(options.frontend_lcov, "Frontend"),
    },
  };
  const output = resolve(options.output);
  mkdirSync(dirname(output), { recursive: true });
  writeFileSync(output, `${JSON.stringify(candidate, null, 2)}\n`);

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
  const failures = compareCoverage(candidate, baseline);
  if (failures.length > 0)
    fail(`Coverage ratchet failed:\n- ${failures.join("\n- ")}`);
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
