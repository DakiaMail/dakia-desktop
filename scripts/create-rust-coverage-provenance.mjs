#!/usr/bin/env node

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";

function fail(message) {
  throw new Error(message);
}

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function stateDigest(state) {
  return createHash("sha256").update(JSON.stringify(state)).digest("hex");
}

function isSha256(value) {
  return typeof value === "string" && /^[a-f0-9]{64}$/i.test(value);
}

function write(path, value) {
  const output = resolve(path);
  mkdirSync(dirname(output), { recursive: true });
  writeFileSync(output, value, { mode: 0o600 });
}

function sourceManifest(root) {
  const files = execFileSync("git", ["-C", root, "ls-files", "-z"], {
    encoding: "utf8",
  })
    .split("\0")
    .filter(Boolean)
    .sort()
    .map((path) => {
      const absolute = resolve(root, path);
      return { path, bytes: statSync(absolute).size, sha256: sha256(absolute) };
    });
  const hash = createHash("sha256");
  for (const file of files) {
    hash.update(file.path).update("\0");
    hash.update(Buffer.from(file.sha256, "hex")).update("\0");
  }
  return {
    sourceTree: {
      algorithm: "sha256-path-and-content-manifest",
      digest: hash.digest("hex"),
      fileCount: files.length,
    },
    files,
  };
}

function readAndRevalidateSourceManifest(path, root) {
  let recorded;
  try {
    recorded = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    fail(
      `Source manifest is invalid: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  const current = sourceManifest(root);
  if (JSON.stringify(recorded) !== JSON.stringify(current)) {
    fail(
      "Tracked source inventory changed during coverage instrumentation or export",
    );
  }
  return { ...recorded, sha256: sha256(path) };
}

export function prepareRustCoverageSource(options) {
  const manifest = sourceManifest(options.source_root);
  write(options.source_manifest, `${JSON.stringify(manifest, null, 2)}\n`);
  write(options.source_digest_output, `${manifest.sourceTree.digest}\n`);
  return manifest;
}

function readLog(path) {
  const lines = readFileSync(path, "utf8").split("\n").filter(Boolean);
  if (lines.length === 0)
    fail("llvm-cov proxy did not record an export invocation");
  return lines.map((line) => {
    try {
      return JSON.parse(line);
    } catch (error) {
      fail(
        `llvm-cov proxy log is invalid: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  });
}

function sameState(before, after, label) {
  if (
    !Array.isArray(before) ||
    before.length === 0 ||
    JSON.stringify(before) !== JSON.stringify(after)
  ) {
    fail(`${label} changed during export or has no observed object state`);
  }
  for (const entry of before) {
    if (
      !entry ||
      typeof entry.path !== "string" ||
      !Number.isSafeInteger(entry.bytes) ||
      !isSha256(entry.sha256)
    ) {
      fail(`${label} has invalid observed object state`);
    }
    if (
      statSync(entry.path).size !== entry.bytes ||
      sha256(entry.path) !== entry.sha256
    ) {
      fail(`${label} object no longer matches the export observation`);
    }
  }
}

function validateInvocation(value, label, format) {
  if (
    value?.version !== 1 ||
    value.kind !== "rust-coverage-export-invocation" ||
    value.exitCode !== 0 ||
    value.signal !== null ||
    typeof value.program !== "string" ||
    !Array.isArray(value.arguments) ||
    value.arguments[0] !== "export"
  ) {
    fail(`${label} proxy invocation is invalid or failed`);
  }
  const args = value.arguments;
  if (format === "lcov") {
    if (!args.includes("-format=lcov") || !args.includes("--skip-functions")) {
      fail("Line-and-branch export was not LCOV with --skip-functions");
    }
  } else if (
    args.includes("-format=lcov") ||
    args.includes("--skip-functions")
  ) {
    fail("Function export must be JSON-compatible and include functions");
  }
  const profile = args.find((argument) =>
    argument.startsWith("-instr-profile="),
  );
  if (
    !profile ||
    value.profilePath !== profile.slice("-instr-profile=".length)
  ) {
    fail(`${label} proxy invocation does not bind its profile argument`);
  }
  if (
    !isSha256(value.profileSha256Before) ||
    value.profileSha256Before !== value.profileSha256After
  ) {
    fail(`${label} profile changed during export`);
  }
  if (sha256(value.profilePath) !== value.profileSha256Before) {
    fail(`${label} profile no longer matches the export observation`);
  }
  sameState(value.objectStateBefore, value.objectStateAfter, label);
  const objectArguments = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] !== "-object") continue;
    const path = args[index + 1];
    if (!path || path.startsWith("-"))
      fail(`${label} has an invalid -object argument`);
    objectArguments.push(path);
    index += 1;
  }
  if (
    new Set(objectArguments).size !== objectArguments.length ||
    JSON.stringify(objectArguments) !==
      JSON.stringify(value.objectStateBefore.map((entry) => entry.path))
  ) {
    fail(`${label} object arguments do not match the observed object state`);
  }
  return value;
}

function selectInvocation(events, label, format) {
  const matches = events.filter((event) => {
    const args = event?.arguments;
    if (!Array.isArray(args) || args[0] !== "export") return false;
    return format === "lcov"
      ? args.includes("-format=lcov")
      : !args.includes("-format=lcov");
  });
  if (matches.length !== 1)
    fail(`${label} requires exactly one recorded export invocation`);
  return validateInvocation(matches[0], label, format);
}

function sameCohort(left, right) {
  if (
    left.program !== right.program ||
    left.profilePath !== right.profilePath ||
    left.profileSha256Before !== right.profileSha256Before ||
    JSON.stringify(left.objectStateBefore) !==
      JSON.stringify(right.objectStateBefore)
  ) {
    fail(
      "Recorded exports do not use one llvm-cov program, profile, and object cohort",
    );
  }
}

function receipt({ invocation, format, output, manifest, toolchain }) {
  return {
    version: 1,
    kind: "rust-coverage-export",
    format,
    sourceDigest: manifest.sourceTree.digest,
    sourceManifestSha256: manifest.sha256,
    profilePath: invocation.profilePath,
    profileSha256: invocation.profileSha256Before,
    objectSetSha256: stateDigest(invocation.objectStateBefore),
    outputSha256: sha256(output),
    producer: {
      program: invocation.program,
      toolchain,
      arguments: invocation.arguments,
    },
    observedInputState: {
      profileSha256Before: invocation.profileSha256Before,
      profileSha256After: invocation.profileSha256After,
      objectStateBefore: invocation.objectStateBefore,
      objectStateAfter: invocation.objectStateAfter,
    },
    scope:
      "cargo-llvm-cov report export captured by the checked-in proxy; the receipt binds the completed output to the observed profile and object cohort",
  };
}

export function createRustCoverageProvenance(options) {
  const boundManifest = readAndRevalidateSourceManifest(
    options.source_manifest,
    options.source_root,
  );
  const events = readLog(options.export_log);
  const lineBranch = selectInvocation(events, "Line-and-branch", "lcov");
  const functions = selectInvocation(events, "Function JSON", "json");
  sameCohort(lineBranch, functions);
  for (const output of [options.line_branch_lcov, options.functions_json]) {
    if (statSync(output).size === 0)
      fail(`Coverage output is empty: ${output}`);
  }
  const lineBranchReceipt = receipt({
    invocation: lineBranch,
    format: "lcov",
    output: options.line_branch_lcov,
    manifest: boundManifest,
    toolchain: options.toolchain,
  });
  const functionsReceipt = receipt({
    invocation: functions,
    format: "json",
    output: options.functions_json,
    manifest: boundManifest,
    toolchain: options.toolchain,
  });
  write(
    options.line_branch_receipt,
    `${JSON.stringify(lineBranchReceipt, null, 2)}\n`,
  );
  write(
    options.functions_receipt,
    `${JSON.stringify(functionsReceipt, null, 2)}\n`,
  );
  if (
    readFileSync(options.source_digest_output, "utf8").trim() !==
    boundManifest.sourceTree.digest
  ) {
    fail("Source digest receipt does not match the preserved source manifest");
  }
  return { manifest: boundManifest, lineBranchReceipt, functionsReceipt };
}

const PROVENANCE_ARGUMENTS = [
  "source-root",
  "source-manifest",
  "source-digest-output",
  "export-log",
  "line-branch-lcov",
  "functions-json",
  "line-branch-receipt",
  "functions-receipt",
  "toolchain",
];

const SOURCE_ARGUMENTS = [
  "source-root",
  "source-manifest",
  "source-digest-output",
];

export function parseArgs(args, names = PROVENANCE_ARGUMENTS) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const option = args[index];
    const value = args[index + 1];
    if (!names.includes(option?.slice(2)) || !value || value.startsWith("--"))
      fail(`Invalid coverage provenance option: ${option}`);
    options[option.slice(2).replaceAll("-", "_")] = value;
    index += 1;
  }
  for (const name of names)
    if (!options[name.replaceAll("-", "_")]) fail(`Missing --${name}`);
  return options;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  try {
    const args = process.argv.slice(2);
    if (args[0] === "--prepare-source") {
      prepareRustCoverageSource(parseArgs(args.slice(1), SOURCE_ARGUMENTS));
    } else {
      createRustCoverageProvenance(parseArgs(args));
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}
